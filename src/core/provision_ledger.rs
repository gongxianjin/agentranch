//! Manager-local, non-secret provisioning history. A ledger is evidence for display
//! and recovery diagnostics only; it is never an authorization source or live session.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fmt, fs, io::{Read, Write}, path::{Component, Path, PathBuf}};
use super::{provision::ensure_no_reparse_ancestors, ssh_gateway::SshGatewayConfig};
use crate::config::{valid_id, Config, Mode};

pub const LEDGER_SCHEMA: &str = "worker-provision-ledger/1";
const LEDGER_FILE: &str = "provisioning-ledger.json";
const MAX_LEDGER_BYTES: u64 = 64 * 1024;
const FAILURE_CODES: &[&str] = &[
    "input_invalid", "helper_unavailable", "helper_process_error", "helper_timeout",
    "helper_result_invalid", "helper_io_bound_or_error", "fixture_invalid", "provision_error",
    "activation_blocked", "ledger_blocked", "attempt_identity_invalid", "remote_upload_failed",
];
const REMOTE_SIDE_EFFECTS: &[&str] = &["present", "absent"];
const SAFE_NEXT_STEP: &str = "重新输入秘密并重试 Worker 准备；不要手工猜测旧 Gateway";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerState { Pending, Failed, Preflight, Connected, Executing, Admitted }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionLedger {
    pub schema: String,
    pub state: LedgerState,
    pub output_dir: Option<PathBuf>,
    pub gateway_path: Option<PathBuf>,
    pub worker_id: Option<String>,
    pub host: Option<String>,
    pub artifact_sha256: Option<String>,
    pub artifact_bytes: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    pub failure_code: Option<String>,
    pub failure_next_step: Option<String>,
    pub attempt_id: Option<String>,
    pub remote_root: Option<String>,
    pub remote_side_effects: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionLedgerFacts {
    pub state: LedgerState,
    pub output_dir: Option<PathBuf>,
    pub gateway_path: Option<PathBuf>,
    pub worker_id: Option<String>,
    pub host: Option<String>,
    pub artifact_sha256: Option<String>,
    pub artifact_bytes: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    pub failure_code: Option<String>,
    pub failure_next_step: Option<String>,
    pub attempt_id: Option<String>,
    pub remote_root: Option<String>,
    pub remote_side_effects: Option<String>,
}

#[derive(Debug)]
pub enum LedgerError {
    Blocked(String),
    Io(std::io::Error),
    Json(serde_json::Error),
}
impl LedgerError {
    pub fn is_blocked(&self) -> bool { matches!(self, Self::Blocked(_)) }
}
impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocked(message) => write!(f, "blocked provisioning ledger: {message}"),
            Self::Io(error) => write!(f, "provisioning ledger I/O failed: {error}"),
            Self::Json(error) => write!(f, "provisioning ledger JSON failed: {error}"),
        }
    }
}
impl std::error::Error for LedgerError {}
impl From<std::io::Error> for LedgerError { fn from(error: std::io::Error) -> Self { Self::Io(error) } }
impl From<serde_json::Error> for LedgerError { fn from(error: serde_json::Error) -> Self { Self::Json(error) } }

impl ProvisionLedger {
    pub fn write_new(state_dir: &Path, facts: ProvisionLedgerFacts) -> std::result::Result<PathBuf, LedgerError> {
        let state_root = state_root(state_dir)?;
        ensure_no_reparse_ancestors(&state_root).map_err(|e| blocked(e.to_string()))?;
        fs::create_dir_all(&state_root)?;
        let provisioning = state_root.join("provisioning");
        ensure_no_reparse_ancestors(&provisioning).map_err(|e| blocked(e.to_string()))?;
        fs::create_dir_all(&provisioning)?;
        ensure_no_reparse_ancestors(&provisioning).map_err(|e| blocked(e.to_string()))?;
        let ledger = Self::from_facts(&state_root, facts)?;
        let target = state_root.join(LEDGER_FILE);
        atomic_write(&target, &serde_json::to_vec_pretty(&ledger)?)?;
        Ok(target)
    }

    pub fn load(state_dir: &Path) -> std::result::Result<Option<Self>, LedgerError> {
        let state_root = state_root(state_dir)?;
        if !state_root.exists() { return Ok(None); }
        ensure_no_reparse_ancestors(&state_root).map_err(|e| blocked(e.to_string()))?;
        let path = state_root.join(LEDGER_FILE);
        if !path.exists() { return Ok(None); }
        ensure_no_reparse_ancestors(&path).map_err(|e| blocked(e.to_string()))?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() { return Err(blocked("ledger file is a symlink")); }
        let mut raw = Vec::new();
        fs::File::open(&path)?.take(MAX_LEDGER_BYTES + 1).read_to_end(&mut raw)?;
        if raw.len() as u64 > MAX_LEDGER_BYTES { return Err(blocked("ledger exceeds size bound")); }
        let ledger: Self = serde_json::from_slice(&raw).map_err(|_| blocked("ledger contains unknown or malformed fields"))?;
        ledger.validate(&state_root)?;
        Ok(Some(ledger))
    }

    fn from_facts(state_root: &Path, facts: ProvisionLedgerFacts) -> std::result::Result<Self, LedgerError> {
        let ledger = Self {
            schema: LEDGER_SCHEMA.into(), state: facts.state, output_dir: facts.output_dir,
            gateway_path: facts.gateway_path, worker_id: facts.worker_id, host: facts.host,
            artifact_sha256: facts.artifact_sha256, artifact_bytes: facts.artifact_bytes,
            created_at: facts.created_at, updated_at: facts.updated_at,
            failure_code: facts.failure_code, failure_next_step: facts.failure_next_step,
            attempt_id: facts.attempt_id, remote_root: facts.remote_root,
            remote_side_effects: facts.remote_side_effects,
        };
        ledger.validate(state_root)?;
        Ok(ledger)
    }

    fn validate(&self, state_root: &Path) -> std::result::Result<(), LedgerError> {
        if self.schema != LEDGER_SCHEMA { return Err(blocked("unsupported ledger schema")); }
        if self.created_at > self.updated_at { return Err(blocked("ledger timestamps are inconsistent")); }
        if self.output_dir.is_none() && self.gateway_path.is_some() { return Err(blocked("gateway selector has no output directory")); }
        let provisioning = state_root.join("provisioning");
        if let Some(output) = &self.output_dir {
            validate_output_path(output, &provisioning)?;
        }
        if let Some(gateway) = &self.gateway_path {
            let output = self.output_dir.as_ref().ok_or_else(|| blocked("gateway selector has no output directory"))?;
            let expected = output.join("gateway.json");
            if gateway != &expected { return Err(blocked("gateway selector is not bound to selected output")); }
            validate_gateway(self, gateway, output)?;
        }
        match self.state {
            LedgerState::Pending | LedgerState::Failed => {
                if self.state == LedgerState::Failed {
                    let code = self.failure_code.as_deref().ok_or_else(|| blocked("failed ledger has no safe failure code"))?;
                    if !FAILURE_CODES.contains(&code) { return Err(blocked("failure code is not allowlisted")); }
                    let next = self.failure_next_step.as_deref().ok_or_else(|| blocked("failed ledger has no next step"))?;
                    if next != SAFE_NEXT_STEP { return Err(blocked("failure next step is not fixed safe copy")); }
                }
            }
            LedgerState::Preflight | LedgerState::Connected | LedgerState::Executing | LedgerState::Admitted => {
                if self.gateway_path.is_none() || self.output_dir.is_none() { return Err(blocked("active ledger has no selected gateway")); }
                for (name, value) in [("worker_id", self.worker_id.as_deref()), ("host", self.host.as_deref()), ("artifact_sha256", self.artifact_sha256.as_deref())] {
                    if value.is_none() { return Err(blocked(format!("active ledger has no {name}"))); }
                }
                if self.artifact_bytes.is_none() { return Err(blocked("active ledger has no artifact bytes")); }
            }
        }
        if let Some(worker) = &self.worker_id {
            if !valid_id(worker) { return Err(blocked("invalid worker identity")); }
        }
        if let Some(host) = &self.host {
            let parsed = host.parse::<std::net::IpAddr>().map_err(|_| blocked("ledger host is not an IP address"))?;
            if !parsed.is_ipv4() { return Err(blocked("ledger host must be IPv4")); }
        }
        if let Some(sha) = &self.artifact_sha256 {
            if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) { return Err(blocked("invalid artifact identity")); }
        }
        if let Some(bytes) = self.artifact_bytes {
            if bytes == 0 { return Err(blocked("artifact bytes must be positive")); }
        }
        if let Some(attempt_id) = &self.attempt_id {
            if !valid_attempt_id(attempt_id) { return Err(blocked("invalid provisioning attempt identity")); }
        }
        if let Some(remote_root) = &self.remote_root {
            if !valid_remote_root(remote_root) { return Err(blocked("invalid remote staging root")); }
        }
        if let Some(side_effects) = &self.remote_side_effects {
            if !REMOTE_SIDE_EFFECTS.contains(&side_effects.as_str()) { return Err(blocked("remote side-effect state is not allowlisted")); }
            if self.remote_root.is_none() { return Err(blocked("remote side-effect state has no remote root")); }
        }
        if self.state != LedgerState::Failed && (self.failure_code.is_some() || self.failure_next_step.is_some()) {
            return Err(blocked("non-failed ledger contains failure fields"));
        }
        Ok(())
    }
}

fn state_root(path: &Path) -> std::result::Result<PathBuf, LedgerError> {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    if absolute.components().any(|part| matches!(part, Component::CurDir | Component::ParentDir)) { return Err(blocked("state path is not normalized")); }
    Ok(absolute)
}
fn validate_output_path(output: &Path, provisioning: &Path) -> std::result::Result<(), LedgerError> {
    if !output.is_absolute() || output.components().any(|part| matches!(part, Component::CurDir | Component::ParentDir)) || !output.starts_with(provisioning) {
        return Err(blocked("output path escapes provisioning root"));
    }
    ensure_no_reparse_ancestors(output).map_err(|e| blocked(e.to_string()))?;
    if output.exists() && !output.is_dir() { return Err(blocked("output path is not a directory")); }
    Ok(())
}
fn valid_attempt_id(value: &str) -> bool {
    value.strip_prefix("attempt-").is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}
fn valid_remote_root(value: &str) -> bool {
    let mut parts = value.split('/');
    matches!(parts.next(), Some("") )
        && matches!(parts.next(), Some("home"))
        && matches!(parts.next(), Some(user) if !user.is_empty() && user != "." && user != ".."
            && user.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
        && matches!(parts.next(), Some(runtime) if runtime.starts_with("cross-runtime-")
            && runtime.len() > "cross-runtime-".len()
            && runtime.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
        && parts.next().is_none()
}
fn validate_gateway(ledger: &ProvisionLedger, gateway_path: &Path, output: &Path) -> std::result::Result<(), LedgerError> {
    if !gateway_path.is_file() { return Err(blocked("selected gateway is stale or missing")); }
    ensure_no_reparse_ancestors(gateway_path).map_err(|e| blocked(e.to_string()))?;
    let config = SshGatewayConfig::load(gateway_path).map_err(|_| blocked("selected gateway configuration is invalid"))?;
    if config.worker_id != ledger.worker_id.as_deref().unwrap_or("") || config.host != ledger.host.as_deref().unwrap_or("") || config.artifact_sha256 != ledger.artifact_sha256.as_deref().unwrap_or("") || Some(config.artifact_bytes) != ledger.artifact_bytes {
        return Err(blocked("gateway and ledger identity differ"));
    }
    if config.controller_config != output.join("controller.json") || config.state_dir != output.join("gateway-state") { return Err(blocked("gateway paths are not bound to selected output")); }
    let controller_path = output.join("controller.json");
    ensure_no_reparse_ancestors(&controller_path).map_err(|e| blocked(e.to_string()))?;
    if !controller_path.is_file() { return Err(blocked("controller configuration is stale or missing")); }
    let controller: Config = serde_json::from_slice(&fs::read(&controller_path).map_err(LedgerError::Io)?).map_err(|_| blocked("controller configuration is invalid"))?;
    if controller.mode != Mode::Real || controller.role != "controller" || controller.deployment != config.deployment || controller.state_dir != output.join("controller-state") { return Err(blocked("controller and gateway identity differ")); }
    if controller.nodes.len() != 1 || controller.nodes[0].id != config.worker_id || controller.nodes[0].address != format!("{}:22", config.host) { return Err(blocked("controller worker identity differs")); }
    Ok(())
}
fn blocked(message: impl Into<String>) -> LedgerError { LedgerError::Blocked(message.into()) }
fn atomic_write(target: &Path, bytes: &[u8]) -> std::result::Result<(), LedgerError> {
    let parent = target.parent().ok_or_else(|| blocked("ledger parent missing"))?;
    ensure_no_reparse_ancestors(parent).map_err(|e| blocked(e.to_string()))?;
    let temp = parent.join(format!(".provisioning-ledger-{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    ensure_no_reparse_ancestors(&temp).map_err(|e| blocked(e.to_string()))?;
    #[cfg(windows)] {
        use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
        let source: Vec<u16> = OsStr::new(&temp).encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = OsStr::new(target).encode_wide().chain(Some(0)).collect();
        let moved = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
        if moved == 0 { let _ = fs::remove_file(&temp); return Err(LedgerError::Io(std::io::Error::last_os_error())); }
    }
    #[cfg(not(windows))]
    fs::rename(&temp, target)?;
    Ok(())
}

impl ProvisionLedgerFacts {
    pub fn failure(code: &str, created_at: u64, updated_at: u64, output_dir: Option<PathBuf>) -> Self {
        Self::failure_with_attempt(code, created_at, updated_at, output_dir, None)
    }
    pub fn failure_with_attempt(code: &str, created_at: u64, updated_at: u64, output_dir: Option<PathBuf>, attempt_id: Option<String>) -> Self {
        Self::failure_with_remote(code, created_at, updated_at, output_dir, attempt_id, None, None)
    }
    pub fn failure_with_remote(code: &str, created_at: u64, updated_at: u64, output_dir: Option<PathBuf>, attempt_id: Option<String>, remote_root: Option<String>, remote_side_effects: Option<String>) -> Self {
        Self { state: LedgerState::Failed, output_dir, gateway_path: None, worker_id: None, host: None, artifact_sha256: None, artifact_bytes: None, created_at, updated_at, failure_code: Some(if FAILURE_CODES.contains(&code) { code.into() } else { "provision_error".into() }), failure_next_step: Some(SAFE_NEXT_STEP.into()), attempt_id, remote_root, remote_side_effects }
    }
    pub fn preflight(output_dir: PathBuf, gateway_path: PathBuf, worker_id: String, host: String, artifact_sha256: String, artifact_bytes: u64, now: u64) -> Self {
        Self::preflight_with_attempt(output_dir, gateway_path, worker_id, host, artifact_sha256, artifact_bytes, now, None)
    }
    pub fn preflight_with_attempt(output_dir: PathBuf, gateway_path: PathBuf, worker_id: String, host: String, artifact_sha256: String, artifact_bytes: u64, now: u64, attempt_id: Option<String>) -> Self {
        Self { state: LedgerState::Preflight, output_dir: Some(output_dir), gateway_path: Some(gateway_path), worker_id: Some(worker_id), host: Some(host), artifact_sha256: Some(artifact_sha256), artifact_bytes: Some(artifact_bytes), created_at: now, updated_at: now, failure_code: None, failure_next_step: None, attempt_id, remote_root: None, remote_side_effects: None }
    }
}

pub fn safe_failure_next_step() -> &'static str { SAFE_NEXT_STEP }

pub fn ledger_result<T>(result: std::result::Result<T, LedgerError>) -> Result<T> { result.map_err(|error| anyhow::anyhow!(error)) }
