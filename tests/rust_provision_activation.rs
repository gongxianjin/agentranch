use agentranch::{config::Mode, core::{provision::{ProvisionFailure, Provisioner, ProvisioningContext, ProvisionResult}, service::{GatewayTransportFactory, ProvisionRequest, ServiceHandle, ServiceOptions, UiCommand}, ssh_gateway::{SshGatewayConfig, WorkerTransport}}};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::{Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}, Barrier}, time::{Duration, Instant}};

fn request() -> ProvisionRequest {
    ProvisionRequest { host:"192.0.2.10".into(), root_user:"root".into(), root_password:b"root-secret".to_vec(),
        model_base_url:"https://model.example".into(), model_token:b"api-secret".to_vec(), model:"test-model".into() }
}
fn sha(raw:&[u8])->String { hex::encode(Sha256::digest(raw)) }
fn write_json(path:&Path, value:&Value) { std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap(); }

struct NoopFactory;
impl GatewayTransportFactory for NoopFactory {
    fn sum_transport(&self, _: SshGatewayConfig, _: agentranch::core::session_credentials::CredentialLease, _: Arc<AtomicBool>) -> anyhow::Result<Box<dyn WorkerTransport>> { anyhow::bail!("not used") }
    fn project_transport(&self, _: SshGatewayConfig, _: agentranch::core::session_credentials::CredentialLease, _: Arc<AtomicBool>) -> anyhow::Result<Box<dyn WorkerTransport>> { anyhow::bail!("not used") }
}

struct BlockingPassProvisioner { inner: FixtureProvisioner, entered: Arc<Barrier>, release: Arc<AtomicBool> }
impl Provisioner for BlockingPassProvisioner {
    fn provision(&self, request:&ProvisionRequest)->Result<ProvisionResult, ProvisionFailure> { self.provision_with_context(request, &ProvisioningContext::from_state_dir(self.inner.bundle.parent().unwrap()).unwrap()) }
    fn prerequisites_root(&self)->Result<PathBuf, ProvisionFailure> { self.inner.prerequisites_root() }
    fn provision_with_context(&self, request:&ProvisionRequest, context:&ProvisioningContext)->Result<ProvisionResult, ProvisionFailure> {
        self.entered.wait();
        while !self.release.load(Ordering::Acquire) { std::thread::yield_now(); }
        self.inner.provision_with_context(request, context)
    }
}

struct FixtureProvisioner { bundle:PathBuf, mutation:&'static str, calls:Arc<AtomicUsize> }
impl Provisioner for FixtureProvisioner {
    fn provision(&self, _: &ProvisionRequest)->Result<ProvisionResult, ProvisionFailure> { panic!("Manager context required") }
    fn prerequisites_root(&self)->Result<PathBuf, ProvisionFailure> { Ok(self.bundle.clone()) }
    fn provision_with_context(&self, request:&ProvisionRequest, context:&ProvisioningContext)->Result<ProvisionResult, ProvisionFailure> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.mutation == "fail_second" && call == 2 {
            return Err(ProvisionFailure::new("synthetic_second_failure"));
        }
        let output=context.output_dir();
        std::fs::create_dir_all(output).unwrap();
        std::fs::write(output.join("protocol.key"),format!("{}\n","70".repeat(32))).unwrap();
        let mut controller=json!({"schema":"ao-reason-config/1","mode":"real","role":"controller","deployment":"worker-test",
            "state_dir":output.join("controller-state"),"nodes":[{"id":"worker-b","address":"192.0.2.10:22","key_file":output.join("protocol.key"),
            "slots":1,"templates":["sum-v1","control-v1","project-work-v1"],"admitted":true,"healthy":false,"draining":false}]});
        let mut gateway=json!({"schema":"native-ssh-gateway/1","deployment":"worker-test","worker_id":"worker-b","host":"192.0.2.10",
            "port":22,"user":"operator","known_hosts":self.bundle.join("known_hosts"),"identity_file":output.join("ordinary-user-rsa"),
            "controller_config":output.join("controller.json"),"key_file":output.join("protocol.key"),"state_dir":output.join("gateway-state"),
            "remote_worker":"/home/operator/cross-runtime-test/aw-cli","remote_config":"/home/operator/cross-runtime-test/worker.json",
            "artifact_sha256":sha(b"worker"),"artifact_bytes":6,"timeout_secs":300,"launch_mode":"delegated-user-service",
            "expected_worker_timeout_secs":270,"remote_library_path":"/home/operator/cross-runtime-test/lib",
            "remote_library_sha256":sha(b"loader"),"remote_library_bytes":6});
        let mut worker=json!({"schema":"native-worker-config/1","deployment":"worker-test","worker_id":"worker-b","key_file":"protocol.key",
            "cgroup_root":"/sys/fs/cgroup/user.slice/user-1007.slice/user@1007.service","cgroup_root_mode":"current-unit",
            "artifact_sha256":sha(b"worker"),"timeout_secs":270});
        let mut endpoint=json!({"schema":"model-endpoint/1","base_url":"https://model.example","model":"test-model",
            "certificate_pem":"-----BEGIN CERTIFICATE-----\nsynthetic\n-----END CERTIFICATE-----\n","timeout_secs":240});
        match self.mutation {
            "host"=>gateway["host"]=json!("192.0.2.11"),
            "artifact"=>gateway["artifact_sha256"]=json!("ab".repeat(32)),
            "trust"=>gateway["known_hosts"]=json!(output.join("unapproved-hosts")),
            "state_escape"=>controller["state_dir"]=json!(self.bundle.join("escaped-state")),
            "controller_escape"=>gateway["controller_config"]=json!(self.bundle.join("controller.json")),
            "worker"=>worker["deployment"]=json!("different"),
            "endpoint"=>endpoint["base_url"]=json!("https://different.example"),
            "certificate"=>endpoint["certificate_pem"]=json!("-----BEGIN CERTIFICATE-----different-----END CERTIFICATE-----"),
            "key_format"=>std::fs::write(output.join("protocol.key"),b"p".repeat(32)).unwrap(),
            "secret_file"=>{std::fs::write(output.join("ordinary-user-rsa"),b"synthetic-private-key").unwrap();},
            "ledger_write_failure"=>{
                let state_dir = output.parent().and_then(Path::parent).expect("fixture state dir");
                std::fs::create_dir_all(state_dir).unwrap();
                std::fs::create_dir(state_dir.join("provisioning-ledger.json")).unwrap();
            },
            "ledger_write_failure_once" if self.calls.load(Ordering::SeqCst) == 1 => {
                let state_dir = output.parent().and_then(Path::parent).expect("fixture state dir");
                std::fs::create_dir_all(state_dir).unwrap();
                std::fs::create_dir(state_dir.join("provisioning-ledger.json")).unwrap();
            },
            "diag_write_failure" => {
                let state_dir = output.parent().and_then(Path::parent).expect("fixture state dir");
                std::fs::create_dir_all(state_dir).unwrap();
                std::fs::create_dir(state_dir.join("manager-diag.log")).unwrap();
            },
            _=>{},
        }
        write_json(&output.join("controller.json"), &controller);
        write_json(&output.join("gateway.json"), &gateway);
        write_json(&output.join("worker.json"), &worker);
        write_json(&output.join("model-endpoint.json"), &endpoint);
        let result=json!({"schema":"worker-provision/1","status":"PASS_WORKER_PROVISION","preflight":true,
            "gateway_path":output.join("gateway.json"),"host":request.host,"worker_id":"worker-b", "artifact_sha256":sha(b"worker"),"artifact_bytes":6,"attempt_id":context.attempt_id()});
        let raw=serde_json::to_vec(&result).unwrap();
        let key=if self.mutation=="empty_key" { &b""[..] } else { &b"synthetic-private-key"[..] };
        let mut frame=b"AW-WORKER-PROVISION-RESULT/1\n".to_vec();
        frame.extend_from_slice(&(raw.len() as u32).to_be_bytes()); frame.extend_from_slice(&raw);
        frame.extend_from_slice(&(key.len() as u32).to_be_bytes()); frame.extend_from_slice(key);
        let mut parsed = agentranch::core::provision::decode_result_frame_in_dir_with_attempt(&frame,&request.root_password,&request.model_token,context).map_err(|_|ProvisionFailure::new("fixture_invalid"))?;
        if self.mutation == "attempt" {
            parsed.metadata["attempt_id"] = json!("attempt-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        }
        Ok(parsed)
    }
}
fn bundle(root:&Path)->PathBuf {
    let bundle=root.join("bundle"); std::fs::create_dir(&bundle).unwrap();
    for (name, raw) in [("aw-cli", &b"worker"[..]),("libsqlite3.so.0", &b"loader"[..]),("known_hosts", &b"synthetic-host-key\n"[..]),
        ("model.pem", &b"-----BEGIN CERTIFICATE-----\nsynthetic\n-----END CERTIFICATE-----\n"[..])] {
        std::fs::write(bundle.join(name),raw).unwrap();
    }
    write_json(&bundle.join("assets.json"),&json!({"schema":"worker-provision-assets/1","version":"0.3.0",
        "worker":{"path":"aw-cli","bytes":6,"sha256":sha(b"worker")},"loader":{"path":"libsqlite3.so.0","bytes":6,"sha256":sha(b"loader")}}));
    write_json(&bundle.join("provisioning-prerequisites.json"),&json!({"schema":"worker-provision-prerequisites/1","asset_manifest":"assets.json",
        "asset_version":"0.3.0","known_hosts":"known_hosts","tls_certificate":"model.pem"}));
    bundle
}
fn service(root:&Path, mutation:&'static str)->(ServiceHandle, Arc<AtomicUsize>) {
    let calls=Arc::new(AtomicUsize::new(0));
    let provisioner=FixtureProvisioner { bundle:bundle(root), mutation, calls:calls.clone() };
    let service=ServiceHandle::spawn_with_provisioner(ServiceOptions { mode:Mode::Real,state_dir:root.join("ui"),worker_exe:"unused.exe".into(),gateway_key:None,gateway_model_token:None },Arc::new(||{}),Box::new(provisioner)).unwrap();
    (service,calls)
}

#[test]
fn activation_diagnostic_write_failure_does_not_publish_gateway_or_credentials() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, calls) = service(root.path(), "diag_write_failure");
    let id = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    let result = reply(&mut service, id).result;
    assert!(result.is_err(), "activation must fail when its durable diagnostic cannot be written");
    assert!(service.snapshot().gateway.is_none(), "diagnostic failure must not publish a Gateway");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    service.stop();
}

#[test]
fn expired_session_reprepare_failure_does_not_keep_an_old_submit_ready_gateway() {
    let root = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let provisioner = FixtureProvisioner { bundle: bundle(root.path()), mutation: "fail_second", calls: calls.clone() };
    let mut service = ServiceHandle::spawn_with_clock_and_provisioner(
        ServiceOptions { mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}), clock, Box::new(provisioner),
    ).unwrap();
    let first = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(reply(&mut service, first).result.is_ok());
    now.store(2_801, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if service.snapshot().gateway.as_ref().is_some_and(|gateway| !gateway.credentials_available) { break; }
        assert!(Instant::now() < deadline, "expired credentials must invalidate the published Gateway");
        std::thread::sleep(Duration::from_millis(5));
    }
    let second = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(reply(&mut service, second).result.is_err());
    let snapshot = service.snapshot();
    assert!(snapshot.gateway.is_none(), "failed reprepare must not leave an expired Gateway snapshot");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    service.stop();
}

#[test]
fn expired_session_allows_same_manager_to_start_a_new_provisioning_attempt() {
    let root = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let provisioner = FixtureProvisioner { bundle: bundle(root.path()), mutation: "", calls: calls.clone() };
    let mut service = ServiceHandle::spawn_with_clock_and_provisioner(
        ServiceOptions { mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}),
        clock,
        Box::new(provisioner),
    ).unwrap();

    let first = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(reply(&mut service, first).result.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    now.store(2_801, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if service.snapshot().gateway.as_ref().is_some_and(|gateway| !gateway.credentials_available) { break; }
        assert!(Instant::now() < deadline, "expired credentials must invalidate the published Gateway");
        std::thread::sleep(Duration::from_millis(5));
    }

    let second = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(reply(&mut service, second).result.is_ok(), "expired session must accept reprepare");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "reprepare must invoke provisioning again");
    service.stop();
}
fn reply(service:&mut ServiceHandle, id:u64)->agentranch::core::service::CommandResult {
    let deadline=Instant::now()+Duration::from_secs(5);
    loop { if let Some(reply)=service.drain_results().into_iter().find(|r|r.request_id==id) { return reply; }
        assert!(Instant::now()<deadline,"receipt timeout");std::thread::sleep(Duration::from_millis(5)); }
}
#[test]
fn close_during_post_validation_publication_does_not_publish_gateway_or_credentials() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
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
    let provisioner = FixtureProvisioner { bundle: bundle(root.path()), mutation: "", calls: calls.clone() };
    let mut service = ServiceHandle::spawn_with_test_publication_hook(
        ServiceOptions { mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}), Box::new(provisioner), Arc::new(NoopFactory), hook,
    ).unwrap();
    let id = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    entered.wait();
    service.stop();
    release.store(true, Ordering::Release);
    let result = reply(&mut service, id).result;
    assert!(result.is_err(), "close must reject post-validation publication");
    assert!(service.snapshot().gateway.is_none(), "close must not publish a Gateway");
    assert!(!root.path().join("ui/manager-diag.log").exists(), "close must not publish activation diagnostics");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn close_during_blocking_pass_does_not_publish_gateway_or_credentials() {
    let root=tempfile::tempdir().unwrap();
    let calls=Arc::new(AtomicUsize::new(0));
    let entered=Arc::new(Barrier::new(2));
    let release=Arc::new(AtomicBool::new(false));
    let inner=FixtureProvisioner { bundle:bundle(root.path()), mutation:"", calls:calls.clone() };
    let provisioner=BlockingPassProvisioner { inner, entered:entered.clone(), release:release.clone() };
    let mut service=ServiceHandle::spawn_with_provisioner(ServiceOptions { mode:Mode::Real,state_dir:root.path().join("ui"),worker_exe:"unused.exe".into(),gateway_key:None,gateway_model_token:None },Arc::new(||{}),Box::new(provisioner)).unwrap();
    let id=service.try_submit(UiCommand::ProvisionWorker{request:request()}).unwrap();
    entered.wait();
    service.disconnect();
    release.store(true,Ordering::Release);
    let result=reply(&mut service,id).result;
    assert!(result.is_err(), "PASS arriving after close must be rejected");
    assert!(service.snapshot().gateway.is_none(), "close must not publish a Gateway");
    assert!(!root.path().join("ui/manager-diag.log").exists(), "close must not publish activation diagnostics");
    assert_eq!(calls.load(Ordering::SeqCst),1);
}

#[test]
fn provisioning_success_activates_generated_gateway_before_receipt_without_dispatch() {
    let root=tempfile::tempdir().unwrap();let(mut service,calls)=service(root.path(),"");
    let id=service.try_submit(UiCommand::ProvisionWorker{request:request()}).unwrap();
    let result=reply(&mut service,id).result.unwrap();
    assert_eq!(result["state"],"worker_preflight"); assert_eq!(result["remote_verified"],false);
    let snapshot=service.snapshot();let gateway=snapshot.gateway.as_ref().expect("success receipt must publish gateway");
    assert_eq!(gateway.worker_id,"worker-b");assert_eq!(gateway.host,"192.0.2.10");assert_eq!(gateway.used_slots,0);
    assert!(!gateway.remote_verified);assert!(snapshot.runs.is_empty());assert_eq!(calls.load(Ordering::SeqCst),1);
    assert_eq!(snapshot.provision_ledger.as_ref().map(|ledger| ledger.state.clone()), Some(agentranch::core::provision_ledger::LedgerState::Preflight), "success snapshot must publish the persisted preflight ledger");
    assert!(std::fs::read_to_string(root.path().join("ui/manager-diag.log")).unwrap().contains("gateway_configured"));
    let second=service.try_submit(UiCommand::ProvisionWorker{request:request()}).unwrap();
    assert!(reply(&mut service,second).result.is_err());assert_eq!(calls.load(Ordering::SeqCst),1,"must refuse replacement before SSH");
    service.stop();
}
#[test]
fn preflight_fact_is_current_session_only_and_clears_on_manager_restart() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, _) = service(root.path(), "");
    let id = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(reply(&mut service, id).result.is_ok());
    let activated = service.snapshot();
    let gateway = activated.gateway.as_ref().expect("activated Gateway missing");
    assert!(gateway.preflight_passed, "successful activation must project the session preflight fact");
    assert!(!gateway.remote_verified, "preflight must not imply remote verification");
    service.stop();

    let mut restarted = ServiceHandle::spawn_with_provisioner(
        ServiceOptions { mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}),
        Box::new(FixtureProvisioner { bundle: root.path().join("not-used"), mutation: "", calls: Arc::new(AtomicUsize::new(0)) }),
    ).unwrap();
    assert!(restarted.snapshot().gateway.is_none(), "session preflight fact must not survive manager restart");
    restarted.stop();
}

#[test]
fn activation_rejects_untrusted_or_inconsistent_configs_before_store_creation() {
    for mutation in ["host","artifact","trust","state_escape","controller_escape","worker","endpoint","certificate","key_format","secret_file","empty_key","attempt"] {
        let root=tempfile::tempdir().unwrap();let(mut service,_)=service(root.path(),mutation);
        let id=service.try_submit(UiCommand::ProvisionWorker{request:request()}).unwrap();
        let result=reply(&mut service,id).result;
        assert!(result.is_err(),"accepted {mutation}");assert!(service.snapshot().gateway.is_none(),"activated {mutation}");
        let parent=root.path().join("ui/provisioning");
        for entry in std::fs::read_dir(parent).unwrap() { assert!(!entry.unwrap().path().join("controller-state").exists(),"Store created for {mutation}"); }
        assert!(!root.path().join("bundle/escaped-state").exists());service.stop();
    }
}

#[test]
fn activation_failure_replaces_pending_ledger_with_safe_failed_state() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, calls) = service(root.path(), "host");
    let id = service
        .try_submit(UiCommand::ProvisionWorker { request: request() })
        .unwrap();

    assert!(reply(&mut service, id).result.is_err());
    let ledger = agentranch::core::provision_ledger::ProvisionLedger::load(
        &root.path().join("ui"),
    )
    .unwrap()
    .expect("activation failure must leave a diagnostic ledger");
    assert_eq!(ledger.state, agentranch::core::provision_ledger::LedgerState::Failed);
    assert_eq!(ledger.failure_code.as_deref(), Some("activation_blocked"));
    assert_eq!(
        ledger.failure_next_step.as_deref(),
        Some(agentranch::core::provision_ledger::safe_failure_next_step())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(service.snapshot().gateway.is_none());
    service.stop();
}

#[test]
fn one_time_ledger_write_failure_can_retry_without_publishing_stale_session() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, calls) = service(root.path(), "ledger_write_failure_once");
    let first = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(reply(&mut service, first).result.is_err());
    assert!(service.snapshot().gateway.is_none());
    std::fs::remove_dir(root.path().join("ui/provisioning-ledger.json")).unwrap();

    let second = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(reply(&mut service, second).result.is_ok(), "retry after isolated ledger fault must activate: {:?}", service.snapshot());
    assert!(service.snapshot().gateway.is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    service.stop();
}

#[test]
fn ledger_write_failure_rolls_back_gateway_preflight_and_session_credentials() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, calls) = service(root.path(), "ledger_write_failure");
    let id = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    let result = reply(&mut service, id).result;
    assert!(result.is_err(), "ledger failure must reject activation");
    assert!(service.snapshot().gateway.is_none(), "ledger failure must not publish Gateway");
    let dispatch = service.try_submit(UiCommand::GatewayDispatchProject {
        worker_id: "worker-b".into(),
        doc_path: root.path().join("missing-design.md"),
    }).unwrap();
    assert!(reply(&mut service, dispatch).result.is_err(), "ledger failure must not leave a dispatchable session");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    service.stop();
}
