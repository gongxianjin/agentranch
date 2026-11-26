use agentranch::config::Mode;
use agentranch::core::{
    provision::{
        decode_result_frame, decode_result_frame_in_dir_with_attempt, encode_request_frame,
        encode_request_frame_with_output_dir, exchange_process, HelperOutput,
        ProvisionBridge, ProvisioningContext,
    },
    service::{ProvisionRequest, ServiceHandle, ServiceOptions, UiCommand},
    provision::{Provisioner, ProvisionResult, ProvisionFailure},
};
use serde_json::json;
use std::{
    io::{Read, Write},
    process::Command,
    sync::Arc,
    time::Duration,
};

fn request() -> ProvisionRequest {
    ProvisionRequest {
        host: "192.0.2.10".into(),
        root_user: "root".into(),
        root_password: b"root-secret".to_vec(),
        model_base_url: "https://model.example".into(),
        model_token: b"api-secret".to_vec(),
        model: "test-model".into(),
    }
}

#[test]
fn manager_context_derives_a_new_absolute_output_dir_without_reusing_old_content() {
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("provisioning").join("worker-old");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("gateway.json"), b"old").unwrap();
    let context = ProvisioningContext::from_state_dir(root.path()).unwrap();
    assert!(context.output_dir().is_absolute());
    assert!(!context.output_dir().exists());
    assert_ne!(context.output_dir(), old.as_path());
    assert!(context.output_dir().starts_with(root.path().join("provisioning")));
}

#[test]
fn retry_context_always_allocates_fresh_output_and_preserves_old_attempt() {
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("provisioning").join("worker-old");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("gateway.json"), b"failed-attempt").unwrap();

    let retry = ProvisioningContext::retry_from(root.path()).unwrap();

    assert!(retry.output_dir().is_absolute());
    assert!(!retry.output_dir().exists());
    assert_ne!(retry.output_dir(), old.as_path());
    assert_eq!(std::fs::read(old.join("gateway.json")).unwrap(), b"failed-attempt");
    assert!(retry.output_dir().starts_with(root.path().join("provisioning")));
}

#[test]
fn retry_context_rejects_linked_state_before_helper_mutation() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = root.path().join("state-link");
    #[cfg(windows)]
    {
        // A junction exercises the Windows reparse check without symlink privileges.
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .status()
            .unwrap();
        assert!(status.success(), "junction creation failed: {status}");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let error = ProvisioningContext::retry_from(&link).unwrap_err();
    assert!(
        error.to_string().contains("provisioning path contains a symlink")
            || error.to_string().contains("provisioning path contains a reparse point"),
        "unexpected linked-state rejection: {error}"
    );
    assert!(!real.join("provisioning").exists());
}

#[test]
fn manager_context_rejects_relative_existing_and_traversing_output_dirs() {
    let root = tempfile::tempdir().unwrap();
    let existing = root.path().join("existing");
    std::fs::create_dir(&existing).unwrap();
    assert!(ProvisioningContext::new(existing).is_err());
    assert!(ProvisioningContext::new(root.path().join("..").join("escape")).is_err());
    assert!(ProvisioningContext::new("relative-output".into()).is_err());
}

#[test]
fn request_frame_carries_manager_output_dir_but_never_secret_argv_or_env() {
    let root = tempfile::tempdir().unwrap();
    let context = ProvisioningContext::from_state_dir(root.path()).unwrap();
    let frame = encode_request_frame_with_output_dir(&request(), &context).unwrap();
    let magic_len = b"AW-WORKER-PROVISION/1\n".len();
    let length = u32::from_be_bytes(frame[magic_len..magic_len + 4].try_into().unwrap()) as usize;
    let metadata = &frame[magic_len + 4..magic_len + 4 + length];
    let decoded: serde_json::Value = serde_json::from_slice(metadata).unwrap();
    assert_eq!(decoded["output_dir"], context.output_dir().display().to_string());
    assert!(!metadata.windows(b"root-secret".len()).any(|part| part == b"root-secret"));
    assert!(!metadata.windows(b"api-secret".len()).any(|part| part == b"api-secret"));
}

#[test]
fn successful_result_must_carry_current_attempt_identity() {
    let root = tempfile::tempdir().unwrap();
    let context = ProvisioningContext::from_state_dir(root.path()).unwrap();
    let missing = json!({"schema":"worker-provision/1", "status":"PASS_WORKER_PROVISION", "preflight":true,
        "gateway_path":context.output_dir().join("gateway.json")});
    assert!(decode_result_frame_in_dir_with_attempt(
        &result_frame(&missing, b"synthetic-private-key"),
        b"root-secret", b"api-secret", &context,
    ).is_err(), "successful helper output without attempt identity must be rejected");

    let wrong = json!({"schema":"worker-provision/1", "status":"PASS_WORKER_PROVISION", "preflight":true,
        "gateway_path":context.output_dir().join("gateway.json"), "attempt_id":"attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"});
    assert!(decode_result_frame_in_dir_with_attempt(
        &result_frame(&wrong, b"synthetic-private-key"),
        b"root-secret", b"api-secret", &context,
    ).is_err(), "successful helper output from another attempt must be rejected");

    let current = json!({"schema":"worker-provision/1", "status":"PASS_WORKER_PROVISION", "preflight":true,
        "gateway_path":context.output_dir().join("gateway.json"), "attempt_id":context.attempt_id()});
    assert!(decode_result_frame_in_dir_with_attempt(
        &result_frame(&current, b"synthetic-private-key"),
        b"root-secret", b"api-secret", &context,
    ).is_ok());
}

#[test]
fn pass_result_gateway_must_be_the_manager_output_gateway() {
    let root = tempfile::tempdir().unwrap();
    let context = ProvisioningContext::from_state_dir(root.path()).unwrap();
    let pass = json!({"schema":"worker-provision/1", "status":"PASS_WORKER_PROVISION", "preflight":true,
        "gateway_path":context.output_dir().join("gateway.json"), "attempt_id":context.attempt_id(), "next_step":"ready"});
    let parsed = decode_result_frame_in_dir_with_attempt(&result_frame(&pass, b"synthetic-private-key"),
        b"root-secret", b"api-secret", &context).unwrap();
    let private_key = parsed.take_private_key();
    assert_eq!(&*private_key, b"synthetic-private-key");
    let escaped = json!({"schema":"worker-provision/1", "status":"PASS_WORKER_PROVISION", "preflight":true,
        "gateway_path":root.path().join("escape").join("gateway.json")});
    assert!(decode_result_frame_in_dir_with_attempt(&result_frame(&escaped, b"key"),
        b"root-secret", b"api-secret", &context).is_err());
}

fn child_command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "provision_child_fixture", "--nocapture"])
        .env("AW_PROVISION_FIXTURE", mode);
    command
}

#[test]
fn request_frame_keeps_secrets_out_of_metadata() {
    let frame = encode_request_frame(&request()).unwrap();
    assert!(frame.starts_with(b"AW-WORKER-PROVISION/1\n"));
    let magic_len = b"AW-WORKER-PROVISION/1\n".len();
    let length = u32::from_be_bytes(frame[magic_len..magic_len + 4].try_into().unwrap()) as usize;
    let metadata = &frame[magic_len + 4..magic_len + 4 + length];
    assert!(!metadata.windows(b"root-secret".len()).any(|part| part == b"root-secret"));
    assert!(!metadata.windows(b"api-secret".len()).any(|part| part == b"api-secret"));
    assert!(!format!("{frame:?}").contains("root-secret"));
    assert!(!format!("{frame:?}").contains("api-secret"));
    assert!(frame.windows(b"192.0.2.10".len()).any(|part| part == b"192.0.2.10"));
}

#[test]
fn request_rejects_secrets_accidentally_echoed_in_public_metadata() {
    let mut request = request();
    request.model = "api-secret".into();
    assert!(encode_request_frame(&request).is_err(), "secret must not be serialized into metadata");
}

#[test]
fn result_rejects_input_secret_echoes_and_embedded_private_key() {
    let blocked = json!({"schema":"worker-provision/1", "status":"BLOCKED_OR_FAILED", "next_step":"echo api-secret"});
    assert!(decode_result_frame(&result_frame(&blocked, b""), b"root-secret", b"api-secret").is_err());
    let success = json!({"schema":"worker-provision/1", "status":"PASS_WORKER_PROVISION", "preflight":true,
        "gateway_path":std::env::temp_dir().join("gateway.json"), "attempt_id":"attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "next_step":"ready"});
    let parsed = decode_result_frame(&result_frame(&success, b"synthetic-private-key"), b"root-secret", b"api-secret").unwrap();
    assert!(!format!("{parsed:?}").contains("synthetic-private-key"));
}

#[test]
fn helper_command_clears_environment_and_keeps_secrets_out_of_argv() {
    let temp = tempfile::tempdir().unwrap();
    let helper = temp.path().join("paramiko_provision.py");
    let interpreter = temp.path().join("python.exe");
    std::fs::write(&helper, b"# synthetic helper").unwrap();
    std::fs::write(&interpreter, b"# synthetic interpreter").unwrap();
    let bridge = ProvisionBridge::with_runtime(helper, interpreter).unwrap();
    let command = bridge.command_for_test().unwrap();
    let args = command
        .get_args()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(args.iter().any(|arg| arg == "-I"));
    assert!(args.iter().any(|arg| arg.ends_with("paramiko_provision.py")));
    assert!(!args.iter().any(|arg| arg.contains("root-secret")));
    assert!(!args.iter().any(|arg| arg.contains("api-secret")));
    assert!(command.get_envs().all(|(key, value)| {
        key != "ROOT_PASSWORD"
            && key != "MODEL_TOKEN"
            && value
                .map(|value| {
                    let value = value.to_string_lossy();
                    !value.contains("root-secret") && !value.contains("api-secret")
                })
                .unwrap_or(true)
    }));
}

#[test]
fn non_json_helper_output_is_rejected_without_gateway_side_effects() {
    let mut command = child_command("non-json");
    let output = exchange_process(&mut command, b"synthetic-frame", Duration::from_secs(2)).unwrap();
    assert_eq!(output.exit_code, Some(0));
    let error = decode_result_frame(&output.stdout, b"root-secret", b"api-secret").unwrap_err();
    assert!(error.to_string().contains("result frame"));
    assert!(output.stderr.len() <= 16 * 1024);
}

#[test]
fn helper_budget_covers_the_internal_provisioning_stage_window() {
    // Root authentication/discovery (15s), ordinary-user connection (15s),
    // upload (30s), and remote preflight (30s) must fit inside the outer
    // Manager helper budget. An immediate helper keeps this a contract test,
    // not a 105-second wait.
    let mut command = child_command("non-json");
    let output = exchange_process(&mut command, b"synthetic-frame", Duration::from_secs(105))
        .expect("outer budget must allow the documented internal stage window");
    assert_eq!(output.exit_code, Some(0));
}

#[test]
fn timeout_is_bounded_and_keeps_only_bounded_stderr_diagnostics() {
    let started = std::time::Instant::now();
    let mut command = child_command("timeout");
    let output = exchange_process(&mut command, b"synthetic-frame", Duration::from_millis(250)).unwrap();
    assert!(output.failure.as_deref().unwrap_or_default().contains("timeout"));
    assert!(started.elapsed() < Duration::from_secs(2), "timeout escaped its budget");
    assert!(output.stderr.len() <= 16 * 1024);
    assert!(output.stdout.len() <= 64 * 1024);
    assert!(output.exit_code.is_some());
}

#[test]
fn stderr_diagnostic_is_bounded_and_redacted_before_service_error() {
    let failure = ProvisionFailure::from_stderr("helper_result_invalid", b"bad root-secret api-secret token", &[b"root-secret", b"api-secret"]);
    let error = failure.service_error();
    assert!(!error.message.contains("root-secret"));
    assert!(!error.message.contains("api-secret"));
    assert!(error.message.len() < 512);
}

#[test]
fn blocked_result_cannot_hold_private_key_bytes() {
    let blocked = json!({"schema":"worker-provision/1", "status":"BLOCKED_OR_FAILED", "next_step":"retry"});
    assert!(decode_result_frame(&result_frame(&blocked, b"not-allowed"), b"root-secret", b"api-secret").is_err());
}

#[test]
fn result_private_key_cannot_echo_input_secret_bytes() {
    let success = json!({"schema":"worker-provision/1", "status":"PASS_WORKER_PROVISION", "preflight":true,
        "gateway_path":std::env::temp_dir().join("gateway.json"), "attempt_id":"attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"});
    assert!(decode_result_frame(&result_frame(&success, b"root-secret"), b"root-secret", b"api-secret").is_err());
    assert!(decode_result_frame(&result_frame(&success, b"api-secret"), b"root-secret", b"api-secret").is_err());
}

#[test]
fn helper_that_does_not_read_stdin_still_returns_within_budget() {
    let started = std::time::Instant::now();
    let mut command = child_command("no-read");
    let output = exchange_process(&mut command, &vec![b'x'; 128 * 1024], Duration::from_millis(250)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "blocked stdin escaped its budget");
    assert!(output.failure.is_some());
}

#[test]
fn result_metadata_rejects_secret_fields_and_private_key_stays_outside_metadata() {
    let secret_result = json!({
        "schema": "worker-provision/1",
        "status": "BLOCKED_OR_FAILED",
        "error_type": "secret-leak",
        "api_key": "api-secret"
    });
    let frame = result_frame(&secret_result, b"");
    assert!(decode_result_frame(&frame, b"root-secret", b"api-secret").is_err());

    let safe_result = json!({
        "schema": "worker-provision/1",
        "status": "BLOCKED_OR_FAILED",
        "error_type": "helper_not_wired",
        "next_step": "生成完整 gateway config 后重试"
    });
    let frame = result_frame(&safe_result, b"");
    let parsed = decode_result_frame(&frame, b"root-secret", b"api-secret").unwrap();
    assert_eq!(parsed.metadata, safe_result);
    assert!(decode_result_frame(&result_frame(&safe_result, b"unexpected-key"), b"root-secret", b"api-secret").is_err());

    let success = json!({"schema":"worker-provision/1", "status":"PASS_WORKER_PROVISION", "preflight":true,
        "gateway_path":std::env::temp_dir().join("synthetic/gateway.json"), "attempt_id":"attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"});
    let parsed = decode_result_frame(&result_frame(&success, b"synthetic-private-key"), b"root-secret", b"api-secret").unwrap();
    assert!(!format!("{parsed:?}").contains("synthetic-private-key"));
}

#[test]
fn blocked_helper_result_maps_to_blocked_and_does_not_create_gateway() {
    let result = agentranch::core::provision::service_error_from_result(&json!({
        "schema": "worker-provision/1",
        "status": "BLOCKED_OR_FAILED",
        "failed_stage": "provisioning",
        "error_type": "helper_not_wired",
        "next_step": "先生成完整 gateway config"
    }));
    assert_eq!(result.code, "blocked");
    assert!(result.next_step.contains("gateway"));

    let temp = tempfile::tempdir().unwrap();
    let mut service = ServiceHandle::spawn(
        ServiceOptions {
            mode: Mode::Real,
            state_dir: temp.path().join("state"),
            worker_exe: "unused.exe".into(),
            gateway_key: None,
            gateway_model_token: None,
        },
        Arc::new(|| {}),
    )
    .unwrap();
    let id = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    let result = wait_result(&mut service, id);
    assert!(result.result.is_err());
    assert!(service.snapshot().gateway.is_none());
    service.stop();
}

#[test]
fn provisioning_failure_does_not_replace_existing_gateway_snapshot_or_create_state() {
    let (root, config_path) = gateway_fixture();
    let mut service = ServiceHandle::spawn_with_provisioner(ServiceOptions {
        mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(),
        gateway_key: None, gateway_model_token: None,
    }, Arc::new(|| {}), Box::new(OfflineFailure)).unwrap();
    let configure = service.try_submit(UiCommand::GatewayConfigure { config_path }).unwrap();
    assert!(wait_result(&mut service, configure).result.is_ok());
    let before = service.snapshot();
    assert_eq!(before.gateway.as_ref().map(|gateway|gateway.worker_id.as_str()), Some("B"));
    let provision = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    let error = wait_result(&mut service, provision).result.unwrap_err();
    assert_eq!(error.code, "provision_error");
    let after = service.snapshot();
    assert_eq!(after.gateway.as_ref().map(|gateway|gateway.worker_id.as_str()), Some("B"));
    assert!(after.runs.is_empty());
    assert!(!root.path().join("ui").join("controller.sqlite").exists());
    service.stop();
}

#[test]
fn provision_child_fixture() {
    let Ok(mode) = std::env::var("AW_PROVISION_FIXTURE") else { return };
    if mode == "timeout" {
        std::thread::sleep(Duration::from_secs(10));
        return;
    }
    if mode == "no-read" {
        std::thread::sleep(Duration::from_secs(10));
        return;
    }
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    assert_eq!(input, b"synthetic-frame");
    if mode == "non-json" {
        std::io::stdout().write_all(b"not-json\n").unwrap();
    } else {
        std::io::stdout().write_all(b"unexpected\n").unwrap();
    }
}

fn result_frame(metadata: &serde_json::Value, private_key: &[u8]) -> Vec<u8> {
    let raw = serde_json::to_vec(metadata).unwrap();
    let mut frame = b"AW-WORKER-PROVISION-RESULT/1\n".to_vec();
    frame.extend_from_slice(&(raw.len() as u32).to_be_bytes());
    frame.extend_from_slice(&raw);
    frame.extend_from_slice(&(private_key.len() as u32).to_be_bytes());
    frame.extend_from_slice(private_key);
    frame
}

struct OfflineFailure;
impl Provisioner for OfflineFailure {
    fn provision(&self, _: &ProvisionRequest) -> Result<ProvisionResult, ProvisionFailure> {
        Err(ProvisionFailure::new("helper_timeout"))
    }
}

struct BlockedResidualProvisioner;
impl Provisioner for BlockedResidualProvisioner {
    fn provision(&self, _: &ProvisionRequest) -> Result<ProvisionResult, ProvisionFailure> {
        panic!("Manager context required")
    }

    fn provision_with_context(&self, request: &ProvisionRequest, context: &agentranch::core::provision::ProvisioningContext) -> Result<ProvisionResult, ProvisionFailure> {
        let metadata = json!({
            "schema": "worker-provision/1",
            "status": "BLOCKED_OR_FAILED",
            "failed_stage": "orchestration",
            "error_type": "remote_upload_failed",
            "next_step": "检查远端目录权限、文件 owner/mode 和资产 hash 后重试",
            "attempt_id": context.attempt_id(),
            "remote_root": "/home/operator/cross-runtime-abc123",
            "remote_side_effects": "present",
            "reason_code": "remote_upload_failed"
        });
        decode_result_frame_in_dir_with_attempt(
            &result_frame(&metadata, b""),
            &request.root_password,
            &request.model_token,
            context,
        ).map_err(|_| ProvisionFailure::new("fixture_invalid"))
    }
}

#[test]
fn blocked_result_persists_redacted_remote_residual_facts_in_failed_ledger() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("state")).unwrap();
    let mut service = ServiceHandle::spawn_with_provisioner(ServiceOptions {
        mode: Mode::Real,
        state_dir: temp.path().join("state"),
        worker_exe: "unused.exe".into(),
        gateway_key: None,
        gateway_model_token: None,
    }, Arc::new(|| {}), Box::new(BlockedResidualProvisioner)).unwrap();
    let id = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    assert!(wait_result(&mut service, id).result.is_err());
    let ledger = agentranch::core::provision_ledger::ProvisionLedger::load(
        &temp.path().join("state"),
    ).unwrap().expect("blocked helper result must leave a failed ledger");
    assert_eq!(ledger.failure_code.as_deref(), Some("remote_upload_failed"));
    assert_eq!(ledger.attempt_id.as_deref().map(str::len), Some("attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".len()));
    assert_eq!(ledger.remote_root.as_deref(), Some("/home/operator/cross-runtime-abc123"));
    assert_eq!(ledger.remote_side_effects.as_deref(), Some("present"));
    let raw = std::fs::read_to_string(temp.path().join("state/provisioning-ledger.json")).unwrap();
    assert!(!raw.contains("root-secret"));
    assert!(!raw.contains("api-secret"));
    service.stop();
}

fn gateway_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("test.key"), "07".repeat(32)).unwrap();
    let controller = json!({"schema":"ao-reason-config/1","mode":"real","role":"controller","deployment":"bridge-test",
        "state_dir":"state","nodes":[{"id":"B","address":"B","key_file":"test.key","slots":1,
        "templates":["sum-v1"],"admitted":true}]});
    std::fs::write(root.path().join("controller.json"), serde_json::to_vec(&controller).unwrap()).unwrap();
    let gateway = json!({"schema":"native-ssh-gateway/1","deployment":"bridge-test","worker_id":"B","host":"b.example",
        "port":22,"user":"worker","known_hosts":"known_hosts","identity_file":"id_ed25519","key_file":"test.key",
        "controller_config":"controller.json","state_dir":"state","remote_worker":"/home/worker/aw-cli",
        "remote_config":"/home/worker/worker.json","artifact_sha256":"ab".repeat(32),"artifact_bytes":123,"timeout_secs":1});
    let path = root.path().join("gateway.json");
    std::fs::write(&path, serde_json::to_vec(&gateway).unwrap()).unwrap();
    (root, path)
}

fn wait_result(service: &mut ServiceHandle, request_id: u64) -> agentranch::core::service::CommandResult {
    let until = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = service.drain_results().into_iter().find(|result|result.request_id == request_id) {
            return result;
        }
        assert!(std::time::Instant::now() < until, "service result did not arrive");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn worker_routes_provision_failure_to_provision_error_not_real_not_ready() {
    let temp = tempfile::tempdir().unwrap();
    let mut service = ServiceHandle::spawn_with_provisioner(ServiceOptions {
        mode: Mode::Real, state_dir: temp.path().join("state"), worker_exe: "unused.exe".into(),
        gateway_key: Some(b"untouched-key".to_vec()), gateway_model_token: Some(b"untouched-token".to_vec()),
    }, Arc::new(|| {}), Box::new(OfflineFailure)).unwrap();
    let id = service.try_submit(UiCommand::ProvisionWorker { request: request() }).unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(3);
    let error = loop {
        if let Some(reply) = service.drain_results().into_iter().find(|reply|reply.request_id==id) {
            break reply.result.unwrap_err();
        }
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(error.code, "provision_error");
    assert_ne!(error.code, "real_not_ready");
    assert!(!error.next_step.is_empty());
    assert!(service.snapshot().gateway.is_none());
    assert!(!temp.path().join("state").exists());
    service.stop();
}

#[allow(dead_code)]
fn _assert_output_type_is_bounded(_: HelperOutput) {}
