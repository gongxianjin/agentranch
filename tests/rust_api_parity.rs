#[path = "support/daemon_temp.rs"]
mod daemon_temp;

use agentranch::api::{backend::Backend, contract::decode_command};
use agentranch::config::Mode;
use agentranch::core::service::{CommandResult, ServiceHandle, ServiceOptions, SubmitError};
use serde_json::Value;
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

/// Fixed snapshot-only gate of Real mode: the command receipt carries the
/// project contract, and the published snapshot additionally blocks dispatch
/// until a signed readiness probe passes (service.rs project projection).
const READINESS_BLOCKER: &str =
    "真实节点未通过当前签名 readiness probe/ready；配置有效不等于可派工";

const PROJECT_KEYS: [&str; 15] = [
    "id",
    "name",
    "phase",
    "project_path",
    "product_path",
    "document",
    "tasks",
    "actions",
    "blockers",
    "can_confirm",
    "can_split",
    "can_start",
    "can_test",
    "can_repair",
    "can_accept",
];

fn options(state_dir: PathBuf) -> ServiceOptions {
    ServiceOptions { mode: Mode::Real, state_dir, worker_exe: PathBuf::from(env!("CARGO_BIN_EXE_aw-cli")), gateway_key: None, gateway_model_token: None }
}

fn wait_result(backend: &mut dyn Backend, request_id: u64) -> agentranch::core::service::CommandResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(result) = backend.drain_updates().1.into_iter().find(|result| result.request_id == request_id) {
            return result;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("request {request_id} did not produce a result")
}

/// Collect exactly one receipt per accepted request; silence is a failure.
fn wait_results(backend: &mut dyn Backend, request_ids: &[u64]) -> BTreeMap<u64, CommandResult> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found = BTreeMap::new();
    while found.len() < request_ids.len() && Instant::now() < deadline {
        for result in backend.drain_updates().1 {
            if request_ids.contains(&result.request_id) {
                found.insert(result.request_id, result);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        found.len(),
        request_ids.len(),
        "accepted requests without a real receipt: {request_ids:?} vs {found:?}"
    );
    found
}

fn submit(backend: &mut dyn Backend, wire: &[u8]) -> u64 {
    backend.try_submit(decode_command(wire).unwrap()).unwrap()
}

fn submit_and_wait(backend: &mut dyn Backend, wire: &[u8]) -> CommandResult {
    let request_id = submit(backend, wire);
    wait_result(backend, request_id)
}

fn import_command(project_id: &str, path: &std::path::Path) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "kind": "project_import",
        "args": { "project_id": project_id, "path": path }
    }))
    .unwrap()
}

fn confirm_command(project_id: &str, version: u32, sha256: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "kind": "project_confirm",
        "args": { "project_id": project_id, "version": version, "sha256": sha256 }
    }))
    .unwrap()
}

/// Derive the published snapshot project from the command receipt. Real mode
/// is NotReady by construction, so the snapshot adds the fixed readiness
/// blocker and suppresses the four dispatch flags; every other field (including
/// the managed paths and document) must be identical.
fn projection_of_receipt(receipt: &Value) -> Value {
    let mut projected = receipt.clone();
    let blockers = projected["blockers"].as_array_mut().expect("project blockers");
    if !blockers.iter().any(|blocker| blocker.as_str() == Some(READINESS_BLOCKER)) {
        blockers.push(Value::String(READINESS_BLOCKER.into()));
    }
    for flag in ["can_split", "can_start", "can_test", "can_repair"] {
        projected[flag] = Value::Bool(false);
    }
    projected
}

fn wait_project(backend: &mut dyn Backend, project_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = serde_json::to_value(backend.snapshot().as_ref()).unwrap();
        if let Some(project) = snapshot["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|project| project["id"] == project_id)
        {
            return project.clone();
        }
        assert!(Instant::now() < deadline, "project {project_id} not projected");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn inproc_backend_preserves_acceptance_results_and_snapshot() {
    let temp = daemon_temp::temp_root();
    let service = ServiceHandle::spawn(options(temp.path().join("real")), Arc::new(|| {})).unwrap();
    let mut backend: Box<dyn Backend> = Box::new(service);

    let create = decode_command(br#"{"kind":"project_create","args":{"name":"parity"}}"#).unwrap();
    let create_id = backend.try_submit(create).unwrap();
    let created = wait_result(backend.as_mut(), create_id).result.unwrap();
    let project_id = created["id"].as_str().unwrap().to_owned();

    let document = temp.path().join("design.md");
    std::fs::write(&document, "# parity\n\n- preserve API boundary\n").unwrap();
    let import_wire = serde_json::json!({"kind":"project_import","args":{"project_id":project_id,"path":document}});
    let import = decode_command(&serde_json::to_vec(&import_wire).unwrap()).unwrap();
    let import_id = backend.try_submit(import).unwrap();
    let imported = wait_result(backend.as_mut(), import_id).result.unwrap();
    let document_sha256 = hex::encode(sha2::Sha256::digest(b"# parity\n\n- preserve API boundary\n"));
    assert_eq!(imported["id"], project_id);
    assert_eq!(imported["document"]["version"], 1);
    assert_eq!(imported["document"]["sha256"], document_sha256);

    let confirm_wire = serde_json::json!({"kind":"project_confirm","args":{"project_id":project_id,"version":1,"sha256":document_sha256}});
    let confirm = decode_command(&serde_json::to_vec(&confirm_wire).unwrap()).unwrap();
    let confirm_id = backend.try_submit(confirm).unwrap();
    assert!(wait_result(backend.as_mut(), confirm_id).result.is_ok());

    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        let snapshot = backend.snapshot();
        if snapshot.projects.iter().any(|project| project.id == project_id && project.document.as_ref().is_some_and(|document| document.confirmed)) { break snapshot; }
        if Instant::now() >= deadline { panic!("confirmed project not projected: {snapshot:?}"); }
        thread::sleep(Duration::from_millis(10));
    };
    let project = snapshot.projects.iter().find(|project| project.id == project_id).unwrap();
    assert!(project.document.as_ref().unwrap().confirmed);
    assert_eq!(snapshot.mode, Mode::Real);
    assert_eq!(snapshot.readiness, agentranch::core::service::Readiness::NotReady);

    backend.disconnect();
    backend.disconnect();
    assert_eq!(backend.try_submit(decode_command(br#"{"kind":"start_local","args":{}}"#).unwrap()), Err(SubmitError::Stopped));
}

/// The receipt returned over HTTP is `CommandResult.result` verbatim, so the
/// in-process projection must equal the documented HTTP payload: same field
/// set, same independently computed document hash, and only the Real-mode
/// readiness gate added by the snapshot projection.
#[test]
fn inproc_receipt_and_snapshot_projection_agree_on_normalized_project_json() {
    let temp = daemon_temp::temp_root();
    let service = ServiceHandle::spawn(options(temp.path().join("real")), Arc::new(|| {})).unwrap();
    let mut backend: Box<dyn Backend> = Box::new(service);

    let created = submit_and_wait(
        backend.as_mut(),
        br#"{"kind":"project_create","args":{"name":"projection parity"}}"#,
    )
    .result
    .unwrap();
    let project_id = created["id"].as_str().expect("project id").to_owned();

    let body = b"# projection parity\n\n- same fields over HTTP and in-process\n";
    let document = temp.path().join("projection.md");
    std::fs::write(&document, body).unwrap();
    let imported = submit_and_wait(backend.as_mut(), &import_command(&project_id, &document))
        .result
        .unwrap();
    assert_eq!(imported["document"]["version"], 1);
    assert_eq!(imported["document"]["filename"], "projection.md");
    assert_eq!(imported["document"]["sha256"], hex::encode(sha2::Sha256::digest(body)));
    assert_eq!(imported["document"]["text"], std::str::from_utf8(body).unwrap());

    let sha256 = imported["document"]["sha256"].as_str().unwrap().to_owned();
    let confirmed = submit_and_wait(backend.as_mut(), &confirm_command(&project_id, 1, &sha256))
        .result
        .unwrap();
    assert_eq!(confirmed["phase"], "confirmed");
    assert_eq!(confirmed["document"]["confirmed"], true);
    assert_eq!(confirmed["can_split"], true, "the receipt carries the contract, not the gate");

    let projected = wait_project(backend.as_mut(), &project_id);
    assert_eq!(
        projected.as_object().unwrap().keys().map(String::as_str).collect::<BTreeSet<_>>(),
        PROJECT_KEYS.into_iter().collect::<BTreeSet<_>>()
    );
    assert_eq!(projected, projection_of_receipt(&confirmed));
    assert_eq!(projected["blockers"].as_array().unwrap().len(), 1);
    assert_eq!(projected["blockers"][0], READINESS_BLOCKER);
    assert_eq!(projected["document"]["sha256"], sha256);
}

/// Duplicate submits are independent accepted requests, and disconnect never
/// silences one: each accepted id yields exactly one receipt, a success always
/// has its durable project, and a queued request may only meet the core's real
/// `stopped` answer (known non-execution, never a silent cancellation).
#[test]
fn duplicate_submits_are_distinct_and_disconnect_never_silences_accepted_work() {
    let temp = daemon_temp::temp_root();
    let service = ServiceHandle::spawn(options(temp.path().join("real")), Arc::new(|| {})).unwrap();
    let mut backend: Box<dyn Backend> = Box::new(service);

    let wire = br#"{"kind":"project_create","args":{"name":"same payload"}}"#;
    let first = submit(backend.as_mut(), wire);
    let second = submit(backend.as_mut(), wire);
    assert_ne!(first, second, "duplicate submits must be distinct requests");

    backend.disconnect();
    backend.disconnect();

    let results = wait_results(backend.as_mut(), &[first, second]);
    let snapshot = serde_json::to_value(backend.snapshot().as_ref()).unwrap();
    let projects = snapshot["projects"].as_array().unwrap();
    let mut completed = Vec::new();
    for request_id in [first, second] {
        match &results[&request_id].result {
            Ok(value) => {
                let project_id = value["id"].as_str().expect("created project id").to_owned();
                assert_eq!(value["name"], "same payload");
                assert!(
                    projects.iter().any(|project| project["id"] == project_id.as_str()),
                    "successful receipt without a durable project: {project_id}"
                );
                completed.push(project_id);
            }
            Err(error) => assert_eq!(
                error.code, "stopped",
                "only the core's real stop may replace execution of a queued request"
            ),
        }
    }
    assert_eq!(completed.len(), projects.len(), "published projects disagree with success receipts");
    assert_eq!(
        completed.iter().collect::<BTreeSet<_>>().len(),
        completed.len(),
        "duplicate submits produced the same project"
    );
    assert_eq!(
        backend.try_submit(decode_command(br#"{"kind":"start_local","args":{}}"#).unwrap()),
        Err(SubmitError::Stopped)
    );
}

/// Cross-transport parity for Brief Step 4: the same `project_create` wire sent
/// over real HTTP and through an in-process `Backend` must produce the same
/// normalized project JSON. Only identity and managed paths may differ because
/// the two cores own separate isolated state roots.
#[cfg(feature = "daemon")]
mod daemon_parity {
    use super::*;
    use agentranch::api::{
        bridge::{BridgeWake, ServiceBridge},
        http::{serve_connection, EventHubAccess, HttpApp},
    };
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpStream},
        sync::atomic::{AtomicBool, Ordering::SeqCst},
        thread::{self, JoinHandle},
        time::Duration,
    };

    struct Surface {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        owner: Option<JoinHandle<()>>,
        server: Option<JoinHandle<()>>,
    }

    impl Surface {
        fn start(state_dir: PathBuf) -> Self {
            let service = ServiceHandle::spawn(options(state_dir), Arc::new(|| {})).unwrap();
            let mut bridge = ServiceBridge::with_wake(Box::new(service), BridgeWake::new());
            let client = Arc::new(bridge.client());
            let events = Arc::new(EventHubAccess::new(bridge.hub()));
            let app = Arc::new(HttpApp::new(Mode::Real, client, events));
            let stop = Arc::new(AtomicBool::new(false));

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
                addr,
                stop,
                owner: Some(owner),
                server: Some(server),
            }
        }
    }

    impl Drop for Surface {
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
    }

    /// Minimal independent wire client: headers, fixed-length bodies and
    /// chunked SSE frames are parsed from the actual bytes on the socket.
    struct Raw {
        stream: TcpStream,
        buffer: Vec<u8>,
        decoded: Vec<u8>,
        chunked: bool,
    }

    impl Raw {
        fn connect(addr: SocketAddr) -> Self {
            let stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
            Self {
                stream,
                buffer: Vec::new(),
                decoded: Vec::new(),
                chunked: false,
            }
        }

        fn send(&mut self, method: &str, path: &str, content_type: Option<&str>, body: &[u8]) {
            let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
            if method == "GET" {
                head.push_str("Accept: text/event-stream\r\n");
            }
            if let Some(content_type) = content_type {
                head.push_str(&format!("Content-Type: {content_type}\r\n"));
            }
            head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
            self.stream.write_all(head.as_bytes()).unwrap();
            self.stream.write_all(body).unwrap();
        }

        fn read_more(&mut self) -> bool {
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => false,
                Ok(count) => {
                    self.buffer.extend_from_slice(&chunk[..count]);
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    panic!("HTTP/SSE read timed out")
                }
                Err(_) => false,
            }
        }

        fn read_head(&mut self) -> (u16, Vec<(String, String)>) {
            loop {
                if let Some(position) = self
                    .buffer
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                {
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
                assert!(self.read_more(), "connection closed before response headers");
            }
        }

        fn read_exact_bytes(&mut self, count: usize) -> Vec<u8> {
            while self.buffer.len() < count {
                assert!(self.read_more(), "connection closed inside chunk");
            }
            let rest = self.buffer.split_off(count);
            std::mem::replace(&mut self.buffer, rest)
        }

        fn read_chunk(&mut self) -> Option<Vec<u8>> {
            loop {
                if let Some(position) = self
                    .buffer
                    .windows(2)
                    .position(|window| window == b"\r\n")
                {
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
                    true => {}
                    false => return None,
                }
            }
        }

        fn read_response(&mut self) -> HttpResponse {
            let (status, headers) = self.read_head();
            let chunked = headers.iter().any(|(name, value)| {
                name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked")
            });
            let body = if chunked {
                let mut body = Vec::new();
                while let Some(chunk) = self.read_chunk() {
                    body.extend(chunk);
                }
                body
            } else {
                let length = headers
                    .iter()
                    .find(|(name, _)| name == "content-length")
                    .map(|(_, value)| value.parse::<usize>().unwrap())
                    .unwrap_or(0);
                self.read_exact_bytes(length)
            };
            HttpResponse { status, body }
        }

        fn next_event(&mut self) -> Option<(String, Value)> {
            loop {
                if let Some(position) = self
                    .decoded
                    .windows(2)
                    .position(|window| window == b"\n\n")
                {
                    let frame: Vec<u8> = self.decoded.drain(..position + 2).collect();
                    let text = String::from_utf8(frame).unwrap();
                    let mut event = String::new();
                    let mut data = String::new();
                    for line in text.lines() {
                        if let Some(value) = line.strip_prefix("event: ") {
                            event = value.to_owned();
                        } else if let Some(value) = line.strip_prefix("data: ") {
                            data.push_str(value);
                        }
                    }
                    if !data.is_empty() {
                        return Some((event, serde_json::from_str(&data).unwrap()));
                    }
                    continue;
                }
                let chunk = if self.chunked {
                    self.read_chunk()
                } else if self.read_more() {
                    Some(std::mem::take(&mut self.buffer))
                } else {
                    None
                };
                match chunk {
                    Some(bytes) if !bytes.is_empty() => self.decoded.extend(bytes),
                    Some(_) => {}
                    None => return None,
                }
            }
        }
    }

    /// Identity and managed paths are per-state-root by construction; every
    /// other field must survive both transports byte-for-byte in JSON.
    fn normalized_project(mut project: Value) -> Value {
        let object = project.as_object_mut().expect("project object");
        for volatile in ["id", "project_path", "product_path"] {
            object.remove(volatile);
        }
        project
    }

    #[test]
    fn http_and_in_process_receipts_agree_on_normalized_project_json() {
        let temp = daemon_temp::temp_root();
        let surface = Surface::start(temp.path().join("http/state"));

        let mut sse = Raw::connect(surface.addr);
        sse.send("GET", "/v1/events", None, b"");
        let (status, headers) = sse.read_head();
        assert_eq!(status, 200, "SSE must be subscribed before the submit");
        sse.chunked = headers.iter().any(|(name, value)| {
            name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked")
        });
        // A no-cursor subscription is answered by a checkpoint frame: a
        // snapshot frame carrying the current head id, never `after=0` replay.
        let (event, checkpoint) = sse.next_event().expect("checkpoint frame");
        assert_eq!(event, "snapshot");
        assert!(
            checkpoint["projects"].is_array(),
            "checkpoint carries the published snapshot: {checkpoint}"
        );

        let wire = br#"{"kind":"project_create","args":{"name":"wire parity"}}"#;
        let mut request = Raw::connect(surface.addr);
        request.send("POST", "/v1/commands", Some("application/json"), wire);
        let response = request.read_response();
        assert_eq!(
            response.status,
            202,
            "body: {}",
            String::from_utf8_lossy(&response.body)
        );
        let accepted = response.json();
        assert_eq!(accepted["accepted"], true);
        let request_id = accepted["request_id"].as_u64().expect("request id");

        let mut http_project: Option<Value> = None;
        while http_project.is_none() {
            let (event, data) = sse.next_event().expect("command_result before stream end");
            if event == "command_result" && data["request_id"].as_u64() == Some(request_id) {
                http_project = Some(data["result"].clone());
            }
        }
        let http_project = http_project.unwrap();

        let service =
            ServiceHandle::spawn(options(temp.path().join("inproc/state")), Arc::new(|| {})).unwrap();
        let mut backend: Box<dyn Backend> = Box::new(service);
        let inproc_id = submit(backend.as_mut(), wire);
        let inproc_project = wait_result(backend.as_mut(), inproc_id).result.unwrap();
        backend.disconnect();

        let http_keys: BTreeSet<&str> = http_project
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let inproc_keys: BTreeSet<&str> = inproc_project
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(http_keys, PROJECT_KEYS.into_iter().collect::<BTreeSet<_>>());
        assert_eq!(inproc_keys, PROJECT_KEYS.into_iter().collect::<BTreeSet<_>>());
        assert_eq!(http_project["name"], "wire parity");
        assert_ne!(
            http_project["id"], inproc_project["id"],
            "separate isolated state roots must mint separate projects"
        );
        assert_ne!(http_project["project_path"], inproc_project["project_path"]);
        assert_eq!(
            normalized_project(http_project),
            normalized_project(inproc_project),
            "HTTP receipt and in-process receipt must agree after dropping identity and managed paths"
        );
    }
}
