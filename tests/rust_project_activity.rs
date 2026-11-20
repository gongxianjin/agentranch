#![cfg(feature = "gui")]
#[path = "../src/desktop/theme.rs"]
#[allow(dead_code)]
mod theme;
#[path = "../src/desktop/project_activity.rs"]
mod project_activity;
use project_activity::ProjectActivity;
use agentranch::core::{live_output::LiveOutput, service::LiveOutputEvent};
use eframe::egui;
use std::time::{Duration, Instant};

fn event(now:Instant, output:LiveOutput, bytes:usize, truncated:bool)->LiveOutputEvent {
    LiveOutputEvent { request_id:7, project_id:"p".into(), output, received_at:now, received_bytes:bytes, truncated }
}
fn frame(activity:&mut ProjectActivity,ctx:&egui::Context,now:Instant,events:Vec<egui::Event>)->egui::FullOutput {
    let mut out=ctx.run_ui(egui::RawInput {screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(900.,800.))),events,..Default::default()},|ui| { activity.render(ui,now); });
    out.textures_delta.clear(); out
}
fn texts(out:&egui::FullOutput)->String {
    out.shapes.iter().filter_map(|s|match &s.shape {egui::epaint::Shape::Text(t)=>Some(t.galley.job.text.as_str()),_=>None}).collect::<Vec<_>>().join("\n")
}
// Published status is a view input, never a mutation of the local submission
// barrier. Catches reconciling by overwriting task_status or erasing evidence.
#[test]
fn live_result_projection_preserves_local_gate_body_error_and_clock() {
    let t = Instant::now();
    for (status, label, next) in [
        ("done", "完成", "核对产物目录和操作回执"),
        ("failed", "失败", "查看操作回执；不会自动重跑"),
        ("cancelled", "已取消", "以最终取消回执为准"),
        ("unknown", "结果未知", "名额保留；不要手动释放或自动重跑"),
        ("running", "失败", "查看操作回执；不会自动重跑"),
        ("unexpected", "失败", "查看操作回执；不会自动重跑"),
    ] {
        let ctx = egui::Context::default();
        let mut activity = ProjectActivity::new(7, "p".into(), t, 86_401);
        activity.apply(event(t + Duration::from_secs(1), LiveOutput::Delta("preserved body".into()), 1_048_579, true));
        activity.apply(event(t + Duration::from_secs(2), LiveOutput::ModelFinished, 1_048_579, true));
        activity.apply(event(t + Duration::from_secs(2), LiveOutput::Error("synthetic operation diagnostic".into()), 1_048_579, true));
        activity.finish(7, &Err(agentranch::core::service::ServiceError { code:"fixture".into(), message:"local".into(), next_step:"receipt".into() }), t + Duration::from_secs(3));
        let mut out = ctx.run_ui(egui::RawInput { screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(900.,1000.))), ..Default::default() }, |ui| {
            activity.render_with_published_status(ui, t + Duration::from_secs(20), Some(status));
        });
        out.textures_delta.clear();
        let text = texts(&out);
        assert_eq!(text.lines().filter(|line| line.starts_with("最终任务结果：")).collect::<Vec<_>>(), vec![format!("最终任务结果：{label}")], "status={status}: {text}");
        assert!(text.contains(next));
        assert!(text.contains("preserved body"));
        assert!(text.contains("synthetic operation diagnostic"));
        assert!(text.contains("1048579 bytes"));
        assert!(text.contains("展示已截断"));
        assert!(text.contains("本次请求耗时：3 秒"));
        assert_eq!(activity.task_status.as_deref(), Some("failed"), "rendering published facts must not rewrite the local barrier");
        assert!(!text.contains("任务结果仍以终态证据与产物准入为准"));
    }
}

#[test]
fn output_body_placeholder_tracks_waiting_and_terminal_without_inventing_text() {
    let t = Instant::now();
    let ctx = egui::Context::default();
    let mut activity = ProjectActivity::new(7, "p".into(), t, 0);
    let waiting = texts(&frame(&mut activity, &ctx, t, vec![]));
    assert!(waiting.contains("模型实时输出（SSE）"));
    assert!(waiting.contains("等待首段正文；收到后将在这里逐段显示"));
    activity.finish(7, &Ok(serde_json::json!({"status":"failed"})), t);
    let ended = texts(&frame(&mut activity, &ctx, t, vec![]));
    assert!(ended.contains("本次未收到模型正文，请查看最终任务结果与操作回执"));
    assert!(!ended.contains("等待首段正文"));
}

#[test]
fn output_body_grows_across_frames_before_final_receipt() {
    let t = Instant::now();
    let ctx = egui::Context::default();
    let mut activity = ProjectActivity::new(7, "p".into(), t, 0);
    activity.apply(event(t, LiveOutput::Delta("第一段".into()), 9, false));
    let first = texts(&frame(&mut activity, &ctx, t, vec![]));
    assert!(first.contains("模型实时输出（SSE）"));
    assert!(first.contains("第一段"));
    assert!(!first.contains("第二段"));
    activity.apply(event(t, LiveOutput::Delta("第二段".into()), 18, false));
    let second = texts(&frame(&mut activity, &ctx, t, vec![]));
    assert!(second.contains("第一段第二段"));
    assert!(second.contains("18 bytes"));
    assert!(second.contains("最终任务结果：尚未返回"));
    assert!(!second.contains("等待首段正文"));
}

#[test]
fn honest_local_times_and_stages_freeze_only_on_final_receipt() {
    let t=Instant::now(); let ctx=egui::Context::default();
    let mut activity=ProjectActivity::new(7,"p".into(),t,86_401);
    let waiting=texts(&frame(&mut activity,&ctx,t+Duration::from_secs(5),vec![]));
    assert!(waiting.contains("已提交，等待模型响应"));
    assert!(waiting.contains("00:00:01 UTC"));
    assert!(waiting.contains("本次请求耗时：5 秒"));
    assert!(waiting.contains("尚未收到正文"));
    activity.apply(event(t+Duration::from_secs(6),LiveOutput::Connected,0,false));
    assert!(texts(&frame(&mut activity,&ctx,t+Duration::from_secs(7),vec![])).contains("模型已连接，等待输出"));
    activity.apply(event(t+Duration::from_secs(8),LiveOutput::Delta("中\n<html>".into()),10,false));
    let streaming=texts(&frame(&mut activity,&ctx,t+Duration::from_secs(11),vec![]));
    assert!(streaming.contains("正在接收模型实际输出"));
    assert!(streaming.contains("10 bytes"));
    assert!(streaming.contains("最近正文：3 秒前"));
    assert!(streaming.contains("中\n<html>"));
    activity.apply(event(t+Duration::from_secs(12),LiveOutput::ModelFinished,10,false));
    let finished=texts(&frame(&mut activity,&ctx,t+Duration::from_secs(15),vec![]));
    assert!(finished.contains("等待最终证据"));
    assert!(finished.contains("本次请求耗时：15 秒"));
    assert!(!finished.contains("最终任务结果：完成"));
    activity.finish(7,&Ok(serde_json::json!({"status":"done"})),t+Duration::from_secs(17));
    let final_text=texts(&frame(&mut activity,&ctx,t+Duration::from_secs(100),vec![]));
    assert!(final_text.contains("本次请求耗时：17 秒"));
    assert!(final_text.contains("最终任务结果：完成"));
    assert!(final_text.contains("中\n<html>"));
}
#[test]
fn terminal_unknown_cannot_be_overwritten_by_wrong_or_late_events() {
    let t=Instant::now(); let ctx=egui::Context::default();
    let mut activity=ProjectActivity::new(7,"p".into(),t,0);
    for (request_id,project_id) in [(8,"p"),(7,"other")] {
        let mut e=event(t,LiveOutput::Delta("WRONG".into()),5,false); e.request_id=request_id; e.project_id=project_id.into(); activity.apply(e);
    }
    activity.apply(event(t,LiveOutput::Delta("保留".into()),6,false));
    activity.apply(event(t,LiveOutput::ModelFinished,6,false));
    activity.apply(event(t,LiveOutput::Error("输出连接中断；保留已有输出。".into()),6,false));
    activity.finish(7,&Ok(serde_json::json!({"status":"unknown"})),t);
    activity.apply(event(t,LiveOutput::Connected,6,false));
    activity.apply(event(t,LiveOutput::Delta("LATE".into()),10,false));
    let text=texts(&frame(&mut activity,&ctx,t,vec![]));
    assert!(text.contains("最终任务结果：结果未知"));
    assert!(text.contains("名额保留，不会自动重跑"));
    assert!(text.contains("输出连接中断"));
    assert!(text.contains("保留")); assert!(!text.contains("WRONG")); assert!(!text.contains("LATE"));
}
#[test]
fn truncation_is_visible_without_inventing_failure_or_hiding_completion() {
    let t=Instant::now(); let ctx=egui::Context::default();
    let mut activity=ProjectActivity::new(7,"p".into(),t,0);
    activity.apply(event(t,LiveOutput::Delta("shown".into()),1_048_579,true));
    activity.apply(event(t,LiveOutput::ModelFinished,1_048_579,true));
    activity.finish(7,&Ok(serde_json::json!({"status":"done"})),t);
    let text=texts(&frame(&mut activity,&ctx,t,vec![]));
    assert!(text.contains("展示已截断")); assert!(text.contains("1048579 bytes"));
    assert!(text.contains("最终任务结果：完成")); assert!(!text.contains("输出已中断"));
    assert!(text.contains("shown"));
}
#[test]
fn unrecognized_final_status_stays_unknown_and_empty_events_do_not_invent_body() {
    let t=Instant::now(); let ctx=egui::Context::default();
    let mut activity=ProjectActivity::new(7,"p".into(),t,0);
    activity.apply(event(t+Duration::from_secs(2),LiveOutput::Delta(String::new()),0,false));
    activity.finish(8,&Ok(serde_json::json!({"status":"done"})),t);
    let before=texts(&frame(&mut activity,&ctx,t+Duration::from_secs(3),vec![]));
    assert!(before.contains("尚未收到正文"));
    assert!(before.contains("最终任务结果：尚未返回"));
    activity.finish(7,&Ok(serde_json::json!({"status":"unexpected"})),t+Duration::from_secs(4));
    assert_eq!(activity.task_status.as_deref(),Some("unknown"),"unrecognized status must retain the existing unknown submit gate");
}
#[test]
fn truncated_delta_updates_last_receive_even_if_coalesced_control_already_carries_count() {
    let t=Instant::now(); let ctx=egui::Context::default();
    let mut activity=ProjectActivity::new(7,"p".into(),t,0);
    activity.apply(event(t,LiveOutput::Delta("prefix".into()),1_048_576,false));
    activity.apply(event(t+Duration::from_secs(8),LiveOutput::Connected,1_048_579,true));
    activity.apply(event(t+Duration::from_secs(7),LiveOutput::Delta(String::new()),1_048_579,true));
    let text=texts(&frame(&mut activity,&ctx,t+Duration::from_secs(10),vec![]));
    assert!(text.contains("最近正文：3 秒前"),"coalesced control must not hide actual truncated-body receipt: {}",text.chars().take(300).collect::<String>());
}
#[test]
fn failure_and_cancellation_do_not_show_streaming_or_erase_output() {
    let t=Instant::now(); let ctx=egui::Context::default();
    for (status,label) in [("failed","最终任务结果：失败"),("cancelled","最终任务结果：已取消")] {
        let mut activity=ProjectActivity::new(7,"p".into(),t,0);
        activity.apply(event(t,LiveOutput::Delta("actual partial".into()),14,false));
        activity.finish(7,&Ok(serde_json::json!({"status":status})),t);
        let text=texts(&frame(&mut activity,&ctx,t,vec![]));
        assert!(text.contains(label)); assert!(text.contains("actual partial")); assert!(!text.contains("正在接收模型实际输出"));
    }
    let mut activity=ProjectActivity::new(7,"p".into(),t,0);
    activity.finish(7,&Err(agentranch::core::service::ServiceError {code:"blocked".into(),message:"local fixture".into(),next_step:"check receipt".into()}),t);
    let text=texts(&frame(&mut activity,&ctx,t,vec![]));
    assert!(text.contains("最终任务结果：失败")); assert!(text.contains("本次操作失败"));
}
#[test]
fn nested_project_scroll_keeps_reader_position_and_jump_control_reachable() {
    let t=Instant::now(); let ctx=egui::Context::default();
    let mut activity=ProjectActivity::new(7,"p".into(),t,0);
    let body="actual nested line\n".repeat(120);
    activity.apply(event(t,LiveOutput::Delta(body.clone()),body.len(),false));
    let frame=|activity:&mut ProjectActivity,events:Vec<egui::Event>| {
        let mut out=ctx.run_ui(egui::RawInput {screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(900.,800.))),events,..Default::default()},|ui| {
            egui::ScrollArea::vertical().id_salt("outer-project").show(ui,|ui| {ui.label("项目详情"); activity.render(ui,t); ui.label("操作回执");});
        }); out.textures_delta.clear(); out
    };
    let mut out=frame(&mut activity,vec![]);
    for _ in 0..5 {out=frame(&mut activity,vec![]);}
    let wheel=|delta|vec![egui::Event::PointerMoved(egui::pos2(150.,260.)),egui::Event::MouseWheel {unit:egui::MouseWheelUnit::Point,delta:egui::vec2(0.,delta),phase:egui::TouchPhase::Move,source:egui::MouseWheelSource::Wheel,modifiers:Default::default()}];
    let _=frame(&mut activity,wheel(180.));
    for _ in 0..20 {out=frame(&mut activity,vec![]);}
    assert!(texts(&out).contains("正在查看前文"));
    activity.apply(event(t,LiveOutput::Delta("new nested\n".into()),body.len()+11,false));
    for _ in 0..3 {out=frame(&mut activity,vec![]);}
    assert!(texts(&out).contains("有新内容"));
    let pos=out.shapes.iter().find_map(|s|match &s.shape {egui::epaint::Shape::Text(t) if t.galley.job.text=="回到最新"=>Some(egui::Rect::from_min_size(t.pos,t.galley.size()).center()),_=>None}).unwrap();
    assert!(pos.y<800.,"jump must be visible above body");
    for pressed in [true,false] {let _=frame(&mut activity,vec![egui::Event::PointerMoved(pos),egui::Event::PointerButton {pos,button:egui::PointerButton::Primary,pressed,modifiers:Default::default()}]);}
    for _ in 0..5 {out=frame(&mut activity,vec![]);}
    assert!(texts(&out).contains("跟随最新"));
    assert!(!texts(&out).contains("有新内容"));
}
#[test]
fn wheel_reading_pauses_follow_and_jump_click_restores_latest() {
    let t=Instant::now(); let ctx=egui::Context::default();
    let mut activity=ProjectActivity::new(7,"p".into(),t,0);
    let body=(0..100).map(|n|format!("actual line {n}\n")).collect::<String>();
    activity.apply(event(t,LiveOutput::Delta(body.clone()),body.len(),false));
    let mut out=frame(&mut activity,&ctx,t,vec![]);
    for _ in 0..4 {out=frame(&mut activity,&ctx,t,vec![]);}
    fn body_y(out:&egui::FullOutput)->f32 {
        out.shapes.iter().find_map(|s|match &s.shape {egui::epaint::Shape::Text(t) if t.galley.job.text.starts_with("actual line")=>Some(t.pos.y),_=>None}).expect("real body shape")
    }
    let bottom=body_y(&out);
    // Pointer lies inside the activity's inner scroll viewport, not the outer page.
    out=frame(&mut activity,&ctx,t,vec![egui::Event::PointerMoved(egui::pos2(150.,240.)),egui::Event::MouseWheel {
        unit:egui::MouseWheelUnit::Point,delta:egui::vec2(0.,200.),phase:egui::TouchPhase::Move,source:egui::MouseWheelSource::Wheel,modifiers:Default::default()
    }]);
    for _ in 0..20 {out=frame(&mut activity,&ctx,t,vec![]);}
    let before=body_y(&out);
    assert!(before>bottom+50.,"wheel must actually leave bottom: {before} vs {bottom}");
    activity.apply(event(t,LiveOutput::Delta("new actual line\n".into()),body.len()+16,false));
    let _=frame(&mut activity,&ctx,t,vec![]);
    out=frame(&mut activity,&ctx,t,vec![]);
    assert!((body_y(&out)-before).abs()<2.,"new text must not drag reader to bottom");
    let text=texts(&out);
    assert!(text.contains("正在查看前文"));
    assert!(text.contains("有新内容"),"unseen output feedback missing: {text}");
    let pos=out.shapes.iter().find_map(|s|match &s.shape {egui::epaint::Shape::Text(t) if t.galley.job.text=="回到最新"=>Some(egui::Rect::from_min_size(t.pos,t.galley.size()).center()),_=>None}).expect("jump button");
    for pressed in [true,false] {out=frame(&mut activity,&ctx,t,vec![egui::Event::PointerMoved(pos),egui::Event::PointerButton {pos,button:egui::PointerButton::Primary,pressed,modifiers:Default::default()}]);}
    for _ in 0..5 {out=frame(&mut activity,&ctx,t,vec![]);}
    assert!(body_y(&out)<before-50.,"jump failed: bottom={bottom}, before={before}, after={}, text={}",body_y(&out),texts(&out).chars().take(250).collect::<String>());
    assert!(texts(&out).contains("跟随最新"));
    assert!(!texts(&out).contains("有新内容"));
}

#[test]
fn error_before_final_receipt_keeps_task_pending() {
    let t = Instant::now();
    let ctx = egui::Context::default();
    let mut activity = ProjectActivity::new(7, "p".into(), t, 0);
    activity.apply(event(t, LiveOutput::Connected, 0, false));
    activity.apply(event(t + Duration::from_secs(1), LiveOutput::Delta("partial".into()), 7, false));
    activity.apply(event(t + Duration::from_secs(2), LiveOutput::Error("transport interrupted".into()), 7, false));
    let text = texts(&frame(&mut activity, &ctx, t + Duration::from_secs(3), vec![]));
    assert!(text.contains("输出通道：中断"), "channel label must not repeat itself: {text}");
    assert!(!text.contains("输出通道：输出通道"));
    assert!(text.contains("最终任务结果：尚未返回"));
    assert!(text.contains("等待最终任务回执"));
    assert!(text.contains("不要手动释放或自动重跑"));
    assert!(!text.contains("最终任务结果：失败"));
}

#[test]
fn model_finished_has_explicit_waiting_guidance() {
    let t = Instant::now();
    let ctx = egui::Context::default();
    let mut activity = ProjectActivity::new(7, "p".into(), t, 0);
    activity.apply(event(t, LiveOutput::ModelFinished, 0, false));
    let text = texts(&frame(&mut activity, &ctx, t + Duration::from_secs(1), vec![]));
    assert!(text.contains("输出通道：模型输出已结束"));
    assert!(text.contains("最终任务结果：尚未返回"));
    assert!(text.contains("等待最终证据与产物准入"));
    assert!(!text.contains("最终任务结果：完成"));
}

#[test]
fn done_keeps_body_and_points_to_artifacts() {
    let t = Instant::now();
    let ctx = egui::Context::default();
    let mut activity = ProjectActivity::new(7, "p".into(), t, 0);
    activity.apply(event(t, LiveOutput::Delta("actual body".into()), 1_048_579, true));
    activity.finish(7, &Ok(serde_json::json!({"status": "done"})), t + Duration::from_secs(1));
    let text = texts(&frame(&mut activity, &ctx, t + Duration::from_secs(2), vec![]));
    assert!(text.contains("actual body"));
    assert!(text.contains("最终任务结果：完成"));
    assert!(text.contains("核对产物目录和操作回执"));
    assert!(text.contains("完整正文：通过最终产物核对"));
    assert!(!text.contains("正在接收模型实际输出"));
}

#[test]
fn terminal_outcomes_have_distinct_safe_next_steps() {
    let t = Instant::now();
    let ctx = egui::Context::default();
    for (status, result_label, next_step) in [
        ("failed", "最终任务结果：失败", "查看操作回执"),
        ("cancelled", "最终任务结果：已取消", "最终取消回执"),
        ("unknown", "最终任务结果：结果未知", "名额保留"),
    ] {
        let mut activity = ProjectActivity::new(7, "p".into(), t, 0);
        activity.apply(event(t, LiveOutput::Delta("partial".into()), 7, false));
        activity.finish(7, &Ok(serde_json::json!({"status": status})), t + Duration::from_secs(1));
        let text = texts(&frame(&mut activity, &ctx, t + Duration::from_secs(2), vec![]));
        assert!(text.contains(result_label), "missing {result_label}: {text}");
        assert!(text.contains(next_step), "missing {next_step}: {text}");
        assert!(!text.contains("正在接收模型实际输出"), "terminal status still streams: {text}");
        if status == "cancelled" {
            assert!(!text.contains("自动重跑"));
            assert!(!text.contains("手动释放"));
        }
    }
}

// Catches rendering the terminal headline and the explicit final-result row
// together; expectations come from the receipt contract, not status helpers.
#[test]
fn status_clarity_terminal_result_is_reported_once() {
    let t = Instant::now();
    for (status, label, next) in [
        ("done", "完成", "核对产物目录和操作回执"),
        ("failed", "失败", "查看操作回执；不会自动重跑"),
        ("cancelled", "已取消", "以最终取消回执为准"),
        ("unknown", "结果未知", "名额保留；不要手动释放或自动重跑"),
        ("unexpected", "结果未知", "名额保留；不要手动释放或自动重跑"),
        ("submit-error", "失败", "查看操作回执；不会自动重跑"),
    ] {
        let ctx = egui::Context::default();
        let mut activity = ProjectActivity::new(7, "p".into(), t, 0);
        activity.apply(event(t, LiveOutput::Delta("actual preserved body".into()), 21, false));
        let result = if status == "submit-error" {
            Err(agentranch::core::service::ServiceError { code: "blocked".into(), message: "synthetic".into(), next_step: "check receipt".into() })
        } else {
            Ok(serde_json::json!({"status": status}))
        };
        activity.finish(7, &result, t + Duration::from_secs(1));
        let text = texts(&frame(&mut activity, &ctx, t + Duration::from_secs(2), vec![]));
        assert_eq!(text.matches("最终任务结果：").count(), 1, "duplicate final-result rows for {status}: {text}");
        assert!(text.lines().any(|line| line == format!("最终任务结果：{label}")), "missing result for {status}: {text}");
        assert!(text.contains("输出通道：已结束"));
        assert!(text.contains(next));
        assert!(text.contains("actual preserved body"));
        assert!(!text.contains("正在接收模型实际输出"));
        assert!(!text.contains("等待最终证据"));
        if status == "submit-error" { assert!(text.contains("本次操作失败")); }
    }
}

// Catches an obsolete ModelFinished explanation outliving the final receipt;
// hiding that note must not erase actual error/truncation/body evidence.
#[test]
fn status_clarity_terminal_receipt_removes_obsolete_model_note() {
    let t = Instant::now();
    for status in ["done", "failed", "cancelled", "unknown"] {
        let ctx = egui::Context::default();
        let mut activity = ProjectActivity::new(7, "p".into(), t, 0);
        activity.apply(event(t, LiveOutput::Delta("actual body".into()), 1_048_579, true));
        activity.apply(event(t, LiveOutput::ModelFinished, 1_048_579, true));
        activity.apply(event(t, LiveOutput::Error("synthetic output diagnostic".into()), 1_048_579, true));
        activity.finish(7, &Ok(serde_json::json!({"status": status})), t + Duration::from_secs(1));
        let text = texts(&frame(&mut activity, &ctx, t + Duration::from_secs(2), vec![]));
        assert!(!text.contains("任务结果仍以终态证据与产物准入为准"), "obsolete pre-terminal explanation for {status}: {text}");
        assert!(text.contains("synthetic output diagnostic"));
        assert!(text.contains("展示已截断"));
        assert!(text.contains("1048579 bytes"));
        assert!(text.contains("actual body"));
    }
}

#[test]
fn waiting_and_streaming_have_no_remote_liveness_claim() {
    let t = Instant::now();
    let ctx = egui::Context::default();
    let mut activity = ProjectActivity::new(7, "p".into(), t, 0);
    let waiting = texts(&frame(&mut activity, &ctx, t + Duration::from_secs(1), vec![]));
    assert!(waiting.contains("等待模型输出或最终任务回执"));
    assert!(!waiting.contains("Worker 存活"));
    assert!(!waiting.contains("超时"));
    assert!(!waiting.contains("最终任务结果：失败"));
    activity.apply(event(t + Duration::from_secs(2), LiveOutput::Connected, 0, false));
    activity.apply(event(t + Duration::from_secs(3), LiveOutput::Delta("actual".into()), 6, false));
    let streaming = texts(&frame(&mut activity, &ctx, t + Duration::from_secs(4), vec![]));
    assert!(streaming.contains("继续查看实际输出"));
    assert!(!streaming.contains("Worker 存活"));
    assert!(!streaming.contains("超时"));
}
