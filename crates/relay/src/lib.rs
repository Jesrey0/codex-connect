//! ChatGPT-oriented composition over the pinned official App Server.

mod actions;
mod activity;
mod command_sessions;
mod event_journal;
#[cfg(test)]
mod terminal_tests;
mod thread_subscriptions;

pub use actions::{
    ApprovalDecision, ElicitationAction, PermissionGrant, PermissionScope,
    operator_approval_decision,
};
use activity::{activity, compact_text};
use base64::Engine;
pub use codex_connect_app_server::APP_SERVER_LAUNCH_OVERRIDES;
pub use codex_connect_app_server::protocol::{
    ApprovalPolicy, CommandExec, CommandExecTerminalSize, ModelList, ReviewTarget, RpcId,
    SandboxMode, SandboxPolicy,
};
use codex_connect_app_server::protocol::{
    CommandExecOutputDeltaNotification, CommandExecResize, CommandExecTerminate, CommandExecWrite,
    FsGetMetadata, FsGetMetadataResponse, FsReadDirectory, FsReadDirectoryResponse, FsReadFile,
    FuzzyFileSearch, FuzzyFileSearchResponse, RateLimitsRead, ReviewStart, SkillsList,
    SortDirection, StreamingCommandExec, TextInput, Thread, ThreadArchive,
    ThreadBackgroundTerminalsList, ThreadBackgroundTerminalsTerminate, ThreadDelete, ThreadFork,
    ThreadItemsList, ThreadItemsListResponse, ThreadList, ThreadRead, ThreadResume, ThreadSortKey,
    ThreadStart, ThreadTurnsList, ThreadUnarchive, ThreadUnsubscribe, TurnInterrupt, TurnItemsView,
    TurnStart, TurnSteer,
};
use codex_connect_app_server::{
    AppServerClient, AppServerConfig, AppServerError, DEFAULT_REQUEST_TIMEOUT, DeferredRequest,
    MAX_WIRE_BYTES,
};
pub use codex_connect_app_server::{PendingActionKind, PendingServerRequest};
pub const CODEX_RELEASE: &str = codex_connect_app_server::protocol::CODEX_PIN;
use codex_connect_host::{Host, HostError, MAX_IMAGE_BYTES};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use thread_subscriptions::ThreadSubscriptions;
use tokio::sync::{Mutex, oneshot};
use tokio::time::{Duration, Instant};

const WORK_WAIT_MS: u64 = 33_000;
const WAIT_FINALIZATION_RESERVE_MS: u64 = 10_000;
pub const WORK_WAIT_OPERATION_MS: u64 = WORK_WAIT_MS + WAIT_FINALIZATION_RESERVE_MS;
const WAIT_FINAL_RECONCILE_MS: u64 = 500;
const WAIT_STORAGE_RETRY_MS: u64 = 25;
const WAIT_STORAGE_RETRY_MAX_MS: u64 = 500;
const MAX_SEMANTIC_EVENTS: usize = 16;
const MAX_TRANSCRIPT_TEXT_CHARS: usize = 32 * 1024;
const MAX_TRANSCRIPT_TOTAL_CHARS: usize = 192 * 1024;
const MAX_TRANSCRIPT_ENTRIES: usize = 512;
const MAX_TRANSCRIPT_SCAN_ITEMS: usize = 4_096;
const MAX_LIVE_MESSAGE_CHARS: usize = 8 * 1024;
const MAX_OBSERVER_PROMPT_CHARS: usize = 8 * 1024;
const MAX_OBSERVER_SUMMARY_PROMPT_CHARS: usize = 512;
const MAX_UNANNOTATED_TERMINALS: usize = 256;
const MAX_RECENT_WORKERS: usize = 32;
const MAX_WAIT_SCAN_ITEMS: usize = 512;
// Keep the normal codex.wait handoff small enough for an operator context window.
const MAX_WAIT_HANDOFF_CHARS: usize = 10 * 1024;
const MAX_WAIT_OUTPUT_PAGES: usize = 32;
const RESULT_SELECTION_BUDGET_MS: u64 = 30_000;
const MAX_TRANSCRIPT_PAGES: usize = 128;
const MAX_TURN_LOOKUP_PAGES: usize = 1_024;
const THREAD_REUSE_POLICY_MS: u64 = 30 * 60 * 1000;
const THREAD_REUSE_POLICY_SECS: i64 = 30 * 60;
const TURN_PAGE_SIZE: u32 = 50;
const ITEM_PAGE_SIZE: u32 = 100;
const CODEX_QUERY_PAGE_DEFAULT: u32 = 25;
const CODEX_QUERY_PAGE_MAX: u32 = 50;
const MAX_THREAD_PREVIEW_CHARS: usize = 512;
const MAX_BACKGROUND_COMMAND_CHARS: usize = 1024;
pub const COMMAND_EXEC_RESPONSE_ALLOWANCE_MS: u64 = 10_000;
pub const DEFAULT_COMMAND_MS: u64 = 33_000;
pub const DEFAULT_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_MS: u64 = 45_000;
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_COMMAND_READ_MS: u64 = 43_000;
pub const DEFAULT_COMMAND_READ_MS: u64 = 40_000;
pub const DEFAULT_COMMAND_YIELD_MS: u64 = 1_000;
pub const MAX_COMMAND_YIELD_MS: u64 = 10_000;
pub const MAX_COMMAND_WRITE_BYTES: usize = 64 * 1024;
const APP_SERVER_RESPONSE_HEADROOM_BYTES: usize = 64 * 1024;
const MAX_APP_SERVER_RESPONSE_BYTES: usize = MAX_WIRE_BYTES - APP_SERVER_RESPONSE_HEADROOM_BYTES;
const MAX_FS_READ_FILE_BYTES: u64 = ((MAX_APP_SERVER_RESPONSE_BYTES / 4) * 3) as u64;

fn result_selection_deadline(started_at: Instant) -> Instant {
    started_at + Duration::from_millis(RESULT_SELECTION_BUDGET_MS)
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

fn checked_next_cursor(
    seen: &mut HashSet<String>,
    next: Option<String>,
    method: &str,
) -> Result<Option<String>, RelayError> {
    if let Some(next) = next.as_ref()
        && !seen.insert(next.clone())
    {
        return Err(RelayError::Invalid(format!(
            "{method} returned a repeated pagination cursor"
        )));
    }
    Ok(next)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalTurn {
    pub thread_id: String,
    pub turn_id: String,
    pub status: String,
    pub timestamp_ms: u64,
}

type TerminalObserver = Arc<dyn Fn(TerminalTurn) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
pub struct Relay {
    app_server: Arc<AppServerClient>,
    host: Host,
    journal: event_journal::EventJournal,
    live_turns: Arc<Mutex<LiveTurns>>,
    thread_subscriptions: Arc<Mutex<ThreadSubscriptions>>,
    observer_usage: Arc<Mutex<ObserverUsageState>>,
    command_sessions: command_sessions::CommandSessions,
    command_generation: Arc<str>,
    next_command_id: Arc<AtomicU64>,
    terminal_observer: Arc<std::sync::OnceLock<TerminalObserver>>,
}

// A bounded observation attempt may be cancelled while resume is in flight.
// Always release its temporary start claim so terminal cleanup can proceed.
struct ObservationStart {
    relay: Relay,
    thread_id: String,
    armed: bool,
}

impl ObservationStart {
    async fn finish(&mut self, turn_id: Option<&str>) {
        self.relay
            .finish_thread_start(&self.thread_id, turn_id)
            .await;
        self.armed = false;
    }
}

impl Drop for ObservationStart {
    fn drop(&mut self) {
        if self.armed {
            let relay = self.relay.clone();
            let thread = self.thread_id.clone();
            tokio::spawn(async move { relay.finish_thread_start(&thread, None).await });
        }
    }
}

#[derive(Default)]
struct LiveTurns {
    // Active turns stay here until terminal. An unannotated terminal may stay briefly
    // while its turn/start response catches up with an early completion event.
    turns: HashMap<(String, String), ObservedTurn>,
    order: VecDeque<(String, String)>,
    // App Server usage updates are cumulative thread snapshots, including on resume replay.
    thread_token_usage_totals: HashMap<String, u64>,
    // Only the newest delegated terminal observations remain in the console view.
    recent: VecDeque<(String, String, ObservedTurn)>,
}

#[derive(Clone)]
struct ObservedTurn {
    turn: codex_connect_app_server::protocol::Turn,
    cwd: Option<String>,
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
    thread_total_tokens: Option<u64>,
    last_request_model_context_window: Option<u64>,
    last_request_input_tokens: Option<u64>,
    last_request_cached_input_tokens: Option<u64>,
    last_model_usage_at_ms: Option<u64>,
}

#[derive(Default)]
struct ObserverUsageState {
    value: Option<Value>,
    error: Option<String>,
    updated_at_ms: Option<u64>,
    refresh_running: bool,
    refresh_pending: bool,
}

#[derive(Clone)]
struct WorkerAnnotation {
    mode: String,
    cwd: String,
    model: Option<String>,
    effort: Option<String>,
    prompt: Option<String>,
}

struct PreparedThread {
    id: String,
    cwd: String,
    model: Option<String>,
    effort: Option<String>,
}

fn validate_thread_model(requested: &str, canonical: Option<&str>) -> Result<(), RelayError> {
    if canonical == Some(requested) {
        Ok(())
    } else {
        Err(RelayError::Invalid(format!(
            "model {requested:?} does not match the canonical thread model {:?}",
            canonical
        )))
    }
}

struct TerminalOutput {
    handoff_item: Option<Value>,
    selection_complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectDetail {
    Semantic,
    Raw,
    Result,
}

fn turn_metadata(
    turn: &codex_connect_app_server::protocol::Turn,
) -> codex_connect_app_server::protocol::Turn {
    codex_connect_app_server::protocol::Turn {
        id: turn.id.clone(),
        status: turn.status,
        items: Vec::new(),
        error: turn.error.clone(),
        completed_at: turn.completed_at,
    }
}

impl LiveTurns {
    fn has_active_thread(&self, thread_id: &str) -> bool {
        self.turns.iter().any(|((candidate, _), observed)| {
            candidate == thread_id && !observed.turn.status.is_terminal()
        })
    }

    fn insert(
        &mut self,
        thread_id: &str,
        mut turn: codex_connect_app_server::protocol::Turn,
    ) -> bool {
        // Live observer state owns lifecycle/presentation metadata, not App Server-owned
        // turn history. Never retain potentially multi-megabyte item payloads here.
        turn.items.clear();
        let key = (thread_id.to_string(), turn.id.clone());
        if let Some((_, _, observed)) =
            self.recent
                .iter_mut()
                .find(|(recent_thread, recent_turn, _)| {
                    recent_thread == thread_id && recent_turn == &turn.id
                })
        {
            if turn.status.is_terminal() {
                observed.turn = turn;
            }
            return false;
        }
        if let Some(existing) = self.turns.get_mut(&key) {
            // App Server notifications can race ahead of the turn/start response. Once a
            // terminal lifecycle event has been observed, a later stale inProgress response
            // must not regress the local projection back to active forever.
            if existing.turn.status.is_terminal() && !turn.status.is_terminal() {
                return false;
            }
            let became_terminal = !existing.turn.status.is_terminal() && turn.status.is_terminal();
            existing.turn = turn;
            if became_terminal {
                existing.terminal_at_ms = Some(now_epoch_ms());
            }
            return self.prune_unannotated_terminals();
        }
        self.order.push_back(key.clone());
        let terminal_at_ms = turn.status.is_terminal().then(now_epoch_ms);
        self.turns.insert(
            key,
            ObservedTurn {
                turn,
                cwd: None,
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
                thread_total_tokens: None,
                last_request_model_context_window: None,
                last_request_input_tokens: None,
                last_request_cached_input_tokens: None,
                last_model_usage_at_ms: None,
            },
        );
        self.prune_unannotated_terminals()
    }

    fn prune_unannotated_terminals(&mut self) -> bool {
        let mut terminal_count = self
            .turns
            .values()
            .filter(|turn| turn.mode.is_none() && turn.turn.status.is_terminal())
            .count();
        let mut history_lost = false;
        while terminal_count > MAX_UNANNOTATED_TERMINALS {
            let Some(index) = self.order.iter().position(|key| {
                self.turns
                    .get(key)
                    .is_some_and(|turn| turn.mode.is_none() && turn.turn.status.is_terminal())
            }) else {
                break;
            };
            if let Some(key) = self.order.remove(index) {
                self.turns.remove(&key);
                terminal_count -= 1;
                history_lost = true;
            }
        }
        history_lost
    }

    fn get(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Option<codex_connect_app_server::protocol::Turn> {
        self.observed(thread_id, turn_id)
            .map(|observed| observed.turn.clone())
    }

    fn observed(&self, thread_id: &str, turn_id: &str) -> Option<&ObservedTurn> {
        self.turns
            .get(&(thread_id.to_string(), turn_id.to_string()))
            .or_else(|| {
                self.recent
                    .iter()
                    .find(|(recent_thread, recent_turn, _)| {
                        recent_thread == thread_id && recent_turn == turn_id
                    })
                    .map(|(_, _, observed)| observed)
            })
    }

    fn annotate(&mut self, thread_id: &str, turn_id: &str, annotation: WorkerAnnotation) {
        let observed = self
            .turns
            .get_mut(&(thread_id.to_string(), turn_id.to_string()))
            .or_else(|| {
                self.recent
                    .iter_mut()
                    .find(|(recent_thread, recent_turn, _)| {
                        recent_thread == thread_id && recent_turn == turn_id
                    })
                    .map(|(_, _, observed)| observed)
            });
        if let Some(observed) = observed {
            observed.mode = Some(annotation.mode);
            observed.cwd = Some(annotation.cwd);
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

        if method == "thread/tokenUsage/updated" {
            let usage = &params["tokenUsage"];
            let Some(thread_total_tokens) = usage["total"]["totalTokens"].as_u64() else {
                return;
            };
            let turn_id = params.get("turnId").and_then(Value::as_str);
            let observed = turn_id.and_then(|turn_id| {
                self.turns
                    .get_mut(&(thread_id.to_string(), turn_id.to_string()))
                    .or_else(|| {
                        self.recent
                            .iter_mut()
                            .find(|(recent_thread, recent_turn, _)| {
                                recent_thread == thread_id && recent_turn == turn_id
                            })
                            .map(|(_, _, observed)| observed)
                    })
            });
            let previous_total = self
                .thread_token_usage_totals
                .insert(thread_id.to_string(), thread_total_tokens);
            let usage_advanced = previous_total
                .map(|previous| thread_total_tokens > previous)
                // A nonzero first snapshot on an observed live turn establishes usage from
                // the implicit zero baseline. An unassociated resume replay only seeds it.
                .unwrap_or(observed.is_some() && thread_total_tokens > 0);

            if let Some(observed) = observed {
                observed.thread_total_tokens = Some(thread_total_tokens);
                if usage_advanced {
                    observed.last_request_model_context_window =
                        usage["modelContextWindow"].as_u64();
                    let last = &usage["last"];
                    observed.last_request_input_tokens = last["inputTokens"].as_u64();
                    observed.last_request_cached_input_tokens = last["cachedInputTokens"].as_u64();
                    observed.last_model_usage_at_ms = Some(now_epoch_ms());
                }
            }
            return;
        }

        let turn_id = params.get("turnId").and_then(Value::as_str).or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
        });
        let Some(turn_id) = turn_id else {
            return;
        };
        let observed = self
            .turns
            .get_mut(&(thread_id.to_string(), turn_id.to_string()))
            .or_else(|| {
                self.recent
                    .iter_mut()
                    .find(|(recent_thread, recent_turn, _)| {
                        recent_thread == thread_id && recent_turn == turn_id
                    })
                    .map(|(_, _, observed)| observed)
            });
        let Some(observed) = observed else {
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
    ) -> (Option<ObservedTurn>, bool) {
        if !turn.status.is_terminal() {
            return (None, false);
        }
        let changed;
        if let Some(observed) = self
            .turns
            .get_mut(&(thread_id.to_string(), turn.id.clone()))
        {
            changed = observed.mode.is_some()
                || observed.turn.status != turn.status
                || observed.turn.error != turn.error;
            observed.turn = turn_metadata(turn);
            observed.terminal_at_ms.get_or_insert_with(now_epoch_ms);
        } else if let Some((_, _, observed)) =
            self.recent
                .iter_mut()
                .find(|(recent_thread, recent_turn, _)| {
                    recent_thread == thread_id && recent_turn == &turn.id
                })
        {
            changed = observed.turn.status != turn.status || observed.turn.error != turn.error;
            observed.turn = turn_metadata(turn);
        } else {
            return (None, false);
        }
        self.record_recent(thread_id, &turn.id);
        (
            self.recent
                .iter()
                .find(|(recent_thread, recent_turn, _)| {
                    recent_thread == thread_id && recent_turn == &turn.id
                })
                .map(|(_, _, observed)| observed.clone()),
            changed,
        )
    }

    fn record_recent(&mut self, thread_id: &str, turn_id: &str) {
        let key = (thread_id.to_string(), turn_id.to_string());
        let Some(observed) = self.turns.get(&key) else {
            return;
        };
        if observed.mode.is_none() || !observed.turn.status.is_terminal() {
            return;
        }
        let Some(mut observed) = self.turns.remove(&key) else {
            return;
        };
        self.order.retain(|candidate| candidate != &key);
        observed.turn.items.clear();
        self.recent
            .push_back((thread_id.to_string(), turn_id.to_string(), observed));
        while self.recent.len() > MAX_RECENT_WORKERS {
            let mut counts = HashMap::<&str, usize>::new();
            for (_, _, turn) in &self.recent {
                *counts.entry(turn.cwd.as_deref().unwrap_or("")).or_default() += 1;
            }
            let largest = counts.values().copied().max().unwrap_or(0);
            let index = self
                .recent
                .iter()
                .position(|(_, _, turn)| {
                    counts.get(turn.cwd.as_deref().unwrap_or("")).copied() == Some(largest)
                })
                .unwrap();
            self.recent.remove(index);
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

    fn operator_worker_handles(&self) -> Vec<Value> {
        let mut active = Vec::new();
        for (thread_id, turn_id) in &self.order {
            let Some(observed) = self.turns.get(&(thread_id.clone(), turn_id.clone())) else {
                continue;
            };
            if observed.mode.is_none() || observed.turn.status.is_terminal() {
                continue;
            }
            active.push(operator_worker_handle_value(thread_id, observed));
        }
        active.extend(
            self.recent
                .iter()
                .rev()
                .map(|(thread_id, _, observed)| operator_worker_handle_value(thread_id, observed)),
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
        "cwd": observed.cwd,
        "model": observed.model,
        "effort": observed.effort,
        "prompt": observed.prompt,
        "terminalAtMs": observed.terminal_at_ms,
        "lastActivityAtMs": observed.last_activity_at_ms,
        "activityKind": observed.activity_kind,
        "activitySummary": observed.activity_summary,
        "transcriptRevision": observed.transcript_revision,
        "tokenUsage": token_usage_value(observed),
    })
}

fn observer_worker_summary_value(thread_id: &str, observed: &ObservedTurn) -> Value {
    json!({
        "threadId": thread_id,
        "turnId": observed.turn.id,
        "status": observed.turn.status,
        "mode": observed.mode,
        "cwd": observed.cwd,
        "model": observed.model,
        "effort": observed.effort,
        "prompt": observed.prompt.as_deref().map(|value| observer_clip(value, MAX_OBSERVER_SUMMARY_PROMPT_CHARS)),
        "terminalAtMs": observed.terminal_at_ms,
        "lastActivityAtMs": observed.last_activity_at_ms,
        "activityKind": observed.activity_kind,
        "activitySummary": observed.activity_summary,
        "transcriptRevision": observed.transcript_revision,
        "tokenUsage": token_usage_value(observed),
    })
}

fn operator_worker_handle_value(thread_id: &str, observed: &ObservedTurn) -> Value {
    json!({
        "threadId": thread_id,
        "turnId": observed.turn.id,
        "status": observed.turn.status,
        "mode": observed.mode,
        "cwd": observed.cwd,
        "prompt": observed.prompt.as_deref().map(|value| observer_clip(value, MAX_OBSERVER_SUMMARY_PROMPT_CHARS)),
        "terminalAtMs": observed.terminal_at_ms,
        "lastActivityAtMs": observed.last_activity_at_ms,
        "activityKind": observed.activity_kind,
        "activitySummary": observed.activity_summary,
    })
}

fn token_usage_value(observed: &ObservedTurn) -> Value {
    let cache_hit_percent = match (
        observed.last_request_input_tokens,
        observed.last_request_cached_input_tokens,
    ) {
        (Some(input), Some(cached)) if input > 0 => {
            Some(((u128::from(cached) * 100 / u128::from(input)).min(100)) as u64)
        }
        _ => None,
    };
    let cache_guaranteed_until_ms = observed
        .last_model_usage_at_ms
        .map(|timestamp| timestamp.saturating_add(THREAD_REUSE_POLICY_MS));
    let cache_guarantee_active = cache_guaranteed_until_ms.map(|until| now_epoch_ms() < until);
    json!({
        "threadTotalTokens": observed.thread_total_tokens,
        "lastRequestModelContextWindow": observed.last_request_model_context_window,
        "lastRequestInputTokens": observed.last_request_input_tokens,
        "lastRequestCachedInputTokens": observed.last_request_cached_input_tokens,
        "cacheHitPercent": cache_hit_percent,
        "lastModelUsageAtMs": observed.last_model_usage_at_ms,
        "cacheGuaranteedUntilMs": cache_guaranteed_until_ms,
        "cacheGuaranteeActive": cache_guarantee_active,
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
    pub async fn start(codex_bin: impl AsRef<Path>, host: Host) -> Result<Self, RelayError> {
        let app_server = Arc::new(
            AppServerClient::start(AppServerConfig {
                codex_bin: codex_bin.as_ref().to_path_buf(),
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
            terminal_observer: Arc::new(std::sync::OnceLock::new()),
        };
        relay.start_event_loop();
        Ok(relay)
    }

    pub fn set_terminal_observer(&self, observer: TerminalObserver) -> Result<(), RelayError> {
        self.terminal_observer
            .set(observer)
            .map_err(|_| RelayError::Invalid("terminal observer already installed".into()))
    }

    // Validate canonical upstream IDs and retain observation through the existing owner.
    pub async fn watch_terminal(&self, thread_id: &str, turn_id: &str) -> Result<(), RelayError> {
        let thread = self.read_thread_metadata(thread_id.to_owned()).await?;
        if thread.id != thread_id {
            return Err(RelayError::Invalid("threadId must be canonical".into()));
        }
        // Attach before reading: metadata-only resume does not replay a completion
        // that preceded attachment. The subsequent read closes that interval.
        self.ensure_wait_subscription(thread_id, turn_id, Instant::now() + Duration::from_secs(10))
            .await?;
        self.reconcile_watched_turn(thread_id, turn_id).await
    }

    pub fn history_gaps(&self) -> tokio::sync::watch::Receiver<u64> {
        self.journal.history_gaps()
    }

    async fn reconcile_watched_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(), RelayError> {
        let stored = self.find_stored_turn_metadata(thread_id, turn_id).await?;
        let live = self.live_turn(thread_id, turn_id).await;
        let turn = match (stored, live) {
            (Some(stored), Some(live))
                if !stored.status.is_terminal() && live.status.is_terminal() =>
            {
                Some(live)
            }
            (Some(stored), _) => Some(stored),
            (None, live) => live,
        };
        let Some(turn) = turn else {
            // Only authoritative absence releases this registration. A read error
            // must not detach an existing worker that still needs observation.
            self.release_terminal_subscription(thread_id, turn_id).await;
            return Err(RelayError::Invalid(
                "turnId does not belong to threadId".into(),
            ));
        };
        // insert preserves a terminal notification racing with this snapshot.
        self.remember_live_turn(thread_id, &turn).await;
        if let Some(latest) = self.live_turn(thread_id, turn_id).await
            && latest.status.is_terminal()
        {
            self.reconcile_terminal_turn(thread_id, &latest).await;
        }
        Ok(())
    }

    async fn remember_live_turn(
        &self,
        thread_id: &str,
        turn: &codex_connect_app_server::protocol::Turn,
    ) {
        if self
            .live_turns
            .lock()
            .await
            .insert(thread_id, turn_metadata(turn))
        {
            self.journal.mark_gap().await;
        }
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
            live.get(thread_id, turn_id)
                .filter(|turn| turn.status.is_terminal())
        };
        if let Some(turn) = terminal {
            self.reconcile_terminal_turn(thread_id, &turn).await;
        } else {
            self.journal
                .push(
                    "codexConnect/observerWorkerChanged",
                    &json!({"threadId":thread_id,"turnId":turn_id}),
                )
                .await;
        }
        self.trigger_observer_usage_refresh().await;
    }

    async fn register_worker_turn(
        &self,
        thread_id: &str,
        turn: &codex_connect_app_server::protocol::Turn,
        annotation: WorkerAnnotation,
    ) {
        self.finish_thread_start(thread_id, Some(&turn.id)).await;
        self.remember_live_turn(thread_id, turn).await;
        self.annotate_live_turn(thread_id, &turn.id, annotation)
            .await;
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

    async fn release_terminal_subscription(&self, thread_id: &str, turn_id: &str) {
        let should_unsubscribe = self
            .thread_subscriptions
            .lock()
            .await
            .finish_turn(thread_id, turn_id);
        if should_unsubscribe {
            self.spawn_thread_unsubscribe(thread_id.to_string());
        }
    }

    async fn reconcile_terminal_turn(
        &self,
        thread_id: &str,
        turn: &codex_connect_app_server::protocol::Turn,
    ) {
        if !turn.status.is_terminal() {
            return;
        }
        if let Some(observer) = self.terminal_observer.get() {
            let fact = TerminalTurn {
                thread_id: thread_id.to_owned(),
                turn_id: turn.id.clone(),
                status: serde_json::to_value(turn.status)
                    .expect("turn status serializes")
                    .as_str()
                    .unwrap()
                    .to_owned(),
                timestamp_ms: turn
                    .completed_at
                    .and_then(|time| u64::try_from(time).ok())
                    .map(|seconds| seconds.saturating_mul(1000))
                    .unwrap_or_else(now_epoch_ms),
            };
            if observer(fact).is_err() {
                self.journal
                    .push(
                        "codexConnect/eventsStorageFailure",
                        &json!({"error":"terminal delivery storage failed"}),
                    )
                    .await;
            }
        }
        let (_, changed) = self
            .live_turns
            .lock()
            .await
            .reconcile_terminal(thread_id, turn);
        self.release_terminal_subscription(thread_id, &turn.id)
            .await;
        if changed {
            self.journal
                .push(
                    "codexConnect/observerWorkerChanged",
                    &json!({"threadId":thread_id,"turnId":turn.id}),
                )
                .await;
        }
    }

    async fn ensure_wait_subscription(
        &self,
        thread_id: &str,
        turn_id: &str,
        operation_deadline: Instant,
    ) -> Result<(), RelayError> {
        if tokio::time::timeout_at(operation_deadline, self.begin_thread_start(thread_id))
            .await
            .is_err()
        {
            return Err(RelayError::BudgetExceeded(
                "codex.wait could not acquire thread subscription ownership before its operation deadline"
                    .into(),
            ));
        }
        let mut start = ObservationStart {
            relay: self.clone(),
            thread_id: thread_id.to_owned(),
            armed: true,
        };
        if self
            .thread_subscriptions
            .lock()
            .await
            .is_subscribed(thread_id)
        {
            start.finish(Some(turn_id)).await;
            return Ok(());
        }
        let remaining = operation_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            start.finish(None).await;
            return Err(RelayError::BudgetExceeded(
                "codex.wait reached its operation deadline before thread subscription recovery"
                    .into(),
            ));
        }
        let resumed = self
            .app_server
            .request_with_timeout(
                ThreadResume {
                    thread_id: thread_id.to_string(),
                    cwd: None,
                    developer_instructions: None,
                    exclude_turns: true,
                },
                remaining,
            )
            .await;
        match resumed {
            Ok(_) => {
                self.mark_thread_subscribed(thread_id).await;
                start.finish(Some(turn_id)).await;
                Ok(())
            }
            Err(error) => {
                start.finish(None).await;
                Err(error.into())
            }
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
        let Some(observed) = live.observed(thread_id, turn_id) else {
            return Value::Null;
        };
        json!({
            "kind": observed.activity_kind,
            "summary": observed.activity_summary,
            "lastActivityAtMs": observed.last_activity_at_ms,
            "tokenUsage": token_usage_value(observed)
        })
    }

    async fn observer_activity_value(&self, thread_id: &str, turn_id: &str) -> Value {
        let live = self.live_turns.lock().await;
        let Some(observed) = live.observed(thread_id, turn_id) else {
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
            "tokenUsage": token_usage_value(observed)
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
        // response delivery; the calling client's deadline is independent of this local budget.
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
            // The cap applies to captured bytes before upstream text decoding, which can
            // expand an incomplete or invalid multibyte sequence beyond that byte count.
            stdout_may_be_truncated: stdout_bytes >= output_bytes_cap,
            stderr_may_be_truncated: stderr_bytes >= output_bytes_cap,
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
            .insert(process_id.clone(), cwd.clone(), tty)
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
                cwd: Some(cwd.clone()),
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
            "cwd":cwd,
            "state":"running",
            "tty":tty,
            "cursor":0,
        }))
    }

    /// Start once and return the first retained output/exit observation. Yielding
    /// never stops the upstream process or creates another command lifecycle.
    #[allow(clippy::too_many_arguments)]
    pub async fn command_start_with_output(
        &self,
        command: Vec<String>,
        cwd: Option<String>,
        env: Option<std::collections::BTreeMap<String, Option<String>>>,
        tty: bool,
        size: Option<CommandExecTerminalSize>,
        yield_time_ms: u64,
    ) -> Result<Value, RelayError> {
        validate_command_yield(yield_time_ms)?;
        let mut started = self.command_start(command, cwd, env, tty, size).await?;
        let process_id = started["processId"].as_str().unwrap().to_owned();
        match self.command_read(process_id, 0, yield_time_ms).await {
            Ok(output) => {
                started["output"] = output;
                started["readError"] = Value::Null;
            }
            Err(error) => {
                started["output"] = Value::Null;
                started["readError"] = json!(error.to_string());
            }
        }
        Ok(started)
    }

    /// Validate read intent before writing, then observe the same retained
    /// command. A failed read after acknowledgement must not invite input replay.
    pub async fn command_write_with_output(
        &self,
        process_id: String,
        input: Option<String>,
        close_stdin: bool,
        after_cursor: u64,
        yield_time_ms: u64,
    ) -> Result<Value, RelayError> {
        validate_command_yield(yield_time_ms)?;
        self.command_sessions
            .read_after(&process_id, after_cursor)
            .await
            .map_err(RelayError::Invalid)?;
        let acknowledgement = self
            .command_write(process_id.clone(), input, close_stdin)
            .await?;
        // The write has already succeeded. Keep that fact explicit even when
        // observation fails (for example, App Server disconnects afterward).
        let read = match self
            .command_read(process_id, after_cursor, yield_time_ms)
            .await
        {
            Ok(output) => json!({"output": output, "readError": Value::Null}),
            Err(error) => json!({"output": Value::Null, "readError": error.to_string()}),
        };
        let mut value = acknowledgement;
        value["output"] = read["output"].clone();
        value["readError"] = read["readError"].clone();
        Ok(value)
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

    /// Retained handles for recovering a command.start response lost by the caller.
    pub async fn command_handles(&self) -> Vec<Value> {
        self.command_sessions.handles().await
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
        fork_from_thread_id: Option<String>,
        last_turn_id: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        sandbox_policy: Option<SandboxPolicy>,
    ) -> Result<Value, RelayError> {
        if task.trim().is_empty() {
            return Err(RelayError::Invalid("task must not be empty".into()));
        }
        if model.as_deref().is_none_or(|model| model.trim().is_empty()) {
            return Err(RelayError::Invalid(
                "codex.start requires an explicit model".into(),
            ));
        }
        if thread_id.is_some() && fork_from_thread_id.is_some() {
            return Err(RelayError::Invalid(
                "threadId and forkFromThreadId are mutually exclusive".into(),
            ));
        }
        if last_turn_id.is_some() && fork_from_thread_id.is_none() {
            return Err(RelayError::Invalid(
                "lastTurnId requires forkFromThreadId".into(),
            ));
        }
        if thread_id.is_some() && (cwd.is_some() || effort.is_some() || sandbox_policy.is_some()) {
            return Err(RelayError::Invalid(
                "resumed work rejects cwd, effort, access, and writableRoots overrides; start a fresh thread to change workstream settings".into(),
            ));
        }
        if fork_from_thread_id.is_some()
            && (cwd.is_some() || effort.is_some() || sandbox_policy.is_some())
        {
            return Err(RelayError::Invalid(
                "forked work rejects cwd, effort, access, and writableRoots overrides; settings come from its source thread".into(),
            ));
        }
        if thread_id.is_none() && fork_from_thread_id.is_none() && cwd.is_none() {
            return Err(RelayError::Invalid(
                "fresh codex.start requires an explicit cwd".into(),
            ));
        }
        if let Some(policy) = sandbox_policy.as_ref() {
            validate_work_sandbox_policy(policy)?;
        } else if thread_id.is_none() && fork_from_thread_id.is_none() {
            return Err(RelayError::Invalid(
                "new work requires an explicit sandbox policy".into(),
            ));
        }
        let relay = self.clone();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let result = relay
                .work_start_owned(
                    task,
                    cwd,
                    thread_id,
                    fork_from_thread_id,
                    last_turn_id,
                    model,
                    effort,
                    sandbox_policy,
                )
                .await;
            let _ = sender.send(result);
        });
        receiver
            .await
            .map_err(|_| RelayError::AppServer(AppServerError::Disconnected))?
    }

    #[allow(clippy::too_many_arguments)]
    async fn work_start_owned(
        &self,
        task: String,
        cwd: Option<String>,
        thread_id: Option<String>,
        fork_from_thread_id: Option<String>,
        last_turn_id: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        sandbox_policy: Option<SandboxPolicy>,
    ) -> Result<Value, RelayError> {
        let observer_prompt = task.clone();
        let cursor = self.journal.cursor().await;
        let created = thread_id.is_none();
        let fresh = thread_id.is_none() && fork_from_thread_id.is_none();
        let thread_sandbox = sandbox_policy.as_ref().map(sandbox_mode);
        let prepared = match fork_from_thread_id {
            Some(source_thread_id) => {
                self.prepare_forked_thread(
                    source_thread_id,
                    last_turn_id,
                    model.as_deref().unwrap(),
                )
                .await?
            }
            None => {
                self.prepare_thread(
                    cwd,
                    thread_id,
                    model.clone(),
                    thread_sandbox,
                    fresh.then_some(ApprovalPolicy::Never),
                )
                .await?
            }
        };
        let effective_model = prepared.model.clone();
        let effective_effort = effort.or(prepared.effort.clone());
        let annotation = WorkerAnnotation {
            mode: "work".into(),
            cwd: prepared.cwd.clone(),
            model: effective_model.clone(),
            effort: effective_effort.clone(),
            prompt: Some(observer_prompt),
        };
        let turn_sandbox = if fresh { sandbox_policy } else { None };
        let response = match self
            .app_server
            .start_request(TurnStart {
                thread_id: prepared.id.clone(),
                input: vec![TextInput::Text { text: task }],
                cwd: prepared.cwd.clone(),
                approval_policy: None,
                sandbox_policy: turn_sandbox,
                model: effective_model.clone(),
                effort: effective_effort.clone(),
                service_tier_for_turn: None,
            })
            .await
        {
            Ok(deferred) => deferred.wait().await,
            Err(error) => {
                self.finish_thread_start(&prepared.id, None).await;
                return Err(error.into());
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.finish_thread_start(&prepared.id, None).await;
                return Err(error.into());
            }
        };
        let turn_id = response.turn.id.clone();
        self.register_worker_turn(&prepared.id, &response.turn, annotation)
            .await;
        Ok(json!({
            "threadId":prepared.id,
            "turnId":turn_id,
            "createdThread":created,
            "cursor":cursor,
            "cwd":prepared.cwd,
            "model":effective_model,
            "effort":effective_effort,
        }))
    }

    async fn prepare_forked_thread(
        &self,
        source_thread_id: String,
        last_turn_id: Option<String>,
        expected_model: &str,
    ) -> Result<PreparedThread, RelayError> {
        let source = self.read_thread_metadata(source_thread_id.clone()).await?;
        validate_thread_model(expected_model, source.model.as_deref())?;
        let response = self
            .app_server
            .start_request(ThreadFork {
                thread_id: source_thread_id,
                last_turn_id,
                exclude_turns: true,
            })
            .await?
            .wait()
            .await?;
        validate_thread_model(expected_model, response.thread.model.as_deref())?;
        let thread_id = response.thread.id.clone();
        self.begin_thread_start(&thread_id).await;
        self.mark_thread_subscribed(&thread_id).await;
        let cwd = match self.host.resolve_app_server_directory(&response.cwd) {
            Ok(cwd) => cwd,
            Err(error) => {
                self.finish_thread_start(&thread_id, None).await;
                return Err(error.into());
            }
        };
        Ok(PreparedThread {
            id: thread_id,
            cwd,
            model: response.thread.model,
            effort: response.thread.reasoning_effort,
        })
    }

    async fn prepare_thread(
        &self,
        cwd: Option<String>,
        thread_id: Option<String>,
        new_thread_model: Option<String>,
        new_thread_sandbox: Option<SandboxMode>,
        new_thread_approval: Option<ApprovalPolicy>,
    ) -> Result<PreparedThread, RelayError> {
        let cwd = cwd
            .map(|v| self.host.resolve_app_server_directory(&v))
            .transpose()?;
        let response = if let Some(id) = thread_id {
            if cwd.is_some() {
                return Err(RelayError::Invalid(
                    "resumed threads keep their existing cwd; start a fresh thread to change it"
                        .into(),
                ));
            }
            self.begin_thread_start(&id).await;
            // Serialize the whole resume preparation against an in-flight unsubscribe, then
            // verify the stored cwd before resume can load hooks or tools for it.
            let metadata = match self.read_thread_metadata(id.clone()).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.finish_thread_start(&id, None).await;
                    return Err(error);
                }
            };
            if let Some(expected_model) = new_thread_model.as_deref()
                && let Err(error) = validate_thread_model(expected_model, metadata.model.as_deref())
            {
                self.finish_thread_start(&id, None).await;
                return Err(error);
            }
            if let Err(error) = self.ensure_thread_reuse_policy(&metadata).await {
                self.finish_thread_start(&id, None).await;
                return Err(error);
            }
            let response = match self
                .app_server
                .start_request(ThreadResume {
                    thread_id: id.clone(),
                    cwd: None,
                    developer_instructions: None,
                    exclude_turns: true,
                })
                .await
            {
                Ok(deferred) => deferred.wait().await,
                Err(error) => Err(error),
            };
            match response {
                Ok(response) => {
                    if let Some(expected_model) = new_thread_model.as_deref()
                        && let Err(error) =
                            validate_thread_model(expected_model, response.thread.model.as_deref())
                    {
                        self.finish_thread_start(&id, None).await;
                        return Err(error);
                    }
                    self.mark_thread_subscribed(&id).await;
                    response
                }
                Err(error) => {
                    self.finish_thread_start(&id, None).await;
                    return Err(error.into());
                }
            }
        } else {
            let deferred = self
                .app_server
                .start_request(ThreadStart {
                    model: new_thread_model,
                    sandbox: new_thread_sandbox,
                    approval_policy: new_thread_approval,
                    cwd: Some(cwd.ok_or_else(|| {
                        RelayError::Invalid("fresh codex.start requires an explicit cwd".into())
                    })?),
                    service_name: Some("codex-connect".into()),
                    ..ThreadStart::default()
                })
                .await?;
            let response = deferred.wait().await?;
            self.begin_thread_start(&response.thread.id).await;
            self.mark_thread_subscribed(&response.thread.id).await;
            response
        };
        let thread_id = response.thread.id.clone();
        let cwd = match self.host.resolve_app_server_directory(&response.cwd) {
            Ok(cwd) => cwd,
            Err(error) => {
                self.finish_thread_start(&thread_id, None).await;
                return Err(error.into());
            }
        };
        Ok(PreparedThread {
            id: thread_id,
            cwd,
            model: response.thread.model,
            effort: response.thread.reasoning_effort,
        })
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

    async fn ensure_thread_reuse_policy(&self, thread: &Thread) -> Result<(), RelayError> {
        let observed_model_usage_at_ms = {
            let live = self.live_turns.lock().await;
            let active = live
                .turns
                .iter()
                .filter(|((thread_id, _), _)| thread_id == &thread.id)
                .filter_map(|(_, observed)| observed.last_model_usage_at_ms);
            let recent = live
                .recent
                .iter()
                .filter(|(thread_id, _, _)| thread_id == &thread.id)
                .filter_map(|(_, _, observed)| observed.last_model_usage_at_ms);
            active.chain(recent).max()
        };
        if let Some(last_model_usage_at_ms) = observed_model_usage_at_ms {
            if now_epoch_ms().saturating_sub(last_model_usage_at_ms) >= THREAD_REUSE_POLICY_MS {
                return Err(RelayError::Invalid(format!(
                    "thread {} is outside Codex Connect's conservative 30-minute guaranteed-cache reuse policy; start a fresh thread without threadId and provide self-contained context",
                    thread.id
                )));
            }
            return Ok(());
        }
        let latest = self
            .app_server
            .request(ThreadTurnsList {
                thread_id: thread.id.clone(),
                cursor: None,
                limit: Some(1),
                sort_direction: Some(SortDirection::Desc),
                items_view: Some(TurnItemsView::NotLoaded),
            })
            .await?
            .data
            .into_iter()
            .next();
        let last_model_turn_at = latest
            .and_then(|turn| turn.completed_at)
            .unwrap_or(thread.updated_at);
        let now_secs = (now_epoch_ms() / 1000).min(i64::MAX as u64) as i64;
        let age_secs = now_secs.saturating_sub(last_model_turn_at);
        if age_secs >= THREAD_REUSE_POLICY_SECS {
            return Err(RelayError::Invalid(format!(
                "thread {} is outside Codex Connect's conservative 30-minute guaranteed-cache reuse policy; start a fresh thread without threadId and provide self-contained context",
                thread.id
            )));
        }
        Ok(())
    }

    async fn hydrate_turn_items_when_ready(
        &self,
        thread_id: &str,
        turn: &mut codex_connect_app_server::protocol::Turn,
    ) -> Result<bool, RelayError> {
        let output = self
            .read_terminal_output_when_ready(thread_id, &turn.id)
            .await?;
        turn.items = output
            .handoff_item
            .as_ref()
            .map(terminal_handoff_projection)
            .into_iter()
            .collect();
        Ok(output.selection_complete)
    }

    async fn read_terminal_output_when_ready(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<TerminalOutput, RelayError> {
        let mut retry_ms = WAIT_STORAGE_RETRY_MS;
        loop {
            match self.read_terminal_output(thread_id, turn_id).await {
                Ok(output) => return Ok(output),
                Err(error) if is_unflushed_thread_store(&error) => {
                    tokio::time::sleep(Duration::from_millis(retry_ms)).await;
                    retry_ms = retry_ms.saturating_mul(2).min(WAIT_STORAGE_RETRY_MAX_MS);
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn read_thread_items_page(
        &self,
        thread_id: &str,
        turn_id: &str,
        cursor: Option<String>,
        requested_limit: u32,
    ) -> Result<ThreadItemsListResponse, RelayError> {
        let mut limit = requested_limit.clamp(1, ITEM_PAGE_SIZE);
        loop {
            let request = ThreadItemsList {
                thread_id: thread_id.to_string(),
                turn_id: Some(turn_id.to_string()),
                cursor: cursor.clone(),
                limit: Some(limit),
                sort_direction: Some(SortDirection::Desc),
            };
            match self.app_server.request(request).await {
                Ok(response) => return Ok(response),
                Err(AppServerError::MessageTooLarge) if limit > 1 => {
                    limit = (limit / 2).max(1);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn read_terminal_output(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<TerminalOutput, RelayError> {
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut items = Vec::new();
        let mut scanned = 0usize;
        let mut pages = 0usize;
        let mut history_truncated = false;
        'pages: loop {
            if pages >= MAX_WAIT_OUTPUT_PAGES {
                history_truncated = true;
                break;
            }
            pages += 1;
            let page_limit = (MAX_WAIT_SCAN_ITEMS - scanned).min(ITEM_PAGE_SIZE as usize) as u32;
            let response = match self
                .read_thread_items_page(thread_id, turn_id, cursor.clone(), page_limit)
                .await
            {
                Ok(response) => response,
                Err(RelayError::AppServer(AppServerError::MessageTooLarge)) => {
                    history_truncated = true;
                    break 'pages;
                }
                Err(error) => return Err(error),
            };
            let page_len = response.data.len();
            let next_cursor =
                checked_next_cursor(&mut seen_cursors, response.next_cursor, "thread/items/list")?;
            for (index, entry) in response.data.into_iter().enumerate() {
                if entry.turn_id != turn_id {
                    return Err(RelayError::Invalid(
                        "thread/items/list returned an item from another turn".into(),
                    ));
                }
                scanned += 1;
                if matches!(
                    entry.item.get("type").and_then(Value::as_str),
                    Some("agentMessage" | "exitedReviewMode")
                ) {
                    let is_final_answer = entry.item.get("type").and_then(Value::as_str)
                        == Some("agentMessage")
                        && entry.item.get("phase").and_then(Value::as_str) == Some("final_answer");
                    let is_review =
                        entry.item.get("type").and_then(Value::as_str) == Some("exitedReviewMode");
                    let is_agent_message =
                        entry.item.get("type").and_then(Value::as_str) == Some("agentMessage");
                    let already_have_review = items.iter().any(|item: &Value| {
                        item.get("type").and_then(Value::as_str) == Some("exitedReviewMode")
                    });
                    let already_have_agent_message = items.iter().any(|item: &Value| {
                        item.get("type").and_then(Value::as_str) == Some("agentMessage")
                    });
                    if is_final_answer
                        || (is_review && !already_have_review)
                        || (is_agent_message && !already_have_agent_message)
                    {
                        items.push(entry.item);
                    }
                    // App Server items arrive newest first. Once the newest final answer is
                    // found, no older item can supersede it under the handoff priority.
                    if is_final_answer {
                        break 'pages;
                    }
                }
                if scanned >= MAX_WAIT_SCAN_ITEMS {
                    history_truncated = index + 1 < page_len || next_cursor.is_some();
                    break 'pages;
                }
            }
            match next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let handoff_item = canonical_handoff_item(items.iter()).cloned();
        Ok(TerminalOutput {
            handoff_item,
            selection_complete: !history_truncated,
        })
    }

    async fn read_canonical_terminal_result(
        &self,
        thread_id: &str,
        turn_id: &str,
        deadline: Instant,
    ) -> Result<TerminalOutput, RelayError> {
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut newest_review = None;
        let mut newest_agent_message = None;
        let mut retry_ms = WAIT_STORAGE_RETRY_MS;
        'pages: loop {
            if Instant::now() >= deadline {
                break;
            }
            let response = match tokio::time::timeout_at(
                deadline,
                self.read_thread_items_page(thread_id, turn_id, cursor.clone(), ITEM_PAGE_SIZE),
            )
            .await
            {
                Err(_) => break,
                Ok(Ok(response)) => response,
                Ok(Err(RelayError::AppServer(AppServerError::MessageTooLarge))) => break,
                Ok(Err(error)) if is_unflushed_thread_store(&error) => {
                    if tokio::time::timeout_at(
                        deadline,
                        tokio::time::sleep(Duration::from_millis(retry_ms)),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                    retry_ms = retry_ms.saturating_mul(2).min(WAIT_STORAGE_RETRY_MAX_MS);
                    continue 'pages;
                }
                Ok(Err(error)) => return Err(error),
            };
            retry_ms = WAIT_STORAGE_RETRY_MS;
            for entry in response.data {
                if Instant::now() >= deadline {
                    break 'pages;
                }
                if entry.turn_id != turn_id {
                    return Err(RelayError::Invalid(
                        "thread/items/list returned an item from another turn".into(),
                    ));
                }
                match entry.item.get("type").and_then(Value::as_str) {
                    Some("agentMessage")
                        if entry.item.get("phase").and_then(Value::as_str)
                            == Some("final_answer") =>
                    {
                        return Ok(TerminalOutput {
                            handoff_item: Some(entry.item),
                            selection_complete: true,
                        });
                    }
                    Some("exitedReviewMode") if newest_review.is_none() => {
                        newest_review = Some(entry.item)
                    }
                    Some("agentMessage") if newest_agent_message.is_none() => {
                        newest_agent_message = Some(entry.item)
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            match checked_next_cursor(&mut seen_cursors, response.next_cursor, "thread/items/list")?
            {
                Some(next) => cursor = Some(next),
                None => {
                    return Ok(TerminalOutput {
                        handoff_item: newest_review.or(newest_agent_message),
                        selection_complete: true,
                    });
                }
            }
        }
        Ok(TerminalOutput {
            handoff_item: newest_review.or(newest_agent_message),
            selection_complete: false,
        })
    }

    async fn read_recent_transcript_entries(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(Vec<Value>, bool), RelayError> {
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut entries = Vec::new();
        let mut remaining_chars = MAX_TRANSCRIPT_TOTAL_CHARS;
        let mut scanned = 0usize;
        let mut pages = 0usize;
        let mut truncated = false;
        let mut reached_history_start = false;
        'pages: loop {
            if scanned >= MAX_TRANSCRIPT_SCAN_ITEMS || pages >= MAX_TRANSCRIPT_PAGES {
                truncated = true;
                break;
            }
            pages += 1;
            let page_limit = (MAX_TRANSCRIPT_SCAN_ITEMS - scanned).min(ITEM_PAGE_SIZE as usize);
            let response = match self
                .read_thread_items_page(thread_id, turn_id, cursor.clone(), page_limit as u32)
                .await
            {
                Ok(response) => response,
                Err(RelayError::AppServer(AppServerError::MessageTooLarge)) => {
                    truncated = true;
                    break 'pages;
                }
                Err(error) => return Err(error),
            };
            let next_cursor =
                checked_next_cursor(&mut seen_cursors, response.next_cursor, "thread/items/list")?;
            let page_len = response.data.len();
            for (index, entry) in response.data.into_iter().enumerate() {
                if entry.turn_id != turn_id {
                    return Err(RelayError::Invalid(
                        "thread/items/list returned an item from another turn".into(),
                    ));
                }
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
        let mut seen_cursors = HashSet::new();
        let mut pages = 0usize;
        loop {
            if pages >= MAX_TURN_LOOKUP_PAGES {
                return Err(RelayError::Invalid(
                    "thread/turns/list exceeded the turn lookup page limit".into(),
                ));
            }
            pages += 1;
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
            match checked_next_cursor(&mut seen_cursors, response.next_cursor, "thread/turns/list")?
            {
                Some(next) => cursor = Some(next),
                None => return Ok(None),
            }
        }
    }

    pub async fn work_wait(&self, thread_id: String, turn_id: String) -> Result<Value, RelayError> {
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_millis(WORK_WAIT_MS);
        let operation_deadline = started_at + Duration::from_millis(WORK_WAIT_OPERATION_MS);
        // Subscribe before the authoritative read so actionable requests and terminal
        // notifications cannot race the wait setup.
        let mut transport_changes = self.app_server.changes();
        let mut journal_changes = self.journal.changes();
        let mut journal_cursor = self.journal.cursor().await;
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
                        "codex.wait could not reconcile turn state within {WORK_WAIT_OPERATION_MS} ms"
                    )));
                }
            }
        } else {
            // A turn started by this relay is already represented by the official turn/start
            // response and the subscribed lifecycle stream. Reads are reserved for explicit
            // reconciliation boundaries rather than used to observe ordinary progress.
            None
        };
        if initial_live.is_none()
            && stored
                .as_ref()
                .is_some_and(|turn| !turn.status.is_terminal())
        {
            // A turn that predates this relay cannot rely on a subscription owned by the prior
            // App Server process. Resume the thread once so subsequent lifecycle state arrives on
            // the official notification stream instead of requiring periodic status reads.
            self.ensure_wait_subscription(&thread_id, &turn_id, operation_deadline)
                .await?;
        }
        let mut final_reconciled = false;
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
            if selected.status.is_terminal() {
                self.reconcile_terminal_turn(&thread_id, &selected).await;
            }
            let mut selection_incomplete = selected.status.is_terminal().then_some(false);
            if selected.status.is_terminal() && selected.items.is_empty() {
                match tokio::time::timeout_at(
                    operation_deadline,
                    self.hydrate_turn_items_when_ready(&thread_id, &mut selected),
                )
                .await
                {
                    Ok(result) => selection_incomplete = Some(!result?),
                    Err(_) => {
                        return Err(RelayError::BudgetExceeded(
                            "codex.wait reached terminal state but terminal output hydration exceeded the reserved finalization budget; inspect the completed turn explicitly"
                                .into(),
                        ));
                    }
                }
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
                    "turn":turn_snapshot(&selected, selection_incomplete), "currentActivity":activity,
                    "pendingActions":pending.iter().map(|r| r.as_ref()).collect::<Vec<_>>(),
                });
                if selected.status.is_terminal() && stored_terminal {
                    self.forget_live_turn(&thread_id, &turn_id).await;
                }
                return Ok(result);
            }
            if Instant::now() >= deadline {
                if !final_reconciled {
                    final_reconciled = true;
                    let reconcile_deadline = operation_deadline
                        .min(Instant::now() + Duration::from_millis(WAIT_FINAL_RECONCILE_MS));
                    match tokio::time::timeout_at(
                        reconcile_deadline,
                        self.find_stored_turn_metadata(&thread_id, &turn_id),
                    )
                    .await
                    {
                        Ok(Ok(result)) => {
                            stored = result;
                            continue;
                        }
                        Ok(Err(error)) if is_unflushed_thread_store(&error) => {}
                        Ok(Err(error)) => return Err(error),
                        Err(_) => {}
                    }
                }
                let result = json!({
                    "threadId":thread_id,"turnId":selected_id,"state":"active","wakeReason":"timeout",
                    "turn":turn_snapshot(&selected, None), "currentActivity":activity,
                    "pendingActions":pending.iter().map(|r| r.as_ref()).collect::<Vec<_>>(),
                });
                return Ok(result);
            }

            // Ordinary worker notifications remain journaled but do not end the operator wait
            // or force an App Server read. Pending requests and lifecycle notifications wake the
            // join directly. An explicit journal history gap is the only mid-lease condition that
            // requires authoritative persistence reconciliation.
            let selected_id = selected_id.map(str::to_owned);
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
                        let batch = self
                            .journal
                            .read_after(journal_cursor, &thread_id, Some(&turn_id))
                            .await
                            .map_err(RelayError::Invalid)?;
                        journal_cursor = batch.cursor;
                        if batch.history_lost {
                            break true;
                        }
                        if self
                            .live_turn(&thread_id, &turn_id)
                            .await
                            .is_some_and(|turn| turn.status.is_terminal())
                        {
                            break false;
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => break false,
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
        detail: InspectDetail,
        text_offset: usize,
    ) -> Result<Value, RelayError> {
        if text_offset != 0 && !matches!(detail, InspectDetail::Result) {
            return Err(RelayError::Invalid(
                "textOffset is only valid with detail=result".into(),
            ));
        }
        let result_deadline = matches!(detail, InspectDetail::Result)
            .then(|| result_selection_deadline(Instant::now()));
        let _thread_metadata = if let Some(deadline) = result_deadline {
            match tokio::time::timeout_at(deadline, self.read_thread_metadata(thread_id.clone()))
                .await
            {
                Ok(result) => result?,
                Err(_) => {
                    return Err(RelayError::BudgetExceeded(
                        "codex.inspect result selection budget expired before thread metadata was available"
                            .into(),
                    ));
                }
            }
        } else {
            self.read_thread_metadata(thread_id.clone()).await?
        };
        let live = self.live_turn(&thread_id, &turn_id).await;
        let stored = if let Some(deadline) = result_deadline {
            match tokio::time::timeout_at(
                deadline,
                self.find_stored_turn_metadata(&thread_id, &turn_id),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    return Err(RelayError::BudgetExceeded(
                        "codex.inspect result selection budget expired before turn metadata was available"
                            .into(),
                    ));
                }
            }
        } else {
            self.find_stored_turn_metadata(&thread_id, &turn_id).await?
        };
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
            self.reconcile_terminal_turn(&thread_id, &selected).await;
        }
        let activity = self.current_activity_value(&thread_id, &turn_id).await;
        let result = match detail {
            InspectDetail::Result => {
                if text_offset != 0 && !selected.status.is_terminal() {
                    return Err(RelayError::Invalid(
                        "textOffset requires a terminal turn".into(),
                    ));
                }
                let output = self
                    .read_canonical_terminal_result(
                        &thread_id,
                        &turn_id,
                        result_deadline.expect("result detail has a shared deadline"),
                    )
                    .await?;
                let selection_complete = selected.status.is_terminal() && output.selection_complete;
                let result_page = persisted_handoff_page(
                    output.handoff_item.as_ref(),
                    text_offset,
                    selection_complete,
                )?;
                json!({
                    "threadId":thread_id,
                    "turnId":turn_id,
                    "status":selected.status,
                    "detail":"result",
                    "currentActivity":activity,
                    "resultPage":result_page,
                })
            }
            InspectDetail::Raw => {
                let batch = self
                    .journal
                    .read_after(after_cursor, &thread_id, Some(&turn_id))
                    .await
                    .map_err(RelayError::Invalid)?;
                json!({
                    "threadId":thread_id,
                    "turnId":turn_id,
                    "status":selected.status,
                    "detail":"raw",
                    "currentActivity":activity,
                    "cursor":batch.cursor,
                    "historyLost":batch.history_lost,
                    "hasMore":batch.has_more,
                    "events":batch.events,
                })
            }
            InspectDetail::Semantic => {
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
                json!({
                    "threadId":thread_id,
                    "turnId":turn_id,
                    "status":selected.status,
                    "detail":"semantic",
                    "currentActivity":activity,
                    "cursor":batch.cursor,
                    "historyLost":batch.history_lost,
                    "hasMore":batch.has_more,
                    "events":batch.events,
                })
            }
        };
        Ok(result)
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
                thread_id: thread_id.clone(),
                expected_turn_id: expected_turn_id.clone(),
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
                thread_id: thread_id.clone(),
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
        if model.as_deref().is_none_or(|model| model.trim().is_empty()) {
            return Err(RelayError::Invalid(
                "codex.start requires an explicit model".into(),
            ));
        }
        if thread_id.is_some() && cwd.is_some() {
            return Err(RelayError::Invalid(
                "resumed reviews inherit cwd; start a fresh review thread to change it".into(),
            ));
        }
        if thread_id.is_none() && cwd.is_none() {
            return Err(RelayError::Invalid(
                "fresh review requires an explicit cwd".into(),
            ));
        }
        let relay = self.clone();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let result = relay.review_owned(cwd, thread_id, target, model).await;
            let _ = sender.send(result);
        });
        receiver
            .await
            .map_err(|_| RelayError::AppServer(AppServerError::Disconnected))?
    }

    async fn review_owned(
        &self,
        cwd: Option<String>,
        thread_id: Option<String>,
        target: ReviewTarget,
        model: Option<String>,
    ) -> Result<Value, RelayError> {
        let cursor = self.journal.cursor().await;
        let created = thread_id.is_none();
        let observer_prompt = review_target_prompt(&target);
        let prepared = self
            .prepare_thread(
                cwd,
                thread_id,
                model.clone(),
                created.then_some(SandboxMode::ReadOnly),
                None,
            )
            .await?;
        let effective_model = prepared.model.clone();
        let effective_effort = prepared.effort.clone();
        let annotation = WorkerAnnotation {
            mode: "review".into(),
            cwd: prepared.cwd.clone(),
            model: effective_model.clone(),
            effort: effective_effort.clone(),
            prompt: Some(observer_prompt),
        };
        let response = match self
            .app_server
            .start_request(ReviewStart {
                thread_id: prepared.id.clone(),
                target,
                delivery: "inline",
            })
            .await
        {
            Ok(deferred) => deferred.wait().await,
            Err(error) => {
                self.finish_thread_start(&prepared.id, None).await;
                return Err(error.into());
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.finish_thread_start(&prepared.id, None).await;
                return Err(error.into());
            }
        };
        let turn_id = response.turn.id.clone();
        self.register_worker_turn(&prepared.id, &response.turn, annotation)
            .await;
        // The pinned App Server returns the inline review turn on the source thread even when
        // reviewThreadId names the internal reviewer thread. work.wait needs that pair.
        Ok(json!({
            "threadId":prepared.id,
            "turnId":turn_id,
            "createdThread":created,
            "cursor":cursor,
            "cwd":prepared.cwd,
            "model":effective_model,
            "effort":effective_effort,
        }))
    }

    pub async fn pending_actions(&self, thread_id: Option<&str>) -> Vec<Value> {
        self.app_server
            .pending_requests(thread_id)
            .iter()
            .map(|r| serde_json::to_value(r.as_ref()).unwrap())
            .collect()
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

    pub async fn thread_list(
        &self,
        cursor: Option<String>,
        limit: Option<u32>,
        archived: Option<bool>,
        cwd: Option<String>,
        search_term: Option<String>,
    ) -> Result<Value, RelayError> {
        if limit.is_some_and(|limit| limit == 0 || limit > CODEX_QUERY_PAGE_MAX) {
            return Err(RelayError::Invalid(format!(
                "thread query limit must be between 1 and {CODEX_QUERY_PAGE_MAX}"
            )));
        }
        let cwd = cwd
            .map(|cwd| self.host.resolve_app_server_directory(&cwd))
            .transpose()?;
        let response = self
            .app_server
            .request(ThreadList {
                cursor,
                limit: Some(limit.unwrap_or(CODEX_QUERY_PAGE_DEFAULT)),
                sort_key: Some(ThreadSortKey::RecencyAt),
                sort_direction: Some(SortDirection::Desc),
                archived,
                cwd,
                search_term,
            })
            .await?;
        let mut threads = Vec::with_capacity(response.data.len());
        for thread in response.data {
            self.host.resolve_app_server_directory(&thread.cwd)?;
            threads.push(thread_summary(&thread));
        }
        Ok(json!({
            "threads":threads,
            "nextCursor":response.next_cursor,
            "backwardsCursor":response.backwards_cursor,
        }))
    }

    pub async fn thread_summary(&self, thread_id: String) -> Result<Value, RelayError> {
        let thread = self.read_thread_metadata(thread_id).await?;
        Ok(thread_summary(&thread))
    }

    pub async fn background_terminals(
        &self,
        thread_id: String,
        cursor: Option<String>,
        limit: Option<u32>,
    ) -> Result<Value, RelayError> {
        if limit.is_some_and(|limit| limit == 0 || limit > CODEX_QUERY_PAGE_MAX) {
            return Err(RelayError::Invalid(format!(
                "background terminal query limit must be between 1 and {CODEX_QUERY_PAGE_MAX}"
            )));
        }
        self.read_thread_metadata(thread_id.clone()).await?;
        let response = self
            .app_server
            .request(ThreadBackgroundTerminalsList {
                thread_id,
                cursor,
                limit: Some(limit.unwrap_or(CODEX_QUERY_PAGE_DEFAULT)),
            })
            .await?;
        for terminal in &response.data {
            self.host.resolve_app_server_directory(&terminal.cwd)?;
        }
        let terminals = response
            .data
            .into_iter()
            .map(|terminal| {
                json!({
                    "itemId":terminal.item_id,
                    "processId":terminal.process_id,
                    "command":observer_clip(&terminal.command, MAX_BACKGROUND_COMMAND_CHARS),
                    "cwd":terminal.cwd,
                    "osPid":terminal.os_pid,
                    "cpuPercent":terminal.cpu_percent,
                    "rssKb":terminal.rss_kb,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"terminals":terminals,"nextCursor":response.next_cursor}))
    }

    async fn ensure_thread_not_active(&self, thread_id: &str) -> Result<(), RelayError> {
        if self.live_turns.lock().await.has_active_thread(thread_id) {
            return Err(RelayError::Invalid(format!(
                "thread {thread_id} has an active delegated turn"
            )));
        }
        let thread = self.read_thread_metadata(thread_id.to_string()).await?;
        if thread.status.get("type").and_then(Value::as_str) == Some("active") {
            return Err(RelayError::Invalid(format!(
                "thread {thread_id} is active in App Server"
            )));
        }
        Ok(())
    }

    pub async fn thread_set_archived(
        &self,
        thread_ids: Vec<String>,
        archived: bool,
    ) -> Result<Value, RelayError> {
        validate_thread_batch(&thread_ids)?;
        let mut results = Vec::with_capacity(thread_ids.len());
        for thread_id in thread_ids {
            let result = if archived {
                match self.ensure_thread_not_active(&thread_id).await {
                    Ok(()) => self
                        .app_server
                        .request(ThreadArchive {
                            thread_id: thread_id.clone(),
                        })
                        .await
                        .map(|_| ()),
                    Err(error) => {
                        results.push(json!({"threadId":thread_id,"error":error.to_string()}));
                        continue;
                    }
                }
            } else {
                self.app_server
                    .request(ThreadUnarchive {
                        thread_id: thread_id.clone(),
                    })
                    .await
                    .map(|_| ())
            };
            match result {
                Ok(()) => results.push(json!({"threadId":thread_id,"archived":archived})),
                Err(error) => results.push(json!({"threadId":thread_id,"error":error.to_string()})),
            }
        }
        Ok(json!({"results":results}))
    }

    pub async fn thread_delete(&self, thread_ids: Vec<String>) -> Result<Value, RelayError> {
        validate_thread_batch(&thread_ids)?;
        let mut results = Vec::with_capacity(thread_ids.len());
        for thread_id in thread_ids {
            if let Err(error) = self.ensure_thread_not_active(&thread_id).await {
                results.push(json!({"threadId":thread_id,"error":error.to_string()}));
                continue;
            }
            match self
                .app_server
                .request(ThreadDelete {
                    thread_id: thread_id.clone(),
                })
                .await
            {
                Ok(_) => results.push(json!({"threadId":thread_id,"deleted":true})),
                Err(error) => results.push(json!({"threadId":thread_id,"error":error.to_string()})),
            }
        }
        Ok(json!({"results":results}))
    }

    pub async fn background_terminal_terminate(
        &self,
        thread_id: String,
        process_id: String,
    ) -> Result<Value, RelayError> {
        self.read_thread_metadata(thread_id.clone()).await?;
        let response = self
            .app_server
            .request(ThreadBackgroundTerminalsTerminate {
                thread_id: thread_id.clone(),
                process_id: process_id.clone(),
            })
            .await?;
        Ok(json!({
            "threadId":thread_id,
            "processId":process_id,
            "terminated":response.terminated,
        }))
    }

    async fn store_observer_usage_result(&self, result: Result<Value, RelayError>) {
        let (ok, error) = {
            let mut state = self.observer_usage.lock().await;
            match result {
                Ok(value) => {
                    state.value = Some(value);
                    state.error = None;
                    state.updated_at_ms = Some(now_epoch_ms());
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
        let (usage, usage_error, usage_updated_at_ms) = {
            let cached = self.observer_usage.lock().await;
            (
                cached.value.clone().unwrap_or(Value::Null),
                cached.error.clone(),
                cached.updated_at_ms,
            )
        };
        let workers = {
            let live = self.live_turns.lock().await;
            live.observer_workers()
        };
        let pending_actions = self.pending_actions(None).await;
        let notices = self.journal.observer_notices(8).await;
        json!({
            "defaultCwd": self.default_cwd(),
            "usage": usage,
            "usageError": usage_error,
            "usageUpdatedAtMs": usage_updated_at_ms,
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

    pub async fn worker_handles(&self) -> Vec<Value> {
        self.live_turns.lock().await.operator_worker_handles()
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
            self.reconcile_terminal_turn(&thread_id, &selected).await;
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
        let relay = self.clone();
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
                            let mut terminal_turn = None;
                            let mut history_lost = false;
                            if matches!(method, "turn/started" | "turn/completed") {
                                let params = event.get("params").unwrap_or(&Value::Null);
                                if let (Some(thread_id), Some(turn)) = (
                                    params.get("threadId").and_then(Value::as_str),
                                    params.get("turn").cloned(),
                                ) && let Ok(turn) = serde_json::from_value::<
                                    codex_connect_app_server::protocol::Turn,
                                >(turn)
                                {
                                    history_lost = live_turns
                                        .lock()
                                        .await
                                        .insert(thread_id, turn_metadata(&turn));
                                    if turn.status.is_terminal() {
                                        terminal_turn = Some((thread_id.to_string(), turn));
                                    }
                                }
                            }
                            if history_lost {
                                journal.mark_gap().await;
                            }
                            live_turns
                                .lock()
                                .await
                                .observe_event(method, event.get("params").unwrap_or(&Value::Null));
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
                            if let Some((thread_id, turn)) = terminal_turn {
                                relay.reconcile_terminal_turn(&thread_id, &turn).await;
                            }
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
}

fn thread_summary(thread: &Thread) -> Value {
    json!({
        "threadId":thread.id,
        "sessionId":thread.session_id,
        "forkedFromThreadId":thread.forked_from_id,
        "parentThreadId":thread.parent_thread_id,
        "name":thread.name,
        "preview":observer_clip(&thread.preview, MAX_THREAD_PREVIEW_CHARS),
        "cwd":thread.cwd,
        "model":thread.model,
        "effort":thread.reasoning_effort,
        "createdAt":thread.created_at,
        "updatedAt":thread.updated_at,
        "status":thread.status.get("type").cloned().unwrap_or(Value::Null),
    })
}

fn validate_thread_batch(thread_ids: &[String]) -> Result<(), RelayError> {
    if thread_ids.is_empty() || thread_ids.len() > 100 {
        return Err(RelayError::Invalid(
            "threadIds must contain between 1 and 100 IDs".into(),
        ));
    }
    if thread_ids.iter().any(|id| id.trim().is_empty()) {
        return Err(RelayError::Invalid(
            "threadIds must not contain empty IDs".into(),
        ));
    }
    if thread_ids.iter().collect::<HashSet<_>>().len() != thread_ids.len() {
        return Err(RelayError::Invalid(
            "threadIds must not contain duplicates".into(),
        ));
    }
    Ok(())
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

fn validate_command_yield(yield_time_ms: u64) -> Result<(), RelayError> {
    if yield_time_ms > MAX_COMMAND_YIELD_MS {
        return Err(RelayError::Invalid(format!(
            "yieldTimeMs must be less than or equal to {MAX_COMMAND_YIELD_MS}; yielding does not stop the command"
        )));
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

fn turn_snapshot(
    turn: &codex_connect_app_server::protocol::Turn,
    selection_incomplete: Option<bool>,
) -> Value {
    let output = canonical_handoff_item(turn.items.iter().rev())
        .map(terminal_handoff_projection)
        .into_iter()
        .collect::<Vec<_>>();
    json!({
        "id":turn.id,
        "status":turn.status,
        "error":turn.error,
        "output":output,
        "selectionIncomplete":selection_incomplete
    })
}

fn canonical_handoff_item<'a>(
    items_newest_first: impl Iterator<Item = &'a Value>,
) -> Option<&'a Value> {
    let mut newest_review = None;
    let mut newest_agent_message = None;
    for item in items_newest_first {
        match item.get("type").and_then(Value::as_str) {
            Some("agentMessage")
                if item.get("phase").and_then(Value::as_str) == Some("final_answer") =>
            {
                return Some(item);
            }
            Some("exitedReviewMode") if newest_review.is_none() => newest_review = Some(item),
            Some("agentMessage") if newest_agent_message.is_none() => {
                newest_agent_message = Some(item)
            }
            _ => {}
        }
    }
    newest_review.or(newest_agent_message)
}

fn terminal_handoff_projection(item: &Value) -> Value {
    let text = item
        .get("text")
        .or_else(|| item.get("review"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut characters = text.chars();
    let clipped = characters
        .by_ref()
        .take(MAX_WAIT_HANDOFF_CHARS)
        .collect::<String>();
    let truncated = item
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || characters.next().is_some();
    json!({
        "id":item.get("id").cloned().unwrap_or(Value::Null),
        "type":item.get("type"),
        "phase":item.get("phase"),
        "text":clipped,
        "truncated":truncated
    })
}

fn persisted_handoff_page(
    item: Option<&Value>,
    text_offset: usize,
    selection_complete: bool,
) -> Result<Value, RelayError> {
    let text = item
        .and_then(|item| item.get("text").or_else(|| item.get("review")))
        .and_then(Value::as_str)
        .unwrap_or("");
    let text_len = text.chars().count();
    if text_offset > text_len {
        return Err(RelayError::Invalid(format!(
            "textOffset {text_offset} exceeds the result length {text_len}"
        )));
    }
    let mut characters = text.chars().skip(text_offset);
    let page_text = characters
        .by_ref()
        .take(MAX_WAIT_HANDOFF_CHARS)
        .collect::<String>();
    let page_len = page_text.chars().count();
    let next_text_offset = (text_offset + page_len < text_len).then_some(text_offset + page_len);
    let item_metadata = item.map(|item| {
        json!({
            "id":item.get("id").cloned().unwrap_or(Value::Null),
            "type":item.get("type"),
            "phase":item.get("phase")
        })
    });
    Ok(json!({
        "item":item_metadata,
        "text":page_text,
        "textOffset":text_offset,
        "nextTextOffset":next_text_offset,
        "hasMoreText":next_text_offset.is_some(),
        "selectionComplete":selection_complete
    }))
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
            completed_at: None,
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
                cwd: "/project".into(),
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
    fn observer_projects_latest_prompt_cache_usage() {
        let mut live = LiveTurns::default();
        live.insert("thread", test_turn("turn", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "turn",
            WorkerAnnotation {
                mode: "work".into(),
                cwd: "/project".into(),
                model: Some("gpt-6-luna".into()),
                effort: Some("low".into()),
                prompt: Some("inspect cache telemetry".into()),
            },
        );
        live.observe_event(
            "thread/tokenUsage/updated",
            &json!({
                "threadId":"thread",
                "turnId":"turn",
                "tokenUsage":{
                    "modelContextWindow":828400,
                    "last":{
                        "cacheWriteInputTokens":0,
                        "cachedInputTokens":15104,
                        "inputTokens":16314,
                        "outputTokens":19,
                        "reasoningOutputTokens":0,
                        "totalTokens":16333
                    },
                    "total":{"totalTokens":32270}
                }
            }),
        );
        let usage = &live.observer_workers()[0]["tokenUsage"];
        assert_eq!(usage["threadTotalTokens"], 32270);
        assert!(usage.get("totalTokens").is_none());
        assert_eq!(usage["lastRequestInputTokens"], 16314);
        assert_eq!(usage["lastRequestCachedInputTokens"], 15104);
        assert_eq!(usage["cacheHitPercent"], 92);
        assert!(usage["lastModelUsageAtMs"].as_u64().is_some());
        assert!(usage["cacheGuaranteedUntilMs"].as_u64().is_some());
        assert_eq!(usage["cacheGuaranteeActive"], true);
    }

    #[test]
    fn unchanged_thread_usage_snapshots_do_not_refresh_or_reattribute_last_request() {
        let mut live = LiveTurns::default();
        live.insert("thread", test_turn("first-turn", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "first-turn",
            WorkerAnnotation {
                mode: "work".into(),
                cwd: "/project".into(),
                model: None,
                effort: None,
                prompt: None,
            },
        );
        let usage_event = |turn_id: &str, total: u64, input: u64| {
            json!({
                "threadId":"thread",
                "turnId":turn_id,
                "tokenUsage":{
                    "modelContextWindow":200000,
                    "last":{
                        "cachedInputTokens":input / 2,
                        "inputTokens":input,
                        "outputTokens":10,
                        "reasoningOutputTokens":0,
                        "totalTokens":input + 10
                    },
                    "total":{"totalTokens":total}
                }
            })
        };

        live.observe_event(
            "thread/tokenUsage/updated",
            &usage_event("first-turn", 100, 80),
        );
        let first = live
            .observer_workers()
            .into_iter()
            .find(|worker| worker["turnId"] == "first-turn")
            .unwrap()["tokenUsage"]
            .clone();
        let first_at = first["lastModelUsageAtMs"].as_u64().unwrap();
        assert_eq!(first["threadTotalTokens"], 100);
        assert_eq!(first["lastRequestInputTokens"], 80);

        // A rate-limit-only update may repeat the previous last-request breakdown.
        live.observe_event(
            "thread/tokenUsage/updated",
            &usage_event("first-turn", 100, 999),
        );
        let workers = live.observer_workers();
        let unchanged = &workers
            .iter()
            .find(|worker| worker["turnId"] == "first-turn")
            .unwrap()["tokenUsage"];
        assert_eq!(unchanged["threadTotalTokens"], 100);
        assert_eq!(unchanged["lastRequestInputTokens"], 80);
        assert_eq!(unchanged["lastModelUsageAtMs"], first_at);
        assert_eq!(
            unchanged["cacheGuaranteedUntilMs"],
            first["cacheGuaranteedUntilMs"]
        );

        // A resume replay can arrive before its turn is present; it still seeds the
        // thread baseline so a later unchanged notification cannot be misattributed.
        live.observe_event(
            "thread/tokenUsage/updated",
            &usage_event("unobserved-replay-turn", 150, 120),
        );
        live.insert("thread", test_turn("second-turn", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "second-turn",
            WorkerAnnotation {
                mode: "work".into(),
                cwd: "/project".into(),
                model: None,
                effort: None,
                prompt: None,
            },
        );
        live.observe_event(
            "thread/tokenUsage/updated",
            &usage_event("second-turn", 150, 120),
        );
        let second = live
            .observer_workers()
            .into_iter()
            .find(|worker| worker["turnId"] == "second-turn")
            .unwrap()["tokenUsage"]
            .clone();
        assert_eq!(second["threadTotalTokens"], 150);
        assert!(second["lastRequestInputTokens"].is_null());
        assert!(second["lastRequestCachedInputTokens"].is_null());
        assert!(second["lastModelUsageAtMs"].is_null());
        assert!(second["cacheGuaranteedUntilMs"].is_null());
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
                cwd: "/project".into(),
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
                cwd: "/project".into(),
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
                cwd: "/project".into(),
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
                cwd: "/project".into(),
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
                cwd: "/project".into(),
                model: None,
                effort: None,
                prompt: Some("turn-0".into()),
            },
        );
        for index in 1..MAX_RECENT_WORKERS + 2 {
            let turn_id = format!("turn-{index}");
            live.insert("thread", test_turn(&turn_id, TurnStatus::Completed));
            live.annotate(
                "thread",
                &turn_id,
                WorkerAnnotation {
                    mode: "work".into(),
                    cwd: "/project".into(),
                    model: None,
                    effort: None,
                    prompt: Some(turn_id.clone()),
                },
            );
            live.record_recent("thread", &turn_id);
        }
        live.reconcile_terminal("thread", &test_turn("turn-0", TurnStatus::Completed));

        let workers = live.observer_workers();
        assert_eq!(workers.len(), MAX_RECENT_WORKERS);
        assert_eq!(workers[0]["turnId"], "turn-0");
        assert_eq!(
            workers[1]["turnId"],
            format!("turn-{}", MAX_RECENT_WORKERS + 1)
        );
        assert!(!workers.iter().any(|worker| worker["turnId"] == "turn-1"));

        let handles = live.operator_worker_handles();
        assert_eq!(handles.len(), MAX_RECENT_WORKERS);
        assert_eq!(handles[0]["turnId"], "turn-0");
        assert_eq!(
            handles[1]["turnId"],
            format!("turn-{}", MAX_RECENT_WORKERS + 1)
        );
        assert!(handles[0].get("tokenUsage").is_none());
        assert!(handles[0].get("transcriptRevision").is_none());
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
                cwd: "/project".into(),
                model: None,
                effort: None,
                prompt: Some("task".into()),
            },
        );
        live.record_recent("thread", "turn");
        assert!(live.recent.back().unwrap().2.turn.items.is_empty());
    }

    #[test]
    fn terminal_worker_retention_preserves_less_represented_cwds() {
        let mut live = LiveTurns::default();
        for index in 0..MAX_RECENT_WORKERS {
            let id = format!("a-{index}");
            live.insert("thread", test_turn(&id, TurnStatus::Completed));
            live.annotate(
                "thread",
                &id,
                WorkerAnnotation {
                    mode: "work".into(),
                    cwd: "/a".into(),
                    model: None,
                    effort: None,
                    prompt: None,
                },
            );
            live.record_recent("thread", &id);
        }
        for (id, cwd) in [("b", "/b"), ("a-new", "/a")] {
            live.insert("thread", test_turn(id, TurnStatus::Completed));
            live.annotate(
                "thread",
                id,
                WorkerAnnotation {
                    mode: "work".into(),
                    cwd: cwd.into(),
                    model: None,
                    effort: None,
                    prompt: None,
                },
            );
            live.record_recent("thread", id);
        }
        let handles = live.operator_worker_handles();
        assert_eq!(handles.len(), MAX_RECENT_WORKERS);
        assert!(
            handles
                .iter()
                .any(|handle| handle["turnId"] == "b" && handle["cwd"] == "/b")
        );
        assert!(!handles.iter().any(|handle| handle["turnId"] == "a-0"));
        assert!(!handles.iter().any(|handle| handle["turnId"] == "a-1"));
    }

    #[test]
    fn active_turns_survive_both_terminal_retention_bounds() {
        let mut live = LiveTurns::default();
        live.insert("thread", test_turn("active", TurnStatus::InProgress));
        live.annotate(
            "thread",
            "active",
            WorkerAnnotation {
                mode: "work".into(),
                cwd: "/project".into(),
                model: None,
                effort: None,
                prompt: Some("long task".into()),
            },
        );
        for index in 0..MAX_UNANNOTATED_TERMINALS + 20 {
            let id = format!("recent-{index}");
            live.insert("thread", test_turn(&id, TurnStatus::Completed));
            live.annotate(
                "thread",
                &id,
                WorkerAnnotation {
                    mode: "work".into(),
                    cwd: "/project".into(),
                    model: None,
                    effort: None,
                    prompt: None,
                },
            );
            live.record_recent("thread", &id);
        }
        assert_eq!(live.recent.len(), MAX_RECENT_WORKERS);
        assert_eq!(live.turns.len(), 1);
        for index in 0..MAX_UNANNOTATED_TERMINALS + 20 {
            live.insert(
                "auxiliary",
                test_turn(&format!("unannotated-{index}"), TurnStatus::Completed),
            );
        }
        assert_eq!(live.turns.len(), MAX_UNANNOTATED_TERMINALS + 1);
        assert_eq!(
            live.get("thread", "active").unwrap().status,
            TurnStatus::InProgress
        );
        assert_eq!(live.observer_workers()[0]["turnId"], "active");

        let (observed, changed) =
            live.reconcile_terminal("thread", &test_turn("active", TurnStatus::Completed));
        assert!(changed);
        assert_eq!(observed.unwrap().turn.status, TurnStatus::Completed);
        assert!(!live.turns.contains_key(&("thread".into(), "active".into())));
        assert_eq!(live.recent.len(), MAX_RECENT_WORKERS);
        let (_, changed_again) =
            live.reconcile_terminal("thread", &test_turn("active", TurnStatus::Completed));
        assert!(!changed_again);
        assert_eq!(live.recent.len(), MAX_RECENT_WORKERS);
    }

    #[test]
    fn wait_projection_returns_one_canonical_handoff_and_prefers_final_answer() {
        let mut turn = test_turn("turn", TurnStatus::Completed);
        turn.items = vec![
            json!({"type":"agentMessage","id":"older","phase":"commentary","text":"older"}),
            json!({"type":"exitedReviewMode","id":"review-1","review":"same terminal content"}),
            json!({"type":"exitedReviewMode","id":"review-2","review":"same terminal content"}),
            json!({"type":"agentMessage","id":"newest","phase":"commentary","text":"same terminal content"}),
            json!({
            "type":"agentMessage",
            "id":"final",
            "phase":"final_answer",
                "text":"same terminal content",
            }),
        ];
        let snapshot = turn_snapshot(&turn, Some(false));
        let output = snapshot["output"].as_array().unwrap().clone();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["id"], "final");
        assert_eq!(output[0]["phase"], "final_answer");
        assert_eq!(output[0]["text"], "same terminal content");
        assert_eq!(output[0]["truncated"], false);
        assert_eq!(snapshot["selectionIncomplete"], false);
    }

    #[test]
    fn wait_projection_prefers_review_then_newest_agent_message() {
        let mut turn = test_turn("turn", TurnStatus::Completed);
        turn.items = vec![
            json!({"type":"agentMessage","id":"old","phase":"commentary","text":"old"}),
            json!({"type":"exitedReviewMode","id":"review-old","review":"same review finding"}),
            json!({"type":"exitedReviewMode","id":"review-newest","review":"same review finding"}),
            json!({"type":"agentMessage","id":"newest","phase":"commentary","text":"newest"}),
        ];
        let output = turn_snapshot(&turn, Some(false))["output"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["id"], "review-newest");
        assert_eq!(output[0]["text"], "same review finding");

        turn.items
            .retain(|item| item.get("type").and_then(Value::as_str) != Some("exitedReviewMode"));
        let output = turn_snapshot(&turn, Some(false))["output"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["id"], "newest");
    }

    #[test]
    fn wait_projection_caps_handoff_text_at_10240_characters() {
        let mut turn = test_turn("turn", TurnStatus::Completed);
        turn.items = vec![json!({
            "type":"agentMessage",
            "id":"final",
            "phase":"final_answer",
            "text":"x".repeat(MAX_WAIT_HANDOFF_CHARS + 1),
        })];
        let output = turn_snapshot(&turn, Some(false))["output"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["truncated"], true);
        assert_eq!(
            output[0]["text"].as_str().unwrap().chars().count(),
            10 * 1024
        );
    }

    #[test]
    fn wait_projection_reports_incomplete_selection_even_without_output() {
        let turn = test_turn("turn", TurnStatus::Completed);
        let snapshot = turn_snapshot(&turn, Some(true));
        assert_eq!(snapshot["output"], json!([]));
        assert_eq!(snapshot["selectionIncomplete"], true);
    }

    #[test]
    fn wait_text_truncation_does_not_mark_selection_incomplete() {
        let mut turn = test_turn("turn", TurnStatus::Completed);
        turn.items = vec![json!({
            "type":"agentMessage",
            "id":"final",
            "phase":"final_answer",
            "text":"x".repeat(MAX_WAIT_HANDOFF_CHARS + 1),
        })];
        let snapshot = turn_snapshot(&turn, Some(false));
        assert_eq!(snapshot["output"][0]["truncated"], true);
        assert_eq!(snapshot["selectionIncomplete"], false);
    }

    #[test]
    fn result_inspection_deadline_is_not_reset_after_metadata() {
        let started_at = Instant::now();
        let deadline = result_selection_deadline(started_at);
        let metadata_finished_at = started_at + Duration::from_secs(25);
        assert_eq!(
            deadline.duration_since(metadata_finished_at),
            Duration::from_secs(5)
        );
        assert_eq!(result_selection_deadline(started_at), deadline);
    }

    #[test]
    fn repeated_pagination_cursor_fails_visibly() {
        let mut seen = HashSet::new();
        assert_eq!(
            checked_next_cursor(&mut seen, Some("page-2".into()), "thread/items/list").unwrap(),
            Some("page-2".into())
        );
        assert!(
            checked_next_cursor(&mut seen, Some("page-2".into()), "thread/items/list").is_err()
        );
    }
}
