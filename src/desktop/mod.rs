pub mod action;
pub mod approval;
pub mod launch;
pub mod pending;
pub mod projects;
pub mod project_activity;
pub mod worker_settings;
mod worker_submit;
pub mod theme;
use worker_submit::WorkerSubmitGate;
#[cfg(test)]
#[path = "../../tests/support/settings_vault.rs"]
mod settings_vault;
use std::{collections::VecDeque, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};
use agentranch::{config::Mode, core::{provision_ledger::LedgerState, service::{validate_provision_input, AppSnapshot, ProvisionRequest, ServiceError, ServiceHandle, ServiceOptions, UiCommand, SubmitError, Readiness, RunView}}};
use serde_json::Value;
use eframe::egui::{self, RichText};

/// Convert only the validator's stable, non-secret categories into actionable
/// novice-facing copy. Unknown diagnostics stay generic so arbitrary helper or
/// transport text cannot be echoed into the onboarding form.
pub fn onboarding_input_error_copy(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("model_base_url") {
        return "第三方服务地址无效，请填写不带路径、查询参数或用户信息的 HTTPS origin".into();
    }
    if message.contains("host must be a valid IP address")
        || message.contains("host must be an IPv4 address")
        || message.contains("host is required")
    {
        return "Linux 工作节点 IP 无效，请填写 IPv4 地址，例如 192.168.1.20".into();
    }
    if message.contains("root_user") {
        return "root 账号无效，请填写不含空格或控制字符的账号名".into();
    }
    if message.contains("root_password") {
        return "请填写 root 密码；仅用于本次准备，不会写入回执或日志".into();
    }
    if message.contains("model_token") {
        return "请填写 API key；仅用于本次准备，不会写入回执或日志".into();
    }
    if message.contains("model") {
        return "请填写默认模型，例如 gpt-6.1-sol".into();
    }
    "输入未通过校验，请检查六项信息后重试".into()
}

struct Receipt { id: u64, action: String, detail: String, diagnostic_detail: String, failed: bool }

fn safe_receipt_detail(value: &Value) -> String {
    if value.get("state").and_then(Value::as_str) == Some("worker_preflight") {
        return "Worker 准备完成。\n状态：Worker 已连接，可提交项目。".into();
    }
    let mut safe = value.clone();
    if let Some(object) = safe.as_object_mut() {
        object.remove("gateway_path");
    }
    serde_json::to_string_pretty(&safe).unwrap_or_else(|_| "Worker 准备完成。".into())
}
/// UI selection, receipts and pending interactions only. Domain state is always core's Arc snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnboardingUiState {
    NotStarted,
    InputCorrection(String),
    Preparing,
    Failed { code: String, next_step: String },
    WaitingRemote,
    SessionExpired,
    Connected,
    History,
    Executing,
    Submitting,
    Unknown,
    Occupied,
    Admitted,
}

pub fn assign_unique_gateway_worker(selected: &mut String, snapshot: &AppSnapshot) {
    if let Some(gateway) = snapshot.gateway.as_ref() {
        *selected = gateway.worker_id.clone();
    }
}

pub fn onboarding_ui_state(
    snapshot: &AppSnapshot,
    pending: &pending::Pending,
    input_error: Option<&str>,
    failure: Option<&(String, String)>,
) -> OnboardingUiState {
    let gate = WorkerSubmitGate::from_snapshot(snapshot, pending);
    match gate {
        WorkerSubmitGate::Preparing => return OnboardingUiState::Preparing,
        WorkerSubmitGate::Unknown => return OnboardingUiState::Unknown,
        WorkerSubmitGate::Submitting => return OnboardingUiState::Submitting,
        WorkerSubmitGate::Executing => return OnboardingUiState::Executing,
        WorkerSubmitGate::Occupied => return OnboardingUiState::Occupied,
        WorkerSubmitGate::SessionExpired => return OnboardingUiState::SessionExpired,
        WorkerSubmitGate::WaitingRemote => return OnboardingUiState::WaitingRemote,
        WorkerSubmitGate::Ready | WorkerSubmitGate::NotConnected => {},
    }
    // Admission is a current Store-backed result, not readiness for new work.
    // Consider it only after ALL task/Worker blockers, independent of run order.
    if snapshot.runs.iter().any(|view| view.run.template == "project-work-v1"
        && view.run.status == "done"
        && view.run.detail["owner_admission"] == "admitted"
        && view.run.detail["exit_proof"].is_object()
        && view.run.detail["artifact_dir"].as_str().is_some_and(|path| !path.is_empty())) {
        return OnboardingUiState::Admitted;
    }
    if gate == WorkerSubmitGate::Ready { return OnboardingUiState::Connected; }
    if let Some(error) = input_error { return OnboardingUiState::InputCorrection(error.into()); }
    if let Some((code, next_step)) = failure { return OnboardingUiState::Failed { code: code.clone(), next_step: next_step.clone() }; }
    if let Some(ledger) = snapshot.provision_ledger.as_ref() {
        if ledger.state == LedgerState::Failed {
            if let (Some(code), Some(next)) = (&ledger.failure_code, &ledger.failure_next_step) {
                return OnboardingUiState::Failed { code: code.clone(), next_step: next.clone() };
            }
        }
        return OnboardingUiState::History;
    }
    OnboardingUiState::NotStarted
}

pub fn provisioning_status_copy(snapshot: &AppSnapshot, pending: &pending::Pending, input_error: Option<&str>, failure: Option<&(String, String)>) -> &'static str {
    match onboarding_ui_state(snapshot, pending, input_error, failure) {
        OnboardingUiState::NotStarted => "未开始",
        OnboardingUiState::InputCorrection(_) => "输入待修正",
        OnboardingUiState::Preparing => "正在准备",
        OnboardingUiState::Failed { .. } => "准备失败",
        OnboardingUiState::WaitingRemote => "Worker 已连接，等待远端就绪",
        OnboardingUiState::SessionExpired => "会话凭据已超时，需要重新准备 Worker",
        OnboardingUiState::Connected => "Worker 已连接，可提交项目",
        OnboardingUiState::History => "历史准备记录 · 需要重新认证",
        OnboardingUiState::Executing => "远端任务执行中",
        OnboardingUiState::Submitting => "项目已提交，等待回执",
        OnboardingUiState::Unknown => "任务结果未知，名额保留",
        OnboardingUiState::Occupied => "Worker 名额已占用，当前不能提交",
        OnboardingUiState::Admitted => "项目结果已准入",
    }
}

pub struct Workbench {
    service: ServiceHandle,
    pending: pending::Pending,
    receipts: VecDeque<Receipt>,
    selected: Option<String>,
    template: String,
    onboarding_host: String,
    onboarding_root_user: String,
    onboarding_root_password: String,
    onboarding_model_base_url: String,
    onboarding_model_token: String,
    onboarding_model: String,
    gateway_worker: String,
    onboarding_form_open: bool,
    onboarding_input_error: Option<String>,
    onboarding_failure: Option<(String, String)>,
    onboarding_reprepare_requested: bool,
    settings_store: worker_settings::SettingsStore,
    settings_notice: Option<String>,
    token_origin: String,
    state_dir: String,
    notice: Option<String>,
    confirm_close: bool,
    closing: bool,
    close_requested: bool,
    projects: projects::Projects,
    diagnostics: bool,
    project_diagnostics: bool,
    #[cfg(test)]
    snapshot_override: Option<AppSnapshot>,
}
impl Workbench {
    pub fn new(options: ServiceOptions, ctx: &egui::Context) -> anyhow::Result<Self> {
        Self::new_inner(options, ctx, None)
    }

    #[cfg(test)]
    pub fn new_with_provisioner(
        options: ServiceOptions,
        ctx: &egui::Context,
        provisioner: Box<dyn agentranch::core::provision::Provisioner>,
    ) -> anyhow::Result<Self> {
        Self::new_inner(options, ctx, Some(provisioner))
    }

    fn new_inner(
        options: ServiceOptions,
        ctx: &egui::Context,
        provisioner: Option<Box<dyn agentranch::core::provision::Provisioner>>,
    ) -> anyhow::Result<Self> {
        let repaint = ctx.clone();
        let state_dir = options.state_dir.display().to_string();
        let service = match provisioner {
            Some(provisioner) => ServiceHandle::spawn_with_provisioner(options, Arc::new(move || repaint.request_repaint()), provisioner)?,
            None => ServiceHandle::spawn(options, Arc::new(move || repaint.request_repaint()))?,
        };
        Ok(Self::from_service(service, ctx, state_dir))
    }

    /// Attach the ordinary UI to an existing core service; no native window is required.
    pub fn from_service(mut service: ServiceHandle, ctx: &egui::Context, state_dir: String) -> Self {
        let template = if service.snapshot().mode == Mode::Local { "sum-native-v1" } else { "sum-v1" }.into();
        let mut visuals = egui::Visuals::light();
        visuals.override_text_color = Some(egui::Color32::from_rgb(34, 50, 71));
        visuals.panel_fill = egui::Color32::from_rgb(245, 247, 250);
        ctx.set_theme(egui::ThemePreference::Light);
        ctx.set_visuals_of(egui::Theme::Light, visuals);
        let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
        style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(16.));
        style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(16.));
        style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(22.));
        style.spacing.item_spacing = egui::vec2(10., 8.);
        ctx.set_style_of(egui::Theme::Light, style);
        #[cfg(not(test))]
        let settings_store = worker_settings::SettingsStore::new(std::path::Path::new(&state_dir));
        #[cfg(test)]
        let settings_store = worker_settings::SettingsStore::with_vault(std::path::Path::new(&state_dir), Box::new(settings_vault::TestVault));
        let mut app = Self { service, pending: Default::default(), receipts: VecDeque::new(), selected: None, template, onboarding_host: String::new(), onboarding_root_user: String::new(), onboarding_root_password: String::new(), onboarding_model_base_url: String::new(), onboarding_model_token: String::new(), onboarding_model: String::new(), gateway_worker: String::new(), onboarding_form_open: false, onboarding_input_error: None, onboarding_failure: None, onboarding_reprepare_requested: false, settings_store, settings_notice: None, token_origin: String::new(), state_dir, notice: None, confirm_close: false, closing: false, close_requested: false, projects: Default::default(), diagnostics: false, project_diagnostics: false, #[cfg(test)] snapshot_override: None };
        app.restore_connection_settings();
        app
    }

    fn restore_connection_settings(&mut self) {
        match self.settings_store.load() {
            Ok(Some(saved)) => {
                self.onboarding_host = saved.settings.host;
                self.onboarding_root_user = saved.settings.root_user;
                self.onboarding_model_base_url = saved.settings.model_base_url;
                self.onboarding_model = saved.settings.model;
                match saved.token {
                    Ok(token) => {
                        self.token_origin = self.onboarding_model_base_url.clone();
                        self.onboarding_model_token = token;
                        self.settings_notice = Some("已恢复保存配置；仍需当前会话认证，不代表 Worker 已连接。".into());
                    }
                    Err(error) => {
                        self.onboarding_model_token.clear();
                        self.token_origin.clear();
                        self.settings_notice = Some(format!("四项普通配置已恢复；API key 未恢复，请重新输入并保存。恢复配置不代表 Worker 已连接。\n{error}"));
                    }
                }
            }
            Ok(None) => {},
            Err(error) => self.settings_notice = Some(error.to_string()),
        }
    }

    fn save_connection_settings(&mut self) -> bool {
        let settings = worker_settings::WorkerSettings {
            host: self.onboarding_host.trim().into(), root_user: self.onboarding_root_user.trim().into(),
            model_base_url: self.onboarding_model_base_url.trim().into(), model: self.onboarding_model.trim().into(),
        };
        match self.settings_store.save(&settings, &self.onboarding_model_token) {
            Ok(()) => {
                self.token_origin = settings.model_base_url;
                self.settings_notice = Some("配置已保存；API key 已安全保存，root 密码不保存。保存不代表 Worker 已连接。".into());
                true
            }
            Err(error) => { self.settings_notice = Some(error.to_string()); false }
        }
    }
    fn record_result(&mut self, request_id: u64, action: String, result: Result<Value, ServiceError>) {
        let is_provision = action == "worker-provision";
        let (detail, diagnostic_detail, failed) = match result {
            Ok(value) => {
                if is_provision {
                    self.onboarding_failure = None;
                    self.onboarding_form_open = false;
                    self.onboarding_reprepare_requested = false;
                }
                if let Some(id) = value.get("id").and_then(|v| v.as_str()) { self.selected = Some(id.into()); self.projects.selected = Some(id.into()); }
                let safe_detail = if is_provision {
                    safe_receipt_detail(&value)
                } else {
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                };
                let diagnostic_detail = serde_json::to_string_pretty(&value).unwrap_or_default();
                (safe_detail, diagnostic_detail, false)
            }
            Err(mut e) => {
                if is_provision {
                    // Only recognize this core-owned local category; never echo
                    // arbitrary Helper diagnostics into the settings modal.
                    if e.code == "provision_error" && e.message.starts_with("Worker 自动准备受阻（helper_unavailable）：") {
                        e.next_step = "本地 Worker 准备组件不可用，尚未连接 Linux。请解压并运行完整程序包，保留 tools/paramiko_provision.py 和 runtime/python/python.exe；不要只复制 exe，也不要因此反复修改密码或 API key。".into();
                    }
                    self.onboarding_failure = Some((e.code.clone(), e.next_step.clone()));
                    self.onboarding_reprepare_requested = false;
                    self.onboarding_root_password.clear();
                    self.onboarding_model_token.clear();
                    self.onboarding_form_open = true;
                }
                let detail = format!("{}: {}\n下一步：{}", e.code, e.message, e.next_step);
                (detail.clone(), detail, true)
            }
        };
        self.receipts.push_front(Receipt { id: request_id, action, detail, diagnostic_detail, failed });
        self.receipts.truncate(20);
    }

    pub fn poll(&mut self) {
        let (live, results) = self.service.drain_updates();
        for event in live { self.projects.apply_live(event); }
        for result in results {
            self.projects.finish_live(result.request_id, &result.result);
            let action = self.pending.complete(result.request_id).unwrap_or_else(|| "未关联请求".into());
            self.record_result(result.request_id, action, result.result);
        }
    }

    #[cfg(test)]
    pub fn inject_receipt_for_test(&mut self, value: Value) {
        self.record_result(1, "worker-provision".into(), Ok(value));
    }
    pub fn snapshot(&mut self) -> Arc<AppSnapshot> {
        #[cfg(test)]
        if let Some(snapshot) = self.snapshot_override.clone() { return Arc::new(snapshot); }
        self.service.snapshot()
    }

    #[cfg(test)]
    pub fn set_snapshot_for_test(&mut self, snapshot: AppSnapshot) { self.snapshot_override = Some(snapshot); }

    #[cfg(test)]
    pub fn set_onboarding_fields_for_test(&mut self, host: &str, root_user: &str, password: &str, base_url: &str, token: &str, model: &str) {
        self.onboarding_host = host.into();
        self.onboarding_root_user = root_user.into();
        self.onboarding_root_password = password.into();
        self.onboarding_model_base_url = base_url.into();
        self.onboarding_model_token = token.into();
        self.onboarding_model = model.into();
    }

    #[cfg(test)]
    pub fn onboarding_secrets_empty_for_test(&self) -> bool {
        self.onboarding_root_password.is_empty() && self.onboarding_model_token.is_empty()
    }

    #[cfg(test)]
    pub fn snapshot_json_for_test(&mut self) -> String {
        serde_json::to_string(&*self.snapshot()).expect("AppSnapshot must serialize")
    }

    #[cfg(test)]
    pub fn set_diagnostics_for_test(&mut self, diagnostics: bool) { self.diagnostics = diagnostics; }

    #[cfg(test)]
    pub fn set_project_diagnostics_for_test(&mut self, project_diagnostics: bool) { self.project_diagnostics = project_diagnostics; }

    pub fn send(&mut self, key: String, command: UiCommand) {
        if self.closing || self.pending.contains(&key) { return; }
        let project_id = match &command {UiCommand::ProjectWorkSubmit {project_id} => Some(project_id.clone()), _ => None};
        match self.service.try_submit(command) {
            Ok(id) => {
                if let Some(project_id) = project_id {self.projects.begin_live(id, project_id);}
                self.pending.accept(key, id); self.notice = None;
            }
            Err(SubmitError::Busy) => self.notice = Some("后台忙：此操作尚未接受，请稍后再试。".into()),
            Err(SubmitError::Stopped) => self.notice = Some("后台已停止：此操作未执行。重新打开工作台。".into()),
        }
    }
    pub fn pending_count(&self) -> usize { self.pending.len() }
    #[cfg(test)]
    pub fn receipt_count(&self) -> usize { self.receipts.len() }

    #[cfg(test)]
    pub fn set_pending_provision_for_test(&mut self) { self.pending.accept("worker-provision".into(), 1); }

    #[cfg(test)]
    pub fn set_onboarding_failure_for_test(&mut self, code: &str, next_step: &str) {
        self.onboarding_failure = Some((code.into(), next_step.into()));
        self.onboarding_form_open = true;
    }

    fn reopen_onboarding_form(&mut self) {
        if self.onboarding_model_token.is_empty() {
            match self.settings_store.load() {
                Ok(Some(saved)) if saved.settings.model_base_url == self.onboarding_model_base_url.trim() => {
                    match saved.token {
                        Ok(token) => {
                            self.token_origin = saved.settings.model_base_url;
                            self.onboarding_model_token = token;
                            self.settings_notice = Some("API key 已恢复；当前表单内容已保留，仍需当前会话认证，不代表 Worker 已连接。".into());
                        }
                        Err(error) => {
                            self.token_origin.clear();
                            self.settings_notice = Some(format!("四项普通配置已保留；API key 未恢复，请重新输入并保存。恢复配置不代表 Worker 已连接。\n{error}"));
                        }
                    }
                }
                Err(error) => self.settings_notice = Some(error.to_string()),
                _ => {},
            }
        }
        self.onboarding_form_open = true;
        self.onboarding_reprepare_requested = true;
        self.onboarding_failure = None;
        self.onboarding_input_error = None;
    }

    fn password_text_edit_with_copy(ui: &mut egui::Ui, value: &mut String) {
        let output = egui::TextEdit::singleline(value).password(true).show(ui);
        let copy_requested = ui.input(|input| {
            input.events.iter().any(|event| matches!(event, egui::Event::Copy))
        });
        if copy_requested {
            if let Some(range) = output.cursor_range.filter(|range| !range.is_empty()) {
                ui.ctx().copy_text(range.slice_str(value).to_owned());
            }
        }
    }

    #[cfg(test)]
    pub fn reprepare_for_test(&mut self) { self.reopen_onboarding_form(); }

    #[cfg(test)]
    pub fn onboarding_form_open_for_test(&self) -> bool { self.onboarding_form_open && self.onboarding_reprepare_requested }

    #[cfg(test)]
    pub fn onboarding_reprepare_visible_for_test(&self, snapshot: &AppSnapshot) -> bool {
        snapshot.gateway.is_some() && self.onboarding_reprepare_requested
    }

    fn button(&mut self, ui: &mut egui::Ui, label: &str, enabled: bool, key: String, command: UiCommand) {
        let pending = self.pending.contains(&key);
        let label = if pending { format!("{label}（等待回执）") } else { label.into() };
        if ui.add_enabled(enabled && !pending, egui::Button::new(label)).clicked() { self.send(key, command); }
    }
    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.poll();
        let snapshot = self.snapshot();
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(250));
        if ctx.input(|i| i.viewport().close_requested()) && !self.close_requested {
            // eframe exits at the end of this frame unless closure is cancelled.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.closing {
                if snapshot.unfinished > 0 || self.pending_count() > 0 { self.confirm_close = true; }
                else { self.begin_close(); }
            }
        }
        if self.closing {
            if !snapshot.running && self.pending_count() == 0 {
                if !self.close_requested {
                    self.close_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else {
                ui.heading("正在关闭工作台");
                ui.label(format!("等待后台停止并收齐操作回执（剩余 {} 项）。关闭不代表远端任务已取消。", self.pending_count()));
                ctx.request_repaint_after(Duration::from_millis(20));
            }
            return;
        }
        egui::Panel::top("mode-toolbar").show(ui, |ui| {
            ui.heading(RichText::new("agentranch 工作台").color(theme::BRAND));
            ui.strong(RichText::new(if !self.diagnostics { "项目工作流 · 新建项目 → 确认设计 → 提交 → 查看结果" } else if snapshot.mode == Mode::Local { "本地体验 · 无真实 AO/Reason/模型" } else { "真实模式 · 直接 SSH Worker；配置不等于远端已验证" }).color(theme::BRAND));
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("服务：{}", if self.diagnostics { readiness(&snapshot.readiness) } else if snapshot.running { "后台运行；执行依赖见项目阻塞" } else { "后台已停止" }));
                ui.label(format!("等待回执：{}", self.pending_count()));
                if ui.button("项目工作流").clicked() { self.diagnostics = false; self.project_diagnostics = false; }
                if ui.button("高级 · 本地诊断").clicked() { self.diagnostics = true; self.project_diagnostics = false; }
                if self.diagnostics && snapshot.mode == Mode::Local {
                    self.button(ui, "启动本地体验", matches!(snapshot.readiness, Readiness::NotStarted | Readiness::NotReady), "start".into(), UiCommand::StartLocal);
                }
                if ui.button("关闭工作台").clicked() {
                    if snapshot.unfinished > 0 || self.pending_count() > 0 { self.confirm_close = true; }
                    else { self.begin_close(); }
                }
            });
            if !self.diagnostics && snapshot.mode == Mode::Local { ui.small("本地体验仅用于可选诊断；项目真实执行不会回退到模拟 AO / Reason / 模型。"); }
            if self.diagnostics { ui.small(format!("状态目录：{}", self.state_dir)); }
            if let Some(notice) = &self.notice { ui.strong(RichText::new(notice).color(theme::WARN)); }
        });
        // Node/dependency engineering state: full detail only in diagnostics;
        // the workflow view keeps a single honest status line so local mode
        // stays visible before startup (dependency text is an anchor for tests).
        if self.diagnostics {
        egui::Panel::bottom("dependencies").resizable(true).default_size(155.).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("dependency-scroll").max_height(220.).show(ui, |ui| {
                ui.strong("节点与依赖");
                for n in &snapshot.nodes {
                    ui.label(format!("{}  名额 {}/{}  准入={} 健康={} 排空={}  {}", n.id, n.used, n.slots, n.admitted, n.healthy, n.draining, n.address));
                    ui.small(format!("来源：{}；观测：{}", n.source, stamp(n.observed_at)));
                }
                for d in &snapshot.dependencies {
                    ui.label(format!("{}：{} · {}", d.id, d.state, d.detail));
                    ui.small(format!("来源：{}；观测：{}", d.source, stamp(d.observed_at)));
                }
                ui.small(format!("快照生成：{}；不是远端进度证明。关闭窗口不等于取消任务。", stamp(snapshot.observed_at)));
            });
        });
        } else {
            egui::Panel::bottom("dependency-status").default_size(26.).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.small("节点与依赖：");
                    for d in &snapshot.dependencies {
                        ui.small(format!("{}：{} · {}", d.id, d.state, d.detail));
                    }
                });
            });
        }
        if !self.diagnostics {
            if snapshot.mode == Mode::Real {
                egui::Panel::top("worker-onboarding").show(ui, |ui| {
                    self.render_onboarding(ui, &snapshot);
                });
            }
            let receipts: Vec<_> = self.receipts.iter().map(|r| (format!("#{} {} · {}", r.id, r.action, if r.failed { "失败" } else { "核心已返回" }), if self.diagnostics { r.diagnostic_detail.clone() } else { r.detail.clone() })).collect();
            for (key, command) in self.projects.show(ui, &snapshot, &self.pending, &receipts, self.diagnostics) { self.send(key, command); }
        } else if self.project_diagnostics {
            egui::Panel::top("advanced-kind").show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.selectable_label(!self.project_diagnostics, "任务诊断").clicked() { self.project_diagnostics = false; }
                    let _ = ui.selectable_label(true, "项目诊断");
                });
            });
            let receipts: Vec<_> = self.receipts.iter().map(|r| (format!("#{} {} · {}", r.id, r.action, if r.failed { "失败" } else { "核心已返回" }), r.diagnostic_detail.clone())).collect();
            for (key, command) in self.projects.show(ui, &snapshot, &self.pending, &receipts, true) { self.send(key, command); }
        } else {
            egui::Panel::top("advanced-kind").show(ui, |ui| {
                ui.horizontal(|ui| {
                    let _ = ui.selectable_label(true, "任务诊断");
                    if ui.selectable_label(false, "项目诊断").clicked() { self.project_diagnostics = true; }
                });
            });
            egui::Panel::left("tasks").resizable(true).default_size(335.).min_size(235.).show(ui, |ui| {
                ui.strong("任务");
                let templates = if snapshot.mode == Mode::Local { ["sum-native-v1", "control-native-v1"] } else { ["sum-v1", "control-v1"] };
                egui::ComboBox::from_id_salt("template").selected_text(&self.template).show_ui(ui, |ui| {
                    for template in templates { ui.selectable_value(&mut self.template, template.into(), template); }
                });
                ui.small(if self.template.starts_with("sum") { "计算 1 + 2 + 3，并独立核验产物。" } else { "请求一次审批，然后可取消受管进程。" });
                self.button(ui, "创建任务", snapshot.readiness == Readiness::Ready, format!("dispatch:{}", self.template), UiCommand::Dispatch { template: self.template.clone() });
                ui.separator();
                egui::ScrollArea::vertical().id_salt("run-scroll").show(ui, |ui| {
                    if snapshot.runs.is_empty() { ui.label("还没有任务。启动本地体验后创建一个任务。"); }
                    for view in &snapshot.runs {
                        let run = &view.run;
                        if ui.selectable_label(self.selected.as_deref() == Some(run.id.as_str()), format!("{}\n{}  第 {} 次\n{}", run.template, run.status, run.attempt, run.id)).clicked() { self.selected = Some(run.id.clone()); }
                        ui.separator();
                    }
                });
            });
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("detail-scroll").show(ui, |ui| {
                    if let Some(view) = snapshot.runs.iter().find(|r| self.selected.as_deref() == Some(r.run.id.as_str())) {
                        self.run_detail(ui, view);
                        for approval in snapshot.approvals.iter().filter(|p| p.run_id == view.run.id) {
                            ui.separator();
                            ui.strong(if approval.simulated { "待审批（模拟）" } else { "待审批" });
                            ui.label(&approval.summary);
                            ui.small(format!("请求 {}；第 {} 次；generation {}；过期 {}", approval.request_id, approval.attempt, approval.generation, stamp(approval.expires_at)));
                            ui.horizontal_wrapped(|ui| {
                                for offered in &approval.offered {
                                    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                                    let command = approval::decision_command(approval, &offered.id, now);
                                    let key = format!("decide:{}:{}:{}", approval.run_id, approval.request_id, approval.generation);
                                    if ui.add_enabled(view.can_decide && command.is_some() && !self.pending.contains(&key), egui::Button::new(&offered.label)).clicked() {
                                        if let Some(command) = command { self.send(key, command); }
                                    }
                                }
                            });
                        }
                    } else {
                        ui.heading("选择任务查看执行证据");
                        ui.label("任务状态、审批、结果来自真实本地核心和 SQLite。AO、Reason 与模型在本地体验中是模拟依赖。");
                    }
                    for e in &snapshot.errors { ui.separator(); ui.strong(format!("{}：{}", e.code, e.message)); ui.label(format!("下一步：{}", e.next_step)); }
                    ui.separator();
                    ui.strong("操作回执（最近 20 条）");
                    ui.small("接受请求或 state=queued 仅表示已记录意图，不表示远端执行或审批已完成。");
                    for receipt in &self.receipts {
                        egui::CollapsingHeader::new(format!("#{} {} · {}", receipt.id, receipt.action, if receipt.failed { "失败" } else { "核心已返回" })).default_open(true).id_salt(("receipt", receipt.id)).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(&receipt.diagnostic_detail).monospace()).wrap());
                        });
                    }
                });
            });
        }
        if !self.diagnostics && snapshot.mode == Mode::Real && self.onboarding_form_open && !self.confirm_close {
            self.render_connection_settings(&ctx, &snapshot);
        }
        if self.confirm_close {
            egui::Modal::new(egui::Id::unique("confirm-close")).show(&ctx, |ui| {
                ui.heading("仍有未完成工作");
                ui.label(format!("{} 个未完成任务，{} 个请求等待回执。", snapshot.unfinished, self.pending_count()));
                ui.label("关闭只停止本地工作台，不证明远端已取消。未确认的任务可能继续，重开后可能显示 UNKNOWN。");
                ui.horizontal(|ui| {
                    if ui.button("继续查看").clicked() { self.confirm_close = false; }
                    if ui.button("仍然关闭").clicked() { self.begin_close(); }
                });
            });
        }
    }
    fn render_onboarding(&mut self, ui: &mut egui::Ui, snapshot: &AppSnapshot) {
        assign_unique_gateway_worker(&mut self.gateway_worker, snapshot);
        let state = if self.onboarding_form_open {
            if let Some(error) = self.onboarding_input_error.as_deref() {
                OnboardingUiState::InputCorrection(error.into())
            } else {
                onboarding_ui_state(snapshot, &self.pending, None, self.onboarding_failure.as_ref())
            }
        } else {
            onboarding_ui_state(snapshot, &self.pending, None, self.onboarding_failure.as_ref())
        };
        let status_copy = if self.onboarding_form_open && self.onboarding_input_error.is_some() {
            "输入待修正"
        } else {
            provisioning_status_copy(snapshot, &self.pending, None, self.onboarding_failure.as_ref())
        };
        ui.horizontal_wrapped(|ui| {
            ui.strong(format!("Worker：{status_copy}"));
            if ui.button("连接设置").clicked() { self.reopen_onboarding_form(); }
            if matches!(state, OnboardingUiState::Connected) && ui.button("重新准备 Worker").clicked() { self.reopen_onboarding_form(); }
        });
        match &state {
            OnboardingUiState::NotStarted => {
                ui.small("尚未连接 Worker；打开“连接设置”填写信息，准备完成后才能提交项目。");
            }
            OnboardingUiState::InputCorrection(error) => {
                ui.small(error);
            }
            OnboardingUiState::Preparing => {
                ui.small("正在验证 root / 安装 Worker / 生成配置 / 校验连接");
            }
            OnboardingUiState::Failed { code, next_step } => {
                ui.small(format!("错误码：{code}"));
                ui.small(format!("下一步：{next_step}"));
            }
            OnboardingUiState::WaitingRemote => {
                ui.small("本地连接信息已准备好；等待远端产生可执行证据。");
            }
            OnboardingUiState::SessionExpired => {
                ui.small("当前会话凭据已清理；打开连接设置补充本次 root 密码并重新准备，不会删除旧远端残留。");
            }
            OnboardingUiState::Connected => {
                if let Some(gateway) = snapshot.gateway.as_ref() {
                    let artifact = gateway.artifact_sha256.get(..8).unwrap_or(&gateway.artifact_sha256);
                    ui.small(format!("{} · Worker {} · artifact {}", gateway.host, gateway.worker_id, artifact));
                }
                ui.small("下一步：新建项目或导入 design.md。");
            }
            OnboardingUiState::History => {
                if let Some(ledger) = snapshot.provision_ledger.as_ref() {
                    if let (Some(worker), Some(host)) = (&ledger.worker_id, &ledger.host) {
                        ui.small(format!("历史记录：Worker {} · {}", worker, host));
                    }
                }
                ui.small("这是本机保存的非秘密记录；请重新认证后才能建立当前连接。");
            }
            OnboardingUiState::Executing | OnboardingUiState::Submitting
            | OnboardingUiState::Unknown | OnboardingUiState::Occupied => {
                ui.small(WorkerSubmitGate::from_snapshot(snapshot, &self.pending).blocking_guidance());
            }
            OnboardingUiState::Admitted => {}
        }
    }

    fn dismiss_connection_settings(&mut self) {
        self.onboarding_form_open = false;
        self.onboarding_reprepare_requested = false;
        self.onboarding_root_password.clear();
        self.onboarding_model_token.clear();
    }

    fn render_connection_settings(&mut self, ctx: &egui::Context, snapshot: &AppSnapshot) {
        let response = egui::Modal::new(egui::Id::unique("connection-settings")).show(ctx, |ui| {
            let viewport = ctx.content_rect().size();
            ui.set_width((viewport.x - 64.).clamp(280., 640.));
            ui.heading("连接 Linux Worker");
            let body_height = (viewport.y - 190.).clamp(120., 640.);
            egui::ScrollArea::vertical().id_salt("connection-settings-body")
                .max_height(body_height).show(ui, |ui| {
            if let Some(notice) = &self.settings_notice { ui.label(notice); }
            if let Some(error) = &self.onboarding_input_error { ui.colored_label(theme::WARN, error); }
            if let Some((code, next)) = &self.onboarding_failure { ui.label(format!("准备失败：{code}；下一步：{next}")); }
            if snapshot.gateway.as_ref().is_some_and(|gateway| gateway.credentials_available) { ui.small("已连接的 Worker 不会被替换；如需更换，请关闭并重新打开工作台。"); }
            if snapshot.gateway.as_ref().is_some_and(|gateway| !gateway.credentials_available) { ui.small("会话已超时；本次重新准备会创建新的隔离输出，不会删除旧远端残留。"); }
            ui.small("只需填写六项信息；程序会自动准备并连接 Worker。");
            ui.small("支持 Ctrl+C/Ctrl+V：可从记事本复制内容后粘贴到任意输入框。");
            ui.horizontal(|ui| {
                ui.label("Linux 工作节点 IP");
                ui.text_edit_singleline(&mut self.onboarding_host);
                ui.small("例如 192.168.1.20");
            });
            ui.horizontal(|ui| {
                ui.label("root 账号");
                ui.text_edit_singleline(&mut self.onboarding_root_user);
                ui.small("通常为 root");
            });
            ui.horizontal(|ui| {
                ui.label("root 密码");
                Self::password_text_edit_with_copy(ui, &mut self.onboarding_root_password);
                ui.small("仅用于本次准备，不会写入回执或日志");
            });
            ui.horizontal(|ui| {
                ui.label("模型服务地址");
                if ui.text_edit_singleline(&mut self.onboarding_model_base_url).changed() && !self.token_origin.is_empty() {
                    self.onboarding_model_token.clear();
                    self.token_origin.clear();
                    self.settings_notice = Some("服务地址已改变，请重新输入 API key；旧保存配置尚未改变。".into());
                }
                ui.small("例如 https://boapi.uno:18317；必须是 HTTPS origin");
            });
            ui.horizontal(|ui| {
                ui.label("API key");
                Self::password_text_edit_with_copy(ui, &mut self.onboarding_model_token);
                ui.small("保存到 Windows 凭据管理器，不写入普通配置或日志");
            });
            ui.horizontal(|ui| {
                ui.label("默认模型");
                ui.text_edit_singleline(&mut self.onboarding_model);
                ui.small("例如 gpt-6.1-sol");
            });
            ui.small("Windows 只负责控制和显示；任务负载、模型调用和资源门禁在 Linux Worker 上执行。");
            });
            ui.separator();
            let ready = !self.pending.contains("worker-provision");
            if ui.add_enabled(ready, egui::Button::new("保存配置")).clicked() { self.save_connection_settings(); }
            if ui.add_enabled(ready, egui::Button::new(if self.pending.contains("worker-provision") { "正在准备 Worker（等待回执）" } else { "自动准备并连接 Worker" })).clicked() {
                let request = ProvisionRequest {
                    host: self.onboarding_host.trim().into(),
                    root_user: self.onboarding_root_user.trim().into(),
                    root_password: self.onboarding_root_password.as_bytes().to_vec(),
                    model_base_url: self.onboarding_model_base_url.trim().into(),
                    model_token: self.onboarding_model_token.as_bytes().to_vec(),
                    model: self.onboarding_model.trim().into(),
                };
                match validate_provision_input(&request) {
                    Ok(()) => {
                        if !self.save_connection_settings() { return; }
                        self.onboarding_input_error = None;
                        self.onboarding_failure = None;
                        self.onboarding_reprepare_requested = false;
                        self.send("worker-provision".into(), UiCommand::ProvisionWorker { request });
                        self.onboarding_root_password.clear();
                        self.onboarding_model_token.clear();
                    }
                    Err(error) => self.onboarding_input_error = Some(onboarding_input_error_copy(&error)),
                }
            }
            if ui.button("关闭设置").clicked() { self.dismiss_connection_settings(); }
        });
        if response.should_close() { self.dismiss_connection_settings(); }
    }
    fn begin_close(&mut self) {
        self.confirm_close = false;
        self.closing = true;
        self.service.disconnect();
    }
    fn run_detail(&mut self, ui: &mut egui::Ui, view: &RunView) {
        let r = &view.run;
        ui.heading(&r.template);
        ui.label(format!("状态：{}；第 {} 次；{}", r.status, r.attempt, if view.read_only { "只读" } else { "本地入口所属" }));
        ui.monospace(&r.id);
        if let Some(reason) = &view.disabled_reason { ui.label(reason); }
        ui.horizontal_wrapped(|ui| {
            self.button(ui, "查看状态", view.can_status, format!("status:{}", r.id), UiCommand::Status { run_id: r.id.clone() });
            self.button(ui, "刷新审批", view.can_approvals, format!("approvals:{}", r.id), UiCommand::Approvals { run_id: r.id.clone() });
            self.button(ui, "查看报告", view.can_report, format!("report:{}", r.id), UiCommand::Report { run_id: r.id.clone() });
            self.button(ui, "重试", view.can_retry, format!("retry:{}", r.id), UiCommand::Retry { run_id: r.id.clone(), attempt: r.attempt });
            self.button(ui, "取消任务", view.can_cancel, format!("cancel:{}", r.id), UiCommand::Cancel { run_id: r.id.clone() });
        });
        for (label, field) in [("创建", "created_at"), ("实际进度", "progress_at"), ("实际联系", "contact_at")] {
            ui.small(format!("{label}：{}", r.detail.get(field).and_then(|v| v.as_u64()).map(stamp).unwrap_or_else(|| "未观测".into())));
        }
        for field in ["contact_source", "progress_source", "reason", "failure", "artifact_dir"] {
            if let Some(value) = r.detail.get(field) { ui.label(format!("{field}：{value}")); }
        }
        ui.monospace(format!("epoch={} session={:?}",r.epoch,r.session));
        for (label, field) in [("进程 Launch 证据", "owner_launch"), ("结果报告", "report"), ("独立核验", "verification"), ("退出证据", "exit_proof")] {
            if let Some(value) = r.detail.get(field) {
                ui.separator(); ui.strong(label);
                ui.add(egui::Label::new(RichText::new(serde_json::to_string_pretty(value).unwrap_or_default()).monospace()).wrap());
            }
        }
    }
}
fn readiness(value: &Readiness) -> &'static str { match value { Readiness::NotStarted => "未启动", Readiness::Starting => "启动中", Readiness::Ready => "已就绪", Readiness::NotReady => "未就绪", Readiness::Stopped => "已停止" } }
fn stamp(value: u64) -> String { if value == 0 { "未观测".into() } else { format!("{value}（Unix 秒）") } }
