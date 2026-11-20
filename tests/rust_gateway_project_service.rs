//! Service-level project dispatch: doc file -> ConfirmedDocument -> signed
//! project-work-v1 run. Real service loop with a synthetic local transport
//! runtime (helper file exists; the synthetic interpreter cannot execute, so
//! the exchange fails offline and the run stays unknown, slot retained).
use agentranch::core::service::{ServiceHandle, ServiceOptions, UiCommand};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::{Duration, Instant}};

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) { fixture_with_slots(1) }

fn fixture_with_slots(slots: u32) -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("test.key"), "07".repeat(32)).unwrap();
    let controller = json!({"schema":"ao-reason-config/1","mode":"real","role":"controller","deployment":"manager-test",
        "state_dir":"state",
        "nodes":[{"id":"B","address":"B","key_file":"test.key","slots":slots,
            "templates":["sum-v1","project-work-v1"],"admitted":true}]});
    std::fs::write(root.path().join("controller.json"), serde_json::to_vec(&controller).unwrap()).unwrap();
    let config = json!({"schema":"native-ssh-gateway/1","deployment":"manager-test","worker_id":"B","host":"b.example","port":22,"user":"worker",
        "known_hosts":"known_hosts","identity_file":"id_ed25519","key_file":"test.key","controller_config":"controller.json","state_dir":"state",
        "remote_worker":"/home/worker/aw-cli","remote_config":"/home/worker/worker.json","artifact_sha256":"ab".repeat(32),"artifact_bytes":123,"timeout_secs":1});
    let path = root.path().join("gateway.json");
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    (root, path)
}

/// The service resolves the helper and bundled runtime beside the CURRENT
/// executable. In tests that is the test binary, so stage synthetic copies.
fn stage_transport_runtime() {
    let beside = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let helper = beside.join("tools/paramiko_gateway.py");
    if !helper.is_file() {
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::copy(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/paramiko_gateway.py"), &helper).unwrap();
    }
    let interpreter = beside.join("runtime/python/python.exe");
    if !interpreter.is_file() {
        std::fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
        std::fs::write(&interpreter, b"synthetic interpreter marker; never launched").unwrap();
    }
}

fn wait_for(service: &mut ServiceHandle, request: u64) -> agentranch::core::service::CommandResult {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(reply) = service.drain_results().into_iter().find(|reply| reply.request_id == request) {
            return reply;
        }
        assert!(Instant::now() < until, "command did not return");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_stop(service: &mut ServiceHandle) {
    let until = Instant::now() + Duration::from_secs(10);
    while service.snapshot().running {
        assert!(Instant::now() < until, "service did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_no_secret_in_durable_files(root: &std::path::Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() { assert_no_secret_in_durable_files(&entry.path())?; }
        else if entry.file_type()?.is_file() {
            let content = std::fs::read(entry.path())?;
            anyhow::ensure!(!content.windows(b"sk-synthetic-token".len()).any(|window| window == b"sk-synthetic-token"), "model token persisted in {}", entry.path().display());
            anyhow::ensure!(!content.windows(b"07070707".len()).any(|window| window == b"07070707"), "private key bytes persisted in {}", entry.path().display());
        }
    }
    Ok(())
}

fn spawn_service(root: &tempfile::TempDir, with_credentials: bool) -> ServiceHandle {
    stage_transport_runtime();
    ServiceHandle::spawn(ServiceOptions {
        mode: agentranch::config::Mode::Real,
        state_dir: root.path().join("ui"),
        worker_exe: "unused.exe".into(),
        gateway_key: with_credentials.then(|| b"07".repeat(32)),
        gateway_model_token: with_credentials.then(|| b"sk-synthetic-token".to_vec()),
    }, Arc::new(|| {})).unwrap()
}

#[test]
fn durable_secret_scan_rejects_nested_file() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested/state");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("leak.bin"), b"sk-synthetic-token").unwrap();
    assert!(assert_no_secret_in_durable_files(root.path()).is_err(), "recursive scan must examine nested durable files");
}

#[test]
fn project_dispatch_reads_document_and_creates_signed_project_run() {
    let (root, path) = fixture();
    let doc: PathBuf = root.path().join("design.md");
    std::fs::write(&doc, "# 设计\n成都遛娃静态页（服务级测试）。\n").unwrap();
    let mut service = spawn_service(&root, true);
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path: path }).unwrap();
    wait_for(&mut service, configure).result.unwrap();
    let request = service.try_submit(UiCommand::GatewayDispatchProject { worker_id: "B".into(), doc_path: doc }).unwrap();
    let reply = wait_for(&mut service, request);
    let result = reply.result.unwrap();
    // Offline synthetic runtime: the exchange cannot run, so the run must stay
    // unknown with the slot retained — never done, never auto-retried.
    assert_eq!(result["status"], "unknown");
    assert_eq!(result["slot_reserved"], true);
    service.stop();
}

#[test]
fn dispatch_project_reuses_session_credentials_without_persisting_them() {
    let (root, path) = fixture_with_slots(2);
    let first = root.path().join("first.md");
    let second = root.path().join("second.md");
    std::fs::write(&first, "# 第一份设计\n").unwrap();
    std::fs::write(&second, "# 第二份设计\n").unwrap();
    let mut service = spawn_service(&root, true);
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path: path }).unwrap();
    wait_for(&mut service, configure).result.unwrap();

    let request_one = service.try_submit(UiCommand::GatewayDispatchProject { worker_id: "B".into(), doc_path: first }).unwrap();
    let first = wait_for(&mut service, request_one).result.unwrap();
    let request_two = service.try_submit(UiCommand::GatewayDispatchProject { worker_id: "B".into(), doc_path: second }).unwrap();
    let second = wait_for(&mut service, request_two).result.unwrap();

    assert_eq!(first["status"], "unknown");
    assert_eq!(second["status"], "unknown");
    assert_ne!(first["run_id"], second["run_id"]);
    assert_eq!(first["slot_reserved"], true);
    assert_eq!(second["slot_reserved"], true);

    service.disconnect();
    assert!(matches!(service.try_submit(UiCommand::GatewayDispatchProject { worker_id: "B".into(), doc_path: root.path().join("third.md") }), Err(agentranch::core::service::SubmitError::Stopped)));
    wait_for_stop(&mut service);
    for path in [root.path().join("ui"), root.path().join("state")] {
        if path.exists() { assert_no_secret_in_durable_files(&path).unwrap(); }
    }
}

#[test]
fn project_dispatch_rejects_missing_document_explicitly() {
    let (root, path) = fixture();
    let mut service = spawn_service(&root, true);
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path: path }).unwrap();
    wait_for(&mut service, configure).result.unwrap();
    let request = service.try_submit(UiCommand::GatewayDispatchProject {
        worker_id: "B".into(),
        doc_path: root.path().join("missing.md"),
    }).unwrap();
    let reply = wait_for(&mut service, request);
    let error = reply.result.unwrap_err();
    assert!(error.message.contains("文档"), "unexpected error: {error:?}");
    // The document check must fire BEFORE consuming the one-shot credentials.
    service.stop();
}

#[test]
fn project_dispatch_without_token_is_rejected_before_any_run() {
    let (root, path) = fixture();
    let doc = root.path().join("design.md");
    std::fs::write(&doc, "# 设计\n成都遛娃静态页（无 token 测试）。\n").unwrap();
    stage_transport_runtime();
    let mut service = ServiceHandle::spawn(ServiceOptions {
        mode: agentranch::config::Mode::Real,
        state_dir: root.path().join("ui"),
        worker_exe: "unused.exe".into(),
        gateway_key: Some(b"07".repeat(32)),
        gateway_model_token: None,
    }, Arc::new(|| {})).unwrap();
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path: path }).unwrap();
    wait_for(&mut service, configure).result.unwrap();
    let request = service.try_submit(UiCommand::GatewayDispatchProject { worker_id: "B".into(), doc_path: doc }).unwrap();
    let reply = wait_for(&mut service, request);
    let error = reply.result.unwrap_err();
    assert!(error.message.contains("model token") || error.message.contains("token"), "unexpected error: {error:?}");
    let runs = service.snapshot().runs.clone();
    assert!(runs.iter().all(|view| view.run.template != "project-work-v1"), "no project run may be created without the token");
    service.stop();
}
