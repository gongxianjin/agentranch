#![cfg(all(feature = "daemon", windows))]
//! Settings and artifact boundary tests for the isolated daemon API.
//!
//! The HTTP surface is exercised over a raw TCP client against the in-process
//! `HttpApp`, with a fresh state root under the checkout `target/` directory.
//! Every fixture asserts that it never touches AppData.

use agentranch::{
    api::{
        backend::Backend,
        bridge::{BridgeWake, ServiceBridge},
        http::{serve_connection, EventHubAccess, HttpApp},
    },
    config::Mode,
    core::projects::ProjectSnapshot,
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
        atomic::{AtomicBool, Ordering::SeqCst},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use uuid::Uuid;

const SETTINGS_PATH: &str = "/v1/settings/connection";

fn project_entry(id: Uuid) -> ProjectSnapshot {
    serde_json::from_value(json!({
        "id": id.to_string(),
        "name": "artifact-fixture",
        "phase": "confirmed",
        "document": null,
        "tasks": [],
        "actions": [],
        "blockers": [],
        "can_confirm": true,
        "can_split": false,
        "can_start": false,
        "can_test": false,
        "can_repair": false,
        "can_accept": false
    }))
    .unwrap()
}

fn snapshot() -> AppSnapshot {
    snapshot_with_projects(&[])
}

fn snapshot_with_projects(projects: &[Uuid]) -> AppSnapshot {
    AppSnapshot {
        mode: Mode::Real,
        role: "api-files-test".into(),
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
        projects: projects.iter().copied().map(project_entry).collect(),
        gateway: None,
        provision_ledger: None,
    }
}

struct FakeInner {
    next_id: Mutex<u64>,
    snapshot: Mutex<Arc<AppSnapshot>>,
    results: Mutex<Vec<CommandResult>>,
}

#[derive(Clone)]
struct FakeBackend {
    inner: Arc<FakeInner>,
}

impl FakeBackend {
    fn new() -> Self {
        Self::with_projects(&[])
    }

    fn with_projects(projects: &[Uuid]) -> Self {
        Self {
            inner: Arc::new(FakeInner {
                next_id: Mutex::new(1),
                snapshot: Mutex::new(Arc::new(snapshot_with_projects(projects))),
                results: Mutex::new(Vec::new()),
            }),
        }
    }
}

impl Backend for FakeBackend {
    fn try_submit(&self, _command: UiCommand) -> Result<u64, SubmitError> {
        let mut next = self.inner.next_id.lock().unwrap();
        let request_id = *next;
        *next += 1;
        Ok(request_id)
    }

    fn drain_updates(&mut self) -> (Vec<LiveOutputEvent>, Vec<CommandResult>) {
        let results = std::mem::take(&mut *self.inner.results.lock().unwrap());
        (Vec::new(), results)
    }

    fn snapshot(&mut self) -> Arc<AppSnapshot> {
        self.inner.snapshot.lock().unwrap().clone()
    }

    fn disconnect(&self) {}
}

struct TestServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    owner: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(state_dir: PathBuf) -> Self {
        Self::start_with_projects(state_dir, &[])
    }

    fn start_with_projects(state_dir: PathBuf, projects: &[Uuid]) -> Self {
        let fake = FakeBackend::with_projects(projects);
        let stop = Arc::new(AtomicBool::new(false));

        let wake = BridgeWake::new();
        let mut bridge = ServiceBridge::with_wake(Box::new(fake), wake);
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
            stop,
            owner: Some(owner),
            server: Some(server),
        }
    }

    fn get(&self, path: &str) -> HttpResponse {
        self.request("GET", path, None, b"")
    }

    fn put_json(&self, path: &str, body: &[u8]) -> HttpResponse {
        self.request("PUT", path, Some("application/json"), body)
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> HttpResponse {
        let mut conn = Conn::connect(self.addr);
        conn.send_json(method, path, content_type, body);
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

enum ReadOutcome {
    Data,
    Eof,
    Timeout,
}

struct Conn {
    stream: TcpStream,
    buffer: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn error_code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap().to_owned()
    }
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

    fn write_all(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }

    fn send_head(&mut self, method: &str, path: &str, headers: &[(&str, &str)]) {
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        for (name, value) in headers {
            request.push_str(name);
            request.push_str(": ");
            request.push_str(value);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        self.write_all(request.as_bytes());
    }

    fn send_json(&mut self, method: &str, path: &str, content_type: Option<&str>, body: &[u8]) {
        let mut headers = vec![("Connection", "close")];
        if let Some(content_type) = content_type {
            headers.push(("Content-Type", content_type));
        }
        let length = body.len().to_string();
        headers.push(("Content-Length", length.as_str()));
        self.send_head(method, path, &headers);
        self.write_all(body);
    }

    fn read_more(&mut self) -> ReadOutcome {
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => ReadOutcome::Eof,
            Ok(count) => {
                self.buffer.extend_from_slice(&chunk[..count]);
                ReadOutcome::Data
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                ReadOutcome::Timeout
            }
            Err(_) => ReadOutcome::Eof,
        }
    }

    fn read_head(&mut self) -> (u16, Vec<(String, String)>) {
        loop {
            if let Some(position) = find_subslice(&self.buffer, b"\r\n\r\n") {
                let head = String::from_utf8(self.buffer[..position].to_vec()).unwrap();
                self.buffer.drain(..position + 4);
                let mut lines = head.split("\r\n");
                let status = lines
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse::<u16>()
                    .unwrap();
                let headers = lines
                    .filter_map(|line| line.split_once(": "))
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
                    .collect();
                return (status, headers);
            }
            assert!(
                matches!(self.read_more(), ReadOutcome::Data),
                "connection closed before response headers"
            );
        }
    }

    fn read_response(&mut self) -> HttpResponse {
        let (status, headers) = self.read_head();
        let chunked = headers
            .iter()
            .any(|(name, value)| name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
        let body = if chunked {
            self.read_chunked_body()
        } else {
            let length = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .map(|(_, value)| value.parse::<usize>().unwrap())
                .unwrap_or(0);
            while self.buffer.len() < length {
                assert!(
                    matches!(self.read_more(), ReadOutcome::Data),
                    "connection closed before response body"
                );
            }
            self.buffer.drain(..length).collect()
        };
        HttpResponse {
            status,
            headers,
            body,
        }
    }

    fn read_line(&mut self) -> String {
        loop {
            if let Some(position) = find_subslice(&self.buffer, b"\r\n") {
                let line = String::from_utf8(self.buffer[..position].to_vec()).unwrap();
                self.buffer.drain(..position + 2);
                return line;
            }
            assert!(
                matches!(self.read_more(), ReadOutcome::Data),
                "connection closed inside chunk header"
            );
        }
    }

    fn read_exact_bytes(&mut self, count: usize) -> Vec<u8> {
        while self.buffer.len() < count {
            assert!(
                matches!(self.read_more(), ReadOutcome::Data),
                "connection closed inside chunk"
            );
        }
        let rest = self.buffer.split_off(count);
        std::mem::replace(&mut self.buffer, rest)
    }

    fn read_chunked_body(&mut self) -> Vec<u8> {
        let mut body = Vec::new();
        loop {
            let line = self.read_line();
            let size = usize::from_str_radix(line.trim(), 16).unwrap();
            if size == 0 {
                return body;
            }
            body.extend_from_slice(&self.read_exact_bytes(size));
            assert_eq!(self.read_exact_bytes(2), b"\r\n");
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn fixture_root() -> tempfile::TempDir {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize().unwrap();
    let base = checkout.join("target").join("task-6-api-files");
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
            "API file fixtures must not use AppData"
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

fn fresh_state(root: &Path) -> PathBuf {
    let state = root.join("state");
    std::fs::create_dir(&state).unwrap();
    state
}

fn settings_json() -> Value {
    json!({
        "schema": 2,
        "model": {
            "base_url": "https://model.example",
            "api_key": "sk-api-files-canary",
            "model": "test-model"
        },
        "workers": [
            {"id": "worker-split", "role": "split", "host": "192.0.2.10", "username": "root", "password": "split-secret"},
            {"id": "worker-develop", "role": "develop", "host": "192.0.2.11", "username": "root", "password": "develop-secret"},
            {"id": "worker-test", "role": "test", "host": "192.0.2.12", "username": "root", "password": "test-secret"},
            {"id": "worker-review", "role": "review", "host": "192.0.2.13", "username": "root", "password": "review-secret"}
        ]
    })
}

fn put_settings(server: &TestServer, value: &Value) -> HttpResponse {
    server.put_json(SETTINGS_PATH, &serde_json::to_vec(value).unwrap())
}

fn artifact_dir(state: &Path, project: Uuid) -> PathBuf {
    state
        .join("projects")
        .join(project.to_string())
        .join("product")
}

fn write_artifact(state: &Path, project: Uuid, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = artifact_dir(state, project).join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
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

#[test]
fn settings_round_trip_keeps_schema_two_and_isolated_test_values() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let server = TestServer::start(state.clone());
    let expected = settings_json();

    let absent = server.get(SETTINGS_PATH);
    assert_eq!(absent.status, 404);
    assert_eq!(absent.error_code(), "not_found");

    let saved = put_settings(&server, &expected);
    assert_eq!(saved.status, 200, "PUT settings must succeed");
    assert_eq!(saved.json(), json!({"saved": true}));

    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(state.join("connection-settings.json")).unwrap())
            .unwrap();
    assert_eq!(on_disk, expected, "atomic save must persist the exact schema 2 value");

    let read = server.get(SETTINGS_PATH);
    assert_eq!(read.status, 200);
    assert_eq!(read.json(), expected, "GET returns the isolated test values verbatim");
    assert_eq!(read.header("cache-control").as_deref(), Some("no-store"));
}

#[test]
fn settings_absent_is_404_and_corrupt_is_fixed_internal() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    std::fs::write(state.join("connection-settings.json"), b"{ this is not json").unwrap();
    let server = TestServer::start(state.clone());

    let corrupt = server.get(SETTINGS_PATH);
    assert_eq!(corrupt.status, 500);
    assert_eq!(corrupt.error_code(), "internal");
    let body = String::from_utf8_lossy(&corrupt.body);
    assert!(!body.contains("connection-settings"), "corrupt response must not echo the file path");
    assert!(!body.contains("this is not json"), "corrupt response must not echo file bytes");
    assert_eq!(corrupt.header("cache-control").as_deref(), Some("no-store"));
}

#[test]
fn settings_put_distinguishes_bad_json_schema_and_media_type() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let server = TestServer::start(state.clone());

    let media = server.request(
        "PUT",
        SETTINGS_PATH,
        Some("text/plain"),
        serde_json::to_vec(&settings_json()).unwrap().as_slice(),
    );
    assert_eq!(media.status, 415);
    assert_eq!(media.error_code(), "unsupported_media_type");

    let malformed = server.put_json(SETTINGS_PATH, b"{");
    assert_eq!(malformed.status, 400);
    assert_eq!(malformed.error_code(), "bad_request");

    let mut wrong_schema = settings_json();
    wrong_schema["schema"] = json!(1);
    let wrong_schema = put_settings(&server, &wrong_schema);
    assert_eq!(wrong_schema.status, 422);
    assert_eq!(wrong_schema.error_code(), "invalid_settings");

    let mut no_workers = settings_json();
    no_workers["workers"] = json!([]);
    let no_workers = put_settings(&server, &no_workers);
    assert_eq!(no_workers.status, 422);
    assert_eq!(no_workers.error_code(), "invalid_settings");

    let mut unknown_field = settings_json();
    unknown_field["unexpected"] = json!(true);
    let unknown_field = put_settings(&server, &unknown_field);
    assert_eq!(unknown_field.status, 422, "valid JSON with an unknown field fails schema validation");
    assert_eq!(unknown_field.error_code(), "invalid_settings");

    let mut wrong_type = settings_json();
    wrong_type["model"]["api_key"] = json!(42);
    let wrong_type = put_settings(&server, &wrong_type);
    assert_eq!(wrong_type.status, 422);
    assert_eq!(wrong_type.error_code(), "invalid_settings");

    let mut missing_field = settings_json();
    missing_field["model"].as_object_mut().unwrap().remove("api_key");
    let missing_field = put_settings(&server, &missing_field);
    assert_eq!(missing_field.status, 422);
    assert_eq!(missing_field.error_code(), "invalid_settings");
    for response in [&unknown_field, &wrong_type, &missing_field] {
        assert!(!String::from_utf8_lossy(&response.body).contains("sk-api-files-canary"));
    }

    assert!(
        !state.join("connection-settings.json").exists(),
        "rejected settings must not be written"
    );

    let accepted = put_settings(&server, &settings_json());
    assert_eq!(accepted.status, 200);
    assert_eq!(accepted.json(), json!({"saved": true}));
    assert_eq!(server.get(SETTINGS_PATH).status, 200);
}

#[test]
fn settings_rejects_reparse_state_directory_without_writing_through_it() {
    let root = fixture_root();
    let real = root.path().join("real-state");
    std::fs::create_dir(&real).unwrap();
    let original = settings_json();
    std::fs::write(
        real.join("connection-settings.json"),
        serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    let linked = root.path().join("linked-state");
    junction(&linked, &real);
    let server = TestServer::start(linked);

    let read = server.get(SETTINGS_PATH);
    assert_eq!(read.status, 500, "settings must not follow a reparse directory");
    assert!(!String::from_utf8_lossy(&read.body).contains("sk-api-files-canary"));

    let mut replacement = settings_json();
    replacement["model"]["api_key"] = json!("sk-replaced");
    let written = put_settings(&server, &replacement);
    assert_eq!(written.status, 500, "settings must not write through a reparse directory");
    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(real.join("connection-settings.json")).unwrap())
            .unwrap();
    assert_eq!(on_disk, original, "write-through must not modify the junction target");
}

#[test]
fn settings_rejects_reparse_file_entry_without_reading_or_replacing_it() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let entry = state.join("connection-settings.json");
    let outside_file = root.path().join("outside-settings.json");
    let original = settings_json();
    std::fs::write(&outside_file, serde_json::to_vec(&original).unwrap()).unwrap();
    // A symlink needs a privilege this account usually lacks; a junction at
    // the same path exercises the same reparse rejection without it.
    let symlinked = std::os::windows::fs::symlink_file(&outside_file, &entry).is_ok();
    let outside_dir = root.path().join("outside-dir");
    if !symlinked {
        junction(&entry, &outside_dir);
    }
    let server = TestServer::start(state);

    let read = server.get(SETTINGS_PATH);
    assert_eq!(read.status, 500, "the file entry must never be followed");
    assert!(!String::from_utf8_lossy(&read.body).contains("sk-api-files-canary"));

    let mut replacement = settings_json();
    replacement["model"]["api_key"] = json!("sk-replaced");
    let written = put_settings(&server, &replacement);
    assert_eq!(written.status, 500, "the file entry must never be replaced through a link");
    if symlinked {
        let on_disk: Value = serde_json::from_slice(&std::fs::read(&outside_file).unwrap()).unwrap();
        assert_eq!(on_disk, original, "symlink target must stay untouched");
    } else {
        assert!(
            !outside_dir.join("connection-settings.json").exists(),
            "junction target must stay untouched"
        );
    }
}

#[test]
fn artifact_returns_exact_bytes_and_fixed_response_metadata() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let project = Uuid::new_v4();
    let payload = b"\x00\x01binary-artifact-payload\xff";
    write_artifact(&state, project, "nested/data.bin", payload);
    let server = TestServer::start_with_projects(state, &[project]);

    let response = server.get(&format!("/v1/artifacts/{project}/nested/data.bin"));
    assert_eq!(response.status, 200);
    assert_eq!(response.body, payload, "artifact bytes must be returned unmodified");
    assert_eq!(
        response.header("content-type").as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(
        response.header("content-disposition").as_deref(),
        Some("attachment")
    );
    assert_eq!(
        response.header("x-content-type-options").as_deref(),
        Some("nosniff")
    );
    assert_eq!(response.header("cache-control").as_deref(), Some("no-store"));
}

#[test]
fn artifact_size_boundary_is_exclusive_of_one_extra_byte() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let project = Uuid::new_v4();
    write_artifact(&state, project, "exact.bin", &vec![0x41u8; 32 * 1024]);
    write_artifact(&state, project, "overflow.bin", &vec![0x42u8; 32 * 1024 + 1]);
    let server = TestServer::start_with_projects(state, &[project]);

    let exact = server.get(&format!("/v1/artifacts/{project}/exact.bin"));
    assert_eq!(exact.status, 200);
    assert_eq!(exact.body.len(), 32 * 1024);

    let overflow = server.get(&format!("/v1/artifacts/{project}/overflow.bin"));
    assert_eq!(overflow.status, 404, "a file above 32 KiB is unavailable");
    assert_eq!(overflow.error_code(), "not_found");
}

#[test]
fn artifact_rejects_traversal_percent_ads_device_names_and_directories_as_identical_404() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let project = Uuid::new_v4();
    write_artifact(&state, project, "nested/data.bin", b"ok");
    std::fs::write(root.path().join("secret.txt"), b"secret").unwrap();
    let server = TestServer::start_with_projects(state.clone(), &[project]);

    let baseline = server.get(&format!("/v1/artifacts/{}/missing.bin", Uuid::new_v4()));
    assert_eq!(baseline.status, 404);

    let cases = [
        format!("/v1/artifacts/{project}/../secret.txt"),
        format!("/v1/artifacts/{project}/%2e%2e/secret.txt"),
        format!("/v1/artifacts/{project}/data.bin:stream"),
        format!("/v1/artifacts/{project}/CON"),
        format!("/v1/artifacts/{project}/nul.txt"),
        format!("/v1/artifacts/{project}/nested/"),
        format!("/v1/artifacts/{project}/nested//data.bin"),
        format!("/v1/artifacts/{project}/nested"),
        format!("/v1/artifacts/{project}//data.bin"),
        format!("/v1/artifacts/{project}"),
        format!("/v1/artifacts/{project}/"),
        "/v1/artifacts/".to_owned(),
        format!("/v1/artifacts/{}/nested/data.bin", Uuid::new_v4()),
    ];
    for path in cases {
        let response = server.get(&path);
        assert_eq!(response.status, 404, "expected 404 for a rejected artifact path");
        assert_eq!(
            response.body, baseline.body,
            "every illegal or missing artifact case must be indistinguishable"
        );
        assert_eq!(response.error_code(), "not_found");
    }
}

#[test]
fn artifact_rejects_hardlinks_reparse_points_and_leaves_targets_untouched() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let project = Uuid::new_v4();
    let product = artifact_dir(&state, project);
    std::fs::create_dir_all(product.parent().unwrap()).unwrap();

    let outside = root.path().join("outside.bin");
    std::fs::write(&outside, b"outside").unwrap();
    let linked = product.join("hardlink.bin");
    std::fs::create_dir_all(&product).unwrap();
    std::fs::hard_link(&outside, &linked).unwrap();

    let target = root.path().join("outside-product");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("inner.bin"), b"inner").unwrap();
    let linked_project = Uuid::new_v4();
    let linked_product = state
        .join("projects")
        .join(linked_project.to_string())
        .join("product");
    std::fs::create_dir_all(linked_product.parent().unwrap()).unwrap();
    junction(&linked_product, &target);

    let final_target = root.path().join("outside-final");
    std::fs::create_dir(&final_target).unwrap();
    std::fs::write(final_target.join("payload.bin"), b"payload").unwrap();
    junction(&product.join("linked-entry.bin"), &final_target);

    let server = TestServer::start_with_projects(state, &[project, linked_project]);

    let hardlink = server.get(&format!("/v1/artifacts/{project}/hardlink.bin"));
    assert_eq!(hardlink.status, 404, "multiple hard links are not served");
    assert_eq!(hardlink.error_code(), "not_found");

    let final_reparse = server.get(&format!("/v1/artifacts/{project}/linked-entry.bin"));
    assert_eq!(final_reparse.status, 404, "a reparse final entry is not followed");
    assert_eq!(final_reparse.error_code(), "not_found");

    let through_junction = server.get(&format!(
        "/v1/artifacts/{}/inner.bin",
        linked_product.parent().unwrap().file_name().unwrap().to_string_lossy()
    ));
    assert_eq!(through_junction.status, 404, "a reparse product root is not followed");
    assert_eq!(through_junction.error_code(), "not_found");
    assert_eq!(std::fs::read(target.join("inner.bin")).unwrap(), b"inner");
}

#[test]
fn artifact_api_rejects_separator_alternatives_and_noncanonical_names() {
    use agentranch::api::files::read_artifact;

    let root = fixture_root();
    let state = fresh_state(root.path());
    let project = Uuid::new_v4();
    write_artifact(&state, project, "data.bin", b"ok");
    let id = project.to_string();

    assert!(read_artifact(&state, &id, "data.bin").is_ok());
    assert!(read_artifact(&state, &id, "nested\\data.bin").is_err());
    assert!(read_artifact(&state, &id.to_uppercase(), "data.bin").is_err());
    assert!(read_artifact(&state, "not-a-uuid", "data.bin").is_err());
    assert!(read_artifact(&state, &id, "a/b/c/d/e/f/g/h/i").is_err());
    assert!(read_artifact(&state, &id, &"a".repeat(121)).is_err());
    assert!(read_artifact(&state, &id, ".").is_err());
    assert!(read_artifact(&state, &id, "..").is_err());
    assert!(read_artifact(&state, &id, "name%2ebin").is_err());
    assert!(read_artifact(&state, &id, "lpt9").is_err());
}

#[test]
fn artifact_rejects_unregistered_project_even_when_bytes_exist() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let stale = Uuid::new_v4();
    write_artifact(&state, stale, "data.bin", b"leftover");
    let server = TestServer::start(state);

    let stale_response = server.get(&format!("/v1/artifacts/{stale}/data.bin"));
    let missing = server.get(&format!("/v1/artifacts/{}/missing.bin", Uuid::new_v4()));
    assert_eq!(stale_response.status, 404, "a project outside the published snapshot is not readable");
    assert_eq!(stale_response.error_code(), "not_found");
    assert_eq!(
        stale_response.body, missing.body,
        "unregistered and missing artifacts must be indistinguishable"
    );
    assert!(!String::from_utf8_lossy(&stale_response.body).contains("leftover"));
}

#[test]
fn artifact_failures_create_no_directories() {
    let root = fixture_root();
    let state = fresh_state(root.path());
    let server = TestServer::start(state.clone());
    let missing = Uuid::new_v4();

    let response = server.get(&format!("/v1/artifacts/{missing}/nested/data.bin"));
    assert_eq!(response.status, 404);
    assert_eq!(
        std::fs::read_dir(&state).unwrap().count(),
        0,
        "a missing artifact read must not create any state entries"
    );
}
