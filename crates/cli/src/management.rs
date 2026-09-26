use crate::artifact;
use crate::backend::{BACKEND_ADDR, BackendClient};
use crate::deployment;
use crate::paths::{
    cache_root, home_dir, managed_build_root, managed_install_root, operator_path, state_root,
};
use crate::service::{BACKEND_SERVICE, SystemdManager, UnitState, backend_unit};
use crate::{ServeConfig, serve_mcp};
use anyhow::{Context, Result, bail};
use codex_connect_mcp::RuntimeStatus;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::time::sleep;

pub async fn run_backend() -> Result<()> {
    serve_mcp(ServeConfig {
        codex_bin: find_executable("codex")?,
        default_cwd: default_cwd()?,
        listen: BACKEND_ADDR,
    })
    .await
}

async fn reconcile_deployment(operation_id: &str) -> Result<deployment::DeploymentRecord> {
    let record = deployment::load(operation_id)?;
    match record.state {
        deployment::DeploymentState::Building => {
            let unit = deployment::prepare_service_unit(operation_id)?;
            if transient_unit_pending(&unit)? {
                return Ok(record);
            }
            deployment::mark_failed_if_state(
                operation_id,
                &[deployment::DeploymentState::Building],
                format!("detached build job {unit} ended before recording completion"),
            )
        }
        deployment::DeploymentState::ActivationQueued | deployment::DeploymentState::Activating => {
            let service = deployment::activation_service_unit(operation_id)?;
            let timer = deployment::activation_timer_unit(operation_id)?;
            if transient_unit_pending(&service)? || transient_unit_pending(&timer)? {
                return Ok(record);
            }
            let expected_sha256 = record
                .sha256
                .as_deref()
                .context("pending activation has no SHA-256")?;
            if deployment_effect_visible(&record).await?
                && record.state == deployment::DeploymentState::Activating
            {
                return deployment::mark_succeeded_if_activating(operation_id, expected_sha256);
            }
            deployment::mark_failed_if_state(
                operation_id,
                &[record.state],
                format!(
                    "detached activation job ended before recording completion and build {} is not fully active",
                    record.build_id().unwrap_or("unknown")
                ),
            )
        }
        deployment::DeploymentState::Prepared => {
            if let Err(error) = deployment::verify_prepared_artifact(&record) {
                return deployment::mark_failed_if_state(
                    operation_id,
                    &[deployment::DeploymentState::Prepared],
                    error.to_string(),
                );
            }
            Ok(record)
        }
        _ => Ok(record),
    }
}

fn transient_unit_pending(unit: &str) -> Result<bool> {
    let state = SystemdManager.unit_state(unit)?;
    Ok(transient_active_state(&state.active_state))
}

fn transient_active_state(active_state: &str) -> bool {
    matches!(
        active_state,
        "active" | "activating" | "deactivating" | "reloading"
    )
}

#[cfg(test)]
#[test]
fn deactivating_transient_units_remain_pending() {
    for state in ["active", "activating", "deactivating", "reloading"] {
        assert!(transient_active_state(state), "{state}");
    }
    for state in ["inactive", "failed", "dead"] {
        assert!(!transient_active_state(state), "{state}");
    }
}

async fn deployment_effect_visible(record: &deployment::DeploymentRecord) -> Result<bool> {
    let expected_sha256 = record
        .sha256
        .as_deref()
        .context("deployment has no SHA-256")?;
    let operator = operator_path()?;
    let operator_matches = operator.is_file()
        && artifact::for_path(&operator)
            .map(|identity| identity.sha256 == expected_sha256)
            .unwrap_or(false);
    if !operator_matches {
        return Ok(false);
    }
    if record.no_start {
        return Ok(true);
    }
    let runtime = match runtime_status_once().await {
        Ok(runtime) => runtime,
        Err(_) => return Ok(false),
    };
    Ok(runtime.ready && runtime.binary_sha256 == expected_sha256)
}

pub async fn deploy_prepare() -> Result<()> {
    let source = source_tree()?;
    let record = deployment::queue_prepare(&source)?;
    println!(
        "✓ Deployment build queued as operation {}. The running backend is unchanged.",
        record.operation_id
    );
    println!(
        "DEPLOYMENT_PREPARE operation_id={} state={}",
        record.operation_id,
        record.state.as_str()
    );
    Ok(())
}

pub async fn uninstall() -> Result<()> {
    let manager = SystemdManager;
    manager.available()?;
    let state = manager.unit_state(BACKEND_SERVICE)?;

    if state.load_state != "not-found" && state.active_state != "inactive" {
        manager.stop(BACKEND_SERVICE)?;
    }
    if matches!(
        state.unit_file_state.as_str(),
        "enabled" | "enabled-runtime"
    ) {
        manager.disable(BACKEND_SERVICE)?;
    }
    manager.remove_unit(BACKEND_SERVICE)?;
    manager.daemon_reload()?;

    let operator = operator_path()?;
    let build_root = managed_build_root()?;
    if operator.symlink_metadata().is_ok() && !remove_operator_if_owned(&operator, &build_root)? {
        println!(
            "• Preserved unmanaged operator path at {}.",
            operator.display()
        );
    }
    remove_tree_if_present(&managed_install_root()?)?;
    remove_tree_if_present(&state_root()?.join("codex-connect"))?;
    remove_tree_if_present(&cache_root()?.join("codex-connect"))?;

    println!(
        "✓ Removed Codex Connect-managed backend service, state, cache, and installed binaries."
    );
    println!("The source tree, Codex CLI, and host ingress state were not changed.");
    println!(
        "If you no longer need the ChatGPT connection, remove its public route through host ingress."
    );
    Ok(())
}

pub async fn prepare_deployment(operation_id: &str, source: &Path) -> Result<()> {
    let result = prepare_deployment_inner(operation_id, source).await;
    if let Err(error) = result {
        let message = error.to_string();
        let _ = deployment::mark_failed_if_state(
            operation_id,
            &[deployment::DeploymentState::Building],
            message,
        );
        return Err(error);
    }
    Ok(())
}

async fn prepare_deployment_inner(operation_id: &str, source: &Path) -> Result<()> {
    let source = source
        .canonicalize()
        .with_context(|| format!("unable to canonicalize source tree {}", source.display()))?;
    let record = deployment::load(operation_id)?;
    if record.source != source.display().to_string() {
        bail!(
            "deployment source mismatch for operation {operation_id}: expected {}, found {}",
            record.source,
            source.display()
        );
    }
    match record.state {
        deployment::DeploymentState::Building => {}
        deployment::DeploymentState::Prepared
        | deployment::DeploymentState::ActivationQueued
        | deployment::DeploymentState::Activating
        | deployment::DeploymentState::Succeeded => return Ok(()),
        deployment::DeploymentState::Failed => {
            bail!("deployment operation {operation_id} is already failed")
        }
    }

    let _build_lock = deployment::acquire_build_lock()?;
    let build_root = deployment::build_target(operation_id)?;
    let build = (|| -> Result<_> {
        let cargo = find_executable("cargo").or_else(|_| {
            let candidate = crate::paths::home_dir()?.join(".cargo/bin/cargo");
            ensure_executable(&candidate)?;
            Ok::<PathBuf, anyhow::Error>(candidate)
        })?;
        let status = Command::new(cargo)
            .current_dir(&source)
            .env("CARGO_TARGET_DIR", &build_root)
            .args(["build", "--release", "-p", "codex-connect", "--locked"])
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            bail!("release build failed with {status}");
        }
        let built = build_root.join("release/codex-connect");
        let identity = artifact::for_path(&built)?;
        install_artifact(&built, &identity.sha256)?;
        Ok(identity)
    })();

    match build {
        Ok(identity) => {
            let record = deployment::mark_prepared(operation_id, &identity)?;
            println!(
                "✓ Deployment operation {} prepared build {}.",
                record.operation_id, identity.build_id
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub async fn deploy_activate(operation_id: &str, no_start: bool) -> Result<()> {
    let (record, newly_queued) = deployment::queue_activation(operation_id, no_start)?;
    let build_id = record.build_id().unwrap_or("pending");
    let sha256 = record.sha256.as_deref().unwrap_or("pending");
    if !newly_queued {
        println!(
            "Deployment operation {} is already {}.",
            record.operation_id,
            record.state.as_str()
        );
    } else if record.no_start {
        println!(
            "✓ Activation queued for operation {} with a minimum handoff delay of {}; the backend will not be restarted (--no-start).",
            record.operation_id,
            deployment::activation_delay()
        );
    } else {
        println!(
            "✓ Activation queued for operation {} with a minimum handoff delay of {}; this command returns before the backend restart.",
            record.operation_id,
            deployment::activation_delay()
        );
    }
    println!(
        "DEPLOYMENT_ACTIVATION operation_id={} build_id={} sha256={} state={}",
        record.operation_id,
        build_id,
        sha256,
        record.state.as_str()
    );
    Ok(())
}

pub async fn deploy_status(operation_id: &str) -> Result<()> {
    let record = reconcile_deployment(operation_id).await?;
    println!("Codex Connect deployment operation {}", record.operation_id);
    println!("State: {}", record.state.as_str());
    println!("Source: {}", record.source);
    if let Some(build_id) = record.build_id() {
        println!("Build: {build_id}");
    }
    if let Some(sha256) = &record.sha256 {
        println!("SHA-256: {sha256}");
    }
    if let Some(executable) = record.artifact_path()? {
        println!("Artifact: {}", executable.display());
    }
    if let Some(error) = &record.error {
        println!("Error: {error}");
    }

    match record.state {
        deployment::DeploymentState::Building => {
            println!(
                "DEPLOYMENT_STATUS operation_id={} state=building verified=false",
                record.operation_id
            );
            Ok(())
        }
        deployment::DeploymentState::Prepared => {
            println!("Live verification: not activated");
            println!(
                "DEPLOYMENT_STATUS operation_id={} state=prepared build_id={} sha256={} verified=false",
                record.operation_id,
                record.build_id().unwrap_or("unknown"),
                record.sha256.as_deref().unwrap_or("unknown")
            );
            Ok(())
        }
        deployment::DeploymentState::ActivationQueued | deployment::DeploymentState::Activating => {
            println!("Live verification: activation pending");
            println!(
                "DEPLOYMENT_STATUS operation_id={} state={} build_id={} verified=false",
                record.operation_id,
                record.state.as_str(),
                record.build_id().unwrap_or("unknown")
            );
            Ok(())
        }
        deployment::DeploymentState::Failed => {
            println!(
                "DEPLOYMENT_STATUS operation_id={} state=failed verified=false",
                record.operation_id
            );
            bail!(
                "deployment {} failed: {}",
                record.operation_id,
                record
                    .error
                    .as_deref()
                    .unwrap_or("unknown activation failure")
            )
        }
        deployment::DeploymentState::Succeeded => {
            if record.no_start {
                println!("Live verification: intentionally skipped (--no-start)");
                println!(
                    "DEPLOYMENT_STATUS operation_id={} state=succeeded build_id={} verified=false no_start=true",
                    record.operation_id,
                    record.build_id().unwrap_or("unknown")
                );
                return Ok(());
            }
            let expected_sha256 = record
                .sha256
                .as_deref()
                .context("successful deployment has no SHA-256")?;
            let runtime = runtime_status_once()
                .await
                .context("deployment succeeded but the backend health endpoint is unavailable")?;
            if !runtime.ready || runtime.binary_sha256 != expected_sha256 {
                bail!(
                    "deployment {} recorded success but live backend is build {} ({})",
                    record.operation_id,
                    runtime.build_id,
                    runtime.binary_sha256
                );
            }
            println!("Live verification: ready build {} ✓", runtime.build_id);
            println!(
                "DEPLOYMENT_STATUS operation_id={} state=succeeded build_id={} sha256={} verified=true",
                record.operation_id, runtime.build_id, runtime.binary_sha256
            );
            Ok(())
        }
    }
}

pub async fn activate_deployment(
    operation_id: &str,
    expected_sha256: &str,
    no_start: bool,
) -> Result<()> {
    let result = activate_deployment_inner(operation_id, expected_sha256, no_start).await;
    if let Err(error) = result {
        let message = format!("{error:#}");
        deployment::mark_failed_if_state(
            operation_id,
            &[
                deployment::DeploymentState::ActivationQueued,
                deployment::DeploymentState::Activating,
            ],
            message,
        )
        .with_context(|| {
            format!(
                "activation failed: {error:#}; deployment {operation_id} could not be marked failed"
            )
        })?;
        return Err(error);
    }
    Ok(())
}

async fn activate_deployment_inner(
    operation_id: &str,
    expected_sha256: &str,
    no_start: bool,
) -> Result<()> {
    // All deployment operations mutate the same backend unit/operator symlink.
    // Serialize that shared activation boundary across operation ids.
    let _activation_lock = deployment::acquire_activation_lock()?;
    let running = artifact::current()?;
    if running.sha256 != expected_sha256 {
        bail!(
            "activation artifact hash mismatch: expected {expected_sha256}, running {}",
            running.sha256
        );
    }
    preflight_operator_path()?;
    deployment::mark_activating(operation_id, expected_sha256, no_start)?;
    setup_with_commit(no_start, || {
        match deployment::mark_succeeded(operation_id, expected_sha256) {
            Ok(_) => Ok(()),
            Err(error) => {
                // Atomic record replacement may succeed before a directory sync fails.
                // If success is already visible, keep the backend and link aligned with it.
                let record = deployment::load(operation_id)?;
                if record.state == deployment::DeploymentState::Succeeded {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    })
    .await
}

pub async fn setup(no_start: bool) -> Result<()> {
    let _activation_lock = deployment::acquire_activation_lock()?;
    setup_with_commit(no_start, || Ok(())).await
}

async fn setup_with_commit(no_start: bool, commit: impl FnOnce() -> Result<()>) -> Result<()> {
    let manager = SystemdManager;
    manager.available()?;
    let current = artifact::current()?;
    let binary = install_artifact(&current.executable, &current.sha256)?;
    // Resolve ownership and prepare the replacement before touching the service.
    let old_operator = preflight_operator_path()?;
    let old_unit = manager.unit_text(BACKEND_SERVICE)?;
    let old_state = manager.unit_state(BACKEND_SERVICE)?;
    let operator = operator_path()?;
    let link = operator.with_file_name(format!(".codex-connect-link-{}", std::process::id()));
    if link.symlink_metadata().is_ok() {
        fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(&binary, &link)?;
    let mut link_activated = false;
    let mut runtime_touched = false;
    let activation = async {
        manager.install_unit(BACKEND_SERVICE, &backend_unit(&binary)?)?;
        manager.daemon_reload()?;
        if !no_start {
            let state = manager.unit_state(BACKEND_SERVICE)?;
            runtime_touched = true;
            if state.status.is_running() && is_enabled(&state) {
                manager.restart(BACKEND_SERVICE)?;
            } else {
                manager.enable_start(BACKEND_SERVICE)?;
            }
            wait_for_backend_health(Some(&current.sha256)).await?;
        }
        // The link and deployment record are the final commit steps. On failure,
        // attempt restoration while the activation lock is still held.
        if preflight_operator_path()? != old_operator {
            bail!("operator path changed during backend activation");
        }
        fs::rename(&link, &operator)?;
        link_activated = true;
        commit()?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = activation {
        let link_cleanup = fs::remove_file(&link);
        let operator_restore = if link_activated {
            restore_operator_path(&operator, old_operator.as_deref())
        } else {
            Ok(())
        };
        let backend_restore =
            rollback_setup(&manager, old_unit.as_deref(), &old_state, runtime_touched);
        let mut restoration_failures = Vec::new();
        if let Err(restore_error) = operator_restore {
            restoration_failures.push(format!("operator path restoration failed: {restore_error}"));
        }
        if let Err(restore_error) = backend_restore {
            restoration_failures.push(format!("backend restoration failed: {restore_error}"));
        }
        if let Err(cleanup_error) = link_cleanup
            && cleanup_error.kind() != std::io::ErrorKind::NotFound
        {
            restoration_failures.push(format!("operator staging cleanup failed: {cleanup_error}"));
        }
        if !restoration_failures.is_empty() {
            return Err(error.context(format!(
                "partial rollback: {}",
                restoration_failures.join("; ")
            )));
        }
        return Err(error);
    }
    if no_start {
        println!("Backend service installed but not started.");
        return Ok(());
    }
    println!(
        "✓ Backend installed and ready at http://{}/mcp",
        BACKEND_ADDR
    );
    println!(
        "Connect ChatGPT to the authenticated public HTTPS MCP URL managed by host ingress. Verify ingress and OAuth separately from backend readiness."
    );
    Ok(())
}

pub async fn status() -> Result<()> {
    let manager = SystemdManager;
    manager.available()?;
    let operator = artifact::current()?;
    let state = manager.unit_state(BACKEND_SERVICE)?;
    println!("Codex Connect backend");
    println!(
        "Service: {} ({})",
        state.status.label(),
        state.unit_file_state
    );
    println!("Private endpoint: http://{BACKEND_ADDR}/mcp");
    println!(
        "Public transport: ngrok HTTPS via host ingress (routing and OAuth checked by host ingress)"
    );
    match runtime_status_once().await {
        Ok(runtime) => {
            println!("Navigation cwd: {}", runtime.cwd);
            println!(
                "Ready: {}  build {}{}",
                if runtime.ready { "yes ✓" } else { "no" },
                runtime.build_id,
                if runtime.binary_sha256 == operator.sha256 {
                    " ✓"
                } else {
                    " ≠"
                }
            );
            println!(
                "Codex: {} ({})",
                runtime.codex.release, runtime.codex.binary
            );
        }
        Err(error) => {
            println!("Navigation cwd: unavailable");
            println!("Ready: no ({error})");
        }
    }
    Ok(())
}

pub async fn restart() -> Result<()> {
    let manager = SystemdManager;
    manager.available()?;
    let state = manager.unit_state(BACKEND_SERVICE)?;
    if state.status.is_running() && is_enabled(&state) {
        manager.restart(BACKEND_SERVICE)?;
    } else {
        manager.enable_start(BACKEND_SERVICE)?;
    }
    let runtime = wait_for_backend_health(Some(&artifact::current()?.sha256)).await?;
    println!("✓ Restarted backend — ready build {}.", runtime.build_id);
    Ok(())
}

pub async fn logs(lines: usize, follow: bool) -> Result<()> {
    let manager = SystemdManager;
    manager.available()?;
    let output = manager.logs(BACKEND_SERVICE, lines, follow)?;
    if !output.is_empty() {
        print!("{output}");
    }
    Ok(())
}

pub async fn doctor() -> Result<()> {
    let manager = SystemdManager;
    let mut failures = Vec::new();
    check(
        &mut failures,
        "systemd user manager",
        manager.available(),
        |_| "available".to_string(),
    );
    match manager.unit_state(BACKEND_SERVICE) {
        Ok(state) if state.status.is_running() && is_enabled(&state) => {
            print_check("Backend", true, "running and enabled")
        }
        Ok(state) => {
            print_check("Backend", false, state.status.label());
            failures.push("backend".to_string());
        }
        Err(error) => {
            print_check("Backend", false, &error.to_string());
            failures.push("backend".to_string());
        }
    }
    let expected_sha256 = artifact::current()?.sha256;
    match runtime_status_once().await {
        Ok(status) => match validate_runtime_status(&status, Some(&expected_sha256)) {
            Ok(()) => {
                print_check(
                    "Runtime",
                    true,
                    &format!(
                        "cwd={} codex={} ({})",
                        status.cwd, status.codex.release, status.codex.binary
                    ),
                );
                print_check("App Server", true, "stdio ready; identity matched");
            }
            Err(error) => {
                print_check("App Server", false, &error.to_string());
                failures.push("backend".to_string());
            }
        },
        Err(error) => {
            print_check("Backend health", false, &error.to_string());
            failures.push("backend".to_string());
        }
    }
    println!("• Public HTTPS routing and OAuth are checked separately by host ingress.");
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "doctor found {} issue(s); run `codex-connect setup` or `codex-connect restart` as appropriate",
            failures.len()
        )
    }
}

fn rollback_setup(
    manager: &SystemdManager,
    old_unit: Option<&str>,
    old_state: &UnitState,
    restore_runtime: bool,
) -> Result<()> {
    if restore_runtime {
        manager.stop(BACKEND_SERVICE)?;
        if old_unit.is_none() {
            manager.disable(BACKEND_SERVICE)?;
        }
    }
    match old_unit {
        Some(unit) => manager.install_unit(BACKEND_SERVICE, unit)?,
        None => manager.remove_unit(BACKEND_SERVICE)?,
    }
    manager.daemon_reload()?;
    if restore_runtime && old_unit.is_some() {
        match old_state.unit_file_state.as_str() {
            "enabled" | "enabled-runtime" => manager.enable(BACKEND_SERVICE)?,
            "disabled" => manager.disable(BACKEND_SERVICE)?,
            _ => {}
        }
        if old_state.status.is_running() {
            manager.start(BACKEND_SERVICE)?;
        }
    }
    Ok(())
}

fn source_tree() -> Result<PathBuf> {
    let current = std::env::current_dir()?.canonicalize()?;
    for candidate in current.ancestors() {
        if candidate.join("Cargo.toml").is_file()
            && candidate.join("crates/cli/Cargo.toml").is_file()
        {
            return Ok(candidate.to_path_buf());
        }
    }
    bail!("current directory is not inside the Codex Connect source tree")
}

fn operator_path_is_owned(path: &Path, build_root: &Path) -> Result<bool> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("unable to inspect {}", path.display()));
        }
    };
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let target = fs::read_link(path)
        .with_context(|| format!("unable to read operator symlink {}", path.display()))?;
    let target = if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or(Path::new("/")).join(target)
    };
    let target = match target.canonicalize() {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let root = match build_root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(target.starts_with(root))
}

fn remove_operator_if_owned(path: &Path, build_root: &Path) -> Result<bool> {
    if !operator_path_is_owned(path, build_root)? {
        return Ok(false);
    }
    fs::remove_file(path).with_context(|| format!("unable to remove {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
#[test]
fn operator_ownership_only_accepts_managed_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let build_root = directory.path().join(".local/lib/codex-connect/builds");
    let managed = build_root.join("abc/codex-connect");
    let bin = directory.path().join(".local/bin");
    fs::create_dir_all(managed.parent().unwrap()).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(&managed, b"managed").unwrap();

    let operator = bin.join("codex-connect");
    std::os::unix::fs::symlink(&managed, &operator).unwrap();
    assert!(operator_path_is_owned(&operator, &build_root).unwrap());
    assert!(remove_operator_if_owned(&operator, &build_root).unwrap());
    assert!(!operator.exists());

    let unmanaged = directory.path().join("unmanaged");
    fs::write(&unmanaged, b"unmanaged").unwrap();
    std::os::unix::fs::symlink(&unmanaged, &operator).unwrap();
    assert!(!operator_path_is_owned(&operator, &build_root).unwrap());
    assert!(
        preflight_operator_path_at(&operator, &build_root)
            .unwrap_err()
            .to_string()
            .contains("unmanaged operator path")
    );
    assert!(!remove_operator_if_owned(&operator, &build_root).unwrap());
    assert!(operator.symlink_metadata().is_ok());
}

#[cfg(test)]
#[test]
fn operator_link_restores_previous_target_after_commit_failure() {
    let directory = tempfile::tempdir().unwrap();
    let operator = directory.path().join("codex-connect");
    let previous = directory.path().join("previous");
    let replacement = directory.path().join("replacement");
    std::os::unix::fs::symlink(&previous, &operator).unwrap();
    fs::remove_file(&operator).unwrap();
    std::os::unix::fs::symlink(&replacement, &operator).unwrap();
    restore_operator_path(&operator, Some(&previous)).unwrap();
    assert_eq!(fs::read_link(&operator).unwrap(), previous);
}

fn remove_tree_if_present(path: &Path) -> Result<()> {
    if path.symlink_metadata().is_ok() {
        fs::remove_dir_all(path).with_context(|| format!("unable to remove {}", path.display()))?;
    }
    Ok(())
}

fn install_artifact(built: &Path, sha256: &str) -> Result<PathBuf> {
    let directory = managed_build_root()?.join(&sha256[..12]);
    fs::create_dir_all(&directory)?;
    let destination = directory.join("codex-connect");
    if destination.is_file() {
        let existing = artifact::for_path(&destination)?;
        if existing.sha256 != sha256 {
            bail!(
                "deployment artifact collision at {}; expected {}, found {}",
                destination.display(),
                sha256,
                existing.sha256
            );
        }
        return Ok(destination.canonicalize()?);
    }
    let temporary = directory.join(format!(".codex-connect-{}", std::process::id()));
    fs::copy(built, &temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))?;
    }
    if artifact::for_path(&temporary)?.sha256 != sha256 {
        let _ = fs::remove_file(&temporary);
        bail!("deployed artifact hash changed while copying");
    }
    fs::rename(&temporary, &destination)?;
    Ok(destination.canonicalize()?)
}
fn preflight_operator_path() -> Result<Option<PathBuf>> {
    let destination = operator_path()?;
    let directory = destination
        .parent()
        .context("operator path has no parent directory")?;
    fs::create_dir_all(directory)?;
    let build_root = managed_build_root()?;
    preflight_operator_path_at(&destination, &build_root)
}

fn preflight_operator_path_at(destination: &Path, build_root: &Path) -> Result<Option<PathBuf>> {
    match destination.symlink_metadata() {
        Ok(_) if !operator_path_is_owned(destination, build_root)? => bail!(
            "refusing to replace unmanaged operator path {}",
            destination.display()
        ),
        Ok(_) => Ok(Some(fs::read_link(destination)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn restore_operator_path(destination: &Path, old_target: Option<&Path>) -> Result<()> {
    if let Some(target) = old_target {
        let temporary =
            destination.with_file_name(format!(".codex-connect-restore-{}", std::process::id()));
        if temporary.symlink_metadata().is_ok() {
            fs::remove_file(&temporary)?;
        }
        std::os::unix::fs::symlink(target, &temporary)?;
        if let Err(error) = fs::rename(&temporary, destination) {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
    } else {
        fs::remove_file(destination)?;
    }
    Ok(())
}
fn default_cwd() -> Result<PathBuf> {
    let path = home_dir()?
        .canonicalize()
        .context("HOME does not resolve to an existing directory")?;
    if !path.is_dir() {
        bail!("HOME is not a directory: {}", path.display());
    }
    Ok(path)
}
fn find_executable(name: &str) -> Result<PathBuf> {
    let path_var = std::env::var_os("PATH").context("PATH is not set")?;
    for directory in std::env::split_paths(&path_var) {
        let candidate = directory.join(name);
        if candidate.is_file() && is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    bail!("executable `{name}` was not found in PATH")
}
fn ensure_executable(path: &Path) -> Result<()> {
    if !path.is_file() || !is_executable(path) {
        bail!("{} is not an executable file", path.display());
    }
    Ok(())
}
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
fn is_enabled(state: &UnitState) -> bool {
    matches!(
        state.unit_file_state.as_str(),
        "enabled" | "enabled-runtime" | "static"
    )
}

async fn wait_for_backend_health(expected_sha256: Option<&str>) -> Result<RuntimeStatus> {
    let mut last_error = None;
    for _ in 0..40 {
        match runtime_status_once().await {
            Ok(status) => match validate_runtime_status(&status, expected_sha256) {
                Ok(()) => return Ok(status),
                Err(error) => last_error = Some(error),
            },
            Err(error) => last_error = Some(error),
        }
        sleep(Duration::from_millis(250)).await;
    }
    bail!(
        "backend did not become ready: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no response".to_string())
    )
}

fn validate_runtime_status(status: &RuntimeStatus, expected_sha256: Option<&str>) -> Result<()> {
    if expected_sha256.is_some_and(|hash| status.binary_sha256 != hash) {
        bail!("backend is running a stale build");
    }
    if !status.ready {
        bail!("App Server worker is unavailable");
    }
    if !status.experimental_api {
        bail!("backend does not report the required protocol capabilities");
    }
    if status.app_server_transport != "stdio" {
        bail!("backend reports an unexpected App Server transport");
    }
    Ok(())
}

async fn runtime_status_once() -> Result<RuntimeStatus> {
    BackendClient::new().runtime().await
}

fn print_check(label: &str, passed: bool, detail: &str) {
    println!("{} {label}: {detail}", if passed { "✓" } else { "✗" });
}
fn check<T>(
    failures: &mut Vec<String>,
    label: &str,
    result: Result<T>,
    detail: impl FnOnce(&T) -> String,
) {
    match result {
        Ok(value) => print_check(label, true, &detail(&value)),
        Err(error) => {
            print_check(label, false, &error.to_string());
            failures.push(label.to_string());
        }
    }
}
