#![cfg(feature="gui")]
#[path="../src/desktop/mod.rs"]
mod desktop;
use agentranch::{config::Mode,core::service::ServiceOptions};
use eframe::egui;

// The real-mode onboarding form must render both sections without starting SSH.
#[test]
fn real_manager_renders_worker_onboarding_without_starting_ssh() {
    let root=tempfile::tempdir().unwrap();
    let context=egui::Context::default();
    let mut app=desktop::Workbench::new(ServiceOptions{mode:Mode::Real,state_dir:root.path().join("ui"),worker_exe:"unused.exe".into(),gateway_key:None,gateway_model_token:None},&context).unwrap();
    app.reprepare_for_test(); // Real form is now inside the explicit settings modal.
    for _ in 0..3 {
        let mut warm=context.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(1100.,1400.))),..Default::default()},|ui|{
            egui::CentralPanel::default().show(ui,|ui|app.show(ui));
        }); warm.textures_delta.clear();
    }
    let mut output=context.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(1100.,1400.))),..Default::default()},|ui|{
        egui::CentralPanel::default().show(ui,|ui|app.show(ui));
    });
    output.textures_delta.clear();
    fn text(shape:&egui::epaint::Shape,result:&mut String) {
        match shape {
            egui::epaint::Shape::Text(value)=>{result.push_str(&value.galley.job.text);result.push('\n');}
            egui::epaint::Shape::Vec(values)=>for value in values{text(value,result);},
            _=>{},
        }
    }
    let mut rendered=String::new();
    for shape in &output.shapes{text(&shape.shape,&mut rendered);}
    for label in [
        "模型配置",
        "模型服务地址",
        "API key",
        "默认模型",
        "Worker 配置机",
        "＋ 添加 Worker",
    ] {
        assert!(rendered.contains(label), "onboarding field missing ({label}): {rendered}");
    }
    assert!(rendered.contains("自动准备并连接 Worker"), "onboarding action missing: {rendered}");
    assert!(app.snapshot().gateway.is_none());
    assert!(app.snapshot().runs.is_empty());
}
