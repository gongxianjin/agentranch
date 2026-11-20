use std::process::Command;

#[test]
fn capability_probe_is_read_only_and_preserves_the_gui_identity_contract() {
    let output = Command::new(env!("CARGO_BIN_EXE_aw-cli"))
        .arg("--capabilities").output().unwrap();
    assert!(output.status.success(), "capability probe must be available: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty());
    let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual["schema"], "worker-capabilities/1");
    assert_eq!(actual["binary"], "aw-cli");
    assert_eq!(actual["platform"], std::env::consts::OS);
    if cfg!(target_os = "linux") {
        assert_eq!(actual["owner_exec_templates"], serde_json::json!(["sum-v1", "project-work-v1"]));
        assert_eq!(actual["model_token_frame"], "u32be-utf8/1");
        assert_eq!(actual["live_output"], "AO-LIVE/1");
    } else {
        assert_eq!(actual["owner_exec_templates"], serde_json::json!([]));
        assert!(actual["model_token_frame"].is_null());
        assert!(actual["live_output"].is_null());
    }
    let identity = Command::new(env!("CARGO_BIN_EXE_aw-cli")).arg("--identity").output().unwrap();
    assert!(identity.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&identity.stdout).unwrap(), serde_json::json!({
        "binary": "aw-cli", "package": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION")
    }));
}

#[test]
fn help_lists_controller_entrypoint_with_once_option() {
    let output = Command::new(env!("CARGO_BIN_EXE_aw-cli"))
        .arg("--help")
        .output()
        .expect("aw-cli --help should start");

    assert!(
        output.status.success(),
        "aw-cli --help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let help = String::from_utf8(output.stdout).expect("help output should be UTF-8");
    assert!(
        help.contains("aw-cli controller --config FILE [--once]"),
        "help should list the controller entrypoint, got:\n{help}"
    );
    assert!(
        help.contains("aw-cli --identity"),
        "help should document the sibling identity probe used by the GUI, got:\n{help}"
    );
    assert!(help.contains("aw-cli ssh-gateway --config FILE --request FILE --evidence-dir DIR"), "missing SSH Gateway CLI: {help}");
    assert!(
        help.contains("aw-cli owner-exec --config FILE"),
        "help should document the native owner execution entrypoint, got:\n{help}"
    );
}
