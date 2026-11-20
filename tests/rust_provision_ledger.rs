use agentranch::{config::Mode, core::{provision_ledger::{LedgerState, ProvisionLedger, ProvisionLedgerFacts}, service::{ServiceHandle, ServiceOptions}}};
use serde_json::json;
use std::{path::{Path, PathBuf}, sync::Arc, time::{Duration, Instant}};

fn pending_facts(output_dir: Option<PathBuf>) -> ProvisionLedgerFacts {
    ProvisionLedgerFacts {
        state: LedgerState::Pending,
        output_dir,
        gateway_path: None,
        worker_id: None,
        host: None,
        artifact_sha256: None,
        artifact_bytes: None,
        created_at: 100,
        updated_at: 101,
        failure_code: None,
        failure_next_step: None,
        attempt_id: None,
        remote_root: None,
        remote_side_effects: None,
    }
}

#[test]
fn ledger_persists_safe_retry_identity_and_remote_residual_facts_without_secrets() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("provisioning").join("worker-current");
    std::fs::create_dir_all(&output).unwrap();
    let mut facts = pending_facts(Some(output.clone()));
    facts.attempt_id = Some("attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into());
    facts.remote_root = Some("/home/operator/cross-runtime-abc123".into());
    facts.remote_side_effects = Some("present".into());
    let path = ProvisionLedger::write_new(root.path(), facts).unwrap();
    let raw = std::fs::read_to_string(path).unwrap();
    let loaded = ProvisionLedger::load(root.path()).unwrap().unwrap();
    assert_eq!(loaded.attempt_id.as_deref(), Some("attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    assert_eq!(loaded.remote_root.as_deref(), Some("/home/operator/cross-runtime-abc123"));
    assert_eq!(loaded.remote_side_effects.as_deref(), Some("present"));
    for secret in ["root-password", "model-token", "private-key-bytes"] {
        assert!(!raw.contains(secret));
    }
}

#[test]
fn ledger_round_trip_contains_only_non_secret_facts() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("provisioning").join("worker-current");
    std::fs::create_dir_all(&output).unwrap();
    let path = ProvisionLedger::write_new(root.path(), pending_facts(Some(output.clone()))).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    let loaded = ProvisionLedger::load(root.path()).unwrap().unwrap();

    assert_eq!(loaded.schema, "worker-provision-ledger/1");
    assert_eq!(loaded.state, LedgerState::Pending);
    assert_eq!(loaded.output_dir.as_deref(), Some(output.as_path()));
    assert_eq!(loaded.created_at, 100);
    assert_eq!(loaded.updated_at, 101);
    for secret in ["root-password", "model-token", "private-key-bytes"] {
        assert!(!raw.contains(secret), "ledger persisted secret-like value {secret}");
    }
    assert!(!raw.contains("root_password"));
    assert!(!raw.contains("model_token"));
    assert!(!raw.contains("private_key"));
}

#[test]
fn restart_recovery_rejects_ambiguous_or_escaped_ledger() {
    let root = tempfile::tempdir().unwrap();
    let provisioning = root.path().join("provisioning");
    let current = provisioning.join("worker-current");
    let older = provisioning.join("worker-older");
    std::fs::create_dir_all(&current).unwrap();
    std::fs::create_dir_all(&older).unwrap();
    ProvisionLedger::write_new(root.path(), pending_facts(Some(current.clone()))).unwrap();
    assert!(ProvisionLedger::load(root.path()).is_ok(), "preserved older sibling is not ambiguity");

    let escaped = json!({
        "schema": "worker-provision-ledger/1",
        "state": "pending",
        "output_dir": root.path().join("..").join("escape"),
        "gateway_path": null,
        "worker_id": null,
        "host": null,
        "artifact_sha256": null,
        "artifact_bytes": null,
        "created_at": 1,
        "updated_at": 1,
        "failure_code": null,
        "failure_next_step": null,
        "attempt_id": null,
        "remote_root": null,
        "remote_side_effects": null
    });
    std::fs::write(root.path().join("provisioning-ledger.json"), serde_json::to_vec(&escaped).unwrap()).unwrap();
    let error = ProvisionLedger::load(root.path()).unwrap_err();
    assert!(error.is_blocked(), "path escape must be a typed blocked error: {error:?}");

    let secret_field = json!({
        "schema": "worker-provision-ledger/1",
        "state": "failed",
        "output_dir": null,
        "gateway_path": null,
        "worker_id": null,
        "host": null,
        "artifact_sha256": null,
        "artifact_bytes": null,
        "created_at": 1,
        "updated_at": 1,
        "failure_code": "provision_error",
        "failure_next_step": "重新输入秘密并重试 Worker 准备；不要手工猜测旧 Gateway",
        "model_token": "must-not-load"
    });
    std::fs::write(root.path().join("provisioning-ledger.json"), serde_json::to_vec(&secret_field).unwrap()).unwrap();
    let error = ProvisionLedger::load(root.path()).unwrap_err();
    assert!(error.is_blocked(), "secret-looking field must be blocked: {error:?}");

    let options = ServiceOptions {
        mode: Mode::Real,
        state_dir: root.path().to_path_buf(),
        worker_exe: PathBuf::from("unused.exe"),
        gateway_key: None,
        gateway_model_token: None,
    };
    let mut service = ServiceHandle::spawn(options, Arc::new(|| {})).unwrap();
    assert!(service.snapshot().gateway.is_none(), "recovery must never recreate a live gateway");
    assert!(!root.path().join("controller.lock").exists(), "recovery must not open Store/controller lock");
    assert!(!root.path().join("identity.lock").exists(), "recovery must not open identity lock");
    service.stop();
}

#[test]
fn service_startup_recovers_non_secret_ledger_without_claiming_live_connection() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("provisioning").join("worker-current");
    std::fs::create_dir_all(&output).unwrap();
    let mut facts = pending_facts(Some(output));
    facts.state = LedgerState::Failed;
    facts.failure_code = Some("provision_error".into());
    facts.failure_next_step = Some("重新输入秘密并重试 Worker 准备；不要手工猜测旧 Gateway".into());
    ProvisionLedger::write_new(root.path(), facts).unwrap();

    let mut service = ServiceHandle::spawn(ServiceOptions {
        mode: Mode::Real,
        state_dir: root.path().to_path_buf(),
        worker_exe: PathBuf::from("unused.exe"),
        gateway_key: None,
        gateway_model_token: None,
    }, Arc::new(|| {})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while service.snapshot().provision_ledger.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let snapshot = service.snapshot();
    let ledger = snapshot.provision_ledger.as_ref().expect("valid ledger should be displayed");
    assert_eq!(ledger.host, None);
    assert_eq!(ledger.state, LedgerState::Failed);
    assert!(snapshot.gateway.is_none(), "historical ledger must not claim live connection");
    service.stop();
}

#[test]
fn controller_reparse_record_is_blocked_before_controller_config_read() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("provisioning").join("worker-current");
    std::fs::create_dir_all(&output).unwrap();
    let controller_target = root.path().join("controller-target.json");
    let controller_path = output.join("controller.json");
    std::fs::write(&controller_target, serde_json::to_vec(&json!({
        "schema": "ao-reason-config/1",
        "mode": "real",
        "role": "controller",
        "deployment": "ledger-fixture",
        "state_dir": output.join("controller-state"),
        "nodes": [{
            "id": "worker-a",
            "address": "192.0.2.10:22",
            "key_file": output.join("protocol.key"),
            "slots": 1,
            "templates": ["sum-v1", "control-v1", "project-work-v1"],
            "admitted": true,
            "healthy": false,
            "draining": false
        }]
    })).unwrap()).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&controller_target, &controller_path).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&controller_target, &controller_path).unwrap();
    let gateway_path = output.join("gateway.json");
    std::fs::write(&gateway_path, serde_json::to_vec(&json!({
        "schema": "native-ssh-gateway/1",
        "deployment": "ledger-fixture",
        "worker_id": "worker-a",
        "host": "192.0.2.10",
        "port": 22,
        "user": "operator",
        "known_hosts": output.join("known_hosts"),
        "identity_file": output.join("ordinary-user-rsa"),
        "controller_config": controller_path,
        "key_file": output.join("protocol.key"),
        "state_dir": output.join("gateway-state"),
        "remote_worker": "/home/operator/cross-runtime-fixture/aw-cli",
        "remote_config": "/home/operator/cross-runtime-fixture/worker.json",
        "artifact_sha256": "a".repeat(64),
        "artifact_bytes": 6,
        "timeout_secs": 30
    })).unwrap()).unwrap();
    let ledger = json!({
        "schema": "worker-provision-ledger/1",
        "state": "connected",
        "output_dir": output,
        "gateway_path": gateway_path,
        "worker_id": "worker-a",
        "host": "192.0.2.10",
        "artifact_sha256": "a".repeat(64),
        "artifact_bytes": 6,
        "created_at": 1,
        "updated_at": 2,
        "failure_code": null,
        "failure_next_step": null,
        "attempt_id": null,
        "remote_root": null,
        "remote_side_effects": null
    });
    std::fs::write(root.path().join("provisioning-ledger.json"), serde_json::to_vec(&ledger).unwrap()).unwrap();
    let error = ProvisionLedger::load(root.path()).unwrap_err();
    assert!(error.is_blocked(), "controller reparse path must be blocked: {error:?}");
}

#[cfg(windows)]
#[test]
fn ledger_reparse_record_is_blocked_before_opening_the_ledger() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("ledger-target");
    std::fs::create_dir(&target).unwrap();
    let ledger_path = root.path().join("provisioning-ledger.json");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&ledger_path)
        .arg(&target)
        .status()
        .unwrap();
    assert!(status.success(), "junction creation failed: {status}");

    let error = ProvisionLedger::load(root.path()).unwrap_err();
    assert!(error.is_blocked(), "ledger reparse path must be blocked before open: {error:?}");
    assert!(
        error.to_string().contains("provisioning path contains"),
        "ledger reparse path must use the shared ancestor guard: {error:?}"
    );
}

#[test]
fn provisioning_context_does_not_silently_ignore_path_metadata_errors() {
    let root = tempfile::tempdir().unwrap();
    let invalid = root.path().join("invalid\0component");
    let error = agentranch::core::provision::ProvisioningContext::new(invalid)
        .unwrap_err();
    assert!(
        error.to_string().to_ascii_lowercase().contains("nul")
            || error.to_string().to_ascii_lowercase().contains("invalid")
            || error.to_string().contains("provisioning path"),
        "metadata failure must be surfaced: {error}"
    );
}

#[test]
fn stale_gateway_record_is_blocked_without_creating_store_or_ssh_locks() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("provisioning").join("worker-current");
    std::fs::create_dir_all(&output).unwrap();
    let stale = json!({
        "schema": "worker-provision-ledger/1",
        "state": "connected",
        "output_dir": output,
        "gateway_path": output.join("gateway.json"),
        "worker_id": "worker-a",
        "host": "192.0.2.10",
        "artifact_sha256": "a".repeat(64),
        "artifact_bytes": 6,
        "created_at": 1,
        "updated_at": 2,
        "failure_code": null,
        "failure_next_step": null,
        "attempt_id": null,
        "remote_root": null,
        "remote_side_effects": null
    });
    std::fs::write(root.path().join("provisioning-ledger.json"), serde_json::to_vec(&stale).unwrap()).unwrap();
    let error = ProvisionLedger::load(root.path()).unwrap_err();
    assert!(error.is_blocked(), "missing current gateway must be blocked: {error:?}");
    assert!(!root.path().join("controller.lock").exists());
    assert!(!root.path().join("identity.lock").exists());
}

#[allow(dead_code)]
fn _path_type_is_explicit(path: &Path) -> bool { path.is_absolute() }
