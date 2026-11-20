#![cfg(feature = "gui")]
#[path = "../src/desktop/worker_settings.rs"]
mod worker_settings;
use worker_settings::{CredentialVault, SettingsError, SettingsStore, WorkerSettings};
use std::{collections::HashMap, sync::{Arc, Mutex}};

#[derive(Clone, Default)]
struct Vault(Arc<Mutex<HashMap<String, String>>>);
impl CredentialVault for Vault {
    fn read(&self, target: &str) -> Result<Option<String>, SettingsError> { Ok(self.0.lock().unwrap().get(target).cloned()) }
    fn write(&self, target: &str, token: &str) -> Result<(), SettingsError> { self.0.lock().unwrap().insert(target.into(), token.into()); Ok(()) }
    fn delete(&self, target: &str) -> Result<(), SettingsError> { self.0.lock().unwrap().remove(target); Ok(()) }
}
fn fields() -> WorkerSettings {
    WorkerSettings { host: "192.0.2.10".into(), root_user: "root".into(), model_base_url: "https://model.example".into(), model: "test-model".into() }
}

#[test]
fn restart_restores_four_fields_and_vault_key_but_file_contains_no_secrets() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::default();
    let store = SettingsStore::with_vault(root.path(), Box::new(vault.clone()));
    store.save(&fields(), "synthetic-api-secret").unwrap();
    let body = std::fs::read_to_string(root.path().join("worker-settings.json")).unwrap();
    assert!(!body.contains("synthetic-api-secret"));
    assert!(!body.contains("password"));
    assert!(!body.contains("model_token"));
    let next = SettingsStore::with_vault(root.path(), Box::new(vault));
    let restored = next.load().unwrap().unwrap();
    assert_eq!(restored.settings, fields());
    assert!(restored.token.as_deref() == Ok("synthetic-api-secret"));
}

#[test]
fn vault_failure_preserves_previous_settings_and_returns_safe_error() {
    struct Failing;
    impl CredentialVault for Failing {
        fn read(&self, _: &str) -> Result<Option<String>, SettingsError> { Err(SettingsError::Vault) }
        fn write(&self, _: &str, _: &str) -> Result<(), SettingsError> { Err(SettingsError::Vault) }
        fn delete(&self, _: &str) -> Result<(), SettingsError> { Err(SettingsError::Vault) }
    }
    let root = tempfile::tempdir().unwrap();
    let store = SettingsStore::with_vault(root.path(), Box::new(Vault::default()));
    store.save(&fields(), "synthetic-old-secret").unwrap();
    let before = std::fs::read(root.path().join("worker-settings.json")).unwrap();
    let failed = SettingsStore::with_vault(root.path(), Box::new(Failing));
    let error = failed.save(&fields(), "synthetic-new-secret").unwrap_err();
    assert_eq!(error, SettingsError::Vault);
    assert!(!error.to_string().contains("synthetic"));
    assert_eq!(std::fs::read(root.path().join("worker-settings.json")).unwrap(), before);
}

#[test]
fn settings_commit_failure_removes_new_vault_key() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("worker-settings.json")).unwrap();
    let vault = Vault::default();
    let store = SettingsStore::with_vault(root.path(), Box::new(vault.clone()));
    assert!(store.save(&fields(), "synthetic-secret").is_err());
    assert!(vault.0.lock().unwrap().is_empty());
}

#[test]
fn replacing_settings_keeps_only_the_current_owned_credential() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::default();
    let store = SettingsStore::with_vault(root.path(), Box::new(vault.clone()));
    store.save(&fields(), "synthetic-old-secret").unwrap();
    let mut changed = fields(); changed.model = "next-model".into();
    store.save(&changed, "synthetic-new-secret").unwrap();
    assert_eq!(vault.0.lock().unwrap().len(), 1);
    let restored = store.load().unwrap().unwrap();
    assert_eq!(restored.settings, changed);
    assert!(restored.token.as_deref() == Ok("synthetic-new-secret"));
}

#[test]
fn missing_vault_key_is_a_credential_error_not_a_successful_key_restore() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::default();
    let store = SettingsStore::with_vault(root.path(), Box::new(vault.clone()));
    store.save(&fields(), "synthetic-secret").unwrap();
    vault.0.lock().unwrap().clear();
    assert!(matches!(store.load().unwrap().unwrap().token, Err(SettingsError::MissingCredential)));
}

#[test]
fn foreign_reference_and_unknown_schema_are_rejected_without_vault_access() {
    struct NeverRead;
    impl CredentialVault for NeverRead {
        fn read(&self, _: &str) -> Result<Option<String>, SettingsError> { panic!("must reject before vault read") }
        fn write(&self, _: &str, _: &str) -> Result<(), SettingsError> { panic!("no write") }
        fn delete(&self, _: &str) -> Result<(), SettingsError> { panic!("no delete") }
    }
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::default();
    let source = SettingsStore::with_vault(root.path(), Box::new(vault));
    source.save(&fields(), "synthetic-secret").unwrap();
    let path = root.path().join("worker-settings.json");
    let original: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (field, value) in [("credential_target", serde_json::json!("unrelated-user-credential")), ("schema", serde_json::json!(999))] {
        let mut edited = original.clone(); edited[field] = value;
        std::fs::write(&path, serde_json::to_vec(&edited).unwrap()).unwrap();
        let store = SettingsStore::with_vault(root.path(), Box::new(NeverRead));
        assert!(matches!(store.load(), Err(SettingsError::InvalidFile)));
    }
}

#[test]
fn empty_and_oversized_keys_and_invalid_origins_never_reach_vault() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::default();
    let store = SettingsStore::with_vault(root.path(), Box::new(vault.clone()));
    for token in [String::new(), "x".repeat(2561)] { assert!(store.save(&fields(), &token).is_err()); }
    let mut invalid = fields(); invalid.model_base_url = "https://user:secret@example.org/path".into();
    assert!(store.save(&invalid, "synthetic-secret").is_err());
    assert!(vault.0.lock().unwrap().is_empty());
    assert!(!root.path().join("worker-settings.json").exists());
}

#[test]
fn editing_saved_origin_cannot_reuse_another_origins_key() {
    let root = tempfile::tempdir().unwrap();
    let store = SettingsStore::with_vault(root.path(), Box::new(Vault::default()));
    store.save(&fields(), "synthetic-secret").unwrap();
    let path = root.path().join("worker-settings.json");
    let mut edited: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edited["settings"]["model_base_url"] = serde_json::json!("https://other.example");
    std::fs::write(&path, serde_json::to_vec(&edited).unwrap()).unwrap();
    assert!(matches!(store.load(), Err(SettingsError::InvalidFile)));
}

#[test]
fn cleanup_failure_reports_committed_settings_without_losing_current_key() {
    struct CleanupFailure(Vault);
    impl CredentialVault for CleanupFailure {
        fn read(&self, target: &str) -> Result<Option<String>, SettingsError> { self.0.read(target) }
        fn write(&self, target: &str, token: &str) -> Result<(), SettingsError> { self.0.write(target, token) }
        fn delete(&self, _: &str) -> Result<(), SettingsError> { Err(SettingsError::Vault) }
    }
    let root = tempfile::tempdir().unwrap();
    let store = SettingsStore::with_vault(root.path(), Box::new(CleanupFailure(Vault::default())));
    store.save(&fields(), "synthetic-old").unwrap();
    let error = store.save(&fields(), "synthetic-new").unwrap_err();
    assert!(error.to_string().contains("配置已保存，但旧凭据未能清理"));
    assert!(store.load().unwrap().unwrap().token.as_deref() == Ok("synthetic-new"));
}

// A credential failure must not discard already validated ordinary fields.
// Load is read-only; these failures must never rewrite JSON or Vault entries.
#[test]
fn credential_failures_preserve_validated_fields_without_storage_mutation() {
    for token in [None, Some(String::new()), Some("x".repeat(2561))] {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::default();
        let store = SettingsStore::with_vault(root.path(), Box::new(vault.clone()));
        store.save(&fields(), "synthetic-secret").unwrap();
        let path = root.path().join("worker-settings.json");
        let before = std::fs::read(&path).unwrap();
        let target = vault.0.lock().unwrap().keys().next().unwrap().clone();
        match token {
            Some(token) => { vault.0.lock().unwrap().insert(target, token); },
            None => vault.0.lock().unwrap().clear(),
        }
        let vault_before = vault.0.lock().unwrap().clone();
        let restored = store.load().expect("valid ordinary configuration must survive unusable key").unwrap();
        assert_eq!(restored.settings, fields());
        assert!(matches!(restored.token, Err(SettingsError::MissingCredential)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(*vault.0.lock().unwrap(), vault_before);
    }
}

#[test]
fn vault_read_failure_preserves_validated_fields_without_writes() {
    struct ReadFailure;
    impl CredentialVault for ReadFailure {
        fn read(&self, _: &str) -> Result<Option<String>, SettingsError> { Err(SettingsError::Vault) }
        fn write(&self, _: &str, _: &str) -> Result<(), SettingsError> { panic!("restore must not write") }
        fn delete(&self, _: &str) -> Result<(), SettingsError> { panic!("restore must not delete") }
    }
    let root = tempfile::tempdir().unwrap();
    SettingsStore::with_vault(root.path(), Box::new(Vault::default())).save(&fields(), "synthetic-secret").unwrap();
    let path = root.path().join("worker-settings.json");
    let before = std::fs::read(&path).unwrap();
    let store = SettingsStore::with_vault(root.path(), Box::new(ReadFailure));
    let restored = store.load().expect("valid ordinary configuration must survive Vault failure").unwrap();
    assert_eq!(restored.settings, fields());
    assert!(matches!(restored.token, Err(SettingsError::Vault)));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn invalid_configuration_never_partially_restores_or_reads_vault() {
    struct NeverAccess;
    impl CredentialVault for NeverAccess {
        fn read(&self, _: &str) -> Result<Option<String>, SettingsError> { panic!("validate before credential access") }
        fn write(&self, _: &str, _: &str) -> Result<(), SettingsError> { panic!("no write") }
        fn delete(&self, _: &str) -> Result<(), SettingsError> { panic!("no delete") }
    }
    let root = tempfile::tempdir().unwrap();
    SettingsStore::with_vault(root.path(), Box::new(Vault::default())).save(&fields(), "synthetic-secret").unwrap();
    let path = root.path().join("worker-settings.json");
    let original: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut cases = vec![b"not json".to_vec(), vec![b' '; 16 * 1024 + 1]];
    for (pointer, value) in [
        ("/schema", serde_json::json!(999)),
        ("/credential_target", serde_json::json!("foreign-user-key")),
        ("/settings/host", serde_json::json!("invalid-host")),
        ("/settings/root_user", serde_json::json!("bad user")),
        ("/settings/model_base_url", serde_json::json!("https://other.example")),
        ("/settings/model", serde_json::json!("")),
    ] {
        let mut edited = original.clone();
        *edited.pointer_mut(pointer).unwrap() = value;
        cases.push(serde_json::to_vec(&edited).unwrap());
    }
    let foreign = tempfile::tempdir().unwrap();
    std::fs::write(foreign.path().join("worker-settings.json"), serde_json::to_vec(&original).unwrap()).unwrap();
    assert!(matches!(SettingsStore::with_vault(foreign.path(), Box::new(NeverAccess)).load(), Err(SettingsError::InvalidFile)));
    for body in cases {
        std::fs::write(&path, &body).unwrap();
        assert!(matches!(SettingsStore::with_vault(root.path(), Box::new(NeverAccess)).load(), Err(SettingsError::InvalidFile)));
        assert_eq!(std::fs::read(&path).unwrap(), body);
    }
}

#[cfg(windows)]
#[test]
fn windows_credential_manager_synthetic_roundtrip_and_cleanup() {
    use worker_settings::WindowsVault;
    let vault = WindowsVault;
    let target = format!("agentranch/test/{}", uuid::Uuid::new_v4());
    struct Cleanup(String);
    impl Drop for Cleanup { fn drop(&mut self) { let _ = WindowsVault.delete(&self.0); } }
    let _cleanup = Cleanup(target.clone());
    assert!(vault.read(&target).unwrap().is_none());
    vault.write(&target, "synthetic-only-not-a-real-api-key").unwrap();
    assert!(vault.read(&target).unwrap().as_deref() == Some("synthetic-only-not-a-real-api-key"));
    vault.delete(&target).unwrap();
    assert!(vault.read(&target).unwrap().is_none());
}
