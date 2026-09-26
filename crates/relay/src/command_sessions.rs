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

fn retain_chunk(session: &mut CommandSession, stream: CommandExecOutputStream, bytes: Vec<u8>) {
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
}

fn flush_utf8_pending(session: &mut CommandSession) {
    for stream in [
        CommandExecOutputStream::Stdout,
        CommandExecOutputStream::Stderr,
    ] {
        let bytes = match stream {
            CommandExecOutputStream::Stdout => std::mem::take(&mut session.stdout_utf8_pending),
            CommandExecOutputStream::Stderr => std::mem::take(&mut session.stderr_utf8_pending),
        };
        if !bytes.is_empty() {
            retain_chunk(session, stream, bytes);
        }
    }
}

fn incomplete_utf8_start(bytes: &[u8]) -> Option<usize> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => return None,
            Err(error) => {
                offset += error.valid_up_to();
                match error.error_len() {
                    Some(length) => offset = offset.saturating_add(length),
                    None => return Some(offset),
                }
            }
        }
    }
    None
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
    cwd: String,
    tty: bool,
    stdin_open: bool,
    cursor: u64,
    dropped_through: u64,
    retained_bytes: usize,
    chunks: VecDeque<OutputChunk>,
    stdout_utf8_pending: Vec<u8>,
    stderr_utf8_pending: Vec<u8>,
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

    pub async fn handles(&self) -> Vec<Value> {
        let state = self.state.lock().await;
        state
            .order
            .iter()
            .filter_map(|process_id| {
                let session = state.sessions.get(process_id)?;
                let status = match &session.terminal {
                    None => "running",
                    Some(TerminalState::Exited { .. }) => "exited",
                    Some(TerminalState::Failed { .. }) => "failed",
                };
                Some(json!({
                    "processId":process_id,
                    "cwd":session.cwd,
                    "state":status,
                    "tty":session.tty,
                }))
            })
            .collect()
    }

    fn wake(&self) {
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    pub async fn insert(&self, process_id: String, cwd: String, tty: bool) -> Result<(), String> {
        let mut state = self.state.lock().await;
        while state.sessions.len() >= MAX_SESSIONS {
            let mut counts = HashMap::<&str, usize>::new();
            for session in state
                .sessions
                .values()
                .filter(|session| session.terminal.is_some())
            {
                *counts.entry(&session.cwd).or_default() += 1;
            }
            let largest = counts.values().copied().max().unwrap_or(0);
            let Some(index) = state.order.iter().position(|id| {
                state.sessions.get(id).is_some_and(|session| {
                    session.terminal.is_some()
                        && counts.get(session.cwd.as_str()).copied() == Some(largest)
                })
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
                cwd,
                tty,
                stdin_open: true,
                cursor: 0,
                dropped_through: 0,
                retained_bytes: 0,
                chunks: VecDeque::new(),
                stdout_utf8_pending: Vec::new(),
                stderr_utf8_pending: Vec::new(),
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
        session.stdout_utf8_pending.clear();
        session.stderr_utf8_pending.clear();
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
        let pending = match stream {
            CommandExecOutputStream::Stdout => &mut session.stdout_utf8_pending,
            CommandExecOutputStream::Stderr => &mut session.stderr_utf8_pending,
        };
        if !pending.is_empty() {
            let mut merged = std::mem::take(pending);
            merged.extend_from_slice(&bytes);
            bytes = merged;
        }
        if bytes.len() > MAX_CHUNK_BYTES {
            let drain = bytes.len() - MAX_CHUNK_BYTES;
            bytes.drain(..drain);
            session.dropped_through = session.cursor;
        }
        if let Some(split) = incomplete_utf8_start(&bytes) {
            let trailing = bytes.split_off(split);
            match stream {
                CommandExecOutputStream::Stdout => session.stdout_utf8_pending = trailing,
                CommandExecOutputStream::Stderr => session.stderr_utf8_pending = trailing,
            }
        }
        if !bytes.is_empty() {
            retain_chunk(session, stream, bytes);
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
        flush_utf8_pending(session);
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
        flush_utf8_pending(session);
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
                "hasMoreOutput":omitted_newer_chunk,
                "drained":terminal && !omitted_newer_chunk,
                "stdout":String::from_utf8_lossy(&stdout),
                "stderr":String::from_utf8_lossy(&stderr),
                "exitCode":exit_code,
                "error":error,
            }),
        })
    }
}

fn stale_handle() -> String {
    "unknown or expired processId; completed handles may be evicted, and backend/App Server restart invalidates all handles"
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_reads_report_retained_output_drain_state() {
        let sessions = CommandSessions::default();
        sessions
            .insert("process".into(), "/project".into(), false)
            .await
            .unwrap();
        sessions
            .push_output(
                "process",
                CommandExecOutputStream::Stdout,
                vec![b'a'; 64 * 1024],
            )
            .await;
        sessions
            .push_output(
                "process",
                CommandExecOutputStream::Stderr,
                vec![b'b'; 64 * 1024],
            )
            .await;
        sessions
            .push_output(
                "process",
                CommandExecOutputStream::Stdout,
                vec![b'c'; 64 * 1024],
            )
            .await;
        sessions
            .complete(
                "process",
                CommandExecResponse {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )
            .await;

        let first = sessions.read_after("process", 0).await.unwrap();
        assert!(first.terminal);
        assert_eq!(first.value["hasMoreOutput"], true);
        assert_eq!(first.value["drained"], false);
        let cursor = first.value["cursor"].as_u64().unwrap();

        let second = sessions.read_after("process", cursor).await.unwrap();
        assert!(second.terminal);
        assert_eq!(second.value["hasMoreOutput"], false);
        assert_eq!(second.value["drained"], true);
        assert_eq!(second.value["cursor"].as_u64(), Some(4));
    }

    #[tokio::test]
    async fn split_utf8_is_buffered_across_output_notifications() {
        let sessions = CommandSessions::default();
        sessions
            .insert("process".into(), "/project".into(), false)
            .await
            .unwrap();
        sessions
            .push_output("process", CommandExecOutputStream::Stdout, vec![0xe2, 0x82])
            .await;
        let first = sessions.read_after("process", 0).await.unwrap();
        assert_eq!(first.value["stdout"], "");
        assert_eq!(first.value["cursor"], 1);

        sessions
            .push_output("process", CommandExecOutputStream::Stdout, vec![0xac])
            .await;
        let second = sessions.read_after("process", 1).await.unwrap();
        assert_eq!(second.value["stdout"], "€");
        assert_eq!(second.value["cursor"], 2);
        assert_eq!(second.value["historyLost"], false);
    }

    #[tokio::test]
    async fn terminal_flushes_incomplete_utf8_without_hiding_bytes() {
        let sessions = CommandSessions::default();
        sessions
            .insert("process".into(), "/project".into(), false)
            .await
            .unwrap();
        sessions
            .push_output("process", CommandExecOutputStream::Stderr, vec![0xe2])
            .await;
        sessions
            .complete(
                "process",
                CommandExecResponse {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )
            .await;
        let result = sessions.read_after("process", 0).await.unwrap();
        assert_eq!(result.value["stderr"], "�");
        assert_eq!(result.value["drained"], true);
    }

    #[tokio::test]
    async fn handle_snapshot_is_compact_and_includes_terminal_sessions() {
        let sessions = CommandSessions::default();
        sessions
            .insert("running".into(), "/project".into(), true)
            .await
            .unwrap();
        sessions
            .insert("exited".into(), "/project".into(), false)
            .await
            .unwrap();
        sessions
            .insert("failed".into(), "/project".into(), false)
            .await
            .unwrap();
        sessions
            .complete(
                "exited",
                CommandExecResponse {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )
            .await;
        sessions.fail("failed", "secret diagnostic".into()).await;

        assert_eq!(
            sessions.handles().await,
            vec![
                json!({"processId":"running","cwd":"/project","state":"running","tty":true}),
                json!({"processId":"exited","cwd":"/project","state":"exited","tty":false}),
                json!({"processId":"failed","cwd":"/project","state":"failed","tty":false}),
            ]
        );
    }

    #[tokio::test]
    async fn terminal_command_eviction_preserves_less_represented_cwds_and_active_handles() {
        let sessions = CommandSessions::default();
        sessions
            .insert("active".into(), "/a".into(), false)
            .await
            .unwrap();
        for index in 0..MAX_SESSIONS - 1 {
            let id = format!("a-{index}");
            sessions
                .insert(id.clone(), "/a".into(), false)
                .await
                .unwrap();
            sessions
                .complete(
                    &id,
                    CommandExecResponse {
                        exit_code: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    },
                )
                .await;
        }
        sessions
            .insert("b".into(), "/b".into(), false)
            .await
            .unwrap();
        sessions
            .complete(
                "b",
                CommandExecResponse {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )
            .await;
        sessions
            .insert("a-new".into(), "/a".into(), false)
            .await
            .unwrap();
        let handles = sessions.handles().await;
        assert_eq!(handles.len(), MAX_SESSIONS);
        assert!(handles.iter().any(|handle| handle["processId"] == "active"));
        assert!(
            handles
                .iter()
                .any(|handle| handle["processId"] == "b" && handle["cwd"] == "/b")
        );
        assert!(!handles.iter().any(|handle| handle["processId"] == "a-0"));
        assert!(!handles.iter().any(|handle| handle["processId"] == "a-1"));
    }
}
