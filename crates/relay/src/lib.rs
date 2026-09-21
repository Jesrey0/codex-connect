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

pub const MAX_WAIT_MS: u64 = 30_000;
const WAIT_FINALIZATION_RESERVE_MS: u64 = 10_000;
const MAX_WAIT_OPERATION_MS: u64 = 40_000;
const WAIT_RECONCILE_MS: u64 = 1_000;
const WAIT_STORAGE_RETRY_MS: u64 = 25;
const MAX_SEMANTIC_EVENTS: usize = 16;
const MAX_TRANSCRIPT_TEXT_CHARS: usize = 32 * 1024;
const MAX_TRANSCRIPT_TOTAL_CHARS: usize = 192 * 1024;
const MAX_TRANSCRIPT_ENTRIES: usize = 512;
const MAX_TRANSCRIPT_SCAN_ITEMS: usize = 4_096;
const MAX_LIVE_MESSAGE_CHARS: usize = 8 * 1024;
const MAX_OBSERVER_PROMPT_CHARS: usize = 8 * 1024;
const MAX_OBSERVER_SUMMARY_PROMPT_CHARS: usize = 512;
const MAX_LIVE_TURNS: usize = 256;
const MAX_RECENT_WORKERS: usize = 8;
const TURN_PAGE_SIZE: u32 = 50;
const ITEM_PAGE_SIZE: u32 = 100;
pub const COMMAND_EXEC_RESPONSE_ALLOWANCE_MS: u64 = 5_000;
pub const DEFAULT_COMMAND_MS: u64 = 30_000;
pub const DEFAULT_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_MS: u64 = 35_000;
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_COMMAND_READ_MS: u64 = 40_000;
pub const DEFAULT_COMMAND_READ_MS: u64 = 20_000;
pub const MAX_COMMAND_WRITE_BYTES: usize = 64 * 1024;
const APP_SERVER_RESPONSE_HEADROOM_BYTES: usize = 64 * 1024;
const MAX_APP_SERVER_RESPONSE_BYTES: usize = MAX_WIRE_BYTES - APP_SERVER_RESPONSE_HEADROOM_BYTES;
const MAX_FS_READ_FILE_BYTES: u64 = ((MAX_APP_SERVER_RESPONSE_BYTES / 4) * 3) as u64;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub codex_bin: PathBuf,
    pub default_cwd: PathBuf,
}

fn review_target_prompt(target: &ReviewTarget) -> String {
    match target {
        ReviewTarget::UncommittedChanges => "Review uncommitted changes".into(),
        ReviewTarget::BaseBranch { branch } => {
            format!("Review changes against base branch {branch}")
        }
        ReviewTarget::Commit { sha, title } => title
            .as_ref()
            .map(|title| format!("Review commit {sha}: {title}"))
            .unwrap_or_else(|| format!("Review commit {sha}")),
        ReviewTarget::Custom { instructions } => instructions.clone(),
    }
}

fn tail_text(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(2);
    let tail = value
        .chars()
        .skip(count.saturating_sub(keep))
        .collect::<String>();
    format!("…\n{tail}")
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
    #[error("Codex operation budget exceeded: {0}")]
    BudgetExceeded(String),
}

fn is_unflushed_thread_store(error: &RelayError) -> bool {
    matches!(
        error,
        RelayError::AppServer(AppServerError::Remote { method, message, .. })
            if matches!(method.as_str(), "thread/read" | "thread/turns/list" | "thread/items/list")
                && message.contains("thread-store internal error")
                && message.contains("rollout")
                && message.contains(" is empty")
    )
}

#[derive(Clone)]
pub struct Relay {
    app_server: Arc<AppServerClient>,
    host: Host,
    journal: event_journal::EventJournal,
    operator_inbox: operator_inbox::OperatorInbox,
    live_turns: Arc<Mutex<LiveTurns>>,
    thread_subscriptions: Arc<Mutex<ThreadSubscriptions>>,
    observer_usage: Arc<Mutex<ObserverUsageState>>,
    command_sessions: command_sessions::CommandSessions,
    command_generation: Arc<str>,
    next_command_id: Arc<AtomicU64>,
}

#[derive(Default)]
struct LiveTurns {
    turns: HashMap<(String, String), ObservedTurn>,
    order: VecDeque<(String, String)>,
    recent: VecDeque<(String, String, ObservedTurn)>,
}

#[derive(Clone)]
struct ObservedTurn {
    turn: codex_connect_app_server::protocol::Turn,
    mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    prompt: Option<String>,
    terminal_at_ms: Option<u64>,
    last_activity_at_ms: u64,
    activity_kind: String,
    activity_summary: Option<String>,
    transcript_revision: u64,
    message_item_id: Option<String>,
    message_excerpt: String,
    token_usage_total: Option<u64>,
    model_context_window: Option<u64>,
}

#[derive(Default)]
struct ObserverUsageState {
    value: Option<Value>,
    error: Option<String>,
    refresh_running: bool,
    refresh_pending: bool,
}

struct WorkerAnnotation {
    mode: String,
    model: Option<String>,
    effort: Option<String>,
    prompt: Option<String>,
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
    fn insert(&mut self, thread_id: &str, mut turn: codex_connect_app_server::protocol::Turn) {
        // Live observer state owns lifecycle/presentation metadata, not App Server-owned
        // turn history. Never retain potentially multi-megabyte item payloads here.
        turn.items.clear();
        let key = (thread_id.to_string(), turn.id.clone());
        if let Some(existing) = self.turns.get_mut(&key) {
            // App Server notifications can race ahead of the turn/start response. Once a
            // terminal lifecycle event has been observed, a later stale inProgress response
            // must not regress the local projection back to active forever.
            if existing.turn.status.is_terminal() && !turn.status.is_terminal() {
                return;
            }
            let became_terminal = !existing.turn.status.is_terminal() && turn.status.is_terminal();
            existing.turn = turn;
            if became_terminal {
                existing.terminal_at_ms = Some(now_epoch_ms());
            }
            return;
        }
        if !self.turns.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        let terminal_at_ms = turn.status.is_terminal().then(now_epoch_ms);
        self.turns.insert(
            key,
            ObservedTurn {
                turn,
                mode: None,
                model: None,
                effort: None,
                prompt: None,
                terminal_at_ms,
                last_activity_at_ms: now_epoch_ms(),
                activity_kind: "turn".into(),
                activity_summary: Some("turn observed".into()),
                transcript_revision: 0,
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

    fn annotate(&mut self, thread_id: &str, turn_id: &str, annotation: WorkerAnnotation) {
        if let Some(observed) = self
            .turns
            .get_mut(&(thread_id.to_string(), turn_id.to_string()))
        {
            observed.mode = Some(annotation.mode);
            observed.model = annotation.model;
            observed.effort = annotation.effort;
            observed.prompt = annotation
                .prompt
                .map(|value| observer_clip(&value, MAX_OBSERVER_PROMPT_CHARS));
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

        if method == "turn/completed"
            || (method == "item/completed"
                && matches!(
                    params["item"]["type"].as_str(),
                    Some("agentMessage" | "exitedReviewMode")
                ))
        {
            observed.transcript_revision = observed.transcript_revision.saturating_add(1);
        }

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
                    observed.message_excerpt =
                        tail_text(&observed.message_excerpt, MAX_LIVE_MESSAGE_CHARS);
                }
                observed.activity_kind = "message".into();
                observed.activity_summary = (!observed.message_excerpt.is_empty())
                    .then(|| compact_text(&observed.message_excerpt, 240));
            }
            return;
        }

        if let Some(next) = activity(method, params) {
            observed.last_activity_at_ms = now_epoch_ms();
            observed.activity_kind = next.kind;
            observed.activity_summary = next.summary;
        }
    }

    fn reconcile_terminal(
        &mut self,
        thread_id: &str,
        turn: &codex_connect_app_server::protocol::Turn,
    ) {
        if !turn.status.is_terminal() {
            return;
        }
        let Some(observed) = self
            .turns
            .get_mut(&(thread_id.to_string(), turn.id.clone()))
        else {
            return;
        };
        observed.turn = turn.clone();
        observed.turn.items.clear();
        observed.terminal_at_ms.get_or_insert_with(now_epoch_ms);
        self.record_recent(thread_id, &turn.id);
    }

    fn record_recent(&mut self, thread_id: &str, turn_id: &str) {
        let key = (thread_id.to_string(), turn_id.to_string());
        let Some(mut observed) = self.turns.get(&key).cloned() else {
            return;
        };
        if observed.mode.is_none() || !observed.turn.status.is_terminal() {
            return;
        }
        observed.turn.items.clear();
        if let Some(index) = self
            .recent
            .iter()
            .position(|(recent_thread, recent_turn, _)| {
                recent_thread == thread_id && recent_turn == turn_id
            })
        {
            self.recent[index] = (thread_id.to_string(), turn_id.to_string(), observed);
            return;
        }
        self.recent
            .push_back((thread_id.to_string(), turn_id.to_string(), observed));
        while self.recent.len() > MAX_RECENT_WORKERS {
            self.recent.pop_front();
        }
    }

    fn observer_workers(&self) -> Vec<Value> {
        let mut active = Vec::new();
        for (thread_id, turn_id) in &self.order {
            let Some(observed) = self.turns.get(&(thread_id.clone(), turn_id.clone())) else {
                continue;
            };
            if observed.mode.is_none() || observed.turn.status.is_terminal() {
                continue;
            }
            active.push(observer_worker_summary_value(thread_id, observed));
        }
        active.extend(
            self.recent
                .iter()
                .rev()
                .map(|(thread_id, _, observed)| observer_worker_summary_value(thread_id, observed)),
        );
        active
    }

    fn observer_context_value(&self, thread_id: &str, turn_id: &str) -> Option<Value> {
        self.turns
            .get(&(thread_id.to_string(), turn_id.to_string()))
            .filter(|observed| observed.mode.is_some())
            .map(|observed| observer_worker_value(thread_id, observed))
            .or_else(|| {
                self.recent
                    .iter()
                    .find(|(recent_thread, recent_turn, _)| {
                        recent_thread == thread_id && recent_turn == turn_id
                    })
                    .map(|(_, _, observed)| observer_worker_value(thread_id, observed))
            })
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

fn observer_worker_value(thread_id: &str, observed: &ObservedTurn) -> Value {
    json!({
        "threadId": thread_id,
        "turnId": observed.turn.id,
        "status": observed.turn.status,
        "mode": observed.mode,
        "model": observed.model,
        "effort": observed.effort,
        "prompt": observed.prompt,
        "terminalAtMs": observed.terminal_at_ms,
        "lastActivityAtMs": observed.last_activity_at_ms,
        "activityKind": observed.activity_kind,
        "activitySummary": observed.activity_summary,
        "transcriptRevision": observed.transcript_revision,
        "tokenUsage": {
            "totalTokens": observed.token_usage_total,
            "modelContextWindow": observed.model_context_window,
        },
    })
}

fn observer_worker_summary_value(thread_id: &str, observed: &ObservedTurn) -> Value {
    json!({
        "threadId": thread_id,
        "turnId": observed.turn.id,
        "status": observed.turn.status,
        "mode": observed.mode,
        "model": observed.model,
        "effort": observed.effort,
        "prompt": observed.prompt.as_deref().map(|value| observer_clip(value, MAX_OBSERVER_SUMMARY_PROMPT_CHARS)),
        "terminalAtMs": observed.terminal_at_ms,
        "lastActivityAtMs": observed.last_activity_at_ms,
        "activityKind": observed.activity_kind,
        "activitySummary": observed.activity_summary,
        "transcriptRevision": observed.transcript_revision,
        "tokenUsage": {
            "totalTokens": observed.token_usage_total,
            "modelContextWindow": observed.model_context_window,
        },
    })
}

fn observer_clip(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    format!("{}…", value.chars().take(keep).collect::<String>())
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
            observer_usage: Arc::new(Mutex::new(ObserverUsageState::default())),
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
        annotation: WorkerAnnotation,
    ) {
        let terminal = {
            let mut live = self.live_turns.lock().await;
            live.annotate(thread_id, turn_id, annotation);
            live.record_recent(thread_id, turn_id);
            live.turns
                .get(&(thread_id.to_string(), turn_id.to_string()))
                .cloned()
                .filter(|observed| observed.turn.status.is_terminal())
        };
        if let Some(observed) = terminal {
            self.push_terminal_worker_event(thread_id, &observed).await;
            self.observe_terminal_turn(thread_id, turn_id).await;
        }
        self.journal
            .push(
                "codexConnect/observerWorkerChanged",
                &json!({"threadId":thread_id,"turnId":turn_id}),
            )
            .await;
        self.trigger_observer_usage_refresh().await;
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

    async fn observer_activity_value(&self, thread_id: &str, turn_id: &str) -> Value {
        let live = self.live_turns.lock().await;
        let Some(observed) = live
            .turns
            .get(&(thread_id.to_string(), turn_id.to_string()))
        else {
            return Value::Null;
        };
        let summary = if observed.activity_kind == "message" && !observed.message_excerpt.is_empty()
        {
            Some(observed.message_excerpt.as_str())
        } else {
            observed.activity_summary.as_deref()
        };
        json!({
            "kind": observed.activity_kind,
            "summary": summary,
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
        model: Option<String>,
        effort: Option<String>,
        sandbox_policy: SandboxPolicy,
    ) -> Result<Value, RelayError> {
        if task.trim().is_empty() {
            return Err(RelayError::Invalid("task must not be empty".into()));
        }
        validate_work_sandbox_policy(&sandbox_policy)?;
        let observer_prompt = task.clone();
        let thread_sandbox = sandbox_mode(&sandbox_policy);
        let cursor = self.journal.cursor().await;
        let created = thread_id.is_none();
        let (thread_id, cwd) = self
            .prepare_thread(cwd, thread_id, None, Some(thread_sandbox))
            .await?;
        let response = match self
            .app_server
            .request(TurnStart {
                thread_id: thread_id.clone(),
                input: vec![TextInput::Text { text: task }],
                cwd,
                approval_policy: Some(ApprovalPolicy::Never),
                sandbox_policy: Some(sandbox_policy),
                model: model.clone(),
                effort: effort.clone(),
                service_tier_for_turn: None,
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
            WorkerAnnotation {
                mode: "work".into(),
                model,
                effort,
                prompt: Some(observer_prompt),
            },
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

    async fn hydrate_turn_items_when_ready(
        &self,
        thread_id: &str,
        turn: &mut codex_connect_app_server::protocol::Turn,
    ) -> Result<(), RelayError> {
        loop {
            match self.hydrate_turn_items(thread_id, turn).await {
                Ok(()) => return Ok(()),
                Err(error) if is_unflushed_thread_store(&error) => {
                    tokio::time::sleep(Duration::from_millis(WAIT_STORAGE_RETRY_MS)).await;
                }
                Err(error) => return Err(error),
            }
        }
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

    async fn read_recent_transcript_entries(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(Vec<Value>, bool), RelayError> {
        let mut cursor = None;
        let mut entries = Vec::new();
        let mut remaining_chars = MAX_TRANSCRIPT_TOTAL_CHARS;
        let mut scanned = 0usize;
        let mut truncated = false;
        let mut reached_history_start = false;
        'pages: loop {
            if scanned >= MAX_TRANSCRIPT_SCAN_ITEMS {
                truncated = true;
                break;
            }
            let page_limit = (MAX_TRANSCRIPT_SCAN_ITEMS - scanned).min(ITEM_PAGE_SIZE as usize);
            let response = self
                .app_server
                .request(ThreadItemsList {
                    thread_id: thread_id.to_string(),
                    turn_id: Some(turn_id.to_string()),
                    cursor,
                    limit: Some(page_limit as u32),
                    sort_direction: Some(SortDirection::Desc),
                })
                .await?;
            let next_cursor = response.next_cursor;
            let page_len = response.data.len();
            for (index, entry) in response.data.into_iter().enumerate() {
                scanned += 1;
                if entries.len() >= MAX_TRANSCRIPT_ENTRIES {
                    truncated = true;
                    break 'pages;
                }
                if remaining_chars == 0 {
                    truncated = true;
                    break 'pages;
                }
                if let Some(projected) =
                    transcript_entry(&entry.item, &mut remaining_chars, &mut truncated)
                {
                    entries.push(projected);
                }
                if remaining_chars == 0 {
                    reached_history_start = index + 1 == page_len && next_cursor.is_none();
                    if index + 1 < page_len || next_cursor.is_some() {
                        truncated = true;
                    }
                    break 'pages;
                }
                if scanned >= MAX_TRANSCRIPT_SCAN_ITEMS {
                    reached_history_start = index + 1 == page_len && next_cursor.is_none();
                    if index + 1 < page_len || next_cursor.is_some() {
                        truncated = true;
                    }
                    break 'pages;
                }
            }
            match next_cursor {
                Some(next) => cursor = Some(next),
                None => {
                    reached_history_start = true;
                    break;
                }
            }
        }
        entries.reverse();
        mark_initial_task_entry(&mut entries, reached_history_start);
        Ok((entries, truncated))
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
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_millis(timeout_ms);
        let operation_deadline = started_at
            + Duration::from_millis(
                timeout_ms
                    .saturating_add(WAIT_FINALIZATION_RESERVE_MS)
                    .min(MAX_WAIT_OPERATION_MS),
            );
        // Subscribe before the authoritative read so actionable requests and terminal
        // notifications cannot race the wait setup.
        let mut transport_changes = self.app_server.changes();
        let mut journal_changes = self.journal.changes();
        let initial_live = self.live_turn(&thread_id, &turn_id).await;
        let mut stored = if initial_live.is_none() {
            // A turn that predates this relay process needs the minimum authoritative read
            // required to establish trustworthy state. Reconciliation may use the finalization
            // reserve, but it must not consume the entire public MCP lifetime.
            let initial_reconcile = async {
                self.read_thread_metadata(thread_id.clone()).await?;
                self.find_stored_turn_metadata(&thread_id, &turn_id).await
            };
            match tokio::time::timeout_at(operation_deadline, initial_reconcile).await {
                Ok(result) => result?,
                Err(_) => {
                    return Err(RelayError::BudgetExceeded(format!(
                        "codex.wait could not reconcile turn state within {} ms",
                        timeout_ms
                            .saturating_add(WAIT_FINALIZATION_RESERVE_MS)
                            .min(MAX_WAIT_OPERATION_MS)
                    )));
                }
            }
        } else {
            // A turn started by this relay is already represented by the official turn/start
            // response and live lifecycle notifications, and its thread cwd was validated during
            // thread/start or thread/resume. Avoid an immediate thread/read here: upstream may
            // acknowledge a new turn before its rollout metadata has been flushed to disk. Keep
            // the turn-list reconciliation so terminal state remains authoritative even when an
            // upstream lifecycle notification is absent.
            match tokio::time::timeout_at(
                deadline,
                self.find_stored_turn_metadata(&thread_id, &turn_id),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(error)) if is_unflushed_thread_store(&error) => None,
                Ok(Err(error)) => return Err(error),
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
            if selected.status.is_terminal() && selected.items.is_empty() {
                match tokio::time::timeout_at(
                    operation_deadline,
                    self.hydrate_turn_items_when_ready(&thread_id, &mut selected),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        return Err(RelayError::BudgetExceeded(
                            "codex.wait reached terminal state but terminal output hydration exceeded the reserved finalization budget; inspect the completed turn explicitly"
                                .into(),
                        ));
                    }
                }
            }
            if selected.status.is_terminal() {
                self.live_turns
                    .lock()
                    .await
                    .reconcile_terminal(&thread_id, &selected);
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
                    Ok(Ok(result)) => stored = result,
                    Ok(Err(error)) if is_unflushed_thread_store(&error) => {
                        // A just-created App Server rollout can briefly exist before its session
                        // metadata is readable. The relay-owned live turn remains authoritative
                        // for this interval; retry persistence reconciliation on the next lease.
                    }
                    Ok(Err(error)) => return Err(error),
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
        let observer_prompt = review_target_prompt(&target);
        let (thread_id, _) = self
            .prepare_thread(
                cwd,
                thread_id,
                model,
                created.then_some(SandboxMode::ReadOnly),
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
            WorkerAnnotation {
                mode: "review".into(),
                model: observer_model,
                effort: None,
                prompt: Some(observer_prompt),
            },
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

    pub async fn model_list(&self) -> Result<Value, RelayError> {
        let mut cursor = None;
        let mut data = Vec::new();
        let mut seen_cursors = HashSet::new();
        loop {
            let response = self
                .app_server
                .request(ModelList {
                    include_hidden: None,
                    cursor,
                    limit: None,
                })
                .await?;
            data.extend(response.data);
            let Some(next_cursor) = response.next_cursor else {
                return Ok(json!({"data":data,"nextCursor":null}));
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(RelayError::Invalid(
                    "model/list returned a repeated pagination cursor".into(),
                ));
            }
            cursor = Some(next_cursor);
        }
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

    async fn store_observer_usage_result(&self, result: Result<Value, RelayError>) {
        let (ok, error) = {
            let mut state = self.observer_usage.lock().await;
            match result {
                Ok(value) => {
                    state.value = Some(value);
                    state.error = None;
                    (true, None)
                }
                Err(error) => {
                    let message = error.to_string();
                    state.error = Some(message.clone());
                    (false, Some(message))
                }
            }
        };
        self.journal
            .push(
                "codexConnect/observerUsageUpdated",
                &json!({"ok":ok,"error":error}),
            )
            .await;
    }

    async fn trigger_observer_usage_refresh(&self) {
        let should_spawn = {
            let mut state = self.observer_usage.lock().await;
            if state.refresh_running {
                state.refresh_pending = true;
                false
            } else {
                state.refresh_running = true;
                true
            }
        };

        if should_spawn {
            let relay = self.clone();
            tokio::spawn(async move {
                loop {
                    let result = relay.usage().await;
                    relay.store_observer_usage_result(result).await;
                    let continue_refreshing = {
                        let mut state = relay.observer_usage.lock().await;
                        let pending = state.refresh_pending;
                        state.refresh_pending = false;
                        if !pending {
                            state.refresh_running = false;
                        }
                        pending
                    };
                    if !continue_refreshing {
                        break;
                    }
                }
            });
        }
    }

    async fn observer_projection(&self) -> Value {
        let (usage, usage_error) = {
            let cached = self.observer_usage.lock().await;
            (
                cached.value.clone().unwrap_or(Value::Null),
                cached.error.clone(),
            )
        };
        let workers = {
            let live = self.live_turns.lock().await;
            live.observer_workers()
        };
        let pending_actions = self.pending_actions(None).await;
        let notices = self.journal.observer_notices(8).await;
        json!({
            "cwd": self.default_cwd(),
            "usage": usage,
            "usageError": usage_error,
            "workers": workers,
            "pendingActions": pending_actions,
            "notices": notices,
        })
    }

    pub async fn observer_snapshot(&self) -> Value {
        self.trigger_observer_usage_refresh().await;
        json!({
            "cursor": self.journal.cursor().await,
            "projection": self.observer_projection().await,
        })
    }

    pub async fn observer_wait(&self, after_cursor: u64) -> Result<Value, RelayError> {
        let mut changes = self.journal.changes();
        let current = self.journal.cursor().await;
        if after_cursor > current {
            return Err(RelayError::Invalid(
                "observer cursor is ahead of this backend; hydrate /observe again".into(),
            ));
        }
        if current == after_cursor {
            changes
                .changed()
                .await
                .map_err(|_| AppServerError::Disconnected)?;
        }
        Ok(json!({
            "cursor": self.journal.cursor().await,
            "projection": self.observer_projection().await,
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
        if selected.status.is_terminal() {
            self.live_turns
                .lock()
                .await
                .reconcile_terminal(&thread_id, &selected);
        }
        let (entries, truncated) = self
            .read_recent_transcript_entries(&thread_id, &turn_id)
            .await?;
        let context = {
            let live = self.live_turns.lock().await;
            live.observer_context_value(&thread_id, &turn_id)
        };
        let pending_actions = self
            .pending_actions(Some(&thread_id))
            .await
            .into_iter()
            .filter(|action| {
                action["turnId"]
                    .as_str()
                    .is_none_or(|candidate| candidate == turn_id)
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "status": selected.status,
            "context": context,
            "activity": self.observer_activity_value(&thread_id, &turn_id).await,
            "pendingActions": pending_actions,
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
                            if let Some((thread_id, turn_id)) = terminal_turn.as_ref() {
                                live_turns.lock().await.record_recent(thread_id, turn_id);
                            }
                            let params = event.get("params").unwrap_or(&Value::Null);
                            journal.push(method, params).await;
                            let refresh_usage = method == "turn/completed"
                                || (method == "item/completed"
                                    && matches!(
                                        params["item"]["type"].as_str(),
                                        Some("agentMessage" | "exitedReviewMode")
                                    ));
                            if refresh_usage {
                                relay.trigger_observer_usage_refresh().await;
                            }
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
        "reasoning" => None,
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
        _ => None,
    }
}

fn mark_initial_task_entry(entries: &mut [Value], reached_history_start: bool) {
    if !reached_history_start {
        return;
    }
    if let Some(initial) = entries
        .iter_mut()
        .find(|entry| entry["kind"].as_str() == Some("user"))
    {
        initial["initialTask"] = Value::Bool(true);
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
    use codex_connect_app_server::protocol::{Turn, TurnStatus};

    fn test_turn(id: &str, status: TurnStatus) -> Turn {
        Turn {
            id: id.into(),
            status,
            items: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn transcript_hides_reasoning_and_tool_noise() {
        let mut remaining = MAX_TRANSCRIPT_TOTAL_CHARS;
        let mut truncated = false;
        let reasoning = transcript_entry(
            &json!({
                "type":"reasoning",
                "summary":"private reasoning",
                "content":[{"text":"hidden"}]
            }),
            &mut remaining,
            &mut truncated,
        );
        let command = transcript_entry(
            &json!({
                "type":"commandExecution",
                "command":"cargo test",
                "aggregatedOutput":"very noisy output",
            }),
            &mut remaining,
            &mut truncated,
        );
        assert!(reasoning.is_none());
        assert!(command.is_none());
        assert!(!truncated);
    }

    #[test]
    fn transcript_preserves_human_message_newlines() {
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

    #[test]
    fn observer_metadata_is_bounded() {
        let mut live = LiveTurns::default();
        live.insert("thread", test_turn("turn", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "turn",
            WorkerAnnotation {
                mode: "work".into(),
                model: None,
                effort: None,
                prompt: Some("x".repeat(MAX_OBSERVER_PROMPT_CHARS + 10)),
            },
        );
        let observed = live
            .turns
            .get(&("thread".to_string(), "turn".to_string()))
            .unwrap();
        let prompt = observed.prompt.as_ref().unwrap();
        assert_eq!(prompt.chars().count(), MAX_OBSERVER_PROMPT_CHARS);
        assert!(prompt.ends_with('…'));
    }

    #[test]
    fn observer_worker_summary_bounds_prompt_context() {
        let mut live = LiveTurns::default();
        live.insert("thread", test_turn("turn", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "turn",
            WorkerAnnotation {
                mode: "work".into(),
                model: Some("gpt-5.6-sol".into()),
                effort: Some("high".into()),
                prompt: Some("x".repeat(MAX_OBSERVER_PROMPT_CHARS)),
            },
        );
        let worker = live.observer_workers().pop().unwrap();
        assert!(
            worker["prompt"].as_str().unwrap().chars().count() <= MAX_OBSERVER_SUMMARY_PROMPT_CHARS
        );
        assert!(serde_json::to_vec(&worker).unwrap().len() < 4 * 1024);
    }

    #[test]
    fn transcript_revision_tracks_human_visible_completion_boundaries() {
        let mut live = LiveTurns::default();
        live.insert("thread", test_turn("turn", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "turn",
            WorkerAnnotation {
                mode: "work".into(),
                model: None,
                effort: None,
                prompt: Some("task".into()),
            },
        );

        live.observe_event(
            "item/agentMessage/delta",
            &json!({"threadId":"thread","turnId":"turn","delta":"hello"}),
        );
        assert_eq!(live.observer_workers()[0]["transcriptRevision"], 0);

        live.observe_event(
            "item/completed",
            &json!({
                "threadId":"thread",
                "turnId":"turn",
                "item":{"type":"reasoning"}
            }),
        );
        assert_eq!(live.observer_workers()[0]["transcriptRevision"], 0);

        live.observe_event(
            "item/completed",
            &json!({
                "threadId":"thread",
                "turnId":"turn",
                "item":{"type":"agentMessage","text":"hello"}
            }),
        );
        assert_eq!(live.observer_workers()[0]["transcriptRevision"], 1);

        live.observe_event(
            "turn/completed",
            &json!({"threadId":"thread","turn":{"id":"turn","status":"completed"}}),
        );
        assert_eq!(live.observer_workers()[0]["transcriptRevision"], 2);
    }

    #[test]
    fn terminal_recent_projection_keeps_reduced_transcript_revision() {
        let mut live = LiveTurns::default();
        live.insert("thread", test_turn("turn", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "turn",
            WorkerAnnotation {
                mode: "work".into(),
                model: None,
                effort: None,
                prompt: Some("task".into()),
            },
        );
        live.insert("thread", test_turn("turn", TurnStatus::Completed));
        live.observe_event(
            "turn/completed",
            &json!({"threadId":"thread","turn":{"id":"turn","status":"completed"}}),
        );
        live.record_recent("thread", "turn");

        let worker = live.observer_workers().pop().unwrap();
        assert_eq!(worker["status"], "completed");
        assert_eq!(worker["transcriptRevision"], 1);
    }

    #[test]
    fn initial_task_marker_does_not_hide_repeated_followup() {
        let mut entries = vec![
            json!({"kind":"user","text":"repeat"}),
            json!({"kind":"user","text":"repeat"}),
            json!({"kind":"agent","text":"answer"}),
        ];
        mark_initial_task_entry(&mut entries, true);
        assert_eq!(entries[0]["initialTask"], true);
        assert!(entries[1].get("initialTask").is_none());

        let mut truncated_entries = entries.clone();
        for entry in &mut truncated_entries {
            entry.as_object_mut().unwrap().remove("initialTask");
        }
        mark_initial_task_entry(&mut truncated_entries, false);
        assert!(
            truncated_entries
                .iter()
                .all(|entry| entry.get("initialTask").is_none())
        );
    }

    #[test]
    fn live_message_keeps_large_console_excerpt_but_compact_operator_summary() {
        let mut live = LiveTurns::default();
        live.insert("thread", test_turn("turn", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "turn",
            WorkerAnnotation {
                mode: "work".into(),
                model: None,
                effort: None,
                prompt: None,
            },
        );
        live.observe_event(
            "item/agentMessage/delta",
            &json!({
                "threadId":"thread",
                "turnId":"turn",
                "itemId":"message",
                "delta":"x".repeat(2_000),
            }),
        );
        let observed = live
            .turns
            .get(&("thread".to_string(), "turn".to_string()))
            .unwrap();
        assert_eq!(observed.message_excerpt.chars().count(), 2_000);
        assert!(observed.activity_summary.as_ref().unwrap().chars().count() <= 241);
    }

    #[test]
    fn recent_workers_are_ranked_by_terminal_recency() {
        let mut live = LiveTurns::default();
        live.insert("thread", test_turn("turn-0", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "turn-0",
            WorkerAnnotation {
                mode: "work".into(),
                model: None,
                effort: None,
                prompt: Some("turn-0".into()),
            },
        );
        for index in 1..10 {
            let turn_id = format!("turn-{index}");
            live.insert("thread", test_turn(&turn_id, TurnStatus::Completed));
            live.annotate(
                "thread",
                &turn_id,
                WorkerAnnotation {
                    mode: "work".into(),
                    model: None,
                    effort: None,
                    prompt: Some(turn_id.clone()),
                },
            );
            live.record_recent("thread", &turn_id);
        }
        live.reconcile_terminal("thread", &test_turn("turn-0", TurnStatus::Completed));

        let workers = live.observer_workers();
        assert_eq!(workers.len(), 8);
        assert_eq!(workers[0]["turnId"], "turn-0");
        assert_eq!(workers[1]["turnId"], "turn-9");
        assert!(!workers.iter().any(|worker| worker["turnId"] == "turn-1"));
    }

    #[test]
    fn recent_workers_drop_terminal_turn_items() {
        let mut live = LiveTurns::default();
        let mut turn = test_turn("turn", TurnStatus::Completed);
        turn.items =
            vec![json!({"type":"commandExecution","aggregatedOutput":"x".repeat(32 * 1024)})];
        live.insert("thread", turn);
        live.annotate(
            "thread",
            "turn",
            WorkerAnnotation {
                mode: "work".into(),
                model: None,
                effort: None,
                prompt: Some("task".into()),
            },
        );
        live.record_recent("thread", "turn");
        assert!(live.recent.back().unwrap().2.turn.items.is_empty());
    }
}
