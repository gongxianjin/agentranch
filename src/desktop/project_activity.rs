//! One session-local display, never a lifecycle authority or a persisted history.
use std::time::Instant;
use agentranch::core::{live_output::LiveOutput, service::{LiveOutputEvent, ServiceError}};
use eframe::egui::{self, RichText};
use super::theme;

pub struct ProjectActivity {
    pub request_id: u64,
    pub project_id: String,
    text: String,
    connected: bool,
    model_finished: bool,
    pub task_status: Option<String>,
    error: Option<String>,
    submitted: Instant,
    submitted_unix_secs: u64,
    returned: Option<Instant>,
    last_output: Option<Instant>,
    received_bytes: usize,
    truncated: bool,
    following: bool,
    seen_bytes: usize,
}
impl ProjectActivity {
    pub fn new(request_id: u64, project_id: String, now: Instant, submitted_unix_secs: u64) -> Self {
        Self { request_id, project_id, text: String::new(), connected: false, model_finished: false,
            task_status: None, error: None, submitted: now, submitted_unix_secs, returned: None,
            last_output: None, received_bytes: 0, truncated: false, following: true, seen_bytes: 0 }
    }
    pub fn apply(&mut self, event: LiveOutputEvent) {
        if self.request_id != event.request_id || self.project_id != event.project_id || self.returned.is_some() { return; }
        let new_body = event.received_bytes > self.received_bytes;
        self.received_bytes = self.received_bytes.max(event.received_bytes);
        self.truncated |= event.truncated;
        match event.output {
            LiveOutput::Connected => self.connected = true,
            LiveOutput::Delta(text) => {
                // Mailbox may coalesce controls ahead of this delta carrying
                // the same cumulative count. An empty truncated delta still
                // means real bytes arrived (empty upstream deltas are omitted).
                if !text.is_empty() || new_body || event.truncated {
                    self.last_output = Some(self.last_output.map_or(event.received_at, |previous| previous.max(event.received_at)));
                }
                self.text.push_str(&text);
            }
            LiveOutput::ModelFinished => self.model_finished = true,
            LiveOutput::Error(error) => self.error = Some(error),
        }
    }
    pub fn finish(&mut self, request_id: u64, result: &Result<serde_json::Value, ServiceError>, now: Instant) {
        if self.request_id != request_id || self.returned.is_some() { return; }
        self.returned = Some(now);
        match result {
            Ok(value) => {
                let status = match value["status"].as_str() {
                    Some(status @ ("done" | "failed" | "cancelled")) => status,
                    _ => "unknown",
                };
                self.task_status = Some(status.into());
            },
            Err(_) => {
                self.task_status = Some("failed".into());
                self.error.get_or_insert_with(|| "本次操作失败；保留已有输出，任务结果以已发布记录为准，请查看操作回执。".into());
            }
        }
    }
    pub fn status_label(&self) -> &'static str {
        match self.task_status.as_deref() {
            Some("done") => "最终任务结果：完成",
            Some("failed") => "最终任务结果：失败",
            Some("cancelled") => "最终任务结果：已取消",
            Some(_) => "最终任务结果：结果未知",
            None if self.error.is_some() => "输出已中断；保留已有输出。等待最终任务结果。",
            None if self.model_finished => "模型输出已结束；等待最终证据与产物准入。",
            None if !self.text.is_empty() || self.received_bytes > 0 => "正在接收模型实际输出……",
            None if self.connected => "模型已连接，等待输出……",
            None => "已提交，等待模型响应……",
        }
    }
    fn channel_status_label(&self, status: Option<&str>) -> &'static str {
        if status.is_some() {
            "已结束"
        } else if self.error.is_some() {
            "中断"
        } else if self.model_finished {
            "模型输出已结束"
        } else if !self.text.is_empty() || self.received_bytes > 0 {
            "已连接并收到正文"
        } else if self.connected {
            "已连接，等待正文"
        } else {
            "已提交，等待连接"
        }
    }
    fn task_status_label(status: Option<&str>) -> &'static str {
        match status {
            Some("done") => "完成",
            Some("failed") => "失败",
            Some("cancelled") => "已取消",
            Some(_) => "结果未知",
            None => "尚未返回",
        }
    }
    fn next_step_label(&self, status: Option<&str>) -> &'static str {
        match status {
            Some("done") => "核对产物目录和操作回执",
            Some("failed") => "查看操作回执；不会自动重跑",
            Some("cancelled") => "以最终取消回执为准",
            Some(_) => "名额保留；不要手动释放或自动重跑",
            None if self.error.is_some() => "等待最终任务回执；不要手动释放或自动重跑",
            None if self.model_finished => "等待最终证据与产物准入",
            None if !self.text.is_empty() || self.received_bytes > 0 => "继续查看实际输出；最终结果以回执为准",
            None => "等待模型输出或最终任务回执",
        }
    }
    pub fn truncation_note(&self) -> Option<&'static str> {
        self.truncated.then_some("展示已截断；完整正文：通过最终产物核对；当前面板仅保留前 1 MiB，未展示正文不会自动回放。")
    }
    pub fn render(&mut self, ui: &mut egui::Ui, now: Instant) {
        self.render_with_published_status(ui, now, None);
    }
    pub fn render_with_published_status(&mut self, ui: &mut egui::Ui, now: Instant, published_status: Option<&str>) {
        // Snapshot authority affects this paint only, never the local status
        // used by submission barriers or the operation receipt/error.
        let status = published_status.filter(|status| matches!(*status, "done" | "failed" | "cancelled" | "unknown"))
            .or(self.task_status.as_deref()).map(str::to_owned);
        theme::card(ui, |ui| {
            ui.strong(RichText::new("模型实时输出（SSE）").color(theme::BRAND).size(19.));
            ui.small("执行活动 · 仅显示实际收到的模型正文；关闭工作台后不会恢复该正文。");
            if status.is_none() { ui.label(self.status_label()); }
            ui.label(format!("输出通道：{}", self.channel_status_label(status.as_deref())));
            ui.label(format!("最终任务结果：{}", Self::task_status_label(status.as_deref())));
            ui.label(format!("下一步：{}", self.next_step_label(status.as_deref())));
            let utc = self.submitted_unix_secs % 86_400;
            ui.horizontal_wrapped(|ui| {
                ui.small(format!("提交时刻：{:02}:{:02}:{:02} UTC", utc / 3600, utc / 60 % 60, utc % 60));
                ui.small(format!("本次请求耗时：{} 秒", self.returned.unwrap_or(now).saturating_duration_since(self.submitted).as_secs()));
                ui.small(format!("已接收正文：{} bytes（{:.1} KiB）", self.received_bytes, self.received_bytes as f64 / 1024.));
            });
            ui.small(match self.last_output {
                Some(last) => format!("最近正文：{} 秒前（Manager 本机接收）", now.saturating_duration_since(last).as_secs()),
                None => "尚未收到正文；等待不代表任务失败。".into(),
            });
            let mut jump = false;
            ui.horizontal_wrapped(|ui| {
                ui.strong("模型输出（纯文本）");
                ui.label(if self.following { "跟随最新" } else { "正在查看前文" });
                if !self.following && self.received_bytes > self.seen_bytes {
                    ui.label(format!("有新内容 · {} bytes", self.received_bytes - self.seen_bytes));
                }
                jump = ui.small_button("回到最新").clicked();
            });
            let view = egui::ScrollArea::vertical().id_salt(("model-output", self.request_id))
                .max_height(320.).auto_shrink([false, true]).stick_to_bottom(true)
                .animated(false).show(ui, |ui| {
                    if self.text.is_empty() {
                        ui.label(if status.is_some() || self.model_finished {
                            "本次未收到模型正文，请查看最终任务结果与操作回执。"
                        } else if self.error.is_some() {
                            "尚未收到模型正文，输出通道已中断；等待最终任务回执。"
                        } else {
                            "等待首段正文；收到后将在这里逐段显示。"
                        });
                    } else {
                        ui.add(egui::Label::new(RichText::new(&self.text).monospace()).wrap().selectable(true));
                    }
                    if jump { ui.scroll_to_cursor(Some(egui::Align::BOTTOM)); }
                });
            self.following = view.state.clamped_offset().y + view.inner_rect.height() >= view.content_size.y - 1.;
            if self.following { self.seen_bytes = self.received_bytes; }
            if let Some(note) = self.truncation_note() { ui.colored_label(theme::WARN, note); }
            if let Some(error) = &self.error { ui.colored_label(theme::WARN, error); }
            if self.model_finished && status.is_none() { ui.small("模型输出已结束；任务结果仍以终态证据与产物准入为准。"); }
            if let Some(status) = &status {
                ui.small(match status.as_str() {
                    "done" => "任务终态已返回；请核对产物目录。",
                    "unknown" => "任务结果未知；名额保留，不会自动重跑。",
                    "failed" => "任务或提交失败；请查看操作回执。不会自动重跑。",
                    "cancelled" => "任务已取消；以最终回执为准。",
                    _ => "任务结果未识别；按未知处理，不会自动重跑。",
                });
            }
        });
    }
}
