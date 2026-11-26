//! Single owner of the workbench `Backend` and the only core-to-hub publisher.
//!
//! HTTP callers only ever reach the core through this bridge: they enqueue a
//! command and receive its request id once the core accepts it. One owner loop
//! calls [`ServiceBridge::poll`] on repaint wakeups plus a periodic fallback,
//! which is the only place that drains receipts/live output and reads core
//! snapshots. The EventHub stays the sole publisher of transport sequence
//! numbers, so a slow or vanished SSE reader never blocks credit return.

use crate::{
    api::{
        backend::Backend,
        contract::CommandResponse,
        events::{EventHub, PublishedEvent, PublishedView},
    },
    core::service::{LiveOutputEvent, SubmitError, UiCommand},
};
#[cfg(feature = "daemon")]
use crate::api::files::{ImportRegistry, ImportStage, ImportTicket};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Commands may wait in the bridge before the core sees them.
pub const BRIDGE_QUEUE_LIMIT: usize = 32;
/// A poll round must drain again after at most this many core submissions.
pub const SUBMISSIONS_PER_DRAIN: usize = 8;
/// Owner-loop fallback tick; also the bound on noticing a stop request.
const OWNER_TICK: Duration = Duration::from_millis(20);

/// Admission outcome for an HTTP command POST. `Busy` and `Stopped` are known
/// non-acceptance; `AcceptanceUnknown` means no caller may safely replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeSubmitError {
    Busy,
    Stopped,
    AcceptanceUnknown,
}

/// Wake handle shared with the core repaint callback. A single owner loop
/// waits on it with a 20 ms fallback timeout, so a notification that arrives
/// before the wait is still observed.
pub struct BridgeWake {
    notified: Mutex<bool>,
    condvar: Condvar,
}

impl BridgeWake {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { notified: Mutex::new(false), condvar: Condvar::new() })
    }

    pub fn notify(&self) {
        *lock(&self.notified) = true;
        self.condvar.notify_all();
    }

    /// Returns `true` when notified, `false` on the fallback timeout.
    pub fn wait(&self, timeout: Duration) -> bool {
        let notified = lock(&self.notified);
        let (mut notified, _) = self
            .condvar
            .wait_timeout_while(notified, timeout, |notified| !*notified)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = *notified;
        *notified = false;
        result
    }

    /// Repaint callback handed to the core so snapshot publication wakes the
    /// owner loop immediately instead of waiting for the fallback tick.
    pub fn repaint(self: &Arc<Self>) -> Arc<dyn Fn() + Send + Sync> {
        let wake = self.clone();
        Arc::new(move || wake.notify())
    }
}

pub struct ServiceBridge {
    backend: Box<dyn Backend>,
    /// Last core snapshot identity the bridge observed. The hub re-wraps the
    /// value it stores, so pointer comparison must stay on the core side or
    /// one snapshot would be republished on every poll.
    last_snapshot: Arc<crate::core::service::AppSnapshot>,
    admission: Arc<Admission>,
    hub: Arc<Mutex<EventHub>>,
    wake: Arc<BridgeWake>,
    /// Import cleanup is owned here but its I/O runs on a dedicated thread.
    #[cfg(feature = "daemon")]
    imports: Arc<ImportRegistry>,
}

/// Cloneable submission handle for HTTP callers. Admission only touches the
/// bounded queue and wake path; waiting for core acceptance happens outside
/// every bridge lock and outside the owner thread.
#[derive(Clone)]
pub struct BridgeClient {
    admission: Arc<Admission>,
    #[cfg(feature = "daemon")]
    imports: Arc<ImportRegistry>,
}

impl BridgeClient {
    pub fn enqueue(&self, command: UiCommand) -> Result<PendingSubmission, BridgeSubmitError> {
        self.admission.enqueue(command)
    }

    /// Blocking convenience for non-owner callers. HTTP uses [`enqueue`] plus a
    /// blocking waiter so no async runtime thread is held inside a lock.
    pub fn submit(&self, command: UiCommand) -> Result<u64, BridgeSubmitError> {
        self.admission.enqueue(command)?.wait()
    }

    /// Enqueue an import whose private staging copy is cleaned only after the
    /// real receipt, or on known non-acceptance. Acceptance uncertainty and a
    /// bridge exit retain the copy; no caller may replay it.
    #[cfg(feature = "daemon")]
    pub fn enqueue_import(
        &self,
        command: UiCommand,
        stage: ImportStage,
    ) -> Result<PendingSubmission, BridgeSubmitError> {
        let Some(ticket) = self.imports.reserve() else {
            // A full registry is a known rejection, so the copy goes away.
            self.imports.reject(stage);
            return Err(BridgeSubmitError::Busy);
        };
        self.admission.enqueue_with(
            command,
            SubmissionExtras {
                import: Some(QueuedImport { ticket, stage }),
            },
        )
    }
}

struct Admission {
    queue: Mutex<QueueState>,
    wake: Arc<BridgeWake>,
    /// Needed only to reject staging when admission itself refuses a command.
    #[cfg(feature = "daemon")]
    imports: Arc<ImportRegistry>,
}

impl Admission {
    fn enqueue(&self, command: UiCommand) -> Result<PendingSubmission, BridgeSubmitError> {
        self.enqueue_with(command, SubmissionExtras::default())
    }

    fn enqueue_with(
        &self,
        command: UiCommand,
        extras: SubmissionExtras,
    ) -> Result<PendingSubmission, BridgeSubmitError> {
        let (reply, pending) = mpsc::sync_channel(1);
        {
            let mut state = lock(&self.queue);
            // Ingress closure is a known non-acceptance: queued work that the
            // core never saw answers `Stopped` (HTTP `unavailable`), and no
            // caller may treat it as replayable.
            if state.ingress_closed {
                drop(state);
                extras.reject_import(self);
                return Err(BridgeSubmitError::Stopped);
            }
            if state.core_stopped {
                drop(state);
                extras.reject_import(self);
                return Err(BridgeSubmitError::Stopped);
            }
            if state.items.len() >= BRIDGE_QUEUE_LIMIT {
                drop(state);
                extras.reject_import(self);
                return Err(BridgeSubmitError::Busy);
            }
            state.items.push_back(QueuedSubmission::with_extras(command, reply, extras));
        }
        self.wake.notify();
        Ok(PendingSubmission { reply: pending })
    }
}

#[derive(Default)]
struct SubmissionExtras {
    #[cfg(feature = "daemon")]
    import: Option<QueuedImport>,
}

impl SubmissionExtras {
    /// Known rejection before acceptance: clean through the registry so the
    /// I/O never runs on the caller's thread.
    fn reject_import(self, _admission: &Admission) {
        #[cfg(feature = "daemon")]
        if let Some(import) = self.import {
            _admission.imports.reject(import.stage);
        }
    }
}

struct QueueState {
    items: VecDeque<QueuedSubmission>,
    core_stopped: bool,
    ingress_closed: bool,
}

struct QueuedSubmission {
    command: UiCommand,
    reply: SyncSender<Result<u64, BridgeSubmitError>>,
    #[cfg(feature = "daemon")]
    import: Option<QueuedImport>,
}

impl QueuedSubmission {
    fn with_extras(command: UiCommand, reply: SyncSender<Result<u64, BridgeSubmitError>>, extras: SubmissionExtras) -> Self {
        #[cfg(feature = "daemon")]
        {
            let SubmissionExtras { import } = extras;
            Self { command, reply, import }
        }
        #[cfg(not(feature = "daemon"))]
        {
            let SubmissionExtras {} = extras;
            Self { command, reply }
        }
    }
}

#[cfg(feature = "daemon")]
struct QueuedImport {
    ticket: ImportTicket,
    stage: ImportStage,
}

/// Handle for one enqueued command. Dropping it (HTTP waiter disconnect) does
/// not remove the command from the bridge queue.
pub struct PendingSubmission {
    reply: Receiver<Result<u64, BridgeSubmitError>>,
}

impl PendingSubmission {
    /// Blocks until the owner loop settles this submission. A bridge exit
    /// without a settled reply is reported as acceptance uncertainty.
    pub fn wait(self) -> Result<u64, BridgeSubmitError> {
        self.reply.recv().unwrap_or(Err(BridgeSubmitError::AcceptanceUnknown))
    }
}

impl ServiceBridge {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        Self::with_wake(backend, BridgeWake::new())
    }

    /// Uses an externally created wake so the same handle can be registered as
    /// the core repaint callback before the bridge is assembled.
    pub fn with_wake(mut backend: Box<dyn Backend>, wake: Arc<BridgeWake>) -> Self {
        let initial = backend.snapshot();
        #[cfg(feature = "daemon")]
        let imports = ImportRegistry::new(BRIDGE_QUEUE_LIMIT);
        Self {
            backend,
            last_snapshot: initial.clone(),
            admission: Arc::new(Admission {
                queue: Mutex::new(QueueState {
                    items: VecDeque::new(),
                    core_stopped: false,
                    ingress_closed: false,
                }),
                wake: wake.clone(),
                #[cfg(feature = "daemon")]
                imports: imports.clone(),
            }),
            hub: Arc::new(Mutex::new(EventHub::new(initial))),
            wake,
            #[cfg(feature = "daemon")]
            imports,
        }
    }

    /// Cloneable handle that can be held by HTTP connections without sharing
    /// the owner-only `ServiceBridge` itself.
    pub fn client(&self) -> BridgeClient {
        BridgeClient {
            admission: self.admission.clone(),
            #[cfg(feature = "daemon")]
            imports: self.imports.clone(),
        }
    }

    /// Shared hub handle for transport assembly. HTTP receives only the
    /// read-only `EventHubAccess` wrapper, never this mutable handle.
    pub fn hub(&self) -> Arc<Mutex<EventHub>> {
        self.hub.clone()
    }

    pub fn repaint_callback(&self) -> Arc<dyn Fn() + Send + Sync> {
        self.wake.repaint()
    }

    pub fn wait_for_wakeup(&self, timeout: Duration) -> bool {
        self.wake.wait(timeout)
    }

    /// Reads only what the bridge already published; never the core directly.
    pub fn published_view(&self) -> PublishedView {
        lock(&self.hub).published_view()
    }

    /// Queues a command and waits for core acceptance before returning.
    pub fn submit(&self, command: UiCommand) -> Result<u64, BridgeSubmitError> {
        self.enqueue(command)?.wait()
    }

    /// Separates queue admission from acceptance so a caller can observe that
    /// queued work survives an HTTP waiter disconnect.
    pub fn enqueue(&self, command: UiCommand) -> Result<PendingSubmission, BridgeSubmitError> {
        self.admission.enqueue(command)
    }

    /// Closes the bridge ingress before the core is stopped. Work still queued
    /// here never reached the core, so it answers `Stopped` (a known
    /// non-acceptance) instead of the drop path's `AcceptanceUnknown`.
    pub fn close_ingress(&self) {
        let mut state = lock(&self.admission.queue);
        state.ingress_closed = true;
        self.stop_queued(&mut state);
    }

    pub fn ingress_closed(&self) -> bool {
        lock(&self.admission.queue).ingress_closed
    }

    /// Non-blocking core stop request; the owner loop keeps polling so real
    /// receipts and the stopped snapshot are still drained and published.
    pub fn request_stop(&self) {
        self.backend.request_stop();
    }

    /// The one blocking core cleanup. Only the bridge owner calls this, after
    /// the stopped snapshot was drained and off every HTTP/timer thread.
    pub fn disconnect(&self) {
        self.backend.disconnect();
    }

    /// One owner-loop round: drain and publish first, then hand at most
    /// [`SUBMISSIONS_PER_DRAIN`] queued commands to the core, then drain again
    /// so their early receipts and returned credits do not wait for the next
    /// wakeup.
    pub fn poll(&mut self) {
        self.drain_and_publish();
        let mut handled = 0;
        while handled < SUBMISSIONS_PER_DRAIN {
            let Some(submission) = self.take_submission() else { break; };
            self.submit_one(submission);
            handled += 1;
        }
        if handled > 0 {
            self.drain_and_publish();
        }
    }

    fn take_submission(&self) -> Option<QueuedSubmission> {
        let mut state = lock(&self.admission.queue);
        if state.core_stopped {
            self.stop_queued(&mut state);
            return None;
        }
        state.items.pop_front()
    }

    fn submit_one(&mut self, submission: QueuedSubmission) {
        let QueuedSubmission {
            command,
            reply,
            #[cfg(feature = "daemon")]
            import,
        } = submission;
        match self.backend.try_submit(command) {
            Ok(request_id) => {
                // Register before the acceptance reply so a receipt published
                // in the same owner round can never outrun the cleanup entry.
                #[cfg(feature = "daemon")]
                if let Some(import) = import {
                    import.ticket.activate(request_id, import.stage);
                }
                let _ = reply.send(Ok(request_id));
            }
            Err(SubmitError::Busy) => {
                #[cfg(feature = "daemon")]
                if let Some(import) = import {
                    self.imports.reject(import.stage);
                }
                let _ = reply.send(Err(BridgeSubmitError::Busy));
            }
            Err(SubmitError::Stopped) => {
                #[cfg(feature = "daemon")]
                if let Some(import) = import {
                    self.imports.reject(import.stage);
                }
                self.mark_stopped();
                let _ = reply.send(Err(BridgeSubmitError::Stopped));
            }
        }
    }

    fn mark_stopped(&self) {
        let mut state = lock(&self.admission.queue);
        state.core_stopped = true;
        self.stop_queued(&mut state);
    }

    fn stop_queued(&self, state: &mut QueueState) {
        for submission in state.items.drain(..) {
            #[cfg(feature = "daemon")]
            if let Some(import) = submission.import {
                self.imports.reject(import.stage);
            }
            let _ = submission.reply.send(Err(BridgeSubmitError::Stopped));
        }
    }

    fn drain_and_publish(&mut self) {
        // The core's drain_updates already collects receipts before live output.
        let (live, results) = self.backend.drain_updates();
        let snapshot = self.backend.snapshot();
        let mut hub = lock(&self.hub);
        for event in live {
            hub.publish(PublishedEvent::LiveOutput(live_output_value(&event)));
        }
        for result in results {
            // A real receipt is the only signal that ends an import's cleanup
            // ownership; removal itself is queued to the cleanup thread.
            #[cfg(feature = "daemon")]
            self.imports.complete(result.request_id);
            hub.publish(PublishedEvent::CommandResult(CommandResponse::from(result)));
        }
        if !Arc::ptr_eq(&snapshot, &self.last_snapshot) {
            self.last_snapshot = snapshot.clone();
            hub.publish(PublishedEvent::Snapshot((*snapshot).clone()));
        }
    }
}

/// The single polling owner of a [`ServiceBridge`].
///
/// Stop ordering is fixed here: close ingress, request the core stop once,
/// keep draining real receipts and the stopped snapshot, and only then run the
/// one blocking disconnect. Every wait happens on this named thread, never on
/// an HTTP connection or a shutdown timer.
pub struct BridgeOwner {
    stop_requested: Arc<AtomicBool>,
    handle: Option<JoinHandle<PublishedView>>,
}

impl BridgeOwner {
    pub fn spawn(mut bridge: ServiceBridge) -> Self {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let requested = stop_requested.clone();
        let handle = thread::Builder::new()
            .name("agentranch-bridge-owner".into())
            .spawn(move || owner_loop(&mut bridge, &requested))
            .expect("spawn bridge owner thread");
        Self { stop_requested, handle: Some(handle) }
    }

    /// Idempotent: repeated signals only set the flag; the core stop is issued
    /// once by the owner thread after ingress closure.
    pub fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    pub fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    /// Blocking join for the single shutdown path. Returns the last published
    /// view when the owner finished; `None` if it was already joined or
    /// panicked.
    pub fn join(mut self) -> Option<PublishedView> {
        let handle = self.handle.take()?;
        handle.join().ok()
    }
}

impl Drop for BridgeOwner {
    fn drop(&mut self) {
        // Never block here: an abandoned owner still observes the request and
        // exits on its own tick.
        self.stop_requested.store(true, Ordering::Release);
    }
}

fn owner_loop(bridge: &mut ServiceBridge, stop_requested: &AtomicBool) -> PublishedView {
    loop {
        if stop_requested.load(Ordering::Acquire) {
            break;
        }
        bridge.poll();
        if stop_requested.load(Ordering::Acquire) {
            break;
        }
        bridge.wait_for_wakeup(OWNER_TICK);
    }
    // Ingress closes before the core stop, so work still queued here can never
    // be handed over after shutdown began.
    bridge.close_ingress();
    bridge.request_stop();
    loop {
        bridge.poll();
        if !bridge.published_view().snapshot.running {
            // One extra drain for a receipt queued after the snapshot read.
            bridge.poll();
            break;
        }
        bridge.wait_for_wakeup(OWNER_TICK);
    }
    let view = bridge.published_view();
    // The only blocking disconnect, on the owner thread. Nothing else may
    // stop or clean up the core.
    bridge.disconnect();
    view
}

/// Exit without a settled reply. Queued work is reported as acceptance
/// unknown rather than replayable, so a caller never resubmits it.
impl Drop for ServiceBridge {
    fn drop(&mut self) {
        let items = {
            let mut state = lock(&self.admission.queue);
            state.core_stopped = true;
            std::mem::take(&mut state.items)
        };
        for submission in items {
            // Imports still queued here keep their staging copy: uncertainty
            // is never resolved by deleting evidence or replaying the command.
            #[cfg(feature = "daemon")]
            if let Some(mut import) = submission.import {
                import.stage.retain();
            }
            let _ = submission.reply.send(Err(BridgeSubmitError::AcceptanceUnknown));
        }
    }
}

fn live_output_value(event: &LiveOutputEvent) -> Value {
    // `received_at` is a process-local Instant and is deliberately omitted.
    json!({
        "request_id": event.request_id,
        "project_id": event.project_id,
        "output": event.output,
        "received_bytes": event.received_bytes,
        "truncated": event.truncated,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
