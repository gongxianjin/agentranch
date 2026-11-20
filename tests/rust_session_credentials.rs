//! Session-only credential storage: redaction, timeout, and close semantics.
use agentranch::{
    config::Mode,
    core::{
        provision::SecretBytes,
        service::{AppSnapshot, GatewayTransportFactory, ServiceHandle, ServiceOptions, UiCommand},
        session_credentials::{CredentialLease, SessionCredentials},
        ssh_gateway::{SshGatewayConfig, SshWorkerTransport, TransportOutput, WorkerTransport},
    },
};
use serde_json::json;
use std::{sync::{Arc, atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}, Barrier}, time::{Duration, Instant}};

const KEY: &[u8] = b"PRIVATE-KEY-SYNTHETIC-SESSION-ONLY";
const TOKEN: &[u8] = b"MODEL-TOKEN-SYNTHETIC-SESSION-ONLY";

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("test.key"), "07".repeat(32)).unwrap();
    let controller = json!({"schema":"ao-reason-config/1","mode":"real","role":"controller","deployment":"session-test",
        "state_dir":"state",
        "nodes":[{"id":"B","address":"B","key_file":"test.key","slots":1,
            "templates":["sum-v1","project-work-v1"],"admitted":true}]});
    std::fs::write(root.path().join("controller.json"), serde_json::to_vec(&controller).unwrap()).unwrap();
    let config = json!({"schema":"native-ssh-gateway/1","deployment":"session-test","worker_id":"B","host":"b.example","port":22,"user":"worker",
        "known_hosts":"known_hosts","identity_file":"id_ed25519","key_file":"test.key","controller_config":"controller.json","state_dir":"state",
        "remote_worker":"/home/worker/aw-cli","remote_config":"/home/worker/worker.json","artifact_sha256":"ab".repeat(32),"artifact_bytes":123,"timeout_secs":1});
    let path = root.path().join("gateway.json");
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    (root, path)
}

fn wait_for(service: &mut ServiceHandle, request: u64) -> agentranch::core::service::CommandResult {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(reply) = service.drain_results().into_iter().find(|reply| reply.request_id == request) {
            return reply;
        }
        assert!(Instant::now() < until, "command did not return");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_stop(service: &mut ServiceHandle) {
    let until = Instant::now() + Duration::from_secs(5);
    while service.snapshot().running {
        assert!(Instant::now() < until, "service did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_next_tick() { std::thread::sleep(Duration::from_millis(60)); }

fn synthetic_runtime(root: &std::path::Path) -> std::path::PathBuf {
    let runtime = root.join("runtime/python/python.exe");
    std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    std::fs::write(&runtime, b"synthetic interpreter marker; never launched").unwrap();
    runtime
}

struct BlockingTransport { started: Arc<std::sync::Barrier>, release: Arc<std::sync::Barrier> }
impl WorkerTransport for BlockingTransport {
    fn exchange(&mut self, _: &[u8], _: u64) -> anyhow::Result<TransportOutput> {
        self.started.wait();
        self.release.wait();
        Ok(TransportOutput { stdout: vec![], stderr: b"synthetic close".to_vec(), exit_code: Some(255), failure: Some("synthetic close".into()) })
    }
}

struct CountingFactory { calls: Arc<AtomicUsize> }
impl GatewayTransportFactory for CountingFactory {
    fn project_transport(&self, _: SshGatewayConfig, _: CredentialLease, _: Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<Box<dyn WorkerTransport>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(BlockingTransport { started: Arc::new(std::sync::Barrier::new(1)), release: Arc::new(std::sync::Barrier::new(1)) }))
    }
    fn sum_transport(&self, config: SshGatewayConfig, lease: CredentialLease, cancel: Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<Box<dyn WorkerTransport>> {
        self.project_transport(config, lease, cancel)
    }
}

struct BlockingFactory { started: Arc<std::sync::Barrier>, release: Arc<std::sync::Barrier> }
impl GatewayTransportFactory for BlockingFactory {
    fn project_transport(&self, _: SshGatewayConfig, _: CredentialLease, _: Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<Box<dyn WorkerTransport>> {
        Ok(Box::new(BlockingTransport { started: self.started.clone(), release: self.release.clone() }))
    }
    fn sum_transport(&self, _: SshGatewayConfig, _: CredentialLease, _: Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<Box<dyn WorkerTransport>> {
        Ok(Box::new(BlockingTransport { started: self.started.clone(), release: self.release.clone() }))
    }
}

#[test]
fn close_before_dispatch_registration_rejects_factory_and_store_work() {
    let (root, config_path) = fixture();
    let hook_entered = Arc::new(std::sync::Barrier::new(2));
    let hook_release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let hook = {
        let entered = hook_entered.clone();
        let release = hook_release.clone();
        Arc::new(move || {
            entered.wait();
            while !release.load(Ordering::Acquire) { std::thread::yield_now(); }
        })
    };
    let factory = Arc::new(CountingFactory { calls: calls.clone() });
    let mut service = ServiceHandle::spawn_with_test_work_registration_hook(ServiceOptions {
        mode: Mode::Real,
        state_dir: root.path().join("ui"),
        worker_exe: "unused.exe".into(),
        gateway_key: Some(KEY.to_vec()),
        gateway_model_token: Some(TOKEN.to_vec()),
    }, Arc::new(|| {}), Box::new(agentranch::core::provision::BundledProvisioner), factory, hook).unwrap();
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path }).unwrap();
    assert!(wait_for(&mut service, configure).result.is_ok());
    let dispatch = service.try_submit(UiCommand::GatewayDispatch { worker_id: "B".into() }).unwrap();
    hook_entered.wait();
    service.disconnect();
    hook_release.store(true, Ordering::Release);
    let deadline=Instant::now()+Duration::from_millis(300);
    while calls.load(Ordering::Acquire)==0 && Instant::now()<deadline { std::thread::yield_now(); }
    wait_for_stop(&mut service);
    let reply = wait_for(&mut service, dispatch);
    assert!(reply.result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0, "closed dispatch must not construct a transport");
    assert!(service.snapshot().runs.is_empty(), "closed dispatch must not create a durable Store run");
}

#[test]
fn close_during_gateway_configure_publication_rejects_install() {
    let (root, config_path) = fixture();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(AtomicBool::new(false));
    let hook = {
        let entered = entered.clone();
        let release = release.clone();
        Arc::new(move || {
            entered.wait();
            while !release.load(Ordering::Acquire) { std::thread::yield_now(); }
        })
    };
    let factory = Arc::new(CountingFactory { calls: Arc::new(AtomicUsize::new(0)) });
    let mut service = ServiceHandle::spawn_with_test_publication_hook(ServiceOptions {
        mode: Mode::Real,
        state_dir: root.path().join("ui"),
        worker_exe: "unused.exe".into(),
        gateway_key: Some(KEY.to_vec()),
        gateway_model_token: Some(TOKEN.to_vec()),
    }, Arc::new(|| {}), Box::new(agentranch::core::provision::BundledProvisioner), factory, hook).unwrap();
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path }).unwrap();
    entered.wait();
    service.stop();
    release.store(true, Ordering::Release);
    let result = wait_for(&mut service, configure).result;
    assert!(result.is_err(), "close must reject GatewayConfigure publication");
    assert!(service.snapshot().gateway.is_none(), "close must not install a Gateway");
    assert!(!root.path().join("ui/manager-diag.log").exists(), "close must not publish configuration diagnostics");
    wait_for_stop(&mut service);
}

#[test]
fn session_credentials_debug_and_snapshot_are_redacted() {
    let mut credentials = SessionCredentials::default();
    credentials.set_worker_key(SecretBytes::copy_from(KEY));
    credentials.set_model_token(TOKEN);
    assert!(!format!("{credentials:?}").contains("PRIVATE-KEY-SYNTHETIC"));
    assert!(!format!("{credentials:?}").contains("MODEL-TOKEN-SYNTHETIC"));

    let (root, config_path) = fixture();
    let options = ServiceOptions {
        mode: Mode::Real,
        state_dir: root.path().join("ui"),
        worker_exe: "unused.exe".into(),
        gateway_key: Some(KEY.to_vec()),
        gateway_model_token: Some(TOKEN.to_vec()),
    };
    let options_debug = format!("{options:?}");
    assert!(!options_debug.contains("PRIVATE-KEY-SYNTHETIC"));
    assert!(!options_debug.contains("MODEL-TOKEN-SYNTHETIC"));

    let mut service = ServiceHandle::spawn(options, Arc::new(|| {})).unwrap();
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path }).unwrap();
    let receipt = wait_for(&mut service, configure).result.unwrap();
    let serialized_receipt = serde_json::to_string(&receipt).unwrap();
    let snapshot: Arc<AppSnapshot> = service.snapshot();
    let snapshot_debug = format!("{snapshot:?}");
    for visible in [&serialized_receipt, &snapshot_debug] {
        assert!(!visible.contains("PRIVATE-KEY-SYNTHETIC"));
        assert!(!visible.contains("MODEL-TOKEN-SYNTHETIC"));
    }

    service.disconnect();
    wait_for_stop(&mut service);
}

#[test]
fn idle_service_tick_clears_expired_credentials_before_next_dispatch() {
    let (root, config_path) = fixture();
    let now = Arc::new(AtomicU64::new(100));
    let mut service = ServiceHandle::spawn_with_clock(ServiceOptions {
        mode: Mode::Real,
        state_dir: root.path().join("ui"),
        worker_exe: "unused.exe".into(),
        gateway_key: Some(KEY.to_vec()),
        gateway_model_token: Some(TOKEN.to_vec()),
    }, Arc::new(|| {}), {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::Acquire))
    }).unwrap();
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path }).unwrap();
    wait_for(&mut service, configure).result.unwrap();
    now.store(100 + 30 * 60, Ordering::Release);
    wait_for_next_tick();
    let request = service.try_submit(UiCommand::GatewayDispatch { worker_id: "B".into() }).unwrap();
    let error = wait_for(&mut service, request).result.unwrap_err();
    assert!(error.message.contains("credential unavailable"), "unexpected error: {error:?}");
    assert!(service.snapshot().runs.is_empty(), "expired credentials must block before Store run creation");
    service.disconnect();
    wait_for_stop(&mut service);
}

#[test]
fn disconnect_waits_for_in_flight_transport_cleanup_without_releasing_unknown_assignment() {
    let (root, config_path) = fixture();
    let doc = root.path().join("design.md");
    std::fs::write(&doc, "# close boundary\n").unwrap();
    let started = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let factory = Arc::new(BlockingFactory { started: started.clone(), release: release.clone() });
    let mut service = ServiceHandle::spawn_with_clock_and_transport_factory(ServiceOptions {
        mode: Mode::Real,
        state_dir: root.path().join("ui"),
        worker_exe: "unused.exe".into(),
        gateway_key: Some(KEY.to_vec()),
        gateway_model_token: Some(TOKEN.to_vec()),
    }, Arc::new(|| {}), Arc::new(|| 100), factory).unwrap();
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path }).unwrap();
    wait_for(&mut service, configure).result.unwrap();
    let dispatch = service.try_submit(UiCommand::GatewayDispatchProject { worker_id: "B".into(), doc_path: doc }).unwrap();
    started.wait();
    let close_started = Instant::now();
    let releaser = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(40)); release.wait(); });
    service.disconnect();
    assert!(close_started.elapsed() >= Duration::from_millis(30), "disconnect must wait until in-flight lease drops");
    releaser.join().unwrap();
    wait_for_stop(&mut service);
    let reply = wait_for(&mut service, dispatch);
    assert_eq!(reply.result.unwrap()["status"], "unknown");
    assert_eq!(service.snapshot().gateway.as_ref().unwrap().used_slots, 1, "close must retain UNKNOWN assignment");
}

#[test]
fn lease_constructor_failure_keeps_temporary_secrets_zeroized() {
    let root = tempfile::tempdir().unwrap();
    let mut credentials = SessionCredentials::default();
    credentials.set_worker_key(SecretBytes::copy_from(KEY));
    credentials.set_model_token(TOKEN);
    let lease = credentials.take_for_dispatch().unwrap();
    let config = serde_json::from_value(serde_json::json!({"schema":"native-ssh-gateway/1","deployment":"session-test","worker_id":"B","host":"b.example","port":22,"user":"worker","known_hosts":"known_hosts","identity_file":"id_ed25519","key_file":"key","controller_config":"controller.json","state_dir":"state","remote_worker":"/home/worker/aw-cli","remote_config":"/home/worker/worker.json","artifact_sha256":"ab".repeat(32),"artifact_bytes":123,"timeout_secs":1})).unwrap();
    let result = SshWorkerTransport::with_credential_lease_for_test(config, root.path().join("missing-helper.py"), synthetic_runtime(root.path()), lease, true);
    assert!(result.is_err(), "missing helper must fail before transport construction");
    assert!(credentials.take_for_dispatch().is_some(), "constructor failure must not consume session owner");
}

#[test]
fn closing_or_timeout_clears_credentials() {    let mut credentials = SessionCredentials::default();
    credentials.set_worker_key(SecretBytes::copy_from(KEY));
    credentials.set_model_token(TOKEN);
    assert!(credentials.take_for_dispatch().is_some(), "configured session must issue a lease");
    assert!(credentials.clear_if_expired(u64::MAX), "deterministic expiry must clear the session");
    assert!(credentials.take_for_dispatch().is_none(), "expired session must not issue a lease");
    credentials.clear();
    assert!(credentials.take_for_dispatch().is_none(), "repeated clear must be idempotent");

    let (root, config_path) = fixture();
    let mut service = ServiceHandle::spawn(ServiceOptions {
        mode: Mode::Real,
        state_dir: root.path().join("ui"),
        worker_exe: "unused.exe".into(),
        gateway_key: Some(KEY.to_vec()),
        gateway_model_token: Some(TOKEN.to_vec()),
    }, Arc::new(|| {})).unwrap();
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path }).unwrap();
    wait_for(&mut service, configure).result.unwrap();
    service.disconnect();
    service.disconnect();
    wait_for_stop(&mut service);
    assert!(matches!(service.try_submit(UiCommand::GatewayDispatch { worker_id: "B".into() }), Err(agentranch::core::service::SubmitError::Stopped)));
}

