#[cfg(feature = "gui")]
#[path = "../src/desktop/mod.rs"]
mod desktop;

use agentranch::{
    config::Mode,
    core::{
        owner_producer::{CleanupFacts, LaunchFacts, OwnedProcessPort, OwnerLaunchPlan, StreamReadOutcome, TerminalFacts, produce_owner_evidence},
        project_work::ConfirmedDocument,
        projects::Projects,
        protocol::{Envelope, PROTOCOL, canonical},
        service::{GatewayTransportFactory, ProvisionRequest, ServiceHandle, ServiceOptions, UiCommand},
        ssh_gateway::{SshGatewayConfig, TransportOutput, WorkerTransport},
        store::ExitProof,
        worker_owner::{OwnerEvidenceReference, OwnerVerification, owner_binding_sha256, owner_bundle_sha256, owner_report_sha256},
    },
};
use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::{Path, PathBuf}, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}}, time::{Duration, Instant}};

fn wait_for(service: &mut ServiceHandle, request: u64) -> agentranch::core::service::CommandResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = service.drain_results().into_iter().find(|item| item.request_id == request) {
            return result;
        }
        assert!(Instant::now() < deadline, "service request timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_snapshot_run(service: &mut ServiceHandle, run_id: &str) -> Arc<agentranch::core::service::AppSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = service.snapshot();
        if snapshot.runs.iter().any(|view| view.run.id == run_id) {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "service snapshot did not publish run {run_id}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn create_project(projects: &mut Projects) -> String {
    projects.command(UiCommand::ProjectCreate { name: "direct project work".into() })
        .unwrap()["id"].as_str().unwrap().into()
}

fn import_and_confirm(projects: &mut Projects, id: &str, text: &str) -> ConfirmedDocument {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("design.md");
    std::fs::write(&path, text).unwrap();
    projects.command(UiCommand::ProjectImport { project_id: id.into(), path }).unwrap();
    let document = projects.snapshots().into_iter().find(|project| project.id == id).unwrap().document.unwrap();
    projects.command(UiCommand::ProjectConfirm { project_id: id.into(), version: document.version, sha256: document.sha256.clone() }).unwrap();
    projects.confirmed_document(id).unwrap()
}

#[test]
fn confirmed_document_accessor_returns_immutable_verified_identity() {
    let root = tempfile::tempdir().unwrap();
    let mut projects = Projects::open(root.path()).unwrap();
    let id = create_project(&mut projects);
    let text = "# Confirmed\nStored document only.\n";
    let document = import_and_confirm(&mut projects, &id, text);
    assert_eq!(document.version, 1);
    assert_eq!(document.text, text);
    assert_eq!(document.sha256, hex::encode(Sha256::digest(text.as_bytes())));

}

#[test]
fn project_work_submit_without_current_activation_is_blocked_before_store_admission() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("ui");
    let mut projects = Projects::open(&state).unwrap();
    let id = create_project(&mut projects);
    let _ = import_and_confirm(&mut projects, &id, "# Confirmed\nNo activation yet.\n");
    drop(projects);

    let mut service = ServiceHandle::spawn(ServiceOptions {
        mode: Mode::Real,
        state_dir: state,
        worker_exe: "unused.exe".into(),
        gateway_key: None,
        gateway_model_token: None,
    }, Arc::new(|| {})).unwrap();
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap_err();
    assert_eq!(result.code, "project_work_blocked");
    assert!(result.next_step.contains("Worker") || result.message.contains("Worker"));
    assert!(service.snapshot().runs.is_empty(), "blocked novice submit must not create a Store run");
    service.stop();
}

#[test]
fn config_only_gateway_cannot_grant_the_direct_submit_gate() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("ui");
    let mut projects = Projects::open(&state).unwrap();
    let id = create_project(&mut projects);
    let _ = import_and_confirm(&mut projects, &id, "# Confirmed\nConfig-only is insufficient.\n");
    drop(projects);

    let mut service = ServiceHandle::spawn(ServiceOptions {
        mode: Mode::Real, state_dir: state, worker_exe: "unused.exe".into(), gateway_key: Some(b"x".to_vec()), gateway_model_token: Some(b"token".to_vec()),
    }, Arc::new(|| {})).unwrap();
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path: gateway_config(root.path()) }).unwrap();
    assert!(wait_for(&mut service, configure).result.is_ok());
    assert!(!service.snapshot().gateway.as_ref().unwrap().preflight_passed, "config-only must not project Task3 activation");
    let submit = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id }).unwrap();
    let result = wait_for(&mut service, submit).result.unwrap_err();
    assert_eq!(result.code, "project_work_blocked");
    assert!(result.message.contains("自动准备"));
    assert!(service.snapshot().runs.is_empty(), "config-only submit must create no Store run or assignment");
    service.stop();
}

#[test]
fn project_work_payload_type_has_no_novice_worker_or_path_arguments() {
    let command = UiCommand::ProjectWorkSubmit { project_id: "project-id".into() };
    let debug = format!("{command:?}");
    assert!(debug.contains("ProjectWorkSubmit"));
    assert!(!debug.contains("worker_id"));
    assert!(!debug.contains("doc_path"));
    assert!(!debug.contains("gateway_path"));
}

#[test]
fn unconfirmed_document_is_blocked_before_store_admission() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("ui");
    let mut projects = Projects::open(&state).unwrap();
    let id = create_project(&mut projects);
    let path = root.path().join("draft.md");
    std::fs::write(&path, "# Draft\nConfirmation required.\n").unwrap();
    projects.command(UiCommand::ProjectImport { project_id: id.clone(), path }).unwrap();
    drop(projects);

    let captured = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FixtureFactory { captured, calls: calls.clone(), outcome: FixtureOutcome::Done });
    let mut service = ServiceHandle::spawn_with_test_work_registration_hook(
        ServiceOptions { mode: Mode::Real, state_dir: state.clone(), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}), Box::new(FixtureProvisioner { bundle: bundle(root.path()) }), factory, Arc::new(|| {}),
    ).unwrap();
    let activate = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(wait_for(&mut service, activate).result.is_ok());
    let submit = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id }).unwrap();
    let result = wait_for(&mut service, submit).result.unwrap_err();
    assert_eq!(result.code, "project_work_blocked");
    assert!(result.message.contains("confirmed document required"), "unconfirmed document must be explicit: {result:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "unconfirmed document must not reach transport");
    assert!(service.snapshot().runs.is_empty(), "unconfirmed document must create no Store run or assignment");
    service.stop();
}

fn request() -> ProvisionRequest {
    ProvisionRequest { host: "192.0.2.10".into(), root_user: "root".into(), root_password: b"root-secret".to_vec(), model_base_url: "https://model.example".into(), model_token: b"model-token".to_vec(), model: "fixture-model".into() }
}

fn gateway_config(root: &Path) -> PathBuf {
    let key = root.join("config-only.key");
    std::fs::write(&key, "07".repeat(32)).unwrap();
    let controller = root.join("config-only-controller.json");
    write_json(&controller, &json!({
        "schema":"ao-reason-config/1","mode":"real","role":"controller","deployment":"config-only-fixture","state_dir":root.join("config-only-state"),
        "nodes":[{"id":"worker-c","address":"192.0.2.11:22","key_file":key,"slots":1,"templates":["sum-v1","project-work-v1"],"admitted":true,"healthy":false,"draining":false}]
    }));
    let gateway = root.join("config-only-gateway.json");
    write_json(&gateway, &json!({
        "schema":"native-ssh-gateway/1","deployment":"config-only-fixture","worker_id":"worker-c","host":"192.0.2.11","port":22,"user":"operator","known_hosts":root.join("known_hosts"),"identity_file":root.join("ordinary-user-rsa"),"controller_config":controller,"key_file":key,"state_dir":root.join("config-only-gateway-state"),"remote_worker":"/opt/aw/aw-cli","remote_config":"/etc/aw/worker.json","artifact_sha256":sha(b"worker"),"artifact_bytes":6,"timeout_secs":30
    }));
    std::fs::write(root.join("known_hosts"), b"fixture-host-key\n").unwrap();
    gateway
}

fn sha(bytes: &[u8]) -> String { hex::encode(Sha256::digest(bytes)) }

fn write_json(path: &Path, value: &Value) { std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap(); }

struct FixtureProvisioner { bundle: PathBuf }
impl agentranch::core::provision::Provisioner for FixtureProvisioner {
    fn provision(&self, _: &ProvisionRequest) -> Result<agentranch::core::provision::ProvisionResult, agentranch::core::provision::ProvisionFailure> { panic!("context required") }
    fn prerequisites_root(&self) -> Result<PathBuf, agentranch::core::provision::ProvisionFailure> { Ok(self.bundle.clone()) }
    fn provision_with_context(&self, request: &ProvisionRequest, context: &agentranch::core::provision::ProvisioningContext) -> Result<agentranch::core::provision::ProvisionResult, agentranch::core::provision::ProvisionFailure> {
        let output = context.output_dir();
        std::fs::create_dir_all(output).unwrap();
        std::fs::write(output.join("protocol.key"), format!("{}\n", "07".repeat(32))).unwrap();
        write_json(&output.join("controller.json"), &json!({
            "schema":"ao-reason-config/1","mode":"real","role":"controller","deployment":"direct-project-fixture",
            "state_dir":output.join("controller-state"),"nodes":[{"id":"worker-b","address":"192.0.2.10:22","key_file":output.join("protocol.key"),"slots":1,"templates":["sum-v1","control-v1","project-work-v1"],"admitted":true,"healthy":false,"draining":false}]
        }));
        write_json(&output.join("gateway.json"), &json!({
            "schema":"native-ssh-gateway/1","deployment":"direct-project-fixture","worker_id":"worker-b","host":"192.0.2.10","port":22,"user":"operator","known_hosts":self.bundle.join("known_hosts"),"identity_file":output.join("ordinary-user-rsa"),"controller_config":output.join("controller.json"),"key_file":output.join("protocol.key"),"state_dir":output.join("gateway-state"),"remote_worker":"/home/operator/cross-runtime-direct-project/aw-cli","remote_config":"/home/operator/cross-runtime-direct-project/worker.json","artifact_sha256":sha(b"worker"),"artifact_bytes":6,"timeout_secs":300,"launch_mode":"delegated-user-service","expected_worker_timeout_secs":270,"remote_library_path":"/home/operator/cross-runtime-direct-project/lib","remote_library_sha256":sha(b"loader"),"remote_library_bytes":6
        }));
        write_json(&output.join("worker.json"), &json!({"schema":"native-worker-config/1","deployment":"direct-project-fixture","worker_id":"worker-b","key_file":"protocol.key","cgroup_root":"/sys/fs/cgroup/user.slice/user-1007.slice/user@1007.service","cgroup_root_mode":"current-unit","artifact_sha256":sha(b"worker"),"timeout_secs":270}));
        write_json(&output.join("model-endpoint.json"), &json!({"schema":"model-endpoint/1","base_url":"https://model.example","model":"fixture-model","certificate_pem":"-----BEGIN CERTIFICATE-----\nsynthetic\n-----END CERTIFICATE-----\n","timeout_secs":240}));
        let value = json!({"schema":"worker-provision/1","status":"PASS_WORKER_PROVISION","preflight":true,"gateway_path":output.join("gateway.json"),"host":request.host,"worker_id":"worker-b","artifact_sha256":sha(b"worker"),"artifact_bytes":6,"attempt_id":context.attempt_id()});
        let raw = serde_json::to_vec(&value).unwrap();
        let key = b"synthetic-private-key";
        let mut frame = b"AW-WORKER-PROVISION-RESULT/1\n".to_vec();
        frame.extend_from_slice(&(raw.len() as u32).to_be_bytes()); frame.extend_from_slice(&raw);
        frame.extend_from_slice(&(key.len() as u32).to_be_bytes()); frame.extend_from_slice(key);
        agentranch::core::provision::decode_result_frame_in_dir_with_attempt(&frame, &request.root_password, &request.model_token, context).map_err(|_| agentranch::core::provision::ProvisionFailure::new("fixture_invalid"))
    }
}

fn bundle(root: &Path) -> PathBuf {
    let bundle = root.join("bundle"); std::fs::create_dir(&bundle).unwrap();
    for (name, content) in [("aw-cli", b"worker".as_slice()), ("libsqlite3.so.0", b"loader".as_slice()), ("known_hosts", b"fixture-host-key\n".as_slice()), ("model.pem", b"-----BEGIN CERTIFICATE-----\nsynthetic\n-----END CERTIFICATE-----\n".as_slice())] { std::fs::write(bundle.join(name), content).unwrap(); }
    write_json(&bundle.join("assets.json"), &json!({"schema":"worker-provision-assets/1","version":"0.3.0","worker":{"path":"aw-cli","bytes":6,"sha256":sha(b"worker")},"loader":{"path":"libsqlite3.so.0","bytes":6,"sha256":sha(b"loader")}}));
    write_json(&bundle.join("provisioning-prerequisites.json"), &json!({"schema":"worker-provision-prerequisites/1","asset_manifest":"assets.json","asset_version":"0.3.0","known_hosts":"known_hosts","tls_certificate":"model.pem"}));
    bundle
}

struct Port;
impl OwnedProcessPort for Port {
    fn owner_identity(&self) -> Result<agentranch::core::worker_owner::OwnerProcessIdentity> { Ok(agentranch::core::worker_owner::OwnerProcessIdentity { pid: 100, start_ticks: 1000 }) }
    fn launch(&mut self, _: &OwnerLaunchPlan) -> Result<LaunchFacts> { Ok(LaunchFacts { child_pid: 200, child_start_ticks: 2000, pgid: 200 }) }
    fn wait_terminal(&mut self) -> Result<TerminalFacts> { let stream = StreamReadOutcome { prefix: vec![], total_read_bytes: 0, eof: true, error: None }; Ok(TerminalFacts { wait_completed: true, wait_method: "child_wait".into(), raw_wait_status: 0, stdout: stream.clone(), stderr: stream, cleanup: CleanupFacts { proc_scan_complete: true, process_group_scan_complete: true, cgroup_scan_complete: true, process_group_members: 0, cgroup_members: 0, error: None } }) }
}

#[derive(Clone, Copy)]
enum FixtureOutcome { Done, Eof }

struct TerminalTransport { captured: Arc<Mutex<Option<Value>>>, calls: Arc<AtomicUsize>, outcome: FixtureOutcome }
impl WorkerTransport for TerminalTransport {
    fn exchange(&mut self, request: &[u8], _: u64) -> Result<TransportOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let start: Envelope = serde_json::from_slice(request.strip_suffix(b"\n").unwrap()).unwrap();
        let work = start.payload["project_work"].clone();
        *self.captured.lock().unwrap() = Some(work.clone());
        if matches!(self.outcome, FixtureOutcome::Eof) {
            return Ok(TransportOutput { stdout: vec![], stderr: vec![], exit_code: Some(0), failure: None });
        }
        let identity = start.execution_identity().unwrap();
        let session = format!("native-{}", start.message_id);
        let mut expected_identity = identity.clone(); expected_identity.session = Some(session.clone());
        let plan = OwnerLaunchPlan { execution: expected_identity.clone(), node_id: "worker-b".into(), instance_id: "123e4567-e89b-42d3-a456-426614174000".into(), boot_id: "123e4567-e89b-12d3-a456-426614174001".into(), request_bytes: canonical(&start.payload).unwrap() };
        let production = produce_owner_evidence(&mut Port, &plan).unwrap();
        let artifact = b"fixture artifact";
        let report = json!({"project_result":{"schema":"project-result/1","project_id":work["project_id"],"action_id":work["action_id"],"stage":"develop","document_version":work["document"]["version"],"document_sha256":work["document"]["sha256"]},"artifact":{"schema":"project-artifact-package/1","project_id":work["project_id"],"action_id":work["action_id"],"document_version":work["document"]["version"],"document_sha256":work["document"]["sha256"],"files":[{"path":"index.html","size":artifact.len(),"sha256":sha(artifact),"content_hex":hex::encode(artifact)}]}});
        let verification = OwnerVerification { passed: true, state: "verified".into(), details: json!({"meaning":"LOCAL synthetic fixture only; not real SSH/model"}) };
        let reference = OwnerEvidenceReference { schema: "owner-evidence-reference/1".into(), binding_sha256: owner_binding_sha256(&production.binding).unwrap(), bundle_sha256: owner_bundle_sha256(&production.bundle).unwrap(), record_sha256: production.bundle.record_sha256.clone(), report_sha256: owner_report_sha256(&expected_identity, &report, &verification).unwrap() };
        let launch = signed_event(&start, json!({"schema":"owner-evidence-launch/1","phase":"launch","session":session,"binding":production.binding}));
        let proof = ExitProof { run_id: start.run_id.clone(), attempt: start.attempt, epoch: start.epoch.clone(), session: Some(session.clone()), empty: true, observer_alive: true, not_started: false, owner_evidence: Some(reference.clone()) };
        let terminal = signed_event(&start, json!({"schema":"owner-evidence-report/1","phase":"terminal","session":session,"report":report,"verification":verification,"owner_evidence":{"reference":reference,"bundle_json":String::from_utf8(production.bundle_bytes().to_vec()).unwrap()},"exit_proof":proof}));
        let mut stdout = serde_json::to_vec(&launch).unwrap(); stdout.push(b'\n'); stdout.extend(serde_json::to_vec(&terminal).unwrap()); stdout.push(b'\n');
        Ok(TransportOutput { stdout, stderr: vec![], exit_code: Some(0), failure: None })
    }
}

fn signed_event(start: &Envelope, payload: Value) -> Envelope {
    let mut event = Envelope { schema: PROTOCOL.into(), message_id: uuid::Uuid::new_v4().to_string(), deployment: start.deployment.clone(), from: "worker-b".into(), to: "A".into(), run_id: start.run_id.clone(), attempt: start.attempt, epoch: start.epoch.clone(), kind: "report".into(), sent_at: start.expires_at, expires_at: start.expires_at.saturating_add(300), payload, mac: String::new() };
    event.sign(&[7; 32]).unwrap(); event
}

// Only the external transport is synthetic; Manager, Store, Service and UI stay real.
struct StreamingFactory { release: Arc<AtomicBool>, finish_release: Option<Arc<AtomicBool>>, fail: bool }
struct StreamingTransport { release: Arc<AtomicBool>, finish_release: Option<Arc<AtomicBool>>, fail: bool }
impl WorkerTransport for StreamingTransport {
    fn exchange(&mut self, _: &[u8], _: u64) -> Result<TransportOutput> { anyhow::bail!("streaming path required") }
    fn exchange_stream(&mut self, request: &[u8], timeout: u64, emit: &mut dyn FnMut(agentranch::core::live_output::LiveOutput)) -> Result<TransportOutput> {
        use agentranch::core::live_output::LiveOutput;
        emit(LiveOutput::Connected);
        emit(LiveOutput::Delta("首段 <html>".into()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.release.load(Ordering::Acquire) {
            anyhow::ensure!(Instant::now() < deadline, "fixture release timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
        if self.fail { anyhow::bail!("synthetic stream disconnect"); }
        emit(LiveOutput::Delta("第二段</html>".into()));
        emit(LiveOutput::ModelFinished);
        if let Some(release) = &self.finish_release {
            while !release.load(Ordering::Acquire) {
                anyhow::ensure!(Instant::now() < deadline, "fixture final release timed out");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        TerminalTransport { captured: Arc::new(Mutex::new(None)), calls: Arc::new(AtomicUsize::new(0)), outcome: FixtureOutcome::Done }.exchange(request, timeout)
    }
}
impl GatewayTransportFactory for StreamingFactory {
    fn sum_transport(&self, _: SshGatewayConfig, _: agentranch::core::session_credentials::CredentialLease, _: Arc<AtomicBool>) -> Result<Box<dyn WorkerTransport>> { anyhow::bail!("sum transport not used") }
    fn project_transport(&self, _: SshGatewayConfig, _: agentranch::core::session_credentials::CredentialLease, _: Arc<AtomicBool>) -> Result<Box<dyn WorkerTransport>> {
        Ok(Box::new(StreamingTransport { release: self.release.clone(), finish_release: self.finish_release.clone(), fail: self.fail }))
    }
}
fn streaming_service(root: &Path, release: Arc<AtomicBool>, fail: bool) -> (ServiceHandle, String) {
    streaming_service_with_final_barrier(root, release, None, fail)
}
fn streaming_service_with_final_barrier(root: &Path, release: Arc<AtomicBool>, finish_release: Option<Arc<AtomicBool>>, fail: bool) -> (ServiceHandle, String) {
    let state = root.join("ui");
    let mut projects = Projects::open(&state).unwrap();
    let id = create_project(&mut projects);
    import_and_confirm(&mut projects, &id, "# Design\nSynthetic local stream only.\n");
    drop(projects);
    let mut service = ServiceHandle::spawn_with_test_work_registration_hook(
        ServiceOptions { mode: Mode::Real, state_dir: state, worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}), Box::new(FixtureProvisioner { bundle: bundle(root) }), Arc::new(StreamingFactory { release, finish_release, fail }), Arc::new(|| {}),
    ).unwrap();
    let activate = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(wait_for(&mut service, activate).result.is_ok());
    // Provision receipt precedes loop-tail project projection; wait for the
    // actual view instead of assuming the receipt publishes project cards.
    let deadline=Instant::now()+Duration::from_secs(3);
    while !service.snapshot().projects.iter().any(|project| project.id==id) {
        assert!(Instant::now()<deadline,"fixture project projection not published");
        std::thread::yield_now();
    }
    (service, id)
}
#[test]
fn service_live_text_arrives_while_transport_is_blocked_without_final_result_or_admission() {
    use agentranch::core::live_output::LiveOutput;
    for fail in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let (mut service, id) = streaming_service(root.path(), release.clone(), fail);
        let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut events = Vec::new();
        while !events.iter().any(|event: &agentranch::core::service::LiveOutputEvent| matches!(event.output, LiveOutput::Delta(_))) {
            events.extend(service.drain_live_output());
            assert!(Instant::now() < deadline, "first delta must precede transport return");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(events.iter().all(|event| event.request_id == request && event.project_id == id));
        assert!(events.iter().any(|event| event.output == LiveOutput::Delta("首段 <html>".into())));
        assert!(service.drain_results().is_empty(), "no final receipt before release");
        assert!(!service.snapshot().runs.iter().any(|view| view.run.status == "done"));
        release.store(true, Ordering::Release);
        let result = wait_for(&mut service, request).result.unwrap();
        let remaining = service.drain_live_output();
        if fail {
            assert_eq!(result["status"], "unknown");
            assert_eq!(result["slot_reserved"], true);
            assert!(remaining.iter().any(|event| matches!(event.output, LiveOutput::Error(_))));
        } else {
            assert_eq!(result["status"], "done");
            assert_eq!(result["slot_reserved"], false);
            assert!(remaining.iter().any(|event| event.output == LiveOutput::ModelFinished));
            assert_eq!(std::fs::read(Path::new(result["artifact_dir"].as_str().unwrap()).join("index.html")).unwrap(), b"fixture artifact");
        }
        service.stop();
    }
}

#[test]
fn collected_receipt_includes_all_preceding_live_text_in_one_ui_poll() {
    let root=tempfile::tempdir().unwrap();
    let (mut service,id)=streaming_service(root.path(),Arc::new(AtomicBool::new(true)),false);
    let request=service.try_submit(UiCommand::ProjectWorkSubmit {project_id:id}).unwrap();
    let deadline=Instant::now()+Duration::from_secs(3);
    let mut body=String::new();
    loop {
        let (events,results)=service.drain_updates();
        for event in events {if let agentranch::core::live_output::LiveOutput::Delta(text)=event.output {body.push_str(&text);}}
        if results.iter().any(|r|r.request_id==request) {break;}
        assert!(Instant::now()<deadline); std::thread::yield_now();
    }
    assert_eq!(body,"首段 <html>第二段</html>","finalization must not discard text emitted between the live and receipt channel drains");
    service.stop();
}
#[cfg(feature = "gui")]
fn paint_text(app: &mut desktop::Workbench, ctx: &eframe::egui::Context) -> String {
    use eframe::egui;
    let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200., 1800.))), ..Default::default() }, |ui| {
        egui::CentralPanel::default().show(ui, |ui| app.show(ui));
    });
    output.textures_delta.clear();
    fn collect(shape: &egui::epaint::Shape, text: &mut String) {
        match shape {
            egui::epaint::Shape::Text(shape) => {text.push_str(&shape.galley.job.text);text.push('\n');},
            egui::epaint::Shape::Vec(shapes) => for shape in shapes {collect(shape,text)},
            _ => {},
        }
    }
    let mut text = String::new();
    for shape in output.shapes {collect(&shape.shape, &mut text);}
    text
}
#[cfg(feature = "gui")]
#[test]
fn workbench_paints_actual_text_before_final_receipt_and_keeps_it_after_completion_or_disconnect() {
    for fail in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let (service, id) = streaming_service(root.path(), release.clone(), fail);
        let ctx = eframe::egui::Context::default();
        let mut app = desktop::Workbench::from_service(service, &ctx, root.path().join("ui").display().to_string());
        app.send(format!("work-submit:{id}"), UiCommand::ProjectWorkSubmit {project_id:id});
        let deadline = Instant::now() + Duration::from_secs(3);
        let first = loop {
            let text = paint_text(&mut app, &ctx);
            if text.contains("首段 <html>") {break text;}
            assert!(Instant::now() < deadline, "first output not painted while transport blocked: {text}");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(app.receipt_count(), 0);
        assert_eq!(app.pending_count(), 1);
        assert!(first.contains("模型输出"));
        assert!(!first.contains("已收到并准入远端结果"));
        release.store(true, Ordering::Release);
        while app.pending_count() > 0 {
            app.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let after = paint_text(&mut app, &ctx);
        assert!(after.contains("首段 <html>"), "final receipt erased partial text: {after}");
        if fail {assert!(after.contains("保留已有输出"));}
        else {
            assert!(after.contains("第二段</html>"));
            assert!(after.contains("输出通道：已结束"));
            assert!(after.contains("最终任务结果：完成"));
            assert!(!after.contains("任务结果仍以终态证据与产物准入为准"));
        }
    }
}

// Catches a command/materialization error being presented as a failed remote
// task after the real Store has already admitted terminal done evidence.
#[cfg(feature = "gui")]
#[test]
fn live_result_materialization_error_does_not_replace_published_task_completion() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("ui");
    let (service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let ledger = agentranch::core::provision_ledger::ProvisionLedger::load(&state).unwrap().unwrap();
    let db = rusqlite::Connection::open_with_flags(
        ledger.output_dir.unwrap().join("controller-state/controller.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ).unwrap();
    // An exclusive test-owned regular file blocks create_dir_all below it.
    // No real artifacts, permissions, network or credentials are involved.
    std::fs::write(state.join("artifacts"), b"local materialization fault fixture").unwrap();
    let ctx = eframe::egui::Context::default();
    let mut app = desktop::Workbench::from_service(service, &ctx, state.display().to_string());
    app.send(format!("work-submit:{id}"), UiCommand::ProjectWorkSubmit { project_id: id.clone() });
    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        app.poll();
        let snapshot = app.snapshot();
        if app.pending_count() == 0 && snapshot.runs.iter().any(|view| view.run.detail["project_work"]["project_id"] == id && view.run.status == "done") { break snapshot; }
        assert!(Instant::now() < deadline, "fault fixture did not publish terminal Store evidence");
        std::thread::sleep(Duration::from_millis(2));
    };
    let run = snapshot.runs.iter().find(|view| view.run.detail["project_work"]["project_id"] == id).unwrap();
    assert_eq!(run.run.status, "done");
    assert_eq!(run.run.detail["owner_admission"], "admitted");
    assert!(run.run.detail["artifact_dir"].is_null());
    let (done, reserved): (i64, i64) = (
        db.query_row("SELECT count(*) FROM runs WHERE status='done'", [], |row| row.get(0)).unwrap(),
        db.query_row("SELECT count(*) FROM assignments WHERE released=0", [], |row| row.get(0)).unwrap(),
    );
    assert_eq!((done, reserved), (1, 0), "display repair must not change Store completion/release");
    assert_eq!(app.receipt_count(), 1);
    let text = paint_text(&mut app, &ctx);
    assert!(text.contains("project_work_blocked"), "actual Service command error was not retained: {text}");
    assert!(text.contains("首段 <html>第二段</html>"), "operation error lost received body: {text}");
    assert!(text.contains("任务：已完成"));
    assert!(text.contains("产物目录：尚未物化"));
    assert!(text.contains("已返回完成状态但证据不完整"));
    let button = ctx.read_response(eframe::egui::Id::unique(egui_id(&format!("project:{id}:work-submit")).value().wrapping_add(1))).unwrap();
    assert!(!button.enabled(), "a local materialization failure must not enable repeat submission");
    println!("local fault facts: Store done={done}, reserved={reserved}, artifact_dir absent, Service error retained, body retained, submit disabled");
    assert!(!text.lines().any(|line| line == "最终任务结果：失败"), "SSE contradicts admitted Store completion after local operation error: {text}");
    assert_eq!(text.lines().filter(|line| *line == "最终任务结果：完成").count(), 2, "SSE and result summary must agree: {text}");
}

#[cfg(feature = "gui")]
#[test]
fn model_finished_keeps_submit_disabled_and_store_assignment_reserved_until_final_evidence() {
    let root = tempfile::tempdir().unwrap();
    let release = Arc::new(AtomicBool::new(true));
    let final_release = Arc::new(AtomicBool::new(false));
    let (service, id) = streaming_service_with_final_barrier(root.path(), release, Some(final_release.clone()), false);
    let ledger = agentranch::core::provision_ledger::ProvisionLedger::load(&root.path().join("ui")).unwrap().unwrap();
    let db = rusqlite::Connection::open_with_flags(ledger.output_dir.unwrap().join("controller-state/controller.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let ctx = eframe::egui::Context::default();
    let mut app = desktop::Workbench::from_service(service, &ctx, root.path().join("ui").display().to_string());
    app.send(format!("work-submit:{id}"), UiCommand::ProjectWorkSubmit {project_id:id.clone()});
    let deadline = Instant::now() + Duration::from_secs(3);
    let text = loop {
        let text = paint_text(&mut app, &ctx);
        if text.contains("模型输出已结束") {break text;}
        assert!(Instant::now() < deadline, "model completion not painted: {text}");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(text.contains("首段 <html>第二段</html>"));
    assert!(!text.contains("任务终态已返回"));
    assert!(!text.contains("已收到并准入远端结果"));
    assert_eq!(app.receipt_count(), 0);
    assert_eq!(app.pending_count(), 1);
    let scope = egui_id(&format!("project:{id}:work-submit"));
    let button = ctx.read_response(eframe::egui::Id::unique(scope.value().wrapping_add(1))).expect("real submit button");
    assert!(!button.enabled(), "model completion must not enable resubmission");
    let reserved: i64 = db.query_row("SELECT count(*) FROM assignments WHERE released=0", [], |row| row.get(0)).unwrap();
    let done: i64 = db.query_row("SELECT count(*) FROM runs WHERE status='done'", [], |row| row.get(0)).unwrap();
    assert_eq!(reserved, 1);
    assert_eq!(done, 0);
    app.send(format!("work-submit:{id}"), UiCommand::ProjectWorkSubmit {project_id:id});
    assert_eq!(app.pending_count(), 1, "duplicate send remains blocked");
    final_release.store(true, Ordering::Release);
    while app.pending_count() > 0 {
        app.poll();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(paint_text(&mut app, &ctx).contains("任务终态已返回"));
    let reserved: i64 = db.query_row("SELECT count(*) FROM assignments WHERE released=0", [], |row| row.get(0)).unwrap();
    assert_eq!(reserved, 0);
}
#[cfg(feature = "gui")]
#[test]
fn connected_output_is_distinct_from_waiting_for_model_response_without_inventing_text() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let snapshot = service.snapshot();
    let ctx = eframe::egui::Context::default();
    let mut projects = desktop::projects::Projects::default();
    projects.begin_live(71, id.clone());
    let paint = |projects: &mut desktop::projects::Projects| {
        let mut output = ctx.run_ui(eframe::egui::RawInput {screen_rect:Some(eframe::egui::Rect::from_min_size(eframe::egui::Pos2::ZERO, eframe::egui::vec2(1200.,1800.))),..Default::default()}, |ui| {
            projects.show(ui, &snapshot, &desktop::pending::Pending::default(), &[], false);
        });
        output.textures_delta.clear();
        output.shapes.iter().filter_map(|shape| match &shape.shape {eframe::egui::epaint::Shape::Text(text) => Some(text.galley.job.text.clone()), _ => None}).collect::<Vec<_>>().join("\n")
    };
    let waiting = paint(&mut projects);
    assert!(!waiting.contains("模型已连接"));
    projects.apply_live(agentranch::core::service::LiveOutputEvent {request_id:71, project_id:id, output:agentranch::core::live_output::LiveOutput::Connected, received_at:Instant::now(), received_bytes:0, truncated:false});
    let connected = paint(&mut projects);
    assert!(connected.contains("模型已连接，等待输出"), "HTTP connection feedback missing: {connected}");
    assert!(!connected.contains("正在接收模型实际输出"));
    assert!(!connected.contains("任务终态已返回"));
}
#[cfg(feature = "gui")]
#[test]
fn active_work_in_one_project_disables_submission_in_another_project() {
    let root = tempfile::tempdir().unwrap();
    let release = Arc::new(AtomicBool::new(false));
    let (mut service, first_id) = streaming_service(root.path(), release.clone(), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: first_id.clone() }).unwrap();
    let snapshot = service.snapshot();
    let mut second = snapshot.projects.iter().find(|project| project.id == first_id).cloned().expect("first project snapshot");
    second.id = "second-project".into();
    second.name = "第二项目".into();
    let mut snapshot = (*snapshot).clone();
    snapshot.projects.push(second);
    let mut pending = desktop::pending::Pending::default();
    pending.accept(format!("work-submit:{first_id}"), request);
    let ctx = eframe::egui::Context::default();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some("second-project".into());
    let mut output = ctx.run_ui(eframe::egui::RawInput {screen_rect:Some(eframe::egui::Rect::from_min_size(eframe::egui::Pos2::ZERO, eframe::egui::vec2(1200.,1800.))),..Default::default()}, |ui| {
        projects.show(ui, &snapshot, &pending, &[], false);
    });
    output.textures_delta.clear();
    let button = ctx.read_response(eframe::egui::Id::unique(egui_id("project:second-project:work-submit").value().wrapping_add(1))).expect("second project submit button");
    assert!(!button.enabled(), "one active project must block another submission");
    release.store(true, Ordering::Release);
    let _ = wait_for(&mut service, request);
    service.stop();
}
#[cfg(feature = "gui")]
#[test]
fn activity_is_visible_before_document_controls_in_ordinary_project_page() {
    let root=tempfile::tempdir().unwrap();
    let (mut service,id)=streaming_service(root.path(),Arc::new(AtomicBool::new(false)),false);
    let mut snapshot=(*service.snapshot()).clone();
    snapshot.projects[0].document.as_mut().unwrap().text="Long confirmed design\n".repeat(200);
    let ctx=eframe::egui::Context::default();
    let mut projects=desktop::projects::Projects::default(); projects.begin_live(71,id);
    let mut out=ctx.run_ui(eframe::egui::RawInput {screen_rect:Some(eframe::egui::Rect::from_min_size(eframe::egui::Pos2::ZERO,eframe::egui::vec2(1200.,900.))),..Default::default()},|ui| {projects.show(ui,&snapshot,&desktop::pending::Pending::default(),&[],false);});
    out.textures_delta.clear();
    let at=|label:&str|out.shapes.iter().find_map(|s|match &s.shape {eframe::egui::epaint::Shape::Text(t) if t.galley.job.text==label=>Some(t.pos.y),_=>None}).expect(label);
    assert!(at("模型实时输出（SSE）")<at("定稿设计文档"),"activity cannot hide beneath import/preview controls");
    assert!(at("模型实时输出（SSE）")<900.);
    service.stop();
}
#[cfg(feature = "gui")]
#[test]
fn project_switch_keeps_running_badge_and_returns_to_actual_body() {
    let root=tempfile::tempdir().unwrap();
    let (mut service,id)=streaming_service(root.path(),Arc::new(AtomicBool::new(false)),false);
    let mut snapshot=(*service.snapshot()).clone();
    let mut second=snapshot.projects[0].clone(); second.id="second".into(); second.name="另一个项目".into(); snapshot.projects.push(second);
    let mut projects=desktop::projects::Projects::default(); projects.selected=Some("second".into());
    projects.begin_live(71,id.clone());
    projects.selected=Some("second".into()); // user switches after the accepted submit selects its project
    let ctx=eframe::egui::Context::default();
    let paint=|projects:&mut desktop::projects::Projects| {
        let mut out=ctx.run_ui(eframe::egui::RawInput {screen_rect:Some(eframe::egui::Rect::from_min_size(eframe::egui::Pos2::ZERO,eframe::egui::vec2(1200.,1800.))),..Default::default()},|ui| {projects.show(ui,&snapshot,&desktop::pending::Pending::default(),&[],false);});
        out.textures_delta.clear(); out.shapes.iter().filter_map(|s|match &s.shape {eframe::egui::epaint::Shape::Text(t)=>Some(t.galley.job.text.as_str()),_=>None}).collect::<Vec<_>>().join("\n")
    };
    let waiting=paint(&mut projects);
    assert!(waiting.contains("执行：已提交，等待模型响应"),"list must expose accepted request before first delta: {waiting}");
    assert!(waiting.contains("已有项目任务执行中"),"another project must explain disabled submit honestly");
    projects.apply_live(agentranch::core::service::LiveOutputEvent {request_id:71,project_id:id.clone(),output:agentranch::core::live_output::LiveOutput::Delta("actual project body".into()),received_at:Instant::now(),received_bytes:19,truncated:false});
    let other=paint(&mut projects);
    assert!(other.contains("执行：正在接收模型实际输出"));
    assert!(!other.contains("actual project body"),"body must stay in its project");
    projects.selected=Some(id.clone());
    assert!(paint(&mut projects).contains("actual project body"));
    projects.finish_live(71,&Ok(json!({"status":"unexpected"})));
    let _=paint(&mut projects);
    let unknown_button=ctx.read_response(eframe::egui::Id::unique(egui_id(&format!("project:{id}:work-submit")).value().wrapping_add(1))).expect("real submit button after unrecognized status");
    assert!(!unknown_button.enabled(),"unrecognized final status cannot enable duplicate submission");
    projects.finish_live(71,&Ok(json!({"status":"unknown"})));
    let unknown=paint(&mut projects);
    assert!(unknown.contains("执行：最终任务结果：结果未知"));
    assert!(!unknown.contains("现在做：完成 Worker 准备后再提交项目"));
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn activity_guidance_survives_project_switch() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, first_id) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let mut snapshot = (*service.snapshot()).clone();
    let mut second = snapshot.projects[0].clone();
    second.id = "second-guidance-project".into();
    second.name = "第二项目".into();
    snapshot.projects.push(second);

    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some("second-guidance-project".into());
    projects.begin_live(71, first_id.clone());
    projects.selected = Some("second-guidance-project".into()); // explicit navigation after submission
    projects.apply_live(agentranch::core::service::LiveOutputEvent {
        request_id: 71,
        project_id: first_id.clone(),
        output: agentranch::core::live_output::LiveOutput::Connected,
        received_at: Instant::now(),
        received_bytes: 0,
        truncated: false,
    });
    projects.apply_live(agentranch::core::service::LiveOutputEvent {
        request_id: 71,
        project_id: first_id.clone(),
        output: agentranch::core::live_output::LiveOutput::Delta("first project body".into()),
        received_at: Instant::now(),
        received_bytes: 18,
        truncated: false,
    });
    projects.apply_live(agentranch::core::service::LiveOutputEvent {
        request_id: 71,
        project_id: first_id.clone(),
        output: agentranch::core::live_output::LiveOutput::Error("transport interrupted".into()),
        received_at: Instant::now(),
        received_bytes: 18,
        truncated: false,
    });

    let ctx = eframe::egui::Context::default();
    let paint = |projects: &mut desktop::projects::Projects| {
        let mut out = ctx.run_ui(
            eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(eframe::egui::Pos2::ZERO, eframe::egui::vec2(1200., 1800.))),
                ..Default::default()
            },
            |ui| {
                projects.show(ui, &snapshot, &desktop::pending::Pending::default(), &[], false);
            },
        );
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                eframe::egui::epaint::Shape::Text(text) => Some(text.galley.job.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let second_view = paint(&mut projects);
    assert!(second_view.contains("执行：输出已中断；保留已有输出。等待最终任务结果。"));
    assert!(!second_view.contains("first project body"));

    projects.selected = Some(first_id.clone());
    let first_view = paint(&mut projects);
    assert!(first_view.contains("输出通道：中断"), "missing channel guidance: {first_view}");
    assert!(!first_view.contains("输出通道：输出通道"));
    assert!(first_view.contains("最终任务结果：尚未返回"), "missing pending result guidance: {first_view}");
    assert!(first_view.contains("等待最终任务回执；不要手动释放或自动重跑"), "missing safe next step: {first_view}");
    assert!(first_view.contains("first project body"));
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn final_done_guidance_does_not_change_barrier_authority() {
    let root = tempfile::tempdir().unwrap();
    let release = Arc::new(AtomicBool::new(true));
    let final_release = Arc::new(AtomicBool::new(false));
    let (service, id) = streaming_service_with_final_barrier(root.path(), release, Some(final_release.clone()), false);
    let ledger = agentranch::core::provision_ledger::ProvisionLedger::load(&root.path().join("ui")).unwrap().unwrap();
    let db = rusqlite::Connection::open_with_flags(
        ledger.output_dir.unwrap().join("controller-state/controller.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ).unwrap();
    let ctx = eframe::egui::Context::default();
    let mut app = desktop::Workbench::from_service(service, &ctx, root.path().join("ui").display().to_string());
    app.send(format!("work-submit:{id}"), UiCommand::ProjectWorkSubmit { project_id: id.clone() });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let text = paint_text(&mut app, &ctx);
        if text.contains("输出通道：模型输出已结束") { break; }
        assert!(Instant::now() < deadline, "model completion not painted: {text}");
        std::thread::sleep(Duration::from_millis(2));
    }
    let before_final = paint_text(&mut app, &ctx);
    assert!(before_final.contains("最终任务结果：尚未返回"));
    assert!(before_final.contains("等待最终证据与产物准入"));
    assert_eq!(app.receipt_count(), 0);
    assert_eq!(app.pending_count(), 1);
    let reserved: i64 = db.query_row("SELECT count(*) FROM assignments WHERE released=0", [], |row| row.get(0)).unwrap();
    let done: i64 = db.query_row("SELECT count(*) FROM runs WHERE status='done'", [], |row| row.get(0)).unwrap();
    assert_eq!(reserved, 1);
    assert_eq!(done, 0);

    final_release.store(true, Ordering::Release);
    while app.pending_count() > 0 {
        app.poll();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let after_final = paint_text(&mut app, &ctx);
    assert!(after_final.contains("最终任务结果：完成"));
    assert!(after_final.contains("核对产物目录和操作回执"));
    let reserved: i64 = db.query_row("SELECT count(*) FROM assignments WHERE released=0", [], |row| row.get(0)).unwrap();
    let done: i64 = db.query_row("SELECT count(*) FROM runs WHERE status='done'", [], |row| row.get(0)).unwrap();
    assert_eq!(reserved, 0);
    assert_eq!(done, 1);
}

#[cfg(feature = "gui")]
#[test]
fn error_then_unknown_guidance_is_not_failed() {
    let root = tempfile::tempdir().unwrap();
    let release = Arc::new(AtomicBool::new(true));
    let (service, id) = streaming_service(root.path(), release, true);
    let ctx = eframe::egui::Context::default();
    let mut app = desktop::Workbench::from_service(service, &ctx, root.path().join("ui").display().to_string());
    app.send(format!("work-submit:{id}"), UiCommand::ProjectWorkSubmit { project_id: id });
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.pending_count() > 0 {
        app.poll();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let text = paint_text(&mut app, &ctx);
    assert!(text.contains("输出通道：已结束"), "unknown receipt must close the channel: {text}");
    assert!(text.contains("最终任务结果：结果未知"), "unknown receipt must remain unknown: {text}");
    assert!(text.contains("名额保留；不要手动释放或自动重跑"), "unknown receipt must not invite release or rerun: {text}");
    assert!(!text.contains("最终任务结果：失败"));
}

#[cfg(feature = "gui")]
fn paint_projects_text(projects: &mut desktop::projects::Projects, snapshot: &agentranch::core::service::AppSnapshot) -> String {
    paint_projects(projects, snapshot).0
}

#[cfg(feature = "gui")]
fn paint_projects(
    projects: &mut desktop::projects::Projects,
    snapshot: &agentranch::core::service::AppSnapshot,
) -> (String, bool) {
    let (text, enabled, _) = paint_projects_frame(projects, snapshot);
    (text, enabled)
}

#[cfg(feature = "gui")]
fn paint_project_list_item(
    projects: &mut desktop::projects::Projects,
    snapshot: &agentranch::core::service::AppSnapshot,
    project_id: &str,
) -> String {
    let project = snapshot.projects.iter().find(|project| project.id == project_id).expect("fixture project");
    let prefix = format!("{}\n", project.name);
    paint_projects_frame(projects, snapshot).2.into_iter()
        .find(|text| text.starts_with(&prefix))
        .expect("painted project list item")
}

#[cfg(feature = "gui")]
fn paint_projects_frame(
    projects: &mut desktop::projects::Projects,
    snapshot: &agentranch::core::service::AppSnapshot,
) -> (String, bool, Vec<String>) {
    let ctx = eframe::egui::Context::default();
    let mut output = ctx.run_ui(
        eframe::egui::RawInput {
            screen_rect: Some(eframe::egui::Rect::from_min_size(eframe::egui::Pos2::ZERO, eframe::egui::vec2(1200., 1800.))),
            ..Default::default()
        },
        |ui| {
            projects.show(ui, snapshot, &desktop::pending::Pending::default(), &[], false);
        },
    );
    output.textures_delta.clear();
    fn collect(shape: &eframe::egui::epaint::Shape, text: &mut Vec<String>) {
        match shape {
            eframe::egui::epaint::Shape::Text(shape) => text.push(shape.galley.job.text.clone()),
            eframe::egui::epaint::Shape::Vec(shapes) => for shape in shapes { collect(shape, text); }
            _ => {}
        }
    }
    let mut painted_text = Vec::new();
    for shape in output.shapes { collect(&shape.shape, &mut painted_text); }
    let project_id = projects.selected.as_deref().expect("selected project");
    let button_id = eframe::egui::Id::unique(egui_id(&format!("project:{project_id}:work-submit")).value().wrapping_add(1));
    let enabled = ctx.read_response(button_id).map(|response| response.enabled()).expect("project submit button");
    (painted_text.join("\n"), enabled, painted_text)
}

// Catches overriding the SSE with a different project's/template's terminal
// result, or accepting a nonterminal published value as a final result.
#[cfg(feature = "gui")]
#[test]
fn live_result_projection_uses_only_matching_published_terminal_status() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let run_id = result["run_id"].as_str().unwrap();
    let base = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    service.stop();
    for (status, project_match, template_match, expected) in [
        ("done", true, true, "完成"),
        ("failed", true, true, "失败"),
        ("cancelled", true, true, "已取消"),
        ("unknown", true, true, "结果未知"),
        ("running", true, true, "失败"),
        ("unexpected", true, true, "失败"),
        ("done", false, true, "失败"),
        ("done", true, false, "失败"),
    ] {
        let mut snapshot = base.clone();
        let run = snapshot.runs.iter_mut().find(|view| view.run.id == run_id).unwrap();
        run.run.status = status.into();
        if !project_match { run.run.detail["project_work"]["project_id"] = json!("other-project"); }
        if !template_match { run.run.template = "sum-v1".into(); }
        let mut projects = desktop::projects::Projects::default();
        projects.begin_live(71, id.clone());
        projects.apply_live(agentranch::core::service::LiveOutputEvent { request_id:71, project_id:id.clone(), output:agentranch::core::live_output::LiveOutput::Delta("preserved synthetic body".into()), received_at:Instant::now(), received_bytes:24, truncated:false });
        projects.finish_live(71, &Err(agentranch::core::service::ServiceError { code:"synthetic-operation-error".into(), message:"local fault".into(), next_step:"check receipt".into() }));
        let (text, enabled) = paint_projects(&mut projects, &snapshot);
        // Before any final assertion, identify the actual SSE card region.
        let sse = text.split_once("模型实时输出（SSE）").unwrap().1.split_once("定稿设计文档").unwrap().0;
        assert!(sse.contains(&format!("最终任务结果：{expected}\n")), "status={status}, project_match={project_match}, template_match={template_match}: {sse}");
        assert!(sse.contains("preserved synthetic body"));
        assert!(sse.contains("本次操作失败"));
        if project_match && template_match {
            assert!(!enabled, "published run must still block duplicate submit");
        }
        if status == "unknown" {
            assert!(sse.contains("名额保留；不要手动释放或自动重跑"));
            assert!(!sse.contains("最终任务结果：失败"));
        }
    }
}

// Catches accidental reuse of the advanced phase rail in ordinary mode and
// duplicate shared document content. Only the external fixture transport is fake;
// these assertions inspect the actual Projects renderer and registered controls.
#[cfg(feature = "gui")]
fn paint_guidance(
    snapshot: &agentranch::core::service::AppSnapshot,
    diagnostics: bool,
) -> (Vec<String>, eframe::egui::Context) {
    let ctx = eframe::egui::Context::default();
    let mut projects = desktop::projects::Projects::default();
    let mut output = ctx.run_ui(
        eframe::egui::RawInput {
            screen_rect: Some(eframe::egui::Rect::from_min_size(eframe::egui::Pos2::ZERO, eframe::egui::vec2(1200., 2400.))),
            ..Default::default()
        },
        |ui| { projects.show(ui, snapshot, &desktop::pending::Pending::default(), &[], diagnostics); },
    );
    output.textures_delta.clear();
    fn collect(shape: &eframe::egui::epaint::Shape, text: &mut Vec<String>) {
        match shape {
            eframe::egui::epaint::Shape::Text(shape) => text.push(shape.galley.job.text.clone()),
            eframe::egui::epaint::Shape::Vec(shapes) => for shape in shapes { collect(shape, text); },
            _ => {}
        }
    }
    let mut text = Vec::new();
    for shape in output.shapes { collect(&shape.shape, &mut text); }
    (text, ctx)
}

#[cfg(feature = "gui")]
#[test]
fn guidance_document_card_is_single_without_losing_confirmation_control() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let mut snapshot = (*service.snapshot()).clone();
    service.stop();
    snapshot.projects[0].phase = "document_review".into();
    snapshot.projects[0].document.as_mut().unwrap().confirmed = false;
    snapshot.projects[0].can_confirm = true;
    let (text, ctx) = paint_guidance(&snapshot, false);
    assert_eq!(text.iter().filter(|text| *text == "定稿设计文档").count(), 1, "duplicated card: {text:?}");
    assert_eq!(text.iter().filter(|text| text.starts_with("UTF-8 .md / .txt")).count(), 1);
    for (selector, enabled) in [(format!("project:{id}:import"), false), (format!("project:{id}:confirm:1"), true)] {
        let button = ctx.read_response(eframe::egui::Id::unique(egui_id(&selector).value().wrapping_add(1))).expect("real document action remains registered");
        assert_eq!(button.enabled(), enabled, "gate changed for {selector}");
    }
}

#[cfg(feature = "gui")]
#[test]
fn guidance_novice_list_uses_document_facts_without_promising_worker_readiness() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let base = (*service.snapshot()).clone();
    service.stop();
    for (document, expected, count) in [
        (None, "等待导入设计文档", "已完成 1/3 步"),
        (Some(false), "文档待确认", "已完成 1/3 步"),
        (Some(true), "文档已确认", "已完成 2/3 步"),
    ] {
        let mut snapshot = base.clone();
        snapshot.gateway = None;
        // Intentionally retain the confirmed phase: ordinary progress must use
        // the same actual document facts as ordinary detail, not legacy phase.
        match document {
            None => snapshot.projects[0].document = None,
            Some(confirmed) => snapshot.projects[0].document.as_mut().unwrap().confirmed = confirmed,
        }
        let mut projects = desktop::projects::Projects::default();
        projects.selected = Some(id.clone());
        let item = paint_project_list_item(&mut projects, &snapshot, &id);
        assert!(item.contains(count), "document={document:?}: {item}");
        assert!(item.contains(expected), "document={document:?}: {item}");
        for misleading in ["拆分任务", "手动开始开发", "/6 步", "可提交", "Worker 已连接"] {
            assert!(!item.contains(misleading), "list invents next action/readiness: {item}");
        }
        let (text, enabled) = paint_projects(&mut projects, &snapshot);
        assert!(!enabled, "disconnected document={document:?} must not enable submit");
        assert!(!text.contains("尚未导入文档，不能拆分或开始开发。"), "ordinary empty-state must not require advanced workflow");
    }
}

#[cfg(feature = "gui")]
#[test]
fn guidance_novice_third_step_requires_published_done_result() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let run_id = result["run_id"].as_str().unwrap();
    let base = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    service.stop();
    for (status, count) in [("done", "已完成 3/3 步"), ("failed", "已完成 2/3 步"), ("cancelled", "已完成 2/3 步"), ("unknown", "已完成 2/3 步"), ("running", "已完成 2/3 步")] {
        let mut snapshot = base.clone();
        snapshot.runs.iter_mut().find(|view| view.run.id == run_id).unwrap().run.status = status.into();
        let mut projects = desktop::projects::Projects::default();
        projects.selected = Some(id.clone());
        let item = paint_project_list_item(&mut projects, &snapshot, &id);
        assert!(item.contains(count), "status={status}: {item}");
        assert!(!item.contains("拆分任务"), "result must not invite legacy split: {item}");
    }
}

#[cfg(feature = "gui")]
#[test]
fn guidance_diagnostics_retains_advanced_split_and_six_step_rail() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, _) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let snapshot = (*service.snapshot()).clone();
    service.stop();
    let (text, _) = paint_guidance(&snapshot, true);
    let item = text.iter().find(|text| text.starts_with("direct project work\n")).expect("advanced list item");
    assert!(item.contains("可以拆分任务"));
    assert!(item.contains("已完成 2/6 步"));
    assert!(text.iter().any(|text| text == "现在做：点“按已确认上传版拆分任务”"));
}

#[cfg(feature = "gui")]
#[test]
fn project_list_prefers_terminal_run_result_without_local_activity() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let run_id = result["run_id"].as_str().unwrap();
    let base = (*wait_for_snapshot_run(&mut service, run_id)).clone();

    for (status, expected) in [
        ("done", "任务：已完成"),
        ("failed", "任务：失败"),
        ("cancelled", "任务：已取消"),
        ("unknown", "任务：结果未知，名额保留"),
    ] {
        for local_activity in [false, true] {
            let mut snapshot = base.clone();
            let run = snapshot.runs.iter_mut().find(|view| view.run.id == run_id).unwrap();
            run.run.status = status.into();
            let mut projects = desktop::projects::Projects::default();
            projects.selected = Some(id.clone());
            if local_activity { projects.begin_live(71, id.clone()); }
            let text = paint_project_list_item(&mut projects, &snapshot, &id);
            assert!(text.contains(expected), "missing {expected} for {status}, activity={local_activity}: {text}");
            assert!(!text.contains("执行："), "terminal result must override local activity: {text}");
        }
    }
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn project_list_prefers_local_activity_while_run_is_active() {
    use agentranch::core::live_output::LiveOutput;

    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let run_id = result["run_id"].as_str().unwrap();
    let mut snapshot = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    snapshot.runs.iter_mut().find(|view| view.run.id == run_id).unwrap().run.status = "running".into();
    service.stop();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id.clone());
    projects.begin_live(request, id.clone());
    projects.apply_live(agentranch::core::service::LiveOutputEvent {
        request_id: request,
        project_id: id.clone(),
        output: LiveOutput::Connected,
        received_at: Instant::now(),
        received_bytes: 0,
        truncated: false,
    });
    let text = paint_project_list_item(&mut projects, &snapshot, &id);
    assert!(text.contains("执行：模型已连接，等待输出……"), "local activity must win: {text}");
    assert!(!text.contains("任务：执行中"), "generic running label must not override activity: {text}");
    assert!(!text.contains("任务：已完成"));
    assert!(!text.contains("任务：失败"));
    assert!(!text.contains("任务：结果未知"));
}

#[cfg(feature = "gui")]
#[test]
fn project_list_shows_running_without_activity_without_claiming_liveness() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let run_id = result["run_id"].as_str().unwrap();
    let mut snapshot = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    snapshot.runs.iter_mut().find(|view| view.run.id == run_id).unwrap().run.status = "running".into();
    service.stop();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id.clone());
    let text = paint_project_list_item(&mut projects, &snapshot, &id);
    assert!(text.contains("任务：执行中"), "active run needs a stable list summary: {text}");
    assert!(!text.contains("执行："), "missing activity must not invent a channel status: {text}");
    assert!(!text.contains("Worker 在线"), "list must not claim Worker liveness: {text}");
    assert!(!text.contains("远端运行中"), "list must not claim remote liveness: {text}");
}

#[cfg(feature = "gui")]
#[test]
fn project_list_without_run_or_activity_has_no_fake_task_status() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let snapshot = (*service.snapshot()).clone();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id.clone());
    let text = paint_project_list_item(&mut projects, &snapshot, &id);
    assert!(!text.contains("任务："), "empty project must not invent a task status: {text}");
    assert!(!text.contains("执行："), "empty project must not invent an activity status: {text}");
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn project_list_keeps_results_and_activity_scoped_to_project_and_template() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let run_id = result["run_id"].as_str().unwrap();
    let mut snapshot = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    service.stop();
    let mut second = snapshot.projects.iter().find(|project| project.id == id).unwrap().clone();
    second.id = "second-list-project".into();
    second.name = "另一个列表项目".into();
    let second_id = second.id.clone();
    snapshot.projects.push(second);
    let mut unrelated = snapshot.runs.iter().find(|view| view.run.id == run_id).unwrap().clone();
    unrelated.run.id = "unrelated-list-template".into();
    unrelated.run.template = "sum-v1".into();
    unrelated.run.status = "failed".into();
    unrelated.run.detail["project_work"]["project_id"] = second_id.clone().into();
    snapshot.runs.insert(0, unrelated);

    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(second_id.clone());
    projects.begin_live(71, second_id.clone());
    let first = paint_project_list_item(&mut projects, &snapshot, &id);
    let second = paint_project_list_item(&mut projects, &snapshot, &second_id);
    assert!(first.contains("任务：已完成"), "first project's final result must remain visible: {first}");
    assert!(!first.contains("执行："), "second project's activity must not leak: {first}");
    assert!(second.contains("执行：已提交，等待模型响应……"), "activity without a matching work run must remain visible: {second}");
    assert!(!second.contains("任务："), "other project or template must not supply a task result: {second}");

    let mut without_activity = desktop::projects::Projects::default();
    without_activity.selected = Some(second_id.clone());
    let second = paint_project_list_item(&mut without_activity, &snapshot, &second_id);
    assert!(!second.contains("任务："), "unrelated run must not invent a task state: {second}");
    assert!(!second.contains("执行："), "missing activity must not invent channel status: {second}");
}

#[cfg(feature = "gui")]
#[test]
fn terminal_project_run_disables_duplicate_submission() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let run_id = result["run_id"].as_str().unwrap();
    let base = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    for status in ["done", "failed", "cancelled"] {
        let mut snapshot = base.clone();
        let run = snapshot.runs.iter_mut().find(|run| run.run.id == run_id).unwrap();
        run.run.status = status.into();
        let mut projects = desktop::projects::Projects::default();
        projects.selected = Some(id.clone());
        let (text, enabled) = paint_projects(&mut projects, &snapshot);
        assert!(text.contains("本项目已有任务结果；如需再次执行，请新建项目。"), "missing terminal boundary for {status}: {text}");
        assert!(!text.contains("Worker 已连接，可提交项目"), "terminal {status} must not invite duplicate submission: {text}");
        assert!(!text.contains("完成 Worker 准备后再提交项目"), "terminal {status} must not suggest resubmission: {text}");
        assert!(!enabled, "terminal {status} must disable duplicate submission");
    }
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn unknown_project_run_keeps_duplicate_submission_disabled() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), true);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    assert_eq!(result["status"], "unknown");
    let run_id = result["run_id"].as_str().unwrap();
    let snapshot = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id);
    let (text, enabled) = paint_projects(&mut projects, &snapshot);
    assert!(text.contains("名额保留；不要手动释放或自动重跑"), "unknown safety guidance missing: {text}");
    assert!(!enabled, "unknown result must disable duplicate submission");
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn no_project_run_keeps_submit_enabled() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let snapshot = (*service.snapshot()).clone();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id);
    let (text, enabled) = paint_projects(&mut projects, &snapshot);
    assert!(text.contains("Worker 已连接，可提交项目"), "first-submit guidance missing: {text}");
    assert!(enabled, "first submission must remain enabled when no project run exists");
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn done_result_summary_separates_admission_from_task_status() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    assert_eq!(result["status"], "done");
    let artifact_dir = result["artifact_dir"].as_str().unwrap().to_string();
    let run_id = result["run_id"].as_str().unwrap();
    let snapshot = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id);
    let text = paint_projects_text(&mut projects, &snapshot);
    assert!(text.contains("结果核对"), "missing result summary: {text}");
    assert!(text.contains("最终任务结果：完成"), "missing final task result: {text}");
    assert!(text.contains("准入状态：已准入"), "missing admission state: {text}");
    assert!(text.contains(&format!("产物目录：{artifact_dir}")), "missing artifact path: {text}");
    assert!(text.contains("回执提示：核对产物目录和操作回执"), "missing receipt guidance: {text}");
    assert!(!text.contains("完成 Worker 准备并确认上传版本后，才能提交项目。"), "terminal result must not retain pre-submit guidance: {text}");
    assert!(!text.contains("已有项目任务执行中或结果未知；请查看执行活动，当前不能再次提交。"), "terminal result must not retain active-task guidance: {text}");
    assert!(!text.contains("打开产物目录"));
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn done_without_admission_is_not_reported_as_admitted() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let artifact_dir = result["artifact_dir"].as_str().unwrap().to_string();
    let run_id = result["run_id"].as_str().unwrap();
    let mut snapshot = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    let run = snapshot.runs.iter_mut().find(|run| run.run.detail["project_work"]["project_id"] == id).unwrap();
    run.run.status = "done".into();
    run.run.detail["owner_admission"] = serde_json::Value::String("not-admitted".into());
    run.run.detail["exit_proof"] = serde_json::Value::Null;
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id);
    let text = paint_projects_text(&mut projects, &snapshot);
    assert!(text.contains("最终任务结果：完成"), "missing done result: {text}");
    assert!(text.contains("准入状态：未准入"), "missing incomplete admission state: {text}");
    assert!(!text.contains("准入状态：已准入"), "incomplete evidence must not be admitted: {text}");
    assert!(text.contains(&format!("产物目录：{artifact_dir}")), "existing artifact path should remain visible: {text}");
    assert!(text.contains("证据不完整"), "missing incomplete-evidence guidance: {text}");
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn unknown_result_summary_keeps_slot_and_points_to_receipt() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), true);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    assert_eq!(result["status"], "unknown");
    let run_id = result["run_id"].as_str().unwrap();
    let snapshot = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id);
    let text = paint_projects_text(&mut projects, &snapshot);
    assert!(text.contains("最终任务结果：结果未知"), "missing unknown result: {text}");
    assert!(text.contains("准入状态：未准入"), "unknown must not be admitted: {text}");
    assert!(text.contains("产物目录：尚未物化"), "unknown must not claim an artifact: {text}");
    assert!(text.contains("名额保留；不要手动释放或自动重跑"), "missing unknown safety guidance: {text}");
    assert!(!text.contains("准入状态：已准入"));
    assert!(!text.contains("打开产物目录"));
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn failed_cancelled_and_running_summaries_do_not_claim_admission() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let run_id = result["run_id"].as_str().unwrap();
    let base = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    for (status, result_label, guidance) in [
        ("failed", "最终任务结果：失败", "查看操作回执；不会自动重跑"),
        ("cancelled", "最终任务结果：已取消", "以最终取消回执为准"),
        ("running", "最终任务结果：尚未返回", "等待最终任务回执"),
    ] {
        let mut snapshot = base.clone();
        let run = snapshot.runs.iter_mut().find(|run| run.run.detail["project_work"]["project_id"] == id).unwrap();
        run.run.status = status.into();
        run.run.detail["artifact_dir"] = serde_json::Value::Null;
        let mut projects = desktop::projects::Projects::default();
        projects.selected = Some(id.clone());
        let text = paint_projects_text(&mut projects, &snapshot);
        assert!(text.contains(result_label), "missing {status} result: {text}");
        assert!(text.contains(guidance), "missing {status} guidance: {text}");
        assert!(!text.contains("准入状态：已准入"), "{status} must not be admitted: {text}");
        assert!(!text.contains("产物目录：/"), "{status} must not invent an artifact path: {text}");
        if status == "running" {
            assert!(text.contains("准入状态：尚未形成终态"), "running must remain pre-terminal: {text}");
            assert!(!text.contains("Worker 存活"));
        }
    }
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn result_summary_remains_project_scoped_after_switch() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, first_id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: first_id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let artifact_dir = result["artifact_dir"].as_str().unwrap().to_string();
    let run_id = result["run_id"].as_str().unwrap();
    let mut snapshot = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    let mut second = snapshot.projects[0].clone();
    second.id = "summary-second-project".into();
    second.name = "第二摘要项目".into();
    snapshot.projects.push(second);
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some("summary-second-project".into());
    let second_text = paint_projects_text(&mut projects, &snapshot);
    assert!(second_text.contains("结果核对"), "second project should still expose its own empty summary: {second_text}");
    assert!(second_text.contains("最终任务结果：尚未返回"), "second project must not inherit first result: {second_text}");
    assert!(!second_text.contains(&artifact_dir), "second project leaked first artifact path: {second_text}");
    projects.selected = Some(first_id.clone());
    assert!(snapshot.projects.iter().any(|project| project.id == first_id), "first project missing from snapshot: {:?}", snapshot.projects.iter().map(|project| &project.id).collect::<Vec<_>>());
    assert!(snapshot.runs.iter().any(|run| run.run.detail["project_work"]["project_id"] == first_id), "first run missing from snapshot: {:?}", snapshot.runs.iter().map(|run| (&run.run.status, &run.run.detail["project_work"]["project_id"])).collect::<Vec<_>>());
    let first_text = paint_projects_text(&mut projects, &snapshot);
    assert!(first_text.contains("最终任务结果：完成"), "first project lost completed summary: {first_text}");
    assert!(first_text.contains(&format!("产物目录：{artifact_dir}")), "first project lost its artifact path: {first_text}");
    service.stop();
}

#[cfg(feature = "gui")]
#[test]
fn final_done_summary_does_not_change_barrier_authority() {
    let root = tempfile::tempdir().unwrap();
    let release = Arc::new(AtomicBool::new(true));
    let final_release = Arc::new(AtomicBool::new(false));
    let (service, id) = streaming_service_with_final_barrier(root.path(), release, Some(final_release.clone()), false);
    let ledger = agentranch::core::provision_ledger::ProvisionLedger::load(&root.path().join("ui")).unwrap().unwrap();
    let db = rusqlite::Connection::open_with_flags(
        ledger.output_dir.unwrap().join("controller-state/controller.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ).unwrap();
    let ctx = eframe::egui::Context::default();
    let mut app = desktop::Workbench::from_service(service, &ctx, root.path().join("ui").display().to_string());
    app.send(format!("work-submit:{id}"), UiCommand::ProjectWorkSubmit { project_id: id.clone() });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let text = paint_text(&mut app, &ctx);
        if text.contains("输出通道：模型输出已结束") { break; }
        assert!(Instant::now() < deadline, "model completion not painted: {text}");
        std::thread::sleep(Duration::from_millis(2));
    }
    let before_final = paint_text(&mut app, &ctx);
    assert!(before_final.contains("最终任务结果：尚未返回"), "premature final result: {before_final}");
    assert!(before_final.contains("准入状态：尚未形成终态"), "premature admission: {before_final}");
    assert!(before_final.contains("产物目录：尚未物化"), "premature artifact: {before_final}");
    assert_eq!(app.receipt_count(), 0);
    assert_eq!(app.pending_count(), 1);
    let reserved: i64 = db.query_row("SELECT count(*) FROM assignments WHERE released=0", [], |row| row.get(0)).unwrap();
    let done: i64 = db.query_row("SELECT count(*) FROM runs WHERE status='done'", [], |row| row.get(0)).unwrap();
    assert_eq!(reserved, 1);
    assert_eq!(done, 0);

    final_release.store(true, Ordering::Release);
    while app.pending_count() > 0 {
        app.poll();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let after_final = paint_text(&mut app, &ctx);
    assert!(after_final.contains("最终任务结果：完成"), "missing final result: {after_final}");
    assert!(after_final.contains("准入状态：已准入"), "missing final admission: {after_final}");
    assert!(after_final.contains("产物目录："), "missing final artifact path: {after_final}");
    assert!(after_final.contains("回执提示：核对产物目录和操作回执"), "missing final receipt guidance: {after_final}");
    let reserved: i64 = db.query_row("SELECT count(*) FROM assignments WHERE released=0", [], |row| row.get(0)).unwrap();
    let done: i64 = db.query_row("SELECT count(*) FROM runs WHERE status='done'", [], |row| row.get(0)).unwrap();
    assert_eq!(reserved, 0);
    assert_eq!(done, 1);
}

#[cfg(feature = "gui")]
#[test]
fn summary_does_not_turn_error_or_model_finished_into_terminal_result() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let snapshot = (*service.snapshot()).clone();
    let ctx = eframe::egui::Context::default();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id.clone());
    projects.begin_live(71, id.clone());
    projects.apply_live(agentranch::core::service::LiveOutputEvent {
        request_id: 71,
        project_id: id.clone(),
        output: agentranch::core::live_output::LiveOutput::Error("transport interrupted".into()),
        received_at: Instant::now(),
        received_bytes: 0,
        truncated: false,
    });
    let error_text = paint_projects_text(&mut projects, &snapshot);
    assert!(error_text.contains("最终任务结果：尚未返回"), "transport error must not finalize summary: {error_text}");
    assert!(!error_text.contains("最终任务结果：失败"));
    projects.apply_live(agentranch::core::service::LiveOutputEvent {
        request_id: 71,
        project_id: id.clone(),
        output: agentranch::core::live_output::LiveOutput::ModelFinished,
        received_at: Instant::now(),
        received_bytes: 0,
        truncated: false,
    });
    let model_finished_text = paint_projects_text(&mut projects, &snapshot);
    assert!(model_finished_text.contains("最终任务结果：尚未返回"), "model finish must not finalize summary: {model_finished_text}");
    assert!(model_finished_text.contains("准入状态：尚未形成终态"));
    assert!(!model_finished_text.contains("准入状态：已准入"));
    drop(ctx);
    service.stop();
}

#[cfg(feature = "gui")]
fn output_frame(
    projects: &mut desktop::projects::Projects,
    snapshot: &agentranch::core::service::AppSnapshot,
    ctx: &eframe::egui::Context,
    events: Vec<eframe::egui::Event>,
    receipts: &[(String, String)],
) -> String {
    use eframe::egui;
    let mut out = ctx.run_ui(egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200., 750.))),
        events, ..Default::default()
    }, |ui| { projects.show(ui, snapshot, &desktop::pending::Pending::default(), receipts, false); });
    out.textures_delta.clear();
    out.shapes.iter().filter_map(|shape| match &shape.shape {
        egui::epaint::Shape::Text(text)
            if shape.clip_rect.intersects(egui::Rect::from_min_size(text.pos, text.galley.size())) => Some(text.galley.job.text.clone()),
        _ => None,
    }).collect::<Vec<_>>().join("\n")
}

#[cfg(feature = "gui")]
#[test]
fn output_area_is_visible_before_project_creation_and_submission() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let mut snapshot = (*service.snapshot()).clone();
    service.stop();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some(id);
    let ctx = eframe::egui::Context::default();
    let idle = output_frame(&mut projects, &snapshot, &ctx, vec![], &[]);
    assert!(idle.contains("模型实时输出（SSE）"), "missing permanent output area: {idle}");
    assert!(idle.contains("尚未提交项目"), "must explain why no body exists: {idle}");
    assert!(!idle.contains("输出通道：已连接"));
    assert!(!idle.contains("正在接收模型实际输出"));
    snapshot.projects.clear();
    let welcome = output_frame(&mut projects, &snapshot, &ctx, vec![], &[]);
    assert!(welcome.contains("模型实时输出（SSE）"), "welcome must expose the output location: {welcome}");
    assert!(welcome.contains("新建项目并确认设计文档后"));
}

#[cfg(feature = "gui")]
#[test]
fn output_area_explains_missing_session_body_without_inviting_resubmission() {
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(true)), false);
    let request = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, request).result.unwrap();
    let run_id = result["run_id"].as_str().unwrap();
    let base = (*wait_for_snapshot_run(&mut service, run_id)).clone();
    service.stop();
    for status in ["running", "done", "failed", "cancelled", "unknown"] {
        let mut snapshot = base.clone();
        snapshot.runs.iter_mut().find(|view| view.run.id == run_id).unwrap().run.status = status.into();
        let mut projects = desktop::projects::Projects::default();
        let text = paint_projects_text(&mut projects, &snapshot);
        assert!(text.contains("模型实时输出（SSE）"), "missing output area for {status}: {text}");
        assert!(text.contains("本次会话没有该任务的实时正文；不会回放历史输出"), "missing non-replay boundary for {status}: {text}");
        assert!(!text.contains("尚未提交项目"), "existing run must not invite resubmission: {text}");
        assert!(!text.contains("正在接收模型实际输出"));
    }
    let mut unrelated = base;
    unrelated.runs[0].run.template = "sum-v1".into();
    let mut projects = desktop::projects::Projects::default();
    let text = paint_projects_text(&mut projects, &unrelated);
    assert!(text.contains("尚未提交项目"), "unrelated template must not provide session history: {text}");
    assert!(!text.contains("本次会话没有该任务的实时正文"));
}

#[cfg(feature = "gui")]
#[test]
fn accepted_submit_selects_project_and_reveals_output_only_once() {
    use eframe::egui;
    let root = tempfile::tempdir().unwrap();
    let (mut service, id) = streaming_service(root.path(), Arc::new(AtomicBool::new(false)), false);
    let mut snapshot = (*service.snapshot()).clone();
    service.stop();
    let mut second = snapshot.projects[0].clone();
    second.id = "second-output-project".into();
    snapshot.projects.push(second);
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some("second-output-project".into());
    let ctx = egui::Context::default();
    let receipts = vec![("fixture receipt".into(), "long receipt line\n".repeat(150))];
    let wheel = |delta| vec![egui::Event::PointerMoved(egui::pos2(950., 650.)), egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0., delta), phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Wheel, modifiers: Default::default(),
    }];
    for _ in 0..4 { output_frame(&mut projects, &snapshot, &ctx, vec![], &receipts); }
    output_frame(&mut projects, &snapshot, &ctx, wheel(-700.), &receipts);
    let mut scrolled = String::new();
    for _ in 0..20 { scrolled = output_frame(&mut projects, &snapshot, &ctx, vec![], &receipts); }
    assert!(!scrolled.contains("普通项目路径"), "fixture must genuinely scroll away from top: {scrolled}");
    projects.begin_live(71, id.clone());
    assert_eq!(projects.selected.as_deref(), Some(id.as_str()), "accepted request must select its own project");
    let mut revealed = String::new();
    for _ in 0..4 { revealed = output_frame(&mut projects, &snapshot, &ctx, vec![], &receipts); }
    assert!(revealed.contains("模型实时输出（SSE）"), "accepted submit must reveal output: {revealed}");
    output_frame(&mut projects, &snapshot, &ctx, wheel(-700.), &receipts);
    for _ in 0..20 { scrolled = output_frame(&mut projects, &snapshot, &ctx, vec![], &receipts); }
    assert!(!scrolled.contains("模型实时输出（SSE）"), "later user scrolling must not be reset each frame: {scrolled}");
}

#[cfg(feature = "gui")]
fn egui_id(selector: &str) -> eframe::egui::Id {
    eframe::egui::Id::unique(("agentranch", "project-action", selector))
}

struct FixtureFactory { captured: Arc<Mutex<Option<Value>>>, calls: Arc<AtomicUsize>, outcome: FixtureOutcome }
impl GatewayTransportFactory for FixtureFactory {
    fn sum_transport(&self, _: SshGatewayConfig, _: agentranch::core::session_credentials::CredentialLease, _: Arc<AtomicBool>) -> Result<Box<dyn WorkerTransport>> { anyhow::bail!("sum transport not used") }
    fn project_transport(&self, _: SshGatewayConfig, _: agentranch::core::session_credentials::CredentialLease, _: Arc<AtomicBool>) -> Result<Box<dyn WorkerTransport>> {
        Ok(Box::new(TerminalTransport { captured: self.captured.clone(), calls: self.calls.clone(), outcome: self.outcome }))
    }
}

#[test]
fn activated_direct_submit_captures_stored_identity_and_materializes_admitted_fixture_artifact() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("ui");
    let mut projects = Projects::open(&state).unwrap();
    let id = create_project(&mut projects);
    let text = "# Confirmed direct work\nThe stored text must reach project-work-v1.\n";
    let document = import_and_confirm(&mut projects, &id, text);
    drop(projects);
    let captured = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FixtureFactory { captured: captured.clone(), calls: calls.clone(), outcome: FixtureOutcome::Done });
    let mut service = ServiceHandle::spawn_with_test_work_registration_hook(
        ServiceOptions { mode: Mode::Real, state_dir: state.clone(), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}),
        Box::new(FixtureProvisioner { bundle: bundle(root.path()) }),
        factory,
        Arc::new(|| {}),
    ).unwrap();
    let activate = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(wait_for(&mut service, activate).result.is_ok());
    let submit = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id.clone() }).unwrap();
    let result = wait_for(&mut service, submit).result.unwrap();
    assert_eq!(result["status"], "done", "fixture result: {result}");
    assert_eq!(result["project_id"], id);
    assert_eq!(result["document_version"], document.version);
    assert_eq!(result["document_sha256"], document.sha256);
    let captured = captured.lock().unwrap().clone().expect("transport must receive signed project work");
    assert_eq!(captured["schema"], "project-work/1");
    assert_eq!(captured["project_id"], id);
    assert_eq!(captured["document"]["version"], document.version);
    assert_eq!(captured["document"]["sha256"], document.sha256);
    assert_eq!(captured["document"]["text"], text);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let artifact_dir = result["artifact_dir"].as_str().expect("admitted fixture must materialize artifact");
    assert_eq!(std::fs::read(Path::new(artifact_dir).join("index.html")).unwrap(), b"fixture artifact");
    let snapshot = service.snapshot();
    let run = snapshot.runs.iter().find(|run| run.run.id == result["run_id"]).unwrap();
    assert_eq!(run.run.status, "done");
    assert_eq!(run.run.detail["owner_admission"], "admitted");
    assert!(run.run.detail["exit_proof"].is_object());
    assert_eq!(run.run.detail["artifact_dir"], result["artifact_dir"]);
    assert!(!result["slot_reserved"].as_bool().unwrap());
    let diag = std::fs::read_to_string(state.join("manager-diag.log")).unwrap();
    assert!(diag.lines().any(|line| line.contains("\tartifact_materialized\t") && line.contains(&format!("run_id={}", result["run_id"].as_str().unwrap()))), "direct project-work must record artifact materialization: {diag}");
    service.stop();
}

#[test]
fn activated_submit_with_eof_remains_unknown_and_retains_assignment() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("ui");
    let mut projects = Projects::open(&state).unwrap();
    let id = create_project(&mut projects);
    let _ = import_and_confirm(&mut projects, &id, "# Confirmed\nEOF must not become success.\n");
    drop(projects);
    let captured = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FixtureFactory { captured, calls: calls.clone(), outcome: FixtureOutcome::Eof });
    let mut service = ServiceHandle::spawn_with_test_work_registration_hook(
        ServiceOptions { mode: Mode::Real, state_dir: state, worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}), Box::new(FixtureProvisioner { bundle: bundle(root.path()) }), factory, Arc::new(|| {}),
    ).unwrap();
    let activate = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(wait_for(&mut service, activate).result.is_ok());
    let submit = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id }).unwrap();
    let result = wait_for(&mut service, submit).result.unwrap();
    assert_eq!(result["status"], "unknown");
    assert!(result["slot_reserved"].as_bool().unwrap());
    assert!(result.get("artifact_dir").is_none(), "EOF cannot materialize an unadmitted artifact");
    let snapshot = wait_for_snapshot_run(&mut service, result["run_id"].as_str().unwrap());
    let run = snapshot.runs.iter().find(|run| run.run.id == result["run_id"]).unwrap();
    assert_eq!(run.run.status, "unknown");
    assert_ne!(run.run.detail["owner_admission"], "admitted");
    assert_eq!(snapshot.gateway.as_ref().unwrap().used_slots, 1, "unknown run retains its assignment");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    service.stop();
}

#[test]
fn project_work_submit_clears_expired_credentials_before_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("ui");
    let mut projects = Projects::open(&state).unwrap();
    let id = create_project(&mut projects);
    let _ = import_and_confirm(&mut projects, &id, "# Confirmed\nExpired credentials must block dispatch.\n");
    drop(projects);

    let now = Arc::new(AtomicU64::new(1_000));
    let factory = Arc::new(FixtureFactory { captured: Arc::new(Mutex::new(None)), calls: Arc::new(AtomicUsize::new(0)), outcome: FixtureOutcome::Done });
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let before_work_registration = {
        let now = now.clone();
        Arc::new(move || now.store(2_801, Ordering::SeqCst))
    };
    let mut service = ServiceHandle::spawn_with_clock_provisioner_factory_hook(
        ServiceOptions { mode: Mode::Real, state_dir: state, worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}), clock, Box::new(FixtureProvisioner { bundle: bundle(root.path()) }), factory, before_work_registration,
    ).unwrap();
    let activate = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(wait_for(&mut service, activate).result.is_ok());
    now.store(2_801, Ordering::SeqCst);

    let submit = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: id }).unwrap();
    let result = wait_for(&mut service, submit).result.unwrap_err();
    assert_eq!(result.code, "project_work_blocked");
    assert!(result.message.contains("凭据") || result.message.contains("Worker"), "expired credentials must be explained: {result:?}");
    assert!(service.snapshot().runs.is_empty(), "expired credentials must block before Store dispatch");
    assert!(!service.snapshot().gateway.as_ref().unwrap().credentials_available, "expired credentials must be cleared synchronously");
    service.stop();
}

#[test]
fn project_work_submit_with_no_available_slot_does_not_leave_queued_orphan_run() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("ui");
    let mut projects = Projects::open(&state).unwrap();
    let first = create_project(&mut projects);
    let second = create_project(&mut projects);
    let _ = import_and_confirm(&mut projects, &first, "# First\nThis run retains its slot after EOF.\n");
    let _ = import_and_confirm(&mut projects, &second, "# Second\nThis submit must be rejected while the slot is retained.\n");
    drop(projects);

    let captured = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FixtureFactory { captured, calls: calls.clone(), outcome: FixtureOutcome::Eof });
    let mut service = ServiceHandle::spawn_with_test_work_registration_hook(
        ServiceOptions { mode: Mode::Real, state_dir: state, worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None },
        Arc::new(|| {}), Box::new(FixtureProvisioner { bundle: bundle(root.path()) }), factory, Arc::new(|| {}),
    ).unwrap();
    let activate = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(wait_for(&mut service, activate).result.is_ok());

    let first_submit = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: first.clone() }).unwrap();
    let first_result = wait_for(&mut service, first_submit).result.unwrap();
    assert_eq!(first_result["status"], "unknown");
    assert!(first_result["slot_reserved"].as_bool().unwrap());

    let second_submit = service.try_submit(UiCommand::ProjectWorkSubmit { project_id: second.clone() }).unwrap();
    let second_result = wait_for(&mut service, second_submit).result.unwrap_err();
    assert_eq!(second_result.code, "project_work_blocked");
    assert!(second_result.message.contains("no available slot"), "slot exhaustion must be explicit: {second_result:?}");

    let snapshot = service.snapshot();
    assert_eq!(snapshot.runs.len(), 1, "rejected submission must not leave a queued Store run");
    assert_eq!(snapshot.runs[0].run.status, "unknown");
    assert_eq!(snapshot.runs[0].run.detail["project_work"]["project_id"], first);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the rejected project must not reach transport");
    service.stop();
}
