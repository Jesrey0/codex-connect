//! ChatGPT-oriented composition over the pinned official App Server.

mod actions;
mod command_sessions;
mod event_journal;

pub use actions::{ApprovalDecision, ElicitationAction, PermissionGrant, PermissionScope};
use base64::Engine;
pub use codex_connect_app_server::protocol::{
    ApprovalPolicy, CommandExec, CommandExecTerminalSize, ModelList, ReviewTarget, RpcId,
    SandboxPolicy,
};
use codex_connect_app_server::protocol::{
    CommandExecOutputDeltaNotification, CommandExecResize, CommandExecResponse,
    CommandExecTerminate, CommandExecWrite, FsGetMetadata, FsGetMetadataResponse, FsReadDirectory,
    FsReadDirectoryResponse, FsReadFile, FuzzyFileSearch, FuzzyFileSearchResponse, RateLimitsRead,
    ReviewStart, SkillsList, SortDirection, StreamingCommandExec, TextInput, Thread,
    ThreadItemsList, ThreadRead, ThreadResume, ThreadStart, ThreadTurnsList, TurnInterrupt,
    TurnItemsView, TurnStart, TurnSteer,
};
use codex_connect_app_server::{
    AppServerClient, AppServerConfig, AppServerError, DEFAULT_REQUEST_TIMEOUT, DeferredRequest,
    MAX_WIRE_BYTES,
};
pub use codex_connect_app_server::{PendingActionKind, PendingServerRequest};
use codex_connect_scope::{Scope, ScopeError};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

pub const MAX_WAIT_MS: u64 = 120_000;
const WAIT_RECONCILE_MS: u64 = 1_000;
const MAX_LIVE_TURNS: usize = 256;
const TURN_PAGE_SIZE: u32 = 50;
const ITEM_PAGE_SIZE: u32 = 100;
pub const DEFAULT_COMMAND_MS: u64 = 30_000;
pub const DEFAULT_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_MS: u64 = 60 * 60 * 1_000;
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_COMMAND_READ_MS: u64 = 120_000;
pub const DEFAULT_COMMAND_READ_MS: u64 = 30_000;
pub const MAX_COMMAND_WRITE_BYTES: usize = 64 * 1024;
const WORKSPACE_POLICY: &str = "Workspace policy: treat the working directory as a general filesystem workspace. Version control is optional. Do not initialize repositories, create branches, commits, or tags, or use Git as a checkpoint/workflow mechanism unless the task explicitly requests version-control operations. Existing VCS metadata may be read only when it is materially required by the task.";
const APP_SERVER_RESPONSE_HEADROOM_BYTES: usize = 64 * 1024;
const MAX_APP_SERVER_RESPONSE_BYTES: usize = MAX_WIRE_BYTES - APP_SERVER_RESPONSE_HEADROOM_BYTES;
const MAX_FS_READ_FILE_BYTES: u64 = ((MAX_APP_SERVER_RESPONSE_BYTES / 4) * 3) as u64;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub codex_bin: PathBuf,
    pub scope_root: PathBuf,
}

struct CommandStartCleanup {
    app_server: Arc<AppServerClient>,
    sessions: command_sessions::CommandSessions,
    process_id: String,
    armed: bool,
}

impl Drop for CommandStartCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let app_server = self.app_server.clone();
        let sessions = self.sessions.clone();
        let process_id = self.process_id.clone();
        tokio::spawn(async move {
            let _ = app_server
                .request(CommandExecTerminate {
                    process_id: process_id.clone(),
                })
                .await;
            sessions.remove(&process_id).await;
        });
    }
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
    command_sessions: command_sessions::CommandSessions,
    next_command_id: Arc<AtomicU64>,
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
            command_sessions: command_sessions::CommandSessions::default(),
            next_command_id: Arc::new(AtomicU64::new(1)),
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

    pub async fn command_start(
        &self,
        command: Vec<String>,
        cwd: Option<String>,
        env: Option<std::collections::BTreeMap<String, Option<String>>>,
        sandbox_policy: Option<SandboxPolicy>,
        tty: bool,
        size: Option<CommandExecTerminalSize>,
    ) -> Result<Value, RelayError> {
        validate_command_argv(&command)?;
        if !tty && size.is_some() {
            return Err(RelayError::Invalid(
                "terminal size is only valid when tty is true".into(),
            ));
        }
        if let Some(size) = size {
            validate_terminal_size(size)?;
        }
        let cwd = self
            .scope
            .resolve_app_server_directory(cwd.as_deref().unwrap_or("."))?;
        let sandbox_policy = sandbox_policy
            .map(|policy| self.root_sandbox_policy(policy))
            .transpose()?;
        let process_id = format!(
            "cc-command-{}",
            self.next_command_id.fetch_add(1, Ordering::Relaxed)
        );
        self.command_sessions
            .insert(process_id.clone(), tty)
            .await
            .map_err(RelayError::Invalid)?;
        let command_events = self.app_server.subscribe();

        let mut cleanup = CommandStartCleanup {
            app_server: self.app_server.clone(),
            sessions: self.command_sessions.clone(),
            process_id: process_id.clone(),
            armed: true,
        };
        let deferred = self
            .app_server
            .start_request(StreamingCommandExec {
                command,
                process_id: process_id.clone(),
                stream_stdin: true,
                stream_stdout_stderr: true,
                disable_timeout: true,
                disable_output_cap: true,
                tty,
                size,
                cwd: Some(cwd),
                env,
                sandbox_policy,
            })
            .await?;

        let sessions = self.command_sessions.clone();
        let completed_process_id = process_id.clone();
        tokio::spawn(async move {
            run_command_session(deferred, command_events, sessions, completed_process_id).await;
        });
        cleanup.armed = false;
        Ok(json!({
            "processId":process_id,
            "state":"running",
            "tty":tty,
            "cursor":0,
        }))
    }

    pub async fn command_read(
        &self,
        process_id: String,
        after_cursor: u64,
        timeout_ms: u64,
    ) -> Result<Value, RelayError> {
        let timeout_ms = timeout_ms.min(MAX_COMMAND_READ_MS);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut changes = self.command_sessions.changes();
        loop {
            if !self.worker_available() {
                return Err(AppServerError::Disconnected.into());
            }
            let batch = self
                .command_sessions
                .read_after(&process_id, after_cursor)
                .await
                .map_err(RelayError::Invalid)?;
            if batch.terminal || batch.changed_after {
                let wake_reason = if batch.terminal { "exit" } else { "output" };
                let mut value = batch.value;
                value["wakeReason"] = json!(wake_reason);
                return Ok(value);
            }
            if Instant::now() >= deadline {
                let mut value = batch.value;
                value["wakeReason"] = json!("timeout");
                return Ok(value);
            }
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Err(AppServerError::Disconnected.into());
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    pub async fn command_write(
        &self,
        process_id: String,
        input: Option<String>,
        close_stdin: bool,
    ) -> Result<Value, RelayError> {
        let (_, stdin_open) = self
            .command_sessions
            .ensure_running(&process_id)
            .await
            .map_err(RelayError::Invalid)?;
        if !stdin_open {
            return Err(RelayError::Invalid(format!(
                "stdin is already closed for command session {process_id}"
            )));
        }
        let input = input.unwrap_or_default();
        if input.is_empty() && !close_stdin {
            return Err(RelayError::Invalid(
                "command.write requires non-empty input or closeStdin: true".into(),
            ));
        }
        if input.len() > MAX_COMMAND_WRITE_BYTES {
            return Err(RelayError::Invalid(format!(
                "command.write input must be at most {MAX_COMMAND_WRITE_BYTES} UTF-8 bytes"
            )));
        }
        let delta_base64 = (!input.is_empty())
            .then(|| base64::engine::general_purpose::STANDARD.encode(input.as_bytes()));
        self.app_server
            .request(CommandExecWrite {
                process_id: process_id.clone(),
                delta_base64,
                close_stdin: Some(close_stdin),
            })
            .await?;
        if close_stdin {
            self.command_sessions.set_stdin_closed(&process_id).await;
        }
        Ok(json!({"processId":process_id,"written":true,"stdinClosed":close_stdin}))
    }

    pub async fn command_resize(
        &self,
        process_id: String,
        size: CommandExecTerminalSize,
    ) -> Result<Value, RelayError> {
        validate_terminal_size(size)?;
        let (tty, _) = self
            .command_sessions
            .ensure_running(&process_id)
            .await
            .map_err(RelayError::Invalid)?;
        if !tty {
            return Err(RelayError::Invalid(format!(
                "command session {process_id} does not have a PTY"
            )));
        }
        self.app_server
            .request(CommandExecResize {
                process_id: process_id.clone(),
                size,
            })
            .await?;
        Ok(json!({"processId":process_id,"resized":true}))
    }

    pub async fn command_terminate(&self, process_id: String) -> Result<Value, RelayError> {
        self.command_sessions
            .ensure_running(&process_id)
            .await
            .map_err(RelayError::Invalid)?;
        self.app_server
            .request(CommandExecTerminate {
                process_id: process_id.clone(),
            })
            .await?;
        Ok(json!({"processId":process_id,"terminationRequested":true}))
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
        sandbox_policy: SandboxPolicy,
    ) -> Result<Value, RelayError> {
        if task.trim().is_empty() {
            return Err(RelayError::Invalid("task must not be empty".into()));
        }
        let sandbox_policy = self.root_sandbox_policy(sandbox_policy)?;
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
                sandbox_policy: Some(sandbox_policy),
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
            self.read_thread_metadata(id.clone()).await?;
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

    async fn read_thread_metadata(&self, thread_id: String) -> Result<Thread, RelayError> {
        let response = self
            .app_server
            .request(ThreadRead {
                thread_id,
                include_turns: false,
            })
            .await?;
        self.scope
            .resolve_app_server_directory(&response.thread.cwd)?;
        Ok(response.thread)
    }

    async fn latest_stored_turn(
        &self,
        thread_id: &str,
    ) -> Result<Option<codex_connect_app_server::protocol::Turn>, RelayError> {
        let response = self
            .app_server
            .request(ThreadTurnsList {
                thread_id: thread_id.to_string(),
                cursor: None,
                limit: Some(1),
                sort_direction: Some(SortDirection::Desc),
                items_view: Some(TurnItemsView::NotLoaded),
            })
            .await?;
        match response.data.into_iter().next() {
            Some(mut turn) => {
                self.hydrate_turn_items(thread_id, &mut turn).await?;
                Ok(Some(turn))
            }
            None => Ok(None),
        }
    }

    async fn hydrate_turn_items(
        &self,
        thread_id: &str,
        turn: &mut codex_connect_app_server::protocol::Turn,
    ) -> Result<(), RelayError> {
        let mut cursor = None;
        let mut items = Vec::new();
        loop {
            let response = self
                .app_server
                .request(ThreadItemsList {
                    thread_id: thread_id.to_string(),
                    turn_id: Some(turn.id.clone()),
                    cursor,
                    limit: Some(ITEM_PAGE_SIZE),
                    sort_direction: Some(SortDirection::Asc),
                })
                .await?;
            items.extend(response.data.into_iter().map(|entry| entry.item));
            match response.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        turn.items = items;
        Ok(())
    }

    async fn find_stored_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<Option<codex_connect_app_server::protocol::Turn>, RelayError> {
        let mut cursor = None;
        loop {
            let response = self
                .app_server
                .request(ThreadTurnsList {
                    thread_id: thread_id.to_string(),
                    cursor,
                    limit: Some(TURN_PAGE_SIZE),
                    sort_direction: Some(SortDirection::Desc),
                    items_view: Some(TurnItemsView::NotLoaded),
                })
                .await?;
            if let Some(mut turn) = response.data.into_iter().find(|turn| turn.id == turn_id) {
                self.hydrate_turn_items(thread_id, &mut turn).await?;
                return Ok(Some(turn));
            }
            match response.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(None),
            }
        }
    }

    pub async fn work_read(&self, thread_id: String) -> Result<Value, RelayError> {
        let thread = self.read_thread_metadata(thread_id).await?;
        let latest_turn = self.latest_stored_turn(&thread.id).await?;
        Ok(json!({
            "threadId":thread.id,
            "latestTurn":latest_turn.as_ref().map(turn_snapshot),
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
        self.read_thread_metadata(thread_id.clone()).await?;
        loop {
            if !self.worker_available() {
                return Err(AppServerError::Disconnected.into());
            }
            let turn = match turn_id.as_deref() {
                Some(id) => {
                    let stored = self.find_stored_turn(&thread_id, id).await?;
                    let stored_terminal = stored
                        .as_ref()
                        .is_some_and(|turn| turn.status.is_terminal());
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
                    if selected.status.is_terminal() && stored_terminal {
                        self.forget_live_turn(&thread_id, id).await;
                    }
                    Some(selected)
                }
                None => self.latest_stored_turn(&thread_id).await?,
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
        self.read_thread_metadata(thread_id.clone()).await?;
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
        self.read_thread_metadata(thread_id.clone()).await?;
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
        Ok(self
            .app_server
            .request(RateLimitsRead {
                supports_luna_reserve: true,
                exclude_reset_credit_details: true,
            })
            .await?)
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
                            if method == "command/exec/outputDelta" {
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
                        journal.mark_gap().await;
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

async fn run_command_session(
    deferred: DeferredRequest<StreamingCommandExec>,
    mut events: tokio::sync::broadcast::Receiver<Value>,
    sessions: command_sessions::CommandSessions,
    process_id: String,
) {
    let completion = deferred.wait();
    tokio::pin!(completion);
    let result = loop {
        tokio::select! {
            result = &mut completion => break result,
            event = events.recv() => match event {
                Ok(event) => record_command_event(&sessions, &process_id, event).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    sessions.mark_process_gap(&process_id).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break completion.await,
            }
        }
    };

    loop {
        match events.try_recv() {
            Ok(event) => record_command_event(&sessions, &process_id, event).await,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                sessions.mark_process_gap(&process_id).await;
            }
            Err(
                tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed,
            ) => break,
        }
    }

    match result {
        Ok(response) => sessions.complete(&process_id, response).await,
        Err(error) => sessions.fail(&process_id, error.to_string()).await,
    }
}

async fn record_command_event(
    sessions: &command_sessions::CommandSessions,
    process_id: &str,
    event: Value,
) {
    match event.get("method").and_then(Value::as_str) {
        Some("command/exec/outputDelta") => {
            let params = event.get("params").cloned().unwrap_or(Value::Null);
            let Ok(delta) = serde_json::from_value::<CommandExecOutputDeltaNotification>(params)
            else {
                sessions.mark_process_gap(process_id).await;
                return;
            };
            if delta.process_id != process_id {
                return;
            }
            match base64::engine::general_purpose::STANDARD.decode(delta.delta_base64) {
                Ok(bytes) => {
                    sessions.push_output(process_id, delta.stream, bytes).await;
                    if delta.cap_reached {
                        sessions.mark_process_gap(process_id).await;
                    }
                }
                Err(_) => sessions.mark_process_gap(process_id).await,
            }
        }
        Some("codexConnect/appServerHistoryGap") => sessions.mark_process_gap(process_id).await,
        _ => {}
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

fn validate_terminal_size(size: CommandExecTerminalSize) -> Result<(), RelayError> {
    if size.rows == 0 || size.cols == 0 {
        return Err(RelayError::Invalid(
            "terminal size rows and cols must be greater than 0".into(),
        ));
    }
    Ok(())
}

fn validate_command_argv(command: &[String]) -> Result<(), RelayError> {
    if command.is_empty() || command[0].is_empty() {
        return Err(RelayError::Invalid(
            "command must contain an executable".into(),
        ));
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
    validate_command_argv(&command.command)?;
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
