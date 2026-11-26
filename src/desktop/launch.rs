use std::path::{Path, PathBuf};
use anyhow::ensure;
use agentranch::{config::Mode, worker_identity::{validate_worker, worker_executable}};

#[derive(Debug)]
pub struct Launch {
    pub mode: Mode,
    pub state_dir: PathBuf,
    pub worker_exe: PathBuf,
    pub gateway_key_stdin: bool,
}
impl Launch {
    pub fn parse<'a>(args: impl IntoIterator<Item = &'a str>, executable: &Path, data_home: &Path) -> anyhow::Result<Self> {
        let mut args = args.into_iter();
        let mut mode = None;
        let mut gateway_key_stdin = false;
        let mut state = None;
        while let Some(arg) = args.next() {
            match arg {
                "--real" => {
                    ensure!(mode.is_none(), "duplicate or mutually exclusive mode option: {arg}");
                    mode = Some(Mode::Real);
                }
                "--local" => {
                    ensure!(mode.is_none(), "duplicate or mutually exclusive mode option: {arg}");
                    mode = Some(Mode::Local);
                }
                "--gateway-key-stdin" if !gateway_key_stdin => gateway_key_stdin = true,
                "--state-dir" if state.is_none() => {
                    let value = args.next().filter(|p| !p.is_empty() && !p.starts_with("--"))
                        .ok_or_else(|| anyhow::anyhow!("--state-dir requires a nonempty path"))?;
                    state = Some(PathBuf::from(value));
                }
                _ => anyhow::bail!("Unsupported or duplicate option: {arg}. Use --real, --local, --gateway-key-stdin and/or --state-dir PATH."),
            }
        }
        let mode = mode.unwrap_or(Mode::Real);
        ensure!(mode == Mode::Real || !gateway_key_stdin,"--gateway-key-stdin requires --real");
        let state_dir = state.unwrap_or_else(|| data_home.join("agentranch").join(match mode { Mode::Real => "real", Mode::Local => "local" }));
        let worker_exe = worker_executable(executable)?;
        if mode == Mode::Local {
            validate_worker(&worker_exe)?;
        }
        Ok(Self { mode, state_dir, worker_exe, gateway_key_stdin })
    }
}
