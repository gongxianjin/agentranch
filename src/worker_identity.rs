use std::{path::{Path, PathBuf}, process::Command};
use anyhow::ensure;
use serde_json::Value;

const WORKER_BINARY: &str = if cfg!(windows) { "aw-cli.exe" } else { "aw-cli" };

pub fn worker_executable(executable: &Path) -> anyhow::Result<PathBuf> {
    let parent = executable.parent().ok_or_else(|| anyhow::anyhow!("application executable has no parent directory"))?;
    Ok(parent.join(WORKER_BINARY))
}

pub fn validate_worker(worker_exe: &Path) -> anyhow::Result<()> {
    ensure!(worker_exe.is_file(), "sibling worker executable is missing");
    let output = Command::new(worker_exe).arg("--identity").output()
        .map_err(|_| anyhow::anyhow!("sibling worker identity could not be read"))?;
    ensure!(output.status.success(), "sibling worker identity could not be read");
    let actual: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| anyhow::anyhow!("sibling worker identity mismatch"))?;
    let expected = serde_json::json!({
        "binary": WORKER_BINARY.trim_end_matches(".exe"),
        "package": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
    });
    ensure!(actual == expected, "sibling worker identity mismatch");
    Ok(())
}
