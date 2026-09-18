mod artifact;
mod config;
mod deployment;
mod management;
mod service;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use codex_connect_app_server::AppServerClient;
use codex_connect_app_server::AppServerConfig;
use codex_connect_app_server::DEFAULT_REQUEST_TIMEOUT;
use codex_connect_app_server::ServerRequestMethod;
use codex_connect_app_server::verify_codex_pin;
use codex_connect_mcp::RuntimeIdentity;
use codex_connect_mcp::router as mcp_router;
use codex_connect_mcp::serve_router;
use codex_connect_relay::Relay;
use codex_connect_relay::RelayConfig;
use codex_connect_scope::Scope;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command as StdCommand;

#[derive(Debug)]
pub(crate) struct ServeConfig {
    pub(crate) codex_bin: PathBuf,
    pub(crate) scope_root: PathBuf,
    pub(crate) listen: std::net::SocketAddr,
}

#[derive(Debug, Parser)]
#[command(
    name = "codex-connect",
    about = "Connect ChatGPT to your host workspace and Codex CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandName,
}

#[derive(Debug, Subcommand)]
enum CommandName {
    /// Prepare, activate, or inspect a source deployment.
    Deploy {
        #[command(subcommand)]
        command: DeployCommand,
    },
    /// One-time backend configuration and managed-service installation.
    Setup {
        /// Install configuration and units without starting services.
        #[arg(long)]
        no_start: bool,
    },
    /// Show the managed backend state.
    Status,
    /// Start or restart the backend and ensure its user service is enabled.
    Restart,
    /// Show bounded journal output for the backend.
    Logs {
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Run end-to-end deployment diagnostics.
    Doctor,
    /// Probe the pinned Codex App Server directly without managed-service checks.
    Probe {
        /// Codex binary to probe.
        #[arg(long, default_value = "codex")]
        codex_bin: PathBuf,

        /// Working directory passed to the App Server probe.
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
    },
    /// Internal systemd entrypoint for the backend.
    #[command(hide = true)]
    RunBackend,
    /// Internal detached deployment build entrypoint.
    #[command(hide = true)]
    PrepareDeployment {
        #[arg(long)]
        operation_id: String,

        #[arg(long)]
        source: PathBuf,
    },
    /// Internal detached deployment activation entrypoint.
    #[command(hide = true)]
    ActivateDeployment {
        #[arg(long)]
        operation_id: String,

        #[arg(long)]
        expected_sha256: String,

        #[arg(long)]
        no_start: bool,
    },
    /// Run the local loopback MCP service.
    Serve {
        /// Codex executable used for the official app-server process.
        #[arg(long, default_value = "codex")]
        codex_bin: PathBuf,

        /// Durable host-scope root for file tools and official cwd fields.
        #[arg(long, default_value = "~/projects")]
        scope_root: PathBuf,

        /// Local TCP address for the Streamable HTTP MCP endpoint.
        #[arg(long, default_value = "127.0.0.1:8767")]
        listen: std::net::SocketAddr,
    },
}

#[derive(Debug, Subcommand)]
enum DeployCommand {
    /// Build and install the current source tree without restarting the backend.
    Prepare,
    /// Queue detached activation of a prepared build and return before restart begins.
    Activate {
        operation_id: String,
        /// Install configuration and units without starting services.
        #[arg(long)]
        no_start: bool,
    },
    /// Show durable deployment state and verify the live backend after success.
    Status { operation_id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        CommandName::Deploy { command } => match command {
            DeployCommand::Prepare => management::deploy_prepare().await,
            DeployCommand::Activate {
                operation_id,
                no_start,
            } => management::deploy_activate(&operation_id, no_start).await,
            DeployCommand::Status { operation_id } => {
                management::deploy_status(&operation_id).await
            }
        },
        CommandName::Setup { no_start } => management::setup(no_start).await,
        CommandName::Status => management::status().await,
        CommandName::Restart => management::restart().await,
        CommandName::Logs { lines, follow } => management::logs(lines, follow).await,
        CommandName::Doctor => management::doctor().await,
        CommandName::Probe { codex_bin, cwd } => probe_app_server(&codex_bin, &cwd).await,
        CommandName::RunBackend => management::run_backend().await,
        CommandName::PrepareDeployment {
            operation_id,
            source,
        } => management::prepare_deployment(&operation_id, &source).await,
        CommandName::ActivateDeployment {
            operation_id,
            expected_sha256,
            no_start,
        } => management::activate_deployment(&operation_id, &expected_sha256, no_start).await,
        CommandName::Serve {
            codex_bin,
            scope_root,
            listen,
        } => {
            serve_mcp(ServeConfig {
                codex_bin,
                scope_root,
                listen,
            })
            .await
        }
    }
}

async fn serve_mcp(config: ServeConfig) -> Result<()> {
    let ServeConfig {
        codex_bin,
        scope_root,
        listen,
    } = config;
    if !listen.ip().is_loopback() {
        anyhow::bail!("MCP must listen on loopback; found {listen}");
    }
    let scope_root =
        (if scope_root.to_string_lossy() == "~" || scope_root.to_string_lossy().starts_with("~/") {
            config::expand_path(&scope_root.to_string_lossy())?
        } else {
            scope_root
        })
        .canonicalize()
        .context("unable to access scope root")?;
    let relay = Relay::start(RelayConfig {
        codex_bin,
        scope_root: scope_root.clone(),
    })
    .await
    .context("unable to start Codex Connect relay")?;
    let scope = Scope::open(&scope_root).context("unable to open host scope")?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("unable to listen on {listen}"))?;
    let artifact = artifact::current()?;
    let runtime = RuntimeIdentity {
        build_id: artifact.build_id,
        binary_sha256: artifact.sha256,
        executable: artifact.executable.display().to_string(),
    };
    let mut changes = relay.changes();
    let router = mcp_router(relay.clone(), scope, runtime);
    println!("Codex Connect MCP listening at http://{listen}/mcp");
    println!("scope root: {}", scope_root.display());
    tokio::select! {
        result = serve_router(listener, router) => result,
        _ = async {
            while relay.worker_available() {
                if changes.changed().await.is_err() { break; }
            }
        } => anyhow::bail!("App Server disconnected; restarting the backend is required"),
    }
}

async fn probe_app_server(codex_bin: &Path, cwd: &Path) -> Result<()> {
    verify_codex_pin(codex_bin).await?;

    let schema_dir = tempfile::tempdir().context("unable to create schema probe directory")?;
    let schema = command_with_binary_path(codex_bin)
        .args(["app-server", "generate-json-schema", "--out"])
        .arg(schema_dir.path())
        .output()
        .with_context(|| format!("unable to run {} app-server", codex_bin.display()))?;
    if !schema.status.success() {
        anyhow::bail!(
            "{} app-server generate-json-schema exited with {}: {}",
            codex_bin.display(),
            schema.status,
            String::from_utf8_lossy(&schema.stderr).trim()
        );
    }
    let schema_files = std::fs::read_dir(schema_dir.path())?.count();
    if schema_files == 0 {
        anyhow::bail!("Codex app-server generated no schema files");
    }
    let server_request_schema =
        std::fs::read_to_string(schema_dir.path().join("ServerRequest.json"))
            .context("Codex app-server generated no ServerRequest schema")?;
    let server_request_schema: serde_json::Value = serde_json::from_str(&server_request_schema)?;
    for method in ServerRequestMethod::ALL {
        let found = server_request_schema["oneOf"]
            .as_array()
            .context("ServerRequest omitted oneOf")?
            .iter()
            .any(|branch| branch["properties"]["method"]["enum"][0] == method.as_str());
        if !found {
            anyhow::bail!("pinned server request missing: {}", method.as_str());
        }
    }
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("unable to access cwd {}", cwd.display()))?;
    let app_server = AppServerClient::start(AppServerConfig {
        codex_bin: codex_bin.to_path_buf(),
        working_directory: cwd,
        client_name: "codex-connect".to_string(),
        request_timeout: DEFAULT_REQUEST_TIMEOUT,
    })
    .await
    .context("app-server initialize handshake failed")?;
    app_server
        .shutdown()
        .await
        .context("app-server shutdown failed")?;
    println!("codex contract: matched");
    println!("app-server schema: {schema_files} files generated");
    println!("app-server handshake: passed");
    println!("capabilities: experimentalApi=true (requestUserInput), requestAttestation=false");
    println!("protocol: ready for the Codex Connect adapter");
    Ok(())
}

fn command_with_binary_path(binary: &Path) -> StdCommand {
    let mut command = StdCommand::new(binary);
    let Some(parent) = binary.parent().filter(|path| !path.as_os_str().is_empty()) else {
        return command;
    };
    let mut paths = vec![parent.to_path_buf()];
    if let Some(current) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&current));
    }
    if let Ok(path) = std::env::join_paths(paths) {
        command.env("PATH", path);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use super::CommandName;
    use super::DeployCommand;
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn serve_uses_per_thread_sandbox_selection() {
        let cli =
            Cli::try_parse_from(["codex-connect", "serve"]).expect("serve arguments should parse");

        let CommandName::Serve {
            codex_bin,
            scope_root,
            listen,
        } = cli.command
        else {
            panic!("expected serve command");
        };

        assert_eq!(codex_bin, PathBuf::from("codex"));
        assert_eq!(scope_root, PathBuf::from("~/projects"));
        assert_eq!(listen, "127.0.0.1:8767".parse().unwrap());
    }

    #[test]
    fn operator_commands_parse_cleanly() {
        let deploy = Cli::try_parse_from([
            "codex-connect",
            "deploy",
            "activate",
            "0123456789abcdef01234567",
            "--no-start",
        ])
        .expect("deploy arguments should parse");
        assert!(matches!(
            deploy.command,
            CommandName::Deploy {
                command: DeployCommand::Activate {
                    ref operation_id,
                    no_start: true
                }
            } if operation_id == "0123456789abcdef01234567"
        ));

        let prepare = Cli::try_parse_from(["codex-connect", "deploy", "prepare"])
            .expect("deploy prepare should parse");
        assert!(matches!(
            prepare.command,
            CommandName::Deploy {
                command: DeployCommand::Prepare
            }
        ));

        let status = Cli::try_parse_from([
            "codex-connect",
            "deploy",
            "status",
            "0123456789abcdef01234567",
        ])
        .expect("deploy status should parse");
        assert!(matches!(
            status.command,
            CommandName::Deploy {
                command: DeployCommand::Status { ref operation_id }
            } if operation_id == "0123456789abcdef01234567"
        ));

        let probe = Cli::try_parse_from([
            "codex-connect",
            "probe",
            "--codex-bin",
            "/opt/codex",
            "--cwd",
            "/work",
        ])
        .expect("probe arguments should parse");
        let CommandName::Probe { codex_bin, cwd } = probe.command else {
            panic!("expected probe command");
        };
        assert_eq!(codex_bin, PathBuf::from("/opt/codex"));
        assert_eq!(cwd, PathBuf::from("/work"));

        assert!(
            Cli::try_parse_from(["codex-connect", "doctor", "--codex-bin", "/opt/codex"]).is_err()
        );
    }
}
