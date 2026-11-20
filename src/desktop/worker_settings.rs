//! Desktop-local preferences; never authoritative connection/session state.
use std::{io::{Read, Write}, path::{Path, PathBuf}};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSettings {
    pub host: String,
    pub root_user: String,
    pub model_base_url: String,
    pub model: String,
}
impl WorkerSettings {
    fn valid(&self) -> bool {
        let origin = self.model_base_url.strip_prefix("https://").unwrap_or_default();
        self.host.parse::<std::net::Ipv4Addr>().is_ok()
            && !self.root_user.is_empty() && self.root_user.len() <= 128
            && self.root_user.chars().all(|c| !c.is_control() && !c.is_whitespace())
            && !origin.is_empty() && origin.len() <= 2048
            && !origin.contains(['/', '?', '#', '@'])
            && origin.chars().all(|c| !c.is_control() && !c.is_whitespace())
            && !origin.split(':').next().unwrap_or_default().is_empty()
            && !self.model.trim().is_empty() && self.model.len() <= 128
            && !self.model.chars().any(char::is_control)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum SettingsError { Vault, Io, InvalidFile, InvalidInput, MissingCredential, Cleanup }
impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(match self {
        Self::Vault => "Windows 凭据管理器不可用；本次操作未完整完成，请重试。",
        Self::Io => "无法读写连接设置；请检查状态目录权限后重试。",
        Self::InvalidFile => "连接设置损坏或不属于此状态目录；请重新填写并保存。",
        Self::InvalidInput => "请检查四项配置和 API key（最多 2560 UTF-8 字节）。",
        Self::MissingCredential => "已保存的 API key 不可用；请重新输入并保存。",
        Self::Cleanup => "配置已保存，但旧凭据未能清理；请在 Windows 凭据管理器检查 agentranch 条目。",
    }) }
}
impl std::error::Error for SettingsError {}
pub trait CredentialVault {
    fn read(&self, target: &str) -> Result<Option<String>, SettingsError>;
    fn write(&self, target: &str, token: &str) -> Result<(), SettingsError>;
    fn delete(&self, target: &str) -> Result<(), SettingsError>;
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedFile { schema: u32, settings: WorkerSettings, credential_target: String }
// Validated local preferences survive a credential error; callers must still
// handle that error explicitly before treating the API key as available.
pub struct RestoredSettings { pub settings: WorkerSettings, pub token: Result<String, SettingsError> }
pub struct SettingsStore { directory: PathBuf, vault: Box<dyn CredentialVault> }
impl SettingsStore {
    pub fn new(directory: &Path) -> Self { Self::with_vault(directory, Box::new(WindowsVault)) }
    pub fn with_vault(directory: &Path, vault: Box<dyn CredentialVault>) -> Self {
        Self { directory: directory.into(), vault }
    }
    fn prefix(&self) -> Result<String, SettingsError> {
        let canonical = self.directory.canonicalize().map_err(|_| SettingsError::Io)?;
        let identity = canonical.to_string_lossy();
        #[cfg(windows)]
        let identity = identity.to_lowercase();
        Ok(format!("agentranch/worker-settings/v1/{:x}/", Sha256::digest(identity.as_bytes())))
    }
    fn read_file(&self) -> Result<Option<SavedFile>, SettingsError> {
        let path = self.directory.join("worker-settings.json");
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(SettingsError::Io),
        };
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| SettingsError::Io)?;
        if bytes.len() > 16 * 1024 { return Err(SettingsError::InvalidFile); }
        let saved: SavedFile = serde_json::from_slice(&bytes).map_err(|_| SettingsError::InvalidFile)?;
        let prefix = self.prefix()?;
        let origin_prefix = format!("{}{:x}/", prefix, Sha256::digest(saved.settings.model_base_url.as_bytes()));
        let suffix = saved.credential_target.strip_prefix(&origin_prefix).ok_or(SettingsError::InvalidFile)?;
        if saved.schema != 1 || !saved.settings.valid() || suffix.len() != 32
            || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) { return Err(SettingsError::InvalidFile); }
        Ok(Some(saved))
    }
    pub fn load(&self) -> Result<Option<RestoredSettings>, SettingsError> {
        let Some(saved) = self.read_file()? else { return Ok(None); };
        let token = self.vault.read(&saved.credential_target).and_then(|token| {
            token.filter(|token| !token.is_empty() && token.len() <= 2560)
                .ok_or(SettingsError::MissingCredential)
        });
        Ok(Some(RestoredSettings { settings: saved.settings, token }))
    }
    pub fn save(&self, settings: &WorkerSettings, token: &str) -> Result<(), SettingsError> {
        if !settings.valid() || token.is_empty() || token.len() > 2560 { return Err(SettingsError::InvalidInput); }
        std::fs::create_dir_all(&self.directory).map_err(|_| SettingsError::Io)?;
        // Corrupt files may be replaced after explicit user entry, but their
        // untrusted references must never be read/deleted in the user's vault.
        let previous = self.read_file().ok().flatten();
        let target = format!("{}{:x}/{}", self.prefix()?, Sha256::digest(settings.model_base_url.as_bytes()), uuid::Uuid::new_v4().simple());
        let saved = SavedFile { schema: 1, settings: settings.clone(), credential_target: target.clone() };
        let bytes = serde_json::to_vec_pretty(&saved).map_err(|_| SettingsError::InvalidInput)?;
        self.vault.write(&target, token)?;
        if let Err(error) = self.atomic_write(&bytes) {
            let _ = self.vault.delete(&target);
            return Err(error);
        }
        if let Some(previous) = previous {
            // A failed cleanup is reported, not described as a failed commit:
            // the current JSON and credential are already durably saved.
            if self.vault.delete(&previous.credential_target).is_err() { return Err(SettingsError::Cleanup); }
        }
        Ok(())
    }
    fn atomic_write(&self, bytes: &[u8]) -> Result<(), SettingsError> {
        let temp = self.directory.join(format!(".worker-settings-{}.tmp", uuid::Uuid::new_v4().simple()));
        let destination = self.directory.join("worker-settings.json");
        let result = (|| {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp).map_err(|_| SettingsError::Io)?;
            file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|_| SettingsError::Io)?;
            drop(file);
            #[cfg(windows)] {
                use std::os::windows::ffi::OsStrExt;
                use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
                let source: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
                let target: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
                // Both paths are live, NUL-terminated buffers. Same-directory
                // replacement keeps the prior file intact on move failure.
                if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 { return Err(SettingsError::Io); }
            }
            #[cfg(not(windows))]
            std::fs::rename(&temp, destination).map_err(|_| SettingsError::Io)?;
            Ok(())
        })();
        if result.is_err() { let _ = std::fs::remove_file(temp); }
        result
    }
}

pub struct WindowsVault;
#[cfg(windows)]
impl CredentialVault for WindowsVault {
    fn read(&self, target: &str) -> Result<Option<String>, SettingsError> {
        use windows_sys::Win32::{Foundation::{GetLastError, ERROR_NOT_FOUND}, Security::Credentials::*};
        let target: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let mut pointer = std::ptr::null_mut();
        // CredRead allocates a single buffer; all paths after success free it.
        unsafe {
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut pointer) == 0 {
                return if GetLastError() == ERROR_NOT_FOUND { Ok(None) } else { Err(SettingsError::Vault) };
            }
            if pointer.is_null() { return Err(SettingsError::Vault); }
            let credential = &*pointer;
            let token = if credential.CredentialBlobSize == 0 || credential.CredentialBlobSize > CRED_MAX_CREDENTIAL_BLOB_SIZE || credential.CredentialBlob.is_null() {
                Err(SettingsError::Vault)
            } else {
                let blob = std::slice::from_raw_parts_mut(credential.CredentialBlob, credential.CredentialBlobSize as usize);
                let token = std::str::from_utf8(blob).map(str::to_owned).map_err(|_| SettingsError::Vault);
                blob.fill(0);
                token
            };
            CredFree(pointer.cast());
            token.map(Some)
        }
    }
    fn write(&self, target: &str, token: &str) -> Result<(), SettingsError> {
        use windows_sys::Win32::Security::Credentials::*;
        if token.is_empty() || token.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize { return Err(SettingsError::InvalidInput); }
        let mut target: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let mut username: Vec<u16> = "agentranch".encode_utf16().chain(Some(0)).collect();
        let mut bytes = token.as_bytes().to_vec();
        let credential = CREDENTIALW { Type: CRED_TYPE_GENERIC, TargetName: target.as_mut_ptr(), CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_mut_ptr(), Persist: CRED_PERSIST_LOCAL_MACHINE, UserName: username.as_mut_ptr(), ..Default::default() };
        // CredWrite copies the blob synchronously; erase our temporary copy.
        let ok = unsafe { CredWriteW(&credential, 0) };
        bytes.fill(0);
        if ok == 0 { Err(SettingsError::Vault) } else { Ok(()) }
    }
    fn delete(&self, target: &str) -> Result<(), SettingsError> {
        use windows_sys::Win32::{Foundation::{GetLastError, ERROR_NOT_FOUND}, Security::Credentials::*};
        let target: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        unsafe { if CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) != 0 || GetLastError() == ERROR_NOT_FOUND { Ok(()) } else { Err(SettingsError::Vault) } }
    }
}
#[cfg(not(windows))]
impl CredentialVault for WindowsVault {
    fn read(&self, _: &str) -> Result<Option<String>, SettingsError> { Err(SettingsError::Vault) }
    fn write(&self, _: &str, _: &str) -> Result<(), SettingsError> { Err(SettingsError::Vault) }
    fn delete(&self, _: &str) -> Result<(), SettingsError> { Err(SettingsError::Vault) }
}
