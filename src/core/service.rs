//! Typed, bounded, nonblocking GUI bridge; domain truth stays in the worker's core.
use std::{fmt,path::PathBuf,sync::Arc};
use crate::config::{Mode,Actor};
use super::store::Run;
pub use super::projects::{ProjectSnapshot,DocumentSnapshot,TaskSnapshot,ProjectActionSnapshot};

/// User-facing real-mode provisioning input. Secret fields are kept as bytes so
/// the service can clear them when the command is dropped.
#[derive(Clone)]
pub struct ProvisionRequest {
    pub host: String,
    pub root_user: String,
    pub root_password: Vec<u8>,
    pub model_base_url: String,
    pub model_token: Vec<u8>,
    pub model: String,
}
impl fmt::Debug for ProvisionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ProvisionRequest")
            .field("host", &self.host)
            .field("root_user", &self.root_user)
            .field("root_password_len", &self.root_password.len())
            .field("model_base_url", &self.model_base_url)
            .field("model_token_len", &self.model_token.len())
            .field("model", &self.model)
            .finish()
    }
}
impl Drop for ProvisionRequest {
    fn drop(&mut self) {
        self.root_password.fill(0);
        self.model_token.fill(0);
    }
}

pub fn validate_provision_input(request: &ProvisionRequest) -> anyhow::Result<()> {
    use std::net::IpAddr;
    let host = request.host.trim();
    anyhow::ensure!(!host.is_empty() && host.len() <= 255, "host is required and must be at most 255 bytes");
    let address: IpAddr = host.parse().map_err(|_| anyhow::anyhow!("host must be a valid IP address"))?;
    anyhow::ensure!(address.is_ipv4(), "host must be an IPv4 address");
    let root_user = request.root_user.trim();
    anyhow::ensure!(!root_user.is_empty() && root_user.len() <= 128, "root_user is required");
    anyhow::ensure!(root_user.chars().all(|c| !c.is_control() && !c.is_whitespace()), "root_user contains invalid whitespace or control characters");
    anyhow::ensure!(!request.root_password.is_empty() && request.root_password.len() <= 16 * 1024, "root_password is required and must be at most 16 KiB");
    let endpoint = request.model_base_url.trim();
    anyhow::ensure!(endpoint.starts_with("https://"), "model_base_url must use HTTPS");
    let authority = &endpoint["https://".len()..];
    anyhow::ensure!(!authority.is_empty() && !authority.contains(['/', '?', '#', '@']), "model_base_url must be a bare HTTPS origin without path, query, fragment, or userinfo");
    anyhow::ensure!(authority.chars().all(|c| !c.is_control() && !c.is_whitespace()), "model_base_url contains invalid whitespace or control characters");
    let host_part = authority.split(':').next().unwrap_or_default();
    anyhow::ensure!(!host_part.is_empty(), "model_base_url host is required");
    anyhow::ensure!(!request.model_token.is_empty() && request.model_token.len() <= 16 * 1024, "model_token is required and must be at most 16 KiB");
    anyhow::ensure!(!request.model.trim().is_empty() && request.model.len() <= 128, "model is required and must be at most 128 bytes");
    Ok(())
}

/// Windows resource pressure is a test-environment notice, not a remote
/// Worker readiness failure. Linux readiness/cgroup/OOM evidence owns that gate.
pub fn classify_windows_resource_notice(_reason: &str) -> &'static str {
    "TEST_ENV_RESOURCE_LIMIT"
}

pub struct ServiceOptions {pub mode:Mode,pub state_dir:PathBuf,pub worker_exe:PathBuf,pub gateway_key:Option<Vec<u8>>,pub gateway_model_token:Option<Vec<u8>>}
impl fmt::Debug for ServiceOptions {
 fn fmt(&self, formatter:&mut fmt::Formatter<'_>)->fmt::Result {
  formatter.debug_struct("ServiceOptions")
   .field("mode",&self.mode)
   .field("state_dir",&self.state_dir)
   .field("worker_exe",&self.worker_exe)
   .field("gateway_key",&self.gateway_key.as_ref().map(|_| "[REDACTED]"))
   .field("gateway_model_token",&self.gateway_model_token.as_ref().map(|_| "[REDACTED]"))
   .finish()
 }
}
impl Drop for ServiceOptions { fn drop(&mut self) { if let Some(key)=self.gateway_key.as_mut() { key.fill(0); } if let Some(token)=self.gateway_model_token.as_mut() { token.fill(0); } } }
#[derive(Clone, Debug)]
pub enum UiCommand {
 GatewayConfigure{config_path:PathBuf},GatewayDispatch{worker_id:String},GatewayDispatchProject{worker_id:String,doc_path:PathBuf},ProjectWorkSubmit{project_id:String},
 ProvisionWorker{request:ProvisionRequest},
 StartLocal,Dispatch{template:String},Status{run_id:String},Approvals{run_id:String},
 Decide{run_id:String,attempt:u32,session:String,request_id:String,generation:u64,scope_hash:String,decision_id:String},
 Report{run_id:String},Retry{run_id:String,attempt:u32},Cancel{run_id:String},
 ProjectCreate{name:String},ProjectImport{project_id:String,path:PathBuf},ProjectConfirm{project_id:String,version:u32,sha256:String},ProjectSplit{project_id:String},TaskReview{project_id:String,task_id:String,approve:bool,reason:String},TaskRedo{project_id:String,task_id:String},ProjectStart{project_id:String},ProjectTest{project_id:String},ProjectRepair{project_id:String,reason:String},ProjectAccept{project_id:String},ProjectRefresh{project_id:String},ProjectConfigure{config_path:PathBuf},
}
#[derive(Clone, Debug, PartialEq, Eq)] pub enum SubmitError{Busy,Stopped}
#[derive(Clone, Debug, serde::Serialize)] pub struct ServiceError{pub code:String,pub message:String,pub next_step:String}
#[derive(Clone, Debug)] pub struct CommandResult{pub request_id:u64,pub result:Result<serde_json::Value,ServiceError>}
/// Process-local display metadata; never serialized or admitted to Store.
#[derive(Clone, Debug)]
pub struct LiveOutputEvent {
 pub request_id:u64, pub project_id:String, pub output:super::live_output::LiveOutput,
 pub received_at:std::time::Instant, pub received_bytes:usize, pub truncated:bool,
}
/// One current request, at most one pending event per kind. Draining does not reset budgets.
#[derive(Default)]
struct LiveMailbox {events:Vec<LiveOutputEvent>,text_bytes:usize,received_bytes:usize,truncated:bool}
impl LiveMailbox {
 fn push(&mut self,mut event:LiveOutputEvent) {
  use super::live_output::{LiveOutput,MAX_TEXT_BYTES};
  if let LiveOutput::Delta(text)=&mut event.output {
   if text.is_empty(){return;}
   self.received_bytes=self.received_bytes.saturating_add(text.len());
   let mut keep=if self.truncated {0} else {text.len().min(MAX_TEXT_BYTES-self.text_bytes)};
   while !text.is_char_boundary(keep){keep-=1;}
   self.truncated|=keep<text.len();
   text.truncate(keep);
   self.text_bytes+=keep;
  }
  event.received_bytes=self.received_bytes;
  event.truncated=self.truncated;
  if let Some(previous)=self.events.iter_mut().find(|previous|previous.request_id==event.request_id&&previous.project_id==event.project_id&&std::mem::discriminant(&previous.output)==std::mem::discriminant(&event.output)) {
   if let (LiveOutput::Delta(old),LiveOutput::Delta(text))=(&mut previous.output,&event.output){old.push_str(text);}
   else {previous.output=event.output;}
   previous.received_at=event.received_at;
   previous.received_bytes=event.received_bytes;
   previous.truncated=event.truncated;
  } else {self.events.push(event);}
 }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)] #[serde(rename_all = "snake_case")] pub enum Readiness{NotStarted,Starting,Ready,NotReady,Stopped}
#[derive(Clone, Debug, serde::Serialize)] pub struct DependencyView{pub id:String,pub state:String,pub detail:String,pub source:String,pub observed_at:u64}
#[derive(Clone, Debug, serde::Serialize)] pub struct RunView{pub run:Run,pub read_only:bool,pub can_status:bool,pub can_approvals:bool,pub can_decide:bool,pub can_report:bool,pub can_retry:bool,pub can_cancel:bool,pub disabled_reason:Option<String>}
#[derive(Clone, Debug, serde::Serialize)] pub struct NodeView{pub id:String,pub address:String,pub used:u32,pub slots:u32,pub admitted:bool,pub healthy:bool,pub draining:bool,pub source:String,pub observed_at:u64}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)] pub enum DecisionKind{AllowOnce,RejectOnce}
#[derive(Clone, Debug, serde::Serialize)] pub struct OfferedDecision{pub id:String,pub kind:DecisionKind,pub label:String}
#[derive(Clone, Debug, serde::Serialize)] pub struct ApprovalView{pub run_id:String,pub attempt:u32,pub session:String,pub request_id:String,pub generation:u64,pub scope_hash:String,pub expires_at:u64,pub offered:Vec<OfferedDecision>,pub summary:String,pub simulated:bool,pub can_decide:bool}
#[derive(Clone, Debug, serde::Serialize)] pub struct GatewayView { pub worker_id:String,pub host:String,pub remote_verified:bool,pub preflight_passed:bool,pub credentials_available:bool,pub used_slots:u32,pub artifact_sha256:String }
#[derive(Clone, Debug, serde::Serialize)] pub struct AppSnapshot{pub mode:Mode,pub role:String,pub readiness:Readiness,pub actor:Option<Actor>,pub observed_at:u64,pub running:bool,pub dependencies:Vec<DependencyView>,pub runs:Vec<RunView>,pub nodes:Vec<NodeView>,pub approvals:Vec<ApprovalView>,pub errors:Vec<ServiceError>,pub unfinished:usize,pub projects:Vec<ProjectSnapshot>,pub gateway:Option<GatewayView>,pub provision_ledger:Option<ProvisionLedger>}
use std::{sync::{Condvar,Mutex,atomic::{AtomicBool,AtomicUsize,Ordering},mpsc::{self,SyncSender,Receiver,TrySendError}},time::{Duration,SystemTime,UNIX_EPOCH},io::{Read,Write},path::Path};
use anyhow::{Result,Context,ensure};
use serde_json::{Value,json};
use crate::{config::{Config,NodeSpec,ServiceLock,valid_id},adapters::local::{LocalAo,pump_local}};
use super::{controller::ControlService,provision_ledger::{LedgerState, ProvisionLedger, ProvisionLedgerFacts},runner::Runner,session_credentials::SessionCredentials,store::Store};
#[cfg(test)]
mod activity_mailbox_tests {
 use super::*;
 use super::super::live_output::{LiveOutput, MAX_TEXT_BYTES};
 fn event(output:LiveOutput)->LiveOutputEvent { LiveOutputEvent{request_id:1,project_id:"p".into(),output,received_at:std::time::Instant::now(),received_bytes:0,truncated:false} }
 #[test]
 fn activity_mailbox_preserves_utf8_prefix_when_limit_crosses_delta() {
  let mut mailbox=LiveMailbox::default();
  mailbox.push(event(LiveOutput::Delta("a".repeat(MAX_TEXT_BYTES-2))));
  mailbox.push(event(LiveOutput::Delta("中tail".into())));
  mailbox.push(event(LiveOutput::Delta("z".into())));
  let text:String=mailbox.events.iter().filter_map(|e|match &e.output {LiveOutput::Delta(s)=>Some(s.as_str()),_=>None}).collect();
  assert_eq!(text.len(),MAX_TEXT_BYTES-2,"overflow must retain a prefix, not append a later suffix");
  assert!(!text.ends_with('z'));
 }
 #[test]
 fn activity_mailbox_exact_limit_and_drains_preserve_cumulative_metadata() {
  let mut mailbox=LiveMailbox::default();
  mailbox.push(event(LiveOutput::Delta("x".repeat(MAX_TEXT_BYTES))));
  let first=std::mem::take(&mut mailbox.events);
  assert_eq!(first[0].received_bytes,1_048_576);
  assert!(!first[0].truncated);
  mailbox.push(event(LiveOutput::Delta("中".into())));
  assert_eq!(mailbox.events[0].output,LiveOutput::Delta(String::new()));
  assert_eq!(mailbox.events[0].received_bytes,1_048_579);
  assert!(mailbox.events[0].truncated);
  mailbox.push(event(LiveOutput::ModelFinished));
  assert!(mailbox.events.iter().all(|e|e.received_bytes==1_048_579&&e.truncated));
 }
 #[test]
 fn activity_mailbox_partial_unicode_fits_then_preserves_controls_and_latest_receive_time() {
  let mut mailbox=LiveMailbox::default();
  let t=std::time::Instant::now();
  mailbox.push(event(LiveOutput::Delta("x".repeat(MAX_TEXT_BYTES-4))));
  let mut unicode=event(LiveOutput::Delta("中ab".into())); unicode.received_at=t+Duration::from_secs(2); mailbox.push(unicode);
  for _ in 0..100 {
   mailbox.push(event(LiveOutput::Connected));
   mailbox.push(event(LiveOutput::ModelFinished));
   mailbox.push(event(LiveOutput::Error("older error".into())));
   let mut tail=event(LiveOutput::Delta("discarded".into())); tail.received_at=t+Duration::from_secs(3); mailbox.push(tail);
  }
  mailbox.push(event(LiveOutput::Error("latest error".into())));
  let text=mailbox.events.iter().find(|e|matches!(e.output,LiveOutput::Delta(_))).unwrap();
  let LiveOutput::Delta(body)=&text.output else {unreachable!()};
  assert_eq!(body.len(),1_048_576);
  assert!(body.ends_with("中a")); assert!(text.truncated);
  assert_eq!(text.received_at,t+Duration::from_secs(3));
  assert!(mailbox.events.iter().any(|e|e.output==LiveOutput::Error("latest error".into())));
  assert_eq!(mailbox.events.len(),4);
 }
 #[test]
 fn activity_mailbox_control_flood_cannot_hide_terminal_or_error() {
  let mut mailbox=LiveMailbox::default();
  for _ in 0..100 {mailbox.push(event(LiveOutput::Connected));}
  mailbox.push(event(LiveOutput::ModelFinished));
  mailbox.push(event(LiveOutput::Error("disconnected".into())));
  assert!(mailbox.events.iter().any(|e|e.output==LiveOutput::ModelFinished));
  assert!(mailbox.events.iter().any(|e|matches!(e.output,LiveOutput::Error(_))));
  assert!(mailbox.events.len()<=4,"one coalesced slot per live kind");
 }
}
const BOUND:usize=32;
struct Accepted {id:u64,command:UiCommand}
struct Gate {next:u64,closed:bool}
struct Cleanup { in_flight:usize, closing:bool }
struct Shared {live:Mutex<LiveMailbox>,gate:Mutex<Gate>,credits:AtomicUsize,stop:Arc<AtomicBool>,snapshot:Mutex<Arc<AppSnapshot>>,credentials:Mutex<SessionCredentials>,cleanup:(Mutex<Cleanup>,Condvar),before_work_registration:Arc<dyn Fn()+Send+Sync>,before_publication:Arc<dyn Fn()+Send+Sync>}
struct InFlightGuard<'a> { shared:&'a Shared }
struct PublicationGuard<'a> { shared:&'a Shared, _cleanup:std::sync::MutexGuard<'a, Cleanup> }
impl<'a> InFlightGuard<'a> {
 fn try_new(shared:&'a Shared)->Result<Self> {
  let mut cleanup=shared.cleanup.0.lock().map_err(|_|anyhow::anyhow!("service lifecycle unavailable"))?;
  ensure!(!cleanup.closing&&!shared.stop.load(Ordering::Acquire),"service is closing");
  cleanup.in_flight+=1;
  Ok(Self{shared})
 }
}
impl<'a> PublicationGuard<'a> {
 fn try_new(shared:&'a Shared)->Result<Self> {
  let cleanup=shared.cleanup.0.lock().map_err(|_|anyhow::anyhow!("service lifecycle unavailable"))?;
  ensure!(!cleanup.closing&&!shared.stop.load(Ordering::Acquire),"service is closing");
  Ok(Self{shared,_cleanup:cleanup})
 }
}
impl Drop for InFlightGuard<'_> { fn drop(&mut self) { if let Ok(mut cleanup)=self.shared.cleanup.0.lock(){cleanup.in_flight=cleanup.in_flight.saturating_sub(1);self.shared.cleanup.1.notify_all();} } }
impl Drop for PublicationGuard<'_> { fn drop(&mut self) { self.shared.cleanup.1.notify_all(); } }
pub trait GatewayTransportFactory: Send+Sync {
 fn sum_transport(&self,config:super::ssh_gateway::SshGatewayConfig,lease:super::session_credentials::CredentialLease,cancel:Arc<AtomicBool>)->Result<Box<dyn super::ssh_gateway::WorkerTransport>>;
 fn project_transport(&self,config:super::ssh_gateway::SshGatewayConfig,lease:super::session_credentials::CredentialLease,cancel:Arc<AtomicBool>)->Result<Box<dyn super::ssh_gateway::WorkerTransport>>;
}
struct ParamikoTransportFactory;
impl GatewayTransportFactory for ParamikoTransportFactory {
 fn sum_transport(&self,config:super::ssh_gateway::SshGatewayConfig,lease:super::session_credentials::CredentialLease,cancel:Arc<AtomicBool>)->Result<Box<dyn super::ssh_gateway::WorkerTransport>> { let helper=std::env::current_exe()?.parent().map(|path|path.join("tools/paramiko_gateway.py")).context("gateway helper path unavailable")?;Ok(Box::new(super::ssh_gateway::SshWorkerTransport::with_credential_lease_cancellable(config,helper,lease,false,cancel)?)) }
 fn project_transport(&self,config:super::ssh_gateway::SshGatewayConfig,lease:super::session_credentials::CredentialLease,cancel:Arc<AtomicBool>)->Result<Box<dyn super::ssh_gateway::WorkerTransport>> { let helper=std::env::current_exe()?.parent().map(|path|path.join("tools/paramiko_gateway.py")).context("gateway helper path unavailable")?;Ok(Box::new(super::ssh_gateway::SshWorkerTransport::with_credential_lease_cancellable(config,helper,lease,true,cancel)?)) }
}
pub struct ServiceHandle {commands:SyncSender<Accepted>,results:Receiver<CommandResult>,shared:Arc<Shared>,cached:Arc<AppSnapshot>}
fn clock()->u64 {SystemTime::now().duration_since(UNIX_EPOCH).map_or(0,|d|d.as_secs())}
fn error(code:&str,message:&str,next:&str)->ServiceError {ServiceError{code:code.into(),message:message.into(),next_step:next.into()}}
fn stopped()->ServiceError {error("stopped","后台服务已停止；此请求未执行","重新打开服务；关闭窗口不代表取消任务")}
fn initial(mode:Mode)->AppSnapshot {
 let real=mode==Mode::Real;
 AppSnapshot{mode,role:"controller + local fixture runner".into(),readiness:if real{Readiness::NotReady}else{Readiness::NotStarted},actor:None,observed_at:clock(),running:true,dependencies:vec![DependencyView{id:if real{"real-ports"}else{"local-fixture"}.into(),state:if real{"not_ready"}else{"not_started"}.into(),detail:if real{"真实 AO/Reason/模型尚未接通；不自动回退本地"}else{"本地体验未启动；无真实 AO/Reason/模型"}.into(),source:"explicit configured mode; no external probe".into(),observed_at:0}],runs:vec![],nodes:vec![],approvals:vec![],errors:vec![],unfinished:0,projects:vec![],gateway:None,provision_ledger:None}
}
impl ServiceHandle {
 pub fn spawn(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>)->Result<Self> { Self::spawn_with_provisioner(options,repaint,Box::new(super::provision::BundledProvisioner)) }
 /// Inject only the provisioning process boundary; routing and durable state stay real.
 pub fn spawn_with_provisioner(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>,provisioner:Box<dyn super::provision::Provisioner>)->Result<Self> { Self::spawn_inner(options,repaint,provisioner,Arc::new(clock),Arc::new(ParamikoTransportFactory),Arc::new(||{}),Arc::new(||{})) }
 pub fn spawn_with_clock(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>,clock:Arc<dyn Fn()->u64+Send+Sync>)->Result<Self> { Self::spawn_inner(options,repaint,Box::new(super::provision::BundledProvisioner),clock,Arc::new(ParamikoTransportFactory),Arc::new(||{}),Arc::new(||{})) }
 #[doc(hidden)]
 pub fn spawn_with_clock_and_provisioner(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>,clock:Arc<dyn Fn()->u64+Send+Sync>,provisioner:Box<dyn super::provision::Provisioner>)->Result<Self> { Self::spawn_inner(options,repaint,provisioner,clock,Arc::new(ParamikoTransportFactory),Arc::new(||{}),Arc::new(||{})) }
 pub fn spawn_with_clock_and_transport_factory(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>,clock:Arc<dyn Fn()->u64+Send+Sync>,factory:Arc<dyn GatewayTransportFactory>)->Result<Self> { Self::spawn_inner(options,repaint,Box::new(super::provision::BundledProvisioner),clock,factory,Arc::new(||{}),Arc::new(||{})) }
 #[doc(hidden)]
 pub fn spawn_with_clock_provisioner_and_factory(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>,clock:Arc<dyn Fn()->u64+Send+Sync>,provisioner:Box<dyn super::provision::Provisioner>,factory:Arc<dyn GatewayTransportFactory>)->Result<Self> { Self::spawn_inner(options,repaint,provisioner,clock,factory,Arc::new(||{}),Arc::new(||{})) }
 #[doc(hidden)]
 pub fn spawn_with_clock_provisioner_factory_hook(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>,clock:Arc<dyn Fn()->u64+Send+Sync>,provisioner:Box<dyn super::provision::Provisioner>,factory:Arc<dyn GatewayTransportFactory>,before_work_registration:Arc<dyn Fn()+Send+Sync>)->Result<Self> { Self::spawn_inner(options,repaint,provisioner,clock,factory,before_work_registration,Arc::new(||{})) }
 #[doc(hidden)]
 pub fn spawn_with_test_work_registration_hook(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>,provisioner:Box<dyn super::provision::Provisioner>,factory:Arc<dyn GatewayTransportFactory>,before_work_registration:Arc<dyn Fn()+Send+Sync>)->Result<Self> { Self::spawn_inner(options,repaint,provisioner,Arc::new(clock),factory,before_work_registration,Arc::new(||{})) }
 #[doc(hidden)]
 pub fn spawn_with_test_publication_hook(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>,provisioner:Box<dyn super::provision::Provisioner>,factory:Arc<dyn GatewayTransportFactory>,before_publication:Arc<dyn Fn()+Send+Sync>)->Result<Self> { Self::spawn_inner(options,repaint,provisioner,Arc::new(clock),factory,Arc::new(||{}),before_publication) }
 fn spawn_inner(options:ServiceOptions,repaint:Arc<dyn Fn()+Send+Sync>,provisioner:Box<dyn super::provision::Provisioner>,clock:Arc<dyn Fn()->u64+Send+Sync>,factory:Arc<dyn GatewayTransportFactory>,before_work_registration:Arc<dyn Fn()+Send+Sync>,before_publication:Arc<dyn Fn()+Send+Sync>)->Result<Self> {
  let cached=Arc::new(initial(options.mode.clone()));
  let shared=Arc::new(Shared{live:Mutex::new(LiveMailbox::default()),gate:Mutex::new(Gate{next:1,closed:false}),credits:AtomicUsize::new(0),stop:Arc::new(AtomicBool::new(false)),snapshot:Mutex::new(cached.clone()),credentials:Mutex::new(SessionCredentials::default()),cleanup:(Mutex::new(Cleanup{in_flight:0,closing:false}),Condvar::new()),before_work_registration,before_publication});
  let(commands,rx)=mpsc::sync_channel(BOUND);let(tx,results)=mpsc::sync_channel(BOUND);
  let state=shared.clone();std::thread::Builder::new().name("native-core-service".into()).spawn(move||worker(options,state,rx,tx,repaint,provisioner,clock,factory))?;
  Ok(Self{commands,results,shared,cached})
 }
 pub fn try_submit(&self,command:UiCommand)->std::result::Result<u64,SubmitError>{
  if self.shared.stop.load(Ordering::Acquire){return Err(SubmitError::Stopped)}
  let mut gate=self.shared.gate.try_lock().map_err(|_|SubmitError::Busy)?;
  if gate.closed||self.shared.stop.load(Ordering::Acquire){return Err(SubmitError::Stopped)}
  if self.shared.credits.try_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<BOUND).then_some(n+1)).is_err(){return Err(SubmitError::Busy)}
  let id=gate.next;
  let Some(next)=id.checked_add(1) else{self.shared.credits.fetch_sub(1,Ordering::AcqRel);return Err(SubmitError::Stopped)};
  match self.commands.try_send(Accepted{id,command}) {
   Ok(())=>{gate.next=next;Ok(id)},
   Err(e)=>{self.shared.credits.fetch_sub(1,Ordering::AcqRel);Err(match e{TrySendError::Full(_)=>SubmitError::Busy,TrySendError::Disconnected(_)=>SubmitError::Stopped})}
  }
 }
 pub fn drain_live_output(&mut self)->Vec<LiveOutputEvent>{self.shared.live.lock().map(|mut live|std::mem::take(&mut live.events)).unwrap_or_default()}
 pub fn drain_results(&mut self)->Vec<CommandResult>{let results:Vec<_>=self.results.try_iter().collect();self.shared.credits.fetch_sub(results.len(),Ordering::AcqRel);results}
 /// Receipt publication follows all its live emissions. Collect receipts before
 /// live so a single UI poll cannot finalize while preceding text is still queued.
 pub fn drain_updates(&mut self)->(Vec<LiveOutputEvent>,Vec<CommandResult>){let results=self.drain_results();let live=self.drain_live_output();(live,results)}
 pub fn snapshot(&mut self)->Arc<AppSnapshot>{if let Ok(snapshot)=self.shared.snapshot.try_lock(){self.cached=snapshot.clone();}self.cached.clone()}
 pub fn disconnect(&self){
  self.stop();
  if let Ok(mut credentials)=self.shared.credentials.lock(){credentials.clear();}
  let (lock,ready)=&self.shared.cleanup;
  if let Ok(cleanup)=lock.lock(){let _=ready.wait_timeout_while(cleanup,Duration::from_secs(5),|state|state.in_flight>0);}
 }
 pub fn stop(&self){
  self.shared.stop.store(true,Ordering::Release);
  if let Ok(mut cleanup)=self.shared.cleanup.0.lock(){cleanup.closing=true;}
 }
}
impl Drop for ServiceHandle {fn drop(&mut self){self.disconnect();}}
struct LocalRuntime {control:ControlService,runner:Runner,ao:LocalAo,_locks:Vec<ServiceLock>,observed_at:u64}
fn bounded_file(path:&Path,max:usize)->Result<Vec<u8>> {let mut raw=Vec::new();std::fs::File::open(path)?.take((max+1) as u64).read_to_end(&mut raw)?;ensure!(raw.len()<=max,"local identity size bound");Ok(raw)}
fn create_file(path:&Path,bytes:&[u8])->Result<()> {let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;file.write_all(bytes)?;file.sync_all()?;Ok(())}
impl LocalRuntime {
 fn open(options:&ServiceOptions)->Result<Self>{
  ensure!(options.mode==Mode::Local,"explicit local mode required");
  ensure!(options.worker_exe.is_file(),"bundled Rust fixture missing");
  let config=Config{schema:"ao-reason-config/1".into(),mode:Mode::Local,role:"controller".into(),deployment:"native-local".into(),state_dir:options.state_dir.clone(),nodes:vec![],ao:None,reason_endpoint:None,endpoint:None,runner_node:None,controller_address:None,workspace_root:None};
  let state=config.prepare_state(Path::new("."))?;
  let controller=ServiceLock::acquire(&state,"controller")?;let runner=ServiceLock::acquire(&state,"runner")?;
  let _identity=ServiceLock::acquire(&state,"identity")?;
  let host_path=state.join("local-host.json");
  let actor=if host_path.exists(){
   let host:Value=serde_json::from_slice(&bounded_file(&host_path,1024)?)?;
   ensure!(host["schema"]=="native-local-host/1"&&host.as_object().is_some_and(|o|o.len()==2),"invalid local host identity");
   let id=host["actor"].as_str().context("missing local actor")?;ensure!(valid_id(id),"invalid local actor");Actor::Local(id.into())
  }else{let id=uuid::Uuid::new_v4().to_string();create_file(&host_path,&serde_json::to_vec(&json!({"schema":"native-local-host/1","actor":id}))?)?;Actor::Local(id)};
  let key_path=state.join("local-node.key");
  if !key_path.exists(){let key=format!("{}{}",uuid::Uuid::new_v4().simple(),uuid::Uuid::new_v4().simple());create_file(&key_path,key.as_bytes())?;}
  let key_hex=String::from_utf8(bounded_file(&key_path,64)?)?;
  let node=NodeSpec{id:"local-B".into(),address:"local://fixture-B".into(),key_hex,slots:1,admitted:true,healthy:true,draining:false,templates:vec!["sum-native-v1".into(),"control-native-v1".into()]};node.validate()?;
  let store=Store::open(&state.join("controller.sqlite"),"native-local",std::slice::from_ref(&node))?;
  let runner_db=Runner::open(&state.join("runner.sqlite"),"native-local",node)?;
  let ao=LocalAo::new(&options.worker_exe,&state.join("fixtures"))?;
  Ok(Self{control:ControlService::new(store,actor),runner:runner_db,ao,_locks:vec![controller,runner],observed_at:clock()})
 }
 fn command(&mut self,command:UiCommand,now:u64)->Result<Value>{
  let(tool,args)=match command {
   UiCommand::StartLocal=>return Ok(json!({"state":"ready","simulated":true})),
   UiCommand::Dispatch{template}=>{ensure!(matches!(template.as_str(),"sum-native-v1"|"control-native-v1"),"real template unavailable in local mode");("dispatch",json!({"template":template}))},
   UiCommand::Status{run_id}=>("status",json!({"run_id":run_id})),
   UiCommand::Approvals{run_id}=>("approvals",json!({"run_id":run_id})),
   UiCommand::Report{run_id}=>("report",json!({"run_id":run_id})),
   UiCommand::Retry{run_id,attempt}=>("retry",json!({"run_id":run_id,"attempt":attempt})),
   UiCommand::Cancel{run_id}=>("cancel",json!({"run_id":run_id})),
   UiCommand::Decide{run_id,attempt,session,request_id,generation,scope_hash,decision_id}=>{
    let actor=self.control.actor().clone();
    let decision=super::controller::Decision{run_id,request_id,generation,scope_hash,decision_id};
    return super::controller::decide_with_binding(&mut self.control.store,&actor,&decision,Some((attempt,&session)),now)
   }
   _=>anyhow::bail!("project command belongs to project workflow"),
  };self.control.call(tool,args,now)
 }
 fn project(&self,snapshot:&mut AppSnapshot,now:u64)->Result<()> {
  let store=&self.control.store;let actor=self.control.actor();let mut runs=Vec::new();let mut approvals=Vec::new();
  for run in store.list_runs(200)? {
   let owned=run.owner==*actor;let terminal=matches!(run.status.as_str(),"done"|"failed"|"cancelled");
   let assignment=store.assignment(&run.id,run.attempt)?;
   let can_retry=owned&&matches!(run.status.as_str(),"failed"|"cancelled")&&run.detail["retry_safe"]==true&&assignment.as_ref().is_none_or(|a|a.released);
   let pending=if owned {store.approvals(actor,&run.id,now)?} else {Vec::new()};let mut can_decide=false;
   for p in pending.into_iter().take(400_usize.saturating_sub(approvals.len())) {
    let offered=p["offered"].as_array().context("approval decisions missing")?.iter().map(|v|{let id=v.as_str().context("invalid offered decision")?;let(kind,label)=match id{"allow_once"=>(DecisionKind::AllowOnce,"仅允许本次"),"reject_once"=>(DecisionKind::RejectOnce,"仅拒绝本次"),_=>anyhow::bail!("unsupported decision")};Ok(OfferedDecision{id:id.into(),kind,label:label.into()})}).collect::<Result<Vec<_>>>()?;
    can_decide=true;
    approvals.push(ApprovalView{run_id:run.id.clone(),attempt:run.attempt,session:run.session.clone().context("approval session missing")?,request_id:p["request_id"].as_str().context("request missing")?.into(),generation:p["generation"].as_u64().context("generation missing")?,scope_hash:p["scope_hash"].as_str().context("scope missing")?.into(),expires_at:p["expires_at"].as_u64().context("expiry missing")?,offered,summary:p["summary"].as_str().unwrap_or("模拟审批").chars().take(1024).collect(),simulated:true,can_decide:owned&&now<run.deadline});
   }
   let reason=if !owned{Some("任务属于其他可信入口，只读".into())}else if terminal{Some("已终止；重试仍需精确退出证据".into())}else if run.status=="unknown"{Some("执行结果未知；保留名额，不能自动重跑".into())}else{None};
   runs.push(RunView{read_only:!owned,can_status:owned,can_approvals:owned&&can_decide,can_decide:owned&&can_decide&&now<run.deadline,can_report:owned&&(terminal||run.detail.get("report").is_some()||run.detail.get("failure").is_some()),can_retry,can_cancel:owned&&!terminal&&run.status!="cancel_pending",disabled_reason:reason,run});
  }
  let nodes=store.nodes.iter().map(|n|{let used:i64=store.db.query_row("SELECT count(*) FROM assignments WHERE node=?1 AND released=0",[&n.id],|r|r.get(0))?;Ok(NodeView{id:n.id.clone(),address:n.address.clone(),used:u32::try_from(used)?,slots:n.slots,admitted:n.admitted,healthy:n.healthy,draining:n.draining,source:"explicit local fixture configuration; not real readiness".into(),observed_at:self.observed_at})}).collect::<Result<Vec<_>>>()?;
  let unfinished:i64=store.db.query_row("SELECT count(*) FROM runs WHERE status NOT IN ('done','failed','cancelled') OR EXISTS(SELECT 1 FROM assignments a WHERE a.run=runs.id AND a.released=0)",[],|r|r.get(0))?;
  snapshot.actor=Some(actor.clone());snapshot.runs=runs;snapshot.approvals=approvals;snapshot.nodes=nodes;snapshot.unfinished=usize::try_from(unfinished)?;Ok(())
 }
}
fn push_error(snapshot:&mut AppSnapshot,e:ServiceError){if snapshot.errors.last().is_none_or(|old|old.code!=e.code||old.message!=e.message){snapshot.errors.push(e);if snapshot.errors.len()>20{snapshot.errors.remove(0);}}}
fn repaint_safely(repaint:&Arc<dyn Fn()+Send+Sync>){let _=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||repaint()));}
fn publish_snapshot(shared:&Shared,snapshot:&AppSnapshot){
 let mut value=snapshot.clone();
 value.observed_at=clock();
 if let Ok(mut slot)=shared.snapshot.lock(){*slot=Arc::new(value);}
}
fn publish(shared:&Shared,snapshot:&AppSnapshot,repaint:&Arc<dyn Fn()+Send+Sync>,last:&mut String){
 let mut compared=snapshot.clone();compared.observed_at=0;let fingerprint=format!("{compared:?}");
 if *last!=fingerprint{*last=fingerprint;publish_snapshot(shared,snapshot);repaint_safely(repaint);}
}
fn worker(mut options:ServiceOptions,shared:Arc<Shared>,rx:Receiver<Accepted>,tx:SyncSender<CommandResult>,repaint:Arc<dyn Fn()+Send+Sync>,provisioner:Box<dyn super::provision::Provisioner>,clock:Arc<dyn Fn()->u64+Send+Sync>,factory:Arc<dyn GatewayTransportFactory>){
 if let Ok(cleanup)=shared.cleanup.0.lock(){
  if !cleanup.closing&&!shared.stop.load(Ordering::Acquire) {
   if let Ok(mut credentials)=shared.credentials.lock(){
    if let Some(key)=options.gateway_key.take(){credentials.set_worker_key_at(super::provision::SecretBytes::from_vec(key),clock());}
    if let Some(mut token)=options.gateway_model_token.take(){credentials.set_model_token_at(&token,clock());token.fill(0);}
   }
  }
 }
 let mut snapshot=initial(options.mode.clone());
 let recovery = match ProvisionLedger::load(&options.state_dir) {
  Ok(Some(ledger)) => { snapshot.provision_ledger = Some(ledger); None }
  Ok(None) => None,
  Err(error) => Some(error),
 };
 if let Some(ledger_error) = recovery { push_error(&mut snapshot, error("ledger_recovery", &ledger_error.to_string(), "修复或移除无效的本地 provisioning 账本后重新认证；未恢复 Gateway")); }
 let mut runtime:Option<LocalRuntime>=None;let mut gateway:Option<super::ssh_manager::SshManager>=None;let mut preflight_passed=false;let mut projects:Option<super::projects::Projects>=None;let mut last=String::new();
 if options.state_dir.join("project-workflow/controller.sqlite").is_file(){match super::projects::Projects::open(&options.state_dir){Ok(p)=>{snapshot.projects=p.snapshots();snapshot.unfinished=p.unfinished();projects=Some(p)},Err(_)=>push_error(&mut snapshot,error("project_state_unavailable","已有项目账本读取失败；未创建新项目或覆盖旧状态","保留状态目录并检查访问权限及数据库版本"))}}
 loop {
  if shared.stop.load(Ordering::Acquire){
   // Serialize closure with try_submit, so no accepted command can arrive after this drain.
   if let Ok(mut gate)=shared.gate.lock(){gate.closed=true;}
   for request in rx.try_iter(){let _=tx.try_send(CommandResult{request_id:request.id,result:Err(stopped())});}
   if let Ok(mut credentials)=shared.credentials.lock(){credentials.clear();}drop(gateway.take());drop(runtime.take());snapshot.running=false;snapshot.readiness=Readiness::Stopped;
   for r in &mut snapshot.runs {r.can_status=false;r.can_approvals=false;r.can_decide=false;r.can_report=false;r.can_retry=false;r.can_cancel=false;r.disabled_reason=Some("服务已停止；关闭不等于任务已取消".into());}
   for a in &mut snapshot.approvals{a.can_decide=false;}
   publish(&shared,&snapshot,&repaint,&mut last);repaint_safely(&repaint);return
  }
  match rx.recv_timeout(Duration::from_millis(20)) {
   Ok(request)=>{
    // Hoisted for the pre-receipt publish gate below: only GatewayConfigure's
    // effects are fully projected inside the request branch.
    let is_configure=matches!(request.command,UiCommand::GatewayConfigure{..}|UiCommand::ProvisionWorker{..});
    let result=if shared.stop.load(Ordering::Acquire){Err(stopped())}else if let UiCommand::ProvisionWorker{request}=&request.command {
     // Provisioning is not dispatch. Reject replacement before any remote side effect.
     if options.mode!=Mode::Real {Err(error("blocked","Worker 自动准备只适用于真实模式；未启动 Helper","切换真实模式后重新提交六项输入"))}
     else {
      if gateway.as_ref().is_some_and(|_| preflight_passed && !shared.credentials.lock().map(|credentials| credentials.is_available()).unwrap_or(false)) {
       gateway.take();
       preflight_passed = false;
       snapshot.gateway = None;
      }
      if gateway.is_some(){Err(error("provision_error","当前 Worker 已固定；未重新准备或替换现有连接","保留现有任务；更换 Worker 前关闭并重新打开 Manager"))}
      else if validate_provision_input(request).is_err(){
       if options.state_dir.is_dir() { let _ = ProvisionLedger::write_new(&options.state_dir, ProvisionLedgerFacts::failure("input_invalid", clock(), clock(), None)); }
       Err(error("provision_error","六项准备输入无效；未启动 Helper","核对 IP、root 用户、HTTPS 地址、模型与两项秘密后重试"))
      }
      else {
      match InFlightGuard::try_new(&shared) {
       Ok(_in_flight)=>match super::provision::ProvisioningContext::from_state_dir(&options.state_dir) {
       Ok(context)=>{
        if options.state_dir.is_dir() { let _ = ProvisionLedger::write_new(&options.state_dir, ProvisionLedgerFacts { state: LedgerState::Pending, output_dir: Some(context.output_dir().to_path_buf()), gateway_path: None, worker_id: None, host: None, artifact_sha256: None, artifact_bytes: None, created_at: clock(), updated_at: clock(), failure_code: None, failure_next_step: None, attempt_id: Some(context.attempt_id().to_owned()), remote_root: None, remote_side_effects: None }); }
        match provisioner.provision_with_context(request, &context) {
        Ok(result) if result.metadata["status"]=="PASS_WORKER_PROVISION"=>{
         let activated=(||->Result<Value>{
          let install_root=provisioner.prerequisites_root().map_err(|_|anyhow::anyhow!("installed prerequisites unavailable"))?;
          let path=super::provision_activation::validate_generated(&result,request,&context,&install_root)?;
          let manager=super::ssh_manager::SshManager::open(&path)?;
          let mut projected=snapshot.clone();project_gateway(&manager,&mut projected,true,true)?;
          let config=manager.config();
          let facts=ProvisionLedgerFacts::preflight_with_attempt(path.parent().context("gateway output parent missing")?.to_path_buf(),path.clone(),config.worker_id.clone(),config.host.clone(),config.artifact_sha256.clone(),config.artifact_bytes,clock(),Some(context.attempt_id().to_owned()));
          (shared.before_publication)();
          let publication=PublicationGuard::try_new(&shared)?;
          ProvisionLedger::write_new(&options.state_dir,facts).map_err(|error|anyhow::anyhow!("ledger write failed: {error}"))?;
          let ledger=ProvisionLedger::load(&options.state_dir).map_err(|error|anyhow::anyhow!("ledger reload failed: {error}"))?;
          append_diag(&options.state_dir,"gateway_configured",&format!("worker={} artifact={} source=provision",config.worker_id,&config.artifact_sha256[..8]))
           .map_err(|error| anyhow::anyhow!("activation diagnostic write failed: {error}"))?;
          let key=result.take_private_key();
          let mut credentials=shared.credentials.lock().map_err(|_|anyhow::anyhow!("session credentials unavailable"))?;
          credentials.clear();
          credentials.set_worker_key_at(key,clock());
          credentials.set_model_token_at(&request.model_token,clock());
          let receipt=json!({"state":"worker_preflight","remote_verified":false,"preflight":true,
           "worker_id":config.worker_id,"gateway_path":path});
          preflight_passed=true;gateway=Some(manager);snapshot=projected.clone();snapshot.provision_ledger=ledger;
          publish_snapshot(&shared,&projected);drop(publication);Ok(receipt)
         })();
         match activated {
          Ok(receipt) => Ok(receipt),
          Err(_) => {
           if options.state_dir.is_dir() {
            let _ = ProvisionLedger::write_new(
             &options.state_dir,
             ProvisionLedgerFacts::failure_with_attempt(
              "activation_blocked",
              clock(),
              clock(),
              Some(context.output_dir().to_path_buf()),
              Some(context.attempt_id().to_owned()),
             ),
            );
           }
           Err(error("provision_error","Worker 准备结果未通过本地可信校验或新账本激活；未替换连接或释放名额","保留 staging，核对包内资产、可信材料和生成配置后重试"))
          }
         }
        },
        Ok(result)=>{
         if options.state_dir.is_dir() {
          let code=result.metadata["reason_code"].as_str()
           .or_else(||result.metadata["error_type"].as_str())
           .unwrap_or("provision_error");
          let remote_root=result.metadata["remote_root"].as_str().map(str::to_owned);
          let remote_side_effects=result.metadata["remote_side_effects"].as_str().map(str::to_owned);
          let facts=ProvisionLedgerFacts::failure_with_remote(
           code,clock(),clock(),Some(context.output_dir().to_path_buf()),
           result.metadata["attempt_id"].as_str().map(str::to_owned),
           remote_root,remote_side_effects,
          );
          let _=ProvisionLedger::write_new(&options.state_dir,facts);
         }
         Err(super::provision::service_error_from_result(&result.metadata))
        },
        Err(failure)=>{ if options.state_dir.is_dir() { let _ = ProvisionLedger::write_new(&options.state_dir, ProvisionLedgerFacts::failure_with_attempt("provision_error", clock(), clock(), Some(context.output_dir().to_path_buf()), Some(context.attempt_id().to_owned()))); } Err(failure.service_error()) },
       }
       },
       Err(provisioning_error)=>{ if options.state_dir.is_dir() { let _ = ProvisionLedger::write_new(&options.state_dir, ProvisionLedgerFacts::failure("provision_error", clock(), clock(), None)); } Err(error("provision_error", &provisioning_error.to_string(), "检查受控 provisioning 目录、六项输入及前置条件后重试；未更改 Gateway 或释放任务名额")) },
      },
       Err(_)=>Err(stopped()),
      }
      }
     }
    }else if matches!(&request.command,UiCommand::GatewayConfigure{..}|UiCommand::GatewayDispatch{..}|UiCommand::GatewayDispatchProject{..}) {
     if let Ok(mut credentials)=shared.credentials.lock(){credentials.clear_if_expired(clock());}
     let configured=gateway_command(&mut options,&shared,&mut gateway,&mut snapshot,request.command,&*factory);
     // A configure success must be visible in the snapshot before its receipt
     // is delivered: the UI polls the snapshot right after draining the
     // result, and the loop-tail projection can lag by up to one iteration.
     if configured.is_ok()&&is_configure {
      if let Some(manager)=gateway.as_ref(){if let Err(e)=project_gateway(manager,&mut snapshot,preflight_passed,shared.credentials.lock().map(|credentials| credentials.is_available()).unwrap_or(false)){push_error(&mut snapshot,error("gateway_snapshot_failed",&e.to_string(),"保留账本并检查实际执行证据"));}}
     }
     configured.map_err(|e|error("ssh_gateway_rejected",&format!("{e:#}"),"核对显式Worker配置及保留名额；UNKNOWN不自动重跑，配置成功不表示远端验证"))
    }else if matches!(&request.command,UiCommand::ProjectWorkSubmit{..}) {
     if let Ok(mut credentials)=shared.credentials.lock(){credentials.clear_if_expired(clock());}
     let action=(||->Result<Value>{
      let project_id=match request.command { UiCommand::ProjectWorkSubmit{project_id}=>project_id,_=>unreachable!() };
      if projects.is_none(){projects=Some(super::projects::Projects::open(&options.state_dir)?);}
      let projects=projects.as_mut().context("project state unavailable")?;
      if let Ok(mut live)=shared.live.lock(){*live=LiveMailbox::default();}
      let mut emit=|output| {
       if let Ok(mut live)=shared.live.lock(){live.push(LiveOutputEvent{request_id:request.id,project_id:project_id.clone(),output,received_at:std::time::Instant::now(),received_bytes:0,truncated:false});}
       repaint_safely(&repaint);
      };
      let result=submit_confirmed_project_work(&mut options,&shared,&mut gateway,&mut snapshot,&mut preflight_passed,projects,&project_id,&*factory,&mut emit);
      match &result {
       Ok(value) if value["status"]!="done"=>emit(super::live_output::LiveOutput::Error("任务尚无可准入的终态；保留已有输出，不会自动重跑。".into())),
       Err(_)=>emit(super::live_output::LiveOutput::Error("项目提交或输出连接失败；保留已有输出，请查看操作回执。".into())),
       _=>{},
      }
      result
     })();
     action.map_err(|e|error("project_work_blocked",&e.to_string(),"先完成当前会话的 Worker 自动准备，并确认项目中的上传文档版本后再提交"))
    }else if super::projects::is_project_command(&request.command){
     let action=(||->Result<Value>{if projects.is_none(){projects=Some(super::projects::Projects::open(&options.state_dir)?);}projects.as_mut().context("project state unavailable")?.command(request.command)})();
     action.map_err(|e|error("project_action_rejected",&e.to_string(),"核对上传版本/审核门槛与项目阻塞原因；未执行的阶段不会标为成功"))
    }else if matches!(&request.command,UiCommand::Decide{run_id,..} if options.mode==Mode::Real||projects.as_ref().is_some_and(|p|p.has_run_link(run_id).unwrap_or(true))){
     // Resolve the project route before LocalRuntime. Real mode never falls
     // back to a fixture, even when the host or exact binding is unavailable.
     if let Some(p)=projects.as_mut().filter(|p|p.host_configured()) {
      if let UiCommand::Decide{run_id,attempt,session,request_id,generation,scope_hash,decision_id}=request.command {
       let decision=super::controller::Decision{run_id,request_id,generation,scope_hash,decision_id};
       p.decide(&decision,attempt,&session,clock()).map_err(|e|error("project_decision_rejected",&e.to_string(),"刷新项目审批并核对 owner、action、attempt、session 及当前授权范围；未排队的决定不会生效"))
      }else{unreachable!()}
     }else{Err(error("project_host_unavailable","项目执行主机不可用；未路由到本地替代","配置并打开对应项目主机后刷新审批；不要启动 fixture 代替真实项目"))}
    }else if matches!(request.command,UiCommand::StartLocal)&&runtime.is_none(){
     if options.mode==Mode::Real {Err(error("real_not_ready","真实 AO/Reason/模型尚未接通；不会启动本地替代","完成真实依赖配置和独立验收后再使用真实模式"))}
     else {
      snapshot.readiness=Readiness::Starting;publish(&shared,&snapshot,&repaint,&mut last);
      match LocalRuntime::open(&options){Ok(local)=>{snapshot.dependencies=vec![DependencyView{id:"native-local-fixture".into(),state:"ready".into(),detail:"本地 Rust 子进程 + 独立 verifier；无真实 AO/Reason/模型".into(),source:"worker opened native ledgers and bundled fixture; not a live probe".into(),observed_at:local.observed_at}];runtime=Some(local);snapshot.readiness=Readiness::Ready;Ok(json!({"state":"ready","simulated":true}))},Err(_)=>{snapshot.readiness=Readiness::NotReady;Err(error("state_unavailable","本地原生状态或固定执行器不可用；未报告启动成功","检查目录权限、模式、同目录单实例锁及同版 aw-cli；不要删除账本绕过错误"))}}
     }
    }else if let Some(local)=runtime.as_mut(){local.command(request.command,clock()).map_err(|_|error("action_rejected","核心拒绝此操作；未报告成功","刷新实际状态并核对所属任务、attempt、session、审批范围及退出证据"))}
    else{Err(error("not_started","尚未启动本地体验，或真实模式尚未就绪","显式启动本地体验；真实模式不会自动回退"))};
    if let Err(e)=&result{push_error(&mut snapshot,e.clone());}
    // GatewayConfigure is the one command whose effects are fully projected into
    // the worker snapshot inside the request branch (project_gateway above), so
    // its receipt must imply that projection is published: publish BEFORE the
    // receipt, or a drain-then-snapshot reader races the loop-tail publish
    // (proven by the flaky rust_gateway_manager background_service failure).
    // Other commands deliberately do NOT publish here: StartLocal sets readiness
    // early but the actor/runs projection happens at loop tail, so an early
    // publish would expose Ready without the actor (proven by the rust_service
    // reopen failure); project commands rely on receipt-carried contracts with a
    // deliberately stale published snapshot (rust_projects negative control).
    if is_configure && result.is_ok() {
        publish(&shared,&snapshot,&repaint,&mut last);
    }
    // A result credit was reserved before acceptance. At most BOUND results can be outstanding.
    if tx.try_send(CommandResult{request_id:request.id,result}).is_err(){shared.stop.store(true,Ordering::Release);}
    repaint_safely(&repaint);
   },
   Err(mpsc::RecvTimeoutError::Disconnected)=>{shared.stop.store(true,Ordering::Release);},
   Err(mpsc::RecvTimeoutError::Timeout)=>{if let Ok(mut credentials)=shared.credentials.lock(){credentials.clear_if_expired(clock());}},
  }
  if !shared.stop.load(Ordering::Acquire){if let Some(local)=runtime.as_mut(){
   if pump_local(&mut local.control.store,&mut local.runner,&mut local.ao,clock()).is_err(){snapshot.readiness=Readiness::NotReady;push_error(&mut snapshot,error("lifecycle_error","本地执行或持久化失败；未推断退出或释放名额","查看保留的任务状态并人工核对；不要自动重跑 UNKNOWN 任务"));}
   if local.project(&mut snapshot,clock()).is_err(){snapshot.readiness=Readiness::NotReady;push_error(&mut snapshot,error("snapshot_error","核心状态读取失败；保留上次快照","停止操作并检查状态目录；上次快照不代表当前进度"));}
  }}
  // Replace durable project cards on every refresh; stale scope must not stay
  // actionable after an error, expiry, decision, or revoked workflow binding.
  snapshot.approvals.retain(|approval|approval.simulated&&runtime.is_some());
  if let Some(p)=projects.as_mut(){
   if p.host_configured(){
    match p.node_readiness(clock()) {
     Ok(nodes)=>{snapshot.nodes=nodes.into_iter().map(|node|NodeView{id:node.id,address:node.address,used:node.used,slots:node.slots,admitted:node.admitted,healthy:node.healthy,draining:node.draining,source:if node.verified{"signed readiness probe/ready matched current node, deployment, address, epoch and capability".into()}else{"configured real node; readiness not verified or expired".into()},observed_at:node.observed_at}).collect();snapshot.readiness=if !snapshot.nodes.is_empty()&&snapshot.nodes.iter().all(|node|node.healthy){Readiness::Ready}else{Readiness::NotReady};for project in &mut snapshot.projects{if snapshot.readiness!=Readiness::Ready&&!project.blockers.iter().any(|blocker|blocker.contains("readiness")){project.blockers.push("真实节点未通过当前签名 readiness probe/ready；配置有效不等于可派工".into());project.can_split=false;project.can_start=false;project.can_test=false;project.can_repair=false;}}}
     Err(_)=>{snapshot.readiness=Readiness::NotReady;push_error(&mut snapshot,error("readiness_unavailable","真实节点 readiness 尚未验证或状态读取失败；未允许新派工","检查当前 probe/ready、部署、地址、attempt/epoch、有效期和模板能力；不释放既有名额"));}
    }
   }
   if p.host_configured()&&p.tick().is_err(){snapshot.readiness=Readiness::NotReady;for node in &mut snapshot.nodes{node.healthy=false;node.source="project transport unavailable; readiness invalidated".into();}push_error(&mut snapshot,error("project_execution_pending","项目执行端口或结果暂不可用；账本保留，未推断完成或释放名额","刷新核对项目阻塞原因；不要自动重派 UNKNOWN 动作"));}
   match p.approvals(clock()) {
    Ok(approvals)=>snapshot.approvals.extend(approvals),
    Err(_)=>push_error(&mut snapshot,error("project_approvals_unavailable","项目审批读取失败；已移除上次项目审批卡片","保留账本并刷新对应主机；未读取到审批不代表任务完成"))
   }
   snapshot.projects=p.snapshots();if snapshot.readiness!=Readiness::Ready{for project in &mut snapshot.projects{let blocker="真实节点未通过当前签名 readiness probe/ready；配置有效不等于可派工";if !project.blockers.iter().any(|existing|existing==blocker){project.blockers.push(blocker.into());}project.can_split=false;project.can_start=false;project.can_test=false;project.can_repair=false;}}snapshot.unfinished=runtime.as_ref().map_or(0,|_|snapshot.unfinished)+p.unfinished();
  }
  if let Some(manager)=gateway.as_ref() {
   if let Err(e)=project_gateway(manager,&mut snapshot,preflight_passed,shared.credentials.lock().map(|credentials| credentials.is_available()).unwrap_or(false)) {push_error(&mut snapshot,error("gateway_snapshot_failed",&e.to_string(),"保留账本并检查实际执行证据"));}
  }
  publish(&shared,&snapshot,&repaint,&mut last);
 }
}

/// Append one manager-local diagnostic line to `<state_dir>/manager-diag.log`.
/// Shape: `<unix_ts>\t<event>\t<detail>` — ids, statuses and paths only, never
/// secrets. Tabs/newlines in event or detail are rejected so a field value can
/// never forge extra lines or extra columns.
pub fn append_diag(state_dir:&Path,event:&str,detail:&str)->Result<()> {
 ensure!(!event.is_empty()&&event.chars().all(|c|c.is_ascii_alphanumeric()||c=='_'||c=='-'),"diag event must be a plain token");
 ensure!(!event.contains('\t')&&!detail.contains('\t')&&!event.contains('\n')&&!detail.contains('\n')&&!detail.contains('\r'),"diag fields must not contain tabs or newlines");
 // Real-mode managers never create --state-dir at startup (Projects::open is
 // read-only; only materialize_artifact_package builds it after a done run),
 // so the first diag lines of a session are written before the dir exists.
 // Create it here: diagnostics must survive, not drop silently via `let _ =`.
 std::fs::create_dir_all(state_dir)?;
 let now=clock();
 let line=format!("{now}\t{event}\t{detail}\n");
 let mut file=std::fs::OpenOptions::new().create(true).append(true).open(state_dir.join("manager-diag.log"))?;
 file.write_all(line.as_bytes())?;Ok(())
}

fn gateway_command(options:&mut ServiceOptions,shared:&Shared,gateway:&mut Option<super::ssh_manager::SshManager>,snapshot:&mut AppSnapshot,command:UiCommand,factory:&dyn GatewayTransportFactory)->Result<Value> {
 ensure!(options.mode==Mode::Real,"direct SSH requires explicit real mode");
 match command {
  UiCommand::GatewayConfigure{config_path}=>{
   ensure!(gateway.is_none(),"gateway already configured; restart before changing pinned configuration");
   let manager=super::ssh_manager::SshManager::open(&config_path)?;
   (shared.before_publication)();
   let publication=PublicationGuard::try_new(shared)?;
   let mut projected=snapshot.clone();
   project_gateway(&manager,&mut projected,false,shared.credentials.lock().map(|credentials| credentials.is_available()).unwrap_or(false))?;
   let result=json!({"worker_id":manager.config().worker_id,"remote_verified":false,"state":"config_only"});
   let _=append_diag(&options.state_dir,"gateway_configured",&format!("worker={} artifact={}",manager.config().worker_id,&manager.config().artifact_sha256[..8]));
   *gateway=Some(manager);*snapshot=projected.clone();publish_snapshot(shared,&projected);drop(publication);Ok(result)
  }
  UiCommand::GatewayDispatch{worker_id}=>{
   let manager=gateway.as_mut().context("explicit SSH worker configuration required")?;
   ensure!(manager.config().worker_id==worker_id,"selected worker differs from pinned config");
   (shared.before_work_registration)();
   let _in_flight=InFlightGuard::try_new(shared)?;
   let lease=shared.credentials.lock().map_err(|_|anyhow::anyhow!("session credentials unavailable"))?.take_for_dispatch().context("gateway credential unavailable; launch with --gateway-key-stdin")?;
   let mut transport=factory.sum_transport(manager.config().clone(),lease,shared.stop.clone())?;
   let result=manager.dispatch_with_transport(&Actor::Local("direct-ssh-manager".into()),&mut transport,clock())?;
   let _=append_diag(&options.state_dir,"dispatch_result",&format!("run_id={} status={}",result.run_id,result.status));
   Ok(serde_json::to_value(result)?)
  }
  UiCommand::GatewayDispatchProject{worker_id,doc_path}=>{
   let manager=gateway.as_mut().context("explicit SSH worker configuration required")?;
   ensure!(manager.config().worker_id==worker_id,"selected worker differs from pinned config");
   // Read the confirmed design document from an explicit operator path only.
   let raw=std::fs::read(&doc_path).with_context(||format!("文档读取失败: {}",doc_path.display()))?;
   ensure!(!raw.is_empty(),"文档为空；project-work 派工需要已确认的设计文档");
   ensure!(raw.len()<=65536,"文档超过 64KiB 上限；过大文档请拆分后重新派工");
   let text=String::from_utf8(raw).context("文档必须为 UTF-8 文本")?;
   (shared.before_work_registration)();
   let _in_flight=InFlightGuard::try_new(shared)?;
   let lease=shared.credentials.lock().map_err(|_|anyhow::anyhow!("session credentials unavailable"))?.take_for_dispatch().context("gateway credential unavailable; launch with --gateway-key-stdin")?;
   ensure!(lease.has_model_token(),"model token unavailable; launch with framed --gateway-key-stdin (key + token)");
   let document=super::project_work::ConfirmedDocument{version:1,sha256:encode_sha256(text.as_bytes()),text};
   let tasks=vec![super::project_work::WorkTask{id:"impl-page".into(),title:"实现设计静态页".into(),description:"按设计文档生成静态文件（index.html、css/style.css、js/data.js、js/app.js）".into(),acceptance:"产物包含设计文档要求的稳定选择器与交互；总大小 ≤16KB".into(),depends_on:vec![],review:"approved".into(),reason:None}];
   let work=super::project_work::ProjectWork{schema:"project-work/1".into(),project_id:"doc-project".into(),action_id:"a1".into(),stage:super::project_work::ProjectStage::Develop,document,tasks,target_task:Some("impl-page".into()),repair_reason:None,repair_round:0,test_spec:None,artifact:None};
   let mut transport=factory.project_transport(manager.config().clone(),lease,shared.stop.clone())?;
   let result=manager.dispatch_project_with_transport(&Actor::Local("direct-ssh-manager".into()),&work,&mut transport,clock())?;
   let _=append_diag(&options.state_dir,"dispatch_result",&format!("run_id={} status={} project=doc-project",result.run_id,result.status));
   // Materialize the admitted artifact package (if any) into the local state
   // directory so the GUI can point at real files on disk.
   let mut value=serde_json::to_value(result)?;
   if value["status"]=="done" {
    let run_id=value["run_id"].as_str().context("gateway result run id missing")?.to_owned();
    let run=manager.run(&run_id)?;
    if let Some(package)=run.detail.get("report").and_then(|report|report.get("artifact")) {
     let target=options.state_dir.join("artifacts").join(&run_id);
     match super::owner_project::materialize_artifact_package(package,&target) {
      Ok(written)=>{manager.annotate_run(&run_id,"artifact_dir",json!(target.display().to_string()))?;value["artifact_dir"]=json!(target.display().to_string());value["artifact_files"]=json!(written);let _=append_diag(&options.state_dir,"artifact_materialized",&format!("run_id={} files={}",run_id,written.len()));}
      Err(error)=>{let _=append_diag(&options.state_dir,"artifact_error",&format!("run_id={} error={}",run_id,error.root_cause()));value["artifact_error"]=json!(format!("{error:#}"));}
     }
    }
   }
   Ok(value)
  }
  _=>anyhow::bail!("not a gateway command"),
 }
}

fn encode_sha256(bytes:&[u8])->String { use sha2::Digest; hex::encode(sha2::Sha256::digest(bytes)) }

fn submit_confirmed_project_work(
 options:&mut ServiceOptions,
 shared:&Shared,
 gateway:&mut Option<super::ssh_manager::SshManager>,
 snapshot:&mut AppSnapshot,
 preflight_passed:&mut bool,
 projects:&mut super::projects::Projects,
 project_id:&str,
 factory:&dyn GatewayTransportFactory,
 emit:&mut dyn FnMut(super::live_output::LiveOutput),
)->Result<Value> {
 ensure!(options.mode==Mode::Real,"真实模式才支持直接提交项目");
 ensure!(*preflight_passed,"当前会话尚未完成 Worker 自动准备；配置 Worker 不等于可提交");
 let manager=gateway.as_mut().context("当前没有活动 Worker；先完成 Worker 自动准备")?;
 ensure!(snapshot.gateway.as_ref().is_some_and(|view|view.preflight_passed),"当前 Worker 尚未通过本会话准备门禁；请重新准备 Worker");
 let document=projects.confirmed_document(project_id)?;
 (shared.before_work_registration)();
 let _in_flight=InFlightGuard::try_new(shared)?;
 let lease=shared.credentials.lock().map_err(|_|anyhow::anyhow!("session credentials unavailable"))?.take_for_dispatch().context("当前 Worker 凭据不可用；请重新准备 Worker")?;
 ensure!(lease.has_model_token(),"当前 Worker 的模型凭据不可用；请重新准备 Worker");
 let action_id=uuid::Uuid::new_v4().to_string();
 let work=super::project_work::ProjectWork{
  schema:"project-work/1".into(),project_id:project_id.into(),action_id,
  stage:super::project_work::ProjectStage::Develop,document:document.clone(),
  tasks:vec![super::project_work::WorkTask{id:"impl-page".into(),title:"实现已确认设计文档".into(),description:"按已确认设计文档生成项目产物".into(),acceptance:"产物应对应已确认设计文档并通过独立 Store 终态证据".into(),depends_on:vec![],review:"approved".into(),reason:None}],
  target_task:Some("impl-page".into()),repair_reason:None,repair_round:0,test_spec:None,artifact:None,
 };
 let mut transport=factory.project_transport(manager.config().clone(),lease,shared.stop.clone())?;
 let result=manager.dispatch_project_stream(&Actor::Local("direct-ssh-manager".into()),&work,&mut transport,clock(),emit)?;
 let mut value=serde_json::to_value(&result)?;
 value["project_id"]=json!(project_id);
 value["document_version"]=json!(document.version);
 value["document_sha256"]=json!(document.sha256);
 if result.status=="done" {
  let run=manager.run(&result.run_id)?;
  let package=run.detail.get("report").and_then(|report|report.get("artifact")).context("admitted project result missing artifact package")?;
  let target=options.state_dir.join("artifacts").join(&result.run_id);
  let written=match super::owner_project::materialize_artifact_package(package,&target) {
   Ok(written)=>written,
   Err(error)=>{
    let _=append_diag(&options.state_dir,"artifact_error",&format!("run_id={} error={}",result.run_id,error.root_cause()));
    return Err(error.into());
   }
  };
  manager.annotate_run(&result.run_id,"artifact_dir",json!(target.display().to_string()))?;
  // The committed write log is what the result page lists for the user; it is
  // the core's own record of what it materialized, not a client-side scan.
  manager.annotate_run(&result.run_id,"artifact_files",json!(written))?;
  value["artifact_dir"]=json!(target.display().to_string());
  value["artifact_files"]=json!(written);
  let _=append_diag(&options.state_dir,"artifact_materialized",&format!("run_id={} files={}",result.run_id,written.len()));
 }
 let _=append_diag(&options.state_dir,"dispatch_result",&format!("run_id={} status={} project={}",result.run_id,result.status,project_id));
 if let Err(e)=project_gateway(manager,snapshot,*preflight_passed,shared.credentials.lock().map(|credentials| credentials.is_available()).unwrap_or(false)){push_error(snapshot,error("gateway_snapshot_failed",&e.to_string(),"保留账本并检查实际执行证据"));}
 Ok(value)
}

fn project_gateway(manager:&super::ssh_manager::SshManager,snapshot:&mut AppSnapshot,preflight_passed:bool,credentials_available:bool)->Result<()> { let config=manager.config();let used=manager.used_slots()?;
 snapshot.gateway=Some(GatewayView{worker_id:config.worker_id.clone(),host:config.host.clone(),remote_verified:false,preflight_passed: preflight_passed && credentials_available,credentials_available,used_slots:used,artifact_sha256:config.artifact_sha256.clone()});
 snapshot.role="direct SSH Manager; config-only, remote not verified".into();
 snapshot.runs=manager.runs()?.into_iter().map(|run|RunView{run,read_only:true,can_status:false,can_approvals:false,can_decide:false,can_report:false,can_retry:false,can_cancel:false,disabled_reason:Some("SSH执行证据来自同一账本；未确认终态保留名额，不自动重跑".into())}).collect();
 snapshot.unfinished=snapshot.runs.iter().filter(|view|!matches!(view.run.status.as_str(),"done"|"failed"|"cancelled")).count().max(used as usize);
 Ok(())
}
