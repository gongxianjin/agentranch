use agentranch::{
    api::lifecycle::{
        DaemonArgs, DaemonRuntime, EndpointFileGuard, create_isolated_state_dir,
        validate_fresh_state_dir,
    },
    config::Mode,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

#[path = "support/daemon_temp.rs"]
mod daemon_temp;
use daemon_temp::temp_root;

fn absolute_state(root: &Path) -> PathBuf {
    root.join("state")
}

fn daemon_exe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agentranchd"))
}

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[test]
fn daemon_args_default_to_real_and_ephemeral_port() {
    let root = temp_root();
    let args = DaemonArgs::parse(["--state-dir", absolute_state(root.path()).to_str().unwrap()]).unwrap();
    assert_eq!(args.mode, Mode::Real);
    assert_eq!(args.port, 0);
}

#[test]
fn daemon_args_accept_explicit_local_and_reject_two_modes() {
    let root = temp_root();
    let state = absolute_state(root.path());
    let args = DaemonArgs::parse(["--local", "--port", "43123", "--state-dir", state.to_str().unwrap()]).unwrap();
    assert_eq!(args.mode, Mode::Local);
    assert_eq!(args.port, 43123);
    let error = DaemonArgs::parse(["--real", "--local", "--state-dir", state.to_str().unwrap()]).unwrap_err();
    assert!(error.to_string().contains("mutually exclusive"));
}

#[test]
fn daemon_args_reject_invalid_port_and_missing_or_relative_state() {
    let root = temp_root();
    let absolute = absolute_state(root.path());
    assert!(DaemonArgs::parse(["--port", "65536", "--state-dir", absolute.to_str().unwrap()]).is_err());
    assert!(DaemonArgs::parse(["--state-dir", "relative-state"]).is_err());
    assert!(DaemonArgs::parse(["--local"]).is_err());
}

#[test]
fn parser_does_not_echo_unknown_option_or_positional_input() {
    for (input, diagnostic) in [
        ("--synthetic-lifecycle-canary", "unknown argument"),
        ("synthetic-lifecycle-canary", "unexpected argument"),
    ] {
        assert_eq!(DaemonArgs::parse([input]).unwrap_err().to_string(), diagnostic);
    }
}

#[test]
fn startup_stderr_is_fixed_and_contains_no_untrusted_input() {
    const CANARY: &str = "synthetic-lifecycle-canary";
    let root = temp_root();
    let existing = root.path().join(CANARY);
    std::fs::create_dir(&existing).unwrap();
    let invalid = daemon_exe().arg(format!("--{CANARY}")).output().unwrap();
    let occupied = daemon_exe().arg("--state-dir").arg(&existing).output().unwrap();
    for (output, category) in [(invalid, "invalid_arguments"), (occupied, "startup_preparation_failed")] {
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains(CANARY), "startup diagnostic echoed untrusted data");
        assert_eq!(stderr.trim_end(), format!("agentranchd startup failed: {category}"));
    }
    assert!(existing.read_dir().unwrap().next().is_none());
}

#[cfg(windows)]
#[test]
fn non_unicode_argument_has_a_fixed_startup_diagnostic() {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};

    let malformed = OsString::from_wide(&[b'x' as u16, 0xd800]);
    let output = daemon_exe().arg(malformed).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(String::from_utf8(output.stderr).unwrap().trim_end(), "agentranchd startup failed: invalid_arguments");
}

#[test]
fn fresh_state_validation_requires_nonexistent_absolute_directory() {
    let root = temp_root();
    let state = absolute_state(root.path());
    validate_fresh_state_dir(&state).unwrap();
    create_isolated_state_dir(&state).unwrap();
    assert!(validate_fresh_state_dir(&state).is_err());
    assert!(create_isolated_state_dir(&state).is_err());
    assert!(validate_fresh_state_dir(Path::new("relative-state")).is_err());
}

#[cfg(windows)]
#[test]
fn fresh_state_validation_rejects_junction_parent() {
    let root = temp_root();
    let real_parent = root.path().join("real");
    let linked_parent = root.path().join("linked");
    std::fs::create_dir(&real_parent).unwrap();
    let output = Command::new("cmd.exe")
        .args(["/d", "/C", "mklink", "/J"])
        .arg(&linked_parent)
        .arg(&real_parent)
        .output().unwrap();
    assert!(output.status.success(), "junction fixture creation failed: {}", output.status);
    assert!(validate_fresh_state_dir(&linked_parent.join("state")).is_err());
    assert!(!real_parent.join("state").exists());
}

#[test]
fn port_conflict_does_not_create_state_directory() {
    let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = occupied.local_addr().unwrap().port();
    let root = temp_root();
    let state = absolute_state(root.path());
    let args = DaemonArgs { state_dir: state.clone(), port, mode: Mode::Real };
    assert!(DaemonRuntime::start(args).is_err());
    assert!(!state.exists());
}

/// Task 8 replaced the placeholder that exited with `runtime_not_ready`: the
/// assembled daemon serves before it publishes discovery, so an advertised
/// endpoint must already answer health with the same transport identity.
#[test]
fn published_discovery_implies_a_working_health_route_with_the_same_identity() {
    let root = temp_root();
    let state = absolute_state(root.path());
    let endpoint = state.join("api-endpoint.json");
    let mut child = TestChild(daemon_exe().arg("--state-dir").arg(&state).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !endpoint.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "daemon exited before publishing discovery"
        );
        assert!(Instant::now() < deadline, "assembled daemon did not publish discovery");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(state.join("imports").is_dir());
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&endpoint).unwrap()).unwrap();
    assert_eq!(metadata["schema"], "api-endpoint/1");
    let port: u16 = metadata["url"]
        .as_str()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let health = http_get_json(port, "/v1/health");
    assert_eq!(health["contract"], 1);
    assert_eq!(
        health["instance_id"], metadata["instance_id"],
        "discovery identity must match the instance clients read from health"
    );
}

fn http_get_json(port: u16, path: &str) -> serde_json::Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes())
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
    serde_json::from_slice(&raw[split + 4..]).unwrap()
}

#[test]
fn listener_and_state_are_not_enough_to_publish_discovery() {
    let root = temp_root();
    let state_dir = absolute_state(root.path());
    let mut runtime = DaemonRuntime::start(DaemonArgs { state_dir, port: 0, mode: Mode::Real }).unwrap();
    assert_eq!(runtime.publish_endpoint().unwrap_err().to_string(), "runtime is not ready to publish endpoint");
    assert!(!runtime.endpoint_path().exists());
}

#[test]
fn explicit_readiness_allows_publication_and_owner_removal() {
    let root = temp_root();
    let state = absolute_state(root.path());
    let args = DaemonArgs { state_dir: state.clone(), port: 0, mode: Mode::Real };
    let mut runtime = DaemonRuntime::start(args).unwrap();
    assert!(!runtime.endpoint_path().exists());
    runtime.mark_ready();
    runtime.publish_endpoint().unwrap();
    assert!(runtime.publish_endpoint().is_err());
    let endpoint_path = runtime.endpoint_path();
    let endpoint: serde_json::Value = serde_json::from_slice(&std::fs::read(&endpoint_path).unwrap()).unwrap();
    assert_eq!(endpoint["schema"], "api-endpoint/1");
    assert_eq!(endpoint["instance_id"].as_str().unwrap(), runtime.instance_id().to_string());
    assert_eq!(endpoint["url"].as_str().unwrap(), format!("http://127.0.0.1:{}", runtime.local_addr().port()));
    drop(runtime);
    assert!(!endpoint_path.exists());
}

#[test]
fn endpoint_guard_does_not_remove_a_replaced_instance() {
    let root = temp_root();
    let state = absolute_state(root.path());
    create_isolated_state_dir(&state).unwrap();
    let first = EndpointFileGuard::publish(&state, "http://127.0.0.1:1", 1).unwrap();
    let second = EndpointFileGuard::publish(&state, "http://127.0.0.1:2", 2).unwrap();
    drop(first);
    let endpoint: serde_json::Value = serde_json::from_slice(&std::fs::read(state.join("api-endpoint.json")).unwrap()).unwrap();
    assert_eq!(endpoint["pid"], 2);
    drop(second);
    assert!(!state.join("api-endpoint.json").exists());
}

#[test]
fn forgotten_endpoint_guard_leaves_stale_discovery_file() {
    let root = temp_root();
    let state = absolute_state(root.path());
    create_isolated_state_dir(&state).unwrap();
    let guard = EndpointFileGuard::publish(&state, "http://127.0.0.1:3", 3).unwrap();
    let endpoint_path = state.join("api-endpoint.json");
    std::mem::forget(guard);
    assert!(endpoint_path.exists());
}
