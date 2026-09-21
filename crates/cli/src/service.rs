use crate::config::{
    cache_root, config_root, home_dir, runtime_path, set_file_mode, state_root, sync_directory,
    systemd_registration_dir, systemd_user_dir,
};
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::Builder;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceStatus {
    Running,
    Starting,
    Failed,
    Stopped,
    NotInstalled,
    Unknown,
}

fn same_directory(left: &Path, right: &Path) -> Result<bool> {
    Ok(left.canonicalize()? == right.canonicalize()?)
}

impl ServiceStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Starting => "starting",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::NotInstalled => "not installed",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitState {
    pub status: ServiceStatus,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub unit_file_state: String,
}

pub trait ServiceManager {
    fn available(&self) -> Result<()>;
    fn unit_state(&self, unit: &str) -> Result<UnitState>;
    fn unit_text(&self, unit: &str) -> Result<Option<String>>;
    fn install_unit(&self, unit: &str, contents: &str) -> Result<()>;
    fn remove_unit(&self, unit: &str) -> Result<()>;
    fn daemon_reload(&self) -> Result<()>;
    fn enable_start(&self, unit: &str) -> Result<()>;
    fn enable(&self, unit: &str) -> Result<()>;
    fn disable(&self, unit: &str) -> Result<()>;
    fn start(&self, unit: &str) -> Result<()>;
    fn stop(&self, unit: &str) -> Result<()>;
    fn restart(&self, unit: &str) -> Result<()>;
    fn logs(&self, unit: &str, lines: usize, follow: bool) -> Result<String>;
}

#[derive(Clone, Debug, Default)]
pub struct SystemdManager;

impl SystemdManager {
    fn run_systemctl(&self, args: &[&str]) -> Result<Output> {
        let output = Command::new("systemctl")
            .args(["--user"])
            .args(args)
            .output()
            .context("unable to run systemctl --user")?;
        Ok(output)
    }

    fn unit_path(&self, unit: &str) -> Result<PathBuf> {
        Ok(systemd_user_dir()?.join(unit))
    }
}

impl ServiceManager for SystemdManager {
    fn available(&self) -> Result<()> {
        let output = self.run_systemctl(&["show-environment"])?;
        if output.status.success() {
            Ok(())
        } else {
            bail!(
                "systemd user manager is unavailable: {}",
                output_error(&output)
            )
        }
    }

    fn unit_state(&self, unit: &str) -> Result<UnitState> {
        let output = self.run_systemctl(&[
            "show",
            unit,
            "--no-pager",
            "--property=LoadState,ActiveState,SubState,UnitFileState",
        ])?;
        if !output.status.success() && output.stdout.is_empty() {
            let error = output_error(&output);
            if error.contains("not found") || error.contains("could not be found") {
                return Ok(not_installed_state());
            }
            bail!("could not inspect service {unit}: {error}");
        }
        let fields = parse_show_fields(&output.stdout);
        let load_state = fields.get("LoadState").cloned().unwrap_or_default();
        let active_state = fields.get("ActiveState").cloned().unwrap_or_default();
        let sub_state = fields.get("SubState").cloned().unwrap_or_default();
        let unit_file_state = fields.get("UnitFileState").cloned().unwrap_or_default();
        let status = map_service_status(&load_state, &active_state, &sub_state);
        Ok(UnitState {
            status,
            load_state,
            active_state,
            sub_state,
            unit_file_state,
        })
    }

    fn unit_text(&self, unit: &str) -> Result<Option<String>> {
        let path = self.unit_path(unit)?;
        if path.is_file() {
            return Ok(Some(fs::read_to_string(path)?));
        }
        let output = self.run_systemctl(&["cat", unit, "--no-pager"])?;
        if !output.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        Ok((!text.trim().is_empty()).then_some(text))
    }

    fn install_unit(&self, unit: &str, contents: &str) -> Result<()> {
        let directory = systemd_user_dir()?;
        fs::create_dir_all(&directory)
            .with_context(|| format!("unable to create {}", directory.display()))?;
        let path = directory.join(unit);
        let mut temporary = Builder::new()
            .prefix(".unit-")
            .tempfile_in(&directory)
            .with_context(|| {
                format!("unable to create temporary unit in {}", directory.display())
            })?;
        set_file_mode(temporary.as_file(), 0o644)?;
        temporary.write_all(contents.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("unable to install {}", path.display()))?;
        sync_directory(&directory)?;

        let registration_directory = systemd_registration_dir()?;
        fs::create_dir_all(&registration_directory).with_context(|| {
            format!(
                "unable to create systemd registration directory {}",
                registration_directory.display()
            )
        })?;
        if same_directory(&directory, &registration_directory)? {
            return Ok(());
        }
        let registration = registration_directory.join(unit);
        let temporary_registration =
            registration_directory.join(format!(".{unit}-link-{}", std::process::id()));
        if temporary_registration.symlink_metadata().is_ok() {
            fs::remove_file(&temporary_registration)?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&path, &temporary_registration)?;
        #[cfg(not(unix))]
        fs::copy(&path, &temporary_registration)?;
        fs::rename(&temporary_registration, &registration)?;
        sync_directory(&registration_directory)?;
        Ok(())
    }

    fn remove_unit(&self, unit: &str) -> Result<()> {
        let path = self.unit_path(unit)?;
        if path.exists() || path.symlink_metadata().is_ok() {
            fs::remove_file(path)?;
        }
        let registration = systemd_registration_dir()?.join(unit);
        if registration.exists() || registration.symlink_metadata().is_ok() {
            fs::remove_file(registration)?;
        }
        Ok(())
    }

    fn daemon_reload(&self) -> Result<()> {
        let output = self.run_systemctl(&["daemon-reload"])?;
        command_success(&output, "reload the systemd user manager")
    }

    fn enable_start(&self, unit: &str) -> Result<()> {
        let output = self.run_systemctl(&["enable", "--now", unit])?;
        command_success(&output, &format!("enable and start {unit}"))
    }

    fn enable(&self, unit: &str) -> Result<()> {
        let output = self.run_systemctl(&["enable", unit])?;
        command_success(&output, &format!("enable {unit}"))
    }

    fn disable(&self, unit: &str) -> Result<()> {
        let output = self.run_systemctl(&["disable", unit])?;
        command_success(&output, &format!("disable {unit}"))
    }

    fn start(&self, unit: &str) -> Result<()> {
        let output = self.run_systemctl(&["start", unit])?;
        command_success(&output, &format!("start {unit}"))
    }

    fn stop(&self, unit: &str) -> Result<()> {
        let output = self.run_systemctl(&["stop", unit])?;
        command_success(&output, &format!("stop {unit}"))
    }

    fn restart(&self, unit: &str) -> Result<()> {
        let output = self.run_systemctl(&["restart", unit])?;
        command_success(&output, &format!("restart {unit}"))
    }

    fn logs(&self, unit: &str, lines: usize, follow: bool) -> Result<String> {
        let line_count = lines.clamp(1, 10_000).to_string();
        if follow {
            let status = Command::new("journalctl")
                .args([
                    "--user",
                    "--unit",
                    unit,
                    "--no-pager",
                    "--lines",
                    &line_count,
                    "--follow",
                ])
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .context("unable to run journalctl --user")?;
            if status.success() {
                Ok(String::new())
            } else {
                bail!("journalctl exited with {status}")
            }
        } else {
            let output = Command::new("journalctl")
                .args([
                    "--user",
                    "--unit",
                    unit,
                    "--no-pager",
                    "--lines",
                    &line_count,
                ])
                .output()
                .context("unable to run journalctl --user")?;
            command_success(&output, &format!("read logs for {unit}"))?;
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
    }
}

pub fn backend_unit(binary: &Path) -> Result<String> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    let home = home_dir()?;
    backend_unit_with_path(
        binary,
        &home,
        &config_root()?,
        &state_root()?,
        &cache_root()?,
        &path,
    )
}

fn backend_unit_with_path(
    binary: &Path,
    home: &Path,
    config_root: &Path,
    state_root: &Path,
    cache_root: &Path,
    path: &std::ffi::OsStr,
) -> Result<String> {
    let path = runtime_path(home, path)?;
    // Environment= uses systemd quoting and percent specifiers, not shell expansion.
    let environment = format!("PATH={path}")
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    let home_environment = format!("HOME={}", home.display())
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    let config_environment = format!("XDG_CONFIG_HOME={}", config_root.display())
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    let state_environment = format!("XDG_STATE_HOME={}", state_root.display())
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    let cache_environment = format!("XDG_CACHE_HOME={}", cache_root.display())
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    let binary = binary
        .canonicalize()
        .with_context(|| format!("Codex Connect binary does not exist: {}", binary.display()))?;
    let working_directory = binary.parent().unwrap_or(Path::new("/"));
    Ok(format!(
        "[Unit]\nDescription=Codex Connect backend\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nEnvironment=\"{environment}\"\nEnvironment=\"{home_environment}\"\nEnvironment=\"{config_environment}\"\nEnvironment=\"{state_environment}\"\nEnvironment=\"{cache_environment}\"\nWorkingDirectory={}\nExecStartPre=/usr/bin/test -x {}\nExecStart={} run-backend\nRestart=on-failure\nRestartSec=3\n\n[Install]\nWantedBy=default.target\n",
        systemd_arg(working_directory),
        systemd_arg(&binary),
        systemd_arg(&binary),
    ))
}

fn parse_show_fields(output: &[u8]) -> std::collections::BTreeMap<String, String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn map_service_status(load_state: &str, active_state: &str, sub_state: &str) -> ServiceStatus {
    if load_state == "not-found" {
        return ServiceStatus::NotInstalled;
    }
    match active_state {
        "active" if sub_state == "running" || sub_state.is_empty() => ServiceStatus::Running,
        "activating" | "reloading" => ServiceStatus::Starting,
        "failed" => ServiceStatus::Failed,
        "inactive" | "deactivating" => ServiceStatus::Stopped,
        _ => ServiceStatus::Unknown,
    }
}

fn not_installed_state() -> UnitState {
    UnitState {
        status: ServiceStatus::NotInstalled,
        load_state: "not-found".to_string(),
        active_state: "inactive".to_string(),
        sub_state: "dead".to_string(),
        unit_file_state: "disabled".to_string(),
    }
}

fn command_success(output: &Output, operation: &str) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        bail!("could not {operation}: {}", output_error(output))
    }
}

fn output_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        stdout
    } else {
        output.status.to_string()
    }
}

fn systemd_arg(path: &Path) -> String {
    systemd_value(&path.display().to_string())
}

fn systemd_value(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '\\' => ['\\', '\\'].into_iter().collect::<Vec<_>>(),
            ' ' | '\t' | '"' => vec!['\\', character],
            _ => vec![character],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_path_keeps_absolute_entries_in_order_and_quotes_systemd_environment() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("codex-connect");
        fs::write(&binary, b"binary").unwrap();
        let local_bin = directory.path().join(".local/bin");
        let config = directory.path().join(".config");
        let state = directory.path().join(".local/state");
        let cache = directory.path().join(".cache");
        let path = std::ffi::OsStr::new(
            ":relative:.:/home/operator/.cargo/bin:/opt/tool chain:/usr/bin:/bin:/usr/bin:/opt/100%/$tools/\"quoted\"/back\\slash:",
        );
        let unit = backend_unit_with_path(&binary, directory.path(), &config, &state, &cache, path)
            .unwrap();
        assert!(unit.contains(&format!(
            r#"Environment="PATH={}:{}:"#,
            local_bin.display(),
            directory.path().join(".cargo/bin").display()
        )));
        assert!(unit.contains("/home/operator/.cargo/bin"));
        assert!(unit.contains("/opt/tool chain"));
        assert!(unit.contains("/usr/bin"));
        assert!(unit.contains("/opt/100%%/$tools/"));
        assert!(unit.contains("quoted"));
        assert!(unit.contains("back"));
        assert!(unit.contains("Environment=\"XDG_CONFIG_HOME="));
        assert!(unit.contains("Environment=\"XDG_STATE_HOME="));
        assert!(unit.contains("Environment=\"XDG_CACHE_HOME="));
        assert!(unit.contains("Environment=\"HOME="));
        assert!(!unit.contains("GH_CONFIG_DIR"));
        assert!(!unit.contains("relative"));
        assert!(!unit.contains(".codex/tmp"));
        assert!(!unit.contains("sh -"));
        assert!(unit.contains(&format!("ExecStart={} run-backend", binary.display())));
        assert!(runtime_path(directory.path(), std::ffi::OsStr::new("/bin:/bad\nentry")).is_err());
    }

    #[test]
    fn maps_systemd_states_to_operator_labels() {
        assert_eq!(
            map_service_status("loaded", "active", "running"),
            ServiceStatus::Running
        );
        assert_eq!(
            map_service_status("loaded", "failed", "failed"),
            ServiceStatus::Failed
        );
        assert_eq!(
            map_service_status("not-found", "inactive", "dead"),
            ServiceStatus::NotInstalled
        );
    }

    #[cfg(unix)]
    #[test]
    fn same_directory_detects_symlink_equivalent_registration_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let canonical = temporary.path().join("canonical");
        let alias = temporary.path().join("alias");
        fs::create_dir(&canonical).unwrap();
        std::os::unix::fs::symlink(&canonical, &alias).unwrap();
        assert!(same_directory(&canonical, &alias).unwrap());
    }

    #[test]
    fn generated_service_reads_backend_configuration_at_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("codex-connect");
        std::fs::write(&binary, b"binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let unit = backend_unit(&binary).unwrap();
        assert!(unit.contains("run-backend"));
        assert!(!unit.contains("--default-cwd"));
        assert!(!unit.contains("127.0.0.1:8767"));
        assert!(!unit.contains("/opt/codex"));
    }

    #[test]
    fn backend_unit_does_not_block_sudo() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("codex-connect");
        std::fs::write(&binary, b"binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(!backend_unit(&binary).unwrap().contains("NoNewPrivileges"));
    }
}
