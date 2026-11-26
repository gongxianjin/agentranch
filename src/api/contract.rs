use crate::core::service::{CommandResult, ProvisionRequest, ServiceError, UiCommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue};
use std::path::PathBuf;

pub const CONTRACT_VERSION: u32 = 1;

fn bad_request() -> ServiceError {
    ServiceError {
        code: "bad_request".into(),
        message: "命令请求格式无效；未执行任何操作".into(),
        next_step: "核对 v1 命令 kind、args 及字段类型后重试".into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    kind: String,
    args: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs { config_path: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerArgs { worker_id: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerDocArgs { worker_id: String, doc_path: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectArgs { project_id: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateArgs { template: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs { run_id: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideArgs {
    run_id: String,
    attempt: u32,
    session: String,
    request_id: String,
    generation: u64,
    scope_hash: String,
    decision_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryArgs { run_id: String, attempt: u32 }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectNameArgs { name: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectImportArgs { project_id: String, path: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfirmArgs { project_id: String, version: u32, sha256: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskReviewArgs { project_id: String, task_id: String, approve: bool, reason: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskArgs { project_id: String, task_id: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectRepairArgs { project_id: String, reason: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionArgs {
    host: String,
    root_user: String,
    root_password: String,
    model_base_url: String,
    model_token: String,
    model: String,
}

fn parse_args<T: for<'de> Deserialize<'de>>(raw: &RawValue) -> Result<T, ServiceError> {
    let text = raw.get().trim();
    if !text.starts_with('{') || !text.ends_with('}') {
        return Err(bad_request());
    }
    serde_json::from_str(text).map_err(|_| bad_request())
}

fn path(value: String) -> PathBuf { PathBuf::from(value) }

pub fn decode_command(bytes: &[u8]) -> Result<UiCommand, ServiceError> {
    // Serde structs also accept positional arrays unless the JSON shape is checked.
    if !bytes.iter().copied().find(|byte| !byte.is_ascii_whitespace()).is_some_and(|byte| byte == b'{') {
        return Err(bad_request());
    }
    let raw: RawCommand = serde_json::from_slice(bytes).map_err(|_| bad_request())?;
    let command = match raw.kind.as_str() {
        "gateway_configure" => { let args: PathArgs = parse_args(&raw.args)?; UiCommand::GatewayConfigure { config_path: path(args.config_path) } }
        "gateway_dispatch" => { let args: WorkerArgs = parse_args(&raw.args)?; UiCommand::GatewayDispatch { worker_id: args.worker_id } }
        "gateway_dispatch_project" => { let args: WorkerDocArgs = parse_args(&raw.args)?; UiCommand::GatewayDispatchProject { worker_id: args.worker_id, doc_path: path(args.doc_path) } }
        "project_work_submit" => { let args: ProjectArgs = parse_args(&raw.args)?; UiCommand::ProjectWorkSubmit { project_id: args.project_id } }
        "provision_worker" => {
            let args: ProvisionArgs = parse_args(&raw.args)?;
            UiCommand::ProvisionWorker { request: ProvisionRequest {
                host: args.host,
                root_user: args.root_user,
                root_password: args.root_password.into_bytes(),
                model_base_url: args.model_base_url,
                model_token: args.model_token.into_bytes(),
                model: args.model,
            }}
        }
        "start_local" => { let _: EmptyArgs = parse_args(&raw.args)?; UiCommand::StartLocal }
        "dispatch" => { let args: TemplateArgs = parse_args(&raw.args)?; UiCommand::Dispatch { template: args.template } }
        "status" => { let args: RunArgs = parse_args(&raw.args)?; UiCommand::Status { run_id: args.run_id } }
        "approvals" => { let args: RunArgs = parse_args(&raw.args)?; UiCommand::Approvals { run_id: args.run_id } }
        "decide" => {
            let args: DecideArgs = parse_args(&raw.args)?;
            UiCommand::Decide { run_id: args.run_id, attempt: args.attempt, session: args.session, request_id: args.request_id, generation: args.generation, scope_hash: args.scope_hash, decision_id: args.decision_id }
        }
        "report" => { let args: RunArgs = parse_args(&raw.args)?; UiCommand::Report { run_id: args.run_id } }
        "retry" => { let args: RetryArgs = parse_args(&raw.args)?; UiCommand::Retry { run_id: args.run_id, attempt: args.attempt } }
        "cancel" => { let args: RunArgs = parse_args(&raw.args)?; UiCommand::Cancel { run_id: args.run_id } }
        "project_create" => { let args: ProjectNameArgs = parse_args(&raw.args)?; UiCommand::ProjectCreate { name: args.name } }
        "project_import" => { let args: ProjectImportArgs = parse_args(&raw.args)?; UiCommand::ProjectImport { project_id: args.project_id, path: path(args.path) } }
        "project_confirm" => { let args: ProjectConfirmArgs = parse_args(&raw.args)?; UiCommand::ProjectConfirm { project_id: args.project_id, version: args.version, sha256: args.sha256 } }
        "project_split" => { let args: ProjectArgs = parse_args(&raw.args)?; UiCommand::ProjectSplit { project_id: args.project_id } }
        "task_review" => { let args: TaskReviewArgs = parse_args(&raw.args)?; UiCommand::TaskReview { project_id: args.project_id, task_id: args.task_id, approve: args.approve, reason: args.reason } }
        "task_redo" => { let args: TaskArgs = parse_args(&raw.args)?; UiCommand::TaskRedo { project_id: args.project_id, task_id: args.task_id } }
        "project_start" => { let args: ProjectArgs = parse_args(&raw.args)?; UiCommand::ProjectStart { project_id: args.project_id } }
        "project_test" => { let args: ProjectArgs = parse_args(&raw.args)?; UiCommand::ProjectTest { project_id: args.project_id } }
        "project_repair" => { let args: ProjectRepairArgs = parse_args(&raw.args)?; UiCommand::ProjectRepair { project_id: args.project_id, reason: args.reason } }
        "project_accept" => { let args: ProjectArgs = parse_args(&raw.args)?; UiCommand::ProjectAccept { project_id: args.project_id } }
        "project_refresh" => { let args: ProjectArgs = parse_args(&raw.args)?; UiCommand::ProjectRefresh { project_id: args.project_id } }
        "project_configure" => { let args: PathArgs = parse_args(&raw.args)?; UiCommand::ProjectConfigure { config_path: path(args.config_path) } }
        _ => return Err(bad_request()),
    };
    Ok(command)
}

fn text(value: &[u8]) -> Result<String, ServiceError> {
    String::from_utf8(value.to_vec()).map_err(|_| bad_request())
}

fn envelope<T: Serialize>(kind: &str, args: T) -> Result<Vec<u8>, ServiceError> {
    serde_json::to_vec(&json!({"kind": kind, "args": args})).map_err(|_| bad_request())
}

pub fn encode_command(command: &UiCommand) -> Result<Vec<u8>, ServiceError> {
    match command {
        UiCommand::GatewayConfigure { config_path } => envelope("gateway_configure", json!({"config_path": config_path.to_str().ok_or_else(bad_request)?})),
        UiCommand::GatewayDispatch { worker_id } => envelope("gateway_dispatch", json!({"worker_id": worker_id})),
        UiCommand::GatewayDispatchProject { worker_id, doc_path } => envelope("gateway_dispatch_project", json!({"worker_id": worker_id, "doc_path": doc_path.to_str().ok_or_else(bad_request)?})),
        UiCommand::ProjectWorkSubmit { project_id } => envelope("project_work_submit", json!({"project_id": project_id})),
        UiCommand::ProvisionWorker { request } => envelope("provision_worker", json!({"host": request.host, "root_user": request.root_user, "root_password": text(&request.root_password)?, "model_base_url": request.model_base_url, "model_token": text(&request.model_token)?, "model": request.model})),
        UiCommand::StartLocal => envelope("start_local", json!({})),
        UiCommand::Dispatch { template } => envelope("dispatch", json!({"template": template})),
        UiCommand::Status { run_id } => envelope("status", json!({"run_id": run_id})),
        UiCommand::Approvals { run_id } => envelope("approvals", json!({"run_id": run_id})),
        UiCommand::Decide { run_id, attempt, session, request_id, generation, scope_hash, decision_id } => envelope("decide", json!({"run_id": run_id, "attempt": attempt, "session": session, "request_id": request_id, "generation": generation, "scope_hash": scope_hash, "decision_id": decision_id})),
        UiCommand::Report { run_id } => envelope("report", json!({"run_id": run_id})),
        UiCommand::Retry { run_id, attempt } => envelope("retry", json!({"run_id": run_id, "attempt": attempt})),
        UiCommand::Cancel { run_id } => envelope("cancel", json!({"run_id": run_id})),
        UiCommand::ProjectCreate { name } => envelope("project_create", json!({"name": name})),
        UiCommand::ProjectImport { project_id, path } => envelope("project_import", json!({"project_id": project_id, "path": path.to_str().ok_or_else(bad_request)?})),
        UiCommand::ProjectConfirm { project_id, version, sha256 } => envelope("project_confirm", json!({"project_id": project_id, "version": version, "sha256": sha256})),
        UiCommand::ProjectSplit { project_id } => envelope("project_split", json!({"project_id": project_id})),
        UiCommand::TaskReview { project_id, task_id, approve, reason } => envelope("task_review", json!({"project_id": project_id, "task_id": task_id, "approve": approve, "reason": reason})),
        UiCommand::TaskRedo { project_id, task_id } => envelope("task_redo", json!({"project_id": project_id, "task_id": task_id})),
        UiCommand::ProjectStart { project_id } => envelope("project_start", json!({"project_id": project_id})),
        UiCommand::ProjectTest { project_id } => envelope("project_test", json!({"project_id": project_id})),
        UiCommand::ProjectRepair { project_id, reason } => envelope("project_repair", json!({"project_id": project_id, "reason": reason})),
        UiCommand::ProjectAccept { project_id } => envelope("project_accept", json!({"project_id": project_id})),
        UiCommand::ProjectRefresh { project_id } => envelope("project_refresh", json!({"project_id": project_id})),
        UiCommand::ProjectConfigure { config_path } => envelope("project_configure", json!({"config_path": config_path.to_str().ok_or_else(bad_request)?})),
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum CommandResponse {
    Ok { request_id: u64, result: serde_json::Value },
    Err { request_id: u64, error: ServiceError },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OkResponse {
    request_id: u64,
    result: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrResponse {
    request_id: u64,
    error: ServiceError,
}

impl<'de> Deserialize<'de> for CommandResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum WireResponse {
            Ok(OkResponse),
            Err(ErrResponse),
        }

        match WireResponse::deserialize(deserializer)? {
            WireResponse::Ok(response) => Ok(Self::Ok {
                request_id: response.request_id,
                result: response.result,
            }),
            WireResponse::Err(response) => Ok(Self::Err {
                request_id: response.request_id,
                error: response.error,
            }),
        }
    }
}

impl From<CommandResult> for CommandResponse {
    fn from(result: CommandResult) -> Self {
        match result.result {
            Ok(value) => Self::Ok { request_id: result.request_id, result: value },
            Err(error) => Self::Err { request_id: result.request_id, error },
        }
    }
}
