//! Conservative UI projection only; Service/Store remain submission authority.
use agentranch::core::service::AppSnapshot;
use super::pending::Pending;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerSubmitGate {
    Preparing,
    Unknown,
    Submitting,
    Executing,
    NotConnected,
    SessionExpired,
    WaitingRemote,
    Occupied,
    Ready,
}

impl WorkerSubmitGate {
    pub(crate) fn from_snapshot(snapshot: &AppSnapshot, pending: &Pending) -> Self {
        if pending.contains("worker-provision") { return Self::Preparing; }
        let project_runs = || snapshot.runs.iter().filter(|view| view.run.template == "project-work-v1");
        if project_runs().any(|view| view.run.status == "unknown") { return Self::Unknown; }
        if project_runs().any(|view| !matches!(view.run.status.as_str(), "done" | "failed" | "cancelled")) {
            return Self::Executing;
        }
        if pending.has_project_work_submit() { return Self::Submitting; }
        let Some(gateway) = snapshot.gateway.as_ref() else { return Self::NotConnected; };
        if gateway.preflight_passed && !gateway.credentials_available { return Self::SessionExpired; }
        if !gateway.preflight_passed { return Self::WaitingRemote; }
        if gateway.used_slots > 0 { return Self::Occupied; }
        Self::Ready
    }

    pub(crate) fn blocking_guidance(self) -> &'static str {
        match self {
            Self::Preparing => "Worker 正在准备，等待回执后再提交项目。",
            Self::Unknown => "任务结果未知，名额保留；查看结果核对和操作回执，不要手动释放或自动重跑。",
            Self::Submitting => "项目已提交，等待回执；查看模型实时输出，不要重复提交。",
            Self::Executing => "已有项目任务执行中；查看模型实时输出和操作回执，当前不能再次提交。",
            Self::NotConnected => "完成 Worker 准备并确认上传版本后，才能提交项目。",
            Self::SessionExpired => "会话凭据已超时；打开连接设置重新认证后再提交项目。",
            Self::WaitingRemote => "Worker 等待远端预检通过；当前不能提交项目。",
            Self::Occupied => "Worker 名额已占用；查看现有任务状态与操作回执，当前不能再次提交。",
            Self::Ready => "",
        }
    }
}
