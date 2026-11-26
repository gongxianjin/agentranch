#[path = "support/daemon_temp.rs"]
mod daemon_temp;

use agentranch::worker_identity::{validate_worker, worker_executable};
use std::{path::{Path, PathBuf}, process::Command};

/// `temp_root()` canonicalizes to a `\\?\` verbatim path, which the MinGW
/// linker behind `rustc` cannot consume. Strip the prefix for fixture
/// compilation only; the assertions still exercise the verbatim spelling.
fn native(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_string());
        }
    }
    path.to_path_buf()
}

fn worker_name() -> &'static str {
    if cfg!(windows) { "aw-cli.exe" } else { "aw-cli" }
}

#[test]
fn worker_executable_uses_sibling_name() {
    let root = daemon_temp::temp_root();
    let app = root.path().join("agentranchd.exe");
    assert_eq!(worker_executable(&app).unwrap(), root.path().join(worker_name()));
}

#[test]
fn validate_worker_accepts_matching_identity() {
    let root = daemon_temp::temp_root();
    let worker = root.path().join(worker_name());
    std::fs::copy(env!("CARGO_BIN_EXE_aw-cli"), &worker).unwrap();
    validate_worker(&worker).unwrap();
}

#[test]
fn validate_worker_rejects_wrong_package_or_version() {
    for (package, version) in [
        ("wrong-package", env!("CARGO_PKG_VERSION")),
        (env!("CARGO_PKG_NAME"), "999.999.999"),
    ] {
        let root = daemon_temp::temp_root();
        let worker = native(&root.path().join(worker_name()));
        let identity = serde_json::json!({"binary": "aw-cli", "package": package, "version": version});
        let source = native(&root.path().join("fixture-worker.rs"));
        let code = format!(
            "fn main() {{ assert_eq!(std::env::args().skip(1).collect::<Vec<_>>(), [\"--identity\"]); println!(\"{{}}\", {:?}); }}",
            identity.to_string(),
        );
        std::fs::write(&source, code).unwrap();
        let status = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args([Path::new("--edition=2024"), &source, Path::new("-o"), &worker])
            .status().unwrap();
        assert!(status.success(), "fixture compilation failed: {status}");
        assert_eq!(validate_worker(&worker).unwrap_err().to_string(), "sibling worker identity mismatch");
    }
}
