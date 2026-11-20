//! Single-worker background SSH gateway. Store admission is the sole release authority.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io::{Read, Write}, path::{Path, PathBuf}, process::{Command, Stdio}, sync::{Arc, mpsc, atomic::{AtomicBool, Ordering}}, time::Duration};

const PARAMIKO_MAGIC: &[u8] = b"AO-PARAMIKO-GATEWAY/1\n";
const PARAMIKO_MAX_METADATA: usize = 64 * 1024;
const PARAMIKO_MAX_KEY: usize = 64 * 1024;
const PARAMIKO_MAX_TOKEN: usize = 16 * 1024;
const PARAMIKO_MAX_FRAME: usize = 192 * 1024;
const PARAMIKO_HELPER: &str = "tools/paramiko_gateway.py";
const PARAMIKO_RUNTIME: &str = "runtime/python/python.exe";
const DELEGATED_SETUP_SECS: u64 = 15;
const DELEGATED_GRACE_SECS: u64 = 5;
const DELEGATED_STOP_SECS: u64 = 5;
const DELEGATED_LOCAL_SECS: u64 = 5;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GatewayLaunchMode {
    Direct,
    DelegatedUserService,
}

impl Default for GatewayLaunchMode {
    fn default() -> Self { Self::Direct }
}

pub fn default_paramiko_helper() -> Result<PathBuf> {
    let executable=std::env::current_exe().context("current executable path unavailable")?;
    let parent=executable.parent().context("current executable directory unavailable")?;
    let helper=parent.join(PARAMIKO_HELPER);
    ensure!(helper.is_file(),"Paramiko helper is missing beside bundled executable");
    Ok(helper)
}

fn bundled_python_interpreter() -> Result<PathBuf> {
    ensure!(cfg!(windows),"bundled Paramiko runtime unavailable on this platform");
    let executable=std::env::current_exe().context("current executable path unavailable")?;
    let parent=executable.parent().context("current executable directory unavailable")?;
    Ok(parent.join(PARAMIKO_RUNTIME))
}
use super::{protocol::{Envelope,ExecutionIdentity},session_credentials::CredentialLease,store::Store};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshGatewayConfig {
    pub schema:String, pub deployment:String, pub worker_id:String, pub host:String,
    pub port:u16, pub user:String, pub known_hosts:PathBuf, #[serde(default)] pub identity_file:PathBuf,
    #[serde(default)] pub controller_config:PathBuf, pub key_file:PathBuf, pub state_dir:PathBuf, pub remote_worker:String, pub remote_config:String,
    pub artifact_sha256:String, pub artifact_bytes:u64, pub timeout_secs:u64,
    #[serde(default)]
    pub launch_mode:GatewayLaunchMode,
    /// Assert remote worker timeout; never rewrite or clamp the worker config.
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub expected_worker_timeout_secs:Option<u64>,
    /// One operator-approved per-user directory, not a search list or inherited shell value.
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub remote_library_path:Option<String>,
    /// Expected identity of the selected libsqlite3.so.0 in that directory.
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub remote_library_sha256:Option<String>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub remote_library_bytes:Option<u64>,
}
impl SshGatewayConfig {
    pub fn validate(&self)->Result<()> {
        ensure!(self.schema=="native-ssh-gateway/1","unsupported gateway schema");
        ensure!(crate::config::valid_id(&self.deployment)&&crate::config::valid_id(&self.worker_id),"invalid gateway identity");
        ensure!(safe_host(&self.host)&&safe_user(&self.user)&&self.user!="root","ordinary user and explicit safe host required");
        ensure!(self.port>0&&(1..=300).contains(&self.timeout_secs),"invalid SSH port/timeout");
        match (self.launch_mode, self.expected_worker_timeout_secs) {
            (GatewayLaunchMode::Direct, None) => {},
            (GatewayLaunchMode::DelegatedUserService, Some(worker)) => {
                ensure!((1..=270).contains(&worker), "expected worker timeout must be 1..270");
                ensure!(self.timeout_secs >= worker + DELEGATED_GRACE_SECS + DELEGATED_SETUP_SECS + DELEGATED_STOP_SECS + DELEGATED_LOCAL_SECS, "outer timeout must be at least worker timeout + 30");
            }
            _ => anyhow::bail!("expected_worker_timeout_secs is required only for delegated mode"),
        }
        ensure!(linux_path(&self.remote_worker)&&linux_path(&self.remote_config),"absolute safe Linux remote paths required");
        match (&self.remote_library_path, &self.remote_library_sha256, self.remote_library_bytes) {
            (Some(path), Some(sha), Some(bytes)) => {
                ensure!(library_path(path),"remote_library_path requires one absolute normalized approved directory");
                ensure!(sha.len()==64&&sha.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)),"remote_library_sha256 must be lowercase SHA256");
                ensure!(bytes>0,"remote_library_bytes must be positive");
            }
            (None, None, None) => {}
            _ => anyhow::bail!("remote library path and identity must be supplied together"),
        }
        ensure!(self.artifact_sha256.len()==64&&self.artifact_sha256.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))&&self.artifact_bytes>0,"artifact SHA/bytes required");
        ensure!(!self.controller_config.as_os_str().is_empty(),"controller config path is required");
        for path in [&self.known_hosts,&self.key_file,&self.state_dir] {ensure!(!path.as_os_str().is_empty(),"explicit local path required");}
        Ok(())
    }
    pub fn load(path:&Path)->Result<Self>{
        use std::io::Read;
        let mut raw=Vec::new();std::fs::File::open(path)?.take(65537).read_to_end(&mut raw)?;
        ensure!(raw.len()<=65536,"gateway config exceeds 64KiB");
        let config:Self=serde_json::from_slice(&raw)?;config.validate()?;Ok(config)
    }
}
fn safe_host(value:&str)->bool{!value.is_empty()&&!value.starts_with('-')&&value.bytes().all(|b|b.is_ascii_alphanumeric()||b".-:[]".contains(&b))}
fn safe_user(value:&str)->bool{!value.is_empty()&&!value.starts_with('-')&&value.bytes().all(|b|b.is_ascii_alphanumeric()||b"_-".contains(&b))}
fn linux_path(value:&str)->bool{value.starts_with('/')&&!value.split('/').any(|part|part=="..")&&value.bytes().all(|b|b.is_ascii_alphanumeric()||b"/._-".contains(&b))}
fn library_path(value:&str)->bool {
    linux_path(value)&&value.len()>1&&value[1..].split('/').all(|part|!part.is_empty()&&part!=".")
}
pub fn ssh_arguments(config:&SshGatewayConfig)->Result<Vec<String>> {
    config.validate()?;
    ensure!(!config.identity_file.as_os_str().is_empty(),"identity_file is required for legacy OpenSSH arguments");
    let mut args=vec!["-F".into(),"none".into(),"-T".into(),"-a".into(),"-x".into()];
    for option in ["BatchMode=yes","StrictHostKeyChecking=yes","IdentitiesOnly=yes","GlobalKnownHostsFile=none","PasswordAuthentication=no","KbdInteractiveAuthentication=no","ConnectTimeout=15","ClearAllForwardings=yes","PermitLocalCommand=no","ProxyCommand=none","ProxyJump=none","ControlMaster=no","ControlPath=none"] {
        args.extend(["-o".into(),option.into()]);
    }
    args.extend(["-o".into(),format!("UserKnownHostsFile={}",config.known_hosts.display()),"-p".into(),config.port.to_string(),"-i".into(),config.identity_file.display().to_string(),format!("{}@{}",config.user,config.host)]);
    args.push(build_remote_command(config)?);
    Ok(args)
}
pub struct SshWorkerTransport {
    config:SshGatewayConfig,
    helper:PathBuf,
    interpreter:PathBuf,
    private_key:Vec<u8>,
    model_token:Vec<u8>,
    cancel:Option<Arc<AtomicBool>>,
}
impl SshWorkerTransport {
    pub fn new(config:SshGatewayConfig)->Result<Self>{
        let helper=default_paramiko_helper()?;
        Self::with_paramiko(config,helper,Vec::new())
    }
    pub fn with_paramiko(config:SshGatewayConfig,helper:PathBuf,private_key:Vec<u8>)->Result<Self>{
        let interpreter=bundled_python_interpreter()?;
        Self::with_paramiko_runtime(config,helper,interpreter,private_key)
    }
    /// Same in-memory guarantees as `with_paramiko`, plus the model token as a
    /// third binary frame field (never in JSON metadata, argv, or env).
    pub fn with_paramiko_token(config:SshGatewayConfig,helper:PathBuf,private_key:Vec<u8>,model_token:Vec<u8>)->Result<Self>{
        let interpreter=bundled_python_interpreter()?;
        Self::with_paramiko_runtime_token(config,helper,interpreter,private_key,model_token)
    }
    /// Consume one in-process lease into this short-lived transport only.
    pub fn with_credential_lease(config:SshGatewayConfig,helper:PathBuf,lease:CredentialLease,require_token:bool)->Result<Self>{
        Self::with_credential_lease_inner(config,helper,bundled_python_interpreter()?,lease,require_token,None)
    }
    pub fn with_credential_lease_cancellable(config:SshGatewayConfig,helper:PathBuf,lease:CredentialLease,require_token:bool,cancel:Arc<AtomicBool>)->Result<Self>{
        Self::with_credential_lease_inner(config,helper,bundled_python_interpreter()?,lease,require_token,Some(cancel))
    }
    #[doc(hidden)]
    pub fn with_credential_lease_for_test(config:SshGatewayConfig,helper:PathBuf,interpreter:PathBuf,lease:CredentialLease,require_token:bool)->Result<Self>{
        Self::with_credential_lease_inner(config,helper,interpreter,lease,require_token,None)
    }
    fn with_credential_lease_inner(config:SshGatewayConfig,helper:PathBuf,interpreter:PathBuf,lease:CredentialLease,require_token:bool,cancel:Option<Arc<AtomicBool>>)->Result<Self>{
        ensure!(lease.worker_key_len()>0,"Paramiko private key is unavailable");
        ensure!(!require_token||lease.has_model_token(),"model token is unavailable");
        let (private_key,model_token)=lease.into_transport_secrets();
        Self::with_paramiko_runtime_secrets(config,helper,interpreter,private_key,model_token,require_token,cancel)
    }
    pub fn with_paramiko_runtime(config:SshGatewayConfig,helper:PathBuf,interpreter:PathBuf,private_key:Vec<u8>)->Result<Self>{
        Self::with_paramiko_runtime_inner(config,helper,interpreter,private_key,Vec::new(),false,None)
    }
    pub fn with_paramiko_runtime_token(config:SshGatewayConfig,helper:PathBuf,interpreter:PathBuf,private_key:Vec<u8>,model_token:Vec<u8>)->Result<Self>{
        Self::with_paramiko_runtime_inner(config,helper,interpreter,private_key,model_token,true,None)
    }
    fn with_paramiko_runtime_secrets(config:SshGatewayConfig,helper:PathBuf,interpreter:PathBuf,mut private_key:super::provision::SecretBytes,mut model_token:Option<super::provision::SecretBytes>,require_token:bool,cancel:Option<Arc<AtomicBool>>)->Result<Self>{
        Self::validate_constructor(&config,&helper,&interpreter,private_key.len(),model_token.as_ref().map_or(0,|token|token.len()),require_token)?;
        let private_key=private_key.drain_bytes();
        let model_token=model_token.as_mut().map(super::provision::SecretBytes::drain_bytes).unwrap_or_default();
        Ok(Self{config,helper,interpreter,private_key,model_token,cancel})
    }
    fn with_paramiko_runtime_inner(config:SshGatewayConfig,helper:PathBuf,interpreter:PathBuf,private_key:Vec<u8>,model_token:Vec<u8>,require_token:bool,cancel:Option<Arc<AtomicBool>>)->Result<Self>{
        Self::validate_constructor(&config,&helper,&interpreter,private_key.len(),model_token.len(),require_token)?;
        Ok(Self{config,helper,interpreter,private_key,model_token,cancel})
    }
    fn validate_constructor(config:&SshGatewayConfig,helper:&Path,interpreter:&Path,private_key_len:usize,model_token_len:usize,require_token:bool)->Result<()> {
        config.validate()?;
        ensure!(helper.is_absolute(),"Paramiko helper path must be absolute");
        ensure!(helper.is_file(),"Paramiko helper script is missing");
        if cfg!(windows) { ensure!(interpreter.is_file(),"bundled Python runtime is missing: runtime/python/python.exe"); }
        ensure!(private_key_len>0,"Paramiko private key is unavailable");
        ensure!(private_key_len<=PARAMIKO_MAX_KEY,"Paramiko private key is oversized");
        if require_token { ensure!(model_token_len>0,"model token is unavailable");ensure!(model_token_len<=PARAMIKO_MAX_TOKEN,"model token is oversized"); }
        Ok(())
    }
    pub fn command_for_test(&self)->Result<Command>{
        self.build_command()
    }
    pub fn frame_for_test(&self,request:&[u8])->Result<Vec<u8>>{
        self.request_frame(request)
    }
    fn build_command(&self)->Result<Command>{
        let mut command=Command::new(&self.interpreter);
        command.args(["-I","-B",self.helper.to_string_lossy().as_ref()]);
        command.env_clear();
        #[cfg(windows)]
        {
            let system_root = std::env::var("SystemRoot")
                .context("Windows SystemRoot environment is required for bundled Python runtime")?;
            command.env("SystemRoot", system_root);
        }
        Ok(command)
    }
    fn request_frame(&self,request:&[u8])->Result<Vec<u8>>{
        ensure!(!self.private_key.is_empty(),"Paramiko private key is unavailable");
        ensure!(self.private_key.len()<=PARAMIKO_MAX_KEY,"Paramiko private key is oversized");
        let mut metadata=serde_json::json!({
            "schema":"ao-paramiko-gateway/1",
            "host":self.config.host.clone(),
            "port":self.config.port,
            "user":self.config.user.clone(),
            "known_hosts":self.config.known_hosts.display().to_string(),
            "remote_command":build_remote_command(&self.config)?,
            "timeout_secs": if self.config.launch_mode == GatewayLaunchMode::DelegatedUserService { let w=self.config.expected_worker_timeout_secs.expect("validated delegated timeout"); w + DELEGATED_SETUP_SECS + DELEGATED_GRACE_SECS + DELEGATED_STOP_SECS } else { self.config.timeout_secs },
            "request":String::from_utf8(request.to_vec()).context("signed request is not UTF-8")?,
            "setup_timeout_secs": if self.config.launch_mode == GatewayLaunchMode::DelegatedUserService { Some(DELEGATED_SETUP_SECS) } else { None },
            "channel_timeout_secs": if self.config.launch_mode == GatewayLaunchMode::DelegatedUserService { Some(self.config.expected_worker_timeout_secs.expect("validated delegated timeout") + DELEGATED_GRACE_SECS + DELEGATED_STOP_SECS) } else { None },
        });
        if self.config.launch_mode == GatewayLaunchMode::Direct {
            let object=metadata.as_object_mut().context("invalid transport metadata")?;
            object.remove("setup_timeout_secs");
            object.remove("channel_timeout_secs");
        }
        let metadata=serde_json::to_vec(&metadata)?;
        ensure!(metadata.len()<=PARAMIKO_MAX_METADATA,"Paramiko metadata is oversized");
        let mut frame_len=PARAMIKO_MAGIC.len()+4+metadata.len()+4+self.private_key.len();
        if !self.model_token.is_empty() { frame_len+=4+self.model_token.len(); }
        ensure!(frame_len<=PARAMIKO_MAX_FRAME,"Paramiko frame is oversized");
        let mut frame=Vec::with_capacity(frame_len);
        frame.extend_from_slice(PARAMIKO_MAGIC);
        frame.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
        frame.extend_from_slice(&metadata);
        frame.extend_from_slice(&(self.private_key.len() as u32).to_be_bytes());
        frame.extend_from_slice(&self.private_key);
        if !self.model_token.is_empty() {
            frame.extend_from_slice(&(self.model_token.len() as u32).to_be_bytes());
            frame.extend_from_slice(&self.model_token);
        }
        Ok(frame)
    }
}
impl WorkerTransport for SshWorkerTransport {
    fn exchange(&mut self,request:&[u8],timeout_secs:u64)->Result<TransportOutput>{
        self.exchange_stream(request,timeout_secs,&mut |_| {})
    }
    fn exchange_stream(&mut self,request:&[u8],timeout_secs:u64,emit:&mut dyn FnMut(super::live_output::LiveOutput))->Result<TransportOutput>{
        ensure!(!self.cancel.as_ref().is_some_and(|flag|flag.load(Ordering::Acquire)),"transport cancelled before launch");
        let mut command=self.build_command()?;
        ensure!(!self.cancel.as_ref().is_some_and(|flag|flag.load(Ordering::Acquire)),"transport cancelled before request frame");
        let frame=self.request_frame(request)?;
        exchange_process_budgeted(&mut command,&frame,Duration::from_secs(timeout_secs),self.config.launch_mode == GatewayLaunchMode::DelegatedUserService,self.cancel.as_deref(),Some(emit))
    }
}
impl Drop for SshWorkerTransport {
    fn drop(&mut self) { self.private_key.fill(0); if !self.model_token.is_empty() { self.model_token.fill(0); } }
}

fn build_remote_command(config:&SshGatewayConfig)->Result<String>{
    let checks=format!(
        "test \"$(sha256sum {} | cut -d' ' -f1)\" = \"{}\" && test \"$(wc -c < {})\" -eq {}",
        config.remote_worker, config.artifact_sha256, config.remote_worker, config.artifact_bytes,
    );
    let worker_exec=format!("{} owner-exec --config {}", config.remote_worker, config.remote_config);
    let library_checks=config.remote_library_path.as_ref().map(|path|format!(
        "test -d {path} && test -r {path}/libsqlite3.so.0 && test \"$(sha256sum {path}/libsqlite3.so.0 | cut -d' ' -f1)\" = \"{}\" && test \"$(wc -c < {path}/libsqlite3.so.0)\" -eq {}",
        config.remote_library_sha256.as_deref().unwrap_or_default(), config.remote_library_bytes.unwrap_or_default(),
    ));
    match config.launch_mode {
        GatewayLaunchMode::Direct => {
            let loader=library_checks.map(|checks|format!("{checks} && LD_LIBRARY_PATH={} ", config.remote_library_path.as_deref().unwrap_or_default())).unwrap_or_default();
            Ok(format!("{checks} && {loader}exec {worker_exec}"))
        }
        GatewayLaunchMode::DelegatedUserService => {
            let worker_timeout=config.expected_worker_timeout_secs.context("delegated worker timeout missing")?;
            let runtime=worker_timeout + DELEGATED_GRACE_SECS;
            let loader_checks=library_checks.map(|value|format!(" && {value}")).unwrap_or_default();
            let loader_env=config.remote_library_path.as_ref().map(|path|format!(" --setenv=LD_LIBRARY_PATH={path}" )).unwrap_or_default();
            Ok(format!(
                "{checks}{loader_checks} && exec /usr/bin/systemd-run --user --pipe --wait --collect --quiet --property=Delegate=yes --property=RuntimeMaxSec={}s --property=TimeoutStopSec=5s --setenv=AW_EXPECTED_OWNER_TIMEOUT_SECS={}{} -- {worker_exec}",
                runtime, worker_timeout, loader_env,
            ))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayRequest {pub identity:ExecutionIdentity,pub start:Envelope}
impl GatewayRequest {
    pub fn from_store(store:&Store,config:&SshGatewayConfig,run_id:&str)->Result<Self>{
        let start=store.pending_outbox(100)?.into_iter().find(|(_,m)|m.run_id==run_id&&m.kind=="start").context("pending signed start missing")?.1;
        let mut identity=start.execution_identity()?;
        identity.session=Some(start.payload["session"].as_str().map(str::to_owned).unwrap_or_else(||format!("native-{}",start.message_id)));
        ensure!(start.to==config.worker_id,"start worker route mismatch");
        Ok(Self{identity,start})
    }
    fn validate(&self,config:&SshGatewayConfig,store:&Store)->Result<()>{
        self.identity.validate()?;
        ensure!(self.identity.deployment==config.deployment&&self.start.deployment==config.deployment,"gateway deployment mismatch");
        ensure!(self.start.to==config.worker_id&&self.start.from=="A"&&self.start.kind=="start"&&matches!(self.start.payload["template"].as_str(),Some("sum-v1"|"project-work-v1")),"only pinned sum-v1/project-work-v1 start supported");
        ensure!(self.start.payload["worker_artifact_sha256"]==config.artifact_sha256&&self.identity.artifact_sha256.as_deref()==Some(config.artifact_sha256.as_str()),"gateway worker artifact mismatch");
        ensure!(self.identity.run_id==self.start.run_id&&self.identity.attempt==self.start.attempt&&self.identity.epoch==self.start.epoch,"gateway execution mismatch");
        let mut durable_identity=self.start.execution_identity()?;
        durable_identity.session=Some(self.start.payload["session"].as_str().map(str::to_owned).unwrap_or_else(||format!("native-{}",self.start.message_id)));
        ensure!(durable_identity==self.identity,"gateway identity differs from durable start");
        let run=store.run(&self.identity.run_id)?;
        if run.session.is_some(){ensure!(run.execution_identity(&config.deployment)?==self.identity,"gateway bound execution identity mismatch");}
        ensure!(run.attempt==self.identity.attempt&&run.epoch==self.identity.epoch,"stale gateway execution");
        let assignment=store.assignment(&run.id,run.attempt)?.context("gateway assignment missing")?;
        ensure!(assignment.node_id==config.worker_id,"gateway assigned worker mismatch");
        Ok(())
    }
}
#[derive(Clone,Debug,PartialEq,Serialize)]
pub struct GatewayResult {pub run_id:String,pub status:String,pub reason:Option<String>,pub launch:Option<Value>,pub terminal:Option<Value>,pub slot_reserved:bool}
impl GatewayResult {
    fn unknown(run_id:impl Into<String>,reason:impl Into<String>)->Self{Self{run_id:run_id.into(),status:"unknown".into(),reason:Some(reason.into()),launch:None,terminal:None,slot_reserved:true}}
}
#[derive(Clone,Debug)]
pub struct TransportOutput {pub stdout:Vec<u8>,pub stderr:Vec<u8>,pub exit_code:Option<i32>,pub failure:Option<String>}
pub trait WorkerTransport {
    fn exchange(&mut self,request:&[u8],timeout_secs:u64)->Result<TransportOutput>;
    fn exchange_stream(&mut self,request:&[u8],timeout_secs:u64,_emit:&mut dyn FnMut(super::live_output::LiveOutput))->Result<TransportOutput> {
        self.exchange(request,timeout_secs)
    }
}
impl<T:WorkerTransport+?Sized> WorkerTransport for Box<T> {
    fn exchange(&mut self,request:&[u8],timeout_secs:u64)->Result<TransportOutput>{(**self).exchange(request,timeout_secs)}
    fn exchange_stream(&mut self,request:&[u8],timeout_secs:u64,emit:&mut dyn FnMut(super::live_output::LiveOutput))->Result<TransportOutput>{(**self).exchange_stream(request,timeout_secs,emit)}
}
#[derive(Clone)]
struct CaptureOutcome { bytes:Vec<u8>, overflow:bool, error:Option<String> }

type SharedCapture = std::sync::Arc<std::sync::Mutex<CaptureOutcome>>;

fn capture_bounded<R:std::io::Read+Send+'static>(mut reader:R,limit:usize)->(std::thread::JoinHandle<()>,SharedCapture) {
    let shared=std::sync::Arc::new(std::sync::Mutex::new(CaptureOutcome{bytes:Vec::new(),overflow:false,error:None}));
    let state=shared.clone();
    let handle=std::thread::spawn(move||{
        let mut buffer=[0_u8;8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0)=>break,
                Ok(count)=>{
                    if let Ok(mut value)=state.lock() {
                        let available=limit.saturating_sub(value.bytes.len());
                        if count>available { value.overflow=true; }
                        let keep=available.min(count);
                        value.bytes.extend_from_slice(&buffer[..keep]);
                    } else { break; }
                }
                Err(error)=>{
                    if let Ok(mut value)=state.lock() { value.error=Some(error.to_string()); }
                    break;
                }
            }
        }
    });
    (handle,shared)
}

fn collect_capture(
    reader:&mut Option<(std::thread::JoinHandle<()>,SharedCapture)>,
    deadline:std::time::Instant,
    label:&str,
)->CaptureOutcome {
    while reader.as_ref().is_some_and(|(handle,_)|!handle.is_finished()) {
        if std::time::Instant::now()>=deadline {
            let Some((_,shared))=reader.as_ref() else { break; };
            let mut value=shared.lock().map(|state|state.clone()).unwrap_or(CaptureOutcome{bytes:Vec::new(),overflow:false,error:Some(format!("{label} reader state unavailable"))});
            value.error=Some(format!("{label} reader deadline exceeded"));
            return value;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let Some((handle,shared))=reader.take() else { return CaptureOutcome{bytes:Vec::new(),overflow:false,error:Some(format!("{label} reader unavailable"))}; };
    let _=handle.join();
    shared.lock().map(|value|value.clone()).unwrap_or(CaptureOutcome{bytes:Vec::new(),overflow:false,error:Some(format!("{label} reader state unavailable"))})
}

pub fn exchange_process(command:&mut Command,request:&[u8],timeout:Duration)->Result<TransportOutput>{
    exchange_process_budgeted(command,request,timeout,false,None,None)
}

pub fn exchange_process_stream(command:&mut Command,request:&[u8],timeout:Duration,emit:&mut dyn FnMut(super::live_output::LiveOutput))->Result<TransportOutput>{
    exchange_process_budgeted(command,request,timeout,false,None,Some(emit))
}

fn transport_deadline(started:std::time::Instant,io_ready:std::time::Instant,timeout:Duration,delegated:bool)->std::time::Instant {
    if delegated { started + timeout } else { io_ready + timeout }
}

#[cfg(test)]
mod deadline_tests {
    #[test]
    fn delegated_includes_setup_while_direct_preserves_original_io_deadline() {
        let start=std::time::Instant::now();
        let ready=start+std::time::Duration::from_secs(2);
        let timeout=std::time::Duration::from_secs(10);
        assert_eq!(super::transport_deadline(start,ready,timeout,true), start+timeout);
        assert_eq!(super::transport_deadline(start,ready,timeout,false), ready+timeout);
    }
}

fn exchange_process_budgeted(command:&mut Command,request:&[u8],timeout:Duration,delegated:bool,cancel:Option<&AtomicBool>,mut emit:Option<&mut dyn FnMut(super::live_output::LiveOutput)>)->Result<TransportOutput>{
    ensure!(!cancel.is_some_and(|flag|flag.load(Ordering::Acquire)),"transport cancelled before spawn");
    let started=std::time::Instant::now();
    let mut child=command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("spawn worker transport process")?;
    let stdin=child.stdin.take().context("transport stdin unavailable")?;
    let stdout=child.stdout.take().context("transport stdout unavailable")?;
    let stderr=child.stderr.take().context("transport stderr unavailable")?;
    ensure!(!cancel.is_some_and(|flag|flag.load(Ordering::Acquire)),"transport cancelled before request write");
    let request=request.to_vec();
    let writer=std::thread::spawn(move||{let mut stdin=stdin;let result=stdin.write_all(&request).map_err(|error|error.to_string());drop(stdin);result});
    let mut stdout=Some(capture_bounded(stdout,1024*1024));
    let (live_tx, live_rx) = mpsc::channel::<super::live_output::LiveOutput>();
    let mut stderr_reader = stderr;
    let stderr_state = std::sync::Arc::new(std::sync::Mutex::new(CaptureOutcome{bytes:Vec::new(),overflow:false,error:None}));
    let stderr_shared = stderr_state.clone();
    let stderr_handle = std::thread::spawn(move || {
        let mut raw = [0_u8; 8192];
        let mut decoder = super::live_output::LiveLines::default();
        let mut live_ok = true;
        let mut diagnostic = |bytes: &[u8]| {
            if let Ok(mut state) = stderr_shared.lock() {
                let available = (64_usize * 1024).saturating_sub(state.bytes.len());
                let keep = available.min(bytes.len());
                state.bytes.extend_from_slice(&bytes[..keep]);
                state.overflow |= keep < bytes.len();
            }
        };
        loop {
            let decoded = match stderr_reader.read(&mut raw) {
                Ok(0) => {
                    if live_ok && decoder.finish(&mut diagnostic).is_err() {
                        let _ = live_tx.send(super::live_output::LiveOutput::Error("输出连接中断；保留已有输出。".into()));
                    }
                    break;
                }
                Ok(count) if live_ok => decoder.push(&raw[..count], &mut |event| {let _ = live_tx.send(event);}, &mut diagnostic),
                Ok(count) => {diagnostic(&raw[..count]); Ok(())},
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    if let Ok(mut state) = stderr_shared.lock() {state.error = Some(error.to_string());}
                    let _ = live_tx.send(super::live_output::LiveOutput::Error("输出读取失败；保留已有输出。".into()));
                    break;
                }
            };
            if decoded.is_err() {
                live_ok = false;
                let _ = live_tx.send(super::live_output::LiveOutput::Error("输出格式不完整；保留已有输出。".into()));
            }
        }
    });
    let mut stderr=Some((stderr_handle, stderr_state));
    let deadline=transport_deadline(started,std::time::Instant::now(),timeout,delegated);
    let mut timed_out=false;
    let mut cancelled=false;
    let status=loop {
        while let Ok(event) = live_rx.try_recv() { if let Some(callback) = emit.as_deref_mut() { callback(event); } }
        if let Some(status)=child.try_wait()? { break status; }
        if cancel.is_some_and(|flag|flag.load(Ordering::Acquire)) || std::time::Instant::now()>=deadline {
            cancelled=cancel.is_some_and(|flag|flag.load(Ordering::Acquire));
            timed_out=!cancelled;
            let _=child.kill();
            break child.wait().context("wait after transport cancellation/timeout")?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    while !writer.is_finished() && std::time::Instant::now()<deadline {
        while let Ok(event) = live_rx.try_recv() { if let Some(callback) = emit.as_deref_mut() { callback(event); } }
        std::thread::sleep(Duration::from_millis(2));
    }
    while let Ok(event) = live_rx.try_recv() { if let Some(callback) = emit.as_deref_mut() { callback(event); } }
    let writer_error=if writer.is_finished() { writer.join().ok().and_then(Result::err) } else { Some("transport stdin deadline exceeded".into()) };
    while let Ok(event) = live_rx.try_recv() { if let Some(callback) = emit.as_deref_mut() { callback(event); } }
    let stdout=collect_capture(&mut stdout,deadline,"stdout");
    let stderr=collect_capture(&mut stderr,deadline,"stderr");
    while let Ok(event) = live_rx.try_recv() { if let Some(callback) = emit.as_deref_mut() { callback(event); } }
    let stream_timeout=stdout.error.as_deref().is_some_and(|value|value.contains("deadline exceeded")) || stderr.error.as_deref().is_some_and(|value|value.contains("deadline exceeded"));
    let failure=if cancelled {Some("transport cancelled during Manager disconnect".into())}
        else if timed_out||stream_timeout||writer_error.is_some(){Some("transport timeout".into())}
        else if stdout.error.is_some()||stderr.error.is_some(){Some("transport stream reader failed".into())}
        else if stdout.overflow||stderr.overflow{Some("transport stream bound exceeded".into())}
        else if !status.success(){Some(format!("transport exited {}: {}",status,String::from_utf8_lossy(&stderr.bytes).trim()))}
        else{None};
    Ok(TransportOutput{stdout:stdout.bytes,stderr:stderr.bytes,exit_code:status.code(),failure})
}

fn admitted_result(store:&Store,request:&GatewayRequest)->Result<Option<GatewayResult>> {
    let run=store.run(&request.identity.run_id)?;
    let assignment=store.assignment(&run.id,run.attempt)?.context("gateway assignment missing")?;
    Ok((assignment.released&&run.detail["owner_admission"]=="admitted").then(||GatewayResult{
        run_id:run.id.clone(),status:run.status,reason:None,launch:None,terminal:None,slot_reserved:false}))
}
fn consume_events(raw:&[u8],request:&GatewayRequest,store:&mut Store,now:u64,result:&mut GatewayResult)->Result<()> {
    ensure!(raw.len()<=1024*1024,"worker output exceeds 1MiB");
    for line in raw.split_inclusive(|b|*b==b'\n') {
        ensure!(line.ends_with(b"\n")&&line.len()<=196609,"worker JSONL incomplete or oversized");
        let event:Envelope=serde_json::from_slice(&line[..line.len()-1]).context("invalid worker signed JSONL event")?;
        ensure!(event.deployment==request.identity.deployment&&event.run_id==request.identity.run_id&&event.attempt==request.identity.attempt&&event.epoch==request.identity.epoch,"worker execution mismatch");
        ensure!(event.payload["session"].as_str()==request.identity.session.as_deref(),"worker session mismatch");
        if event.kind=="status" {
            // The worker's only channel to report a failure reason before dying.
            // Persist it so the real cause reaches the controller ledger; it is
            // never admission evidence and never a launch/terminal phase.
            store.receive(&event,now)?;
            continue;
        }
        ensure!(event.kind=="report","worker kind mismatch");
        let phase=event.payload["phase"].as_str().context("worker phase missing")?;
        ensure!(matches!(phase,"launch"|"terminal"),"unsupported worker event phase");
        let target=if phase=="launch"{&mut result.launch}else{&mut result.terminal};
        ensure!(target.as_ref().is_none_or(|prior|prior==&event.payload),"conflicting duplicate {phase} event");
        store.receive(&event,now)?;
        *target=Some(event.payload);
    }
    Ok(())
}
pub fn run_once<T:WorkerTransport>(config:&SshGatewayConfig,request:&GatewayRequest,store:&mut Store,transport:&mut T,now:u64)->Result<GatewayResult>{
    run_once_stream(config,request,store,transport,now,&mut |_| {})
}
pub fn run_once_stream<T:WorkerTransport>(config:&SshGatewayConfig,request:&GatewayRequest,store:&mut Store,transport:&mut T,now:u64,emit:&mut dyn FnMut(super::live_output::LiveOutput))->Result<GatewayResult>{
    config.validate()?;request.validate(config,store)?;
    if let Some(result)=admitted_result(store,request)?{return Ok(result);}
    store.validate_gateway_start(&request.start,now)?;
    let session=request.identity.session.as_deref().context("session missing")?;
    let _=store.claim(&request.identity.run_id,request.identity.attempt,&request.identity.epoch)?;
    store.bind(&request.identity.run_id,request.identity.attempt,&request.identity.epoch,session)?;
    ensure!(store.run(&request.identity.run_id)?.execution_identity(&config.deployment)?==request.identity,"bound gateway identity mismatch");
    let mut frame=serde_json::to_vec(&request.start)?;frame.push(b'\n');
    let output=transport.exchange_stream(&frame,config.timeout_secs,emit);
    // Envelope freshness is checked at arrival, not at dispatch: the worker
    // signs its terminal report only after the run finishes (a real model
    // call can take most of the exchange window), so verifying against the
    // frozen dispatch-time `now` rejects legitimate terminals with
    // "invalid envelope time" (linux221 run 50252bf3). The injected clock
    // cannot observe real elapsed time, so use the exchange-window upper
    // bound `now + timeout_secs` as the arrival point: any envelope legally
    // signed inside this exchange satisfies sent_at <= arrival + 45, and
    // the expiry check at the same point stays stricter than at dispatch.
    let arrival=now.saturating_add(config.timeout_secs);
    let mut result=GatewayResult::unknown(&request.identity.run_id,"worker terminal missing; slot retained");
    let reason=match output {
        Err(error)=>format!("transport failure: {error:#}"),
        Ok(output)=>{
            let consumed=consume_events(&output.stdout,request,store,arrival,&mut result);
            if let Some(mut admitted)=admitted_result(store,request)? {
                admitted.launch=result.launch;admitted.terminal=result.terminal;
                admitted.reason=consumed.err().map(|error|format!("rejected post-terminal event: {error:#}"));return Ok(admitted);
            }
            match consumed {Err(error)=>format!("worker admission rejected: {error:#}"),Ok(())=>output.failure.unwrap_or_else(||format!("terminal incomplete; SSH exit {:?}",output.exit_code))}
        }
    };
    // Surface the worker's own signed status reason (if any) ahead of the
    // generic transport/admission phrasing so the real cause reaches the UI.
    let reason=match store.run(&request.identity.run_id)?.detail.get("reason").and_then(|v|v.as_str()).filter(|v|!v.is_empty()) {
        Some(worker_reason) if !reason.contains(worker_reason)=>format!("{worker_reason}; {reason}"),
        _=>reason,
    };
    store.mark_gateway_unknown(&request.identity,&reason,now)?;
    result.reason=Some(reason);Ok(result)
}
