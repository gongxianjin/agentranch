//! Plaintext local connection preferences, separate from execution evidence.
use std::{fmt, io::{Read, Write}, path::{Path, PathBuf}};
use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA: u32 = 2;
const MAX_FILE_BYTES: usize = 1_048_576;
const MAX_SECRET_BYTES: usize = 16 * 1024;

/// Fixed collaboration role. Exactly one worker per role: the pipeline moves a
/// task from split → develop → test → review, so a role may not be duplicated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRole { Split, Develop, Test, Review }
impl WorkerRole {
    pub const ALL: [Self; 4] = [Self::Split, Self::Develop, Self::Test, Self::Review];
    /// Stable wire/field name; also the id prefix of a worker bound to this role.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Split => "split", Self::Develop => "develop", Self::Test => "test", Self::Review => "review",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Split => "拆分", Self::Develop => "开发", Self::Test => "测试", Self::Review => "审查",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}
impl fmt::Debug for ModelSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelSettings")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredWorker {
    pub id: String,
    pub role: WorkerRole,
    pub host: String,
    pub username: String,
    pub password: String,
}
impl fmt::Debug for ConfiguredWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfiguredWorker")
            .field("id", &self.id)
            .field("role", &self.role)
            .field("host", &self.host)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionSettings {
    pub schema: u32,
    pub model: ModelSettings,
    pub workers: Vec<ConfiguredWorker>,
}
impl fmt::Debug for ConnectionSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionSettings")
            .field("schema", &self.schema)
            .field("model", &self.model)
            .field("workers", &self.workers)
            .finish()
    }
}
impl ConnectionSettings {
    pub fn validate(&self) -> Result<(), ConnectionSettingsError> {
        if self.schema != CURRENT_SCHEMA || self.workers.is_empty() {
            return Err(ConnectionSettingsError::InvalidInput);
        }
        let base_url = self.model.base_url.trim();
        let authority = base_url.strip_prefix("https://").unwrap_or_default();
        if authority.is_empty() || authority.len() > 2048
            || authority.contains(['/', '?', '#', '@'])
            || authority.chars().any(|c| c.is_control() || c.is_whitespace())
            || authority.split(':').next().is_none_or(str::is_empty)
            || self.model.api_key.is_empty() || self.model.api_key.len() > MAX_SECRET_BYTES
            || self.model.api_key.chars().any(char::is_control)
            || self.model.model.trim().is_empty() || self.model.model.len() > 128
            || self.model.model.chars().any(char::is_control)
        {
            return Err(ConnectionSettingsError::InvalidInput);
        }
        let mut ids = std::collections::HashSet::with_capacity(self.workers.len());
        let mut hosts = std::collections::HashSet::with_capacity(self.workers.len());
        let mut roles = std::collections::HashSet::new();
        for worker in &self.workers {
            let host = worker.host.trim();
            if worker.id.is_empty() || worker.id.len() > 128
                || !worker.id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                || !ids.insert(worker.id.clone())
                || !roles.insert(worker.role)
                || host.parse::<std::net::Ipv4Addr>().is_err()
                || !hosts.insert(host.to_owned())
                || worker.username.trim().is_empty() || worker.username.len() > 128
                || worker.username.chars().any(|c| c.is_control() || c.is_whitespace())
                || worker.password.is_empty() || worker.password.len() > MAX_SECRET_BYTES
                || worker.password.chars().any(char::is_control)
            {
                return Err(ConnectionSettingsError::InvalidInput);
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConnectionSettingsError { InvalidInput, InvalidFile, Io }
impl fmt::Display for ConnectionSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "请检查模型配置与 Worker 列表；至少配置一台 Worker，且四个角色各一台。",
            Self::InvalidFile => "本机连接配置损坏或格式不支持；请重新填写并保存。",
            Self::Io => "无法读写本机连接配置；请检查目录权限。",
        })
    }
}
impl std::error::Error for ConnectionSettingsError {}

pub struct ConnectionSettingsStore { directory: PathBuf }
impl ConnectionSettingsStore {
    pub fn new(directory: &Path) -> Self { Self { directory: directory.into() } }
    fn path(&self) -> PathBuf { self.directory.join("connection-settings.json") }
    pub fn load(&self) -> Result<Option<ConnectionSettings>, ConnectionSettingsError> {
        let file = match std::fs::File::open(self.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ConnectionSettingsError::Io),
        };
        let mut bytes = Vec::new();
        file.take((MAX_FILE_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(|_| ConnectionSettingsError::Io)?;
        if bytes.len() > MAX_FILE_BYTES { return Err(ConnectionSettingsError::InvalidFile); }
        let settings: ConnectionSettings = serde_json::from_slice(&bytes).map_err(|_| ConnectionSettingsError::InvalidFile)?;
        settings.validate().map_err(|_| ConnectionSettingsError::InvalidFile)?;
        Ok(Some(settings))
    }
    pub fn save(&self, settings: &ConnectionSettings) -> Result<(), ConnectionSettingsError> {
        settings.validate()?;
        let bytes = serde_json::to_vec_pretty(settings).map_err(|_| ConnectionSettingsError::InvalidInput)?;
        if bytes.len() > MAX_FILE_BYTES { return Err(ConnectionSettingsError::InvalidInput); }
        std::fs::create_dir_all(&self.directory).map_err(|_| ConnectionSettingsError::Io)?;
        let temporary = self.directory.join(format!(".connection-settings-{}.tmp", uuid::Uuid::new_v4().simple()));
        let destination = self.path();
        let result = (|| {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary).map_err(|_| ConnectionSettingsError::Io)?;
            file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|_| ConnectionSettingsError::Io)?;
            drop(file);
            #[cfg(windows)] {
                use std::os::windows::ffi::OsStrExt;
                use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
                let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
                let target: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
                if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
                    return Err(ConnectionSettingsError::Io);
                }
            }
            #[cfg(not(windows))]
            std::fs::rename(&temporary, &destination).map_err(|_| ConnectionSettingsError::Io)?;
            Ok(())
        })();
        if result.is_err() { let _ = std::fs::remove_file(temporary); }
        result
    }
}
