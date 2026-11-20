//! Offline-safe provisioning IPC. This module never opens a Store or a gateway.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fmt, io::{Read, Write}, path::{Component, Path, PathBuf}, process::{Command, Stdio}, time::{Duration, Instant}};
use super::service::{ProvisionRequest, ServiceError, validate_provision_input};

const REQUEST_MAGIC: &[u8] = b"AW-WORKER-PROVISION/1\n";
const RESULT_MAGIC: &[u8] = b"AW-WORKER-PROVISION-RESULT/1\n";
const MAX_METADATA: usize = 64 * 1024;
const MAX_REQUEST: usize = 128 * 1024;
const MAX_PRIVATE_KEY: usize = 64 * 1024;
const MAX_RESULT: usize = MAX_METADATA + MAX_PRIVATE_KEY + 64;
const MAX_STDERR: usize = 16 * 1024;
// The Helper envelope must cover the bounded SSH/upload/preflight stage
// window. Each stage keeps its own timeout and still propagates failures; this
// outer budget only prevents the Manager from killing a valid slow attempt
// before those stages can report their result.
const HELPER_TIMEOUT: Duration = Duration::from_secs(120);

/// Owned secret storage, deliberately opaque to Debug and cleared on all exits.
pub struct SecretBytes(Vec<u8>);
impl SecretBytes {
    pub fn copy_from(bytes: &[u8]) -> Self { Self(bytes.to_vec()) }
    pub(crate) fn from_vec(bytes: Vec<u8>) -> Self { Self(bytes) }
    pub(crate) fn drain_bytes(&mut self) -> Vec<u8> { std::mem::take(&mut self.0) }
}
impl std::ops::Deref for SecretBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] { &self.0 }
}
impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "[REDACTED; {} bytes]", self.0.len()) }
}
impl Drop for SecretBytes { fn drop(&mut self) { self.0.fill(0); } }

#[derive(Debug)]
pub struct ProvisionResult { pub metadata: Value, private_key: SecretBytes }
impl ProvisionResult {
    pub fn take_private_key(self) -> SecretBytes { self.private_key }
    pub(crate) fn private_key(&self) -> &[u8] { &self.private_key }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisioningContext { output_dir: PathBuf, attempt_id: String }
impl ProvisioningContext {
    pub fn new(output_dir: PathBuf) -> Result<Self> {
        ensure!(output_dir.is_absolute(), "provisioning output directory must be absolute");
        ensure!(output_dir.components().all(|component| {
            !matches!(component, Component::CurDir | Component::ParentDir)
        }), "provisioning output directory must not contain traversal components");
        ensure!(!output_dir.exists(), "provisioning output directory already exists");
        ensure_no_reparse_ancestors(&output_dir)?;
        let attempt_id = attempt_id_for_output(&output_dir);
        Ok(Self { output_dir, attempt_id })
    }

    pub fn from_state_dir(state_dir: &Path) -> Result<Self> {
        let state = if state_dir.is_absolute() {
            state_dir.to_path_buf()
        } else {
            std::env::current_dir()?.join(state_dir)
        };
        ensure!(state.components().all(|component| {
            !matches!(component, Component::CurDir | Component::ParentDir)
        }), "state directory must not contain traversal components");
        ensure_no_reparse_ancestors(&state)?;
        let parent = state.join("provisioning");
        ensure_no_reparse_ancestors(&parent)?;
        let output = parent.join(format!("worker-attempt-{}", uuid::Uuid::new_v4().simple()));
        Self::new(output)
    }

    pub fn retry_from(state_dir: &Path) -> Result<Self> { Self::from_state_dir(state_dir) }
    pub fn output_dir(&self) -> &Path { &self.output_dir }
    pub fn attempt_id(&self) -> &str { &self.attempt_id }
}

fn attempt_id_for_output(output: &Path) -> String {
    output.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("worker-attempt-"))
        .filter(|suffix| suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|suffix| format!("attempt-{suffix}"))
        .unwrap_or_else(|| format!("attempt-{}", uuid::Uuid::new_v4().simple()))
}

pub(crate) fn ensure_no_reparse_ancestors(path: &Path) -> Result<()> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        let metadata = match std::fs::symlink_metadata(candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = candidate.parent();
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(!metadata.file_type().is_symlink(), "provisioning path contains a symlink");
        #[cfg(windows)] {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            ensure!(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
                "provisioning path contains a reparse point");
        }
        current = candidate.parent();
    }
    Ok(())
}

#[derive(Debug)]
pub struct ProvisionFailure { kind: &'static str, diagnostic: Option<String> }
impl ProvisionFailure {
    pub fn new(kind: &'static str) -> Self { Self { kind, diagnostic: None } }
    pub fn from_stderr(kind: &'static str, stderr: &[u8], secrets: &[&[u8]]) -> Self {
        Self { kind, diagnostic: safe_diagnostic(stderr, secrets) }
    }
    pub fn diagnostic(&self) -> Option<&str> { self.diagnostic.as_deref() }
    pub fn service_error(&self) -> ServiceError {
        let detail = self.diagnostic.as_deref().unwrap_or("无可安全展示的 Helper 诊断");
        ServiceError { code: "provision_error".into(), message: format!("Worker 自动准备受阻（{}）：{}；未更改 Gateway 或释放任务名额", self.kind, detail),
            next_step: "检查六项输入、同版内置 Helper/运行时与 Windows 本地资源；保留账本后重试，勿重派 UNKNOWN 任务".into() }
    }
}
fn safe_diagnostic(stderr: &[u8], secrets: &[&[u8]]) -> Option<String> {
    let mut text = String::from_utf8_lossy(&stderr[..stderr.len().min(MAX_STDERR)]).into_owned();
    for secret in secrets.iter().filter(|secret|!secret.is_empty()) {
        let secret = String::from_utf8_lossy(secret);
        if !secret.is_empty() { text = text.replace(secret.as_ref(), "[REDACTED]"); }
    }
    if ["password", "token", "secret", "private_key", "api_key", "apikey", "BEGIN"].iter().any(|marker|text.to_ascii_lowercase().contains(&marker.to_ascii_lowercase())) {
        return Some("[Helper diagnostic suppressed]".into());
    }
    let text: String = text.chars().filter(|c|!c.is_control() || *c == '\n' || *c == '\t').take(1024).collect();
    (!text.trim().is_empty()).then_some(text)
}
pub trait Provisioner: Send {
    fn provision(&self, request: &ProvisionRequest) -> std::result::Result<ProvisionResult, ProvisionFailure>;
    fn prerequisites_root(&self) -> std::result::Result<PathBuf, ProvisionFailure> {
        let executable = std::env::current_exe().map_err(|_| ProvisionFailure::new("helper_unavailable"))?;
        Ok(executable.parent().ok_or_else(|| ProvisionFailure::new("helper_unavailable"))?.join("tools"))
    }
    fn provision_with_context(&self, request: &ProvisionRequest, _context: &ProvisioningContext) -> std::result::Result<ProvisionResult, ProvisionFailure> {
        self.provision(request)
    }
}
pub struct BundledProvisioner;
impl Provisioner for BundledProvisioner {
    fn provision(&self, request: &ProvisionRequest) -> std::result::Result<ProvisionResult, ProvisionFailure> {
        ProvisionBridge::bundled().map_err(|_| ProvisionFailure::new("helper_unavailable"))?.provision(request)
    }

    fn provision_with_context(&self, request: &ProvisionRequest, context: &ProvisioningContext) -> std::result::Result<ProvisionResult, ProvisionFailure> {
        ProvisionBridge::bundled_with_context(context.clone()).map_err(|_| ProvisionFailure::new("helper_unavailable"))?.provision(request)
    }
}

pub struct ProvisionBridge { helper: PathBuf, interpreter: PathBuf, context: Option<ProvisioningContext> }
impl ProvisionBridge {
    pub fn bundled() -> Result<Self> {
        let executable = std::env::current_exe().context("Manager executable unavailable")?;
        let parent = executable.parent().context("Manager directory unavailable")?;
        Self::with_runtime(parent.join("tools/paramiko_provision.py"), parent.join("runtime/python/python.exe"))
    }
    pub fn bundled_with_context(context: ProvisioningContext) -> Result<Self> {
        let executable = std::env::current_exe().context("Manager executable unavailable")?;
        let parent = executable.parent().context("Manager directory unavailable")?;
        Self::with_runtime_in_dir(parent.join("tools/paramiko_provision.py"), parent.join("runtime/python/python.exe"), context)
    }
    pub fn with_runtime(helper: PathBuf, interpreter: PathBuf) -> Result<Self> {
        ensure!(helper.is_absolute() && helper.is_file(), "bundled provisioning helper unavailable");
        ensure!(interpreter.is_absolute() && interpreter.is_file(), "bundled provisioning runtime unavailable");
        Ok(Self { helper, interpreter, context: None })
    }
    pub fn with_runtime_in_dir(helper: PathBuf, interpreter: PathBuf, context: ProvisioningContext) -> Result<Self> {
        let bridge = Self::with_runtime(helper, interpreter)?;
        Ok(Self { context: Some(context), ..bridge })
    }
    pub fn command(&self) -> Result<Command> {
        let mut command = Command::new(&self.interpreter);
        command.arg("-I").arg("-B").arg(&self.helper).env_clear();
        command.current_dir(self.helper.parent().context("Helper directory unavailable")?);
        #[cfg(windows)] {
            use std::os::windows::process::CommandExt;
            command.env("SystemRoot", std::env::var_os("SystemRoot").context("Windows SystemRoot unavailable")?);
            command.creation_flags(0x08000000);
        }
        Ok(command)
    }
    pub fn command_for_test(&self) -> Result<Command> { self.command() }
}
impl Provisioner for ProvisionBridge {
    fn provision(&self, request: &ProvisionRequest) -> std::result::Result<ProvisionResult, ProvisionFailure> {
        self.provision_inner(request, self.context.as_ref())
    }

    fn provision_with_context(&self, request: &ProvisionRequest, context: &ProvisioningContext) -> std::result::Result<ProvisionResult, ProvisionFailure> {
        self.provision_inner(request, Some(context))
    }
}
impl ProvisionBridge {
    fn provision_inner(&self, request: &ProvisionRequest, context: Option<&ProvisioningContext>) -> std::result::Result<ProvisionResult, ProvisionFailure> {
        let frame = match context {
            Some(context) => encode_request_frame_with_output_dir(request, context),
            None => encode_request_frame(request),
        }.map_err(|_| ProvisionFailure::new("input_invalid"))?;
        let mut command = self.command().map_err(|_| ProvisionFailure::new("helper_unavailable"))?;
        let output = exchange_process(&mut command, &frame, HELPER_TIMEOUT).map_err(|_| ProvisionFailure::new("helper_process_error"))?;
        let secrets = [&request.root_password[..], &request.model_token[..]];
        if let Some(kind) = output.failure { return Err(ProvisionFailure::from_stderr(kind, &output.stderr, &secrets)); }
        let result = match context {
            Some(context) => decode_result_frame_in_dir_with_attempt(&output.stdout, &request.root_password, &request.model_token, context),
            None => decode_result_frame(&output.stdout, &request.root_password, &request.model_token),
        }.map_err(|_| ProvisionFailure::from_stderr("helper_result_invalid", &output.stderr, &secrets))?;
        if result.metadata["status"] == "PASS_WORKER_PROVISION" && output.exit_code != Some(0) {
            return Err(ProvisionFailure::new("helper_exit_error"));
        }
        Ok(result)
    }
}

pub fn encode_request_frame(request: &ProvisionRequest) -> Result<SecretBytes> {
    encode_request_frame_inner(request, None, None)
}

/// Derive the worker's TCP connect override from the Manager host's view.
///
/// When the Manager host resolves the model origin's hostname to a private
/// LAN address (hosts-pin or split DNS — the view the user actually
/// validated), the Linux worker must connect there instead of trusting the
/// public answer (dist/live-20261120-real-01: public DNS pointed at a dead
/// address, so the worker's model call was refused). TLS SNI, name
/// verification, and the byte-for-byte pin stay bound to the URL hostname;
/// only TCP dialing uses this address. Public resolutions and IP literals
/// are never overridden.
pub fn derive_connect_addr(base_url: &str, resolved: &[std::net::Ipv4Addr]) -> Option<String> {
    let parsed = reqwest::Url::parse(base_url).ok()?;
    let host = parsed.host_str()?;
    // An IP-literal origin never involves resolution.
    if host.parse::<std::net::IpAddr>().is_ok() { return None; }
    let port = parsed.port_or_known_default().unwrap_or(443);
    resolved.iter().find(|address| address.is_private()).map(|address| format!("{address}:{port}"))
}

/// Resolve the model origin on this (Manager) host and derive the override.
/// Resolution failure or timeout never blocks provisioning: no answer means
/// no override, and the worker falls back to its own resolution.
pub fn resolve_connect_addr(base_url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(base_url).ok()?;
    let host = parsed.host_str()?.to_owned();
    if host.parse::<std::net::IpAddr>().is_ok() { return None; }
    let port = parsed.port_or_known_default().unwrap_or(443);
    use std::net::ToSocketAddrs;
    let answers = (host.as_str(), port).to_socket_addrs().ok()?;
    answers.filter_map(|socket| socket.ip().to_string().parse::<std::net::Ipv4Addr>().ok())
        .find(|address| address.is_private())
        .map(|address| format!("{address}:{port}"))
}

pub fn encode_request_frame_with_output_dir(request: &ProvisionRequest, context: &ProvisioningContext) -> Result<SecretBytes> {
    encode_request_frame_inner(request, Some(context.output_dir()), Some(context.attempt_id()))
}

fn encode_request_frame_inner(
    request: &ProvisionRequest, output_dir: Option<&Path>, attempt_id: Option<&str>,
) -> Result<SecretBytes> {
    validate_provision_input(request)?;
    let mut metadata_value = json!({"schema":"worker-provision/1", "host":request.host.trim(),
        "root_user":request.root_user.trim(), "model_base_url":request.model_base_url.trim(), "model":request.model});
    if let Some(connect_addr) = resolve_connect_addr(request.model_base_url.trim()) {
        metadata_value["connect_addr"] = json!(connect_addr);
    }
    if let Some(output_dir) = output_dir {
        ensure!(output_dir.is_absolute(), "provisioning output directory must be absolute");
        metadata_value["output_dir"] = json!(output_dir.display().to_string());
    }
    if let Some(attempt_id) = attempt_id {
        metadata_value["attempt_id"] = json!(attempt_id);
    }
    ensure!(!contains_secret(&metadata_value, &[&request.root_password, &request.model_token]), "provision metadata echoes a secret");
    let metadata = serde_json::to_vec(&metadata_value)?;
    ensure!(metadata.len() <= MAX_METADATA, "provision metadata bound exceeded");
    let mut frame = SecretBytes(REQUEST_MAGIC.to_vec());
    for field in [&metadata[..], &request.root_password, &request.model_token] {
        frame.0.extend_from_slice(&(field.len() as u32).to_be_bytes());
        frame.0.extend_from_slice(field);
    }
    ensure!(frame.len() <= MAX_REQUEST, "provision stdin bound exceeded");
    Ok(frame)
}
fn read_field<'a>(raw: &'a [u8], offset: &mut usize, max: usize) -> Result<&'a [u8]> {
    let length = raw.get(*offset..*offset + 4).context("truncated result frame length")?;
    *offset += 4;
    let length = u32::from_be_bytes(length.try_into()?) as usize;
    ensure!(length <= max, "result frame field bound exceeded");
    let end = offset.checked_add(length).context("result frame length overflow")?;
    let field = raw.get(*offset..end).context("truncated result frame field")?;
    *offset = end;
    Ok(field)
}
fn contains_secret(value: &Value, secrets: &[&[u8]]) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            let key = key.to_ascii_lowercase();
            ["password", "token", "secret", "private_key", "api_key", "apikey"].iter().any(|marker|key.contains(marker)) || contains_secret(value, secrets)
        }),
        Value::Array(items) => items.iter().any(|item|contains_secret(item, secrets)),
        Value::String(text) => secrets.iter().any(|secret| !secret.is_empty() && text.as_bytes().windows(secret.len()).any(|bytes|bytes == *secret)),
        _ => false,
    }
}
fn validate_result_metadata(metadata: &Value, password: &[u8], token: &[u8], output_dir: Option<&Path>) -> Result<()> {
    let fields = metadata.as_object().context("provision result must be an object")?;
    const ALLOWED: &[&str] = &["schema", "status", "failed_stage", "error_type", "next_step", "preflight", "gateway_path", "artifact_sha256", "artifact_bytes", "worker_id", "host", "attempt_id", "remote_root", "remote_side_effects", "reason_code"];
    ensure!(fields.keys().all(|key|ALLOWED.contains(&key.as_str())), "unsupported provision result field");
    ensure!(!contains_secret(metadata, &[password, token]), "provision result contains secret metadata");
    ensure!(metadata["schema"] == "worker-provision/1", "unsupported provision result schema");
    for (key, value) in fields {
        if matches!(key.as_str(), "preflight" | "artifact_bytes") { continue; }
        let text = value.as_str().context("invalid provision result field type")?;
        ensure!(text.len() <= 4096 && !text.chars().any(char::is_control), "invalid provision result text");
        if key == "attempt_id" {
            ensure!(text.strip_prefix("attempt-").is_some_and(|suffix| suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())), "invalid provision attempt identity");
        }
        if key == "remote_root" {
            ensure!(text.strip_prefix("/home/").is_some_and(|rest| {
                let mut parts = rest.split('/');
                let user = parts.next().unwrap_or_default();
                let root = parts.next().unwrap_or_default();
                !user.is_empty() && user.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                    && root.starts_with("cross-runtime-") && root.len() > "cross-runtime-".len()
                    && root.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && parts.next().is_none()
            }), "invalid provision remote staging root");
        }
        if key == "remote_side_effects" {
            ensure!(matches!(text, "present" | "absent"), "invalid provision remote side-effect state");
        }
        if key == "reason_code" {
            ensure!(text.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase())
                && text.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'), "invalid provision reason code");
        }
    }
    ensure!(metadata["remote_side_effects"].is_null() || metadata["remote_root"].is_string(), "remote side-effect state requires remote root");
    match metadata["status"].as_str() {
        Some("BLOCKED_OR_FAILED") => ensure!(metadata["next_step"].as_str().is_some_and(|s|!s.trim().is_empty()), "blocked result needs next step"),
        Some("PASS_WORKER_PROVISION") => {
            ensure!(metadata["attempt_id"].as_str().is_some_and(|value| value.starts_with("attempt-")), "provision success requires attempt identity");
            ensure!(metadata["preflight"] == true, "provision success requires preflight");
            let gateway = metadata["gateway_path"].as_str().context("provision success requires gateway path")?;
            let gateway_path = Path::new(gateway);
            ensure!(gateway_path.is_absolute(), "provision success requires absolute gateway path");
            if let Some(output) = output_dir {
                ensure_no_reparse_ancestors(gateway_path)?;
                ensure!(gateway_path.parent() == Some(output), "gateway path must be inside manager output directory");
                ensure!(gateway_path.file_name().is_some_and(|name| name == "gateway.json"), "gateway path must name gateway.json");
            }
        },
        _ => anyhow::bail!("unsupported provision result status"),
    }
    Ok(())
}
pub fn decode_result_frame(raw: &[u8], password: &[u8], token: &[u8]) -> Result<ProvisionResult> {
    decode_result_frame_inner(raw, password, token, None, None)
}
pub fn decode_result_frame_in_dir(raw: &[u8], password: &[u8], token: &[u8], output_dir: &Path) -> Result<ProvisionResult> {
    decode_result_frame_inner(raw, password, token, Some(output_dir), None)
}

pub fn decode_result_frame_in_dir_with_attempt(
    raw: &[u8], password: &[u8], token: &[u8], context: &ProvisioningContext,
) -> Result<ProvisionResult> {
    decode_result_frame_inner(raw, password, token, Some(context.output_dir()), Some(context.attempt_id()))
}

fn decode_result_frame_inner(
    raw: &[u8], password: &[u8], token: &[u8], output_dir: Option<&Path>, expected_attempt: Option<&str>,
) -> Result<ProvisionResult> {
    ensure!(raw.len() <= MAX_RESULT && raw.starts_with(RESULT_MAGIC), "invalid provision result frame");
    let mut offset = RESULT_MAGIC.len();
    let metadata: Value = serde_json::from_slice(read_field(raw, &mut offset, MAX_METADATA)?)
        .map_err(|_|anyhow::anyhow!("invalid provision result frame JSON"))?;
    validate_result_metadata(&metadata, password, token, output_dir)?;
    if let Some(expected) = expected_attempt {
        ensure!(metadata["attempt_id"].as_str() == Some(expected), "provision result attempt identity differs from current context");
    }
    let key = read_field(raw, &mut offset, MAX_PRIVATE_KEY)?;
    ensure!(offset == raw.len(), "trailing provision result frame bytes");
    ensure!(key.is_empty() || metadata["status"] == "PASS_WORKER_PROVISION", "blocked result must not carry private key");
    ensure!(key != password && key != token, "provision result private key echoes an input secret");
    Ok(ProvisionResult { metadata, private_key: SecretBytes(key.to_vec()) })
}
/// Never accepts a helper path as trusted activation evidence. This increment
/// deliberately preserves existing state until complete config activation exists.
pub fn service_error_from_result(metadata: &Value) -> ServiceError {
    let next_step = if metadata["status"] == "BLOCKED_OR_FAILED" {
        metadata["next_step"].as_str().unwrap_or("补齐可信配置后重试")
    } else { "完整 Gateway 配置的可信校验与激活尚未实现；保留现有配置和任务名额，完成后再提交" };
    ServiceError { code: "blocked".into(), message: "Worker 自动准备尚未闭环；未报告就绪、未更改 Gateway 或释放任务名额".into(), next_step: next_step.into() }
}

/// Captures are secret-bearing until validated: Debug only renders byte counts.
pub struct HelperOutput { pub stdout: Vec<u8>, pub stderr: Vec<u8>, pub exit_code: Option<i32>, pub failure: Option<&'static str> }
impl Drop for HelperOutput { fn drop(&mut self) { self.stdout.fill(0); self.stderr.fill(0); } }
impl fmt::Debug for HelperOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.debug_struct("HelperOutput").field("stdout_len", &self.stdout.len()).field("stderr_len", &self.stderr.len()).field("exit_code", &self.exit_code).field("failure", &self.failure).finish() }
}
#[derive(Default)]
struct Capture { bytes: Vec<u8>, overflow: bool, failed: bool }
impl Drop for Capture { fn drop(&mut self) { self.bytes.fill(0); } }
type Reader = (std::thread::JoinHandle<()>, std::sync::Arc<std::sync::Mutex<Capture>>);
fn capture(mut pipe: impl Read + Send + 'static, max: usize) -> Reader {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Capture::default()));
    let state = shared.clone();
    let thread = std::thread::spawn(move|| {
        let mut bytes = [0_u8; 8192];
        loop {
            match pipe.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => if let Ok(mut capture) = state.lock() {
                    let keep = max.saturating_sub(capture.bytes.len()).min(count);
                    capture.bytes.extend_from_slice(&bytes[..keep]);
                    capture.overflow |= count > keep;
                } else { break; },
                Err(_) => { if let Ok(mut capture) = state.lock() { capture.failed = true; } break; }
            }
            bytes.fill(0);
        }
        bytes.fill(0);
    });
    (thread, shared)
}
fn collect(reader: Reader, deadline: Instant) -> (Vec<u8>, bool) {
    let (thread, shared) = reader;
    while !thread.is_finished() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(2)); }
    let finished = thread.is_finished();
    if finished { let _ = thread.join(); }
    match shared.lock() {
        Ok(mut capture) => (std::mem::take(&mut capture.bytes), !finished || capture.overflow || capture.failed),
        Err(_) => (Vec::new(), true),
    }
}

pub fn exchange_process(command: &mut Command, frame: &[u8], timeout: Duration) -> Result<HelperOutput> {
    ensure!(frame.len() <= MAX_REQUEST, "provision stdin bound exceeded");
    ensure!(!timeout.is_zero() && timeout <= HELPER_TIMEOUT, "invalid provision timeout");
    let deadline = Instant::now() + timeout;
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
        .map_err(|_|anyhow::anyhow!("provision helper spawn failed"))?;
    let mut stdin = child.stdin.take().context("helper stdin unavailable")?;
    let stdout = capture(child.stdout.take().context("helper stdout unavailable")?, MAX_RESULT);
    let stderr = capture(child.stderr.take().context("helper stderr unavailable")?, MAX_STDERR);
    let owned = SecretBytes(frame.to_vec());
    let writer = std::thread::spawn(move||stdin.write_all(&owned).is_err());
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => { let _ = child.kill(); break child.wait().ok(); }
        }
    };
    let timed_out = Instant::now() >= deadline;
    while !writer.is_finished() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(2)); }
    let writer_failed = if writer.is_finished() {
        writer.join().unwrap_or(true)
    } else {
        // Dropping a blocked writer detaches it; joining here would let a helper
        // that never reads stdin extend the Manager timeout without a bound.
        true
    };
    let (stdout, stdout_failed) = collect(stdout, deadline);
    let (stderr, stderr_failed) = collect(stderr, deadline);
    let failure = if timed_out { Some("helper_timeout") }
        else if writer_failed || stdout_failed || stderr_failed { Some("helper_io_bound_or_error") }
        else if status.is_none() { Some("helper_process_error") } else { None };
    Ok(HelperOutput { stdout, stderr, exit_code: status.and_then(|status|status.code()), failure })
}
