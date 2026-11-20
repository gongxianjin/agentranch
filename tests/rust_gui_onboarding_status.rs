#![cfg(feature="gui")]
#[path="../src/desktop/mod.rs"]
mod desktop;
use agentranch::{config::{Actor, Mode}, core::{projects::{DocumentSnapshot, ProjectSnapshot}, provision::{ProvisionFailure, Provisioner}, provision_ledger::{LedgerState, ProvisionLedger}, service::{AppSnapshot, GatewayView, Readiness, RunView, ServiceOptions}, store::Run}};
use eframe::egui;
use std::{sync::{Arc, Barrier, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

fn frame(app: &mut desktop::Workbench, context: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
    let mut output = context.run_ui(egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100., 1400.))),
        events,
        ..Default::default()
    }, |ui| {
        egui::CentralPanel::default().show(ui, |ui| app.show(ui));
    });
    output.textures_delta.clear();
    output
}

fn rendered_text(output: &egui::FullOutput) -> String {
    let mut text = String::new();
    fn collect(shape: &egui::epaint::Shape, text: &mut String) {
        match shape {
            egui::epaint::Shape::Text(value) => {
                text.push_str(&value.galley.job.text);
                text.push('\n');
            }
            egui::epaint::Shape::Vec(values) => {
                for value in values { collect(value, text); }
            }
            _ => {}
        }
    }
    for shape in &output.shapes { collect(&shape.shape, &mut text); }
    text
}

fn rendered(app: &mut desktop::Workbench, context: &egui::Context) -> String {
    for _ in 0..3 { frame(app, context, vec![]); }
    rendered_text(&frame(app, context, vec![]))
}

fn open_settings(app: &mut desktop::Workbench, context: &egui::Context) {
    if !app.onboarding_form_open_for_test() { click_label(app, context, "连接设置"); }
    for _ in 0..3 { frame(app, context, vec![]); }
}

fn label_center(shape: &egui::epaint::Shape, label: &str) -> Option<egui::Pos2> {
    match shape {
        egui::epaint::Shape::Text(value) if value.galley.job.text == label => Some(value.pos + value.galley.size() / 2.),
        egui::epaint::Shape::Vec(values) => values.iter().find_map(|value| label_center(value, label)),
        _ => None,
    }
}

fn click_label(app: &mut desktop::Workbench, context: &egui::Context, label: &str) {
    let output = frame(app, context, vec![]);
    let position = output.shapes.iter().find_map(|shape| label_center(&shape.shape, label))
        .unwrap_or_else(|| panic!("button label must render: {label}"));
    for pressed in [true, false] {
        frame(app, context, vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton { pos: position, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() },
        ]);
    }
}

#[test]
fn connection_settings_open_only_on_request_and_stay_closed_after_dismissal() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let initial = rendered(&mut workbench, &context);
    assert!(initial.contains("连接设置"));
    assert!(!initial.contains("Linux 工作节点 IP"), "configuration must not occupy workflow: {initial}");
    assert!(initial.contains("模型实时输出（SSE）"));
    click_label(&mut workbench, &context, "连接设置");
    for _ in 0..3 { frame(&mut workbench, &context, vec![]); }
    let opened = rendered(&mut workbench, &context);
    for label in ["Linux 工作节点 IP", "root 账号", "root 密码", "API key", "默认模型"] {
        assert!(opened.contains(label), "missing modal field: {label}");
    }
    click_label(&mut workbench, &context, "关闭设置");
    for _ in 0..3 { frame(&mut workbench, &context, vec![]); }
    assert!(!rendered(&mut workbench, &context).contains("Linux 工作节点 IP"));
    assert_eq!(workbench.pending_count(), 0);
    assert!(workbench.snapshot().gateway.is_none());
}

// Catches misidentifying the configured model service as Claude while leaving
// the existing generic origin/model input and local-only setup behavior intact.
#[test]
fn status_clarity_connection_modal_does_not_claim_a_specific_provider() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    valid_onboarding_fields(&mut workbench);
    open_settings(&mut workbench, &context);
    let text = rendered(&mut workbench, &context);
    assert!(text.lines().any(|line| line == "模型服务地址"), "missing neutral service label: {text}");
    assert!(!text.contains("Claude"), "generic model connection falsely names Claude: {text}");
    assert!(text.contains("https://model.example"));
    assert!(text.contains("test-model"));
    assert_eq!(workbench.pending_count(), 0);
    assert!(workbench.snapshot().gateway.is_none());
}

#[test]
fn minimum_window_keeps_modal_footer_actions_inside_viewport() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    workbench.reprepare_for_test();
    let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(720., 540.));
    let mut output = None;
    for _ in 0..5 {
        let mut next = context.run_ui(egui::RawInput { screen_rect: Some(bounds), ..Default::default() }, |ui| {
            egui::CentralPanel::default().show(ui, |ui| workbench.show(ui));
        });
        next.textures_delta.clear();
        output = Some(next);
    }
    for label in ["保存配置", "自动准备并连接 Worker", "关闭设置"] {
        let shaped = output.as_ref().unwrap().shapes.iter().find_map(|shape| {
            label_center(&shape.shape, label).map(|center| (center, shape.clip_rect))
        }).unwrap_or_else(|| panic!("missing footer: {label}"));
        assert!(bounds.contains(shaped.0) && shaped.1.contains(shaped.0), "footer clipped: {label}: {shaped:?}");
    }
}

#[test]
fn project_flow_precedes_sse_on_main_workspace() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let mut snapshot = snapshot(None);
    snapshot.projects.push(project_snapshot("layout-project", false));
    workbench.set_snapshot_for_test(snapshot);
    let output = frame(&mut workbench, &context, vec![]);
    let y = |label| output.shapes.iter().find_map(|shape| label_center(&shape.shape, label)).unwrap().y;
    assert!(y("进度") < y("模型实时输出（SSE）"), "workflow must be before output");
}

#[test]
fn save_settings_is_local_only_and_restart_restores_without_root_password() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    valid_onboarding_fields(&mut workbench);
    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "保存配置");
    assert!(rendered(&mut workbench, &context).contains("配置已保存"));
    assert_eq!(workbench.pending_count(), 0);
    assert!(workbench.snapshot().gateway.is_none());
    click_label(&mut workbench, &context, "关闭设置");
    assert!(workbench.onboarding_secrets_empty_for_test());
    open_settings(&mut workbench, &context);
    let restored = rendered(&mut workbench, &context);
    assert!(restored.contains("192.0.2.10"));
    assert!(restored.contains("https://model.example"));
    assert!(!restored.contains("synthetic-token"));
    // Root password must not be restored; prepare is refused locally.
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    assert!(rendered(&mut workbench, &context).contains("请填写 root 密码"));
    assert_eq!(workbench.pending_count(), 0);
}

#[test]
fn settings_modal_clears_recovered_key_when_service_origin_changes() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    valid_onboarding_fields(&mut workbench);
    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "保存配置");
    click_label(&mut workbench, &context, "关闭设置");
    open_settings(&mut workbench, &context);
    let output = frame(&mut workbench, &context, vec![]);
    let label = output.shapes.iter().find_map(|shape| label_center(&shape.shape, "模型服务地址")).unwrap();
    let field = context.interactive_rects_last_pass().into_iter()
        .filter(|rect| rect.center().y >= label.y - 18. && rect.center().y <= label.y + 18. && rect.width() > 100.)
        .max_by(|a,b| a.width().total_cmp(&b.width())).unwrap();
    for pressed in [true, false] {
        frame(&mut workbench, &context, vec![egui::Event::PointerMoved(field.center()), egui::Event::PointerButton {pos:field.center(),button:egui::PointerButton::Primary,pressed,modifiers:Default::default()}]);
    }
    frame(&mut workbench, &context, vec![egui::Event::Key { key:egui::Key::A, physical_key:None, pressed:true, repeat:false, modifiers:egui::Modifiers::CTRL }]);
    frame(&mut workbench, &context, vec![egui::Event::Text("https://new.example".into())]);
    assert!(rendered(&mut workbench, &context).contains("服务地址已改变，请重新输入 API key"));
    click_label(&mut workbench, &context, "保存配置");
    assert!(!rendered(&mut workbench, &context).contains("配置已保存"));
    click_label(&mut workbench, &context, "关闭设置");
    open_settings(&mut workbench, &context);
    assert!(rendered(&mut workbench, &context).contains("https://new.example"), "reopening must not overwrite unsaved edits with old origin/key");
    click_label(&mut workbench, &context, "保存配置");
    assert!(!rendered(&mut workbench, &context).contains("配置已保存"));
}

#[test]
fn new_workbench_restores_saved_settings_without_claiming_connection() {
    let root = tempfile::tempdir().unwrap();
    let make = |context: &egui::Context| desktop::Workbench::new(ServiceOptions {
        mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None,
    }, context).unwrap();
    let first_ctx = egui::Context::default();
    let mut first = make(&first_ctx);
    valid_onboarding_fields(&mut first);
    open_settings(&mut first, &first_ctx);
    click_label(&mut first, &first_ctx, "保存配置");
    drop(first);
    let second_ctx = egui::Context::default();
    let mut second = make(&second_ctx);
    let main = rendered(&mut second, &second_ctx);
    assert!(second.snapshot().gateway.is_none());
    assert!(!main.contains("Worker 已连接，可提交项目"));
    assert!(!main.contains("Linux 工作节点 IP"));
    open_settings(&mut second, &second_ctx);
    let restored = rendered(&mut second, &second_ctx);
    assert!(restored.contains("192.0.2.10") && restored.contains("test-model"));
    click_label(&mut second, &second_ctx, "自动准备并连接 Worker");
    assert!(rendered(&mut second, &second_ctx).contains("请填写 root 密码"));
    assert_eq!(second.pending_count(), 0);
}

// Catches coupling validated ordinary fields to a missing credential. Only the
// in-process UI-test Vault is used; a new valid UUID suffix names no old key.
fn partial_recovery_fixture() -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>) {
    let root = tempfile::tempdir().unwrap();
    let context = egui::Context::default();
    let mut first = desktop::Workbench::new(ServiceOptions {
        mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None,
    }, &context).unwrap();
    valid_onboarding_fields(&mut first);
    open_settings(&mut first, &context);
    click_label(&mut first, &context, "保存配置");
    assert!(rendered(&mut first, &context).contains("配置已保存"));
    drop(first);
    let path = root.path().join("ui/worker-settings.json");
    let original = std::fs::read(&path).unwrap();
    let mut missing: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let target = missing["credential_target"].as_str().unwrap();
    let prefix = target.rsplit_once('/').unwrap().0;
    missing["credential_target"] = serde_json::json!(format!("{prefix}/{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&path, serde_json::to_vec(&missing).unwrap()).unwrap();
    (root, path, original)
}

#[test]
fn partial_recovery_missing_key_restores_fields_without_connection() {
    let (root, path, _) = partial_recovery_fixture();
    let before = std::fs::read(&path).unwrap();
    let context = egui::Context::default();
    let mut workbench = desktop::Workbench::new(ServiceOptions {
        mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None,
    }, &context).unwrap();
    open_settings(&mut workbench, &context);
    let text = rendered(&mut workbench, &context);
    for value in ["192.0.2.10", "root", "https://model.example", "test-model"] {
        assert!(text.lines().any(|line| line == value), "validated field not restored: {value}; {text}");
    }
    assert!(text.contains("四项普通配置已保留") && text.contains("API key 未恢复"));
    assert!(text.contains("重新输入并保存") && text.contains("不代表 Worker 已连接"));
    assert!(workbench.onboarding_secrets_empty_for_test());
    assert!(workbench.snapshot().gateway.is_none());
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    assert!(rendered(&mut workbench, &context).contains("请填写 root 密码"));
    workbench.set_onboarding_fields_for_test("192.0.2.10", "root", "synthetic-password", "https://model.example", "", "test-model");
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    assert!(rendered(&mut workbench, &context).contains("请填写 API key"));
    assert_eq!(workbench.pending_count(), 0);
    assert!(workbench.snapshot().gateway.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), before, "read/invalid prepare must not rewrite saved settings");
    // Explicit local replacement succeeds, without preparing or persisting root.
    workbench.set_onboarding_fields_for_test("192.0.2.10", "root", "", "https://model.example", "synthetic-replacement", "test-model");
    click_label(&mut workbench, &context, "保存配置");
    assert!(rendered(&mut workbench, &context).contains("配置已保存"));
    assert_eq!(workbench.pending_count(), 0);
    assert!(!std::fs::read_to_string(&path).unwrap().contains("synthetic-replacement"));
    drop(workbench);
    let mut restarted = desktop::Workbench::new(ServiceOptions {
        mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None,
    }, &context).unwrap();
    open_settings(&mut restarted, &context);
    assert!(!restarted.onboarding_secrets_empty_for_test(), "new synthetic key must recover");
    assert!(!rendered(&mut restarted, &context).contains("API key 未恢复"));
    click_label(&mut restarted, &context, "自动准备并连接 Worker");
    assert!(rendered(&mut restarted, &context).contains("请填写 root 密码"));
    assert_eq!(restarted.pending_count(), 0);
}

#[test]
fn partial_recovery_reopen_preserves_edits_and_recovers_same_origin_key() {
    let (root, path, original) = partial_recovery_fixture();
    let context = egui::Context::default();
    let mut workbench = desktop::Workbench::new(ServiceOptions {
        mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None,
    }, &context).unwrap();
    // Unsaved ordinary fields must survive reopening, even if a key reappears.
    workbench.set_onboarding_fields_for_test("192.0.2.44", "deploy", "", "https://model.example", "", "edited-model");
    open_settings(&mut workbench, &context);
    assert!(workbench.onboarding_secrets_empty_for_test());
    click_label(&mut workbench, &context, "关闭设置");
    std::fs::write(&path, original).unwrap();
    open_settings(&mut workbench, &context);
    let text = rendered(&mut workbench, &context);
    for value in ["192.0.2.44", "deploy", "https://model.example", "edited-model"] {
        assert!(text.lines().any(|line| line == value), "reopen replaced edited field: {value}; {text}");
    }
    assert!(!workbench.onboarding_secrets_empty_for_test(), "same-origin synthetic key must recover");
    assert!(text.contains("API key 已恢复") && text.contains("不代表 Worker 已连接"));
    assert!(!text.contains("API key 不可用") && !text.contains("API key 未恢复"), "stale key failure remained: {text}");
    assert_eq!(workbench.pending_count(), 0);
    assert!(workbench.snapshot().gateway.is_none());
}

#[test]
fn partial_recovery_save_failure_blocks_preparation_without_a_request() {
    let root = tempfile::tempdir().unwrap();
    let context = egui::Context::default();
    let mut workbench = desktop::Workbench::new(ServiceOptions {
        mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(), gateway_key: None, gateway_model_token: None,
    }, &context).unwrap();
    // Real filesystem replacement failure, not a mocked save result. Only the
    // in-process Vault receives synthetic input; no provisioner is invoked.
    std::fs::create_dir_all(root.path().join("ui/worker-settings.json")).unwrap();
    valid_onboarding_fields(&mut workbench);
    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    let text = rendered(&mut workbench, &context);
    assert!(text.contains("无法读写连接设置"), "save failure was hidden: {text}");
    assert!(!text.contains("配置已保存"));
    assert_eq!(workbench.pending_count(), 0);
    assert!(workbench.snapshot().gateway.is_none());
    assert!(root.path().join("ui/worker-settings.json").is_dir());
}

// Catches losing the actionable local failure category while projecting a
// service receipt into the compact status and settings modal. The external
// Helper seam fails without starting Python, SSH or model work.
struct MissingHelperProvisioner;
impl Provisioner for MissingHelperProvisioner {
    fn provision(&self, _: &agentranch::core::service::ProvisionRequest) -> Result<agentranch::core::provision::ProvisionResult, ProvisionFailure> {
        Err(ProvisionFailure::new("helper_unavailable"))
    }
}

#[test]
fn missing_bundled_helper_explains_package_repair_without_blaming_credentials() {
    let context = egui::Context::default();
    let root = tempfile::tempdir().unwrap();
    let mut workbench = desktop::Workbench::new_with_provisioner(ServiceOptions {
        mode: Mode::Real, state_dir: root.path().join("ui"), worker_exe: "unused.exe".into(),
        gateway_key: None, gateway_model_token: None,
    }, &context, Box::new(MissingHelperProvisioner)).unwrap();
    valid_onboarding_fields(&mut workbench);
    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    let deadline = Instant::now() + Duration::from_secs(5);
    while workbench.receipt_count() == 0 && Instant::now() < deadline {
        workbench.poll();
        std::thread::yield_now();
    }
    assert_eq!(workbench.receipt_count(), 1);
    let failed = rendered(&mut workbench, &context);
    assert!(failed.contains("tools/paramiko_provision.py") && failed.contains("runtime/python/python.exe"), "local dependency failure must identify the missing package components: {failed}");
    assert!(failed.contains("完整程序包") && failed.contains("尚未连接 Linux"), "repair guidance must distinguish a local package failure from remote authentication: {failed}");
    assert!(!failed.contains("synthetic-password") && !failed.contains("synthetic-token"));
    assert!(workbench.onboarding_secrets_empty_for_test());
    assert!(workbench.snapshot().gateway.is_none());
    click_label(&mut workbench, &context, "关闭设置");
    let compact = rendered(&mut workbench, &context);
    assert!(compact.contains("完整程序包"), "repair guidance must remain visible outside the modal: {compact}");
}

struct BlockingFailureProvisioner { entered: Arc<Barrier>, release: Arc<AtomicBool> }
impl Provisioner for BlockingFailureProvisioner {
    fn provision(&self, _: &agentranch::core::service::ProvisionRequest) -> Result<agentranch::core::provision::ProvisionResult, ProvisionFailure> {
        self.entered.wait();
        while !self.release.load(Ordering::Acquire) { std::thread::yield_now(); }
        Err(ProvisionFailure::new("synthetic_failure"))
    }
}


fn snapshot(gateway: Option<GatewayView>) -> AppSnapshot {
    AppSnapshot {
        mode: Mode::Real,
        role: "test".into(),
        readiness: Readiness::NotReady,
        actor: None,
        observed_at: 0,
        running: true,
        dependencies: vec![],
        runs: vec![],
        nodes: vec![],
        approvals: vec![],
        errors: vec![],
        unfinished: 0,
        projects: vec![],
        gateway,
        provision_ledger: None,
    }
}

fn app(context: &egui::Context) -> desktop::Workbench {
    let root = tempfile::tempdir().unwrap();
    // Leak only the test directory handle so the service remains backed by a
    // valid path for the entire render; no remote operation is started.
    let path = Box::leak(Box::new(root)).path().join("ui");
    desktop::Workbench::new(ServiceOptions {
        mode: Mode::Real,
        state_dir: path,
        worker_exe: "unused.exe".into(),
        gateway_key: None,
        gateway_model_token: None,
    }, context).unwrap()
}

fn app_with_failure_provisioner(
    context: &egui::Context,
    entered: Arc<Barrier>,
    release: Arc<AtomicBool>,
) -> desktop::Workbench {
    let root = tempfile::tempdir().unwrap();
    let path = Box::leak(Box::new(root)).path().join("ui");
    desktop::Workbench::new_with_provisioner(
        ServiceOptions {
            mode: Mode::Real,
            state_dir: path,
            worker_exe: "unused.exe".into(),
            gateway_key: None,
            gateway_model_token: None,
        },
        context,
        Box::new(BlockingFailureProvisioner { entered, release }),
    ).unwrap()
}

fn valid_onboarding_fields(app: &mut desktop::Workbench) {
    app.set_onboarding_fields_for_test(
        "192.0.2.10",
        "root",
        "synthetic-password",
        "https://model.example",
        "synthetic-token",
        "test-model",
    );
}

#[test]
fn missing_root_password_submit_shows_actionable_copy_without_starting_provision() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    workbench.set_onboarding_fields_for_test(
        "192.0.2.10",
        "root",
        "",
        "https://model.example",
        "synthetic-token",
        "test-model",
    );

    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    let text = rendered(&mut workbench, &context);
    assert!(text.contains("请填写 root 密码；仅用于本次准备，不会写入回执或日志"), "missing root password copy not rendered: {text}");
    assert!(!text.contains("synthetic-token"), "API key leaked into rendered text: {text}");
    assert_eq!(workbench.pending_count(), 0, "invalid input must not start provisioning");
}

#[test]
fn missing_api_key_submit_shows_actionable_copy_without_starting_provision() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    workbench.set_onboarding_fields_for_test(
        "192.0.2.10",
        "root",
        "synthetic-password",
        "https://model.example",
        "",
        "test-model",
    );

    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    let text = rendered(&mut workbench, &context);
    assert!(text.contains("请填写 API key；仅用于本次准备，不会写入回执或日志"), "missing API key copy not rendered: {text}");
    assert!(!text.contains("synthetic-password"), "root password leaked into rendered text: {text}");
    assert_eq!(workbench.pending_count(), 0, "invalid input must not start provisioning");
}

#[test]
fn missing_default_model_submit_shows_actionable_copy_without_starting_provision() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    workbench.set_onboarding_fields_for_test(
        "192.0.2.10",
        "root",
        "synthetic-password",
        "https://model.example",
        "synthetic-token",
        "",
    );

    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    let text = rendered(&mut workbench, &context);
    assert!(text.contains("请填写默认模型，例如 gpt-6.1-sol"), "missing default model copy not rendered: {text}");
    assert!(!text.contains("synthetic-password"), "root password leaked into rendered text: {text}");
    assert!(!text.contains("synthetic-token"), "API key leaked into rendered text: {text}");
    assert_eq!(workbench.pending_count(), 0, "invalid input must not start provisioning");
}

#[test]
fn pending_onboarding_submit_stays_disabled_and_does_not_duplicate() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    valid_onboarding_fields(&mut workbench);
    workbench.set_pending_provision_for_test();
    let pending_before = workbench.pending_count();

    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "正在准备 Worker（等待回执）");

    assert_eq!(workbench.pending_count(), pending_before, "pending provisioning must not duplicate");
}

#[test]
fn reprepare_validation_error_overrides_gateway_status_copy() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    workbench.set_snapshot_for_test(snapshot(Some(GatewayView {
        worker_id: "worker-a".into(),
        host: "192.0.2.1".into(),
        remote_verified: false,
        preflight_passed: true,
        credentials_available: false,
        used_slots: 0,
        artifact_sha256: "a".repeat(64),
    })));
    workbench.reprepare_for_test();
    workbench.set_onboarding_fields_for_test(
        "192.0.2.10",
        "root",
        "",
        "https://model.example",
        "synthetic-token",
        "test-model",
    );

    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    let text = rendered(&mut workbench, &context);
    assert!(text.contains("请填写 root 密码；仅用于本次准备，不会写入回执或日志"), "reprepare must show validator copy: {text}");
    assert!(!text.contains("会话凭据已超时，需要重新准备 Worker"), "stale gateway status must not hide input correction: {text}");
    assert_eq!(workbench.pending_count(), 0, "invalid reprepare input must not start provisioning");
}

#[test]
fn novice_onboarding_hides_internal_worker_controls_and_state_directory() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let text = rendered(&mut workbench, &context);
    assert!(!text.contains("执行 sum-v1"), "novice view must not expose internal template: {text}");
    assert!(!text.contains("选择 Worker"), "novice view must not expose worker selector: {text}");
    assert!(!text.contains("状态目录："), "novice view must not expose raw state directory: {text}");
}

#[test]
fn gateway_snapshot_auto_selects_unique_worker() {
    let mut selected = String::new();
    desktop::assign_unique_gateway_worker(&mut selected, &snapshot(Some(GatewayView { worker_id: "worker-a".into(), host: "192.0.2.1".into(), remote_verified: false, preflight_passed: true, credentials_available: true, used_slots: 0, artifact_sha256: "a".repeat(64) })));
    assert_eq!(selected, "worker-a");
}

#[test]
fn expired_session_credentials_are_not_submit_ready() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    workbench.set_snapshot_for_test(snapshot(Some(GatewayView {
        worker_id: "worker-a".into(),
        host: "192.0.2.1".into(),
        remote_verified: false,
        preflight_passed: true,
        credentials_available: false,
        used_slots: 0,
        artifact_sha256: "a".repeat(64),
    })));
    let text = rendered(&mut workbench, &context);
    assert_eq!(desktop::provisioning_status_copy(&workbench.snapshot(), &desktop::pending::Pending::default(), None, None), "会话凭据已超时，需要重新准备 Worker");
    assert!(text.contains("会话凭据已超时，需要重新准备 Worker"), "expired credentials must be visible: {text}");
    assert!(text.contains("连接设置"), "expired credentials must offer explicit reauthentication: {text}");
    assert!(!text.contains("Linux 工作节点 IP"), "expired session must not automatically cover the workflow");
    open_settings(&mut workbench, &context);
    assert!(rendered(&mut workbench, &context).contains("Linux 工作节点 IP"));
    assert!(!text.contains("Worker 已连接，可提交项目"), "expired credentials must not look submit-ready: {text}");
}

#[test]
fn config_only_gateway_with_expired_credentials_stays_waiting_remote() {
    let waiting = snapshot(Some(GatewayView {
        worker_id: "worker-config".into(),
        host: "192.0.2.30".into(),
        remote_verified: false,
        preflight_passed: false,
        credentials_available: false,
        used_slots: 0,
        artifact_sha256: "c".repeat(64),
    }));
    assert_eq!(desktop::onboarding_ui_state(&waiting, &desktop::pending::Pending::default(), None, None), desktop::OnboardingUiState::WaitingRemote);
    assert_eq!(desktop::provisioning_status_copy(&waiting, &desktop::pending::Pending::default(), None, None), "Worker 已连接，等待远端就绪");
}

#[test]
fn connected_snapshot_overrides_stale_input_and_failure_state() {
    let pending = desktop::pending::Pending::default();
    let connected = snapshot(Some(GatewayView { worker_id: "worker-a".into(), host: "192.0.2.1".into(), remote_verified: false, preflight_passed: true, credentials_available: true, used_slots: 0, artifact_sha256: "a".repeat(64) }));
    assert_eq!(desktop::onboarding_ui_state(&connected, &pending, Some("old input"), Some(&("old_failure".into(), "old next step".into()))), desktop::OnboardingUiState::Connected);
    let waiting = snapshot(Some(GatewayView { worker_id: "worker-a".into(), host: "192.0.2.1".into(), remote_verified: false, preflight_passed: false, credentials_available: true, used_slots: 0, artifact_sha256: "a".repeat(64) }));
    assert_eq!(desktop::onboarding_ui_state(&waiting, &pending, Some("old input"), Some(&("old_failure".into(), "old next step".into()))), desktop::OnboardingUiState::WaitingRemote);
}

#[test]
fn onboarding_success_receipt_redacts_gateway_path_from_ordinary_render() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let gateway_path = "C:\\\\private\\\\manager\\\\gateway.json";
    workbench.inject_receipt_for_test(serde_json::json!({
        "state": "worker_preflight",
        "preflight": true,
        "remote_verified": false,
        "worker_id": "worker-a",
        "gateway_path": gateway_path,
    }));
    let text = rendered(&mut workbench, &context);
    assert!(!text.contains(gateway_path), "ordinary receipt exposed raw gateway path: {text}");
    assert!(text.contains("Worker 准备完成"), "ordinary receipt should retain safe outcome copy: {text}");
}

#[test]
fn rendered_onboarding_cards_cover_pending_failure_waiting_and_connected() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    workbench.set_pending_provision_for_test();
    let pending_text = rendered(&mut workbench, &context);
    assert!(pending_text.contains("正在准备"), "pending card missing: {pending_text}");

    let mut workbench = app(&context);
    workbench.set_onboarding_failure_for_test("provision_error", "请检查输入后重试");
    let failure_text = rendered(&mut workbench, &context);
    assert!(failure_text.contains("准备失败"), "failure card missing: {failure_text}");
    assert!(failure_text.contains("provision_error"), "failure code missing: {failure_text}");

    let mut workbench = app(&context);
    workbench.set_snapshot_for_test(snapshot(Some(GatewayView { worker_id: "worker-a".into(), host: "192.0.2.1".into(), remote_verified: false, preflight_passed: false, credentials_available: true, used_slots: 0, artifact_sha256: "a".repeat(64) })));
    let waiting_text = rendered(&mut workbench, &context);
    assert!(waiting_text.contains("Worker 已连接，等待远端就绪"), "waiting card missing: {waiting_text}");

    let mut workbench = app(&context);
    workbench.set_snapshot_for_test(snapshot(Some(GatewayView { worker_id: "worker-a".into(), host: "192.0.2.1".into(), remote_verified: false, preflight_passed: true, credentials_available: true, used_slots: 0, artifact_sha256: "a".repeat(64) })));
    let connected_text = rendered(&mut workbench, &context);
    assert!(connected_text.contains("Worker 已连接，可提交项目"), "connected card missing: {connected_text}");
    assert!(!connected_text.contains("Linux 工作节点 IP"), "connected card must hide onboarding form: {connected_text}");
}

#[test]
fn explicit_reprepare_reopens_form_without_submitting_a_second_request() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    workbench.set_snapshot_for_test(snapshot(Some(GatewayView { worker_id: "worker-a".into(), host: "192.0.2.1".into(), remote_verified: false, preflight_passed: true, credentials_available: true, used_slots: 0, artifact_sha256: "a".repeat(64) })));
    let connected_text = rendered(&mut workbench, &context);
    assert!(connected_text.contains("重新准备 Worker"), "reprepare action missing: {connected_text}");
    assert!(!connected_text.contains("Linux 工作节点 IP"), "connected form must stay hidden: {connected_text}");
    assert_eq!(workbench.pending_count(), 0);

    click_label(&mut workbench, &context, "重新准备 Worker");
    let reopened_text = rendered(&mut workbench, &context);
    assert!(workbench.onboarding_form_open_for_test());
    assert!(workbench.onboarding_reprepare_visible_for_test(&snapshot(Some(GatewayView { worker_id: "worker-a".into(), host: "192.0.2.1".into(), remote_verified: false, preflight_passed: true, credentials_available: true, used_slots: 0, artifact_sha256: "a".repeat(64) }))));
    assert!(reopened_text.contains("Linux 工作节点 IP"), "reprepare must expose the form: {reopened_text}");
    assert!(reopened_text.contains("重新准备 Worker"), "reprepare control must remain available: {reopened_text}");
    assert_eq!(workbench.pending_count(), 0, "reprepare must not submit provisioning automatically");
}

#[test]
fn secret_fields_clear_after_accepted_submit_and_failure_receipt() {
    let context = egui::Context::default();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(AtomicBool::new(false));
    let mut workbench = app_with_failure_provisioner(&context, entered.clone(), release.clone());
    valid_onboarding_fields(&mut workbench);
    open_settings(&mut workbench, &context);
    click_label(&mut workbench, &context, "自动准备并连接 Worker");
    assert!(workbench.onboarding_secrets_empty_for_test(), "accepted submit must clear password and token immediately");
    let accepted_text = rendered(&mut workbench, &context);
    let accepted_snapshot = workbench.snapshot_json_for_test();
    for visible in [&accepted_text, &accepted_snapshot] {
        assert!(!visible.contains("synthetic-password"), "password leaked after accepted submit: {visible}");
        assert!(!visible.contains("synthetic-token"), "token leaked after accepted submit: {visible}");
    }
    entered.wait();
    release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while workbench.receipt_count() == 0 && Instant::now() < deadline {
        workbench.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    let text = rendered(&mut workbench, &context);
    let serialized = workbench.snapshot_json_for_test();
    assert!(workbench.onboarding_secrets_empty_for_test(), "failure receipt must not restore cleared secrets");
    for visible in [&text, &serialized] {
        assert!(!visible.contains("synthetic-password"), "password leaked: {visible}");
        assert!(!visible.contains("synthetic-token"), "token leaked: {visible}");
    }
    assert!(text.contains("准备失败"), "failure receipt must render after the fields clear: {text}");
}

#[test]
fn diagnostics_keeps_raw_gateway_path_while_ordinary_view_redacts_it() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let gateway_path = "C:\\\\private\\\\manager\\\\gateway.json";
    workbench.inject_receipt_for_test(serde_json::json!({
        "state": "worker_preflight",
        "preflight": true,
        "remote_verified": false,
        "worker_id": "worker-a",
        "gateway_path": gateway_path,
    }));
    let ordinary = rendered(&mut workbench, &context);
    assert!(!ordinary.contains(gateway_path));
    workbench.set_diagnostics_for_test(true);
    let diagnostics = rendered(&mut workbench, &context);
    assert!(diagnostics.contains("\"gateway_path\""), "diagnostics must retain the raw gateway_path field: {diagnostics}");
    assert!(diagnostics.contains("private"), "diagnostics must retain the raw path value: {diagnostics}");
}

#[test]
fn onboarding_state_mapper_distinguishes_pending_failure_preflight_and_connected() {
    let mut pending = desktop::pending::Pending::default();
    pending.accept("worker-provision".into(), 1);
    let preparing = desktop::onboarding_ui_state(&snapshot(None), &pending, None, None);
    assert_eq!(preparing, desktop::OnboardingUiState::Preparing);
    let pending = desktop::pending::Pending::default();
    let initial = desktop::onboarding_ui_state(&snapshot(None), &pending, None, None);
    assert_eq!(initial, desktop::OnboardingUiState::NotStarted);
    let input = desktop::onboarding_ui_state(&snapshot(None), &pending, Some("host must be an IP"), None);
    assert_eq!(input, desktop::OnboardingUiState::InputCorrection("host must be an IP".into()));
    let failed = desktop::onboarding_ui_state(&snapshot(None), &pending, None, Some(&("provision_error".into(), "请重试".into())));
    assert_eq!(failed, desktop::OnboardingUiState::Failed { code: "provision_error".into(), next_step: "请重试".into() });
    let waiting = desktop::onboarding_ui_state(&snapshot(Some(GatewayView { worker_id: "worker-a".into(), host: "192.0.2.1".into(), remote_verified: false, preflight_passed: false, credentials_available: true, used_slots: 0, artifact_sha256: "a".repeat(64) })), &pending, None, None);
    assert_eq!(waiting, desktop::OnboardingUiState::WaitingRemote);
    let connected = desktop::onboarding_ui_state(&snapshot(Some(GatewayView { worker_id: "worker-a".into(), host: "192.0.2.1".into(), remote_verified: false, preflight_passed: true, credentials_available: true, used_slots: 0, artifact_sha256: "a".repeat(64) })), &pending, None, None);
    assert_eq!(connected, desktop::OnboardingUiState::Connected);
}

#[test]
fn preflight_copy_never_claims_remote_or_model_readiness() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let text = rendered(&mut workbench, &context);
    assert!(!text.contains("远端已验证"));
    assert!(!text.contains("远端已就绪"));
    assert!(!text.contains("模型已就绪"));
}

fn historical_ledger(state: LedgerState) -> ProvisionLedger {
    ProvisionLedger {
        schema: "worker-provision-ledger/1".into(), state,
        output_dir: Some("C:\\managed\\provisioning\\worker-history".into()),
        gateway_path: Some("C:\\managed\\provisioning\\worker-history\\gateway.json".into()),
        worker_id: Some("worker-history".into()), host: Some("192.0.2.10".into()),
        artifact_sha256: Some("a".repeat(64)), artifact_bytes: Some(6),
        created_at: 1, updated_at: 2, failure_code: None, failure_next_step: None,
        attempt_id: None, remote_root: None, remote_side_effects: None,
    }
}

#[test]
fn historical_execution_and_admission_never_claim_current_live_status() {
    for state in [LedgerState::Preflight, LedgerState::Connected, LedgerState::Executing, LedgerState::Admitted] {
        let context = egui::Context::default();
        let mut workbench = app(&context);
        let mut recovered = snapshot(None);
        recovered.provision_ledger = Some(historical_ledger(state));
        workbench.set_snapshot_for_test(recovered);
        let text = rendered(&mut workbench, &context);
        assert!(text.contains("历史准备记录"), "recovery must explain history: {text}");
        assert!(text.contains("重新认证"), "recovery must require new credentials: {text}");
        assert!(text.contains("worker-history") && text.contains("192.0.2.10"), "history facts missing: {text}");
        assert!(!text.contains("远端任务执行中"), "history must not claim current execution: {text}");
        assert!(!text.contains("项目结果已准入"), "history must not claim current admission: {text}");
        assert!(!text.contains("Worker 已连接，可提交项目"), "history must not claim live connection: {text}");
    }
}

#[test]
fn active_gateway_overrides_stale_history_and_status_mapper_matches_render() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let mut connected = snapshot(Some(GatewayView { worker_id: "worker-current".into(), host: "192.0.2.20".into(), remote_verified: false, preflight_passed: true, credentials_available: true, used_slots: 0, artifact_sha256: "b".repeat(64) }));
    connected.provision_ledger = Some(historical_ledger(LedgerState::Admitted));
    let expected = desktop::provisioning_status_copy(&connected, &Default::default(), None, None);
    assert_eq!(expected, "Worker 已连接，可提交项目");
    workbench.set_snapshot_for_test(connected);
    let text = rendered(&mut workbench, &context);
    assert!(text.contains(expected), "production rendering and status mapper diverged: {text}");
    assert!(!text.contains("项目结果已准入"), "stale history overrode live Gateway: {text}");
}

#[test]
fn novice_onboarding_starts_with_unstarted_status_card() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let text = rendered(&mut workbench, &context);
    assert!(text.contains("未开始"), "initial onboarding state missing: {text}");
    assert!(text.contains("打开“连接设置”填写信息"), "initial onboarding guidance missing: {text}");
}

#[test]
fn initial_onboarding_form_shows_six_safe_field_hints() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    open_settings(&mut workbench, &context);
    let text = rendered(&mut workbench, &context);

    for hint in [
        "例如 192.168.1.20",
        "通常为 root",
        "仅用于本次准备，不会写入回执或日志",
        "例如 https://boapi.uno:18317；必须是 HTTPS origin",
        "例如 gpt-6.1-sol",
    ] {
        assert!(text.contains(hint), "onboarding hint missing: {hint}; rendered: {text}");
    }
    assert!(text.contains("API key"));
    assert!(!text.contains("synthetic-password"));
    assert!(!text.contains("synthetic-token"));
    for hidden in ["Worker ID", "Gateway path", "状态目录：", "sum-v1", "内部 JSON"] {
        assert!(!text.contains(hidden), "novice onboarding exposed internal text {hidden:?}: {text}");
    }
}

fn project_snapshot(id: &str, confirmed: bool) -> ProjectSnapshot {
    ProjectSnapshot {
        id: id.into(),
        name: "结算项目".into(),
        phase: if confirmed { "confirmed" } else { "document_review" }.into(),
        project_path: "C:\\internal\\project".into(),
        product_path: "C:\\internal\\product".into(),
        document: Some(DocumentSnapshot {
            version: 3,
            filename: "design.md".into(),
            sha256: "b".repeat(64),
            text: "# 结算设计\n".into(),
            confirmed,
        }),
        tasks: vec![],
        actions: vec![],
        blockers: vec![],
        can_confirm: !confirmed,
        can_split: confirmed,
        can_start: false,
        can_test: false,
        can_repair: false,
        can_accept: false,
    }
}

fn project_run(project_id: &str, status: &str, detail: serde_json::Value) -> RunView {
    RunView {
        run: Run {
            id: format!("run-{status}"),
            owner: Actor::Local(project_id.into()),
            template: "project-work-v1".into(),
            status: status.into(),
            attempt: 1,
            epoch: "epoch-fixture".into(),
            session: Some("session-fixture".into()),
            spawn_intent: true,
            deadline: 4_000,
            detail,
        },
        read_only: false,
        can_status: true,
        can_approvals: false,
        can_decide: false,
        can_report: false,
        can_retry: false,
        can_cancel: true,
        disabled_reason: None,
    }
}

fn project_work_detail(project_id: &str) -> serde_json::Value {
    serde_json::json!({"project_work": {"project_id": project_id}})
}

#[test]
fn onboarding_status_uses_current_project_evidence_for_execution_and_admission() {
    let context = egui::Context::default();
    let id = "project-fixture";
    let document = project_snapshot(id, true);

    let mut running = app(&context);
    running.set_snapshot_for_test(AppSnapshot {
        projects: vec![document.clone()],
        gateway: Some(GatewayView {
            worker_id: "worker-fixture".into(),
            host: "192.0.2.10".into(),
            remote_verified: false,
            preflight_passed: true,
            credentials_available: true,
            used_slots: 1,
            artifact_sha256: "c".repeat(64),
        }),
        runs: vec![project_run(id, "running", project_work_detail(id))],
        ..snapshot(None)
    });
    let running_text = rendered(&mut running, &context);
    assert!(running_text.contains("远端任务执行中"), "current run must drive execution status: {running_text}");
    assert!(!running_text.contains("Worker 已连接，可提交项目"), "active run must not show submit-ready status: {running_text}");

    let mut admitted = app(&context);
    admitted.set_snapshot_for_test(AppSnapshot {
        projects: vec![document],
        gateway: Some(GatewayView {
            worker_id: "worker-fixture".into(),
            host: "192.0.2.10".into(),
            remote_verified: false,
            preflight_passed: true,
            credentials_available: true,
            used_slots: 0,
            artifact_sha256: "c".repeat(64),
        }),
        runs: vec![project_run(id, "done", serde_json::json!({
            "project_work": {"project_id": id},
            "owner_admission": "admitted",
            "exit_proof": {"empty": true, "observer_alive": true},
            "artifact_dir": "C:\\\\managed\\\\artifacts\\\\run-done",
        }))],
        ..snapshot(None)
    });
    let admitted_text = rendered(&mut admitted, &context);
    assert!(admitted_text.contains("项目结果已准入"), "complete current evidence must drive admission status: {admitted_text}");
    assert!(!admitted_text.contains("Worker 已连接，可提交项目"), "admitted result must not show submit-ready status: {admitted_text}");

    let mut incomplete = app(&context);
    incomplete.set_snapshot_for_test(AppSnapshot {
        projects: vec![project_snapshot(id, true)],
        gateway: Some(GatewayView {
            worker_id: "worker-fixture".into(),
            host: "192.0.2.10".into(),
            remote_verified: false,
            preflight_passed: true,
            credentials_available: true,
            used_slots: 1,
            artifact_sha256: "c".repeat(64),
        }),
        runs: vec![project_run(id, "done", serde_json::json!({
            "project_work": {"project_id": id},
            "owner_admission": "admitted",
            "exit_proof": {"empty": true, "observer_alive": true},
        }))],
        ..snapshot(None)
    });
    let incomplete_text = rendered(&mut incomplete, &context);
    assert!(!incomplete_text.contains("项目结果已准入"), "missing materialized artifact must not claim admission: {incomplete_text}");
}

#[test]
fn novice_project_card_exposes_wizard_gates_and_hides_internal_controls() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let id = "project-fixture";
    workbench.set_snapshot_for_test(AppSnapshot {
        projects: vec![project_snapshot(id, false)],
        gateway: None,
        ..snapshot(None)
    });

    let text = rendered(&mut workbench, &context);
    assert!(text.contains("新建项目"), "novice create gate missing: {text}");
    assert!(text.contains("导入设计文档"), "novice import gate missing: {text}");
    assert!(text.contains("确认此上传版本"), "novice confirmation gate missing: {text}");
    assert!(text.contains("提交项目"), "novice direct-submit gate missing: {text}");
    assert!(text.contains("完成 Worker 准备并确认上传版本后，才能提交项目"), "blocked direct-submit guidance missing: {text}");
    for hidden in [
        "选择 Worker",
        "sum-v1",
        "状态目录：",
        "项目与产物目录",
        "执行依赖配置",
        "配置文件路径（由后台读取，不自动启动任务）",
        "加载执行配置",
    ] {
        assert!(!text.contains(hidden), "novice project page exposed internal control {hidden:?}: {text}");
    }
    for hidden in [
        "按已确认上传版拆分任务",
        "任务审核",
        "手动开始开发",
        "运行独立测试",
        "返修原因",
        "人工验收通过",
    ] {
        assert!(!text.contains(hidden), "novice project page exposed legacy wizard path {hidden:?}: {text}");
    }
}

#[test]
fn novice_project_card_distinguishes_connected_gate_and_active_unknown_admitted_results() {
    let context = egui::Context::default();
    let id = "project-fixture";
    let document = project_snapshot(id, true);

    let mut connected = app(&context);
    connected.set_snapshot_for_test(AppSnapshot {
        projects: vec![document.clone()],
        gateway: Some(GatewayView {
            worker_id: "worker-fixture".into(),
            host: "192.0.2.10".into(),
            remote_verified: false,
            preflight_passed: true,
            credentials_available: true,
            used_slots: 0,
            artifact_sha256: "c".repeat(64),
        }),
        ..snapshot(None)
    });
    let connected_text = rendered(&mut connected, &context);
    assert!(connected_text.contains("Worker 已连接，可提交项目"), "connected gate copy missing: {connected_text}");

    let mut active = app(&context);
    active.set_snapshot_for_test(AppSnapshot {
        projects: vec![document.clone()],
        runs: vec![project_run(id, "running", project_work_detail(id))],
        gateway: None,
        ..snapshot(None)
    });
    let active_text = rendered(&mut active, &context);
    assert!(active_text.contains("最终任务结果：尚未返回"), "active project must remain pre-terminal: {active_text}");
    assert!(active_text.contains("准入状态：尚未形成终态"), "active project must not claim admission: {active_text}");
    assert!(active_text.contains("本次会话没有该任务的实时正文"), "snapshot-only run must not invent live output: {active_text}");
    assert!(!active_text.contains("准入状态：已准入"));

    let mut unknown = app(&context);
    unknown.set_snapshot_for_test(AppSnapshot {
        projects: vec![document.clone()],
        runs: vec![project_run(id, "unknown", project_work_detail(id))],
        gateway: None,
        ..snapshot(None)
    });
    let unknown_text = rendered(&mut unknown, &context);
    assert!(unknown_text.contains("最终任务结果：结果未知"), "unknown result must remain explicit: {unknown_text}");
    assert!(unknown_text.contains("名额保留；不要手动释放或自动重跑"), "unknown result must preserve reservation guidance: {unknown_text}");
    assert!(!unknown_text.contains("准入状态：已准入"));

    let artifact_dir = "C:\\managed\\artifacts\\run-done";
    let mut admitted = app(&context);
    admitted.set_snapshot_for_test(AppSnapshot {
        projects: vec![document],
        runs: vec![project_run(id, "done", serde_json::json!({
            "project_work": {"project_id": id},
            "owner_admission": "admitted",
            "exit_proof": {"empty": true, "observer_alive": true},
            "artifact_dir": artifact_dir,
        }))],
        gateway: None,
        ..snapshot(None)
    });
    let admitted_text = rendered(&mut admitted, &context);
    assert!(admitted_text.contains("最终任务结果：完成"), "admitted task result missing: {admitted_text}");
    assert!(admitted_text.contains("准入状态：已准入"), "complete evidence must be admitted: {admitted_text}");
    assert!(admitted_text.contains(&format!("产物目录：{artifact_dir}")), "materialized artifact path missing: {admitted_text}");
    assert!(admitted_text.contains(artifact_dir), "artifact directory missing from admitted result: {admitted_text}");

    let mut incomplete = app(&context);
    incomplete.set_snapshot_for_test(AppSnapshot {
        projects: vec![project_snapshot(id, true)],
        runs: vec![project_run(id, "done", serde_json::json!({
            "project_work": {"project_id": id},
            "owner_admission": "admitted",
        }))],
        gateway: None,
        ..snapshot(None)
    });
    let incomplete_text = rendered(&mut incomplete, &context);
    assert!(incomplete_text.contains("准入状态：未准入"), "missing proof/artifact must stay unadmitted: {incomplete_text}");
    assert!(!incomplete_text.contains("准入状态：已准入"), "owner_admission alone must not claim admission: {incomplete_text}");
}

fn readiness_gateway(preflight: bool, credentials: bool, used_slots: u32) -> GatewayView {
    GatewayView { worker_id: "worker-readiness".into(), host: "192.0.2.10".into(), remote_verified: false,
        preflight_passed: preflight, credentials_available: credentials, used_slots, artifact_sha256: "c".repeat(64) }
}

// Exercises the production action's enabled flag AND the emitted command path.
// No Service submit, native window, network or clipboard is involved.
fn readiness_click_submit(snapshot: &AppSnapshot, pending: &desktop::pending::Pending, expected_enabled: bool) -> String {
    let context = egui::Context::default();
    let mut projects = desktop::projects::Projects::default();
    projects.selected = Some("fresh-project".into());
    let mut paint = |events| {
        let mut commands = vec![];
        let mut output = context.run_ui(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200., 1800.))),
            events, ..Default::default()
        }, |ui| { commands = projects.show(ui, snapshot, pending, &[], false); });
        output.textures_delta.clear();
        (output, commands)
    };
    for _ in 0..3 { assert!(paint(vec![]).1.is_empty()); }
    let (output, commands) = paint(vec![]);
    assert!(commands.is_empty());
    let scope = egui::Id::unique(("agentranch", "project-action", "project:fresh-project:work-submit"));
    let button = context.read_response(egui::Id::unique(scope.value().wrapping_add(1))).expect("production submit action");
    let position = button.rect.center();
    assert!(button.rect.intersects(output.shapes.iter().find_map(|s| label_center(&s.shape, "提交项目").map(|_| s.clip_rect)).unwrap()), "submit button must be visible");
    let mut clicked_commands = vec![];
    for pressed in [true, false] {
        clicked_commands.extend(paint(vec![egui::Event::PointerMoved(position), egui::Event::PointerButton {
            pos: position, button: egui::PointerButton::Primary, pressed, modifiers: Default::default(),
        }]).1);
    }
    if expected_enabled {
        assert!(button.enabled(), "ready submit must be enabled");
        assert_eq!(clicked_commands.len(), 1, "positive control must emit exactly one command");
        let (key, command) = &clicked_commands[0];
        assert_eq!(key, "work-submit:fresh-project");
        assert!(matches!(command, agentranch::core::service::UiCommand::ProjectWorkSubmit { project_id } if project_id == "fresh-project"));
    } else {
        assert!(clicked_commands.is_empty(), "blocked action emitted a command: {clicked_commands:?}");
        assert!(!button.enabled(), "blocked submit must be disabled");
    }
    rendered_text(&output)
}

#[test]
fn readiness_unknown_is_not_ready_and_cannot_submit_another_project() {
    // Catches skipping UNKNOWN in compact status while the project gate blocks it.
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let mut state = snapshot(Some(readiness_gateway(true, true, 1)));
    state.projects.push(project_snapshot("fresh-project", true));
    state.runs.push(project_run("old-project", "unknown", project_work_detail("old-project")));
    workbench.set_snapshot_for_test(state.clone());
    let text = rendered(&mut workbench, &context);
    assert!(!text.contains("Worker 已连接，可提交项目"), "UNKNOWN must override ready: {text}");
    assert!(text.contains("任务结果未知，名额保留"), "UNKNOWN must have actionable status: {text}");
    assert!(text.contains("不要手动释放或自动重跑"));
    let project_text = readiness_click_submit(&state, &Default::default(), false);
    assert!(project_text.contains("结果未知"));
    assert_eq!(state.gateway.as_ref().unwrap().used_slots, 1);
    assert_eq!(state.runs[0].run.status, "unknown");
}

#[test]
fn readiness_occupied_slot_without_project_run_blocks_actual_submit_click() {
    // Catches a gate that looks only for project-work runs, ignoring Store slots.
    for slots in [1, 2] {
        let mut state = snapshot(Some(readiness_gateway(true, true, slots)));
        state.projects.push(project_snapshot("fresh-project", true));
        let text = readiness_click_submit(&state, &Default::default(), false);
        assert!(text.contains("Worker 名额已占用"), "occupied slot needs local guidance: {text}");
        assert!(!text.contains("完成 Worker 准备后再提交项目"));
        let context = egui::Context::default();
        let mut workbench = app(&context);
        workbench.set_snapshot_for_test(state);
        let text = rendered(&mut workbench, &context);
        assert!(text.contains("Worker 名额已占用"));
        assert!(!text.contains("Worker 已连接，可提交项目"));
    }
}

#[test]
fn readiness_credentials_and_preflight_each_block_actual_submit_click() {
    // preflight=true/credentials=false is defensive UI coverage; Service normally
    // projects preflight && credentials, not evidence it emits this pair.
    for (preflight, credentials, expected) in [
        (true, false, "会话凭据已超时"),
        (false, true, "等待远端"),
        (false, false, "等待远端"),
    ] {
        let mut state = snapshot(Some(readiness_gateway(preflight, credentials, 0)));
        state.projects.push(project_snapshot("fresh-project", true));
        let text = readiness_click_submit(&state, &Default::default(), false);
        assert!(text.contains(expected), "missing specific blocker {expected}: {text}");
    }
}

#[test]
fn readiness_pending_before_run_publication_blocks_actual_submit_click() {
    // Catches treating an accepted-but-not-yet-published request as idle.
    for action in ["worker-provision", "work-submit:other-project"] {
        let mut state = snapshot(Some(readiness_gateway(true, true, 0)));
        state.projects.push(project_snapshot("fresh-project", true));
        let mut pending = desktop::pending::Pending::default();
        pending.accept(action.into(), 999);
        let text = readiness_click_submit(&state, &pending, false);
        assert!(text.contains("等待回执"), "pending request must be explained: {text}");
        assert_ne!(desktop::provisioning_status_copy(&state, &pending, None, None), "Worker 已连接，可提交项目");
        assert_eq!(pending.len(), 1);
    }
}

#[test]
fn readiness_old_admission_cannot_hide_unknown_or_running_in_either_order() {
    // Catches first-match iteration letting an old done/admitted run hide a blocker.
    let admitted = project_run("old-project", "done", serde_json::json!({
        "project_work":{"project_id":"old-project"}, "owner_admission":"admitted",
        "exit_proof":{"empty":true}, "artifact_dir":"C:\\managed\\old-artifacts"
    }));
    for (status, expected) in [("unknown", "任务结果未知，名额保留"), ("running", "远端任务执行中")] {
        for reverse in [false, true] {
            let mut state = snapshot(Some(readiness_gateway(true, true, 1)));
            state.runs = vec![admitted.clone(), project_run("active-project", status, project_work_detail("active-project"))];
            if reverse { state.runs.reverse(); }
            assert_eq!(desktop::provisioning_status_copy(&state, &Default::default(), None, None), expected, "status={status}, reverse={reverse}");
        }
    }
}

#[test]
fn readiness_published_run_outweighs_its_still_pending_receipt() {
    // A submit remains pending until its final receipt. Once Store publishes a
    // running run, the toolbar must not stay at the pre-publication stage.
    for (status, expected) in [("running", "远端任务执行中"), ("unknown", "任务结果未知，名额保留")] {
        let mut state = snapshot(Some(readiness_gateway(true, true, 1)));
        state.projects.push(project_snapshot("fresh-project", true));
        state.runs.push(project_run("active-project", status, project_work_detail("active-project")));
        let mut pending = desktop::pending::Pending::default();
        pending.accept("work-submit:active-project".into(), 999);
        assert_eq!(desktop::provisioning_status_copy(&state, &pending, None, None), expected);
        readiness_click_submit(&state, &pending, false);
    }
}

#[test]
fn readiness_ready_positive_control_emits_one_correct_project_command() {
    let mut state = snapshot(Some(readiness_gateway(true, true, 0)));
    state.projects.push(project_snapshot("fresh-project", true));
    let text = readiness_click_submit(&state, &Default::default(), true);
    assert!(text.contains("Worker 已连接，可提交项目"));
    assert_eq!(desktop::provisioning_status_copy(&state, &Default::default(), None, None), "Worker 已连接，可提交项目");
}

#[test]
fn novice_view_hides_project_internals_while_diagnostics_keeps_legacy_controls() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    let id = "project-fixture";
    workbench.set_snapshot_for_test(AppSnapshot {
        projects: vec![project_snapshot(id, true)],
        gateway: None,
        ..snapshot(None)
    });
    let ordinary = rendered(&mut workbench, &context);
    assert!(!ordinary.contains("项目与产物目录"));
    assert!(!ordinary.contains("执行依赖配置"));
    assert!(!ordinary.contains("配置文件路径（由后台读取，不自动启动任务）"));
    assert!(!ordinary.contains("状态目录："));

    workbench.set_diagnostics_for_test(true);
    let diagnostics = rendered(&mut workbench, &context);
    assert!(diagnostics.contains("高级 · 本地诊断"), "diagnostics mode control missing: {diagnostics}");
    assert!(diagnostics.contains("sum-v1"), "diagnostics template selector missing: {diagnostics}");
    assert!(diagnostics.contains("创建任务"), "diagnostics task control missing: {diagnostics}");
    assert!(diagnostics.contains("节点与依赖"), "diagnostics dependency panel missing: {diagnostics}");
}
