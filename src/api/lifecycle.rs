use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[cfg(feature = "daemon")]
use std::time::Duration;

#[cfg(feature = "daemon")]
use crate::api::{bridge::BridgeOwner, http::EventHubAccess};

use crate::config::Mode;

/// Accept/command ingress owned by the lifecycle. Shutdown closes it before
/// the core is stopped, so no new work can enter once stopping began.
#[derive(Debug, Default)]
pub struct Ingress {
    closed: AtomicBool,
}

impl Ingress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` once, for the caller that actually closed ingress.
    pub fn close(&self) -> bool {
        !self.closed.swap(true, Ordering::AcqRel)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonArgs {
    pub state_dir: PathBuf,
    pub port: u16,
    pub mode: Mode,
}

impl DaemonArgs {
    pub fn parse<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut state_dir = None;
        let mut port = 0u16;
        let mut mode = None;
        let mut values = args.into_iter();
        while let Some(raw) = values.next() {
            let arg = raw.as_ref();
            match arg {
                "--state-dir" => {
                    let value = values.next().context("--state-dir requires a value")?;
                    ensure!(!value.as_ref().is_empty(), "--state-dir requires a value");
                    ensure!(state_dir.replace(PathBuf::from(value.as_ref())).is_none(), "--state-dir specified more than once");
                }
                "--port" => {
                    let value = values.next().context("--port requires a value")?;
                    port = value.as_ref().parse::<u16>().context("--port must be between 0 and 65535")?;
                }
                "--real" => set_mode(&mut mode, Mode::Real)?,
                "--local" => set_mode(&mut mode, Mode::Local)?,
                "-h" | "--help" => bail!(usage()),
                other if other.starts_with('-') => bail!("unknown argument"),
                _ => bail!("unexpected argument"),
            }
        }
        let state_dir = state_dir.context("--state-dir is required")?;
        validate_path_shape(&state_dir)?;
        Ok(Self { state_dir, port, mode: mode.unwrap_or(Mode::Real) })
    }
}

fn set_mode(mode: &mut Option<Mode>, next: Mode) -> Result<()> {
    ensure!(mode.is_none(), "--real and --local are mutually exclusive");
    *mode = Some(next);
    Ok(())
}

fn usage() -> &'static str {
    "usage: agentranchd.exe --state-dir <absolute-new-directory> [--port <0..65535>] [--real | --local]"
}

fn validate_path_shape(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "state directory must be an absolute path");
    ensure!(path.components().all(|component| !matches!(component, Component::CurDir | Component::ParentDir)), "state directory must not contain traversal components");
    for component in path.components() {
        if let Component::Prefix(prefix) = component {
            ensure!(!matches!(prefix.kind(), std::path::Prefix::UNC(_, _) | std::path::Prefix::VerbatimUNC(_, _)), "UNC state directories are not supported");
        }
    }
    Ok(())
}

fn ensure_no_reparse_ancestors(path: &Path) -> Result<()> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) => {
                ensure!(!metadata.file_type().is_symlink(), "state directory path contains a symbolic link");
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                    ensure!(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0, "state directory path contains a reparse point");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect state directory ancestors"),
        }
        current = candidate.parent();
    }
    Ok(())
}

pub fn validate_fresh_state_dir(path: &Path) -> Result<()> {
    validate_path_shape(path)?;
    ensure_no_reparse_ancestors(path)?;
    ensure!(!path.exists(), "state directory already exists; use a new directory for each daemon instance");
    let parent = path.parent().context("state directory has no parent")?;
    ensure!(parent.is_dir(), "state directory parent must already exist");
    Ok(())
}

pub fn create_isolated_state_dir(path: &Path) -> Result<()> {
    validate_fresh_state_dir(path)?;
    std::fs::create_dir(path).context("create isolated state directory")?;
    std::fs::create_dir(path.join("imports")).context("create isolated imports directory")?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EndpointMetadata {
    schema: String,
    url: String,
    pid: u32,
    started_at: u64,
    instance_id: String,
}

// A fresh state directory excludes competing daemon processes. This lock
// serializes this module's in-process replacement and owner cleanup only;
// the state directory's parent must still be trusted.
static ENDPOINT_FILE_LOCK: Mutex<()> = Mutex::new(());

pub struct EndpointFileGuard {
    path: PathBuf,
    instance_id: String,
}

impl EndpointFileGuard {
    pub fn publish(state_dir: &Path, url: &str, pid: u32) -> Result<Self> {
        Self::publish_with_instance_id(state_dir, url, pid, Uuid::new_v4())
    }

    fn publish_for_runtime(state_dir: &Path, url: &str, pid: u32, instance_id: Uuid) -> Result<Self> {
        Self::publish_with_instance_id(state_dir, url, pid, instance_id)
    }

    fn publish_with_instance_id(state_dir: &Path, url: &str, pid: u32, instance_id: Uuid) -> Result<Self> {
        ensure!(state_dir.is_dir(), "state directory is not available");
        let instance_id = instance_id.to_string();
        let metadata = EndpointMetadata {
            schema: "api-endpoint/1".to_owned(),
            url: url.to_owned(),
            pid,
            started_at: unix_seconds(),
            instance_id: instance_id.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&metadata).context("encode endpoint metadata")?;
        let path = state_dir.join("api-endpoint.json");
        let temporary = state_dir.join(format!(".api-endpoint-{}.tmp", Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)
                .context("create endpoint temporary file")?;
            use std::io::Write;
            file.write_all(&bytes).context("write endpoint metadata")?;
            file.sync_all().context("flush endpoint metadata")?;
            drop(file);
            let _endpoint_lock = ENDPOINT_FILE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
            #[cfg(test)]
            tests::before_endpoint_change();
            atomic_replace(&temporary, &path)?;
            Ok::<(), anyhow::Error>(())
        })();
        if result.is_err() { let _ = std::fs::remove_file(&temporary); }
        result?;
        Ok(Self { path, instance_id })
    }
}

impl Drop for EndpointFileGuard {
    fn drop(&mut self) {
        let _endpoint_lock = ENDPOINT_FILE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let current = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<EndpointMetadata>(&bytes).ok());
        if current.as_ref().is_some_and(|metadata| metadata.instance_id == self.instance_id) {
            #[cfg(test)]
            tests::before_endpoint_change();
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
        ensure!(unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } != 0, "publish endpoint metadata");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(source, destination).context("publish endpoint metadata")
    }
}

pub struct DaemonRuntime {
    /// Taken once by the serving assembly; the bound address is kept so the
    /// endpoint URL never changes when the listener moves to Tokio.
    listener: Option<TcpListener>,
    addr: SocketAddr,
    state_dir: PathBuf,
    instance_id: Uuid,
    endpoint: Option<EndpointFileGuard>,
    ready: bool,
}

impl DaemonRuntime {
    pub fn start(args: DaemonArgs) -> Result<Self> {
        validate_fresh_state_dir(&args.state_dir)?;
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.port))
            .context("bind daemon loopback listener")?;
        let addr = listener.local_addr().context("read daemon listener address")?;
        create_isolated_state_dir(&args.state_dir)?;
        Ok(Self {
            listener: Some(listener),
            addr,
            state_dir: args.state_dir,
            instance_id: Uuid::new_v4(),
            endpoint: None,
            ready: false,
        })
    }

    /// Called by the runtime assembly owner only after the HTTP accept loop,
    /// bridge, and published initial snapshot are ready. Task 2 tests this
    /// transition in isolation; the unassembled executable never calls it.
    pub fn mark_ready(&mut self) {
        self.ready = true;
    }

    pub fn publish_endpoint(&mut self) -> Result<()> {
        self.publish_endpoint_with(self.instance_id)
    }

    /// Publishes discovery under the transport identity clients verify via
    /// `GET /v1/health`. The daemon passes the hub's instance id so the
    /// discovery file and every SSE/health identity agree.
    pub fn publish_endpoint_with(&mut self, instance_id: Uuid) -> Result<()> {
        ensure!(self.ready, "runtime is not ready to publish endpoint");
        ensure!(self.endpoint.is_none(), "endpoint is already published");
        let url = format!("http://127.0.0.1:{}", self.local_addr().port());
        let guard = EndpointFileGuard::publish_for_runtime(&self.state_dir, &url, std::process::id(), instance_id)?;
        self.endpoint = Some(guard);
        Ok(())
    }

    pub fn endpoint_path(&self) -> PathBuf { self.state_dir.join("api-endpoint.json") }
    pub fn instance_id(&self) -> Uuid { self.instance_id }
    pub fn local_addr(&self) -> SocketAddr { self.addr }

    /// Moves the bound std listener into the serving assembly exactly once.
    pub fn take_listener(&mut self) -> Result<TcpListener> {
        self.listener.take().context("daemon listener is not available")
    }

    /// Moves the discovery-file guard into the single shutdown path, which
    /// drops it after the stopped snapshot was drained.
    pub fn take_endpoint(&mut self) -> Option<EndpointFileGuard> {
        self.endpoint.take()
    }
}

/// Single shutdown owner. Every signal and internal timer must route here; no
/// HTTP connection or timer thread may stop or disconnect the core itself.
#[cfg(feature = "daemon")]
pub struct ShutdownState {
    ingress: Arc<Ingress>,
    owner: BridgeOwner,
    events: Arc<EventHubAccess>,
    endpoint: Option<EndpointFileGuard>,
}

#[cfg(feature = "daemon")]
impl ShutdownState {
    pub fn new(
        ingress: Arc<Ingress>,
        owner: BridgeOwner,
        events: Arc<EventHubAccess>,
        endpoint: Option<EndpointFileGuard>,
    ) -> Self {
        Self { ingress, owner, events, endpoint }
    }

    pub fn ingress(&self) -> &Arc<Ingress> {
        &self.ingress
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownOutcome {
    /// The owner did not finish inside the budget. The process must exit
    /// nonzero; no receipt or success evidence may be invented.
    pub timed_out: bool,
    pub owner_finished: bool,
}

/// One owner-controlled stop/disconnect path.
///
/// Ordering: close ingress, request the core stop once, let the owner drain
/// real receipts and the stopped snapshot, end SSE, remove this process's
/// discovery file. Only the owner thread performs the blocking disconnect, so
/// waits never stack on the HTTP or timer side.
#[cfg(feature = "daemon")]
pub async fn shutdown_with_deadline(state: ShutdownState, deadline: Duration) -> ShutdownOutcome {
    let ShutdownState { ingress, owner, events, endpoint } = state;
    ingress.close();
    owner.request_stop();
    let joined = tokio::time::timeout(
        deadline,
        tokio::task::spawn_blocking(move || owner.join()),
    )
    .await;
    events.close();
    drop(endpoint);
    match joined {
        Ok(result) => ShutdownOutcome {
            timed_out: false,
            owner_finished: matches!(result, Ok(Some(_))),
        },
        Err(_) => ShutdownOutcome { timed_out: true, owner_finished: false },
    }
}

#[cfg(feature = "daemon")]
static CONSOLE_SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[cfg(all(feature = "daemon", windows))]
unsafe extern "system" fn console_control_handler(event: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    if event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT {
        CONSOLE_SHUTDOWN.store(true, Ordering::SeqCst);
        1
    } else {
        // Close/logoff/shutdown keep their default handling.
        0
    }
}

/// Registers Ctrl+C / Ctrl+Break through `SetConsoleCtrlHandler`. The handler
/// only raises the flag that the serving loop routes into
/// [`shutdown_with_deadline`]; no Tokio signal driver is used.
#[cfg(feature = "daemon")]
pub fn install_console_shutdown() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        unsafe { SetConsoleCtrlHandler(Some(console_control_handler), 1) != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(feature = "daemon")]
pub fn console_shutdown_requested() -> bool {
    CONSOLE_SHUTDOWN.load(Ordering::SeqCst)
}

fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
#[path = "../../tests/support/daemon_temp.rs"]
mod daemon_temp;

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, sync::mpsc, thread, time::Duration};

    use super::{ENDPOINT_FILE_LOCK, EndpointFileGuard, create_isolated_state_dir, daemon_temp};

    thread_local! {
        static BEFORE_CHANGE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(super) fn before_endpoint_change() {
        let callback = BEFORE_CHANGE.with(|hook| hook.take());
        if let Some(callback) = callback {
            callback();
        }
    }

    #[test]
    fn owner_cleanup_and_replacement_share_one_critical_section() {
        let root = daemon_temp::temp_root();
        let state = root.path().join("state");
        create_isolated_state_dir(&state).unwrap();
        let first = EndpointFileGuard::publish(&state, "http://127.0.0.1:1", 1).unwrap();
        let (checked_tx, checked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (removed_tx, removed_rx) = mpsc::channel();
        let cleanup = thread::spawn(move || {
            BEFORE_CHANGE.set(Some(Box::new(move || {
                checked_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
            })));
            drop(first);
            let _ = removed_tx.send(());
        });
        checked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (contention_tx, contention_rx) = mpsc::channel();
        let replacement_state = state.clone();
        let publish = thread::spawn(move || {
            // If cleanup does not hold the lock, finish publishing while it
            // still has the old identity; otherwise let it finish first.
            let blocked = matches!(ENDPOINT_FILE_LOCK.try_lock(), Err(std::sync::TryLockError::WouldBlock));
            if blocked {
                contention_tx.send(true).unwrap();
                removed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let next = EndpointFileGuard::publish(&replacement_state, "http://127.0.0.1:2", 2).unwrap();
            if !blocked {
                contention_tx.send(false).unwrap();
            }
            next
        });
        let contention = contention_rx.recv_timeout(Duration::from_secs(5));
        drop(release_tx);
        cleanup.join().unwrap();
        let next = publish.join().unwrap();
        let path = state.join("api-endpoint.json");
        assert!(path.exists(), "old owner removed the replacement endpoint");
        assert!(contention.unwrap(), "cleanup must hold the endpoint lock through removal");
        let endpoint: super::EndpointMetadata = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(endpoint.pid, 2);
        drop(next);
        assert!(!path.exists());
    }
}

