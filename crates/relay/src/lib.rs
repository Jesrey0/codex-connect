//! ChatGPT-oriented composition over the pinned official App Server.

mod actions;
mod activity;
mod command_sessions;
mod event_journal;
mod operator_inbox;

pub use actions::{ApprovalDecision, ElicitationAction, PermissionGrant, PermissionScope};
use activity::{activity, compact_text};
use base64::Engine;
pub use codex_connect_app_server::protocol::{
    ApprovalPolicy, CommandExec, CommandExecTerminalSize, ModelList, ReviewTarget, RpcId,
    SandboxMode, SandboxPolicy,
};
use codex_connect_app_server::protocol::{
    CommandExecOutputDeltaNotification, CommandExecResize, CommandExecTerminate, CommandExecWrite,
    FsGetMetadata, FsGetMetadataResponse, FsReadDirectory, FsReadDirectoryResponse, FsReadFile,
    FuzzyFileSearch, FuzzyFileSearchResponse, RateLimitsRead, ReviewStart, SkillsList,
    SortDirection, StreamingCommandExec, TextInput, Thread, ThreadItemsList, ThreadRead,
    ThreadResume, ThreadStart, ThreadTurnsList, ThreadUnsubscribe, TurnInterrupt, TurnItemsView,
    TurnStart, TurnSteer,
};
use codex_connect_app_server::{
    AppServerClient, AppServerConfig, AppServerError, DEFAULT_REQUEST_TIMEOUT, DeferredRequest,
    MAX_WIRE_BYTES,
};
pub use codex_connect_app_server::{PendingActionKind, PendingServerRequest};
use codex_connect_host::{Host, HostError, MAX_IMAGE_BYTES};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::futures::OwnedNotified;
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, Instant};

pub const MAX_WAIT_MS: u64 = 120_000;
const WAIT_RECONCILE_MS: u64 = 1_000;
const OBSERVER_USAGE_REFRESH_MS: u64 = 5_000;
const MAX_SEMANTIC_EVENTS: usize = 16;
const MAX_TRANSCRIPT_TEXT_CHARS: usize = 32 * 1024;
const MAX_TRANSCRIPT_TOTAL_CHARS: usize = 192 * 1024;
const MAX_TRANSCRIPT_ENTRIES: usize = 512;
const MAX_LIVE_TURNS: usize = 256;
const TURN_PAGE_SIZE: u32 = 50;
const ITEM_PAGE_SIZE: u32 = 100;
pub const COMMAND_EXEC_RESPONSE_ALLOWANCE_MS: u64 = 5_000;
pub const DEFAULT_COMMAND_MS: u64 = 60_000;
pub const DEFAULT_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_MS: u64 = 5 * 60 * 1_000;
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_COMMAND_READ_MS: u64 = 120_000;
pub const DEFAULT_COMMAND_READ_MS: u64 = 30_000;
pub const MAX_COMMAND_WRITE_BYTES: usize = 64 * 1024;
const WORKSPACE_POLICY: &str = "Workspace policy: treat the working directory as a general filesystem workspace. Version control is optional. Do not initialize repositories, create branches, commits, or tags, or use Git as a checkpoint/workflow mechanism unless the task explicitly requests version-control operations. Existing VCS metadata may be read only when it is materially required by the task. Approval discipline: stay within the granted sandbox whenever possible. Do not request approval or additional permissions merely for convenience, broader discovery, cache writes, or optional tooling. Try a sandbox-safe alternative first. Request additional authority only when it is necessary to complete the explicit task, and state the concrete blocker.";
const APP_SERVER_RESPONSE_HEADROOM_BYTES: usize = 64 * 1024;
const MAX_APP_SERVER_RESPONSE_BYTES: usize = MAX_WIRE_BYTES - APP_SERVER_RESPONSE_HEADROOM_BYTES;
const MAX_FS_READ_FILE_BYTES: u64 = ((MAX_APP_SERVER_RESPONSE_BYTES / 4) * 3) as u64;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub codex_bin: PathBuf,
    pub default_cwd: PathBuf,
}

fn compose_developer_instructions(additional: Option<&str>) -> String {
    match additional {
        Some(additional) => {
            format!("{WORKSPACE_POLICY}\n\nOperator-supplied developer instructions:\n{additional}")
        }
        None => WORKSPACE_POLICY.into(),
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub stdout_may_be_truncated: bool,
    pub stderr_may_be_truncated: bool,
    pub duration_ms: u64,
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
    #[error("host operation failed: {0}")]
    Host(#[from] HostError),
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
    host: Host,
    journal: event_journal::EventJournal,
    operator_inbox: operator_inbox::OperatorInbox,
    live_turns: Arc<Mutex<LiveTurns>>,
    thread_subscriptions: Arc<Mutex<ThreadSubscriptions>>,
    observer_usage: Arc<Mutex<Option<(Instant, Value)>>>,
    command_sessions: command_sessions::CommandSessions,
    command_generation: Arc<str>,
    next_command_id: Arc<AtomicU64>,
}

#[derive(Default)]
struct LiveTurns {
    turns: HashMap<(String, String), ObservedTurn>,
    order: VecDeque<(String, String)>,
}

#[derive(Clone)]
struct ObservedTurn {
    turn: codex_connect_app_server::protocol::Turn,
    mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    service_tier: Option<String>,
    last_activity_at_ms: u64,
    activity_kind: String,
    activity_summary: Option<String>,
    message_item_id: Option<String>,
    message_excerpt: String,
    token_usage_total: Option<u64>,
    model_context_window: Option<u64>,
}

#[derive(Default)]
struct ThreadSubscriptions {
    threads: HashMap<String, ThreadSubscription>,
}

struct ThreadSubscription {
    starting: usize,
    active_turns: HashSet<String>,
    subscribed: bool,
    unsubscribing: bool,
    notify: Arc<Notify>,
}

impl Default for ThreadSubscription {
    fn default() -> Self {
        Self {
            starting: 0,
            active_turns: HashSet::new(),
            subscribed: false,
            unsubscribing: false,
            notify: Arc::new(Notify::new()),
        }
    }
}

impl ThreadSubscriptions {
    fn begin_start(&mut self, thread_id: &str) -> Option<OwnedNotified> {
        let state = self.threads.entry(thread_id.to_string()).or_default();
        if state.unsubscribing {
            return Some(state.notify.clone().notified_owned());
        }
        state.starting += 1;
        None
    }

    fn mark_subscribed(&mut self, thread_id: &str) {
        self.threads
            .entry(thread_id.to_string())
            .or_default()
            .subscribed = true;
    }

    fn finish_start(&mut self, thread_id: &str, turn_id: Option<&str>) -> bool {
        let (should_unsubscribe, remove_inactive) = {
            let Some(state) = self.threads.get_mut(thread_id) else {
                return false;
            };
            state.starting = state.starting.saturating_sub(1);
            if let Some(turn_id) = turn_id {
                state.active_turns.insert(turn_id.to_string());
            }
            let should_unsubscribe = Self::claim_unsubscribe(state);
            let remove_inactive = !state.subscribed
                && !state.unsubscribing
                && state.starting == 0
                && state.active_turns.is_empty();
            (should_unsubscribe, remove_inactive)
        };
        if remove_inactive {
            self.threads.remove(thread_id);
        }
        should_unsubscribe
    }

    fn finish_turn(&mut self, thread_id: &str, turn_id: &str) -> bool {
        let Some(state) = self.threads.get_mut(thread_id) else {
            return false;
        };
        state.active_turns.remove(turn_id);
        Self::claim_unsubscribe(state)
    }

    fn claim_unsubscribe(state: &mut ThreadSubscription) -> bool {
        if state.subscribed
            && !state.unsubscribing
            && state.starting == 0
            && state.active_turns.is_empty()
        {
            state.unsubscribing = true;
            true
        } else {
            false
        }
    }

    fn finish_unsubscribe(&mut self, thread_id: &str, success: bool) -> Option<Arc<Notify>> {
        let state = self.threads.get_mut(thread_id)?;
        state.unsubscribing = false;
        let notify = state.notify.clone();
        if success {
            self.threads.remove(thread_id);
        }
        Some(notify)
    }
}

impl LiveTurns {
    fn insert(&mut self, thread_id: &str, turn: codex_connect_app_server::protocol::Turn) {
        let key = (thread_id.to_string(), turn.id.clone());
        if let Some(existing) = self.turns.get_mut(&key) {
            // App Server notifications can race ahead of the turn/start response. Once a
            // terminal lifecycle event has been observed, a later stale inProgress response
            // must not regress the local projection back to active forever.
            if existing.turn.status.is_terminal() && !turn.status.is_terminal() {
                return;
            }
            existing.turn = turn;
            return;
        }
        if !self.turns.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        self.turns.insert(
            key,
            ObservedTurn {
                turn,
                mode: None,
                model: None,
                effort: None,
                service_tier: None,
                last_activity_at_ms: now_epoch_ms(),
                activity_kind: "turn".into(),
                activity_summary: Some("turn observed".into()),
                message_item_id: None,
                message_excerpt: String::new(),
                token_usage_total: None,
                model_context_window: None,
            },
        );
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
            .map(|observed| observed.turn.clone())
    }

    fn annotate(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        mode: &str,
        model: Option<String>,
        effort: Option<String>,
        service_tier: Option<String>,
    ) {
        if let Some(observed) = self
            .turns
            .get_mut(&(thread_id.to_string(), turn_id.to_string()))
        {
            observed.mode = Some(mode.to_string());
            observed.model = model;
            observed.effort = effort;
            observed.service_tier = service_tier;
        }
    }

    fn observe_event(&mut self, method: &str, params: &Value) {
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            return;
        };
        let turn_id = params.get("turnId").and_then(Value::as_str).or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
        });
        let Some(turn_id) = turn_id else {
            return;
        };
        let Some(observed) = self
            .turns
            .get_mut(&(thread_id.to_string(), turn_id.to_string()))
        else {
            return;
        };

        if method == "thread/tokenUsage/updated" {
            let usage = &params["tokenUsage"];
            observed.token_usage_total = usage["total"]["totalTokens"].as_u64();
            observed.model_context_window = usage["modelContextWindow"].as_u64();
            return;
        }

        if method == "item/agentMessage/delta" {
            observed.last_activity_at_ms = now_epoch_ms();
            {
                let item_id = params["itemId"].as_str().unwrap_or_default();
                if observed.message_item_id.as_deref() != Some(item_id) {
                    observed.message_item_id = Some(item_id.to_string());
                    observed.message_excerpt.clear();
                }
                if let Some(delta) = params["delta"].as_str() {
                    observed.message_excerpt.push_str(delta);
                    observed.message_excerpt = compact_text(&observed.message_excerpt, 240);
                }
                observed.activity_kind = "message".into();
                observed.activity_summary = (!observed.message_excerpt.is_empty())
                    .then(|| observed.message_excerpt.clone());
            }
            return;
        }

        if let Some(next) = activity(method, params) {
            observed.last_activity_at_ms = now_epoch_ms();
            observed.activity_kind = next.kind;
            observed.activity_summary = next.summary;
        }
    }

    fn remove(&mut self, thread_id: &str, turn_id: &str) {
        let key = (thread_id.to_string(), turn_id.to_string());
        self.turns.remove(&key);
        self.order.retain(|candidate| candidate != &key);
    }
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
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
        let host = Host::open(config.default_cwd)?;
        let app_server = Arc::new(
            AppServerClient::start(AppServerConfig {
                codex_bin: config.codex_bin,
                working_directory: host.default_cwd().to_path_buf(),
                client_name: "codex-connect".into(),
                request_timeout: DEFAULT_REQUEST_TIMEOUT,
            })
            .await?,
        );
        let relay = Self {
            app_server,
            host,
            journal: event_journal::EventJournal::default(),
            operator_inbox: operator_inbox::OperatorInbox::default(),
            live_turns: Arc::new(Mutex::new(LiveTurns::default())),
            thread_subscriptions: Arc::new(Mutex::new(ThreadSubscriptions::default())),
            observer_usage: Arc::new(Mutex::new(None)),
            command_sessions: command_sessions::CommandSessions::default(),
            command_generation: format!(
                "{:x}-{:x}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            )
            .into(),
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

    async fn annotate_live_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        mode: &str,
        model: Option<String>,
        effort: Option<String>,
        service_tier: Option<String>,
    ) {
        let terminal = {
            let mut live = self.live_turns.lock().await;
            live.annotate(thread_id, turn_id, mode, model, effort, service_tier);
            live.turns
                .get(&(thread_id.to_string(), turn_id.to_string()))
                .cloned()
                .filter(|observed| observed.turn.status.is_terminal())
        };
        if let Some(observed) = terminal {
            self.push_terminal_worker_event(thread_id, &observed).await;
            self.observe_terminal_turn(thread_id, turn_id).await;
        }
    }

    async fn begin_thread_start(&self, thread_id: &str) {
        loop {
            let wait = self
                .thread_subscriptions
                .lock()
                .await
                .begin_start(thread_id);
            match wait {
                Some(notified) => notified.await,
                None => return,
            }
        }
    }

    async fn mark_thread_subscribed(&self, thread_id: &str) {
        self.thread_subscriptions
            .lock()
            .await
            .mark_subscribed(thread_id);
    }

    async fn finish_thread_start(&self, thread_id: &str, turn_id: Option<&str>) {
        let should_unsubscribe = self
            .thread_subscriptions
            .lock()
            .await
            .finish_start(thread_id, turn_id);
        if should_unsubscribe {
            self.spawn_thread_unsubscribe(thread_id.to_string());
        }
    }

    async fn observe_terminal_turn(&self, thread_id: &str, turn_id: &str) {
        let should_unsubscribe = self
            .thread_subscriptions
            .lock()
            .await
            .finish_turn(thread_id, turn_id);
        if should_unsubscribe {
            self.spawn_thread_unsubscribe(thread_id.to_string());
        }
    }

    fn spawn_thread_unsubscribe(&self, thread_id: String) {
        let app_server = self.app_server.clone();
        let subscriptions = self.thread_subscriptions.clone();
        let journal = self.journal.clone();
        tokio::spawn(async move {
            let result = app_server
                .request(ThreadUnsubscribe {
                    thread_id: thread_id.clone(),
                })
                .await;
            let success = result.is_ok();
            let notify = subscriptions
                .lock()
                .await
                .finish_unsubscribe(&thread_id, success);
            if let Some(notify) = notify {
                notify.notify_waiters();
            }
            match result {
                Ok(response) => {
                    journal
                        .push(
                            "codexConnect/threadUnsubscribed",
                            &json!({"threadId":thread_id,"status":response.status}),
                        )
                        .await;
                }
                Err(error) => {
                    journal
                        .push(
                            "codexConnect/threadUnsubscribeFailed",
                            &json!({"threadId":thread_id,"error":error.to_string()}),
                        )
                        .await;
                }
            }
        });
    }

    async fn live_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Option<codex_connect_app_server::protocol::Turn> {
        self.live_turns.lock().await.get(thread_id, turn_id)
    }

    async fn current_activity_value(&self, thread_id: &str, turn_id: &str) -> Value {
        let live = self.live_turns.lock().await;
        let Some(observed) = live
            .turns
            .get(&(thread_id.to_string(), turn_id.to_string()))
        else {
            return Value::Null;
        };
        json!({
            "kind": observed.activity_kind,
            "summary": observed.activity_summary,
            "lastActivityAtMs": observed.last_activity_at_ms,
            "tokenUsage": {
                "totalTokens": observed.token_usage_total,
                "modelContextWindow": observed.model_context_window,
            }
        })
    }

    async fn forget_live_turn(&self, thread_id: &str, turn_id: &str) {
        self.live_turns.lock().await.remove(thread_id, turn_id);
    }

    pub fn worker_available(&self) -> bool {
        self.app_server.is_available()
    }
    pub fn default_cwd(&self) -> String {
        self.host.default_cwd().display().to_string()
    }
    pub fn app_server_user_agent(&self) -> String {
        self.app_server.user_agent().to_string()
    }
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.app_server.changes()
    }

    /// Filesystem inspection is performed by the pinned App Server after the
    /// configured default cwd resolves relative paths to absolute host paths.
    pub async fn inspect_read_text(
        &self,
        requested: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<TextRead, RelayError> {
        let path = self.host.resolve_app_server_existing(requested)?;
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

    /// Image bytes still come from App Server's authoritative fs/readFile
    /// primitive; Connect only adds transport preflight and
    /// prompt-oriented image presentation after this read.
    pub async fn inspect_image_bytes(&self, requested: &str) -> Result<Vec<u8>, RelayError> {
        let path = self.host.resolve_app_server_existing(requested)?;
        if std::fs::metadata(&path)?.len() > MAX_IMAGE_BYTES as u64 {
            return Err(HostError::LargeImage.into());
        }
        let response = self.app_server.request(FsReadFile { path }).await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(response.data_base64)
            .map_err(|error| {
                RelayError::Invalid(format!("fs/readFile returned invalid base64: {error}"))
            })?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(HostError::LargeImage.into());
        }
        Ok(bytes)
    }

    pub async fn inspect_read_directory(
        &self,
        requested: &str,
    ) -> Result<FsReadDirectoryResponse, RelayError> {
        let path = self.host.resolve_app_server_directory(requested)?;
        ensure_directory_response_fits(&path)?;
        Ok(self.app_server.request(FsReadDirectory { path }).await?)
    }

    pub async fn inspect_metadata(
        &self,
        requested: &str,
    ) -> Result<FsGetMetadataResponse, RelayError> {
        let path = self.host.resolve_app_server_existing(requested)?;
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
            .host
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
    ) -> Result<CommandExecResult, RelayError> {
        validate_command(&request)?;
        request.cwd = Some(
            self.host
                .resolve_app_server_directory(request.cwd.as_deref().unwrap_or("."))?,
        );
        request.timeout_ms = Some(request.timeout_ms.unwrap_or(DEFAULT_COMMAND_MS));
        request.output_bytes_cap = Some(
            request
                .output_bytes_cap
                .unwrap_or(DEFAULT_COMMAND_OUTPUT_BYTES),
        );
        let output_bytes_cap = request.output_bytes_cap.unwrap();
        request.sandbox_policy = None;
        // App Server owns the child-process timeout. Keep a small local allowance for its final
        // response delivery; any outer tunnel command deadline is independently owned and enforced
        // by tunnel-client/control-plane metadata rather than duplicated here.
        let duration =
            Duration::from_millis(request.timeout_ms.unwrap() + COMMAND_EXEC_RESPONSE_ALLOWANCE_MS);
        let started = Instant::now();
        let response = self
            .app_server
            .request_with_timeout(request, duration)
            .await?;
        let stdout_bytes = response.stdout.len();
        let stderr_bytes = response.stderr.len();
        Ok(CommandExecResult {
            exit_code: response.exit_code,
            stdout: response.stdout,
            stderr: response.stderr,
            stdout_bytes,
            stderr_bytes,
            stdout_may_be_truncated: stdout_bytes == output_bytes_cap,
            stderr_may_be_truncated: stderr_bytes == output_bytes_cap,
            duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        })
    }

    pub async fn command_start(
        &self,
        command: Vec<String>,
        cwd: Option<String>,
        env: Option<std::collections::BTreeMap<String, Option<String>>>,
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
            .host
            .resolve_app_server_directory(cwd.as_deref().unwrap_or("."))?;
        let process_id = format!(
            "cc-command-{}-{}",
            self.command_generation,
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
                sandbox_policy: None,
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
        if timeout_ms > MAX_COMMAND_READ_MS {
            return Err(RelayError::Invalid(format!(
                "timeoutMs must be less than or equal to {MAX_COMMAND_READ_MS}"
            )));
        }
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
                "command.control action=write requires non-empty input or closeStdin: true".into(),
            ));
        }
        if input.len() > MAX_COMMAND_WRITE_BYTES {
            return Err(RelayError::Invalid(format!(
                "command.control action=write input must be at most {MAX_COMMAND_WRITE_BYTES} UTF-8 bytes"
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
        developer_instructions: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        service_tier: Option<String>,
        sandbox_policy: SandboxPolicy,
    ) -> Result<Value, RelayError> {
        if task.trim().is_empty() {
            return Err(RelayError::Invalid("task must not be empty".into()));
        }
        if developer_instructions
            .as_ref()
            .is_some_and(|instructions| instructions.trim().is_empty())
        {
            return Err(RelayError::Invalid(
                "developerInstructions must not be empty".into(),
            ));
        }
        if thread_id.is_some() && developer_instructions.is_some() {
            return Err(RelayError::Invalid(
                "developerInstructions applies to new work threads only; omit threadId or omit developerInstructions"
                    .into(),
            ));
        }
        validate_work_sandbox_policy(&sandbox_policy)?;
        let thread_sandbox = sandbox_mode(&sandbox_policy);
        let cursor = self.journal.cursor().await;
        let created = thread_id.is_none();
        let (thread_id, cwd) = self
            .prepare_thread(
                cwd,
                thread_id,
                None,
                Some(thread_sandbox),
                developer_instructions,
            )
            .await?;
        let response = match self
            .app_server
            .request(TurnStart {
                thread_id: thread_id.clone(),
                input: vec![TextInput::Text { text: task }],
                cwd,
                approval_policy: Some(ApprovalPolicy::OnRequest),
                sandbox_policy: Some(sandbox_policy),
                model: model.clone(),
                effort: effort.clone(),
                service_tier_for_turn: service_tier.clone(),
            })
            .await
        {
            Ok(response) => response,
            Err(error) => {
                self.finish_thread_start(&thread_id, None).await;
                return Err(error.into());
            }
        };
        self.finish_thread_start(&thread_id, Some(&response.turn.id))
            .await;
        self.remember_live_turn(&thread_id, &response.turn).await;
        self.annotate_live_turn(
            &thread_id,
            &response.turn.id,
            "work",
            model,
            effort,
            service_tier,
        )
        .await;
        Ok(
            json!({"threadId":thread_id,"turnId":response.turn.id,"createdThread":created,"cursor":cursor}),
        )
    }

    async fn prepare_thread(
        &self,
        cwd: Option<String>,
        thread_id: Option<String>,
        new_thread_model: Option<String>,
        new_thread_sandbox: Option<SandboxMode>,
        new_thread_developer_instructions: Option<String>,
    ) -> Result<(String, String), RelayError> {
        let cwd = cwd
            .map(|v| self.host.resolve_app_server_directory(&v))
            .transpose()?;
        let response = if let Some(id) = thread_id {
            self.begin_thread_start(&id).await;
            // Serialize the whole resume preparation against an in-flight unsubscribe, then
            // verify the stored cwd before resume can load hooks or tools for it.
            if let Err(error) = self.read_thread_metadata(id.clone()).await {
                self.finish_thread_start(&id, None).await;
                return Err(error);
            }
            let response = self
                .app_server
                .request(ThreadResume {
                    thread_id: id.clone(),
                    cwd,
                    developer_instructions: None,
                    exclude_turns: true,
                })
                .await;
            match response {
                Ok(response) => {
                    self.mark_thread_subscribed(&id).await;
                    response
                }
                Err(error) => {
                    self.finish_thread_start(&id, None).await;
                    return Err(error.into());
                }
            }
        } else {
            let response = self
                .app_server
                .request(ThreadStart {
                    model: new_thread_model,
                    sandbox: new_thread_sandbox,
                    cwd: Some(cwd.unwrap_or_else(|| self.default_cwd())),
                    service_name: Some("codex-connect".into()),
                    developer_instructions: Some(compose_developer_instructions(
                        new_thread_developer_instructions.as_deref(),
                    )),
                    ..ThreadStart::default()
                })
                .await?;
            self.begin_thread_start(&response.thread.id).await;
            self.mark_thread_subscribed(&response.thread.id).await;
            response
        };
        let thread_id = response.thread.id;
        let cwd = match self.host.resolve_app_server_directory(&response.cwd) {
            Ok(cwd) => cwd,
            Err(error) => {
                self.finish_thread_start(&thread_id, None).await;
                return Err(error.into());
            }
        };
        Ok((thread_id, cwd))
    }

    async fn read_thread_metadata(&self, thread_id: String) -> Result<Thread, RelayError> {
        let response = self
            .app_server
            .request(ThreadRead {
                thread_id,
                include_turns: false,
            })
            .await?;
        self.host
            .resolve_app_server_directory(&response.thread.cwd)?;
        Ok(response.thread)
    }

    async fn hydrate_turn_items(
        &self,
        thread_id: &str,
        turn: &mut codex_connect_app_server::protocol::Turn,
    ) -> Result<(), RelayError> {
        turn.items = self.read_turn_items(thread_id, &turn.id).await?;
        Ok(())
    }

    async fn read_turn_items(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<Vec<Value>, RelayError> {
        self.read_turn_items_limited(thread_id, turn_id, None).await
    }

    async fn read_turn_items_limited(
        &self,
        thread_id: &str,
        turn_id: &str,
        max_items: Option<usize>,
    ) -> Result<Vec<Value>, RelayError> {
        let mut cursor = None;
        let mut items = Vec::new();
        loop {
            let page_limit = max_items
                .map(|max| max.saturating_sub(items.len()).min(ITEM_PAGE_SIZE as usize) as u32)
                .unwrap_or(ITEM_PAGE_SIZE);
            if page_limit == 0 {
                break;
            }
            let response = self
                .app_server
                .request(ThreadItemsList {
                    thread_id: thread_id.to_string(),
                    turn_id: Some(turn_id.to_string()),
                    cursor,
                    limit: Some(page_limit),
                    sort_direction: Some(SortDirection::Asc),
                })
                .await?;
            items.extend(response.data.into_iter().map(|entry| entry.item));
            if max_items.is_some_and(|max| items.len() >= max) {
                break;
            }
            match response.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(items)
    }

    async fn find_stored_turn_metadata(
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
            if let Some(turn) = response.data.into_iter().find(|turn| turn.id == turn_id) {
                return Ok(Some(turn));
            }
            match response.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(None),
            }
        }
    }

    pub async fn work_wait(
        &self,
        thread_id: String,
        turn_id: String,
        timeout_ms: u64,
    ) -> Result<Value, RelayError> {
        if timeout_ms == 0 || timeout_ms > MAX_WAIT_MS {
            return Err(RelayError::Invalid(format!(
                "timeoutMs must be between 1 and {MAX_WAIT_MS}"
            )));
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        // Subscribe before the authoritative read so actionable requests and terminal
        // notifications cannot race the wait setup.
        let mut transport_changes = self.app_server.changes();
        let mut journal_changes = self.journal.changes();
        let initial_live = self.live_turn(&thread_id, &turn_id).await;
        let initial_reconcile = async {
            self.read_thread_metadata(thread_id.clone()).await?;
            self.find_stored_turn_metadata(&thread_id, &turn_id).await
        };
        let mut stored = if initial_live.is_none() {
            // A turn that predates this relay process needs the minimum authoritative read
            // required to establish trustworthy state.
            initial_reconcile.await?
        } else {
            match tokio::time::timeout_at(deadline, initial_reconcile).await {
                Ok(result) => result?,
                Err(_) => None,
            }
        };
        loop {
            if !self.worker_available() {
                return Err(AppServerError::Disconnected.into());
            }
            let stored_terminal = stored
                .as_ref()
                .is_some_and(|turn| turn.status.is_terminal());
            let live = self.live_turn(&thread_id, &turn_id).await;
            let mut selected = match (stored.clone(), live) {
                (Some(stored), Some(live))
                    if live.status.is_terminal() && !stored.status.is_terminal() =>
                {
                    live
                }
                (Some(stored), _) => stored,
                (None, Some(live)) => live,
                (None, None) => {
                    return Err(RelayError::Invalid(format!(
                        "turn {turn_id} does not exist in thread {thread_id}"
                    )));
                }
            };
            if selected.status.is_terminal() && stored_terminal {
                self.hydrate_turn_items(&thread_id, &mut selected).await?;
            }
            let selected_id = Some(selected.id.as_str());
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
            let activity = self.current_activity_value(&thread_id, &turn_id).await;
            if let Some((state, wake_reason)) = wait_wake(Some(selected.status), &pending) {
                let result = json!({
                    "threadId":thread_id,"turnId":selected_id,"state":state,"wakeReason":wake_reason,
                    "turn":turn_snapshot(&selected), "currentActivity":activity,
                    "pendingActions":pending.iter().map(|r| r.as_ref()).collect::<Vec<_>>(),
                });
                self.acknowledge_wait_result(&result).await;
                if selected.status.is_terminal() {
                    if stored_terminal {
                        self.forget_live_turn(&thread_id, &turn_id).await;
                    }
                    self.observe_terminal_turn(&thread_id, &turn_id).await;
                }
                return Ok(result);
            }
            if Instant::now() >= deadline {
                let result = json!({
                    "threadId":thread_id,"turnId":selected_id,"state":"active","wakeReason":"timeout",
                    "turn":turn_snapshot(&selected), "currentActivity":activity,
                    "pendingActions":pending.iter().map(|r| r.as_ref()).collect::<Vec<_>>(),
                });
                self.acknowledge_wait_result(&result).await;
                return Ok(result);
            }

            // Ordinary worker notifications remain journaled but do not end the operator wait
            // or force an App Server thread/read. Pending server requests are checked locally;
            // terminal notifications and history gaps trigger an authoritative reconciliation.
            let selected_id = selected_id.map(str::to_owned);
            let reconcile_at =
                deadline.min(Instant::now() + Duration::from_millis(WAIT_RECONCILE_MS));
            let needs_reconcile = loop {
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
                            break false;
                        }
                    }
                    changed = journal_changes.changed() => {
                        if changed.is_err() {
                            return Err(AppServerError::Disconnected.into());
                        }
                        if self
                            .live_turn(&thread_id, &turn_id)
                            .await
                            .is_some_and(|turn| turn.status.is_terminal())
                        {
                            break true;
                        }
                    }
                    _ = tokio::time::sleep_until(reconcile_at) => break true,
                }
            };
            if needs_reconcile && Instant::now() < deadline {
                match tokio::time::timeout_at(
                    deadline,
                    self.find_stored_turn_metadata(&thread_id, &turn_id),
                )
                .await
                {
                    Ok(result) => stored = result?,
                    Err(_) => {
                        // The join lease bounds reconciliation, not the worker lifetime. A
                        // slow authoritative status read must not extend an otherwise expired
                        // wait or cancel the underlying Codex turn.
                    }
                }
            }
        }
    }

    pub async fn work_inspect(
        &self,
        thread_id: String,
        turn_id: String,
        after_cursor: u64,
        raw: bool,
    ) -> Result<Value, RelayError> {
        self.read_thread_metadata(thread_id.clone()).await?;
        let live = self.live_turn(&thread_id, &turn_id).await;
        let stored = self.find_stored_turn_metadata(&thread_id, &turn_id).await?;
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
                    "turn {turn_id} does not exist in thread {thread_id}"
                )));
            }
        };
        let activity = self.current_activity_value(&thread_id, &turn_id).await;
        if raw {
            let batch = self
                .journal
                .read_after(after_cursor, &thread_id, Some(&turn_id))
                .await
                .map_err(RelayError::Invalid)?;
            Ok(json!({
                "threadId":thread_id,
                "turnId":turn_id,
                "status":selected.status,
                "detail":"raw",
                "currentActivity":activity,
                "cursor":batch.cursor,
                "historyLost":batch.history_lost,
                "hasMore":batch.has_more,
                "events":batch.events,
            }))
        } else {
            let batch = self
                .journal
                .read_semantic_after(
                    after_cursor,
                    &thread_id,
                    Some(&turn_id),
                    MAX_SEMANTIC_EVENTS,
                )
                .await
                .map_err(RelayError::Invalid)?;
            Ok(json!({
                "threadId":thread_id,
                "turnId":turn_id,
                "status":selected.status,
                "detail":"semantic",
                "currentActivity":activity,
                "cursor":batch.cursor,
                "historyLost":batch.history_lost,
                "hasMore":batch.has_more,
                "events":batch.events,
            }))
        }
    }

    async fn acknowledge_wait_result(&self, result: &Value) {
        let thread_id = result.get("threadId").and_then(Value::as_str);
        let terminal_turn = result
            .get("turn")
            .and_then(Value::as_object)
            .filter(|turn| {
                matches!(
                    turn.get("status").and_then(Value::as_str),
                    Some("completed" | "failed" | "interrupted")
                )
            })
            .and_then(|turn| turn.get("id").and_then(Value::as_str));
        if let (Some(thread_id), Some(turn_id)) = (thread_id, terminal_turn) {
            self.operator_inbox
                .acknowledge_terminal(format!("turn:{thread_id}:{turn_id}"))
                .await;
        }

        let actions = result
            .get("pendingActions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|action| action.get("requestId"))
            .map(|request_id| format!("action:{request_id}"))
            .collect::<Vec<_>>();
        self.operator_inbox.acknowledge_actions(actions).await;
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
        model: Option<String>,
    ) -> Result<Value, RelayError> {
        if thread_id.is_some() && model.is_some() {
            return Err(RelayError::Invalid(
                "model applies to new review threads only; omit threadId or omit model".into(),
            ));
        }
        let cursor = self.journal.cursor().await;
        let created = thread_id.is_none();
        let observer_model = model.clone();
        let (thread_id, _) = self
            .prepare_thread(
                cwd,
                thread_id,
                model,
                created.then_some(SandboxMode::ReadOnly),
                None,
            )
            .await?;
        let response = match self
            .app_server
            .request(ReviewStart {
                thread_id: thread_id.clone(),
                target,
                delivery: "inline",
            })
            .await
        {
            Ok(response) => response,
            Err(error) => {
                self.finish_thread_start(&thread_id, None).await;
                return Err(error.into());
            }
        };
        self.finish_thread_start(&thread_id, Some(&response.turn.id))
            .await;
        self.remember_live_turn(&thread_id, &response.turn).await;
        self.annotate_live_turn(
            &thread_id,
            &response.turn.id,
            "review",
            observer_model,
            None,
            None,
        )
        .await;
        // The pinned App Server returns the inline review turn on the source thread even
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

    async fn push_terminal_worker_event(&self, thread_id: &str, observed: &ObservedTurn) {
        let Some(mode) = observed.mode.as_deref() else {
            return;
        };
        if !observed.turn.status.is_terminal() {
            return;
        }
        let turn_id = observed.turn.id.clone();
        self.operator_inbox
            .push_terminal(
                format!("turn:{thread_id}:{turn_id}"),
                json!({
                    "kind":"turnTerminal",
                    "threadId":thread_id,
                    "turnId":turn_id,
                    "mode":mode,
                    "status":observed.turn.status,
                }),
            )
            .await;
    }

    pub async fn take_worker_events(&self) -> Vec<Value> {
        let pending = self.app_server.pending_requests(None);
        let delegated = self.live_turns.lock().await;
        let actions = pending
            .into_iter()
            .filter(|request| {
                delegated.order.iter().any(|(thread_id, turn_id)| {
                    thread_id == &request.thread_id
                        && request.turn_id.as_deref().is_none_or(|id| id == turn_id)
                        && delegated
                            .turns
                            .get(&(thread_id.clone(), turn_id.clone()))
                            .is_some_and(|observed| observed.mode.is_some())
                })
            })
            .map(|request| {
                let request_id = serde_json::to_value(&request.request_id).unwrap();
                let action_kind = serde_json::to_value(request.kind).unwrap();
                (
                    format!("action:{request_id}"),
                    json!({
                        "kind":"actionRequired",
                        "threadId":request.thread_id,
                        "turnId":request.turn_id,
                        "actionKind":action_kind,
                        "requestId":request_id,
                        "blocking":request.is_blocking,
                    }),
                )
            })
            .collect::<Vec<_>>();
        drop(delegated);
        self.operator_inbox.sync_actions(actions).await;
        self.operator_inbox.take().await
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
            vec![self.default_cwd()]
        } else {
            cwds.into_iter()
                .map(|v| self.host.resolve_app_server_directory(&v))
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

    pub async fn observer_snapshot(&self) -> Result<Value, RelayError> {
        let usage = {
            let cached = self.observer_usage.lock().await;
            cached
                .as_ref()
                .filter(|(sampled, _)| {
                    sampled.elapsed() < Duration::from_millis(OBSERVER_USAGE_REFRESH_MS)
                })
                .map(|(_, value)| value.clone())
        };
        let usage = match usage {
            Some(value) => value,
            None => {
                let value = self.usage().await?;
                *self.observer_usage.lock().await = Some((Instant::now(), value.clone()));
                value
            }
        };
        let active_turns = {
            let live = self.live_turns.lock().await;
            live.order
                .iter()
                .filter_map(|(thread_id, turn_id)| {
                    let observed = live.turns.get(&(thread_id.clone(), turn_id.clone()))?;
                    (!observed.turn.status.is_terminal() && observed.mode.is_some()).then(|| {
                        json!({
                            "threadId": thread_id,
                            "turnId": observed.turn.id,
                            "status": observed.turn.status,
                            "mode": observed.mode,
                            "model": observed.model,
                            "effort": observed.effort,
                            "serviceTier": observed.service_tier,
                            "lastActivityAtMs": observed.last_activity_at_ms,
                            "activityKind": observed.activity_kind,
                            "activitySummary": observed.activity_summary,
                            "tokenUsage": {
                                "totalTokens": observed.token_usage_total,
                                "modelContextWindow": observed.model_context_window,
                            },
                        })
                    })
                })
                .collect::<Vec<_>>()
        };
        let pending_actions = self.pending_actions(None).await;
        let recent = self.journal.semantic_tail(64).await;
        Ok(json!({
            "cwd": self.default_cwd(),
            "usage": usage,
            "usageRefreshMs": OBSERVER_USAGE_REFRESH_MS,
            "activeTurns": active_turns,
            "pendingActions": pending_actions,
            "cursor": recent.cursor,
            "historyLost": recent.history_lost,
            "events": recent.events,
        }))
    }

    pub async fn observer_transcript(
        &self,
        thread_id: String,
        turn_id: String,
    ) -> Result<Value, RelayError> {
        self.read_thread_metadata(thread_id.clone()).await?;
        let live = self.live_turn(&thread_id, &turn_id).await;
        let stored = self.find_stored_turn_metadata(&thread_id, &turn_id).await?;
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
                    "turn {turn_id} does not exist in thread {thread_id}"
                )));
            }
        };
        let items = self
            .read_turn_items_limited(&thread_id, &turn_id, Some(MAX_TRANSCRIPT_ENTRIES + 1))
            .await?;
        let mut truncated = items.len() > MAX_TRANSCRIPT_ENTRIES;
        let mut remaining_chars = MAX_TRANSCRIPT_TOTAL_CHARS;
        let entries = items
            .iter()
            .take(MAX_TRANSCRIPT_ENTRIES)
            .filter_map(|item| transcript_entry(item, &mut remaining_chars, &mut truncated))
            .collect::<Vec<_>>();
        Ok(json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "status": selected.status,
            "activity": self.current_activity_value(&thread_id, &turn_id).await,
            "entries": entries,
            "truncated": truncated,
        }))
    }

    fn start_event_loop(&self) {
        let journal = self.journal.clone();
        let live_turns = self.live_turns.clone();
        let operator_inbox = self.operator_inbox.clone();
        let relay = self.clone();
        let mut events = self.app_server.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if let Some(method) = event.get("method").and_then(Value::as_str) {
                            if method == "codexConnect/appServerHistoryGap" {
                                journal.mark_gap().await;
                                operator_inbox.mark_history_lost().await;
                                continue;
                            }
                            if method == "command/exec/outputDelta" {
                                continue;
                            }
                            let mut terminal_turn = None;
                            if matches!(method, "turn/started" | "turn/completed") {
                                let params = event.get("params").unwrap_or(&Value::Null);
                                if let (Some(thread_id), Some(turn)) = (
                                    params.get("threadId").and_then(Value::as_str),
                                    params.get("turn").cloned(),
                                ) && let Ok(turn) = serde_json::from_value::<
                                    codex_connect_app_server::protocol::Turn,
                                >(turn)
                                {
                                    let turn_id = turn.id.clone();
                                    let terminal = {
                                        let mut live = live_turns.lock().await;
                                        live.insert(thread_id, turn);
                                        live.turns
                                            .get(&(thread_id.to_string(), turn_id))
                                            .cloned()
                                            .filter(|observed| {
                                                observed.mode.is_some()
                                                    && observed.turn.status.is_terminal()
                                            })
                                    };
                                    if let Some(observed) = terminal {
                                        let turn_id = observed.turn.id.clone();
                                        operator_inbox
                                            .push_terminal(
                                                format!("turn:{thread_id}:{turn_id}"),
                                                json!({
                                                    "kind":"turnTerminal",
                                                    "threadId":thread_id,
                                                    "turnId":turn_id,
                                                    "mode":observed.mode,
                                                    "status":observed.turn.status,
                                                }),
                                            )
                                            .await;
                                        terminal_turn = Some((thread_id.to_string(), turn_id));
                                    }
                                }
                            }
                            live_turns
                                .lock()
                                .await
                                .observe_event(method, event.get("params").unwrap_or(&Value::Null));
                            journal
                                .push(method, event.get("params").unwrap_or(&Value::Null))
                                .await;
                            if let Some((thread_id, turn_id)) = terminal_turn {
                                relay.observe_terminal_turn(&thread_id, &turn_id).await;
                            }
                            if method == "codexConnect/appServerStopped" {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        journal.mark_gap().await;
                        operator_inbox.mark_history_lost().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

fn validate_work_sandbox_policy(policy: &SandboxPolicy) -> Result<(), RelayError> {
    if let SandboxPolicy::WorkspaceWrite { writable_roots, .. } = policy
        && writable_roots
            .iter()
            .any(|root| !Path::new(root).is_absolute())
    {
        return Err(RelayError::Invalid(
            "writableRoots must contain absolute paths".into(),
        ));
    }
    Ok(())
}

fn sandbox_mode(policy: &SandboxPolicy) -> SandboxMode {
    match policy {
        SandboxPolicy::DangerFullAccess => SandboxMode::DangerFullAccess,
        SandboxPolicy::ReadOnly { .. } => SandboxMode::ReadOnly,
        SandboxPolicy::WorkspaceWrite { .. } => SandboxMode::WorkspaceWrite,
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
    let output = turn
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("agentMessage" | "exitedReviewMode")
            )
        })
        .map(|item| {
            let text = item
                .get("text")
                .or_else(|| item.get("review"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let phase = item.get("phase").and_then(Value::as_str);
            let final_output = phase == Some("final_answer")
                || item.get("type").and_then(Value::as_str) == Some("exitedReviewMode");
            let limit = if final_output { usize::MAX } else { 4_000 };
            let text_chars = text.chars().count();
            json!({
                "id":item.get("id").cloned().unwrap_or(Value::Null),
                "type":item.get("type"),
                "phase":phase,
                "text":text.chars().take(limit).collect::<String>(),
                "truncated":text_chars > limit
            })
        })
        .collect::<Vec<_>>();
    json!({"id":turn.id,"status":turn.status,"error":turn.error,"output":output})
}

fn transcript_entry(
    item: &Value,
    remaining_chars: &mut usize,
    truncated: &mut bool,
) -> Option<Value> {
    let item_type = item.get("type")?.as_str()?;
    let status = item.get("status").cloned().unwrap_or(Value::Null);
    match item_type {
        "reasoning" => Some(json!({
            "kind":"think",
            "title":"THINK",
            "text":Value::Null,
            "status":status,
        })),
        "userMessage" => Some(json!({
            "kind":"user",
            "title":"USER",
            "text":transcript_clip(&message_item_text(item).unwrap_or_default(), remaining_chars, truncated),
            "status":status,
        })),
        "agentMessage" => Some(json!({
            "kind":"agent",
            "title":match item.get("phase").and_then(Value::as_str) {
                Some("final_answer") => "AGENT · FINAL",
                _ => "AGENT",
            },
            "text":transcript_clip(item.get("text").and_then(Value::as_str).unwrap_or_default(), remaining_chars, truncated),
            "status":status,
        })),
        "exitedReviewMode" => Some(json!({
            "kind":"agent",
            "title":"AGENT · REVIEW",
            "text":transcript_clip(item.get("review").and_then(Value::as_str).unwrap_or_default(), remaining_chars, truncated),
            "status":status,
        })),
        "commandExecution" => Some(json!({
            "kind":"command",
            "title":transcript_clip(item.get("command").and_then(Value::as_str).unwrap_or("command"), remaining_chars, truncated),
            "text":transcript_clip(item.get("aggregatedOutput").and_then(Value::as_str).unwrap_or_default(), remaining_chars, truncated),
            "status":status,
        })),
        "fileChange" => Some(json!({
            "kind":"file",
            "title":"FILESYSTEM CHANGE",
            "text":transcript_json(item.get("changes"), remaining_chars, truncated),
            "status":status,
        })),
        "webSearch" => Some(json!({
            "kind":"search",
            "title":"WEB SEARCH",
            "text":transcript_clip(item.get("query").and_then(Value::as_str).unwrap_or_default(), remaining_chars, truncated),
            "status":status,
        })),
        "mcpToolCall" => {
            let title = item
                .get("tool")
                .and_then(Value::as_str)
                .or_else(|| item.get("name").and_then(Value::as_str))
                .unwrap_or("MCP TOOL");
            Some(json!({
                "kind":"tool",
                "title":transcript_clip(title, remaining_chars, truncated),
                "text":transcript_json(item.get("result"), remaining_chars, truncated),
                "status":status,
            }))
        }
        other => Some(json!({
            "kind":"item",
            "title":other,
            "text":Value::Null,
            "status":status,
        })),
    }
}

fn message_item_text(item: &Value) -> Option<String> {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    let parts = item.get("content")?.as_array()?;
    let text = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    Some(text)
}

fn transcript_json(
    value: Option<&Value>,
    remaining_chars: &mut usize,
    truncated: &mut bool,
) -> Value {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Value::Null;
    };
    let rendered = serde_json::to_string_pretty(value).unwrap_or_default();
    transcript_clip(&rendered, remaining_chars, truncated)
}

fn transcript_clip(value: &str, remaining_chars: &mut usize, truncated: &mut bool) -> Value {
    if value.is_empty() {
        return Value::Null;
    }
    if *remaining_chars == 0 {
        *truncated = true;
        return Value::String("… [observer transcript limit reached]".into());
    }
    let limit = MAX_TRANSCRIPT_TEXT_CHARS.min(*remaining_chars);
    let mut chars = value.chars();
    let clipped = chars.by_ref().take(limit).collect::<String>();
    let consumed = clipped.chars().count();
    *remaining_chars = remaining_chars.saturating_sub(consumed);
    if chars.next().is_some() {
        *truncated = true;
        Value::String(format!("{clipped}\n… [entry truncated]"))
    } else {
        Value::String(clipped)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_reasoning_is_phase_only() {
        let mut remaining = MAX_TRANSCRIPT_TOTAL_CHARS;
        let mut truncated = false;
        let entry = transcript_entry(
            &json!({
                "type":"reasoning",
                "summary":"private reasoning",
                "content":[{"text":"hidden"}]
            }),
            &mut remaining,
            &mut truncated,
        )
        .unwrap();
        assert_eq!(entry["kind"], "think");
        assert_eq!(entry["title"], "THINK");
        assert!(entry["text"].is_null());
        assert!(!entry.to_string().contains("private reasoning"));
        assert!(!entry.to_string().contains("hidden"));
        assert!(!truncated);
    }

    #[test]
    fn transcript_preserves_message_newlines_and_bounds_large_tool_output() {
        let mut remaining = MAX_TRANSCRIPT_TOTAL_CHARS;
        let mut truncated = false;
        let message = transcript_entry(
            &json!({"type":"agentMessage","text":"one\ntwo","phase":"commentary"}),
            &mut remaining,
            &mut truncated,
        )
        .unwrap();
        assert_eq!(message["text"], "one\ntwo");
        assert!(!truncated);

        let mut remaining = MAX_TRANSCRIPT_TOTAL_CHARS;
        let mut truncated = false;
        let command = transcript_entry(
            &json!({
                "type":"commandExecution",
                "command":"cargo test",
                "aggregatedOutput":"x".repeat(MAX_TRANSCRIPT_TEXT_CHARS + 1),
            }),
            &mut remaining,
            &mut truncated,
        )
        .unwrap();
        assert!(truncated);
        assert!(
            command["text"]
                .as_str()
                .unwrap()
                .contains("entry truncated")
        );

        let mut remaining = MAX_TRANSCRIPT_TOTAL_CHARS;
        let mut truncated = false;
        let review = transcript_entry(
            &json!({"type":"exitedReviewMode","review":"review finding"}),
            &mut remaining,
            &mut truncated,
        )
        .unwrap();
        assert_eq!(review["title"], "AGENT · REVIEW");
        assert_eq!(review["text"], "review finding");
        assert!(!truncated);
    }
}
