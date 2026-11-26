#![cfg(feature = "daemon")]
//! Loopback HTTP/1.1 and SSE transport contract tests.
//!
//! The server runs in-process with a fake `Backend` so acceptance, publication
//! and framing are deterministic. Requests use a minimal independent raw TCP
//! client instead of any HTTP library, so status lines, headers, exact bodies
//! and SSE bytes are asserted as seen on the wire.

use agentranch::{
    api::{
        backend::Backend,
        bridge::{BridgeWake, ServiceBridge},
        http::{serve_connection, EventHubAccess, HttpApp},
    },
    config::Mode,
    core::service::{AppSnapshot, CommandResult, LiveOutputEvent, Readiness, SubmitError, UiCommand},
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use uuid::Uuid;

fn snapshot() -> AppSnapshot {
    AppSnapshot {
        mode: Mode::Real,
        role: "http-transport-test".into(),
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

struct FakeInner {
    next_id: Mutex<u64>,
    commands: Mutex<Vec<UiCommand>>,
    results: Mutex<Vec<CommandResult>>,
    snapshot: Mutex<Arc<AppSnapshot>>,
    submit_calls: AtomicUsize,
    disconnect_calls: AtomicUsize,
}

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
                results: Mutex::new(Vec::new()),
                snapshot: Mutex::new(Arc::new(snapshot())),
                submit_calls: AtomicUsize::new(0),
                disconnect_calls: AtomicUsize::new(0),
            }),
        }
    }

    fn submit_calls(&self) -> usize {
        self.inner.submit_calls.load(SeqCst)
    }

    fn disconnect_calls(&self) -> usize {
        self.inner.disconnect_calls.load(SeqCst)
    }

    fn command_kinds(&self) -> Vec<&'static str> {
        self.inner
            .commands
            .lock()
            .unwrap()
            .iter()
            .map(command_kind)
            .collect()
    }
}

fn command_kind(command: &UiCommand) -> &'static str {
    match command {
        UiCommand::StartLocal => "start_local",
        UiCommand::Dispatch { .. } => "dispatch",
        UiCommand::ProjectCreate { .. } => "project_create",
        UiCommand::ProjectConfirm { .. } => "project_confirm",
        UiCommand::ProjectRefresh { .. } => "project_refresh",
        UiCommand::ProjectImport { .. } => "project_import",
        _ => "other",
    }
}

impl Backend for FakeBackend {
    fn try_submit(&self, command: UiCommand) -> Result<u64, SubmitError> {
        self.inner.submit_calls.fetch_add(1, SeqCst);
        let request_id = {
            let mut next = self.inner.next_id.lock().unwrap();
            let id = *next;
            *next += 1;
            id
        };
        self.inner.commands.lock().unwrap().push(command);
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
        self.inner.snapshot.lock().unwrap().clone()
    }

    fn disconnect(&self) {
        self.inner.disconnect_calls.fetch_add(1, SeqCst);
    }
}

struct TestServer {
    addr: SocketAddr,
    instance: Uuid,
    fake: FakeBackend,
    stop: Arc<AtomicBool>,
    owner: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start() -> Self {
        Self::start_with(Mode::Real, Duration::from_secs(15))
    }

    fn start_local() -> Self {
        Self::start_with(Mode::Local, Duration::from_secs(15))
    }

    fn start_with(mode: Mode, heartbeat: Duration) -> Self {
        let fake = FakeBackend::new();
        let stop = Arc::new(AtomicBool::new(false));

        let wake = BridgeWake::new();
        let mut bridge = ServiceBridge::with_wake(Box::new(fake.clone()), wake);
        let client = Arc::new(bridge.client());
        let events = Arc::new(EventHubAccess::new(bridge.hub()));
        // The hub is the single transport identity; read it the same way a
        // client would from health so every assertion cross-checks the wire.
        let instance = events.instance_id();
        let app =
            Arc::new(HttpApp::new(mode, client, events).with_heartbeat_interval(heartbeat));

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
            instance,
            fake,
            stop,
            owner: Some(owner),
            server: Some(server),
        }
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

    fn set_read_timeout(&self, timeout: Duration) {
        self.stream.set_read_timeout(Some(timeout)).unwrap();
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

    fn read_chunk(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(position) = find_subslice(&self.buffer, b"\r\n") {
                let line = String::from_utf8(self.buffer[..position].to_vec()).unwrap();
                self.buffer.drain(..position + 2);
                let size = usize::from_str_radix(line.trim(), 16).unwrap();
                if size == 0 {
                    return None;
                }
                let chunk = self.read_exact_bytes(size);
                assert_eq!(self.read_exact_bytes(2), b"\r\n");
                return Some(chunk);
            }
            match self.read_more() {
                ReadOutcome::Data => {}
                ReadOutcome::Eof | ReadOutcome::Timeout => return None,
            }
        }
    }
}

struct SseConn {
    conn: Conn,
    chunked: bool,
    decoded: VecDeque<u8>,
}

impl SseConn {
    fn open(
        addr: SocketAddr,
        path: &str,
        headers: &[(&str, &str)],
        idle: Duration,
    ) -> (u16, Vec<(String, String)>, Self) {
        let mut conn = Conn::connect(addr);
        conn.send_head("GET", path, headers);
        let (status, response_headers) = conn.read_head();
        let chunked = response_headers
            .iter()
            .any(|(name, value)| name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
        conn.set_read_timeout(idle);
        (
            status,
            response_headers,
            Self {
                conn,
                chunked,
                decoded: VecDeque::new(),
            },
        )
    }

    fn next_frame(&mut self) -> Option<String> {
        loop {
            let position = {
                let contiguous = self.decoded.make_contiguous();
                find_subslice(contiguous, b"\n\n")
            };
            if let Some(position) = position {
                let frame: Vec<u8> = self.decoded.drain(..position + 2).collect();
                return Some(String::from_utf8(frame).unwrap());
            }
            let chunk = if self.chunked {
                self.conn.read_chunk()
            } else {
                match self.conn.read_more() {
                    ReadOutcome::Data => Some(std::mem::take(&mut self.conn.buffer)),
                    ReadOutcome::Eof | ReadOutcome::Timeout => None,
                }
            };
            match chunk {
                Some(bytes) if !bytes.is_empty() => self.decoded.extend(bytes),
                Some(_) => {}
                None => return None,
            }
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn get(addr: SocketAddr, path: &str) -> HttpResponse {
    get_with_headers(addr, path, &[])
}

fn get_with_headers(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
    let mut conn = Conn::connect(addr);
    let mut all = vec![("Connection", "close")];
    all.extend_from_slice(headers);
    conn.send_head("GET", path, &all);
    conn.read_response()
}

fn post_json(addr: SocketAddr, path: &str, content_type: &str, body: &[u8]) -> HttpResponse {
    let mut conn = Conn::connect(addr);
    conn.send_json("POST", path, Some(content_type), body);
    conn.read_response()
}

fn command(body: Value) -> Vec<u8> {
    serde_json::to_vec(&body).unwrap()
}

fn assert_error_envelope(response: &HttpResponse, status: u16, code: &str) {
    assert_eq!(
        response.status,
        status,
        "body: {}",
        String::from_utf8_lossy(&response.body)
    );
    assert_eq!(response.header("cache-control").as_deref(), Some("no-store"));
    let body = response.json();
    let error = body.get("error").expect("fixed error envelope");
    assert_eq!(body.as_object().unwrap().len(), 1);
    assert_eq!(error["code"], code);
    assert!(error["message"].as_str().is_some_and(|value| !value.is_empty()));
    assert!(error["next_step"].as_str().is_some_and(|value| !value.is_empty()));
}

#[test]
fn health_reports_contract_mode_pid_and_instance_identity() {
    let server = TestServer::start();
    let response = get(server.addr, "/v1/health");
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type").as_deref(), Some("application/json"));
    assert_eq!(response.header("cache-control").as_deref(), Some("no-store"));
    let body = response.json();
    assert_eq!(body.as_object().unwrap().len(), 7);
    assert_eq!(body["contract"], 1);
    assert_eq!(body["mode"], "real");
    assert_eq!(body["profile"], "isolated");
    assert_eq!(body["pid"], std::process::id());
    assert!(body["started_at"].as_u64().unwrap() > 0);
    assert!(!body["app_version"].as_str().unwrap().is_empty());
    assert_eq!(body["instance_id"], server.instance.to_string());
}

#[test]
fn health_mode_is_local_only_when_explicitly_started_local() {
    let server = TestServer::start_local();
    let body = get(server.addr, "/v1/health").json();
    assert_eq!(body["mode"], "local");
    assert_eq!(body["profile"], "isolated");
}

#[test]
fn snapshot_headers_share_the_published_view_identity() {
    let server = TestServer::start();
    let response = get(server.addr, "/v1/snapshot");
    assert_eq!(response.status, 200);
    assert_eq!(response.header("cache-control").as_deref(), Some("no-store"));
    let expected = server.instance.to_string();
    assert_eq!(response.header("x-agentranch-instance").as_deref(), Some(expected.as_str()));
    assert_eq!(response.header("x-agentranch-seq").as_deref(), Some("0"));
    let body = response.json();
    assert_eq!(body["mode"], "real");
    assert_eq!(body["role"], "http-transport-test");
    assert_eq!(body["readiness"], "not_ready");
}

#[test]
fn unknown_routes_and_wrong_methods_use_fixed_error_bodies() {
    let server = TestServer::start();
    let not_found = get(server.addr, "/v1/unknown-canary-path");
    assert_error_envelope(&not_found, 404, "not_found");
    assert!(!String::from_utf8_lossy(&not_found.body).contains("unknown-canary-path"));

    let wrong_method = get(server.addr, "/v1/commands");
    assert_error_envelope(&wrong_method, 405, "method_not_allowed");

    let health_post = post_json(
        server.addr,
        "/v1/health",
        "application/json",
        br#"{"kind":"start_local","args":{}}"#,
    );
    assert_error_envelope(&health_post, 405, "method_not_allowed");
}

#[test]
fn command_post_requires_json_content_type() {
    let server = TestServer::start();
    let body = br#"{"kind":"start_local","args":{}}"#;
    let mut missing = Conn::connect(server.addr);
    let length = body.len().to_string();
    missing.send_head(
        "POST",
        "/v1/commands",
        &[("Connection", "close"), ("Content-Length", length.as_str())],
    );
    missing.write_all(body);
    assert_error_envelope(&missing.read_response(), 415, "unsupported_media_type");

    let plain = post_json(server.addr, "/v1/commands", "text/plain", body);
    assert_error_envelope(&plain, 415, "unsupported_media_type");

    let accepted = post_json(
        server.addr,
        "/v1/commands",
        "application/json; charset=utf-8",
        body,
    );
    assert_eq!(
        accepted.status,
        202,
        "body: {}",
        String::from_utf8_lossy(&accepted.body)
    );
    assert_eq!(server.fake.submit_calls(), 1);
}

#[test]
fn command_posts_reject_oversized_and_malformed_bodies_without_echo() {
    let server = TestServer::start();
    let mut oversized = Conn::connect(server.addr);
    oversized.send_head(
        "POST",
        "/v1/commands",
        &[
            ("Connection", "close"),
            ("Content-Type", "application/json"),
            ("Content-Length", "1048577"),
        ],
    );
    assert_error_envelope(&oversized.read_response(), 413, "payload_too_large");

    const CANARY: &str = "synthetic-http-canary";
    let malformed = command(json!({"kind": "start_local", "args": {"canary": CANARY}}));
    let response = post_json(server.addr, "/v1/commands", "application/json", &malformed);
    assert_error_envelope(&response, 400, "bad_request");
    assert!(!String::from_utf8_lossy(&response.body).contains(CANARY));
    assert_eq!(server.fake.submit_calls(), 0);
}

#[test]
fn disabled_commands_are_rejected_before_enqueue() {
    let server = TestServer::start();
    let provision = command(json!({
        "kind": "provision_worker",
        "args": {
            "host": "synthetic-host",
            "root_user": "synthetic-root",
            "root_password": "synthetic-password",
            "model_base_url": "synthetic-model-url",
            "model_token": "synthetic-token",
            "model": "synthetic-model"
        }
    }));
    let response = post_json(server.addr, "/v1/commands", "application/json", &provision);
    assert_error_envelope(&response, 403, "command_disabled");
    let body = String::from_utf8_lossy(&response.body);
    for secret in ["synthetic-host", "synthetic-password", "synthetic-token"] {
        assert!(!body.contains(secret), "error body echoed a command field");
    }

    let import = command(json!({
        "kind": "project_import",
        "args": {"project_id": "synthetic-project", "path": "C:\\synthetic-secret\\design.md"}
    }));
    let response = post_json(server.addr, "/v1/commands", "application/json", &import);
    assert_error_envelope(&response, 503, "unavailable");
    assert!(!String::from_utf8_lossy(&response.body).contains("synthetic-secret"));
    assert_eq!(server.fake.submit_calls(), 0, "rejected commands must not reach the bridge");
}

#[test]
fn local_only_commands_follow_the_explicit_mode() {
    let real = TestServer::start();
    let dispatch = command(json!({"kind": "dispatch", "args": {"template": "synthetic"}}));
    let response = post_json(real.addr, "/v1/commands", "application/json", &dispatch);
    assert_error_envelope(&response, 403, "command_disabled");
    assert_eq!(real.fake.submit_calls(), 0);

    let local = TestServer::start_local();
    let response = post_json(local.addr, "/v1/commands", "application/json", &dispatch);
    assert_eq!(response.status, 202);
    assert_eq!(local.fake.command_kinds(), vec!["dispatch"]);
}

#[test]
fn accepted_command_returns_only_202_identity_and_request_id() {
    let server = TestServer::start();
    let body = command(json!({"kind": "start_local", "args": {}}));
    let first = post_json(server.addr, "/v1/commands", "application/json", &body);
    assert_eq!(
        first.status,
        202,
        "body: {}",
        String::from_utf8_lossy(&first.body)
    );
    assert_eq!(first.header("cache-control").as_deref(), Some("no-store"));
    let accepted = first.json();
    assert_eq!(accepted.as_object().unwrap().len(), 3);
    assert_eq!(accepted["accepted"], true);
    assert_eq!(accepted["instance_id"], server.instance.to_string());
    assert_eq!(accepted["request_id"], 1);

    let second = post_json(server.addr, "/v1/commands", "application/json", &body);
    assert_eq!(second.status, 202);
    assert_eq!(second.json()["request_id"], 2);
    assert_eq!(server.fake.command_kinds(), vec!["start_local", "start_local"]);
}

#[test]
fn sse_checkpoint_then_command_result_and_cursor_replay() {
    let server = TestServer::start();
    let (status, headers, mut sse) = SseConn::open(
        server.addr,
        "/v1/events",
        &[("Connection", "keep-alive")],
        Duration::from_secs(5),
    );
    assert_eq!(status, 200);
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name == "content-type")
            .map(|(_, value)| value.as_str()),
        Some("text/event-stream")
    );
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name == "cache-control")
            .map(|(_, value)| value.as_str()),
        Some("no-store")
    );

    let checkpoint = sse.next_frame().expect("checkpoint frame");
    assert!(checkpoint.starts_with(&format!("id: {}:0\n", server.instance)));
    assert!(checkpoint.contains("event: snapshot\n"));
    let checkpoint_data = checkpoint.lines().find_map(|line| line.strip_prefix("data: ")).unwrap();
    let checkpoint_json: Value = serde_json::from_str(checkpoint_data).unwrap();
    assert_eq!(checkpoint_json["mode"], "real");

    let body = command(json!({"kind": "start_local", "args": {}}));
    assert_eq!(
        post_json(server.addr, "/v1/commands", "application/json", &body).status,
        202
    );
    let result = sse.next_frame().expect("command_result frame");
    assert!(result.starts_with(&format!("id: {}:1\n", server.instance)));
    assert!(result.contains("event: command_result\n"));
    let result_data: Value = serde_json::from_str(
        result.lines().find_map(|line| line.strip_prefix("data: ")).unwrap(),
    )
    .unwrap();
    assert_eq!(result_data["request_id"], 1);
    assert_eq!(result_data["result"]["accepted"], true);
    drop(sse);

    let mut idle = SseConn::open(
        server.addr,
        &format!("/v1/events?instance_id={}&after=1", server.instance),
        &[("Connection", "keep-alive")],
        Duration::from_millis(300),
    )
    .2;
    assert!(idle.next_frame().is_none(), "after=head must wait for new events");
    drop(idle);

    let mut resumed = SseConn::open(
        server.addr,
        &format!("/v1/events?instance_id={}&after=1", server.instance),
        &[("Connection", "keep-alive")],
        Duration::from_secs(5),
    )
    .2;
    assert_eq!(
        post_json(server.addr, "/v1/commands", "application/json", &body).status,
        202
    );
    let next = resumed.next_frame().expect("second command_result");
    assert!(next.starts_with(&format!("id: {}:2\n", server.instance)));
    assert!(next.contains("event: command_result\n"));
    drop(resumed);

    let last_event_id = format!("{}:0", server.instance);
    let mut last_event = SseConn::open(
        server.addr,
        "/v1/events",
        &[
            ("Connection", "keep-alive"),
            ("Last-Event-ID", last_event_id.as_str()),
        ],
        Duration::from_secs(5),
    )
    .2;
    let first_replay = last_event.next_frame().expect("replayed first result");
    assert!(first_replay.starts_with(&format!("id: {}:1\n", server.instance)));
    let second_replay = last_event.next_frame().expect("replayed second result");
    assert!(second_replay.starts_with(&format!("id: {}:2\n", server.instance)));
}

#[test]
fn sse_gap_and_invalid_cursors_are_deterministic() {
    let server = TestServer::start();
    let body = command(json!({"kind": "start_local", "args": {}}));
    assert_eq!(
        post_json(server.addr, "/v1/commands", "application/json", &body).status,
        202
    );

    let other = Uuid::new_v4();
    let (status, _, mut gap) = SseConn::open(
        server.addr,
        &format!("/v1/events?instance_id={other}&after=0"),
        &[("Connection", "keep-alive")],
        Duration::from_secs(5),
    );
    assert_eq!(status, 200);
    let gap_frame = gap.next_frame().expect("gap frame");
    assert!(!gap_frame.starts_with("id: "), "gap must not carry an event id");
    assert!(gap_frame.contains("event: gap\n"));
    let gap_data: Value = serde_json::from_str(
        gap_frame.lines().find_map(|line| line.strip_prefix("data: ")).unwrap(),
    )
    .unwrap();
    assert_eq!(gap_data["reason"], "instance_changed");
    let checkpoint = gap.next_frame().expect("checkpoint after gap");
    assert!(checkpoint.starts_with(&format!("id: {}:1\n", server.instance)));
    assert!(checkpoint.contains("event: snapshot\n"));
    drop(gap);

    for path in [
        format!("/v1/events?instance_id={}&after=999999", server.instance),
        format!("/v1/events?instance_id={}&after=not-a-number", server.instance),
        format!("/v1/events?instance_id={}&instance_id={other}&after=0", server.instance),
        "/v1/events?after=1".to_owned(),
    ] {
        let response = get(server.addr, &path);
        assert_error_envelope(&response, 400, "bad_request");
    }

    let last_event_id = format!("{}:0", server.instance);
    let combined = get_with_headers(
        server.addr,
        "/v1/events?after=0",
        &[("Last-Event-ID", last_event_id.as_str())],
    );
    assert_error_envelope(&combined, 400, "bad_request");
}

#[test]
fn sse_heartbeat_carries_no_id_and_client_disconnect_keeps_work() {
    let server = TestServer::start_with(Mode::Real, Duration::from_millis(100));
    let (_, _, mut sse) = SseConn::open(
        server.addr,
        "/v1/events",
        &[("Connection", "keep-alive")],
        Duration::from_secs(5),
    );
    let _checkpoint = sse.next_frame().expect("checkpoint frame");
    let heartbeat = sse.next_frame().expect("heartbeat frame");
    assert_eq!(heartbeat, ": keep-alive\n\n");
    assert!(!heartbeat.contains("id:"), "heartbeat must not advance the cursor");
    assert!(!heartbeat.contains("event:"), "heartbeat must be a comment, not a named event");
    drop(sse);

    let body = command(json!({"kind": "start_local", "args": {}}));
    assert_eq!(
        post_json(server.addr, "/v1/commands", "application/json", &body).status,
        202
    );

    let mut replay = SseConn::open(
        server.addr,
        &format!("/v1/events?instance_id={}&after=0", server.instance),
        &[("Connection", "keep-alive")],
        Duration::from_secs(5),
    )
    .2;
    let frame = replay.next_frame().expect("accepted work survives disconnect");
    assert!(frame.starts_with(&format!("id: {}:1\n", server.instance)));
    assert!(frame.contains("event: command_result\n"));
    assert_eq!(server.fake.disconnect_calls(), 0);
}
