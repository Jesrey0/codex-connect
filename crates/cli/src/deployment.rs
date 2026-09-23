use crate::artifact::ArtifactIdentity;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const RECORD_VERSION: u32 = 2;
const OPERATION_ID_LENGTH: usize = 24;
const ACTIVATION_HANDOFF_DELAY: &str = "3s";

pub(crate) struct DeploymentLock {
    file: File,
}

pub(crate) fn verify_prepared_artifact(record: &DeploymentRecord) -> Result<()> {
    let (sha256, executable) = prepared_artifact(record)?;
    let actual = crate::artifact::for_path(&executable)
        .with_context(|| format!("prepared artifact is unavailable: {}", executable.display()))?;
    if actual.sha256 != sha256 {
        bail!(
            "prepared artifact hash mismatch: expected {sha256}, found {}",
            actual.sha256
        );
    }
    Ok(())
}

#[cfg(unix)]
impl Drop for DeploymentLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(crate) fn acquire_build_lock() -> Result<DeploymentLock> {
    acquire_lock(
        deployment_directory()?.join("build.lock"),
        "deployment build cache",
    )
}

pub(crate) fn acquire_activation_lock() -> Result<DeploymentLock> {
    acquire_lock(
        deployment_directory()?.join("activation.lock"),
        "deployment activation",
    )
}

pub(crate) fn prepare_service_unit(operation_id: &str) -> Result<String> {
    validate_operation_id(operation_id)?;
    Ok(format!("{}.service", prepare_unit_name(operation_id)))
}

pub(crate) fn activation_service_unit(operation_id: &str) -> Result<String> {
    validate_operation_id(operation_id)?;
    Ok(format!("{}.service", activation_unit_name(operation_id)))
}

pub(crate) fn activation_timer_unit(operation_id: &str) -> Result<String> {
    validate_operation_id(operation_id)?;
    Ok(format!("{}.timer", activation_unit_name(operation_id)))
}

fn acquire_operation_lock(operation_id: &str) -> Result<DeploymentLock> {
    validate_operation_id(operation_id)?;
    acquire_lock(
        deployment_directory()?.join(format!("{operation_id}.lock")),
        &format!("deployment {operation_id}"),
    )
}

fn acquire_lock(path: PathBuf, label: &str) -> Result<DeploymentLock> {
    let directory = path
        .parent()
        .context("deployment lock has no parent directory")?;
    fs::create_dir_all(directory)
        .with_context(|| format!("unable to create {}", directory.display()))?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("unable to open {label} lock {}", path.display()))?;
    crate::storage::set_file_mode(&file, 0o600)?;
    #[cfg(unix)]
    {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("unable to lock {label}"));
        }
    }
    Ok(DeploymentLock { file })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeploymentRecord {
    pub version: u32,
    pub operation_id: String,
    pub source: String,
    pub state: DeploymentState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    pub no_start: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DeploymentRecord {
    pub(crate) fn build_id(&self) -> Option<&str> {
        self.sha256.as_deref().map(|sha256| &sha256[..12])
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DeploymentState {
    Building,
    Prepared,
    ActivationQueued,
    Activating,
    Succeeded,
    Failed,
}

impl DeploymentState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Prepared => "prepared",
            Self::ActivationQueued => "activationQueued",
            Self::Activating => "activating",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

pub(crate) fn queue_prepare(source: &Path) -> Result<DeploymentRecord> {
    let source = source
        .canonicalize()
        .with_context(|| format!("unable to canonicalize source tree {}", source.display()))?;
    let operation_id = new_operation_id()?;
    let _lock = acquire_operation_lock(&operation_id)?;
    let mut record = DeploymentRecord {
        version: RECORD_VERSION,
        operation_id,
        source: source.display().to_string(),
        state: DeploymentState::Building,
        sha256: None,
        executable: None,
        no_start: false,
        error: None,
    };
    if record_path(&record.operation_id)?.exists() {
        bail!("deployment operation id collision: {}", record.operation_id);
    }
    save_unlocked(&record)?;

    let queued = (|| -> Result<()> {
        let launcher = crate::artifact::current()?.executable;
        let environment = launch_environment()?;
        let mut command = prepare_command(
            &record,
            &launcher,
            &environment.path,
            &environment.home,
            environment.xdg_config_home.as_os_str(),
            environment.xdg_state_home.as_os_str(),
            environment.xdg_cache_home.as_os_str(),
        );
        let output = command
            .output()
            .context("unable to hand deployment build to systemd")?;
        if !output.status.success() {
            bail!(
                "unable to queue deployment build: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    })();
    if let Err(error) = queued {
        record.state = DeploymentState::Failed;
        record.error = Some(error.to_string());
        let _ = save_unlocked(&record);
        return Err(error);
    }
    Ok(record)
}

pub(crate) fn mark_prepared(
    operation_id: &str,
    identity: &ArtifactIdentity,
    installed: &Path,
) -> Result<DeploymentRecord> {
    let _lock = acquire_operation_lock(operation_id)?;
    let mut record = load_unlocked(operation_id)?;
    if record.state != DeploymentState::Building {
        bail!(
            "deployment {operation_id} cannot become prepared from state {}",
            record.state.as_str()
        );
    }
    record.state = DeploymentState::Prepared;
    record.sha256 = Some(identity.sha256.clone());
    record.executable = Some(installed.display().to_string());
    record.error = None;
    save_unlocked(&record)?;
    Ok(record)
}

pub(crate) fn mark_failed_if_state(
    operation_id: &str,
    expected_states: &[DeploymentState],
    error: impl Into<String>,
) -> Result<DeploymentRecord> {
    let _lock = acquire_operation_lock(operation_id)?;
    let mut record = load_unlocked(operation_id)?;
    if !expected_states.contains(&record.state) {
        return Ok(record);
    }
    record.state = DeploymentState::Failed;
    record.error = Some(error.into());
    save_unlocked(&record)?;
    Ok(record)
}

pub(crate) fn load(operation_id: &str) -> Result<DeploymentRecord> {
    let path = record_path(operation_id)?;
    if !path.is_file() {
        bail!("deployment record not found for operation {operation_id}");
    }
    let _lock = acquire_operation_lock(operation_id)?;
    load_unlocked(operation_id)
}

fn load_unlocked(operation_id: &str) -> Result<DeploymentRecord> {
    let path = record_path(operation_id)?;
    let bytes = fs::read(&path)
        .with_context(|| format!("deployment record not found for operation {operation_id}"))?;
    let record: DeploymentRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid deployment record at {}", path.display()))?;
    if record.version != RECORD_VERSION {
        bail!(
            "unsupported deployment record version {} for operation {operation_id}",
            record.version
        );
    }
    if record.operation_id != operation_id {
        bail!(
            "deployment record identity mismatch: requested {operation_id}, found {}",
            record.operation_id
        );
    }
    validate_record(&record)?;
    Ok(record)
}

fn save_unlocked(record: &DeploymentRecord) -> Result<()> {
    validate_record(record)?;
    let path = deployment_directory()?.join(format!("{}.json", record.operation_id));
    let mut bytes = serde_json::to_vec_pretty(record)?;
    bytes.push(b'\n');
    crate::storage::atomic_write(&path, ".deployment-", 0o600, &bytes)
        .with_context(|| format!("unable to persist {}", path.display()))
}

pub(crate) fn queue_activation(
    operation_id: &str,
    no_start: bool,
) -> Result<(DeploymentRecord, bool)> {
    let _lock = acquire_operation_lock(operation_id)?;
    let mut record = load_unlocked(operation_id)?;
    match record.state {
        DeploymentState::Prepared => {}
        DeploymentState::ActivationQueued
        | DeploymentState::Activating
        | DeploymentState::Succeeded => {
            if record.no_start != no_start {
                bail!(
                    "deployment {} was already activated with no_start={}; refusing conflicting no_start={no_start}",
                    record.operation_id,
                    record.no_start
                );
            }
            return Ok((record, false));
        }
        DeploymentState::Building => {
            bail!(
                "deployment {} is still building; wait until it is prepared",
                record.operation_id
            );
        }
        DeploymentState::Failed => {
            bail!(
                "deployment {} failed; prepare a new deployment instead of reusing it",
                record.operation_id
            );
        }
    }

    verify_prepared_artifact(&record)?;

    // The per-operation lock serializes competing activation requests and readers, so the
    // queued intent can be durable before handoff without exposing an unowned intermediate
    // state. A failed handoff is rolled back to prepared while the same lock is still held.
    record.no_start = no_start;
    record.state = DeploymentState::ActivationQueued;
    record.error = None;
    save_unlocked(&record)?;
    let queued = (|| -> Result<()> {
        let environment = launch_environment()?;
        let mut command = activation_command(
            &record,
            no_start,
            &environment.path,
            &environment.home,
            environment.xdg_config_home.as_os_str(),
            environment.xdg_state_home.as_os_str(),
            environment.xdg_cache_home.as_os_str(),
        )?;
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
    })();
    if let Err(error) = queued {
        record.state = DeploymentState::Prepared;
        record.no_start = false;
        record.error = None;
        let _ = save_unlocked(&record);
        return Err(error);
    }
    Ok((record, true))
}

pub(crate) fn mark_activating(
    operation_id: &str,
    expected_sha256: &str,
    no_start: bool,
) -> Result<DeploymentRecord> {
    validate_sha256(expected_sha256)?;
    let _lock = acquire_operation_lock(operation_id)?;
    let mut record = load_unlocked(operation_id)?;
    let actual_sha = record
        .sha256
        .as_deref()
        .context("deployment record has no prepared SHA-256")?;
    if actual_sha != expected_sha256 {
        bail!("deployment record hash mismatch: expected {expected_sha256}, found {actual_sha}");
    }
    if record.state == DeploymentState::Activating && record.no_start == no_start {
        return Ok(record);
    }
    if record.state != DeploymentState::ActivationQueued {
        bail!(
            "deployment {operation_id} cannot become activating from state {}",
            record.state.as_str()
        );
    }
    if record.no_start != no_start {
        bail!(
            "deployment {operation_id} activation mode mismatch: queued no_start={}, worker no_start={no_start}",
            record.no_start
        );
    }
    record.state = DeploymentState::Activating;
    record.error = None;
    save_unlocked(&record)?;
    Ok(record)
}

pub(crate) fn mark_succeeded(
    operation_id: &str,
    expected_sha256: &str,
) -> Result<DeploymentRecord> {
    validate_sha256(expected_sha256)?;
    let _lock = acquire_operation_lock(operation_id)?;
    let mut record = load_unlocked(operation_id)?;
    verify_expected_sha(&record, expected_sha256)?;
    if record.state == DeploymentState::Succeeded {
        return Ok(record);
    }
    if record.state != DeploymentState::Activating {
        bail!(
            "deployment {operation_id} cannot become succeeded from state {}",
            record.state.as_str()
        );
    }
    record.state = DeploymentState::Succeeded;
    record.error = None;
    save_unlocked(&record)?;
    Ok(record)
}

pub(crate) fn mark_succeeded_if_activating(
    operation_id: &str,
    expected_sha256: &str,
) -> Result<DeploymentRecord> {
    validate_sha256(expected_sha256)?;
    let _lock = acquire_operation_lock(operation_id)?;
    let mut record = load_unlocked(operation_id)?;
    verify_expected_sha(&record, expected_sha256)?;
    if record.state != DeploymentState::Activating {
        return Ok(record);
    }
    record.state = DeploymentState::Succeeded;
    record.error = None;
    save_unlocked(&record)?;
    Ok(record)
}

pub(crate) fn activation_delay() -> &'static str {
    ACTIVATION_HANDOFF_DELAY
}

pub(crate) fn build_target(operation_id: &str) -> Result<PathBuf> {
    validate_operation_id(operation_id)?;
    Ok(crate::paths::cache_root()?.join("codex-connect/deploy/build"))
}

fn verify_expected_sha(record: &DeploymentRecord, expected_sha256: &str) -> Result<()> {
    let actual_sha = record
        .sha256
        .as_deref()
        .context("deployment record has no prepared SHA-256")?;
    if actual_sha != expected_sha256 {
        bail!("deployment record hash mismatch: expected {expected_sha256}, found {actual_sha}");
    }
    Ok(())
}

fn prepared_artifact(record: &DeploymentRecord) -> Result<(String, PathBuf)> {
    let sha256 = record
        .sha256
        .clone()
        .context("deployment record has no prepared SHA-256")?;
    validate_sha256(&sha256)?;
    let executable = record
        .executable
        .as_ref()
        .map(PathBuf::from)
        .context("deployment record has no prepared executable")?;
    Ok((sha256, executable))
}

fn prepare_command(
    record: &DeploymentRecord,
    launcher: &Path,
    path: &str,
    home: &Path,
    xdg_config_home: &OsStr,
    xdg_state_home: &OsStr,
    xdg_cache_home: &OsStr,
) -> Command {
    let mut command = systemd_command(
        prepare_unit_name(&record.operation_id),
        path,
        home,
        xdg_config_home,
        xdg_state_home,
        xdg_cache_home,
    );
    command
        .arg(launcher)
        .arg("prepare-deployment")
        .arg("--operation-id")
        .arg(&record.operation_id)
        .arg("--source")
        .arg(&record.source);
    command
}

fn activation_command(
    record: &DeploymentRecord,
    no_start: bool,
    path: &str,
    home: &Path,
    xdg_config_home: &OsStr,
    xdg_state_home: &OsStr,
    xdg_cache_home: &OsStr,
) -> Result<Command> {
    let (sha256, executable) = prepared_artifact(record)?;
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--collect",
        "--on-active",
        ACTIVATION_HANDOFF_DELAY,
        "--unit",
        &activation_unit_name(&record.operation_id),
        "--setenv",
        &format!("PATH={path}"),
        "--setenv",
        &format!("HOME={}", home.display()),
    ]);
    append_xdg_environment(
        &mut command,
        xdg_config_home,
        xdg_state_home,
        xdg_cache_home,
    );
    command
        .arg(executable)
        .arg("activate-deployment")
        .arg("--operation-id")
        .arg(&record.operation_id)
        .arg("--expected-sha256")
        .arg(sha256);
    if no_start {
        command.arg("--no-start");
    }
    Ok(command)
}

fn systemd_command(
    unit: String,
    path: &str,
    home: &Path,
    xdg_config_home: &OsStr,
    xdg_state_home: &OsStr,
    xdg_cache_home: &OsStr,
) -> Command {
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--collect",
        "--unit",
        &unit,
        "--setenv",
        &format!("PATH={path}"),
        "--setenv",
        &format!("HOME={}", home.display()),
    ]);
    append_xdg_environment(
        &mut command,
        xdg_config_home,
        xdg_state_home,
        xdg_cache_home,
    );
    command
}

fn append_xdg_environment(
    command: &mut Command,
    xdg_config_home: &OsStr,
    xdg_state_home: &OsStr,
    xdg_cache_home: &OsStr,
) {
    for (name, value) in [
        ("XDG_CONFIG_HOME", xdg_config_home),
        ("XDG_STATE_HOME", xdg_state_home),
        ("XDG_CACHE_HOME", xdg_cache_home),
    ] {
        command.arg("--setenv").arg(OsString::from(format!(
            "{name}={}",
            value.to_string_lossy()
        )));
    }
}

struct LaunchEnvironment {
    path: String,
    home: PathBuf,
    xdg_config_home: OsString,
    xdg_state_home: OsString,
    xdg_cache_home: OsString,
}

fn launch_environment() -> Result<LaunchEnvironment> {
    let home = crate::paths::home_dir()?;
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    Ok(LaunchEnvironment {
        path: crate::paths::runtime_path(&home, &path)?,
        home,
        xdg_config_home: crate::paths::config_root()?.into_os_string(),
        xdg_state_home: crate::paths::state_root()?.into_os_string(),
        xdg_cache_home: crate::paths::cache_root()?.into_os_string(),
    })
}

fn deployment_directory() -> Result<PathBuf> {
    Ok(crate::paths::state_root()?.join("codex-connect/deployments"))
}

fn record_path(operation_id: &str) -> Result<PathBuf> {
    validate_operation_id(operation_id)?;
    Ok(deployment_directory()?.join(format!("{operation_id}.json")))
}

fn new_operation_id() -> Result<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let nanos = u64::try_from(nanos).context("system time does not fit deployment id")?;
    Ok(format!("{nanos:016x}{:08x}", std::process::id()))
}

fn validate_record(record: &DeploymentRecord) -> Result<()> {
    if record.version != RECORD_VERSION {
        bail!("unsupported deployment record version {}", record.version);
    }
    validate_operation_id(&record.operation_id)?;
    let source = Path::new(&record.source);
    if record.source.is_empty() || !source.is_absolute() {
        bail!("deployment source must be a non-empty absolute path");
    }
    if let Some(sha256) = &record.sha256 {
        validate_sha256(sha256)?;
    }
    if record.sha256.is_some() != record.executable.is_some() {
        bail!("deployment artifact identity must be complete or absent");
    }
    let has_artifact = record.sha256.is_some();
    if let Some(executable) = &record.executable
        && !Path::new(executable).is_absolute()
    {
        bail!("deployment executable must be an absolute path");
    }
    match record.state {
        DeploymentState::Building => {
            if has_artifact || record.no_start || record.error.is_some() {
                bail!("building deployment has invalid artifact, no-start, or error fields");
            }
        }
        DeploymentState::Prepared => {
            if !has_artifact || record.no_start || record.error.is_some() {
                bail!("prepared deployment must have an artifact and no activation metadata");
            }
        }
        DeploymentState::ActivationQueued
        | DeploymentState::Activating
        | DeploymentState::Succeeded => {
            if !has_artifact || record.error.is_some() {
                bail!("activation state requires an artifact and no error");
            }
        }
        DeploymentState::Failed => {
            if record
                .error
                .as_deref()
                .is_none_or(|error| error.trim().is_empty())
            {
                bail!("failed deployment must include a non-empty error");
            }
        }
    }
    Ok(())
}

fn validate_operation_id(operation_id: &str) -> Result<()> {
    if operation_id.len() != OPERATION_ID_LENGTH
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("operation id must be exactly {OPERATION_ID_LENGTH} hexadecimal characters");
    }
    Ok(())
}

fn validate_sha256(sha256: &str) -> Result<()> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("deployment sha256 must be exactly 64 hexadecimal characters");
    }
    Ok(())
}

fn prepare_unit_name(operation_id: &str) -> String {
    format!("codex-connect-prepare-{operation_id}")
}

fn activation_unit_name(operation_id: &str) -> String {
    format!("codex-connect-deploy-{operation_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared_record() -> DeploymentRecord {
        DeploymentRecord {
            version: RECORD_VERSION,
            operation_id: "0123456789abcdef01234567".into(),
            source: "/work/codex-connect".into(),
            state: DeploymentState::Prepared,
            sha256: Some("a".repeat(64)),
            executable: Some("/opt/codex-connect/builds/aaaaaaaaaaaa/codex-connect".into()),
            no_start: false,
            error: None,
        }
    }

    #[test]
    fn deployment_ids_are_path_safe() {
        validate_operation_id("0123456789abcdef01234567").unwrap();
        for invalid in [
            "",
            "abc",
            "../../escape",
            "0123456789abcdef0123456g",
            "0123456789abcdef012345678",
        ] {
            assert!(validate_operation_id(invalid).is_err());
        }

        validate_sha256(&"a".repeat(64)).unwrap();
        for invalid in [
            "a".repeat(63),
            "a".repeat(65),
            format!("{}g", "a".repeat(63)),
        ] {
            assert!(validate_sha256(&invalid).is_err());
        }
    }

    #[test]
    fn operation_ids_have_the_stable_shape() {
        let id = new_operation_id().unwrap();
        validate_operation_id(&id).unwrap();
    }

    #[test]
    fn deployment_build_target_is_a_stable_shared_cache() {
        let first = build_target("0123456789abcdef01234567").unwrap();
        let second = build_target("fedcba9876543210fedcba98").unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first,
            crate::paths::cache_root()
                .unwrap()
                .join("codex-connect/deploy/build")
        );
    }

    #[test]
    fn prepare_command_is_complete_and_deterministic() {
        let mut record = prepared_record();
        record.state = DeploymentState::Building;
        record.sha256 = None;
        record.executable = None;
        let command = prepare_command(
            &record,
            Path::new("/opt/codex-connect/operator"),
            "/usr/bin:/bin",
            Path::new("/home/operator"),
            OsStr::new("/tmp/config"),
            OsStr::new("/tmp/state"),
            OsStr::new("/tmp/cache"),
        );
        assert_eq!(command.get_program(), OsStr::new("systemd-run"));
        let args = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                "--user",
                "--collect",
                "--unit",
                "codex-connect-prepare-0123456789abcdef01234567",
                "--setenv",
                "PATH=/usr/bin:/bin",
                "--setenv",
                "HOME=/home/operator",
                "--setenv",
                "XDG_CONFIG_HOME=/tmp/config",
                "--setenv",
                "XDG_STATE_HOME=/tmp/state",
                "--setenv",
                "XDG_CACHE_HOME=/tmp/cache",
                "/opt/codex-connect/operator",
                "prepare-deployment",
                "--operation-id",
                "0123456789abcdef01234567",
                "--source",
                "/work/codex-connect",
            ]
        );
    }

    #[test]
    fn activation_command_is_complete_and_deterministic() {
        let record = prepared_record();
        let command = activation_command(
            &record,
            true,
            "/usr/bin:/bin",
            Path::new("/home/operator"),
            OsStr::new("/tmp/config"),
            OsStr::new("/tmp/state"),
            OsStr::new("/tmp/cache"),
        )
        .unwrap();
        assert_eq!(command.get_program(), OsStr::new("systemd-run"));
        let args = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                "--user",
                "--collect",
                "--on-active",
                "3s",
                "--unit",
                "codex-connect-deploy-0123456789abcdef01234567",
                "--setenv",
                "PATH=/usr/bin:/bin",
                "--setenv",
                "HOME=/home/operator",
                "--setenv",
                "XDG_CONFIG_HOME=/tmp/config",
                "--setenv",
                "XDG_STATE_HOME=/tmp/state",
                "--setenv",
                "XDG_CACHE_HOME=/tmp/cache",
                "/opt/codex-connect/builds/aaaaaaaaaaaa/codex-connect",
                "activate-deployment",
                "--operation-id",
                "0123456789abcdef01234567",
                "--expected-sha256",
                &"a".repeat(64),
                "--no-start",
            ]
        );
    }

    #[test]
    fn deployment_record_requires_complete_artifact_identity() {
        let mut record = prepared_record();
        validate_record(&record).unwrap();
        record.executable = None;
        assert!(validate_record(&record).is_err());
    }

    #[test]
    fn deployment_record_enforces_state_invariants() {
        let mut record = prepared_record();

        record.state = DeploymentState::Building;
        assert!(validate_record(&record).is_err());

        record = prepared_record();
        record.state = DeploymentState::Prepared;
        record.no_start = true;
        assert!(validate_record(&record).is_err());

        record = prepared_record();
        record.state = DeploymentState::Failed;
        record.error = None;
        assert!(validate_record(&record).is_err());

        record.error = Some("activation failed".into());
        validate_record(&record).unwrap();
    }

    #[test]
    fn deployment_build_id_is_derived_from_digest() {
        let record = prepared_record();
        assert_eq!(record.build_id(), Some("aaaaaaaaaaaa"));
        assert!(!serde_json::to_string(&record).unwrap().contains("buildId"));
        let mut old_record = record;
        old_record.version = 1;
        assert!(validate_record(&old_record).is_err());
    }

    #[test]
    fn deployment_record_requires_absolute_paths() {
        let mut record = prepared_record();
        record.source = "relative/source".into();
        assert!(validate_record(&record).is_err());

        record = prepared_record();
        record.executable = Some("relative/codex-connect".into());
        assert!(validate_record(&record).is_err());
    }

    #[test]
    fn prepared_artifact_verification_rejects_missing_or_changed_content() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("codex-connect");
        std::fs::write(&executable, b"prepared bytes").unwrap();
        let identity = crate::artifact::for_path(&executable).unwrap();
        let mut record = prepared_record();
        record.sha256 = Some(identity.sha256);
        record.executable = Some(executable.display().to_string());

        verify_prepared_artifact(&record).unwrap();

        std::fs::write(&executable, b"changed bytes").unwrap();
        assert!(
            verify_prepared_artifact(&record)
                .unwrap_err()
                .to_string()
                .contains("hash mismatch")
        );

        std::fs::remove_file(&executable).unwrap();
        assert!(
            verify_prepared_artifact(&record)
                .unwrap_err()
                .to_string()
                .contains("prepared artifact is unavailable")
        );
    }
}
