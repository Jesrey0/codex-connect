//! ChatGPT-oriented composition over the pinned official App Server.

mod actions;
mod event_journal;

pub use actions::{ApprovalDecision, ElicitationAction, PermissionGrant, PermissionScope};
pub use codex_connect_app_server::protocol::{
    ApprovalPolicy, CommandExec, ModelList, ReviewTarget, RpcId, SandboxPolicy,
};
use codex_connect_app_server::protocol::{
    CommandExecResponse, RateLimitsRead, ReviewStart, SkillsList, TextInput, Thread, ThreadRead,
    ThreadResume, ThreadStart, TurnInterrupt, TurnStart, TurnSteer,
};
use codex_connect_app_server::{
    AppServerClient, AppServerConfig, AppServerError, DEFAULT_REQUEST_TIMEOUT,
};
pub use codex_connect_app_server::{PendingActionKind, PendingServerRequest};
use codex_connect_scope::{Scope, ScopeError};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::time::{Duration, Instant};

pub const MAX_WAIT_MS: u64 = 120_000;
pub const MAX_COMMAND_MS: u64 = 300_000;
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
const WORKSPACE_POLICY: &str = "Workspace policy: treat the working directory as a general filesystem workspace. Version control is optional. Do not initialize repositories, create branches, commits, or tags, or use Git as a checkpoint/workflow mechanism unless the task explicitly requests version-control operations. Existing VCS metadata may be read only when it is materially required by the task.";

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub codex_bin: PathBuf,
    pub scope_root: PathBuf,
}

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("Codex app-server failed: {0}")]
    AppServer(#[from] AppServerError),
    #[error("scope policy failed: {0}")]
    Scope(#[from] ScopeError),
    #[error("unable to encode Codex response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid Codex request: {0}")]
    Invalid(String),
}

#[derive(Clone)]
pub struct Relay {
    app_server: Arc<AppServerClient>,
    scope: Scope,
    journal: event_journal::EventJournal,
}

impl Relay {
    pub async fn start(config: RelayConfig) -> Result<Self, RelayError> {
        let scope = Scope::open(config.scope_root)?;
        let app_server = Arc::new(
            AppServerClient::start(AppServerConfig {
                codex_bin: config.codex_bin,
                working_directory: scope.root().to_path_buf(),
                client_name: "codex-connect".into(),
                request_timeout: DEFAULT_REQUEST_TIMEOUT,
            })
            .await?,
        );
        let relay = Self {
            app_server,
            scope,
            journal: event_journal::EventJournal::default(),
        };
        relay.start_event_loop();
        Ok(relay)
    }

    pub fn worker_available(&self) -> bool {
        self.app_server.is_available()
    }
    pub fn scope_root(&self) -> String {
        self.scope.root().display().to_string()
    }
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.app_server.changes()
    }

    pub async fn command_exec(
        &self,
        mut request: CommandExec,
    ) -> Result<CommandExecResponse, RelayError> {
        validate_command(&request)?;
        request.cwd = Some(
            self.scope
                .resolve_app_server_directory(request.cwd.as_deref().unwrap_or("."))?,
        );
        request.timeout_ms = Some(request.timeout_ms.unwrap_or(30_000));
        request.output_bytes_cap = Some(request.output_bytes_cap.unwrap_or(64 * 1024));
        request.sandbox_policy = request
            .sandbox_policy
            .map(|p| self.root_sandbox_policy(p))
            .transpose()?;
        // App Server enforces process timeout; transport adds a small finite delivery allowance.
        let duration = Duration::from_millis(request.timeout_ms.unwrap() + 5_000);
        Ok(self
            .app_server
            .request_with_timeout(request, duration)
            .await?)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn work_start(
        &self,
        task: String,
        cwd: Option<String>,
        thread_id: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        service_tier: Option<String>,
        approval_policy: Option<ApprovalPolicy>,
        sandbox_policy: Option<SandboxPolicy>,
    ) -> Result<Value, RelayError> {
        if task.trim().is_empty() {
            return Err(RelayError::Invalid("task must not be empty".into()));
        }
        let sandbox_policy = sandbox_policy
            .map(|p| self.root_sandbox_policy(p))
            .transpose()?;
        let cursor = self.journal.cursor().await;
        let created = thread_id.is_none();
        let (thread_id, cwd) = self.prepare_thread(cwd, thread_id).await?;
        let response = self
            .app_server
            .request(TurnStart {
                thread_id: thread_id.clone(),
                input: vec![TextInput::Text { text: task }],
                cwd,
                approval_policy,
                sandbox_policy,
                model,
                effort,
                service_tier,
            })
            .await?;
        Ok(
            json!({"threadId":thread_id,"turnId":response.turn.id,"createdThread":created,"cursor":cursor}),
        )
    }

    async fn prepare_thread(
        &self,
        cwd: Option<String>,
        thread_id: Option<String>,
    ) -> Result<(String, String), RelayError> {
        let cwd = cwd
            .map(|v| self.scope.resolve_app_server_directory(&v))
            .transpose()?;
        let response = if let Some(id) = thread_id {
            // Verify the stored cwd before resume can load hooks or tools for it.
            self.read_thread(id.clone()).await?;
            self.app_server
                .request(ThreadResume {
                    thread_id: id,
                    cwd,
                    developer_instructions: Some(WORKSPACE_POLICY.into()),
                    exclude_turns: true,
                })
                .await?
        } else {
            self.app_server
                .request(ThreadStart {
                    cwd: Some(cwd.unwrap_or_else(|| self.scope_root())),
                    service_name: Some("codex-connect".into()),
                    developer_instructions: Some(WORKSPACE_POLICY.into()),
                    ..ThreadStart::default()
                })
                .await?
        };
        let cwd = self.scope.resolve_app_server_directory(&response.cwd)?;
        Ok((response.thread.id, cwd))
    }

    async fn read_thread(&self, thread_id: String) -> Result<Thread, RelayError> {
        let response = self
            .app_server
            .request(ThreadRead {
                thread_id,
                include_turns: true,
            })
            .await?;
        self.scope
            .resolve_app_server_directory(&response.thread.cwd)?;
        Ok(response.thread)
    }

    pub async fn work_read(&self, thread_id: String) -> Result<Value, RelayError> {
        let thread = self.read_thread(thread_id).await?;
        Ok(json!({
            "threadId":thread.id, "turnCount":thread.turns.len(),
            "latestTurn":thread.turns.last().map(turn_snapshot),
            "cursor":self.journal.cursor().await,
        }))
    }

    pub async fn work_wait(
        &self,
        thread_id: String,
        turn_id: Option<String>,
        after_cursor: u64,
        timeout_ms: u64,
    ) -> Result<Value, RelayError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.min(MAX_WAIT_MS));
        // Subscribe before inspecting either state source, retaining changes during RPC reads.
        let mut transport_changes = self.app_server.changes();
        let mut journal_changes = self.journal.changes();
        loop {
            if !self.worker_available() {
                return Err(AppServerError::Disconnected.into());
            }
            let thread = self.read_thread(thread_id.clone()).await?;
            let turn = match turn_id.as_deref() {
                Some(id) => Some(thread.turns.iter().find(|t| t.id == id).ok_or_else(|| {
                    RelayError::Invalid(format!("turn {id} does not exist in thread {thread_id}"))
                })?),
                None => thread.turns.last(),
            };
            let selected_id = turn.map(|t| t.id.as_str());
            let pending = self
                .app_server
                .pending_requests(Some(&thread_id))
                .into_iter()
                .filter(|r| {
                    r.turn_id
                        .as_deref()
                        .is_none_or(|id| Some(id) == selected_id)
                })
                .collect::<Vec<_>>();
            let batch = self
                .journal
                .read_after(after_cursor, &thread_id, selected_id)
                .await
                .map_err(RelayError::Invalid)?;
            let state = wait_state(turn.map(|t| t.status), &pending, !batch.events.is_empty());
            if state != "timeout" || Instant::now() >= deadline {
                return Ok(json!({
                    "threadId":thread_id,"turnId":selected_id,"state":state,
                    "turn":turn.map(turn_snapshot), "cursor":batch.cursor,
                    "historyLost":batch.history_lost,"events":batch.events,
                    "pendingActions":pending.iter().map(|r| r.as_ref()).collect::<Vec<_>>(),
                }));
            }
            // The journal is an optimization. Reconcile official state periodically even when
            // notifications are lost, oversized, or never emitted.
            tokio::select! {
                _ = transport_changes.changed() => {},
                _ = journal_changes.changed() => {},
                _ = tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(250))) => {},
            }
        }
    }

    pub async fn work_steer(
        &self,
        thread_id: String,
        expected_turn_id: String,
        instruction: String,
    ) -> Result<Value, RelayError> {
        if instruction.trim().is_empty() {
            return Err(RelayError::Invalid("instruction must not be empty".into()));
        }
        self.read_thread(thread_id.clone()).await?;
        let response = self
            .app_server
            .request(TurnSteer {
                thread_id,
                expected_turn_id,
                input: vec![TextInput::Text { text: instruction }],
            })
            .await?;
        Ok(json!({"turnId":response.turn_id}))
    }

    pub async fn work_interrupt(
        &self,
        thread_id: String,
        turn_id: String,
    ) -> Result<Value, RelayError> {
        self.read_thread(thread_id.clone()).await?;
        self.app_server
            .request(TurnInterrupt {
                thread_id,
                turn_id: turn_id.clone(),
            })
            .await?;
        Ok(json!({"turnId":turn_id,"interrupted":true}))
    }

    pub async fn review(
        &self,
        cwd: Option<String>,
        thread_id: Option<String>,
        target: ReviewTarget,
    ) -> Result<Value, RelayError> {
        let cursor = self.journal.cursor().await;
        let created = thread_id.is_none();
        let (thread_id, _) = self.prepare_thread(cwd, thread_id).await?;
        let response = self
            .app_server
            .request(ReviewStart {
                thread_id,
                target,
                delivery: "inline",
            })
            .await?;
        Ok(
            json!({"threadId":response.review_thread_id,"turnId":response.turn.id,"createdThread":created,"cursor":cursor}),
        )
    }

    pub async fn pending_actions(&self, thread_id: Option<&str>) -> Vec<Value> {
        self.app_server
            .pending_requests(thread_id)
            .iter()
            .map(|r| serde_json::to_value(r.as_ref()).unwrap())
            .collect()
    }

    pub async fn model_list(&self, request: ModelList) -> Result<Value, RelayError> {
        Ok(serde_json::to_value(
            self.app_server.request(request).await?,
        )?)
    }

    pub async fn skills_list(
        &self,
        cwds: Vec<String>,
        force_reload: bool,
    ) -> Result<Value, RelayError> {
        let cwds = if cwds.is_empty() {
            vec![self.scope_root()]
        } else {
            cwds.into_iter()
                .map(|v| self.scope.resolve_app_server_directory(&v))
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(self
            .app_server
            .request(SkillsList { cwds, force_reload })
            .await?)
    }

    pub async fn usage(&self) -> Result<Value, RelayError> {
        let value = self
            .app_server
            .request(RateLimitsRead {
                supports_luna_reserve: true,
                exclude_reset_credit_details: true,
            })
            .await?;
        Ok(
            json!({"rateLimits":value.get("rateLimits"),"rateLimitsByLimitId":value.get("rateLimitsByLimitId")}),
        )
    }

    fn start_event_loop(&self) {
        let journal = self.journal.clone();
        let mut events = self.app_server.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if let Some(method) = event.get("method").and_then(Value::as_str) {
                            journal
                                .push(method, event.get("params").unwrap_or(&Value::Null))
                                .await;
                            if method == "codexConnect/appServerStopped" {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        journal.mark_gap().await
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    fn root_sandbox_policy(&self, policy: SandboxPolicy) -> Result<SandboxPolicy, RelayError> {
        match policy {
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                network_access,
                exclude_slash_tmp,
                exclude_tmpdir_env_var,
            } => Ok(SandboxPolicy::WorkspaceWrite {
                writable_roots: writable_roots
                    .into_iter()
                    .map(|v| self.scope.resolve_app_server_directory(&v))
                    .collect::<Result<Vec<_>, _>>()?,
                network_access,
                exclude_slash_tmp,
                exclude_tmpdir_env_var,
            }),
            other => Ok(other),
        }
    }
}

fn turn_snapshot(turn: &codex_connect_app_server::protocol::Turn) -> Value {
    let output = turn.items.iter().filter(|item| matches!(
        item.get("type").and_then(Value::as_str), Some("agentMessage" | "exitedReviewMode")
    )).map(|item| {
        let text = item.get("text").or_else(|| item.get("review")).and_then(Value::as_str).unwrap_or("");
        json!({"type":item.get("type"),"text":text.chars().take(16_000).collect::<String>(),"truncated":text.chars().count()>16_000})
    }).collect::<Vec<_>>();
    json!({"id":turn.id,"status":turn.status,"error":turn.error,"output":output})
}

fn wait_state(
    status: Option<codex_connect_app_server::protocol::TurnStatus>,
    pending: &[Arc<PendingServerRequest>],
    has_events: bool,
) -> &'static str {
    if status.is_some_and(|s| s.is_terminal()) {
        "completed"
    } else if pending
        .iter()
        .any(|a| a.kind == PendingActionKind::UserInput && a.is_blocking)
    {
        "waitingForInput"
    } else if pending
        .iter()
        .any(|a| a.kind != PendingActionKind::UserInput)
    {
        "waitingForAction"
    } else if has_events || !pending.is_empty() {
        "progress"
    } else {
        "timeout"
    }
}

fn validate_command(command: &CommandExec) -> Result<(), RelayError> {
    if command.command.is_empty() || command.command[0].is_empty() {
        return Err(RelayError::Invalid(
            "command must contain an executable".into(),
        ));
    }
    if command
        .timeout_ms
        .is_some_and(|v| v == 0 || v > MAX_COMMAND_MS)
    {
        return Err(RelayError::Invalid(format!(
            "timeoutMs must be 1..={MAX_COMMAND_MS}"
        )));
    }
    if command
        .output_bytes_cap
        .is_some_and(|v| v > MAX_COMMAND_OUTPUT_BYTES)
    {
        return Err(RelayError::Invalid(format!(
            "outputBytesCap must be at most {MAX_COMMAND_OUTPUT_BYTES}"
        )));
    }
    Ok(())
}
