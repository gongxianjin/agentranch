//! Fake-Backend tests for the single ServiceBridge owner. The fake implements
//! only `Backend`; it never constructs a real ServiceHandle or touches files.

use agentranch::{
    api::{
        backend::Backend,
        bridge::{
            BridgeSubmitError, BridgeWake, ServiceBridge, BRIDGE_QUEUE_LIMIT,
            SUBMISSIONS_PER_DRAIN,
        },
        events::{Cursor, EventFrame, SubscribeResult},
    },
    config::Mode,
    core::{
        live_output::LiveOutput,
        service::{
            AppSnapshot, CommandResult, LiveOutputEvent, Readiness, ServiceError, SubmitError,
            UiCommand,
        },
    },
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const FAKE_CORE_CREDITS: usize = 32;

fn snapshot(observed_at: u64) -> AppSnapshot {
    AppSnapshot {
        mode: Mode::Real,
        role: "bridge-test".into(),
        readiness: Readiness::NotReady,
        actor: None,
        observed_at,
        running: true,
        dependencies: vec![],
        runs: vec![],
        nodes: vec![],
        approvals: vec![],
        errors: vec![],
        unfinished: 0,
        projects: vec![],
        gateway: None,
        provision_ledger: None,
    }
}

fn live_event(request_id: u64, project_id: &str, output: LiveOutput) -> LiveOutputEvent {
    LiveOutputEvent {
        request_id,
        project_id: project_id.into(),
        output,
        received_at: Instant::now(),
        received_bytes: 0,
        truncated: false,
    }
}

struct FakeInner {
    next_id: Mutex<u64>,
    commands: Mutex<Vec<UiCommand>>,
    live: Mutex<Vec<LiveOutputEvent>>,
    results: Mutex<Vec<CommandResult>>,
    credits: AtomicUsize,
    snapshot: Mutex<Arc<AppSnapshot>>,
    next_submit_error: Mutex<Option<SubmitError>>,
    auto_complete: AtomicBool,
    submit_calls: AtomicUsize,
    drain_calls: AtomicUsize,
    disconnect_calls: AtomicUsize,
}

/// Backend-only fake core. Results are supplied explicitly so a synchronization
/// point can hold the business receipt while acceptance is already observable.
#[derive(Clone)]
struct FakeBackend {
    inner: Arc<FakeInner>,
}

impl FakeBackend {
    fn new() -> Self {
        Self {
            inner: Arc::new(FakeInner {
                next_id: Mutex::new(1),
                commands: Mutex::new(Vec::new()),
                live: Mutex::new(Vec::new()),
                results: Mutex::new(Vec::new()),
                credits: AtomicUsize::new(0),
                snapshot: Mutex::new(Arc::new(snapshot(10))),
                next_submit_error: Mutex::new(None),
                auto_complete: AtomicBool::new(false),
                submit_calls: AtomicUsize::new(0),
                drain_calls: AtomicUsize::new(0),
                disconnect_calls: AtomicUsize::new(0),
            }),
        }
    }

    fn complete(&self, request_id: u64) {
        self.complete_with(request_id, Ok(json!({"accepted": true})));
    }

    fn complete_with(&self, request_id: u64, result: Result<Value, ServiceError>) {
        self.inner
            .results
            .lock()
            .unwrap()
            .push(CommandResult { request_id, result });
    }

    fn push_live(&self, event: LiveOutputEvent) {
        self.inner.live.lock().unwrap().push(event);
    }

    fn set_snapshot(&self, snapshot: Arc<AppSnapshot>) {
        *self.inner.snapshot.lock().unwrap() = snapshot;
    }

    fn snapshot_arc(&self) -> Arc<AppSnapshot> {
        self.inner.snapshot.lock().unwrap().clone()
    }

    fn fail_next_submit(&self, error: SubmitError) {
        *self.inner.next_submit_error.lock().unwrap() = Some(error);
    }

    fn set_auto_complete(&self, value: bool) {
        self.inner.auto_complete.store(value, SeqCst);
    }

    fn credits(&self) -> usize {
        self.inner.credits.load(SeqCst)
    }

    fn submit_calls(&self) -> usize {
        self.inner.submit_calls.load(SeqCst)
    }

    fn drain_calls(&self) -> usize {
        self.inner.drain_calls.load(SeqCst)
    }
}

impl Backend for FakeBackend {
    fn try_submit(&self, command: UiCommand) -> Result<u64, SubmitError> {
        self.inner.submit_calls.fetch_add(1, SeqCst);
        if let Some(error) = self.inner.next_submit_error.lock().unwrap().take() {
            return Err(error);
        }
        if self.inner.credits.load(SeqCst) >= FAKE_CORE_CREDITS {
            return Err(SubmitError::Busy);
        }
        let mut next = self.inner.next_id.lock().unwrap();
        let id = *next;
        *next += 1;
        self.inner.credits.fetch_add(1, SeqCst);
        self.inner.commands.lock().unwrap().push(command);
        drop(next);
        if self.inner.auto_complete.load(SeqCst) {
            self.complete(id);
        }
        Ok(id)
    }

    fn drain_updates(&mut self) -> (Vec<LiveOutputEvent>, Vec<CommandResult>) {
        self.inner.drain_calls.fetch_add(1, SeqCst);
        let results = std::mem::take(&mut *self.inner.results.lock().unwrap());
        self.inner.credits.fetch_sub(results.len(), SeqCst);
        let live = std::mem::take(&mut *self.inner.live.lock().unwrap());
        (live, results)
    }

    fn snapshot(&mut self) -> Arc<AppSnapshot> {
        self.snapshot_arc()
    }

    fn disconnect(&self) {
        self.inner.disconnect_calls.fetch_add(1, SeqCst);
    }
}

fn commands_seen(fake: &FakeBackend) -> usize {
    fake.inner.commands.lock().unwrap().len()
}

fn replay(bridge: &ServiceBridge) -> Vec<EventFrame> {
    let hub = bridge.hub();
    let hub = hub.lock().unwrap();
    match hub.subscribe(Some(Cursor::current(0))).unwrap() {
        SubscribeResult::Replay(frames) => frames,
        other => panic!("expected replay, got {other:?}"),
    }
}

fn start_local() -> UiCommand {
    UiCommand::StartLocal
}

#[test]
fn submit_reports_acceptance_before_the_final_receipt() {
    let fake = FakeBackend::new();
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));

    let pending = bridge.enqueue(start_local()).unwrap();
    bridge.poll();

    assert_eq!(pending.wait(), Ok(1), "acceptance must arrive while the result is still held");
    assert_eq!(fake.submit_calls(), 1);
    assert_eq!(commands_seen(&fake), 1);
    assert!(replay(&bridge).is_empty(), "no receipt may be published before the core releases it");
    assert_eq!(bridge.published_view().head, 0);

    // Synchronization point released: the same request finally produces a receipt.
    fake.complete(1);
    bridge.poll();
    let frames = replay(&bridge);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event, "command_result");
    assert_eq!(frames[0].data, json!({"request_id": 1, "result": {"accepted": true}}));
}

#[test]
fn live_output_precedes_command_result_within_one_drain_batch() {
    let fake = FakeBackend::new();
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));
    let pending = bridge.enqueue(start_local()).unwrap();
    bridge.poll();
    assert_eq!(pending.wait(), Ok(1));

    fake.push_live(live_event(1, "project-a", LiveOutput::Delta("hello".into())));
    fake.complete(1);
    bridge.poll();

    let frames = replay(&bridge);
    assert_eq!(frames.iter().map(|frame| frame.event.as_str()).collect::<Vec<_>>(), ["live_output", "command_result"]);
    assert_eq!(frames[0].data, json!({
        "request_id": 1,
        "project_id": "project-a",
        "output": {"kind": "delta", "text": "hello"},
        "received_bytes": 0,
        "truncated": false,
    }));
    assert!(frames[0].data.get("received_at").is_none(), "process-local Instant must not be published");
    assert_eq!(frames[1].data["request_id"], 1);
    assert_eq!(frames[1].id.unwrap().seq, frames[0].id.unwrap().seq + 1);
}

#[test]
fn credits_return_without_any_sse_reader() {
    let fake = FakeBackend::new();
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));
    let stale_view = bridge.published_view();

    for request_id in 1..=2 {
        let pending = bridge.enqueue(start_local()).unwrap();
        bridge.poll();
        assert_eq!(pending.wait(), Ok(request_id));
    }
    assert_eq!(fake.credits(), 2, "accepted work holds core credits");

    fake.complete(1);
    fake.complete(2);
    bridge.poll();

    assert_eq!(fake.credits(), 0, "draining returns credits even with no reader consuming frames");
    assert!(fake.drain_calls() >= 2);
    assert_eq!(replay(&bridge).len(), 2);
    let _ = stale_view;

    let pending = bridge.enqueue(start_local()).unwrap();
    bridge.poll();
    assert_eq!(pending.wait(), Ok(3), "returned credits allow further acceptance");
}

#[test]
fn full_bridge_queue_rejects_with_busy_and_never_enqueues_extra_work() {
    let fake = FakeBackend::new();
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));

    let mut pending = Vec::new();
    for _ in 0..BRIDGE_QUEUE_LIMIT {
        pending.push(bridge.enqueue(start_local()).unwrap());
    }
    assert!(matches!(bridge.enqueue(start_local()), Err(BridgeSubmitError::Busy)));
    assert_eq!(fake.submit_calls(), 0, "a full queue must not deliver or retry anything");

    let rounds = BRIDGE_QUEUE_LIMIT / SUBMISSIONS_PER_DRAIN;
    for _ in 0..rounds {
        bridge.poll();
    }
    for (index, pending) in pending.into_iter().enumerate() {
        assert_eq!(pending.wait(), Ok(index as u64 + 1));
    }
    assert_eq!(fake.submit_calls(), BRIDGE_QUEUE_LIMIT);
}

#[test]
fn a_poll_drains_again_after_at_most_eight_submissions() {
    let fake = FakeBackend::new();
    fake.set_auto_complete(true);
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));

    let pending: Vec<_> = (0..SUBMISSIONS_PER_DRAIN).map(|_| bridge.enqueue(start_local()).unwrap()).collect();
    bridge.poll();

    for (index, pending) in pending.into_iter().enumerate() {
        assert_eq!(pending.wait(), Ok(index as u64 + 1));
    }
    assert_eq!(fake.credits(), 0, "the same poll must drain the batch it just submitted");
    let frames = replay(&bridge);
    assert_eq!(frames.len(), SUBMISSIONS_PER_DRAIN);
    assert!(frames.iter().all(|frame| frame.event == "command_result"));
    assert_eq!(frames[SUBMISSIONS_PER_DRAIN - 1].id.unwrap().seq, SUBMISSIONS_PER_DRAIN as u64);
    assert_eq!(fake.drain_calls(), 2, "one drain before the batch and one after it");
}

#[test]
fn client_disconnect_never_retracts_queued_or_accepted_work() {
    let fake = FakeBackend::new();
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));

    let pending = bridge.enqueue(start_local()).unwrap();
    drop(pending);
    bridge.poll();
    assert_eq!(fake.submit_calls(), 1, "the HTTP waiter vanishing must not remove queued work");

    let pending = bridge.enqueue(start_local()).unwrap();
    bridge.poll();
    assert_eq!(pending.wait(), Ok(2));

    fake.complete(2);
    bridge.poll();
    let frames = replay(&bridge);
    assert_eq!(frames.len(), 1, "the accepted request keeps its receipt despite the missing reader");
    assert_eq!(frames[0].data["request_id"], 2);
}

#[test]
fn published_view_never_reads_an_unpublished_core_snapshot() {
    let fake = FakeBackend::new();
    let initial = fake.snapshot_arc();
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));

    assert!(Arc::ptr_eq(&bridge.published_view().snapshot, &initial));
    assert!(replay(&bridge).is_empty(), "the initial snapshot is the checkpoint, not an event");

    bridge.poll();
    assert!(replay(&bridge).is_empty(), "an unchanged snapshot Arc is not republished");

    let later = Arc::new(snapshot(99));
    fake.set_snapshot(later.clone());
    assert_eq!(bridge.published_view().snapshot.observed_at, 10, "view reads only the hub");

    bridge.poll();
    let frames = replay(&bridge);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event, "snapshot");
    assert_eq!(frames[0].data["observed_at"], 99);
    let view = bridge.published_view();
    assert_eq!(view.snapshot.observed_at, 99);
    assert_eq!(view.snapshot_seq, frames[0].id.unwrap().seq);
    assert_eq!(view.snapshot_seq, view.head);

    // The same core Arc is not published again on the next poll.
    bridge.poll();
    assert_eq!(replay(&bridge).len(), 1, "one core snapshot maps to exactly one published event");
    assert_eq!(bridge.published_view().snapshot_seq, frames[0].id.unwrap().seq);

    // A later result advances the head without re-publishing the snapshot.
    let pending = bridge.enqueue(start_local()).unwrap();
    bridge.poll();
    assert_eq!(pending.wait(), Ok(1));
    fake.complete(1);
    bridge.poll();
    let view = bridge.published_view();
    assert_eq!(view.snapshot_seq, frames[0].id.unwrap().seq);
    assert_eq!(view.head, frames[0].id.unwrap().seq + 1);
}

#[test]
fn bridge_exit_reports_acceptance_uncertainty_without_retrying() {
    let fake = FakeBackend::new();
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));

    let accepted = bridge.enqueue(start_local()).unwrap();
    bridge.poll();
    assert_eq!(accepted.wait(), Ok(1));

    let queued = bridge.enqueue(start_local()).unwrap();
    drop(bridge);

    assert_eq!(queued.wait(), Err(BridgeSubmitError::AcceptanceUnknown));
    assert_eq!(fake.submit_calls(), 1, "exit must not replay the unsettled submission");
}

#[test]
fn stopped_core_is_reported_as_unavailable_without_further_submission() {
    let fake = FakeBackend::new();
    fake.fail_next_submit(SubmitError::Stopped);
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));

    let pending = bridge.enqueue(start_local()).unwrap();
    bridge.poll();
    assert_eq!(pending.wait(), Err(BridgeSubmitError::Stopped));
    assert!(matches!(bridge.enqueue(start_local()), Err(BridgeSubmitError::Stopped)));
    assert_eq!(fake.submit_calls(), 1, "a stopped core is not retried");
}

#[test]
fn busy_core_maps_to_busy_without_retry() {
    let fake = FakeBackend::new();
    fake.fail_next_submit(SubmitError::Busy);
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));

    let pending = bridge.enqueue(start_local()).unwrap();
    bridge.poll();
    assert_eq!(pending.wait(), Err(BridgeSubmitError::Busy));
    assert_eq!(fake.submit_calls(), 1);

    let pending = bridge.enqueue(start_local()).unwrap();
    bridge.poll();
    assert_eq!(pending.wait(), Ok(1), "a later attempt is a new request, not a retry of the rejected one");
}

#[test]
fn command_business_errors_keep_the_m0_error_shape() {
    let fake = FakeBackend::new();
    let mut bridge = ServiceBridge::new(Box::new(fake.clone()));
    let pending = bridge.enqueue(start_local()).unwrap();
    bridge.poll();
    assert_eq!(pending.wait(), Ok(1));

    fake.complete_with(1, Err(ServiceError {
        code: "not_allowed".into(),
        message: "rejected".into(),
        next_step: "retry later".into(),
    }));
    bridge.poll();

    let frames = replay(&bridge);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, json!({
        "request_id": 1,
        "error": {"code": "not_allowed", "message": "rejected", "next_step": "retry later"},
    }));
}

#[test]
fn wake_notifications_survive_the_gap_between_notify_and_wait() {
    let wake = BridgeWake::new();
    assert!(!wake.wait(Duration::from_millis(10)));
    wake.notify();
    let started = Instant::now();
    assert!(wake.wait(Duration::from_secs(5)));
    assert!(started.elapsed() < Duration::from_secs(1));

    let fake = FakeBackend::new();
    let bridge = ServiceBridge::new(Box::new(fake));
    let repaint = bridge.repaint_callback();
    repaint();
    assert!(bridge.wait_for_wakeup(Duration::from_secs(1)));
}
