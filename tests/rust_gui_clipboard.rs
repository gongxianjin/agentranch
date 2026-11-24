#![cfg(feature = "gui")]

#[path = "../src/desktop/mod.rs"]
mod desktop;

use agentranch::{config::Mode, core::service::ServiceOptions};
use eframe::egui;

fn app(context: &egui::Context) -> desktop::Workbench {
    let root = tempfile::tempdir().unwrap();
    let state_dir = Box::leak(Box::new(root)).path().join("ui");
    desktop::Workbench::new(
        ServiceOptions {
            mode: Mode::Real,
            state_dir,
            worker_exe: "unused.exe".into(),
            gateway_key: None,
            gateway_model_token: None,
        },
        context,
    )
    .unwrap()
}

fn frame(
    workbench: &mut desktop::Workbench,
    context: &egui::Context,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    // Modal areas perform an initial sizing pass; settle it before addressing fields.
    for _ in 0..3 {
        let mut sizing = context.run_ui(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400., 1400.))),
            ..Default::default()
        }, |ui| { egui::CentralPanel::default().show(ui, |ui| workbench.show(ui)); });
        sizing.textures_delta.clear();
    }
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 1400.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default().show(ui, |ui| workbench.show(ui));
        },
    );
    output.textures_delta.clear();
    output
}

fn rendered_text(output: &egui::FullOutput) -> String {
    fn collect(shape: &egui::epaint::Shape, text: &mut String) {
        match shape {
            egui::epaint::Shape::Text(value) => {
                text.push_str(&value.galley.job.text);
                text.push('\n');
            }
            egui::epaint::Shape::Vec(values) => {
                for value in values {
                    collect(value, text);
                }
            }
            _ => {}
        }
    }

    let mut text = String::new();
    for shape in &output.shapes {
        collect(&shape.shape, &mut text);
    }
    text
}

fn text_shape_rect(shape: &egui::epaint::Shape, needle: &str) -> Option<egui::Rect> {
    match shape {
        egui::epaint::Shape::Text(value) if value.galley.job.text == needle => {
            Some(egui::Rect::from_min_size(value.pos, value.galley.size()))
        }
        egui::epaint::Shape::Vec(values) => values
            .iter()
            .find_map(|value| text_shape_rect(value, needle)),
        _ => None,
    }
}

fn field_rect(
    context: &egui::Context,
    output: &egui::FullOutput,
    label: &str,
) -> egui::Rect {
    let label_rect = output
        .shapes
        .iter()
        .find_map(|shape| text_shape_rect(&shape.shape, label))
        .unwrap_or_else(|| panic!("onboarding label did not render: {label}"));
    let label_y = label_rect.center().y;
    let mut candidates: Vec<_> = context
        .interactive_rects_last_pass()
        .into_iter()
        .filter(|rect| {
            rect.min.x >= label_rect.max.x
                && rect.width() >= 80.0
                && (rect.center().y - label_y).abs() < 18.0
        })
        .collect();
    candidates.sort_by(|left, right| right.width().total_cmp(&left.width()));
    candidates.first().copied().unwrap_or_else(|| {
        panic!(
            "editable control did not render beside {label}; interactive rects: {candidates:?}"
        )
    })
}

fn click_field(
    workbench: &mut desktop::Workbench,
    context: &egui::Context,
    label: &str,
) {
    let output = frame(workbench, context, vec![]);
    let position = field_rect(context, &output, label).center();
    for pressed in [true, false] {
        frame(
            workbench,
            context,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ],
        );
    }
}

fn select_all_field(
    workbench: &mut desktop::Workbench,
    context: &egui::Context,
    label: &str,
) -> egui::FullOutput {
    let output = frame(workbench, context, vec![]);
    let rect = field_rect(context, &output, label);
    let start = egui::pos2(rect.left() + 2.0, rect.center().y);
    let end = egui::pos2(rect.right() - 2.0, rect.center().y);
    frame(
        workbench,
        context,
        vec![
            egui::Event::PointerMoved(start),
            egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ],
    );
    frame(
        workbench,
        context,
        vec![egui::Event::PointerMoved(end)],
    );
    frame(
        workbench,
        context,
        vec![
            egui::Event::PointerMoved(end),
            egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ],
    )
}

fn copy_selected(workbench: &mut desktop::Workbench, context: &egui::Context) -> egui::FullOutput {
    frame(workbench, context, vec![egui::Event::Copy])
}

fn paste(
    workbench: &mut desktop::Workbench,
    context: &egui::Context,
    text: &str,
) -> egui::FullOutput {
    frame(
        workbench,
        context,
        vec![egui::Event::Paste(text.to_owned())],
    )
}

fn copy_commands(output: &egui::FullOutput) -> Vec<String> {
    output
        .platform_output
        .commands
        .iter()
        .filter_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn onboarding_fields(workbench: &mut desktop::Workbench) {
    workbench.reprepare_for_test();
    workbench.set_onboarding_fields_for_test(
        "192.0.2.10",
        "root",
        "synthetic-root-password",
        "https://model.example",
        "synthetic-api-key",
        "test-model",
    );
}

#[test]
fn ordinary_onboarding_field_paste_replaces_selection_and_preserves_other_fields() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    onboarding_fields(&mut workbench);

    click_field(&mut workbench, &context, "IP");
    select_all_field(&mut workbench, &context, "IP");
    let output = paste(&mut workbench, &context, "节点-Δ-東京");
    let text = rendered_text(&output);

    assert!(text.contains("节点-Δ-東京"), "Unicode paste result missing: {text}");
    assert!(!text.contains("192.0.2.10"), "paste did not replace the selected host: {text}");
    assert!(text.contains("root"), "other ordinary field changed or disappeared: {text}");
    assert!(text.contains("test-model"), "other ordinary field changed or disappeared: {text}");
    assert!(!text.contains("synthetic-root-password"));
    assert!(!text.contains("synthetic-api-key"));
}

#[test]
fn password_paste_replaces_selection_without_rendering_the_secret() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    onboarding_fields(&mut workbench);

    click_field(&mut workbench, &context, "密码");
    select_all_field(&mut workbench, &context, "密码");
    let output = paste(&mut workbench, &context, "新密码-秘密");
    let text = rendered_text(&output);

    assert!(!text.contains("新密码-秘密"), "password paste leaked into rendered text: {text}");
    assert!(!text.contains("synthetic-root-password"));
    assert!(text.contains("密码"), "password control disappeared: {text}");
}

#[test]
fn selected_ordinary_text_is_copied_but_selection_alone_does_not_copy() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    onboarding_fields(&mut workbench);

    click_field(&mut workbench, &context, "IP");
    let selected = select_all_field(&mut workbench, &context, "IP");
    assert!(
        copy_commands(&selected).is_empty(),
        "selection must not auto-copy: {:?}",
        copy_commands(&selected)
    );

    let copied = copy_selected(&mut workbench, &context);
    assert_eq!(copy_commands(&copied), vec!["192.0.2.10".to_owned()]);
}

#[test]
fn selected_password_text_is_copied_without_unmasking_the_control() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    onboarding_fields(&mut workbench);

    click_field(&mut workbench, &context, "密码");
    select_all_field(&mut workbench, &context, "密码");
    let copied = copy_selected(&mut workbench, &context);

    assert_eq!(copy_commands(&copied), vec!["synthetic-root-password".to_owned()]);
    assert!(!rendered_text(&copied).contains("synthetic-root-password"));
}

#[test]
fn onboarding_clipboard_guidance_is_one_concise_form_level_hint() {
    let context = egui::Context::default();
    let mut workbench = app(&context);
    onboarding_fields(&mut workbench);
    let text = rendered_text(&frame(&mut workbench, &context, vec![]));

    assert_eq!(text.matches("支持 Ctrl+C/Ctrl+V").count(), 1, "clipboard hint must be form-level: {text}");
}

#[test]
fn focus_and_selection_never_emit_plaintext_copy_or_accessibility_text() {
    let context = egui::Context::default();
    context.enable_accesskit();
    let mut workbench = app(&context);
    onboarding_fields(&mut workbench);

    let focused = frame(&mut workbench, &context, vec![]);
    assert!(copy_commands(&focused).is_empty());
    let accesskit = format!("{:?}", focused.platform_output.accesskit_update);
    assert!(!accesskit.contains("synthetic-root-password"));
    assert!(!accesskit.contains("synthetic-api-key"));

    click_field(&mut workbench, &context, "密码");
    let selected = select_all_field(&mut workbench, &context, "密码");
    assert!(
        copy_commands(&selected).is_empty(),
        "focus/selection emitted CopyText: {:?}",
        copy_commands(&selected)
    );
    let accesskit = format!("{:?}", selected.platform_output.accesskit_update);
    assert!(!accesskit.contains("synthetic-root-password"));
    assert!(!accesskit.contains("synthetic-api-key"));
}
