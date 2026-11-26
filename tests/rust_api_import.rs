#![cfg(all(feature = "daemon", windows))]
//! Restricted project import: staging, rewrite and receipt-driven cleanup.
//!
//! Source fixtures live in a fresh root under the checkout `target/` directory
//! and never in AppData. HTTP cases use the in-process `HttpApp` over a raw
//! TCP client; bridge cases drive the single owner loop by hand so the moment
//! between acceptance and the real receipt is observable.

use agentranch::{
    api::{
        backend::Backend,
        bridge::{BridgeSubmitError, BridgeWake, ServiceBridge},
        files::{
            finalize_import, stage_import, ImportError, ImportOutcome, ImportRegistry,
            ImportStage,
        },
        http::{serve_connection, EventHubAccess, HttpApp},
    },
    config::Mode,
    core::service::{
        AppSnapshot, CommandResult, LiveOutputEvent, Readiness, SubmitError, UiCommand,
    },
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fixture_root() -> tempfile::TempDir {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize().unwrap();
    let base = checkout.join("target").join("task-7-api-import");
    assert_outside_appdata(&base);
    std::fs::create_dir_all(&base).unwrap();
    let root = tempfile::tempdir_in(&base).unwrap();
    assert_outside_appdata(&root.path().canonicalize().unwrap());
    root
}

fn assert_outside_appdata(path: &Path) {
    let profile_appdata =
        std::env::var_os("USERPROFILE").map(|base| PathBuf::from(base).join("AppData"));
    let env_appdata = ["APPDATA", "LOCALAPPDATA", "LOCALAPPDATALOW"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from);
    let path = comparable(path);
    for appdata in profile_appdata.into_iter().chain(env_appdata) {
        assert!(
            !path.starts_with(comparable(&appdata)),
            "import fixtures must not use AppData"
        );
    }
}

fn comparable(path: &Path) -> PathBuf {
    let spelling = path.to_string_lossy();
    let spelling = spelling.replace('/', "\\");
    PathBuf::from(
        spelling
            .strip_prefix(r"\\?\")
            .unwrap_or(&spelling)
            .to_lowercase(),
    )
}

/// Fresh isolated state root with the `imports/` input directory only.
fn fresh_state(root: &Path) -> PathBuf {
    let state = root.join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(state.join("imports")).unwrap();
    state
}

fn write_source(state: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = state.join("imports").join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn junction(link: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    let output = Command::new("cmd.exe")
        .args(["/d", "/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "junction fixture creation failed: {}",
        output.status
    );
}

fn staging_root(state: &Path) -> PathBuf {
    state.join("imports").join(".staging")
}

fn wait_until(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn wait_gone(path: &Path) {
    wait_until("staging removal", || !path.exists());
}

// ---------------------------------------------------------------------------
// stage_import / finalize_import
// ---------------------------------------------------------------------------

#[test]
fn stage_import_copies_valid_document_into_private_staging() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let source = write_source(&state, "design.md", b"# Synthetic design\n");
    assert_outside_appdata(&state);

    let stage = stage_import(&state, "design.md").expect("valid import must stage");
    let staged = stage.core_path().to_path_buf();

    assert!(staged.is_absolute(), "core path must be absolute");
    assert_ne!(staged, source, "core must never see the source path");
    assert_eq!(stage.filename(), "design.md");
    assert!(staged.starts_with(&state), "staging stays inside state dir");
    assert!(staged.starts_with(staging_root(&state)));
    assert_eq!(
        staged.parent().unwrap().parent().unwrap(),
        staging_root(&state),
        "per-request directory is directly below the private staging root"
    );
    assert_eq!(staged.file_name().unwrap(), "design.md");
    assert_eq!(std::fs::read(&staged).unwrap(), b"# Synthetic design\n");
    assert!(source.exists(), "the source file is never moved or modified");
    assert_outside_appdata(&staged.canonicalize().unwrap());
}

#[test]
fn stage_import_rejects_non_basename_and_unknown_extensions() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    write_source(&state, "design.md", b"content");
    let invalid = [
        "",
        ".",
        "..",
        "design.pdf",
        "design.md.exe",
        " design.md",
        "design.md ",
        "design.md.",
        "sub/design.md",
        "sub\\design.md",
        "C:\\synthetic\\design.md",
        "\\\\server\\share\\design.md",
        "C:design.md",
        "design?.md",
        "设计.md",
        "CON.md",
        "nul.txt",
        "com1.md",
        "lpt9.txt",
        ".md",
        ".txt",
        "*.md",
    ];
    for filename in invalid {
        assert_eq!(
            stage_import(&state, filename).unwrap_err(),
            ImportError::InvalidInput,
            "filename {filename:?} must be rejected"
        );
    }
    assert!(
        !staging_root(&state).exists(),
        "rejected names must not create staging directories"
    );
}

#[test]
fn stage_import_rejects_missing_directory_and_linked_sources() {
    let root = fixture_root();
    let state = fresh_state(root.path());

    assert_eq!(
        stage_import(&state, "absent.md").unwrap_err(),
        ImportError::InvalidInput
    );

    std::fs::create_dir(state.join("imports").join("folder.md")).unwrap();
    assert_eq!(
        stage_import(&state, "folder.md").unwrap_err(),
        ImportError::InvalidInput,
        "a directory is not a document"
    );

    let outside = root.path().join("outside");
    junction(&state.join("imports").join("link.md"), &outside);
    assert_eq!(
        stage_import(&state, "link.md").unwrap_err(),
        ImportError::InvalidInput,
        "a junction is never followed"
    );

    write_source(&state, "design.md", b"shared content");
    std::fs::hard_link(
        state.join("imports").join("design.md"),
        state.join("imports").join("alias.md"),
    )
    .unwrap();
    assert_eq!(
        stage_import(&state, "alias.md").unwrap_err(),
        ImportError::InvalidInput,
        "a multiply-linked file is rejected"
    );
    assert!(
        !staging_root(&state).exists(),
        "rejected sources must not create staging directories"
    );
}

#[test]
fn stage_import_rejects_empty_whitespace_and_invalid_utf8() {
    let root = fixture_root();
    let state = fresh_state(root.path());

    write_source(&state, "empty.md", b"");
    assert_eq!(
        stage_import(&state, "empty.md").unwrap_err(),
        ImportError::InvalidInput
    );

    write_source(&state, "blank.txt", b" \r\n\t \n");
    assert_eq!(
        stage_import(&state, "blank.txt").unwrap_err(),
        ImportError::InvalidInput
    );

    write_source(&state, "binary.md", b"\xff\xfe\xfd");
    assert_eq!(
        stage_import(&state, "binary.md").unwrap_err(),
        ImportError::InvalidInput
    );
}

#[test]
fn stage_import_enforces_the_256_kib_document_bound() {
    let root = fixture_root();
    let state = fresh_state(root.path());

    write_source(&state, "limit.md", &vec![b'a'; 256 * 1024]);
    let stage = stage_import(&state, "limit.md").expect("exactly 256 KiB is allowed");
    assert_eq!(
        std::fs::metadata(stage.core_path()).unwrap().len(),
        256 * 1024
    );
    finalize_import(stage, ImportOutcome::Completed);

    write_source(&state, "over.md", &vec![b'a'; 256 * 1024 + 1]);
    assert_eq!(
        stage_import(&state, "over.md").unwrap_err(),
        ImportError::InvalidInput
    );
}

#[test]
fn finalize_import_removes_known_outcomes_and_retains_unknown() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    write_source(&state, "done.md", b"done");
    write_source(&state, "kept.md", b"kept");

    let completed = stage_import(&state, "done.md").unwrap();
    let completed_path = completed.core_path().to_path_buf();
    finalize_import(completed, ImportOutcome::Completed);
    assert!(!completed_path.exists(), "completed staging is removed");
    assert!(!completed_path.parent().unwrap().exists());

    let rejected = stage_import(&state, "done.md").unwrap();
    let rejected_path = rejected.core_path().to_path_buf();
    finalize_import(rejected, ImportOutcome::Rejected);
    assert!(!rejected_path.exists(), "known rejection staging is removed");

    let unknown = stage_import(&state, "kept.md").unwrap();
    let unknown_path = unknown.core_path().to_path_buf();
    finalize_import(unknown, ImportOutcome::Unknown);
    assert!(
        unknown_path.exists(),
        "unknown outcomes retain staging for manual inspection"
    );
}

#[test]
fn abandoned_stage_before_acceptance_is_removed_on_drop() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    write_source(&state, "abandoned.md", b"# Abandoned\n");

    let stage = stage_import(&state, "abandoned.md").unwrap();
    let path = stage.core_path().to_path_buf();
    assert!(path.exists());
    // A cancelled HTTP request drops the stage before it reaches the bridge.
    drop(stage);
    assert!(
        !path.exists(),
        "an unaccepted stage must not leak its private copy"
    );
}

#[test]
fn registry_shutdown_retains_active_staging() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    write_source(&state, "in-flight.md", b"# In flight\n");

    let registry = ImportRegistry::new(4);
    let ticket = registry.reserve().unwrap();
    let stage = stage_import(&state, "in-flight.md").unwrap();
    let path = stage.core_path().to_path_buf();
    ticket.activate(11, stage);
    drop(registry);
    assert!(
        path.exists(),
        "forced shutdown retains accepted staging without a receipt"
    );
}

// ---------------------------------------------------------------------------
// Bridge: receipt-driven cleanup
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum FakeMode {
    /// Accepts, then keeps the result until the test delivers it.
    ManualResult,
    /// Always reports a full queue.
    Busy,
    /// Always reports a stopped core.
    Stopped,
}

struct FakeInner {
    mode: FakeMode,
    next_id: Mutex<u64>,
    commands: Mutex<Vec<UiCommand>>,
    results: Mutex<Vec<CommandResult>>,
    submit_calls: AtomicUsize,
}

#[derive(Clone)]
struct FakeBackend {
    inner: Arc<FakeInner>,
}

impl FakeBackend {
    fn new(mode: FakeMode) -> Self {
        Self {
            inner: Arc::new(FakeInner {
                mode,
                next_id: Mutex::new(1),
                commands: Mutex::new(Vec::new()),
                results: Mutex::new(Vec::new()),
                submit_calls: AtomicUsize::new(0),
            }),
        }
    }

    fn submit_calls(&self) -> usize {
        self.inner.submit_calls.load(SeqCst)
    }

    fn commands(&self) -> Vec<UiCommand> {
        self.inner.commands.lock().unwrap().clone()
    }

    fn deliver(&self, request_id: u64, result: Result<Value, agentranch::core::service::ServiceError>) {
        self.inner.results.lock().unwrap().push(CommandResult {
            request_id,
            result,
        });
    }
}

impl Backend for FakeBackend {
    fn try_submit(&self, command: UiCommand) -> Result<u64, SubmitError> {
        self.inner.submit_calls.fetch_add(1, SeqCst);
        match self.inner.mode {
            FakeMode::Busy => return Err(SubmitError::Busy),
            FakeMode::Stopped => return Err(SubmitError::Stopped),
            FakeMode::ManualResult => {}
        }
        let mut next = self.inner.next_id.lock().unwrap();
        let request_id = *next;
        *next += 1;
        self.inner.commands.lock().unwrap().push(command);
        Ok(request_id)
    }

    fn drain_updates(&mut self) -> (Vec<LiveOutputEvent>, Vec<CommandResult>) {
        let results = std::mem::take(&mut *self.inner.results.lock().unwrap());
        (Vec::new(), results)
    }

    fn snapshot(&mut self) -> Arc<AppSnapshot> {
        Arc::new(snapshot())
    }

    fn disconnect(&self) {}
}

fn snapshot() -> AppSnapshot {
    AppSnapshot {
        mode: Mode::Real,
        role: "api-import-test".into(),
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

fn import_command(stage: &ImportStage) -> UiCommand {
    UiCommand::ProjectImport {
        project_id: "00000000-0000-4000-8000-000000000001".into(),
        path: stage.core_path().to_path_buf(),
    }
}

#[test]
fn bridge_cleans_staging_only_after_the_real_receipt() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    write_source(&state, "design.md", b"# Bridge\n");
    let fake = FakeBackend::new(FakeMode::ManualResult);
    let mut bridge = ServiceBridge::with_wake(Box::new(fake.clone()), BridgeWake::new());
    let client = bridge.client();

    let stage = stage_import(&state, "design.md").unwrap();
    let staged = stage.core_path().to_path_buf();
    let pending = client.enqueue_import(import_command(&stage), stage).unwrap();
    bridge.poll();
    let request_id = pending.wait().expect("core accepts the staged import");

    assert_eq!(fake.commands().len(), 1);
    assert!(
        staged.exists(),
        "staging must survive until the real receipt arrives"
    );

    fake.deliver(request_id, Ok(json!({"imported": true})));
    bridge.poll();
    wait_gone(&staged);
    assert!(
        !staged.parent().unwrap().exists(),
        "the per-request staging directory is removed with the file"
    );
}

#[test]
fn bridge_cleans_staging_on_known_rejection() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    write_source(&state, "design.md", b"# Busy\n");

    let busy = FakeBackend::new(FakeMode::Busy);
    let mut busy_bridge = ServiceBridge::with_wake(Box::new(busy.clone()), BridgeWake::new());
    let busy_client = busy_bridge.client();
    let busy_stage = stage_import(&state, "design.md").unwrap();
    let busy_path = busy_stage.core_path().to_path_buf();
    let pending = busy_client
        .enqueue_import(import_command(&busy_stage), busy_stage)
        .unwrap();
    busy_bridge.poll();
    assert_eq!(pending.wait().unwrap_err(), BridgeSubmitError::Busy);
    wait_gone(&busy_path);
    assert_eq!(busy.submit_calls(), 1);
    drop(busy_bridge);

    let stopped = FakeBackend::new(FakeMode::Stopped);
    let mut stopped_bridge =
        ServiceBridge::with_wake(Box::new(stopped.clone()), BridgeWake::new());
    let stopped_client = stopped_bridge.client();
    let stopped_stage = stage_import(&state, "design.md").unwrap();
    let stopped_path = stopped_stage.core_path().to_path_buf();
    let pending = stopped_client
        .enqueue_import(import_command(&stopped_stage), stopped_stage)
        .unwrap();
    stopped_bridge.poll();
    assert_eq!(pending.wait().unwrap_err(), BridgeSubmitError::Stopped);
    wait_gone(&stopped_path);
}

#[test]
fn bridge_retains_staging_when_acceptance_is_unknown() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    write_source(&state, "design.md", b"# Unknown\n");
    let fake = FakeBackend::new(FakeMode::ManualResult);
    let bridge = ServiceBridge::with_wake(Box::new(fake), BridgeWake::new());
    let client = bridge.client();

    let stage = stage_import(&state, "design.md").unwrap();
    let staged = stage.core_path().to_path_buf();
    let pending = client.enqueue_import(import_command(&stage), stage).unwrap();
    // No owner loop ever runs: dropping the bridge reports acceptance
    // uncertainty, and the staging copy must be retained for inspection.
    drop(bridge);
    assert_eq!(
        pending.wait().unwrap_err(),
        BridgeSubmitError::AcceptanceUnknown
    );
    assert!(staged.exists(), "unknown acceptance retains staging");
}

#[test]
fn import_registry_is_bounded_and_releases_reservations() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    for name in ["one.md", "two.md", "three.md"] {
        write_source(&state, name, b"bounded");
    }
    let registry = ImportRegistry::new(2);

    let first = registry.reserve().expect("first reservation");
    let second = registry.reserve().expect("second reservation");
    assert!(
        registry.reserve().is_none(),
        "a full registry refuses new imports instead of evicting retained staging"
    );
    drop(second);

    let third = registry.reserve().expect("a dropped reservation frees a slot");
    first.activate(7, stage_import(&state, "one.md").unwrap());
    third.activate(
        8,
        stage_import(&state, "two.md").unwrap(),
    );
    assert_eq!(registry.pending_len(), 2);
    registry.complete(7);
    assert_eq!(registry.pending_len(), 1);
    wait_until("registry cleanup", || {
        std::fs::read_dir(staging_root(&state))
            .map(|entries| entries.count() == 1)
            .unwrap_or(true)
    });
    registry.complete(7);
    assert_eq!(registry.pending_len(), 1, "completion is removed exactly once");
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct HttpFake {
    inner: Arc<HttpFakeInner>,
}

struct HttpFakeInner {
    next_id: Mutex<u64>,
    commands: Mutex<Vec<CapturedCommand>>,
    results: Mutex<Vec<CommandResult>>,
    submit_calls: AtomicUsize,
}

struct CapturedCommand {
    command: UiCommand,
    path_existed_at_submit: bool,
}

impl HttpFake {
    fn new() -> Self {
        Self {
            inner: Arc::new(HttpFakeInner {
                next_id: Mutex::new(1),
                commands: Mutex::new(Vec::new()),
                results: Mutex::new(Vec::new()),
                submit_calls: AtomicUsize::new(0),
            }),
        }
    }

    fn commands(&self) -> Vec<CapturedCommand> {
        std::mem::take(&mut *self.inner.commands.lock().unwrap())
    }

    fn submit_calls(&self) -> usize {
        self.inner.submit_calls.load(SeqCst)
    }
}

impl Backend for HttpFake {
    fn try_submit(&self, command: UiCommand) -> Result<u64, SubmitError> {
        self.inner.submit_calls.fetch_add(1, SeqCst);
        let mut next = self.inner.next_id.lock().unwrap();
        let request_id = *next;
        *next += 1;
        let path_existed_at_submit = match &command {
            UiCommand::ProjectImport { path, .. } => path.exists(),
            _ => false,
        };
        self.inner.commands.lock().unwrap().push(CapturedCommand {
            command,
            path_existed_at_submit,
        });
        // The real receipt for this request is published on the next round.
        self.inner.results.lock().unwrap().push(CommandResult {
            request_id,
            result: Ok(json!({"accepted": true})),
        });
        Ok(request_id)
    }

    fn drain_updates(&mut self) -> (Vec<LiveOutputEvent>, Vec<CommandResult>) {
        let results = std::mem::take(&mut *self.inner.results.lock().unwrap());
        (Vec::new(), results)
    }

    fn snapshot(&mut self) -> Arc<AppSnapshot> {
        Arc::new(snapshot())
    }

    fn disconnect(&self) {}
}

struct TestServer {
    addr: SocketAddr,
    fake: HttpFake,
    stop: Arc<AtomicBool>,
    owner: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(state_dir: PathBuf) -> Self {
        let fake = HttpFake::new();
        let stop = Arc::new(AtomicBool::new(false));

        let wake = BridgeWake::new();
        let mut bridge = ServiceBridge::with_wake(Box::new(fake.clone()), wake);
        let client = Arc::new(bridge.client());
        let events = Arc::new(EventHubAccess::new(bridge.hub()));
        let app = Arc::new(
            HttpApp::new(Mode::Real, client, events)
                .with_heartbeat_interval(Duration::from_secs(15))
                .with_state_dir(state_dir),
        );

        let owner_stop = stop.clone();
        let owner = thread::spawn(move || {
            while !owner_stop.load(SeqCst) {
                bridge.poll();
                bridge.wait_for_wakeup(Duration::from_millis(20));
            }
            bridge.poll();
        });

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    if server_stop.load(SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(50), listener.accept()).await {
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
            addr,
            fake,
            stop,
            owner: Some(owner),
            server: Some(server),
        }
    }

    fn post_command(&self, body: &[u8]) -> HttpResponse {
        let mut conn = Conn::connect(self.addr);
        conn.send_json("POST", "/v1/commands", Some("application/json"), body);
        conn.read_response()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn error_code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap().to_owned()
    }
}

struct Conn {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Conn {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    fn send_json(&mut self, method: &str, path: &str, content_type: Option<&str>, body: &[u8]) {
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        request.push_str("Connection: close\r\n");
        if let Some(content_type) = content_type {
            request.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        self.stream.write_all(request.as_bytes()).unwrap();
        self.stream.write_all(body).unwrap();
    }

    fn read_response(&mut self) -> HttpResponse {
        loop {
            assert!(
                self.read_more(),
                "connection closed before response headers"
            );
            if let Some(position) = find_subslice(&self.buffer, b"\r\n\r\n") {
                let head = String::from_utf8(self.buffer[..position].to_vec()).unwrap();
                self.buffer.drain(..position + 4);
                let status = head
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse::<u16>()
                    .unwrap();
                let length = head
                    .lines()
                    .filter_map(|line| line.split_once(": "))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.parse::<usize>().unwrap())
                    .unwrap_or(0);
                while self.buffer.len() < length {
                    assert!(self.read_more(), "connection closed inside body");
                }
                let body = self.buffer.drain(..length).collect();
                return HttpResponse { status, body };
            }
        }
    }

    fn read_more(&mut self) -> bool {
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => false,
            Ok(count) => {
                self.buffer.extend_from_slice(&chunk[..count]);
                true
            }
            Err(_) => false,
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn import_request(project_id: &str, path: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "kind": "project_import",
        "args": {"project_id": project_id, "path": path}
    }))
    .unwrap()
}

#[test]
fn http_import_rewrites_the_path_to_private_staging_before_the_core() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    write_source(&state, "design.md", b"# HTTP\n");
    let server = TestServer::start(state.clone());

    let response = server.post_command(&import_request(
        "00000000-0000-4000-8000-000000000001",
        "design.md",
    ));
    assert_eq!(
        response.status,
        202,
        "body: {}",
        String::from_utf8_lossy(&response.body)
    );
    let accepted = response.json();
    assert_eq!(accepted["accepted"], json!(true));
    assert!(accepted["request_id"].as_u64().is_some());

    let captured = server.fake.commands();
    assert_eq!(captured.len(), 1);
    let (path, existed) = match &captured[0].command {
        UiCommand::ProjectImport { path, project_id } => {
            assert_eq!(project_id, "00000000-0000-4000-8000-000000000001");
            (path.clone(), captured[0].path_existed_at_submit)
        }
        other => panic!("unexpected core command: {other:?}"),
    };
    assert!(existed, "the staged file must exist when the core sees it");
    assert!(
        path.starts_with(staging_root(&state)),
        "the core receives the rewritten staging path, got {path:?}"
    );
    assert_eq!(path.file_name().unwrap(), "design.md");
    assert_ne!(path, state.join("imports").join("design.md"));

    wait_gone(&path);
    assert!(state.join("imports").join("design.md").exists());
}

#[test]
fn http_import_invalid_input_is_a_fixed_422_without_acceptance_or_echo() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let server = TestServer::start(state.clone());
    const CANARY: &str = "synthetic-import-canary";

    for path in [
        format!("C:\\{CANARY}\\design.md"),
        format!("sub/{CANARY}.md"),
        format!("{CANARY}.pdf"),
        "absent.md".to_owned(),
    ] {
        let response =
            server.post_command(&import_request("00000000-0000-4000-8000-000000000001", &path));
        assert_eq!(response.status, 422, "path {path:?} must be invalid");
        assert_eq!(response.error_code(), "invalid_import");
        assert!(
            !String::from_utf8_lossy(&response.body).contains(CANARY),
            "the error envelope must not echo request values"
        );
    }
    assert_eq!(server.fake.submit_calls(), 0, "invalid imports never reach the core");
    assert!(
        !staging_root(&state).exists(),
        "rejected imports must not create staging directories"
    );
}
