//! ChatGPT-oriented composition over the pinned official App Server.

mod actions;
mod event_journal;

pub use actions::{ApprovalDecision, ElicitationAction, PermissionGrant, PermissionScope};
use base64::Engine;
pub use codex_connect_app_server::protocol::{
    ApprovalPolicy, CommandExec, ModelList, ReviewTarget, RpcId, SandboxPolicy,
};
use codex_connect_app_server::protocol::{
    CommandExecResponse, FsGetMetadata, FsGetMetadataResponse, FsReadDirectory,
    FsReadDirectoryResponse, FsReadFile, FuzzyFileSearch, FuzzyFileSearchResponse, RateLimitsRead,
    ReviewStart, SkillsList, TextInput, Thread, ThreadRead, ThreadResume, ThreadStart,
    TurnInterrupt, TurnStart, TurnSteer,
};
use codex_connect_app_server::{
    AppServerClient, AppServerConfig, AppServerError, DEFAULT_REQUEST_TIMEOUT, MAX_WIRE_BYTES,
};
pub use codex_connect_app_server::{PendingActionKind, PendingServerRequest};
use codex_connect_scope::{Scope, ScopeError};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

pub const MAX_WAIT_MS: u64 = 120_000;
const WAIT_RECONCILE_MS: u64 = 1_000;
const MAX_LIVE_TURNS: usize = 256;
pub const DEFAULT_COMMAND_MS: u64 = 30_000;
pub const DEFAULT_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_MS: u64 = 60 * 60 * 1_000;
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
const WORKSPACE_POLICY: &str = "Workspace policy: treat the working directory as a general filesystem workspace. Version control is optional. Do not initialize repositories, create branches, commits, or tags, or use Git as a checkpoint/workflow mechanism unless the task explicitly requests version-control operations. Existing VCS metadata may be read only when it is materially required by the task.";
const APP_SERVER_RESPONSE_HEADROOM_BYTES: usize = 64 * 1024;
const MAX_APP_SERVER_RESPONSE_BYTES: usize = MAX_WIRE_BYTES - APP_SERVER_RESPONSE_HEADROOM_BYTES;
const MAX_FS_READ_FILE_BYTES: u64 = ((MAX_APP_SERVER_RESPONSE_BYTES / 4) * 3) as u64;

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
    #[error("host I/O failed: {0}")]
    Io(#[from] std::io::Error),
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
    live_turns: Arc<Mutex<LiveTurns>>,
}

#[derive(Default)]
struct LiveTurns {
    turns: HashMap<(String, String), codex_connect_app_server::protocol::Turn>,
    order: VecDeque<(String, String)>,
}

impl LiveTurns {
    fn insert(&mut self, thread_id: &str, turn: codex_connect_app_server::protocol::Turn) {
        let key = (thread_id.to_string(), turn.id.clone());
        if !self.turns.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        self.turns.insert(key, turn);
        while self.turns.len() > MAX_LIVE_TURNS {
            if let Some(oldest) = self.order.pop_front() {
                self.turns.remove(&oldest);
            }
        }
    }

    fn get(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Option<codex_connect_app_server::protocol::Turn> {
        self.turns
            .get(&(thread_id.to_string(), turn_id.to_string()))
            .cloned()
    }

    fn remove(&mut self, thread_id: &str, turn_id: &str) {
        let key = (thread_id.to_string(), turn_id.to_string());
        self.turns.remove(&key);
        self.order.retain(|candidate| candidate != &key);
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextRead {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub total_lines: usize,
    pub text: String,
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
            live_turns: Arc::new(Mutex::new(LiveTurns::default())),
        };
        relay.start_event_loop();
        Ok(relay)
    }

    async fn remember_live_turn(
        &self,
        thread_id: &str,
        turn: &codex_connect_app_server::protocol::Turn,
    ) {
        self.live_turns.lock().await.insert(thread_id, turn.clone());
    }

    async fn live_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Option<codex_connect_app_server::protocol::Turn> {
        self.live_turns.lock().await.get(thread_id, turn_id)
    }

    async fn forget_live_turn(&self, thread_id: &str, turn_id: &str) {
        self.live_turns.lock().await.remove(thread_id, turn_id);
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

    /// Filesystem inspection is performed by the pinned App Server after the
    /// scope resolves and fences the requested absolute path.
    pub async fn inspect_read_text(
        &self,
        requested: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<TextRead, RelayError> {
        let path = self.scope.resolve_app_server_existing(requested)?;
        let file_size = std::fs::metadata(&path)?.len();
        if file_size > MAX_FS_READ_FILE_BYTES {
            return Err(RelayError::Invalid(format!(
                "file exceeds the safe fs/readFile transport limit of {MAX_FS_READ_FILE_BYTES} bytes"
            )));
        }
        let response = self.app_server.request(FsReadFile { path }).await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(response.data_base64)
            .map_err(|error| {
                RelayError::Invalid(format!("fs/readFile returned invalid base64: {error}"))
            })?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| RelayError::Invalid("fs/readFile returned non-UTF-8 text".into()))?;
        let lines = text.lines().collect::<Vec<_>>();
        let total_lines = lines.len();
        let start = start_line.unwrap_or(1);
        let end = end_line.unwrap_or(total_lines.max(1));
        if start == 0 || end < start {
            return Err(RelayError::Invalid("invalid line range".into()));
        }
        Ok(TextRead {
            path: requested.to_string(),
            start_line: start,
            end_line: end.min(total_lines),
            total_lines,
            text: if total_lines == 0 || start > total_lines {
                String::new()
            } else {
                lines[(start - 1)..end.min(total_lines)].join("\n")
            },
        })
    }

    pub async fn inspect_read_directory(
        &self,
        requested: &str,
    ) -> Result<FsReadDirectoryResponse, RelayError> {
        let path = self.scope.resolve_app_server_directory(requested)?;
        ensure_directory_response_fits(&path)?;
        Ok(self.app_server.request(FsReadDirectory { path }).await?)
    }

    pub async fn inspect_metadata(
        &self,
        requested: &str,
    ) -> Result<FsGetMetadataResponse, RelayError> {
        let path = self.scope.resolve_app_server_existing(requested)?;
        Ok(self.app_server.request(FsGetMetadata { path }).await?)
    }

    pub async fn inspect_fuzzy_file_search(
        &self,
        query: &str,
        requested: Option<&str>,
    ) -> Result<FuzzyFileSearchResponse, RelayError> {
        if query.is_empty() {
            return Err(RelayError::Invalid(
                "fuzzy file search query must not be empty".into(),
            ));
        }
        let root = self
            .scope
            .resolve_app_server_directory(requested.unwrap_or("."))?;
        let canonical_root = Path::new(&root).canonicalize().map_err(|error| {
            RelayError::Invalid(format!("unable to canonicalize fuzzy search root: {error}"))
        })?;
        let mut response = self
            .app_server
            .request(FuzzyFileSearch {
                query: query.into(),
                roots: vec![root.clone()],
            })
            .await?;
        response.files.retain(|result| {
            let result_root = Path::new(&result.root);
            let relative = Path::new(&result.path);
            if relative.is_absolute() {
                return false;
            }
            let Ok(result_root) = result_root.canonicalize() else {
                return false;
            };
            if result_root != canonical_root {
                return false;
            }
            canonical_root
                .join(relative)
                .canonicalize()
                .is_ok_and(|candidate| candidate.starts_with(&canonical_root))
        });
        Ok(response)
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
        request.timeout_ms = Some(request.timeout_ms.unwrap_or(DEFAULT_COMMAND_MS));
        request.output_bytes_cap = Some(
            request
                .output_bytes_cap
                .unwrap_or(DEFAULT_COMMAND_OUTPUT_BYTES),
        );
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
        self.remember_live_turn(&thread_id, &response.turn).await;
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
        // Subscribe before the authoritative read so actionable requests and terminal
        // notifications cannot race the wait setup.
        let mut transport_changes = self.app_server.changes();
        let mut journal_changes = self.journal.changes();
        loop {
            if !self.worker_available() {
                return Err(AppServerError::Disconnected.into());
            }
            let thread = self.read_thread(thread_id.clone()).await?;
            let turn = match turn_id.as_deref() {
                Some(id) => {
                    let stored = thread.turns.iter().find(|t| t.id == id).cloned();
                    let live = self.live_turn(&thread_id, id).await;
                    let selected = match (stored, live) {
                        (Some(stored), Some(live))
                            if live.status.is_terminal() && !stored.status.is_terminal() =>
                        {
                            live
                        }
                        (Some(stored), _) => stored,
                        (None, Some(live)) => live,
                        (None, None) => {
                            return Err(RelayError::Invalid(format!(
                                "turn {id} does not exist in thread {thread_id}"
                            )));
                        }
                    };
                    if selected.status.is_terminal()
                        && thread
                            .turns
                            .iter()
                            .any(|turn| turn.id == id && turn.status.is_terminal())
                    {
                        self.forget_live_turn(&thread_id, id).await;
                    }
                    Some(selected)
                }
                None => thread.turns.last().cloned(),
            };
            let selected_id = turn.as_ref().map(|t| t.id.as_str());
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
            if let Some((state, wake_reason)) = wait_wake(turn.as_ref().map(|t| t.status), &pending)
            {
                return Ok(json!({
                    "threadId":thread_id,"turnId":selected_id,"state":state,"wakeReason":wake_reason,
                    "turn":turn.as_ref().map(turn_snapshot), "cursor":batch.cursor,
                    "historyLost":batch.history_lost,"events":batch.events,
                    "pendingActions":pending.iter().map(|r| r.as_ref()).collect::<Vec<_>>(),
                }));
            }
            if Instant::now() >= deadline {
                return Ok(json!({
                    "threadId":thread_id,"turnId":selected_id,"state":"active","wakeReason":"timeout",
                    "turn":turn.as_ref().map(turn_snapshot), "cursor":batch.cursor,
                    "historyLost":batch.history_lost,"events":batch.events,
                    "pendingActions":pending.iter().map(|r| r.as_ref()).collect::<Vec<_>>(),
                }));
            }

            // Ordinary worker notifications remain journaled but do not end the operator wait
            // or force an App Server thread/read. Pending server requests are checked locally;
            // terminal notifications and history gaps trigger an authoritative reconciliation.
            let selected_id = selected_id.map(str::to_owned);
            let reconcile_at =
                deadline.min(Instant::now() + Duration::from_millis(WAIT_RECONCILE_MS));
            loop {
                tokio::select! {
                    changed = transport_changes.changed() => {
                        if changed.is_err() || !self.worker_available() {
                            return Err(AppServerError::Disconnected.into());
                        }
                        let pending = self
                            .app_server
                            .pending_requests(Some(&thread_id))
                            .into_iter()
                            .filter(|r| r.turn_id.as_deref().is_none_or(|id| selected_id.as_deref() == Some(id)))
                            .collect::<Vec<_>>();
                        if wait_wake(None, &pending).is_some() {
                            break;
                        }
                    }
                    changed = journal_changes.changed() => {
                        if changed.is_err() {
                            return Err(AppServerError::Disconnected.into());
                        }
                        let batch = self
                            .journal
                            .read_after(after_cursor, &thread_id, selected_id.as_deref())
                            .await
                            .map_err(RelayError::Invalid)?;
                        if batch.history_lost || batch.events.iter().any(|event| event.method == "turn/completed") {
                            break;
                        }
                    }
                    _ = tokio::time::sleep_until(reconcile_at) => break,
                }
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
                thread_id: thread_id.clone(),
                target,
                delivery: "inline",
            })
            .await?;
        self.remember_live_turn(&thread_id, &response.turn).await;
        // Live App Server 0.154.0 returns the inline review turn on the source thread even
        // when reviewThreadId names the internal reviewer thread. work.wait needs that pair.
        Ok(
            json!({"threadId":thread_id,"turnId":response.turn.id,"createdThread":created,"cursor":cursor}),
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
        let live_turns = self.live_turns.clone();
        let mut events = self.app_server.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if let Some(method) = event.get("method").and_then(Value::as_str) {
                            if method == "codexConnect/appServerHistoryGap" {
                                journal.mark_gap().await;
                                continue;
                            }
                            if matches!(method, "turn/started" | "turn/completed") {
                                let params = event.get("params").unwrap_or(&Value::Null);
                                if let (Some(thread_id), Some(turn)) = (
                                    params.get("threadId").and_then(Value::as_str),
                                    params.get("turn").cloned(),
                                ) {
                                    if let Ok(turn) = serde_json::from_value::<
                                        codex_connect_app_server::protocol::Turn,
                                    >(turn)
                                    {
                                        live_turns.lock().await.insert(thread_id, turn);
                                    }
                                }
                            }
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
                    .map(|v| {
                        if !Path::new(&v).is_absolute() {
                            return Err(RelayError::Invalid(
                                "writableRoots must contain absolute paths".into(),
                            ));
                        }
                        Ok(self.scope.resolve_app_server_directory(&v)?)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                network_access,
                exclude_slash_tmp,
                exclude_tmpdir_env_var,
            }),
            other => Ok(other),
        }
    }
}

fn ensure_directory_response_fits(path: &str) -> Result<(), RelayError> {
    let mut estimated_bytes = 64usize;
    for entry in std::fs::read_dir(path)? {
        let file_name = entry?.file_name();
        let encoded_name_bytes = serde_json::to_vec(file_name.to_string_lossy().as_ref())?.len();
        // Conservatively covers the remaining fixed fields, punctuation, and commas for one entry.
        estimated_bytes = estimated_bytes
            .saturating_add(encoded_name_bytes)
            .saturating_add(64);
        if estimated_bytes > MAX_APP_SERVER_RESPONSE_BYTES {
            return Err(RelayError::Invalid(
                "directory listing exceeds the safe fs/readDirectory transport limit".into(),
            ));
        }
    }
    Ok(())
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

fn wait_wake(
    status: Option<codex_connect_app_server::protocol::TurnStatus>,
    pending: &[Arc<PendingServerRequest>],
) -> Option<(&'static str, &'static str)> {
    if status.is_some_and(|s| s.is_terminal()) {
        Some(("terminal", "terminal"))
    } else if pending
        .iter()
        .any(|a| a.kind == PendingActionKind::UserInput && a.is_blocking)
    {
        Some(("active", "inputRequired"))
    } else if pending
        .iter()
        .any(|a| a.kind != PendingActionKind::UserInput)
    {
        Some(("active", "actionRequired"))
    } else {
        None
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
