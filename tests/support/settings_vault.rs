//! Synthetic in-process vault for UI tests: never accesses OS credentials.
use super::worker_settings::{CredentialVault, SettingsError};
use std::{collections::HashMap, sync::{Mutex, OnceLock}};
pub struct TestVault;
fn values() -> &'static Mutex<HashMap<String, String>> {
    static VALUES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    VALUES.get_or_init(Default::default)
}
impl CredentialVault for TestVault {
    fn read(&self, target: &str) -> Result<Option<String>, SettingsError> { Ok(values().lock().unwrap().get(target).cloned()) }
    fn write(&self, target: &str, token: &str) -> Result<(), SettingsError> { values().lock().unwrap().insert(target.into(), token.into()); Ok(()) }
    fn delete(&self, target: &str) -> Result<(), SettingsError> { values().lock().unwrap().remove(target); Ok(()) }
}
