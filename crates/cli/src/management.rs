use crate::artifact;
use crate::config::{
    BACKEND_SERVICE, BackendConfig, Config, ConfigStore, default_workspace_root, display_path,
    expand_path,
};
use crate::deployment;
use crate::service::{ServiceManager, SystemdManager, UnitState, backend_unit};
use crate::{ServeConfig, serve_mcp};
use anyhow::{Context, Result, bail};
use codex_connect_app_server::verify_codex_pin;
use codex_connect_mcp::OperatorStatus;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const HEALTH_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

pub async fn run_backend() -> Result<()> {
    let config = ConfigStore::default()?.load()?;
    let scope_root = resolve_required_directory(&config.scope.root, "scope root")?;
    serve_mcp(ServeConfig {
        codex_bin: resolve_executable(&config.backend.codex_bin)?,
        scope_root,
        listen: config.backend.listen_addr()?,
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
                    record.build_id.as_deref().unwrap_or("unknown")
                ),
            )
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
    let (_, config) = load_config()?;
    let runtime = match backend_status_once(&config.backend).await {
        Ok(runtime) => runtime,
        Err(_) => return Ok(false),
    };
    Ok(runtime.healthy && runtime.binary_sha256 == expected_sha256)
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

    let _build_lock = deployment::BuildLock::acquire()?;
    let build_root = deployment::build_target(operation_id, &source)?;
    let build = (|| -> Result<_> {
        let cargo = find_executable("cargo").or_else(|_| {
            let candidate = crate::config::home_dir()?.join(".cargo/bin/cargo");
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
        let installed = install_artifact(&built, &identity.sha256)?;
        Ok((identity, installed))
    })();

    match build {
        Ok((identity, installed)) => {
            let record = deployment::mark_prepared(operation_id, &identity, &installed)?;
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
    let build_id = record.build_id.as_deref().unwrap_or("pending");
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
    if let Some(build_id) = &record.build_id {
        println!("Build: {build_id}");
    }
    if let Some(sha256) = &record.sha256 {
        println!("SHA-256: {sha256}");
    }
    if let Some(executable) = &record.executable {
        println!("Artifact: {executable}");
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
                record.build_id.as_deref().unwrap_or("unknown"),
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
                record.build_id.as_deref().unwrap_or("unknown")
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
                    record.build_id.as_deref().unwrap_or("unknown")
                );
                return Ok(());
            }
            let expected_sha256 = record
                .sha256
                .as_deref()
                .context("successful deployment has no SHA-256")?;
            let (_, config) = load_config()?;
            let runtime = backend_status_once(&config.backend)
                .await
                .context("deployment succeeded but the backend health endpoint is unavailable")?;
            if !runtime.healthy || runtime.binary_sha256 != expected_sha256 {
                bail!(
                    "deployment {} recorded success but live backend is build {} ({})",
                    record.operation_id,
                    runtime.build_id,
                    runtime.binary_sha256
                );
            }
            println!("Live verification: healthy build {} ✓", runtime.build_id);
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
        let message = error.to_string();
        let _ = deployment::mark_failed_if_state(
            operation_id,
            &[
                deployment::DeploymentState::ActivationQueued,
                deployment::DeploymentState::Activating,
            ],
            message,
        );
        return Err(error);
    }
    Ok(())
}

async fn activate_deployment_inner(
    operation_id: &str,
    expected_sha256: &str,
    no_start: bool,
) -> Result<()> {
    // All deployment operations mutate the same backend unit/config/operator symlink.
    // Serialize that shared activation boundary across operation ids.
    let _activation_lock = deployment::ActivationLock::acquire()?;
    let running = artifact::current()?;
    if running.sha256 != expected_sha256 {
        bail!(
            "activation artifact hash mismatch: expected {expected_sha256}, running {}",
            running.sha256
        );
    }
    deployment::mark_activating(operation_id, expected_sha256, no_start)?;
    setup(no_start).await?;
    activate_operator_symlink(&running.executable)?;
    deployment::mark_succeeded(operation_id, expected_sha256)?;
    Ok(())
}

pub async fn setup(no_start: bool) -> Result<()> {
    let manager = SystemdManager;
    manager.available()?;
    let store = ConfigStore::default()?;
    let original = store.exists().then(|| store.read_bytes()).transpose()?;
    let mut config = if store.exists() {
        store.load().context("existing configuration cannot be used; replace it with the current single-backend configuration")?
    } else {
        Config::new()
    };
    let root = expand_path(&config.scope.root)?;
    if !root.exists() {
        fs::create_dir_all(&root)
            .with_context(|| format!("unable to create default scope root {}", root.display()))?;
    }
    let root = root.canonicalize()?;
    if !root.is_dir() {
        bail!("scope root is not a directory: {}", root.display());
    }
    config.scope.root = display_path(&root);
    let workspace_root = default_workspace_root()?;
    let workspace_codex = workspace_root.join(".tools/bin/codex");
    let codex = if workspace_codex.is_file() && is_executable(&workspace_codex) {
        workspace_codex
    } else {
        resolve_executable(&config.backend.codex_bin).or_else(|_| find_executable("codex"))?
    };
    config.backend.codex_bin = codex.display().to_string();
    let binary = install_artifact(
        &artifact::current()?.executable,
        &artifact::current()?.sha256,
    )?;
    let old_unit = manager.unit_text(BACKEND_SERVICE)?;
    let old_state = manager.unit_state(BACKEND_SERVICE)?;
    let installation = (|| -> Result<()> {
        store.save(&config)?;
        manager.install_unit(BACKEND_SERVICE, &backend_unit(&binary, &workspace_root)?)?;
        manager.daemon_reload()
    })();
    if let Err(error) = installation {
        rollback_setup(
            &manager,
            &store,
            original.as_deref(),
            old_unit.as_deref(),
            &old_state,
            false,
        )?;
        return Err(error);
    }
    if no_start {
        activate_operator_symlink(&binary)?;
        println!(
            "Configuration saved to {}; backend service installed but not started.",
            store.path().display()
        );
        return Ok(());
    }
    let activation = async {
        let state = manager.unit_state(BACKEND_SERVICE)?;
        if state.status.is_running() && is_enabled(&state) {
            manager.restart(BACKEND_SERVICE)?;
        } else {
            manager.enable_start(BACKEND_SERVICE)?;
        }
        wait_for_backend_health(&config, Some(&artifact::current()?.sha256))
            .await
            .map(|_| ())
    }
    .await;
    if let Err(error) = activation {
        rollback_setup(
            &manager,
            &store,
            original.as_deref(),
            old_unit.as_deref(),
            &old_state,
            true,
        )?;
        return Err(error
            .context("setup failed; the previous backend configuration, unit, and runtime state were restored"));
    }
    activate_operator_symlink(&binary)?;
    println!(
        "✓ Backend installed and healthy at http://{}/mcp",
        config.backend.listen
    );
    println!(
        "Native tunnel lifecycle remains independent. Connect it with `tunnel-client runtimes connect ...` and check it with `tunnel-client runtimes status <alias>`."
    );
    Ok(())
}

pub async fn status() -> Result<()> {
    let (store, config) = load_config()?;
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
    println!("Endpoint: http://{}/mcp", config.backend.listen);
    println!("Scope root: {}", config.scope.root);
    match backend_status_once(&config.backend).await {
        Ok(runtime) => println!(
            "Health: ready ✓  build {}{}",
            runtime.build_id,
            if runtime.binary_sha256 == operator.sha256 {
                " ✓"
            } else {
                " ≠"
            }
        ),
        Err(error) => println!("Health: {error}"),
    }
    println!(
        "Native tunnel: managed by tunnel-client; inspect with `tunnel-client runtimes status <alias>`."
    );
    println!("Config: {}", store.path().display());
    Ok(())
}

pub async fn restart() -> Result<()> {
    let (_, config) = load_config()?;
    let manager = SystemdManager;
    manager.available()?;
    let state = manager.unit_state(BACKEND_SERVICE)?;
    if state.status.is_running() && is_enabled(&state) {
        manager.restart(BACKEND_SERVICE)?;
    } else {
        manager.enable_start(BACKEND_SERVICE)?;
    }
    let runtime = wait_for_backend_health(&config, Some(&artifact::current()?.sha256)).await?;
    println!(
        "✓ Restarted backend — healthy build {}. The native tunnel-client runtime was not restarted.",
        runtime.build_id
    );
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
    let (_, config) = load_config()?;
    let manager = SystemdManager;
    let mut failures = Vec::new();
    check(
        &mut failures,
        "systemd user manager",
        manager.available(),
        |_| "available".to_string(),
    );
    check(
        &mut failures,
        "scope root",
        resolve_required_directory(&config.scope.root, "scope root"),
        |path| display_path(path),
    );
    match resolve_executable(&config.backend.codex_bin).map(|path| (path.clone(), path)) {
        Ok((display, path)) => match verify_codex_pin(&path).await {
            Ok(()) => print_check(
                "Codex binary",
                true,
                &format!("{} (contract matched)", display_path(&display)),
            ),
            Err(error) => {
                print_check("Codex binary", false, &error.to_string());
                failures.push("Codex binary".to_string());
            }
        },
        Err(error) => {
            print_check("Codex binary", false, &error.to_string());
            failures.push("Codex binary".to_string());
        }
    }
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
    match backend_status_once(&config.backend).await {
        Ok(status) if status.healthy && status.app_server_transport == "stdio" => {
            print_check("App Server", true, "stdio ready")
        }
        Ok(_) => {
            print_check("App Server", false, "worker unavailable");
            failures.push("backend".to_string());
        }
        Err(error) => {
            print_check("Backend health", false, &error.to_string());
            failures.push("backend".to_string());
        }
    }
    println!(
        "• Native tunnel lifecycle is intentionally not checked or supervised here; use `tunnel-client runtimes status <alias>`."
    );
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
    store: &ConfigStore,
    original: Option<&[u8]>,
    old_unit: Option<&str>,
    old_state: &UnitState,
    restore_runtime: bool,
) -> Result<()> {
    if restore_runtime {
        let _ = manager.stop(BACKEND_SERVICE);
        if old_unit.is_none() {
            let _ = manager.disable(BACKEND_SERVICE);
        }
    }
    match original {
        Some(bytes) => store.restore_bytes(bytes)?,
        None if store.exists() => fs::remove_file(store.path())?,
        None => {}
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

fn load_config() -> Result<(ConfigStore, Config)> {
    let store = ConfigStore::default()?;
    if !store.exists() {
        bail!(
            "configuration not found at {}; run `codex-connect setup`",
            store.path().display()
        );
    }
    let config = store.load()?;
    Ok((store, config))
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

fn workspace_local_root() -> Result<PathBuf> {
    Ok(default_workspace_root()?.join(".local"))
}

fn operator_path() -> Result<PathBuf> {
    Ok(workspace_local_root()?.join("bin/codex-connect"))
}

fn install_artifact(built: &Path, sha256: &str) -> Result<PathBuf> {
    let directory = workspace_local_root()?
        .join("lib/codex-connect/builds")
        .join(&sha256[..12]);
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
fn activate_operator_symlink(installed: &Path) -> Result<()> {
    let directory = workspace_local_root()?.join("bin");
    fs::create_dir_all(&directory)?;
    let destination = directory.join("codex-connect");
    let temporary = directory.join(format!(".codex-connect-link-{}", std::process::id()));
    if temporary.symlink_metadata().is_ok() {
        fs::remove_file(&temporary)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(installed, &temporary)?;
    #[cfg(not(unix))]
    fs::copy(installed, &temporary)?;
    fs::rename(temporary, destination)?;
    Ok(())
}
fn resolve_required_directory(value: &str, kind: &str) -> Result<PathBuf> {
    let path = expand_path(value)?
        .canonicalize()
        .with_context(|| format!("configured {kind} does not exist: {value}"))?;
    if !path.is_dir() {
        bail!("configured {kind} is not a directory: {}", path.display());
    }
    Ok(path)
}
fn resolve_executable(value: &str) -> Result<PathBuf> {
    if Path::new(value).is_absolute() || value.starts_with("~/") || value.contains('/') {
        let path = expand_path(value)?;
        ensure_executable(&path)?;
        Ok(path)
    } else {
        find_executable(value)
    }
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

async fn wait_for_backend_health(
    config: &Config,
    expected_sha256: Option<&str>,
) -> Result<OperatorStatus> {
    let expected_scope = resolve_required_directory(&config.scope.root, "scope root")?;
    let mut last_error = None;
    for _ in 0..40 {
        match backend_status_once(&config.backend).await {
            Ok(status) if PathBuf::from(&status.scope_root).canonicalize()? != expected_scope => {
                last_error = Some(anyhow::anyhow!("backend reports a different scope root"))
            }
            Ok(status) if expected_sha256.is_some_and(|hash| status.binary_sha256 != hash) => {
                last_error = Some(anyhow::anyhow!("backend is running a stale build"))
            }
            Ok(status) if !status.healthy => {
                last_error = Some(anyhow::anyhow!("App Server worker is unavailable"))
            }
            Ok(status) if !status.experimental_api => {
                last_error = Some(anyhow::anyhow!(
                    "backend does not report the required protocol capabilities"
                ))
            }
            Ok(status) => return Ok(status),
            Err(error) => last_error = Some(error),
        }
        sleep(Duration::from_millis(250)).await;
    }
    bail!(
        "backend did not become healthy: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no response".to_string())
    )
}
async fn backend_status_once(config: &BackendConfig) -> Result<OperatorStatus> {
    let addr = config.listen_addr()?;
    if http_get(addr, "/healthz").await?.0 != 200 {
        bail!("health endpoint did not return HTTP 200");
    }
    let (status, body) = http_get(addr, "/status").await?;
    if status != 200 {
        bail!("status endpoint returned HTTP {status}");
    }
    Ok(serde_json::from_slice(&body)?)
}
async fn http_get(addr: SocketAddr, path: &str) -> Result<(u16, Vec<u8>)> {
    let mut stream = timeout(HEALTH_TIMEOUT, TcpStream::connect(addr)).await??;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    timeout(HEALTH_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .context("backend request timed out")??;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = timeout(HEALTH_TIMEOUT, stream.read(&mut buffer))
            .await
            .context("backend response timed out")??;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > HEALTH_MAX_RESPONSE_BYTES {
            bail!("backend response exceeded 1 MiB");
        }
        if response_complete(&bytes) {
            break;
        }
    }
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("backend returned incomplete HTTP response")?;
    let status = String::from_utf8_lossy(&bytes[..header_end])
        .split_whitespace()
        .nth(1)
        .context("backend returned invalid HTTP status")?
        .parse()?;
    Ok((status, bytes[header_end + 4..].to_vec()))
}

fn response_complete(bytes: &[u8]) -> bool {
    let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = header.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    content_length.is_some_and(|length| bytes.len() >= header_end + 4 + length)
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
