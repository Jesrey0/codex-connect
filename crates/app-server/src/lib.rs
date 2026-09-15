//! Owns the pinned official App Server process and its JSONL connection.

pub mod protocol;
#[cfg(test)]
mod schema_tests;
mod server_request;
mod transport;

use protocol::{Initialize, Request};
use serde_json::{Value, json};
pub use server_request::{PendingActionKind, PendingServerRequest, ServerRequestMethod};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, broadcast, watch};
use transport::Connection;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_WIRE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REMOTE_ERROR_BYTES: usize = 8 * 1024;
const PINNED_CODEX_RELEASE: &str = protocol::CODEX_PIN;

#[derive(Clone, Debug)]
pub struct AppServerConfig {
    pub codex_bin: PathBuf,
    pub working_directory: PathBuf,
    pub client_name: String,
    pub request_timeout: Duration,
}

#[cfg(test)]
mod launch_tests {
    use super::APP_SERVER_ARGS;

    #[test]
    fn app_server_launch_enables_required_operator_features() {
        assert_eq!(
            APP_SERVER_ARGS,
            [
                "app-server",
                "-c",
                "features.default_mode_request_user_input=true",
                "-c",
                "features.request_permissions_tool=true",
                "-c",
                "features.exec_permission_approvals=true",
                "--listen",
                "stdio://",
            ]
        );
    }
}

const APP_SERVER_ARGS: &[&str] = &[
    "app-server",
    "-c",
    "features.default_mode_request_user_input=true",
    "-c",
    "features.request_permissions_tool=true",
    "-c",
    "features.exec_permission_approvals=true",
    "--listen",
    "stdio://",
];

#[derive(Debug, Error)]
pub enum AppServerError {
    #[error("unable to start Codex app-server: {0}")]
    Start(#[source] std::io::Error),
    #[error("App Server transport is at capacity")]
    Overloaded,
    #[error("App Server message exceeds the transport size limit")]
    MessageTooLarge,
    #[error("Codex app-server did not provide standard I/O")]
    MissingStdio,
    #[error("app-server I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("app-server request `{method}` timed out after {timeout_ms} ms")]
    Timeout { method: String, timeout_ms: u64 },
    #[error("app-server connection closed")]
    Disconnected,
    #[error("app-server server request is no longer pending")]
    ServerRequestNotPending,
    #[error("app-server returned invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("app-server response for `{method}` did not match the pinned protocol: {source}")]
    InvalidResponse {
        method: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("app-server returned an error for `{method}` ({code:?}): {message}")]
    Remote {
        method: String,
        code: Option<i64>,
        message: String,
        data: Option<Value>,
    },
    #[error("unable to probe Codex release with {path}: {source}")]
    ReleaseProbe {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Codex release probe failed for {path}: {status}: {stderr}")]
    ReleaseCommand {
        path: PathBuf,
        status: String,
        stderr: String,
    },
    #[error("Codex release output was not recognized: {output}")]
    InvalidReleaseOutput { output: String },
    #[error("unsupported Codex CLI release: expected {expected}, found {actual}")]
    ReleaseMismatch { expected: String, actual: String },
}

pub struct AppServerClient {
    child: Mutex<Child>,
    connection: Arc<Connection>,
    request_timeout: Duration,
}

impl AppServerClient {
    pub async fn start(config: AppServerConfig) -> Result<Self, AppServerError> {
        verify_codex_pin(&config.codex_bin).await?;
        let mut child = codex_command(&config.codex_bin)
            .args(APP_SERVER_ARGS)
            .current_dir(config.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(AppServerError::Start)?;
        let stdin = child.stdin.take().ok_or(AppServerError::MissingStdio)?;
        let stdout = child.stdout.take().ok_or(AppServerError::MissingStdio)?;
        let client = Self {
            child: Mutex::new(child),
            connection: Connection::start(stdout, stdin),
            request_timeout: config.request_timeout,
        };
        client.request(Initialize::new(config.client_name)).await?;
        client
            .connection
            .send(json!({"method":"initialized","params":{}}), None)
            .await?;
        Ok(client)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.connection.subscribe()
    }
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.connection.changes()
    }
    pub fn is_available(&self) -> bool {
        self.connection.available()
    }

    pub async fn request<R: Request>(&self, request: R) -> Result<R::Response, AppServerError> {
        self.request_with_timeout(request, self.request_timeout)
            .await
    }

    pub async fn request_with_timeout<R: Request>(
        &self,
        request: R,
        duration: Duration,
    ) -> Result<R::Response, AppServerError> {
        let value = self
            .connection
            .call(R::METHOD, serde_json::to_value(request)?, duration)
            .await?;
        serde_json::from_value(value).map_err(|source| AppServerError::InvalidResponse {
            method: R::METHOD.into(),
            source,
        })
    }

    pub fn pending_requests(&self, thread_id: Option<&str>) -> Vec<Arc<PendingServerRequest>> {
        self.connection.actions(thread_id)
    }

    pub async fn respond_to_server_request(
        &self,
        request: Arc<PendingServerRequest>,
        result: Value,
    ) -> Result<(), AppServerError> {
        self.connection.respond(request, result).await
    }

    pub async fn shutdown(self) -> Result<(), AppServerError> {
        self.connection.disconnect();
        let mut child = self.child.lock().await;
        if child.try_wait()?.is_none() {
            child.kill().await?;
        }
        child.wait().await?;
        Ok(())
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        self.connection.disconnect();
    }
}

pub async fn verify_codex_pin(codex_bin: &Path) -> Result<(), AppServerError> {
    let output = codex_command(codex_bin)
        .arg("--version")
        .output()
        .await
        .map_err(|source| AppServerError::ReleaseProbe {
            path: codex_bin.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(AppServerError::ReleaseCommand {
            path: codex_bin.to_path_buf(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let output_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let actual = parse_codex_release(&output.stdout).ok_or_else(|| {
        AppServerError::InvalidReleaseOutput {
            output: output_text.clone(),
        }
    })?;
    let expected = PINNED_CODEX_RELEASE.trim();
    if actual != expected {
        return Err(AppServerError::ReleaseMismatch {
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

fn codex_command(codex_bin: &Path) -> Command {
    let mut command = Command::new(codex_bin);
    let Some(parent) = codex_bin
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return command;
    };
    let mut paths = vec![parent.to_path_buf()];
    if let Some(current) = env::var_os("PATH") {
        paths.extend(env::split_paths(&current));
    }
    if let Ok(path) = env::join_paths(paths) {
        command.env("PATH", path);
    }
    command
}

fn parse_codex_release(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("codex-cli"))
                .then(|| fields.next().map(str::to_string))
                .flatten()
        })
}

fn remote_error(envelope: &Value, method: &str) -> AppServerError {
    let error = envelope.get("error");
    let remote_message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("app-server returned an invalid response")
        .to_string();
    let message = truncate_message(remote_message);
    AppServerError::Remote {
        method: method.to_string(),
        code: error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64),
        message,
        data: error.and_then(|error| error.get("data")).cloned(),
    }
}

fn truncate_message(message: String) -> String {
    if message.len() <= MAX_REMOTE_ERROR_BYTES {
        return message;
    }
    let end = message
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= MAX_REMOTE_ERROR_BYTES)
        .last()
        .unwrap_or(0);
    format!("{}…", &message[..end])
}
