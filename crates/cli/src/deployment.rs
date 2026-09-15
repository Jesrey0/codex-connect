use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

const HANDOFF_DELAY: &str = "1s";

pub(crate) fn handoff(
    installed: &Path,
    expected_sha256: &str,
    activation_unit: &str,
    no_start: bool,
) -> Result<()> {
    let path = std::env::var("PATH").context("PATH is not set")?;
    let home = crate::config::home_dir()?;
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--collect",
        "--on-active",
        HANDOFF_DELAY,
        "--unit",
        activation_unit,
        "--setenv",
        &format!("PATH={path}"),
        "--setenv",
        &format!("HOME={}", home.display()),
    ]);
    for name in ["XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
        if let Some(value) = std::env::var_os(name) {
            command
                .arg("--setenv")
                .arg(format!("{name}={}", value.to_string_lossy()));
        }
    }
    command
        .arg(installed)
        .arg("activate-deployment")
        .arg("--expected-sha256")
        .arg(expected_sha256);
    if no_start {
        command.arg("--no-start");
    }
    let output = command
        .output()
        .context("unable to hand deployment activation to systemd")?;
    if !output.status.success() {
        bail!(
            "unable to queue deployment activation: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub(crate) fn unit_name(build_id: &str) -> String {
    format!("codex-connect-deploy-{build_id}")
}
