use std::{collections::BTreeMap, path::PathBuf, time::{Instant, SystemTime, UNIX_EPOCH}};

use agentranch::core::service::{AppSnapshot, LiveOutputEvent, ProjectSnapshot, RunView, ServiceError, UiCommand};
use super::project_activity::ProjectActivity;
use eframe::egui::{self, RichText};
use super::{action::{action, action_step}, pending::Pending, theme, worker_submit::WorkerSubmitGate};

// Step indexes: the current step's main control renders as the primary
// (brand-filled) button; the same control on non-current steps stays an
// ordinary button so its gate stays visible and testable.

/// Human-readable phase names. Internal phase strings are the persisted
/// state-machine contract and never change; this map is display-only.
fn phase_label(phase: &str) -> &str {
    match phase {
        "needs_document" => "① 新建项目——等待导入设计文档",
        "document_review" => "② 文档待确认——请核对内容后点“确认此上传版本”",
        "confirmed" => "③ 文档已确认——可以拆分任务",
        "splitting" => "④ 正在拆分任务……",
        "task_review" => "⑤ 任务待审核——逐项“审核通过”或填写原因“驳回”",
        "ready" => "⑥ 任务全部通过——可“手动开始开发”",
        "developing" => "⑦ 开发进行中……",
        "developed" => "⑧ 开发完成——可“运行独立测试”",
        "testing" => "⑨ 独立测试进行中……",
        "test_failed" => "⑩ 测试未通过——填写返修原因后“提交返修”",
        "repairing" => "⑪ 返修进行中……",
        "redoing" => "⑪ 重做进行中……",
        "awaiting_acceptance" => "⑫ 待人工验收——检查结果后点“人工验收通过”",
        "accepted" => "✓ 已验收——项目完成",
        _ => phase,
    }
}
fn action_kind_label(kind: &str) -> &str {
    match kind {
        "split" => "拆分任务", "redo" => "重做任务", "develop" => "开发",
        "test" => "独立测试", "repair" => "返修", "accept" => "人工验收",
        _ => kind,
    }
}
fn action_state_label(state: &str) -> &str {
    match state {
        "requested" => "已受理，排队中", "running" => "执行中",
        "report_pending_exit" => "结果已回传，等退出确认",
        "succeeded" => "成功", "failed" => "失败",
        "blocked" => "被阻塞（见说明）", "unknown" => "结果未知，需人工判断",
        _ => state,
    }
}

/// Wizard rail steps in fixed order. Markers are derived from the persisted
/// phase only — the rail is display-only and never gates.
const RAIL_STEPS: [&str; 6] = ["①新建", "②定稿设计", "③任务审核", "④开发", "⑤测试与返修", "⑥人工验收"];
/// Map each persisted phase to its rail index (0-based) and completion state.
/// Semantics: ② is current while the document is unconfirmed and done once
/// confirmed (split cannot happen otherwise), ③ is done when all tasks are
/// approved, ④ when development finishes (developed moves the current step on
/// to ⑤, matching rail_now's “运行独立测试”), ⑤ when tests pass without
/// repair, ⑥ on manual acceptance.
fn phase_rail(phase: &str) -> (usize, [bool; 6]) {
    let (current, done_count) = match phase {
        "needs_document" | "document_review" => (1, 1),
        "confirmed" | "splitting" | "task_review" => (2, 2),
        "ready" | "developing" => (3, 3),
        "developed" => (4, 4),
        // Redo reopens task review (fires from task_review|ready): step 3 is
        // current again, not done. Repair loops inside step 5 after a failed test.
        "redoing" => (2, 2),
        "testing" | "test_failed" | "repairing" => (4, 4),
        "awaiting_acceptance" => (5, 5),
        "accepted" => (5, 6),
        _ => (1, 1),
    };
    let mut done = [false; 6];
    for slot in done.iter_mut().take(done_count) { *slot = true; }
    (current, done)
}
/// One-line guidance naming the physical control for the current step.
fn rail_now(phase: &str) -> &'static str {
    match phase {
        "needs_document" => "现在做：在“定稿设计文档”区选择并导入 .md/.txt，再点“导入设计文档”",
        "document_review" => "现在做：核对预览与 SHA-256 后点“确认此上传版本”",
        "confirmed" | "splitting" => "现在做：点“按已确认上传版拆分任务”",
        "task_review" => "现在做：逐项“审核通过”，或填原因“驳回此项”",
        "ready" => "现在做：全部通过后点“手动开始开发”",
        "developing" => "现在做：等待开发执行记录回传（不自动进入下一步）",
        "developed" => "现在做：点“运行独立测试”",
        "testing" => "现在做：等待独立测试结果",
        "test_failed" => "现在做：填“返修原因”后点“提交返修”，返修后复测",
        "repairing" | "redoing" => "现在做：等待返修/重做执行记录",
        "awaiting_acceptance" => "现在做：检查结果后点“人工验收通过”",
        "accepted" => "项目已完成验收；如需变更请新建项目",
        _ => "现在做：查看下方阶段说明",
    }
}
/// Plain-language one-line explanation of what each wizard step means, shown on
/// the step cards. Distinct from rail_now (which names the physical control).
fn step_intro(step: usize) -> &'static str {
    match step {
        0 => "给项目起个名字并创建，作为这次工作的容器。",
        1 => "上传你已经定稿的设计文档（.md/.txt），核对内容后确认这一版本。",
        2 => "系统把设计拆成一项项任务；你逐条检查，通过或写明原因驳回。",
        3 => "确认无误后手动开始，系统在 Linux 上真实执行开发。",
        4 => "开发结果由系统独立测试；未通过可写明原因让系统返修后复测。",
        _ => "你亲自检查最终结果，满意后点击通过，项目完成。",
    }
}
/// Short rail step name without the leading marker glyph.
fn rail_step_name(step: usize) -> &'static str {
    ["新建项目", "定稿设计", "任务审核", "开发", "测试与返修", "人工验收"][step]
}

/// List-only projection: a published result wins over session-local activity.
fn project_list_status(work_run: Option<&RunView>, activity: Option<&ProjectActivity>) -> Option<String> {
    match work_run.map(|view| view.run.status.as_str()) {
        Some("done") => Some("任务：已完成".into()),
        Some("failed") => Some("任务：失败".into()),
        Some("cancelled") => Some("任务：已取消".into()),
        Some("unknown") => Some("任务：结果未知，名额保留".into()),
        _ => activity.map(|output| format!("执行：{}", output.status_label()))
            .or_else(|| work_run.map(|_| "任务：执行中".into())),
    }
}

/// Editable input and selection only. All document/review/execution facts come from core.
pub struct Projects {
    pub selected: Option<String>,
    name: String,
    path: String,
    config_path: String,
    review_reasons: BTreeMap<String, String>,
    repair_reason: String,
    receipts_view: Vec<(String, String)>,
    output: Option<ProjectActivity>,
    reveal_output: bool,
    #[cfg(windows)]
    picker: Option<std::sync::mpsc::Receiver<(String, Option<PathBuf>)>>,
}
impl Default for Projects {
    fn default() -> Self {
        Self { selected: None, name: "新项目".into(), path: String::new(), config_path: String::new(), review_reasons: BTreeMap::new(), repair_reason: String::new(), receipts_view: Vec::new(), output: None, reveal_output: false, #[cfg(windows)] picker: None }
    }
}

impl Projects {
    pub fn begin_live(&mut self, request_id: u64, project_id: String) {
        self.selected = Some(project_id.clone());
        self.reveal_output = true;
        self.output = Some(ProjectActivity::new(request_id, project_id, Instant::now(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()));
    }
    pub fn apply_live(&mut self, event: LiveOutputEvent) {
        if let Some(output) = &mut self.output { output.apply(event); }
    }
    pub fn finish_live(&mut self, request_id: u64, result: &Result<serde_json::Value, ServiceError>) {
        if let Some(output) = &mut self.output { output.finish(request_id, result, Instant::now()); }
    }
    fn render_live(&mut self, ui: &mut egui::Ui, project_id: &str, work_run: Option<&RunView>) {
        if let Some(output) = self.output.as_mut().filter(|output| output.project_id == project_id) {
            if let Some(view) = work_run {
                output.render_with_published_status(ui, Instant::now(), Some(view.run.status.as_str()));
            } else {
                output.render(ui, Instant::now());
            }
        } else {
            Self::render_output_empty(ui, Some(work_run.is_some()));
        }
    }
    fn render_output_empty(ui: &mut egui::Ui, has_work_run: Option<bool>) {
        theme::card(ui, |ui| {
            ui.strong(RichText::new("模型实时输出（SSE）").color(theme::BRAND).size(19.));
            match has_work_run {
                None => { ui.label("新建项目并确认设计文档后，点击“提交项目”；模型正文会在这里逐段显示。"); }
                Some(false) => { ui.label("尚未提交项目。确认设计文档后点击“提交项目”，模型正文会在这里逐段显示。"); }
                Some(true) => {
                    ui.label("本次会话没有该任务的实时正文；不会回放历史输出。");
                    ui.label("请查看下方“结果核对”和“操作回执”；不要重复提交当前任务。");
                }
            }
            ui.small("仅显示当前会话收到的实际正文，不伪造模型思考；关闭工作台后不会恢复该正文。");
        });
    }
    fn render_result_summary(ui: &mut egui::Ui, work_run: Option<&RunView>) {
        ui.strong("结果核对");
        let Some(run) = work_run else {
            ui.label("最终任务结果：尚未返回");
            ui.label("准入状态：尚未形成终态");
            ui.label("产物目录：尚未物化");
            ui.label("回执提示：等待最终任务回执");
            return;
        };
        let status = run.run.status.as_str();
        let artifact_dir = run.run.detail["artifact_dir"].as_str().filter(|path| !path.is_empty());
        match status {
            "done" => {
                ui.label("最终任务结果：完成");
                let admitted = run.run.detail["owner_admission"] == "admitted"
                    && run.run.detail["exit_proof"].is_object()
                    && artifact_dir.is_some();
                if admitted {
                    ui.label("准入状态：已准入");
                    ui.label(format!("产物目录：{}", artifact_dir.expect("checked above")));
                    ui.label("回执提示：核对产物目录和操作回执");
                } else {
                    ui.label("准入状态：未准入");
                    ui.label(format!("产物目录：{}", artifact_dir.unwrap_or("尚未物化")));
                    ui.label("回执提示：已返回完成状态但证据不完整，请查看操作回执");
                }
            }
            "failed" => {
                ui.label("最终任务结果：失败");
                ui.label("准入状态：未准入");
                ui.label(format!("产物目录：{}", artifact_dir.unwrap_or("尚未物化")));
                ui.label("回执提示：查看操作回执；不会自动重跑");
            }
            "cancelled" => {
                ui.label("最终任务结果：已取消");
                ui.label("准入状态：未准入");
                ui.label(format!("产物目录：{}", artifact_dir.unwrap_or("尚未物化")));
                ui.label("回执提示：以最终取消回执为准");
            }
            "unknown" => {
                ui.label("最终任务结果：结果未知");
                ui.label("准入状态：未准入");
                ui.label("产物目录：尚未物化");
                ui.label("回执提示：名额保留；不要手动释放或自动重跑");
            }
            _ => {
                ui.label("最终任务结果：尚未返回");
                ui.label("准入状态：尚未形成终态");
                ui.label("产物目录：尚未物化");
                ui.label("回执提示：等待最终任务回执");
            }
        }
    }
    pub fn show(&mut self, ui: &mut egui::Ui, snapshot: &AppSnapshot, pending: &Pending, receipts: &[(String, String)], diagnostics: bool) -> Vec<(String, UiCommand)> {
        let mut commands = Vec::new();
        self.receipts_view = receipts.to_vec();
        #[cfg(windows)]
        if let Some(receiver) = &self.picker {
            match receiver.try_recv() {
                Ok((project_id, path)) => {
                    if self.selected.as_deref() == Some(project_id.as_str()) { if let Some(path) = path { self.path = path.display().to_string(); } }
                    self.picker = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.picker = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.selected.as_ref().is_none_or(|id| !snapshot.projects.iter().any(|p| &p.id == id)) {
            self.selected = snapshot.projects.first().map(|p| p.id.clone());
        }
        // File drops are path-only input. Route them to the selected project's
        // editable import field before layout/culling decides which step body
        // is painted; never inspect bytes or dispatch an import implicitly.
        if let Some(path) = ui.ctx().input(|input| input.raw.dropped_files.first().map(|file| file.path().to_path_buf())) {
            if self.selected.is_some() { self.path = path.display().to_string(); }
        }
        egui::Panel::left("project-list").resizable(true).default_size(255.).min_size(200.).show(ui, |ui| {
            ui.heading("项目");
            ui.label("项目名称");
            ui.text_edit_singleline(&mut self.name);
            action(ui, pending, &mut commands, "新建项目", !self.name.trim().is_empty(), "project-create", "project-create".into(), UiCommand::ProjectCreate { name: self.name.trim().into() });
            ui.separator();
            egui::ScrollArea::vertical().id_salt("project-list-scroll").show(ui, |ui| {
                for project in &snapshot.projects {
                    let work_run = snapshot.runs.iter().find(|view| {
                        view.run.template == "project-work-v1"
                            && view.run.detail["project_work"]["project_id"] == project.id
                    });
                    let activity = self.output.as_ref().filter(|output| output.project_id == project.id);
                    let status = project_list_status(work_run, activity)
                        .map(|status| format!("\n{status}")).unwrap_or_default();
                    let (phase, done_count, total) = if diagnostics {
                        let (_, done) = phase_rail(&project.phase);
                        (phase_label(&project.phase), done.iter().filter(|d| **d).count(), 6)
                    } else {
                        let confirmed = project.document.as_ref().is_some_and(|document| document.confirmed);
                        let phase = match project.document.as_ref() {
                            None => "等待导入设计文档",
                            Some(document) if !document.confirmed => "文档待确认",
                            Some(_) => "文档已确认",
                        };
                        let done_count = 1 + usize::from(confirmed)
                            + usize::from(work_run.is_some_and(|view| view.run.status == "done"));
                        (phase, done_count, 3)
                    };
                    if ui.selectable_label(self.selected.as_deref() == Some(&project.id), format!("{}\n{phase}\n已完成 {done_count}/{total} 步{status}", project.name)).clicked() {
                        self.selected = Some(project.id.clone());
                        self.path.clear();
                        self.repair_reason.clear();
                    }
                }
                if snapshot.projects.is_empty() { ui.label("先新建项目，再导入已定稿设计。"); }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            let mut detail_scroll = egui::ScrollArea::vertical().id_salt("project-detail-scroll");
            if std::mem::take(&mut self.reveal_output) {
                detail_scroll = detail_scroll.vertical_scroll_offset(0.);
            }
            detail_scroll.show(ui, |ui| {
                if let Some(project) = snapshot.projects.iter().find(|p| self.selected.as_deref() == Some(&p.id)) {
                    self.detail(ui, project, snapshot, pending, &mut commands, diagnostics);
                } else {
                    self.welcome(ui);
                    self.render_receipts(ui);
                }
                for error in &snapshot.errors {
                    ui.separator();
                    ui.strong(format!("{}：{}", error.code, error.message));
                    ui.label(format!("下一步：{}", error.next_step));
                }
            });
        });
        // Receipts stay in the project page as a collapsed engineering log so
        // the step column keeps only actionable content.
        commands
    }
    /// Empty state: the novice's first screen names the one thing to do now.
    /// The create form itself lives only in the left rail (single widget id);
    /// this card duplicates no control, it orients and points there.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("欢迎使用 agentranch").color(theme::BRAND).size(24.));
        ui.label("从定稿设计文档开始：把已经写好的设计交给系统，它负责拆分、真实执行开发与测试，你负责审核和最终验收。");
        ui.add_space(6.);
        theme::card(ui, |ui| {
            ui.set_min_width(ui.available_width() * 0.95);
            ui.strong(RichText::new("第 1 步 · 新建项目").color(theme::BRAND).size(19.));
            ui.label(step_intro(0));
            ui.label(RichText::new("现在做：在左侧“项目名称”填入名字，点“新建项目”").color(theme::BRAND));
        });
        Self::render_output_empty(ui, None);
    }
    fn render_receipts(&self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("操作回执")
            .default_open(true)
            .id_salt("project-receipts")
            .show(ui, |ui| {
                ui.small("核心接受请求不等于外部执行完成；以项目阶段、公开输出和独立测试结果为准。");
                for (title, detail) in &self.receipts_view {
                    egui::CollapsingHeader::new(title).default_open(true).show(ui, |ui| { ui.add(egui::Label::new(RichText::new(detail).monospace()).wrap()); });
                }
            });
    }

    fn detail_novice(&mut self, ui: &mut egui::Ui, project: &ProjectSnapshot, snapshot: &AppSnapshot, pending: &Pending, commands: &mut Vec<(String, UiCommand)>) {
        let id = &project.id;
        ui.heading(&project.name);
        ui.label(RichText::new("普通项目路径：确认设计文档后，直接提交到当前 Worker。").color(theme::BRAND));
        let document_ready = project.document.as_ref().is_some_and(|document| document.confirmed);
        let work_run = snapshot.runs.iter().find(|run| {
            run.run.template == "project-work-v1"
                && run.run.detail["project_work"]["project_id"] == project.id
        });
        let active_work = work_run.is_some_and(|run| !matches!(run.run.status.as_str(), "done" | "failed" | "cancelled"));
        let worker_gate = WorkerSubmitGate::from_snapshot(snapshot, pending);
        let any_live_task = self.output.as_ref().is_some_and(|output| {
            output.task_status.as_deref().is_none_or(|status| status == "unknown")
        });
        let can_submit = worker_gate == WorkerSubmitGate::Ready
            && document_ready
            && work_run.is_none()
            && !any_live_task;

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.strong("进度");
            ui.horizontal_wrapped(|ui| {
                let steps = [("①", "新建项目", true), ("②", "定稿设计", document_ready), ("③", "提交项目", work_run.is_some_and(|run| run.run.status == "done"))];
                for (index, (marker, label, done)) in steps.iter().enumerate() {
                    if index > 0 { ui.label(RichText::new("→").weak()); }
                    let shown = if *done { "✓" } else if (index == 1 && !document_ready) || (index == 2 && document_ready && work_run.is_none()) { "▶" } else { marker };
                    ui.label(RichText::new(format!("{shown} {label}")));
                }
            });
            ui.label(RichText::new(if !document_ready {
                "现在做：导入设计文档，核对内容后确认上传版本"
            } else if any_live_task || active_work {
                "现在做：已有项目任务执行中或结果未知；查看项目列表的执行状态，不会自动重跑"
            } else if work_run.is_some() {
                "现在做：查看结果核对和操作回执；如需再次执行，请新建项目"
            } else if worker_gate != WorkerSubmitGate::Ready {
                worker_gate.blocking_guidance()
            } else {
                "现在做：确认 Worker 状态后点“提交项目”"
            }).color(theme::BRAND));
        });

        self.render_live(ui, id, work_run);

        theme::card(ui, |ui| {
            ui.set_min_width(ui.available_width() * 0.95);
            self.step_body(ui, 1, project, pending, commands);
        });

        theme::card(ui, |ui| {
            ui.set_min_width(ui.available_width() * 0.95);
            ui.strong(RichText::new("项目提交").color(theme::BRAND).size(19.));
            Self::render_result_summary(ui, work_run);
            if work_run.is_none() && can_submit {
                ui.label("Worker 已连接，可提交项目。使用已确认的上传版本提交到当前 Worker。");
            } else if active_work || any_live_task {
                ui.label("已有项目任务执行中或结果未知；请查看执行活动，当前不能再次提交。");
            } else if work_run.is_some() {
                ui.label("本项目已有任务结果；如需再次执行，请新建项目。");
            } else if worker_gate != WorkerSubmitGate::Ready {
                ui.label(worker_gate.blocking_guidance());
            } else {
                ui.label("完成 Worker 准备并确认上传版本后，才能提交项目。");
            }
            action_step(ui, pending, commands, "提交项目", can_submit, true, &format!("project:{id}:work-submit"), format!("work-submit:{id}"), UiCommand::ProjectWorkSubmit { project_id: id.clone() });
            ui.small("提交使用系统保存的确认文档；不会要求 Worker、Gateway 或原文件路径。");
        });
        self.render_receipts(ui);
    }

    /// The six wizard step cards: completed steps collapse into one-line
    /// summaries, the current step expands into the primary card with its main
    /// button highlighted, later steps stay visible but dimmed with their
    /// (gated) controls still registered every frame. This is retained for
    /// 高级 · 本地诊断 only.
    fn detail(&mut self, ui: &mut egui::Ui, project: &ProjectSnapshot, snapshot: &AppSnapshot, pending: &Pending, commands: &mut Vec<(String, UiCommand)>, diagnostics: bool) {
        if !diagnostics {
            self.detail_novice(ui, project, snapshot, pending, commands);
            return;
        }
        let id = &project.id;
        ui.heading(&project.name);
        ui.label(RichText::new(format!("当前阶段：{}", phase_label(&project.phase))).color(theme::BRAND));
        let (current, done) = phase_rail(&project.phase);
        // Compact progress rail for glanceability; markers derive from the
        // persisted phase only and never gate anything.
        ui.add_space(2.);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.strong("进度");
            ui.horizontal_wrapped(|ui| {
                for (index, step) in RAIL_STEPS.iter().enumerate() {
                    if index > 0 { ui.label(RichText::new("→").weak()); }
                    let marker = if done[index] { "✓" } else if index == current { "▶" } else { "○" };
                    let color = if done[index] { theme::OK_GREEN } else if index == current { theme::BRAND } else { ui.visuals().weak_text_color() };
                    ui.label(RichText::new(format!("{marker} {step}")).color(color));
                }
            });
            ui.label(RichText::new(rail_now(&project.phase)).color(theme::BRAND));
        });
        // Blockers sit directly under the rail: they explain why the current
        // step cannot proceed right now, before any step detail.
        for reason in &project.blockers {
            ui.add_space(3.);
            egui::Frame::group(ui.style())
                .fill(egui::Color32::from_rgb(253, 246, 233))
                .stroke(egui::Stroke::new(1., theme::WARN))
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width() * 0.9);
                    ui.strong(RichText::new("⚠ 需要处理").color(theme::WARN));
                    ui.add(egui::Label::new(reason).wrap());
                });
        }
        // Advanced disclosures sit directly under the rail (above the step
        // column): engineering detail the novice never needs, always reachable
        // without scrolling regardless of the current step's card height.
        if diagnostics {
            egui::CollapsingHeader::new("项目与产物目录")
                .id_salt(("project-directories", id))
                .show(ui, |ui| {
                    ui.add(egui::Label::new(format!("项目目录：{}", if project.project_path.is_empty() { "尚未准备目录" } else { &project.project_path })).wrap());
                    ui.add(egui::Label::new(format!("产物目录：{}", if project.product_path.is_empty() { "尚未准备目录" } else { &project.product_path })).wrap());
                    ui.small("目录存在不代表已生成产物或开发完成。");
                    ui.small("刷新项目会由后台准备或校验受管目录；旧项目首次刷新可能创建目录，不启动开发。");
                    action(ui, pending, commands, "刷新项目", true, &format!("project:{id}:refresh"), format!("refresh:{id}"), UiCommand::ProjectRefresh { project_id: id.clone() });
                });
            egui::CollapsingHeader::new("执行依赖配置")
                .id_salt("project-config")
                .show(ui, |ui| {
                    ui.label("配置文件路径（由后台读取，不自动启动任务）");
                    ui.text_edit_singleline(&mut self.config_path);
                    action(ui, pending, commands, "加载执行配置", !self.config_path.trim().is_empty(), "project-configure", "project-configure".into(), UiCommand::ProjectConfigure { config_path: PathBuf::from(self.config_path.trim()) });
                });
        }
        if !diagnostics {
            let work_run = snapshot.runs.iter().find(|run| {
                run.run.template == "project-work-v1"
                    && run.run.detail["project_work"]["project_id"] == project.id
            });
            let active_work = work_run.is_some_and(|run| !matches!(run.run.status.as_str(), "done" | "failed" | "cancelled"));
            let can_submit = snapshot.gateway.as_ref().is_some_and(|gateway| gateway.preflight_passed)
                && project.document.as_ref().is_some_and(|document| document.confirmed)
                && !active_work;
            ui.add_space(6.);
            theme::card(ui, |ui| {
                ui.strong(RichText::new("项目提交").color(theme::BRAND).size(19.));
                if let Some(run) = work_run {
                    match run.run.status.as_str() {
                        "done"
                            if run.run.detail["owner_admission"] == "admitted"
                                && run.run.detail["exit_proof"].is_object()
                                && run.run.detail["artifact_dir"].as_str().is_some_and(|path| !path.is_empty()) => {
                            ui.label("已收到并准入远端结果。");
                            if let Some(path) = run.run.detail["artifact_dir"].as_str() {
                                ui.label("已物化项目产物，可在本机目录查看。");
                                ui.small(format!("产物目录：{path}"));
                            }
                        }
                        "unknown" => { ui.label("远端任务结果未知；尚无可准入终态，系统保留名额且不会自动重跑。"); }
                        _ => { ui.label("远端任务执行中；尚未收到可准入的终态证据。"); }
                    }
                } else if can_submit {
                    ui.label("Worker 已连接，可提交项目。使用已确认的上传版本提交到当前 Worker。");
                } else {
                    ui.label("完成 Worker 准备并确认上传版本后，才能提交项目。");
                }
                action_step(ui, pending, commands, "提交项目", can_submit, true, &format!("project:{id}:work-submit"), format!("work-submit:{id}"), UiCommand::ProjectWorkSubmit { project_id: id.clone() });
                ui.small("提交使用系统保存的确认文档；不会要求 Worker、Gateway 或原文件路径。");
            });
        }
        self.render_receipts(ui);
        // Step cards.
        ui.add_space(4.);
        for step in 0..6 {
            if step != current && done[step] {
                // Completed step: one-line summary, expandable for its record.
                ui.add_space(3.);
                egui::CollapsingHeader::new(format!("✓ {} · 已完成", rail_step_name(step)))
                    .id_salt(("step-done", id, step))
                    .default_open(false)
                    .show(ui, |ui| self.step_body(ui, step, project, pending, commands));
            } else if step == current {
                // The primary card: what the user should act on right now.
                ui.add_space(6.);
                theme::card(ui, |ui| {
                    ui.set_min_width(ui.available_width() * 0.95);
                    ui.strong(RichText::new(format!("第 {} 步 · {}", step + 1, rail_step_name(step))).color(theme::BRAND).size(19.));
                    ui.label(step_intro(step));
                    ui.add_space(2.);
                    self.step_body(ui, step, project, pending, commands);
                    ui.add_space(2.);
                    ui.label(RichText::new(rail_now(&project.phase)).color(theme::BRAND));
                });
            } else {
                // Later step: compact dimmed row — intro plus the gated controls
                // (still registered every frame so disabled gates stay observable).
                ui.add_space(3.);
                theme::card(ui, |ui| {
                    ui.set_min_width(ui.available_width() * 0.95);
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(RichText::new(format!("○ 第 {} 步 · {}", step + 1, rail_step_name(step))).weak().size(16.));
                        ui.label(RichText::new(step_intro(step)).weak().small());
                    });
                    self.step_controls(ui, step, project, pending, commands);
                });
            }
        }
    }
    /// Controls for one wizard step. Every gate comes from the core snapshot;
    /// this layer only lays out and registers the (possibly disabled) buttons.
    fn step_body(&mut self, ui: &mut egui::Ui, step: usize, project: &ProjectSnapshot, pending: &Pending, commands: &mut Vec<(String, UiCommand)>) {
        let id = &project.id;
        let primary = step == phase_rail(&project.phase).0;
        match step {
            0 => {} // Creation lives in the left rail and the welcome card.
            1 => {
                // Document import + confirmation of the uploaded version.
                ui.strong("定稿设计文档");
                ui.label("UTF-8 .md / .txt，原文件不超过 256 KiB。输入文件路径，或把文件拖到此窗口。");
                ui.text_edit_singleline(&mut self.path);
                #[cfg(windows)]
                if ui.add_enabled(self.picker.is_none(), egui::Button::new("选择设计文件…")).clicked() {
                    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
                    self.picker = Some(receiver);
                    let project_id = id.clone();
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || { let path = pick_document(); let _ = sender.send((project_id, path)); ctx.request_repaint(); });
                }
                action_step(ui, pending, commands, "导入设计文档", !self.path.trim().is_empty(), primary, &format!("project:{id}:import"), format!("import:{id}"), UiCommand::ProjectImport { project_id: id.clone(), path: PathBuf::from(self.path.trim()) });
                if let Some(document) = &project.document {
                    ui.label(format!("上传版本 U{} · {} · {}", document.version, document.filename, if document.confirmed { "已确认" } else { "待确认" }));
                    ui.label(format!("SHA-256：{}", document.sha256));
                    egui::ScrollArea::vertical().id_salt(("document-preview", id, document.version)).max_height(200.).show(ui, |ui| {
                        ui.add(egui::Label::new(&document.text).wrap());
                    });
                    action_step(ui, pending, commands, "确认此上传版本", project.can_confirm, primary, &format!("project:{id}:confirm:{}", document.version), format!("confirm:{id}:{}", document.version), UiCommand::ProjectConfirm { project_id: id.clone(), version: document.version, sha256: document.sha256.clone() });
                } else { ui.label("尚未导入文档，请先导入并确认上传版本。"); }
            }
            2 => {
                ui.strong("逐项审核任务");
                action_step(ui, pending, commands, "按已确认上传版拆分任务", project.can_split, primary, &format!("project:{id}:split"), format!("split:{id}"), UiCommand::ProjectSplit { project_id: id.clone() });
                if project.tasks.is_empty() { ui.label("暂无拆分任务。未执行拆分，或执行依赖尚未就绪；不会用固定样例代替。"); }
                for task in &project.tasks {
                    ui.add_space(2.);
                    theme::card(ui, |ui| {
                        ui.strong(format!("{} · {}", task.title, task.review));
                        ui.label(&task.description);
                        ui.label(format!("验收标准：{}", task.acceptance));
                        if let Some(reason) = &task.reason { ui.label(format!("已提交原因：{reason}")); }
                        let reason = self.review_reasons.entry(format!("{id}:{}", task.id)).or_default();
                        ui.label("驳回原因");
                        ui.text_edit_multiline(reason);
                        ui.horizontal_wrapped(|ui| {
                            action(ui, pending, commands, "审核通过", task.review != "approved", &format!("project:{id}:task:{}:approve", task.id), format!("review:{id}:{}", task.id), UiCommand::TaskReview { project_id: id.clone(), task_id: task.id.clone(), approve: true, reason: String::new() });
                            action(ui, pending, commands, "驳回此项", !reason.trim().is_empty(), &format!("project:{id}:task:{}:reject", task.id), format!("review:{id}:{}", task.id), UiCommand::TaskReview { project_id: id.clone(), task_id: task.id.clone(), approve: false, reason: reason.trim().into() });
                            action(ui, pending, commands, "仅重做此项", task.review == "rejected", &format!("project:{id}:task:{}:redo", task.id), format!("redo:{id}:{}", task.id), UiCommand::TaskRedo { project_id: id.clone(), task_id: task.id.clone() });
                        });
                    });
                }
                ui.label("全部任务审核通过后，只解锁手动开始；不会自动进入开发。");
                Self::action_list(ui, project);
            }
            3 => {
                ui.strong("开发");
                action_step(ui, pending, commands, "手动开始开发", project.can_start, primary, &format!("project:{id}:start"), format!("start-project:{id}"), UiCommand::ProjectStart { project_id: id.clone() });
                ui.label("等待执行记录回传（不自动进入下一步）。");
                Self::action_list(ui, project);
            }
            4 => {
                ui.strong("独立测试与返修");
                action_step(ui, pending, commands, "运行独立测试", project.can_test, primary, &format!("project:{id}:test"), format!("test:{id}"), UiCommand::ProjectTest { project_id: id.clone() });
                ui.label("返修原因（测试未通过时必须显式发起返修，之后再复测）");
                ui.text_edit_multiline(&mut self.repair_reason);
                action_step(ui, pending, commands, "提交返修", project.can_repair && !self.repair_reason.trim().is_empty(), primary, &format!("project:{id}:repair"), format!("repair:{id}"), UiCommand::ProjectRepair { project_id: id.clone(), reason: self.repair_reason.trim().into() });
                Self::action_list(ui, project);
            }
            _ => {
                ui.strong("人工验收");
                action_step(ui, pending, commands, "人工验收通过", project.can_accept, primary, &format!("project:{id}:accept"), format!("accept:{id}"), UiCommand::ProjectAccept { project_id: id.clone() });
                Self::action_list(ui, project);
            }
        }
    }
    /// Compact controls-only row for later (not-yet-current) steps: registers
    /// the same gated buttons so their disabled state stays observable without
    /// the full step body's height.
    fn step_controls(&mut self, ui: &mut egui::Ui, step: usize, project: &ProjectSnapshot, pending: &Pending, commands: &mut Vec<(String, UiCommand)>) {
        let id = &project.id;
        let primary = false; // never the primary control outside the current step
        ui.horizontal_wrapped(|ui| {
            match step {
                1 => {
                    action_step(ui, pending, commands, "导入设计文档", !self.path.trim().is_empty(), primary, &format!("project:{id}:import"), format!("import:{id}"), UiCommand::ProjectImport { project_id: id.clone(), path: PathBuf::from(self.path.trim()) });
                    if let Some(document) = &project.document {
                        action_step(ui, pending, commands, "确认此上传版本", project.can_confirm, primary, &format!("project:{id}:confirm:{}", document.version), format!("confirm:{id}:{}", document.version), UiCommand::ProjectConfirm { project_id: id.clone(), version: document.version, sha256: document.sha256.clone() });
                    }
                }
                2 => {
                    action_step(ui, pending, commands, "按已确认上传版拆分任务", project.can_split, primary, &format!("project:{id}:split"), format!("split:{id}"), UiCommand::ProjectSplit { project_id: id.clone() });
                }
                3 => {
                    action_step(ui, pending, commands, "手动开始开发", project.can_start, primary, &format!("project:{id}:start"), format!("start-project:{id}"), UiCommand::ProjectStart { project_id: id.clone() });
                }
                4 => {
                    action_step(ui, pending, commands, "运行独立测试", project.can_test, primary, &format!("project:{id}:test"), format!("test:{id}"), UiCommand::ProjectTest { project_id: id.clone() });
                }
                _ => {
                    action_step(ui, pending, commands, "人工验收通过", project.can_accept, primary, &format!("project:{id}:accept"), format!("accept:{id}"), UiCommand::ProjectAccept { project_id: id.clone() });
                }
            }
        });
    }
    fn action_list(ui: &mut egui::Ui, project: &ProjectSnapshot) {
        if project.actions.is_empty() { ui.label("还没有执行记录。未执行不等于完成。"); }
        for a in &project.actions {
            ui.add_space(2.);
            egui::Frame::group(ui.style()).inner_margin(8).show(ui, |ui| {
                ui.strong(format!("{} · {}", action_kind_label(&a.kind), action_state_label(&a.state)));
                if let Some(session) = &a.session { ui.small(format!("会话：{session}")); }
                ui.add(egui::Label::new(&a.detail).wrap());
            });
        }
    }
}
// Receipts are rendered from Workbench's passed-in view; see show().
#[cfg(windows)]
fn pick_document() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OPENFILENAMEW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OFN_NOCHANGEDIR, OFN_DONTADDTORECENT};
    let mut file = vec![0u16; 32768];
    let filter: Vec<u16> = "定稿设计文档 (*.md;*.txt)\0*.md;*.txt\0\0".encode_utf16().collect();
    let title: Vec<u16> = "选择已定稿设计文档（选择后仍需点击导入）\0".encode_utf16().collect();
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFilter: filter.as_ptr(),
        lpstrFile: file.as_mut_ptr(),
        nMaxFile: file.len() as u32,
        lpstrTitle: title.as_ptr(),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT,
        ..Default::default()
    };
    // Buffers outlive the synchronous native call, made only on this background thread.
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 { return None; }
    let length = file.iter().position(|v| *v == 0)?;
    Some(std::ffi::OsString::from_wide(&file[..length]).into())
}
