#[path = "support/daemon_temp.rs"]
mod daemon_temp;

use agentranch::api::contract::{decode_command, encode_command, CommandResponse};
use agentranch::config::Mode;
use agentranch::core::service::{AppSnapshot, CommandResult, ProvisionRequest, ServiceHandle, ServiceOptions, SubmitError, UiCommand};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, thread, time::{Duration, Instant}};

fn command_samples() -> Vec<Value> {
    vec![
        json!({"kind":"gateway_configure","args":{"config_path":"gateway.json"}}),
        json!({"kind":"gateway_dispatch","args":{"worker_id":"worker-a"}}),
        json!({"kind":"gateway_dispatch_project","args":{"worker_id":"worker-a","doc_path":"design.md"}}),
        json!({"kind":"project_work_submit","args":{"project_id":"00000000-0000-4000-8000-000000000001"}}),
        json!({"kind":"provision_worker","args":{"host":"192.0.2.10","root_user":"root","root_password":"synthetic-root-password","model_base_url":"https://model.example","model_token":"synthetic-model-token","model":"gpt-test"}}),
        json!({"kind":"start_local","args":{}}),
        json!({"kind":"dispatch","args":{"template":"sum-v1"}}),
        json!({"kind":"status","args":{"run_id":"run-1"}}),
        json!({"kind":"approvals","args":{"run_id":"run-1"}}),
        json!({"kind":"decide","args":{"run_id":"run-1","attempt":1,"session":"session-1","request_id":"request-1","generation":2,"scope_hash":"scope","decision_id":"allow_once"}}),
        json!({"kind":"report","args":{"run_id":"run-1"}}),
        json!({"kind":"retry","args":{"run_id":"run-1","attempt":1}}),
        json!({"kind":"cancel","args":{"run_id":"run-1"}}),
        json!({"kind":"project_create","args":{"name":"demo"}}),
        json!({"kind":"project_import","args":{"project_id":"00000000-0000-4000-8000-000000000001","path":"design.md"}}),
        json!({"kind":"project_confirm","args":{"project_id":"00000000-0000-4000-8000-000000000001","version":1,"sha256":"abc"}}),
        json!({"kind":"project_split","args":{"project_id":"00000000-0000-4000-8000-000000000001"}}),
        json!({"kind":"task_review","args":{"project_id":"00000000-0000-4000-8000-000000000001","task_id":"task-1","approve":true,"reason":""}}),
        json!({"kind":"task_redo","args":{"project_id":"00000000-0000-4000-8000-000000000001","task_id":"task-1"}}),
        json!({"kind":"project_start","args":{"project_id":"00000000-0000-4000-8000-000000000001"}}),
        json!({"kind":"project_test","args":{"project_id":"00000000-0000-4000-8000-000000000001"}}),
        json!({"kind":"project_repair","args":{"project_id":"00000000-0000-4000-8000-000000000001","reason":"repair"}}),
        json!({"kind":"project_accept","args":{"project_id":"00000000-0000-4000-8000-000000000001"}}),
        json!({"kind":"project_refresh","args":{"project_id":"00000000-0000-4000-8000-000000000001"}}),
        json!({"kind":"project_configure","args":{"config_path":"project.json"}}),
    ]
}

#[test]
fn all_v1_commands_round_trip_with_canonical_args() {
    let samples = command_samples();
    assert_eq!(samples.len(), 25);
    let mut kinds = std::collections::BTreeSet::new();
    for sample in samples {
        let command = decode_command(&serde_json::to_vec(&sample).unwrap()).unwrap();
        let encoded: Value = serde_json::from_slice(&encode_command(&command).unwrap()).unwrap();
        kinds.insert(sample["kind"].as_str().unwrap().to_owned());
        assert_eq!(encoded, sample, "wire round-trip changed {}", sample["kind"]);
    }
    assert_eq!(kinds.len(), 25);
}

#[test]
fn invalid_requests_return_fixed_errors() {
    let secret = "synthetic-secret-do-not-echo";
    let invalid = [
        json!({"kind":"not_a_command","args":{}}),
        json!({"kind":"start_local","args":[]}),
        json!({"kind":"start_local"}),
        json!({"kind":"start_local","args":{},"extra":"nope"}),
        json!({"kind":"status","args":{}}),
        json!({"kind":"status","args":{"run_id":"run-1","extra":"nope"}}),
        json!({"kind":"status","args":{"run_id":7}}),
        json!({"kind":"start_local","args":{"unexpected":true}}),
        json!(["start_local", {}]),
        json!({"kind":"status","args":["run-1"]}),
        json!({"kind":"provision_worker","args":{"host":"192.0.2.10","root_user":"root","root_password":secret,"model_base_url":"https://model.example","model_token":secret,"model":1}}),
    ];
    for (index, sample) in invalid.into_iter().enumerate() {
        let error = decode_command(&serde_json::to_vec(&sample).unwrap())
            .expect_err(&format!("expected rejection for invalid fixture {index}"));
        assert_eq!(error.code, "bad_request");
        assert!(!error.message.contains(secret), "invalid request leaked secret: {}", error.message);
        assert!(!error.next_step.contains(secret));
    }
    let malformed: &[&[u8]] = &[
        br#"{"kind":"status","args":{"run_id":"run-1","run_id":"secret"}}"#,
        br#"{"kind":"status","kind":"status","args":{"run_id":"run-1"}}"#,
        br#"{"kind":"start_local","args":{},"args":{}}"#,
        br#"{"kind":"status","args":{"run_id":"secret""#,
        b"{\"kind\":\"status\",\"args\":{\"run_id\":\"\xff\"}}",
    ];
    for (index, sample) in malformed.iter().enumerate() {
        let error = decode_command(sample).expect_err(&format!("expected rejection for raw fixture {index}"));
        assert_eq!(error.code, "bad_request");
        assert!(!error.message.contains("secret"));
        assert!(!error.next_step.contains("secret"));
    }

    let request = ProvisionRequest {
        host: "192.0.2.10".into(), root_user: "root".into(),
        root_password: secret.as_bytes().to_vec(), model_base_url: "https://model.example".into(),
        model_token: secret.as_bytes().to_vec(), model: "gpt-test".into(),
    };
    let debug = format!("{request:?}");
    assert!(!debug.contains(secret));
}

#[test]
fn command_response_rejects_ambiguous_result_and_error() {
    let wire = json!({
        "request_id": 7,
        "result": null,
        "error": {"code": "fixture_error", "message": "safe", "next_step": "inspect"}
    });
    assert!(serde_json::from_value::<CommandResponse>(wire).is_err());
}

#[test]
fn non_empty_snapshot_views_round_trip_without_field_loss() {
    let fixture = json!({
        "mode": "real",
        "role": "controller",
        "readiness": "ready",
        "actor": {"kind": "local", "id": "controller"},
        "observed_at": 42,
        "running": true,
        "dependencies": [{"id": "reason", "state": "ready", "detail": "ok", "source": "fixture", "observed_at": 42}],
        "runs": [{
            "run": {"id": "run-1", "owner": {"kind": "local", "id": "controller"}, "template": "sum-v1", "status": "running", "attempt": 1, "epoch": "epoch-1", "session": "session-1", "spawn_intent": true, "deadline": 99, "detail": {"source": "fixture"}},
            "read_only": false,
            "can_status": true,
            "can_approvals": true,
            "can_decide": true,
            "can_report": true,
            "can_retry": true,
            "can_cancel": true,
            "disabled_reason": null
        }],
        "nodes": [{"id": "worker-a", "address": "192.0.2.10:22", "used": 1, "slots": 2, "admitted": true, "healthy": true, "draining": false, "source": "fixture", "observed_at": 42}],
        "approvals": [{
            "run_id": "run-1", "attempt": 1, "session": "session-1", "request_id": "request-1", "generation": 2, "scope_hash": "scope", "expires_at": 100,
            "offered": [{"id": "allow_once", "kind": "AllowOnce", "label": "Allow once"}],
            "summary": "fixture approval", "simulated": false, "can_decide": true
        }],
        "errors": [{"code": "fixture_error", "message": "safe", "next_step": "inspect"}],
        "unfinished": 1,
        "projects": [],
        "gateway": {"worker_id": "worker-a", "host": "192.0.2.10", "remote_verified": true, "preflight_passed": true, "credentials_available": false, "used_slots": 1, "artifact_sha256": "a".repeat(64)},
        "provision_ledger": {
            "schema": "worker-provision-ledger/1", "state": "failed", "output_dir": "C:/state/output", "gateway_path": "C:/state/gateway.json",
            "worker_id": "worker-a", "host": "192.0.2.10", "artifact_sha256": "b".repeat(64), "artifact_bytes": 7,
            "created_at": 1, "updated_at": 2, "failure_code": "provision_error", "failure_next_step": "retry", "attempt_id": "attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "remote_root": "/home/operator/cross-runtime-fixture", "remote_side_effects": "absent"
        }
    });
    let snapshot: AppSnapshot = serde_json::from_value(fixture.clone()).unwrap();
    assert_eq!(serde_json::to_value(snapshot).unwrap(), fixture);
}

fn options(state_dir: PathBuf) -> ServiceOptions {
    ServiceOptions {
        mode: Mode::Real,
        state_dir,
        worker_exe: PathBuf::from(env!("CARGO_BIN_EXE_aw-cli")),
        gateway_key: None,
        gateway_model_token: None,
    }
}

fn wait_result(handle: &mut ServiceHandle, request_id: u64) -> CommandResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(result) = handle.drain_results().into_iter().find(|result| result.request_id == request_id) {
            return result;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("request {request_id} did not produce a result")
}

#[test]
fn result_and_snapshot_round_trip_without_changing_the_projection() {
    let temp = daemon_temp::temp_root();
    let mut handle = ServiceHandle::spawn(options(temp.path().join("real")), Arc::new(|| {})).unwrap();
    let create = handle.try_submit(UiCommand::ProjectCreate { name: "wire-demo".into() }).unwrap();
    let created = wait_result(&mut handle, create);
    let created_body = created.result.as_ref().unwrap();
    let project_id = created_body["id"].as_str().unwrap().to_owned();
    let success = CommandResponse::from(created.clone());
    let success_wire = serde_json::to_value(&success).unwrap();
    assert_eq!(success_wire, json!({"request_id": create, "result": created_body}));
    assert_eq!(serde_json::from_value::<CommandResponse>(success_wire).unwrap(), success);

    let document = temp.path().join("wire.md");
    let text = "# Wire snapshot\n\n投影往返\n";
    std::fs::write(&document, text).unwrap();
    let import = handle.try_submit(UiCommand::ProjectImport { project_id: project_id.clone(), path: document }).unwrap();
    let imported = wait_result(&mut handle, import).result.unwrap();
    use sha2::{Digest, Sha256};
    let sha256 = hex::encode(Sha256::digest(text.as_bytes()));
    assert_eq!(imported["document"]["sha256"], sha256);
    let confirm = handle.try_submit(UiCommand::ProjectConfirm { project_id: project_id.clone(), version: 1, sha256: sha256.clone() }).unwrap();
    assert!(wait_result(&mut handle, confirm).result.is_ok());

    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        let snapshot = handle.snapshot();
        if snapshot.projects.iter().any(|project| {
            project.id == project_id && project.document.as_ref().is_some_and(|doc| doc.confirmed)
        }) {
            break snapshot;
        }
        assert!(Instant::now() < deadline, "confirmed document was not published");
        thread::sleep(Duration::from_millis(10));
    };
    let snapshot_json = serde_json::to_value(&*snapshot).unwrap();
    let project = snapshot_json["projects"].as_array().unwrap().iter().find(|project| project["id"] == project_id).unwrap();
    assert_eq!(project["document"], json!({
        "version": 1, "filename": "wire.md", "sha256": sha256, "text": text, "confirmed": true
    }));
    let decoded: AppSnapshot = serde_json::from_value(snapshot_json.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), snapshot_json);

    let invalid = handle.try_submit(UiCommand::ProjectConfirm { project_id, version: 99, sha256: "wrong".into() }).unwrap();
    let failure = wait_result(&mut handle, invalid);
    let failure_body = failure.result.as_ref().unwrap_err();
    let response = CommandResponse::from(failure.clone());
    let wire = serde_json::to_value(&response).unwrap();
    assert_eq!(wire, json!({"request_id": invalid, "error": failure_body}));
    assert_eq!(serde_json::from_value::<CommandResponse>(wire).unwrap(), response);

    handle.disconnect();
    assert_eq!(handle.try_submit(UiCommand::StartLocal), Err(SubmitError::Stopped));
}
