#![cfg(feature = "daemon")]
//! Shutdown ordering, signal routing and discovery integrity.
//!
//! The core is faked so acceptance, the real `stopped` receipt and the stopped
//! snapshot are deterministic. Closing a client connection is never treated as
//! shutdown or cancellation; only the single owner path may stop the core.

#[path = "support/daemon_temp.rs"]
mod daemon_temp;

use agentranch::{
    api::{
        backend::Backend,
        bridge::{BridgeOwner, BridgeSubmitError, BridgeWake, ServiceBridge},
        events::{Cursor, SubscribeResult},
        http::{serve_connection, EventHubAccess, HttpApp},
        lifecycle::{
            shutdown_with_deadline, create_isolated_state_dir, EndpointFileGuard, Ingress,
            ShutdownOutcome, ShutdownState,
        },
    },
    config::Mode,
    core::service::{
        AppSnapshot, CommandResult, LiveOutputEvent, Readiness, ServiceError, SubmitError,
        UiCommand,
    },
};
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const BUDGET: Duration = Duration::from_secs(5);

fn initial_snapshot() -> AppSnapshot {
    AppSnapshot {
        mode: Mode::Real,
        role: "shutdown-transport-test".into(),
        readiness: Readiness::NotReady,
        actor: None,
        observed_at: 0,
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

fn stopped_error() -> ServiceError {
    ServiceError {
        code: "stopped".into(),
        message: "后台服务已停止；此请求未执行".into(),
        next_step: "重新打开服务；关闭窗口不代表取消任务".into(),
    }
}

struct FakeInner {
    next_id: Mutex<u64>,
    accepted: Mutex<Vec<u64>>,
    results: Mutex<Vec<CommandResult>>,
    snapshot: Mutex<Arc<AppSnapshot>>,
    settle_on_stop: AtomicBool,
    stop_delivered: AtomicBool,
    submit_calls: AtomicUsize,
    stop_calls: AtomicUsize,
    disconnect_calls: AtomicUsize,
}

#[derive(Clone)]
struct FakeBackend {
    inner: Arc<FakeInner>,
}

impl FakeBackend {
    fn new() -> Self {
        Self::with_settlement(true)
    }

    /// Stop never settles until the test releases it; models a core that
    /// exceeds the shutdown budget.
    fn stalled() -> Self {
        Self::with_settlement(false)
    }

    fn with_settlement(settle: bool) -> Self {
        Self {
            inner: Arc::new(FakeInner {
                next_id: Mutex::new(1),
                accepted: Mutex::new(Vec::new()),
                results: Mutex::new(Vec::new()),
                snapshot: Mutex::new(Arc::new(initial_snapshot())),
                settle_on_stop: AtomicBool::new(settle),
                stop_delivered: AtomicBool::new(false),
                submit_calls: AtomicUsize::new(0),
                stop_calls: AtomicUsize::new(0),
                disconnect_calls: AtomicUsize::new(0),
            }),
        }
    }

    fn release_settlement(&self) {
        self.inner.settle_on_stop.store(true, SeqCst);
    }

    fn submit_calls(&self) -> usize {
        self.inner.submit_calls.load(SeqCst)
    }

    fn stop_calls(&self) -> usize {
        self.inner.stop_calls.load(SeqCst)
    }

    fn disconnect_calls(&self) -> usize {
        self.inner.disconnect_calls.load(SeqCst)
    }
}

impl Backend for FakeBackend {
    fn try_submit(&self, _command: UiCommand) -> Result<u64, SubmitError> {
        self.inner.submit_calls.fetch_add(1, SeqCst);
        let request_id = {
            let mut next = self.inner.next_id.lock().unwrap();
            let id = *next;
            *next += 1;
            id
        };
        self.inner.accepted.lock().unwrap().push(request_id);
        Ok(request_id)
    }

    fn drain_updates(&mut self) -> (Vec<LiveOutputEvent>, Vec<CommandResult>) {
        // A real core answers accepted-but-unprocessed work with a `stopped`
        // receipt and then publishes the stopped snapshot. Deliver both only
        // once, after a stop was requested.
        if self.inner.stop_calls.load(SeqCst) > 0
            && self.inner.settle_on_stop.load(SeqCst)
            && !self.inner.stop_delivered.swap(true, SeqCst)
        {
            let accepted = self.inner.accepted.lock().unwrap().clone();
            let mut results = self.inner.results.lock().unwrap();
            for request_id in accepted {
                results.push(CommandResult {
                    request_id,
                    result: Err(stopped_error()),
                });
            }
            let mut stopped = initial_snapshot();
            stopped.running = false;
            stopped.readiness = Readiness::Stopped;
            *self.inner.snapshot.lock().unwrap() = Arc::new(stopped);
        }
        let results = std::mem::take(&mut *self.inner.results.lock().unwrap());
        (Vec::new(), results)
    }

    fn snapshot(&mut self) -> Arc<AppSnapshot> {
        self.inner.snapshot.lock().unwrap().clone()
    }

    fn request_stop(&self) {
        self.inner.stop_calls.fetch_add(1, SeqCst);
    }

    fn disconnect(&self) {
        self.inner.disconnect_calls.fetch_add(1, SeqCst);
    }
}

/// One assembled daemon surface: an owner loop, a fake core, the shared hub
/// and a real loopback HTTP listener using the production transport.
struct Fixture {
    fake: FakeBackend,
    client: agentranch::api::bridge::BridgeClient,
    owner: Option<BridgeOwner>,
    events: Arc<EventHubAccess>,
    ingress: Arc<Ingress>,
    addr: SocketAddr,
    server_stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl Fixture {
    fn new(fake: FakeBackend) -> Self {
        let bridge = ServiceBridge::with_wake(Box::new(fake.clone()), BridgeWake::new());
        let client = bridge.client();
        let events = Arc::new(EventHubAccess::new(bridge.hub()));
        let ingress = Arc::new(Ingress::new());
        let app = Arc::new(
            HttpApp::new(Mode::Real, Arc::new(client.clone()), events.clone())
                .with_ingress(ingress.clone()),
        );
        let owner = BridgeOwner::spawn(bridge);

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let server_stop = Arc::new(AtomicBool::new(false));
        let stop = server_stop.clone();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    if stop.load(SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(50), listener.accept()).await
                    {
                        Ok(Ok((stream, _))) => {
                            let app = app.clone();
                            tokio::spawn(async move {
                                let _ = serve_connection(stream, app).await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_) => continue,
                    }
                }
            });
        });

        Self {
            fake,
            client,
            owner: Some(owner),
            events,
            ingress,
            addr,
            server_stop,
            server: Some(server),
        }
    }

    fn shutdown(&mut self, endpoint: Option<EndpointFileGuard>, deadline: Duration) -> ShutdownOutcome {
        let state = ShutdownState::new(
            self.ingress.clone(),
            self.owner.take().expect("single shutdown path"),
            self.events.clone(),
            endpoint,
        );
        run_shutdown(state, deadline)
    }

    fn frames(&self) -> Vec<agentranch::api::events::EventFrame> {
        match self.events.subscribe(Some(Cursor::current(0))) {
            Ok(SubscribeResult::Replay(frames)) => frames,
            _ => Vec::new(),
        }
    }

    fn command_result_frames(&self) -> Vec<Value> {
        self.frames()
            .into_iter()
            .filter(|frame| frame.event == "command_result")
            .map(|frame| frame.data)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server_stop.store(true, SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn run_shutdown(state: ShutdownState, deadline: Duration) -> ShutdownOutcome {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let outcome = runtime.block_on(shutdown_with_deadline(state, deadline));
    if outcome.timed_out {
        // The owner thread may still be blocked inside the transport; never
        // make the test harness wait for it.
        runtime.shutdown_background();
    } else {
        runtime.shutdown_timeout(Duration::from_secs(1));
    }
    outcome
}

#[test]
fn closing_ingress_rejects_queued_and_new_commands_without_touching_the_core() {
    let fake = FakeBackend::new();
    let bridge = ServiceBridge::with_wake(Box::new(fake.clone()), BridgeWake::new());
    let queued = bridge.client().enqueue(UiCommand::StartLocal).unwrap();
    bridge.close_ingress();
    assert_eq!(
        queued.wait().unwrap_err(),
        BridgeSubmitError::Stopped,
        "a queued command must be a known non-acceptance, never acceptance-unknown"
    );
    let refused = bridge.client().enqueue(UiCommand::StartLocal).map(|_| ());
    assert_eq!(
        refused,
        Err(BridgeSubmitError::Stopped),
        "ingress closure must refuse new work"
    );
    assert_eq!(fake.submit_calls(), 0, "queued work never reached the core");
}

#[test]
fn shutdown_drains_the_real_stopped_receipt_snapshot_and_removes_discovery() {
    let root = daemon_temp::temp_root();
    let state_dir = root.path().join("state");
    create_isolated_state_dir(&state_dir).unwrap();
    let endpoint =
        EndpointFileGuard::publish(&state_dir, "http://127.0.0.1:1", std::process::id()).unwrap();
    let endpoint_path = state_dir.join("api-endpoint.json");
    assert!(endpoint_path.is_file());

    let mut fixture = Fixture::new(FakeBackend::new());
    let accepted = fixture.client.submit(UiCommand::StartLocal).unwrap();
    assert!(fixture.fake.stop_calls() == 0, "no stop before shutdown");

    let outcome = fixture.shutdown(Some(endpoint), BUDGET);
    assert!(!outcome.timed_out);
    assert!(outcome.owner_finished);

    assert_eq!(fixture.fake.stop_calls(), 1, "core stop requested exactly once");
    assert_eq!(
        fixture.fake.disconnect_calls(),
        1,
        "exactly one blocking disconnect runs, on the owner thread"
    );

    let results = fixture.command_result_frames();
    assert_eq!(results.len(), 1, "accepted work gets its real stopped receipt");
    assert_eq!(results[0]["request_id"], accepted);
    assert_eq!(results[0]["error"]["code"], "stopped");

    let view = fixture.events.published_view();
    assert!(!view.snapshot.running, "stopped snapshot published");
    assert_eq!(view.snapshot.readiness, Readiness::Stopped);

    assert!(!endpoint_path.exists(), "discovery file removed on shutdown");
}

#[test]
fn shutdown_closes_open_sse_streams_and_refuses_new_ingress() {
    let mut fixture = Fixture::new(FakeBackend::new());
    let mut sse = RawSse::open(fixture.addr, "/v1/events", Duration::from_secs(3));
    assert_eq!(sse.status, 200);
    let initial = match sse.next_frame(Duration::from_secs(3)) {
        FrameOutcome::Frame(frame) => frame,
        other => panic!("expected the initial checkpoint, got {other:?}"),
    };
    assert!(initial.contains("event: snapshot"), "frame: {initial}");

    let outcome = fixture.shutdown(None, BUDGET);
    assert!(!outcome.timed_out);

    // Frames published while the owner drained (the real stopped snapshot)
    // may still arrive before the body ends.
    let mut drained = Vec::new();
    let closed = loop {
        match sse.next_frame(Duration::from_secs(2)) {
            FrameOutcome::Frame(frame) => drained.push(frame),
            FrameOutcome::Closed => break true,
            FrameOutcome::Timeout => break false,
        }
    };
    assert!(closed, "SSE body must end during shutdown; last frames: {drained:?}");
    assert!(
        drained.iter().any(|frame| frame.contains("\"running\":false")),
        "the stopped snapshot is streamed before closure: {drained:?}"
    );

    let late = request(fixture.addr, "GET", "/v1/events", None);
    assert_eq!(late.status, 503);
    assert_eq!(late.json()["error"]["code"], "unavailable");

    let submits_before = fixture.fake.submit_calls();
    let refused = request(
        fixture.addr,
        "POST",
        "/v1/commands",
        Some(br#"{"kind":"start_local","args":{}}"#),
    );
    assert_eq!(refused.status, 503);
    assert_eq!(refused.json()["error"]["code"], "unavailable");
    assert_eq!(
        fixture.fake.submit_calls(),
        submits_before,
        "a refused command never reaches the bridge queue"
    );
}

#[test]
fn shutdown_deadline_reports_timeout_without_fabricating_receipts() {
    let fake = FakeBackend::stalled();
    let mut fixture = Fixture::new(fake.clone());
    let accepted = fixture.client.submit(UiCommand::StartLocal).unwrap();

    let started = Instant::now();
    let outcome = fixture.shutdown(None, Duration::from_millis(300));
    let elapsed = started.elapsed();

    assert!(outcome.timed_out, "over-budget stop must report timeout");
    assert!(!outcome.owner_finished);
    assert!(elapsed < Duration::from_secs(2), "deadline bounds the wait: {elapsed:?}");
    assert!(fixture.events.is_closed(), "SSE still closes on timeout");
    assert_eq!(fixture.events.head(), 0, "no invented transport events");
    assert!(
        fixture.command_result_frames().is_empty(),
        "no receipt may be fabricated for request {accepted}"
    );

    // Let the owner thread finish so it does not outlive the test process.
    fake.release_settlement();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fake.disconnect_calls() == 0 {
        assert!(Instant::now() < deadline, "owner thread never finished");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[cfg(windows)]
fn real_process_ctrl_break_stops_within_budget_and_removes_discovery() {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    let root = daemon_temp::temp_root();
    let state_dir = root.path().join("state");
    let mut child = ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_agentranchd"))
            .arg("--state-dir")
            .arg(&state_dir)
            .arg("--port")
            .arg("0")
            .arg("--real")
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = child.0.stderr.take();

    let endpoint_path = state_dir.join("api-endpoint.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !endpoint_path.is_file() {
        assert!(
            Instant::now() < deadline,
            "daemon did not publish its endpoint in time"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let metadata: Value = serde_json::from_slice(&std::fs::read(&endpoint_path).unwrap()).unwrap();
    let url = metadata["url"].as_str().unwrap();
    let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let health = request(addr, "GET", "/v1/health", None);
    assert_eq!(health.status, 200);
    assert_eq!(health.json()["instance_id"], metadata["instance_id"]);

    let started = Instant::now();
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.0.id()) };
    assert_ne!(
        sent,
        0,
        "GenerateConsoleCtrlEvent failed: {}",
        std::io::Error::last_os_error()
    );

    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "daemon did not exit after Ctrl+Break"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let elapsed = started.elapsed();
    let mut diagnostics = String::new();
    if let Some(pipe) = stderr.as_mut() {
        let _ = pipe.read_to_string(&mut diagnostics);
    }

    assert_eq!(
        status.code(),
        Some(0),
        "clean shutdown exit status; stderr: {diagnostics}"
    );
    assert!(
        elapsed <= BUDGET,
        "shutdown exceeded the five-second budget: {elapsed:?}"
    );
    assert!(!endpoint_path.exists(), "discovery file removed on shutdown");
}

/// Kills the daemon if an assertion fires before it exited, so no acceptance
/// process (and no locked executable) outlives the test.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct HttpReply {
    status: u16,
    body: Vec<u8>,
}

impl HttpReply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

fn request(addr: SocketAddr, method: &str, path: &str, body: Option<&[u8]>) -> HttpReply {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    if let Some(body) = body {
        stream.write_all(body).unwrap();
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = find_subslice(&raw, b"\r\n\r\n").expect("response head terminator");
    let head = String::from_utf8(raw[..split].to_vec()).unwrap();
    let status = head
        .split("\r\n")
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    HttpReply { status, body: raw[split + 4..].to_vec() }
}

enum FrameOutcome {
    Frame(String),
    Closed,
    Timeout,
}

impl std::fmt::Debug for FrameOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(frame) => write!(formatter, "Frame({frame:?})"),
            Self::Closed => write!(formatter, "Closed"),
            Self::Timeout => write!(formatter, "Timeout"),
        }
    }
}

impl PartialEq for FrameOutcome {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Frame(left), Self::Frame(right)) => left == right,
            (Self::Closed, Self::Closed) | (Self::Timeout, Self::Timeout) => true,
            _ => false,
        }
    }
}

/// Minimal independent chunked SSE reader: status line/headers are asserted on
/// the wire and each frame is decoded from the chunked body.
struct RawSse {
    status: u16,
    stream: TcpStream,
    chunked: bool,
    raw: Vec<u8>,
    decoded: Vec<u8>,
}

impl RawSse {
    fn open(addr: SocketAddr, path: &str, idle: Duration) -> Self {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .unwrap();
        stream.set_read_timeout(Some(idle)).unwrap();
        let mut raw = Vec::new();
        let split = loop {
            let mut chunk = [0u8; 1024];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0, "connection closed before SSE headers");
            raw.extend_from_slice(&chunk[..count]);
            if let Some(position) = find_subslice(&raw, b"\r\n\r\n") {
                break position;
            }
        };
        let head = String::from_utf8(raw[..split].to_vec()).unwrap();
        let status = head
            .split("\r\n")
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let chunked = head
            .split("\r\n")
            .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"));
        Self { status, stream, chunked, raw: raw[split + 4..].to_vec(), decoded: Vec::new() }
    }

    fn next_frame(&mut self, wait: Duration) -> FrameOutcome {
        self.stream.set_read_timeout(Some(wait)).unwrap();
        loop {
            if let Some(position) = find_subslice(&self.decoded, b"\n\n") {
                let frame: Vec<u8> = self.decoded.drain(..position + 2).collect();
                return FrameOutcome::Frame(String::from_utf8(frame).unwrap());
            }
            match self.read_chunk() {
                Chunk::Some(bytes) if !bytes.is_empty() => self.decoded.extend(bytes),
                Chunk::Some(_) => {}
                Chunk::End => return FrameOutcome::Closed,
                Chunk::Timeout => return FrameOutcome::Timeout,
            }
        }
    }

    fn read_chunk(&mut self) -> Chunk {
        if !self.chunked {
            return match self.fill() {
                Fill::Data => Chunk::Some(std::mem::take(&mut self.raw)),
                Fill::End => Chunk::End,
                Fill::Timeout => Chunk::Timeout,
            };
        }
        let line = match self.read_line() {
            Line::Line(line) => line,
            Line::End => return Chunk::End,
            Line::Timeout => return Chunk::Timeout,
        };
        let size = match usize::from_str_radix(line.trim(), 16) {
            Ok(size) => size,
            Err(_) => return Chunk::End,
        };
        if size == 0 {
            return Chunk::End;
        }
        match self.take(size) {
            Take::Bytes(bytes) => {
                // Trailing CRLF of the chunk.
                let _ = self.take(2);
                Chunk::Some(bytes)
            }
            Take::End => Chunk::End,
            Take::Timeout => Chunk::Timeout,
        }
    }

    fn read_line(&mut self) -> Line {
        loop {
            if let Some(position) = find_subslice(&self.raw, b"\r\n") {
                let line = String::from_utf8(self.raw[..position].to_vec()).unwrap();
                self.raw.drain(..position + 2);
                return Line::Line(line);
            }
            match self.fill() {
                Fill::Data => {}
                Fill::End => return Line::End,
                Fill::Timeout => return Line::Timeout,
            }
        }
    }

    fn take(&mut self, count: usize) -> Take {
        while self.raw.len() < count {
            match self.fill() {
                Fill::Data => {}
                Fill::End => return Take::End,
                Fill::Timeout => return Take::Timeout,
            }
        }
        Take::Bytes(self.raw.drain(..count).collect())
    }

    fn fill(&mut self) -> Fill {
        let mut chunk = [0u8; 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => Fill::End,
            Ok(count) => {
                self.raw.extend_from_slice(&chunk[..count]);
                Fill::Data
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Fill::Timeout
            }
            Err(_) => Fill::End,
        }
    }
}

enum Chunk {
    Some(Vec<u8>),
    End,
    Timeout,
}

enum Fill {
    Data,
    End,
    Timeout,
}

enum Line {
    Line(String),
    End,
    Timeout,
}

enum Take {
    Bytes(Vec<u8>),
    End,
    Timeout,
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}
