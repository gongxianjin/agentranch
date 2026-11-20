//! Local real subprocess I/O only. Never starts SSH or a Linux owner.
use agentranch::core::{live_output::LiveOutput, ssh_gateway::{exchange_process, exchange_process_stream}};
use std::{io::{Read, Write}, process::Command, sync::{Arc, Mutex}, time::{Duration, Instant}};

#[test]
fn inherited_pipe_fixture() {
    if std::env::var("AW_GATEWAY_INHERITED_PIPE").is_ok() {std::thread::sleep(Duration::from_secs(10));}
}
#[test]
fn ipc_child_fixture() {
    let Ok(mode)=std::env::var("AW_GATEWAY_IPC_FIXTURE") else{return};
    if mode=="no-read" {
        std::thread::sleep(Duration::from_secs(10));
        std::process::exit(0);
    }
    let mut request=Vec::new();std::io::stdin().read_to_end(&mut request).unwrap();
    assert_eq!(request,b"signed-start\n");
    if mode=="inherited" {
        Command::new(std::env::current_exe().unwrap()).args(["--exact","inherited_pipe_fixture","--nocapture"])
            .env("AW_GATEWAY_INHERITED_PIPE","1").spawn().unwrap();
    } else if mode=="flood" {
        std::io::stdout().write_all(&vec![b'o';131072]).unwrap();
        std::io::stderr().write_all(&vec![b'e';131072]).unwrap();
    } else if mode=="exact" {
        std::io::stdout().write_all(b"ok\n").unwrap();
        std::io::stderr().write_all(&vec![b'e';65536]).unwrap();
    } else if mode == "live-large" {
        use agentranch::core::live_output::write_live;
        write_live(&mut std::io::stderr(), &LiveOutput::Delta("中".repeat(30000))).unwrap();
        std::io::stderr().write_all(b"ordinary diagnostic\n").unwrap();
        std::io::stdout().write_all(b"done\n").unwrap();
    } else if mode == "live-incomplete" {
        std::io::stderr().write_all(b"AO-LIVE {\"kind\":\"delta\",\"text\":\"partial\"}\nAO-LIVE {\"kind\":").unwrap();
    } else if mode == "live-malformed" {
        std::io::stderr().write_all(b"AO-LIVE not-json\n").unwrap();
        std::io::stderr().flush().unwrap();
        std::io::stdout().write_all(b"done\n").unwrap();
    } else if mode == "raw-diagnostic-barrier" {
        std::io::stderr().write_all(b"diagnostic without newline").unwrap();
        std::io::stderr().flush().unwrap();
        std::thread::sleep(Duration::from_secs(10));
    } else if mode == "live-barrier" {
        std::io::stderr().write_all(b"AO-LIVE {\"kind\":\"delta\",\"text\":\"first\"}\n").unwrap();
        std::io::stderr().flush().unwrap();
        while !std::path::Path::new(std::env::var("AW_LIVE_RELEASE").unwrap().as_str()).exists() { std::thread::sleep(Duration::from_millis(5)); }
        std::io::stdout().write_all(b"done\n").unwrap();
    } else {
        std::io::stdout().write_all(b"launch-prefix\n").unwrap();std::io::stdout().flush().unwrap();
        std::io::stderr().write_all(b"diagnostic\n").unwrap();std::io::stderr().flush().unwrap();
        std::thread::sleep(Duration::from_secs(10));
    }
    std::process::exit(0);
}
fn command(mode:&str)->Command {
    let mut command=Command::new(std::env::current_exe().unwrap());
    command.args(["--exact","ipc_child_fixture","--nocapture"]).env("AW_GATEWAY_IPC_FIXTURE",mode);command
}
#[test]
fn live_callback_arrives_before_child_exit() {
    let root = tempfile::tempdir().unwrap();
    let release = root.path().join("release");
    let mut command = command("live-barrier");
    command.env("AW_LIVE_RELEASE", &release);
    let seen = Arc::new(Mutex::new(Vec::<LiveOutput>::new()));
    let observed = seen.clone();
    let output = exchange_process_stream(&mut command, b"signed-start\n", Duration::from_secs(3), &mut |event| {
        observed.lock().unwrap().push(event);
        std::fs::write(&release, b"ok").unwrap();
    }).unwrap();
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(seen.lock().unwrap().as_slice(), &[LiveOutput::Delta("first".into())]);
    assert!(output.stdout.ends_with(b"done\n"), "final child output missing: {:?}", output.stdout);
}
#[test]
fn live_text_over_diagnostic_budget_keeps_actual_text_and_ordinary_stderr() {
    let mut text = String::new();
    let output = exchange_process_stream(&mut command("live-large"), b"signed-start\n", Duration::from_secs(3), &mut |event| {
        if let LiveOutput::Delta(delta) = event {text.push_str(&delta);}
    }).unwrap();
    assert_eq!(text, "中".repeat(30000));
    assert_eq!(output.stderr, b"ordinary diagnostic\n");
    assert_eq!(output.failure, None);
}
#[test]
fn incomplete_live_frame_reports_error_without_erasing_prior_delta() {
    let mut events = Vec::new();
    let output = exchange_process_stream(&mut command("live-incomplete"), b"signed-start\n", Duration::from_secs(3), &mut |event| events.push(event)).unwrap();
    assert_eq!(events.first(), Some(&LiveOutput::Delta("partial".into())));
    assert!(events.iter().any(|event| matches!(event, LiveOutput::Error(_))), "partial EOF must reach UI");
    assert_eq!(output.stderr, b"AO-LIVE {\"kind\":");
}
#[test]
fn malformed_live_frame_is_retained_as_diagnostic_while_ui_gets_error() {
    let mut events = Vec::new();
    let output = exchange_process_stream(&mut command("live-malformed"), b"signed-start\n", Duration::from_secs(3), &mut |event| events.push(event)).unwrap();
    assert!(events.iter().any(|event| matches!(event, LiveOutput::Error(_))), "malformed live frame must reach UI");
    assert_eq!(output.stderr, b"AO-LIVE not-json\n");
}
#[test]
fn unterminated_ordinary_diagnostic_survives_transport_timeout() {
    let output = exchange_process(&mut command("raw-diagnostic-barrier"), b"signed-start\n", Duration::from_millis(200)).unwrap();
    assert_eq!(output.stderr, b"diagnostic without newline");
}
#[test]
fn continuously_drains_both_pipes_without_wait_before_read_deadlock() {
    let mut command=command("flood");
    let output=exchange_process(&mut command,b"signed-start\n",Duration::from_secs(3)).unwrap();
    assert_eq!(output.exit_code,Some(0),"{:?}",output.failure);
    assert!(output.stdout.windows(100).any(|window|window==[b'o';100]));
    assert!(output.stderr.windows(100).any(|window|window==[b'e';100]));
    assert!(output.failure.is_some(),"bounded stderr capture must report overflow");
    assert!(output.stdout.len()<=1024*1024 && output.stderr.len()<=64*1024);
}
#[test]
fn timeout_returns_real_prefixes_and_waits_for_local_process() {
    let started=Instant::now();let mut command=command("timeout");
    let output=exchange_process(&mut command,b"signed-start\n",Duration::from_millis(400)).unwrap();
    assert!(started.elapsed()<Duration::from_secs(3));
    assert!(String::from_utf8_lossy(&output.stdout).contains("launch-prefix"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("diagnostic"));
    assert!(output.failure.unwrap().contains("timeout"));
    assert!(output.exit_code.is_some(),"killed subprocess must be directly waited");
}
#[test]
fn exact_stderr_limit_is_not_an_overflow() {
    let output=exchange_process(&mut command("exact"),b"signed-start\n",Duration::from_secs(2)).unwrap();
    assert_eq!(output.stderr.len(),65536);
    assert_eq!(output.failure,None,"exact capacity must be accepted");
}
#[test]
fn inherited_pipe_eof_is_bounded_by_the_same_deadline() {
    let started=Instant::now();
    let output=exchange_process(&mut command("inherited"),b"signed-start\n",Duration::from_millis(300)).unwrap();
    assert!(started.elapsed()<Duration::from_secs(2),"inherited pipe exceeded deadline");
    assert_eq!(output.exit_code,Some(0));
    assert!(output.failure.unwrap().contains("timeout"));
}
#[test]
fn blocked_stdin_is_bounded_by_the_same_deadline() {
    let started=Instant::now();
    let output=exchange_process(&mut command("no-read"),&vec![b'x';1024*1024],Duration::from_millis(300));
    assert!(started.elapsed()<Duration::from_secs(2),"stdin write escaped deadline");
    assert!(output.unwrap().failure.unwrap().contains("timeout"));
}
