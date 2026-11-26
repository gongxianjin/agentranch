//! API-boundary regression for stage → role → node routing.
//!
//! These tests drive the public `Projects`/`Store` API directly (no egui, no
//! HTTP server): the same seam a separated front end would call. A role is a
//! backend contract (`ProjectStage::role` + `Config::node_for_role`), so the
//! mapping can be asserted without a live pipeline.
use agentranch::{
    config::{Config, NodeRole, NodeSpec},
    core::{
        project_work::{ProjectStage, ProjectWork},
        projects::Projects,
        service::UiCommand,
        store::Store,
    },
};
use serde_json::{json, Value};
use std::path::Path;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// Write one key file per role node. Keys must differ: `Store::open` rejects
/// duplicate key material across nodes.
fn write_role_keys(root: &Path) {
    for (index, id) in ["split-node", "dev-node", "test-node", "review-node"].iter().enumerate() {
        std::fs::write(root.join(format!("{id}.key")), format!("0{}", index + 1).repeat(32)).unwrap();
    }
}

fn role_config_json(root: &Path) -> Value {
    write_role_keys(root);
    #[cfg(windows)]
    let endpoint = format!(r"\\.\pipe\absent-role-routing-{}", uuid::Uuid::new_v4());
    #[cfg(not(windows))]
    let endpoint = root.join("absent-reason.sock").to_string_lossy().into_owned();
    json!({
        "schema": "ao-reason-config/1", "mode": "real", "role": "controller",
        "deployment": "role-routing-fixture", "state_dir": root.join("host"),
        "nodes": [
            {"id": "split-node", "address": "owned-split/runner", "key_file": "split-node.key",
             "slots": 1, "templates": ["project-work-v1"], "role": "split", "admitted": true},
            {"id": "dev-node", "address": "owned-dev/runner", "key_file": "dev-node.key",
             "slots": 1, "templates": ["project-work-v1"], "role": "develop", "admitted": true},
            {"id": "test-node", "address": "owned-test/runner", "key_file": "test-node.key",
             "slots": 1, "templates": ["project-work-v1"], "role": "test", "admitted": true},
            {"id": "review-node", "address": "owned-review/runner", "key_file": "review-node.key",
             "slots": 1, "templates": ["project-work-v1"], "role": "review", "admitted": true}
        ],
        "ao": {"base_url": "http://127.0.0.1:1", "project_id": "fixture", "model": "fixture-only"},
        "reason_endpoint": endpoint, "endpoint": "owned-A/controller"
    })
}

fn configured_projects(projects: &mut Projects, root: &Path) -> String {
    let path = root.join("controller.json");
    std::fs::write(&path, serde_json::to_vec(&role_config_json(root)).unwrap()).unwrap();
    projects.command(UiCommand::ProjectConfigure { config_path: path }).unwrap();
    let id = projects.command(UiCommand::ProjectCreate { name: "role routing".into() })
        .unwrap()["id"].as_str().unwrap().to_owned();
    let text = "# Reviewed requirements\nDeduct inventory atomically.\n";
    let doc = root.join("reviewed.md");
    std::fs::write(&doc, text).unwrap();
    projects.command(UiCommand::ProjectImport { project_id: id.clone(), path: doc }).unwrap();
    projects.command(UiCommand::ProjectConfirm {
        project_id: id.clone(), version: 1,
        sha256: hex::encode(<sha2::Sha256 as sha2::Digest>::digest(text.as_bytes())),
    }).unwrap();
    id
}

/// Read the durable pin the engine recorded for a project's split run. The pin
/// is what a separated front end observes: which role worker owns the stage.
fn split_pinned_node(root: &Path, project_id: &str) -> Option<String> {
    let cfg: Config = serde_json::from_slice(&std::fs::read(root.join("controller.json")).unwrap()).unwrap();
    let nodes: Vec<NodeSpec> = cfg.load_nodes(root).unwrap();
    let host = Store::open(&root.join("host/controller.sqlite"), "role-routing-fixture", &nodes).unwrap();
    let run = host.list_runs(50).unwrap().into_iter()
        .find(|run| run.detail["project_work"]["project_id"] == project_id)
        .expect("split run must be queued");
    let work: ProjectWork = serde_json::from_value(run.detail["project_work"].clone()).unwrap();
    assert_eq!(work.stage, ProjectStage::Split);
    run.detail.get("pinned_node").and_then(|value| value.as_str()).map(str::to_owned)
}

#[test]
fn every_stage_maps_to_exactly_one_fixed_role() {
    use ProjectStage::*;
    let cases = [
        (Split, NodeRole::Split), (Redo, NodeRole::Develop), (Develop, NodeRole::Develop),
        (Repair, NodeRole::Develop), (Test, NodeRole::Test),
        (ReviewArtifact, NodeRole::Review), (ReviewEvidence, NodeRole::Review),
    ];
    for (stage, role) in cases {
        assert_eq!(stage.role(), role, "stage {stage:?} must run on {role:?}");
    }
}

#[test]
fn config_binds_each_role_to_its_own_node() {
    let root = tempfile::tempdir().unwrap();
    let cfg: Config = serde_json::from_value(role_config_json(root.path())).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.node_for_role(NodeRole::Split), Some("split-node"));
    assert_eq!(cfg.node_for_role(NodeRole::Develop), Some("dev-node"));
    assert_eq!(cfg.node_for_role(NodeRole::Test), Some("test-node"));
    assert_eq!(cfg.node_for_role(NodeRole::Review), Some("review-node"));
}

#[test]
fn split_dispatch_is_pinned_to_the_split_role_node() {
    let root = tempfile::tempdir().unwrap();
    let mut projects = Projects::open(&root.path().join("workflow")).unwrap();
    let id = configured_projects(&mut projects, root.path());
    // The engine records the split role's node as the only eligible worker.
    projects.command(UiCommand::ProjectSplit { project_id: id.clone() }).unwrap();
    assert_eq!(split_pinned_node(root.path(), &id).as_deref(), Some("split-node"), "split must be pinned to the split role worker");
    let _ = now();
}

#[test]
fn an_unready_pinned_node_keeps_the_run_queued_instead_of_running_elsewhere() {
    let root = tempfile::tempdir().unwrap();
    let mut projects = Projects::open(&root.path().join("workflow")).unwrap();
    let id = configured_projects(&mut projects, root.path());
    projects.command(UiCommand::ProjectSplit { project_id: id.clone() }).unwrap();

    // All fixture nodes start unready (healthy=false in a real-mode config), so
    // load-based assignment must not fall through to a different role's node.
    let cfg: Config = serde_json::from_slice(&std::fs::read(root.path().join("controller.json")).unwrap()).unwrap();
    let nodes: Vec<NodeSpec> = cfg.load_nodes(root.path()).unwrap();
    let mut host = Store::open(&root.path().join("host/controller.sqlite"), "role-routing-fixture", &nodes).unwrap();
    assert!(host.assign_next(now()).unwrap().is_none(), "an unready pin must not be dispatched to any node");
}

#[test]
fn duplicate_role_in_config_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.key"), "07".repeat(32)).unwrap();
    std::fs::write(root.path().join("b.key"), "08".repeat(32)).unwrap();
    let config = json!({
        "schema": "ao-reason-config/1", "mode": "real", "role": "controller",
        "deployment": "dup-role", "state_dir": root.path().join("host"),
        "nodes": [
            {"id": "a", "address": "owned-a/runner", "key_file": "a.key",
             "slots": 1, "templates": ["project-work-v1"], "role": "develop"},
            {"id": "b", "address": "owned-b/runner", "key_file": "b.key",
             "slots": 1, "templates": ["project-work-v1"], "role": "develop"}
        ]
    });
    let typed: Config = serde_json::from_value(config).unwrap();
    let error = typed.validate().unwrap_err();
    assert!(error.to_string().contains("duplicate node role"), "{error:#}");
}

#[test]
fn config_without_roles_still_validates_and_keeps_load_based_assignment() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("b.key"), "07".repeat(32)).unwrap();
    let config = json!({
        "schema": "ao-reason-config/1", "mode": "real", "role": "controller",
        "deployment": "legacy", "state_dir": root.path().join("host"),
        "nodes": [{"id": "B", "address": "owned-B/runner", "key_file": "b.key",
                   "slots": 1, "templates": ["project-work-v1"], "admitted": true}]
    });
    let typed: Config = serde_json::from_value(config).unwrap();
    typed.validate().expect("role-less config must remain valid");
    assert_eq!(typed.node_for_role(NodeRole::Develop), None);
}
