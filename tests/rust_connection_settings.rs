use agentranch::connection_settings::{ConfiguredWorker, ConnectionSettings, ConnectionSettingsError, ConnectionSettingsStore, ModelSettings, WorkerRole};

fn worker(id: &str, host: &str, role: WorkerRole, password: &str) -> ConfiguredWorker {
    ConfiguredWorker { id: id.into(), role, host: host.into(), username: "root".into(), password: password.into() }
}

fn settings() -> ConnectionSettings {
    ConnectionSettings {
        schema: 2,
        model: ModelSettings { base_url: "https://model.example".into(), api_key: "synthetic-api-key".into(), model: "test-model".into() },
        workers: vec![worker("worker-1", "192.0.2.10", WorkerRole::Split, "synthetic-password-1")],
    }
}

fn four_roles() -> ConnectionSettings {
    let mut input = settings();
    input.workers = vec![
        worker("worker-split", "192.0.2.10", WorkerRole::Split, "synthetic-password-1"),
        worker("worker-develop", "192.0.2.11", WorkerRole::Develop, "synthetic-password-2"),
        worker("worker-test", "192.0.2.12", WorkerRole::Test, "synthetic-password-3"),
        worker("worker-review", "192.0.2.13", WorkerRole::Review, "synthetic-password-4"),
    ];
    input
}

#[test]
fn plaintext_settings_restore_model_and_every_role_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let input = four_roles();
    ConnectionSettingsStore::new(root.path()).save(&input).unwrap();
    let path = root.path().join("connection-settings.json");
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["model"]["api_key"], "synthetic-api-key");
    assert_eq!(saved["workers"][0]["password"], "synthetic-password-1");
    assert_eq!(saved["workers"][0]["role"], "split");
    assert_eq!(saved["workers"][3]["role"], "review");
    assert_eq!(saved["workers"][3]["password"], "synthetic-password-4");
    let restored = ConnectionSettingsStore::new(root.path()).load().unwrap().unwrap();
    assert!(restored == input, "restart must restore all four roles without a vault");
}

#[test]
fn appended_worker_persists_without_a_fixed_upper_bound() {
    let root = tempfile::tempdir().unwrap();
    let store = ConnectionSettingsStore::new(root.path());
    let mut input = settings();
    input.workers.push(worker("worker-2", "192.0.2.11", WorkerRole::Develop, "synthetic-password-2"));
    store.save(&input).unwrap();
    let restored = store.load().unwrap().unwrap();
    assert_eq!(restored.workers.len(), 2, "single-worker save must not drop an appended worker");
    assert_eq!(restored.workers[1].role, WorkerRole::Develop);
}

#[test]
fn empty_worker_list_and_invalid_replacements_preserve_previous_configuration() {
    let root = tempfile::tempdir().unwrap();
    let store = ConnectionSettingsStore::new(root.path());
    store.save(&settings()).unwrap();
    let path = root.path().join("connection-settings.json");
    let before = std::fs::read(&path).unwrap();
    let mut invalid = settings();
    invalid.workers.clear();
    assert!(matches!(store.save(&invalid), Err(ConnectionSettingsError::InvalidInput)));
    let mut invalid = settings();
    invalid.workers.push(invalid.workers[0].clone());
    assert!(matches!(store.save(&invalid), Err(ConnectionSettingsError::InvalidInput)), "duplicate worker id must be rejected");
    let mut invalid = settings();
    invalid.workers.push(worker("worker-2", "192.0.2.11", WorkerRole::Split, "synthetic-password-2"));
    assert!(matches!(store.save(&invalid), Err(ConnectionSettingsError::InvalidInput)), "a role may only be assigned once");
    let mut invalid = settings();
    invalid.workers.push(worker("worker-2", "192.0.2.10", WorkerRole::Develop, "synthetic-password-2"));
    assert!(matches!(store.save(&invalid), Err(ConnectionSettingsError::InvalidInput)), "duplicate host must not be prepared twice");
    let mut invalid = settings();
    invalid.workers[0].password.clear();
    assert!(store.save(&invalid).is_err());
    let mut invalid = settings();
    invalid.model.base_url = "https://user:synthetic-password@example.test/path".into();
    assert!(store.save(&invalid).is_err());
    let mut invalid = settings();
    invalid.schema = 999;
    assert!(store.save(&invalid).is_err());
    let mut invalid = settings();
    invalid.schema = 1;
    assert!(store.save(&invalid).is_err(), "v1 files must be migrated, not silently accepted");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn missing_or_corrupt_configuration_is_read_only_and_never_falls_back_to_defaults() {
    let root = tempfile::tempdir().unwrap();
    let store = ConnectionSettingsStore::new(root.path());
    assert!(store.load().unwrap().is_none());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    let path = root.path().join("connection-settings.json");
    for bytes in [b"not json".to_vec(), vec![b' '; 1_048_577], b"{\"schema\":999}".to_vec()] {
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(store.load(), Err(ConnectionSettingsError::InvalidFile)));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn malformed_worker_and_oversized_secrets_are_rejected_without_creating_a_file() {
    let root = tempfile::tempdir().unwrap();
    let store = ConnectionSettingsStore::new(root.path());
    for alter in 0..7 {
        let mut invalid = settings();
        match alter {
            0 => invalid.workers[0].host = "not-an-ip".into(),
            1 => invalid.workers[0].id = "../worker".into(),
            2 => invalid.workers[0].username = "bad user".into(),
            3 => invalid.workers[0].password = "x".repeat(16_385),
            4 => invalid.model.api_key = "x".repeat(16_385),
            5 => invalid.model.api_key.clear(),
            _ => invalid.model.model.clear(),
        }
        assert!(store.save(&invalid).is_err(), "invalid case {alter} must not save");
    }
    assert!(!root.path().join("connection-settings.json").exists());
}

#[test]
fn explicit_plaintext_save_does_not_modify_or_delete_legacy_settings() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("worker-settings.json");
    std::fs::write(&legacy, b"LEGACY_SENTINEL").unwrap();
    ConnectionSettingsStore::new(root.path()).save(&settings()).unwrap();
    assert_eq!(std::fs::read(legacy).unwrap(), b"LEGACY_SENTINEL");
    let names: Vec<_> = std::fs::read_dir(root.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names.len(), 2, "atomic write must leave no temporary files");
}

#[test]
fn plaintext_persistence_does_not_enable_secret_debug_logging() {
    let value = four_roles();
    let debug = format!("{value:?}");
    for secret in ["synthetic-api-key", "synthetic-password-1", "synthetic-password-4"] {
        assert!(!debug.contains(secret), "Debug leaked a secret: {debug}");
    }
    assert!(!format!("{:?}", value.model).contains("synthetic-api-key"));
    assert!(!format!("{:?}", value.workers[2]).contains("synthetic-password-3"));
}

#[test]
fn each_role_has_a_distinct_human_label() {
    let labels: Vec<_> = WorkerRole::ALL.iter().map(|role| role.label()).collect();
    assert_eq!(labels, ["拆分", "开发", "测试", "审查"]);
    let wire: Vec<_> = WorkerRole::ALL.iter().map(|role| role.as_str()).collect();
    assert_eq!(wire, ["split", "develop", "test", "review"]);
}
