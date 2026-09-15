use crate::artifact;
use crate::config::{
    BACKEND_SERVICE, BackendConfig, Config, ConfigStore, display_path, expand_path,
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

fn prune_installed_builds(keep_build_id: &str) -> Result<()> {
    let builds = crate::config::home_dir()?.join(".local/lib/codex-connect/builds");
    if !builds.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&builds)? {
        let entry = entry?;
        if entry.file_name() == keep_build_id {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("unable to remove stale build {}", path.display()))?;
        } else {
            fs::remove_file(&path).with_context(|| {
                format!("unable to remove stale build artifact {}", path.display())
            })?;
        }
    }
    Ok(())
}

pub async fn deploy(no_start: bool) -> Result<()> {
    let source = source_tree()?;
    println!("Building release from {}", display_path(&source));
    let build_root = source.join("target/codex-connect-deploy");
    let cargo = find_executable("cargo").or_else(|_| {
        let candidate = crate::config::home_dir()?.join(".cargo/bin/cargo");
        ensure_executable(&candidate)?;
        Ok::<PathBuf, anyhow::Error>(candidate)
    })?;
    let status = Command::new(cargo)
        .current_dir(&source)
        .env("CARGO_TARGET_DIR", &build_root)
        .args(["build", "--release", "-p", "codex-connect", "--locked"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        bail!("release build failed with {status}");
    }
    let built = build_root.join("release/codex-connect");
    let artifact = artifact::for_path(&built)?;
    let installed = install_artifact(&built, &artifact.sha256)?;
    println!(
        "✓ Installed build {} at {}",
        artifact.build_id,
        display_path(&installed)
    );
    let activation_unit = deployment::unit_name(&artifact.build_id);
    deployment::handoff(&installed, &artifact.sha256, &activation_unit, no_start)?;
    fs::remove_dir_all(&build_root).with_context(|| {
        format!(
            "unable to remove temporary build tree {}",
            build_root.display()
        )
    })?;
    println!(
        "✓ Activation handed off to {activation_unit}; it restarts only the backend, never the native tunnel-client runtime"
    );
    Ok(())
}

pub async fn activate_deployment(expected_sha256: &str, no_start: bool) -> Result<()> {
    let running = artifact::current()?;
    if running.sha256 != expected_sha256 {
        bail!(
            "activation artifact hash mismatch: expected {expected_sha256}, running {}",
            running.sha256
        );
    }
    setup(no_start).await?;
    activate_operator_symlink(&running.executable)?;
    prune_installed_builds(&running.build_id)?;
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
    if resolve_executable(&config.backend.codex_bin).is_err() {
        config.backend.codex_bin = display_path(&find_executable("codex")?);
    }
    let binary = install_artifact(
        &artifact::current()?.executable,
        &artifact::current()?.sha256,
    )?;
    let old_unit = manager.unit_text(BACKEND_SERVICE)?;
    let old_state = manager.unit_state(BACKEND_SERVICE)?;
    let installation = (|| -> Result<()> {
        store.save(&config)?;
        manager.install_unit(BACKEND_SERVICE, &backend_unit(&binary)?)?;
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
fn install_artifact(built: &Path, sha256: &str) -> Result<PathBuf> {
    let directory = crate::config::home_dir()?
        .join(".local/lib/codex-connect/builds")
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
    let directory = crate::config::home_dir()?.join(".local/bin");
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
