use codex_connect_app_server::protocol::{CommandExecOutputStream, CommandExecResponse};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, watch};

const MAX_SESSIONS: usize = 32;
const MAX_RETAINED_BYTES: usize = 512 * 1024;
const MAX_CHUNK_BYTES: usize = 64 * 1024;
const MAX_BATCH_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub struct CommandSessions {
    state: Arc<Mutex<CommandSessionState>>,
    changed: watch::Sender<u64>,
}

impl Default for CommandSessions {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(CommandSessionState::default())),
            changed: watch::channel(0).0,
        }
    }
}

#[derive(Default)]
struct CommandSessionState {
    sessions: HashMap<String, CommandSession>,
    order: VecDeque<String>,
}

struct CommandSession {
    tty: bool,
    stdin_open: bool,
    cursor: u64,
    dropped_through: u64,
    retained_bytes: usize,
    chunks: VecDeque<OutputChunk>,
    terminal: Option<TerminalState>,
}

struct OutputChunk {
    cursor: u64,
    stream: CommandExecOutputStream,
    bytes: Vec<u8>,
}

enum TerminalState {
    Exited { exit_code: i32 },
    Failed { error: String },
}

pub struct ReadBatch {
    pub value: Value,
    pub changed_after: bool,
    pub terminal: bool,
}

impl CommandSessions {
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn wake(&self) {
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    pub async fn insert(&self, process_id: String, tty: bool) -> Result<(), String> {
        let mut state = self.state.lock().await;
        while state.sessions.len() >= MAX_SESSIONS {
            let Some(index) = state.order.iter().position(|id| {
                state
                    .sessions
                    .get(id)
                    .is_some_and(|session| session.terminal.is_some())
            }) else {
                return Err(format!(
                    "at most {MAX_SESSIONS} persistent command sessions may be active"
                ));
            };
            if let Some(id) = state.order.remove(index) {
                state.sessions.remove(&id);
            }
        }
        state.order.push_back(process_id.clone());
        state.sessions.insert(
            process_id,
            CommandSession {
                tty,
                stdin_open: true,
                cursor: 0,
                dropped_through: 0,
                retained_bytes: 0,
                chunks: VecDeque::new(),
                terminal: None,
            },
        );
        Ok(())
    }

    pub async fn remove(&self, process_id: &str) {
        let mut state = self.state.lock().await;
        state.sessions.remove(process_id);
        state.order.retain(|id| id != process_id);
        drop(state);
        self.wake();
    }

    pub async fn mark_process_gap(&self, process_id: &str) {
        let mut state = self.state.lock().await;
        let Some(session) = state.sessions.get_mut(process_id) else {
            return;
        };
        session.cursor = session.cursor.wrapping_add(1);
        session.dropped_through = session.cursor;
        drop(state);
        self.wake();
    }

    pub async fn push_output(
        &self,
        process_id: &str,
        stream: CommandExecOutputStream,
        mut bytes: Vec<u8>,
    ) {
        let mut state = self.state.lock().await;
        let Some(session) = state.sessions.get_mut(process_id) else {
            return;
        };
        session.cursor = session.cursor.wrapping_add(1);
        if bytes.len() > MAX_CHUNK_BYTES {
            let drain = bytes.len() - MAX_CHUNK_BYTES;
            bytes.drain(..drain);
            session.dropped_through = session.cursor;
        }
        session.retained_bytes = session.retained_bytes.saturating_add(bytes.len());
        session.chunks.push_back(OutputChunk {
            cursor: session.cursor,
            stream,
            bytes,
        });
        while session.retained_bytes > MAX_RETAINED_BYTES {
            let Some(dropped) = session.chunks.pop_front() else {
                break;
            };
            session.retained_bytes = session.retained_bytes.saturating_sub(dropped.bytes.len());
            session.dropped_through = session.dropped_through.max(dropped.cursor);
        }
        drop(state);
        self.wake();
    }

    pub async fn complete(&self, process_id: &str, response: CommandExecResponse) {
        let mut state = self.state.lock().await;
        let Some(session) = state.sessions.get_mut(process_id) else {
            return;
        };
        session.cursor = session.cursor.wrapping_add(1);
        session.stdin_open = false;
        session.terminal = Some(TerminalState::Exited {
            exit_code: response.exit_code,
        });
        drop(state);
        self.wake();
    }

    pub async fn fail(&self, process_id: &str, error: String) {
        let mut state = self.state.lock().await;
        let Some(session) = state.sessions.get_mut(process_id) else {
            return;
        };
        session.cursor = session.cursor.wrapping_add(1);
        session.stdin_open = false;
        session.terminal = Some(TerminalState::Failed { error });
        drop(state);
        self.wake();
    }

    pub async fn set_stdin_closed(&self, process_id: &str) {
        let mut state = self.state.lock().await;
        if let Some(session) = state.sessions.get_mut(process_id) {
            session.stdin_open = false;
        }
    }

    pub async fn ensure_running(&self, process_id: &str) -> Result<(bool, bool), String> {
        let state = self.state.lock().await;
        let session = state.sessions.get(process_id).ok_or_else(stale_handle)?;
        if session.terminal.is_some() {
            return Err(format!("command session {process_id} is no longer running"));
        }
        Ok((session.tty, session.stdin_open))
    }

    pub async fn read_after(&self, process_id: &str, after: u64) -> Result<ReadBatch, String> {
        let state = self.state.lock().await;
        let session = state.sessions.get(process_id).ok_or_else(stale_handle)?;
        if after > session.cursor {
            return Err(
                "cursor is ahead of this command session; after a backend restart use the processId returned by the new backend"
                    .into(),
            );
        }
        let history_lost = after < session.dropped_through;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut bytes = 0usize;
        let mut cursor = after;
        let mut omitted_newer_chunk = false;
        for chunk in session.chunks.iter().filter(|chunk| chunk.cursor > after) {
            if bytes > 0 && bytes.saturating_add(chunk.bytes.len()) > MAX_BATCH_BYTES {
                omitted_newer_chunk = true;
                break;
            }
            bytes = bytes.saturating_add(chunk.bytes.len());
            match chunk.stream {
                CommandExecOutputStream::Stdout => stdout.extend_from_slice(&chunk.bytes),
                CommandExecOutputStream::Stderr => stderr.extend_from_slice(&chunk.bytes),
            }
            cursor = chunk.cursor;
        }
        if !omitted_newer_chunk {
            cursor = session.cursor;
        }
        let (state_name, exit_code, error, terminal) = match &session.terminal {
            None => ("running", Value::Null, Value::Null, false),
            Some(TerminalState::Exited { exit_code }) => {
                ("exited", json!(exit_code), Value::Null, true)
            }
            Some(TerminalState::Failed { error }) => ("failed", Value::Null, json!(error), true),
        };
        Ok(ReadBatch {
            changed_after: session.cursor > after,
            terminal,
            value: json!({
                "processId":process_id,
                "state":state_name,
                "tty":session.tty,
                "stdinOpen":session.stdin_open,
                "cursor":cursor,
                "historyLost":history_lost,
                "stdout":String::from_utf8_lossy(&stdout),
                "stderr":String::from_utf8_lossy(&stderr),
                "exitCode":exit_code,
                "error":error,
            }),
        })
    }
}

fn stale_handle() -> String {
    "unknown, evicted, or stale processId; completed command-session projections may be evicted by bounded retention, and all handles are App Server connection-scoped and do not survive connector restart"
        .into()
}
