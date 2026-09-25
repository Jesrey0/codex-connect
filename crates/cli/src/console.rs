use crate::backend::BackendClient;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{self, IsTerminal, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::unix::AsyncFd;
use tokio::time::{Instant, MissedTickBehavior, interval};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const UI_TICK_MS: u64 = 750;
const RECONNECT_MS: u64 = 500;
const TRANSCRIPT_RETRY_MAX_MS: u64 = 15_000;
const ESCAPE_SEQUENCE_MS: u64 = 50;
const MAX_ACTION_TEXT_CHARS: usize = 2 * 1024;
const MAX_ACTION_ROW_CHARS: usize = 512;
const RESET: &str = "\x1b[0m";
// Neutral 256-color palette: bright text stays grey-white, hierarchy comes from
// luminance rather than saturated decoration. Warning/error hues are intentionally muted.
const BOLD: &str = "\x1b[1m\x1b[38;5;255m";
const TEXT: &str = "\x1b[38;5;252m";
const DIM: &str = "\x1b[38;5;245m";
const CYAN: &str = "\x1b[38;5;250m";
const YELLOW: &str = "\x1b[38;5;180m";
const RED: &str = "\x1b[38;5;203m";

pub async fn run() -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("the console requires an interactive terminal");
    }
    let backend = BackendClient::new();
    let mut screen = ScreenGuard::enter()?;
    let mut input = TerminalInput::enter()?;
    let mut ticker = interval(Duration::from_millis(UI_TICK_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let initial = backend.observe(None).await;
    let (mut snapshot, mut observer_cursor, mut last_error) = match initial {
        Ok(value) => {
            let cursor = value["cursor"].as_u64();
            (Some(value), cursor, None)
        }
        Err(error) => (None, None, Some(error.to_string())),
    };
    let mut observer_task = spawn_observer_task(backend, observer_cursor);
    let mut state = ConsoleState::default();
    let mut transcript_fetch = TranscriptFetch::default();
    let mut frame = 0usize;

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("unable to listen for Ctrl-C")?;
                break;
            }
            result = &mut observer_task => {
                match result {
                    Ok(Ok(value)) => {
                        if observer_cursor.is_none() {
                            // Observer revisions restart with the backend. Re-read an open
                            // transcript once after reconnect, even if its revision is lower.
                            transcript_fetch.cancel();
                            transcript_fetch.refresh_required = true;
                            transcript_fetch.next_attempt = None;
                        }
                        observer_cursor = value["cursor"].as_u64();
                        last_error = None;
                        let _ = sync_transcript_from_snapshot(&mut state, &value);
                        snapshot = Some(value);
                    }
                    Ok(Err(error)) => {
                        last_error = Some(error.to_string());
                        observer_cursor = None;
                    }
                    Err(error) => {
                        last_error = Some(format!("observer task failed: {error}"));
                        observer_cursor = None;
                    }
                }
                observer_task = spawn_observer_task(backend, observer_cursor);
                screen.draw(snapshot.as_ref(), last_error.as_deref(), &mut state, &transcript_fetch, frame)?;
            }
            _ = ticker.tick() => {
                screen.draw(snapshot.as_ref(), last_error.as_deref(), &mut state, &transcript_fetch, frame)?;
                frame = frame.wrapping_add(1);
            }
            result = async { transcript_fetch.task.as_mut().expect("guarded transcript task").await }, if transcript_fetch.task.is_some() => {
                transcript_fetch.finish(result, &mut state, snapshot.as_ref());
                screen.draw(snapshot.as_ref(), last_error.as_deref(), &mut state, &transcript_fetch, frame)?;
            }
            keys = input.next_keys() => {
                let mut changed = false;
                for key in keys? {
                    changed |= state.handle_key(key, snapshot.as_ref());
                }
                if state.quit {
                    break;
                }
                if changed {
                    transcript_fetch.reset_for_target(state.transcript_target());
                    screen.draw(snapshot.as_ref(), last_error.as_deref(), &mut state, &transcript_fetch, frame)?;
                }
            }
        }
        transcript_fetch.schedule(backend, &state, snapshot.as_ref());
    }
    observer_task.abort();
    transcript_fetch.cancel();
    Ok(())
}

fn spawn_observer_task(
    backend: BackendClient,
    cursor: Option<u64>,
) -> tokio::task::JoinHandle<Result<Value>> {
    tokio::spawn(async move {
        match cursor {
            Some(cursor) => backend.observe(Some(cursor)).await,
            None => {
                tokio::time::sleep(Duration::from_millis(RECONNECT_MS)).await;
                backend.observe(None).await
            }
        }
    })
}

#[derive(Default)]
struct ScreenGuard {
    lines: Vec<String>,
    size: (usize, usize),
}

impl ScreenGuard {
    fn enter() -> Result<Self> {
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[2J\x1b[H")?;
        stdout.flush()?;
        Ok(Self::default())
    }

    fn draw(
        &mut self,
        snapshot: Option<&Value>,
        last_error: Option<&str>,
        state: &mut ConsoleState,
        fetch: &TranscriptFetch,
        frame: usize,
    ) -> Result<()> {
        let (width, height) = terminal_size();
        let lines = render_frame(snapshot, last_error, state, fetch, frame, width, height);
        let mut stdout = io::stdout().lock();
        if self.size != (width, height) {
            write!(stdout, "\x1b[2J")?;
            self.lines.clear();
            self.size = (width, height);
        }
        // Address rows explicitly: newline/autowrap at the bottom row can scroll
        // the alternate screen. Unchanged rows need no repaint.
        for (index, line) in lines.iter().enumerate() {
            if self.lines.get(index) != Some(line) {
                write!(stdout, "\x1b[{};1H{line}\x1b[K", index + 1)?;
            }
        }
        if lines.len() < self.lines.len() {
            write!(stdout, "\x1b[{};1H\x1b[J", lines.len() + 1)?;
        }
        stdout.flush()?;
        self.lines = lines;
        Ok(())
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "{RESET}\x1b[?2004l\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
    }
}

struct TerminalInput {
    original: libc::termios,
    original_flags: libc::c_int,
    readiness: AsyncFd<StdinFd>,
    decoder: InputDecoder,
    escape_deadline: Option<Instant>,
}

struct StdinFd;

impl AsRawFd for StdinFd {
    fn as_raw_fd(&self) -> RawFd {
        libc::STDIN_FILENO
    }
}

impl TerminalInput {
    fn enter() -> Result<Self> {
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Err(io::Error::last_os_error()).context("unable to read terminal settings");
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_iflag &= !libc::IXON;
        // Keep ISIG enabled: Ctrl-C must continue to be delivered as SIGINT.
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error()).context("unable to configure terminal input");
        }
        let original_flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
        if original_flags < 0 {
            let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
            return Err(io::Error::last_os_error()).context("unable to read terminal flags");
        }
        if unsafe {
            libc::fcntl(
                libc::STDIN_FILENO,
                libc::F_SETFL,
                original_flags | libc::O_NONBLOCK,
            )
        } < 0
        {
            let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
            return Err(io::Error::last_os_error()).context("unable to configure terminal flags");
        }
        let readiness = match AsyncFd::new(StdinFd) {
            Ok(readiness) => readiness,
            Err(error) => {
                let _ = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, original_flags) };
                let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
                return Err(error).context("unable to register terminal input readiness");
            }
        };
        Ok(Self {
            original,
            original_flags,
            readiness,
            decoder: InputDecoder::default(),
            escape_deadline: None,
        })
    }

    async fn next_keys(&mut self) -> Result<Vec<InputKey>> {
        loop {
            if let Some(deadline) = self.escape_deadline {
                tokio::select! {
                    ready = self.readiness.readable() => {
                        let mut guard = ready.context("unable to wait for terminal input")?;
                        let keys = Self::read_keys_now(
                            &mut self.decoder,
                            &mut self.escape_deadline,
                        )?;
                        guard.clear_ready();
                        if !keys.is_empty() {
                            return Ok(keys);
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        self.escape_deadline = None;
                        return Ok(self.decoder.flush_escape().into_iter().collect());
                    }
                }
            } else {
                let mut guard = self
                    .readiness
                    .readable()
                    .await
                    .context("unable to wait for terminal input")?;
                let keys = Self::read_keys_now(&mut self.decoder, &mut self.escape_deadline)?;
                guard.clear_ready();
                if !keys.is_empty() {
                    return Ok(keys);
                }
            }
        }
    }

    fn read_keys_now(
        decoder: &mut InputDecoder,
        escape_deadline: &mut Option<Instant>,
    ) -> Result<Vec<InputKey>> {
        let mut keys = Vec::new();
        loop {
            let mut bytes = [0_u8; 64];
            let read = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    bytes.as_mut_ptr().cast::<libc::c_void>(),
                    bytes.len(),
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(error).context("unable to read terminal input");
            }
            if read == 0 {
                bail!("terminal input closed");
            }
            for byte in &bytes[..read as usize] {
                keys.extend(decoder.push(*byte));
            }
        }
        if decoder.has_pending_escape() {
            escape_deadline
                .get_or_insert_with(|| Instant::now() + Duration::from_millis(ESCAPE_SEQUENCE_MS));
        } else {
            *escape_deadline = None;
        }
        Ok(keys)
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        let _ = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, self.original_flags) };
        let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original) };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputKey {
    Character(char),
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Escape,
}

#[derive(Default)]
struct InputDecoder {
    escape: Vec<u8>,
    paste: bool,
}

impl InputDecoder {
    fn has_pending_escape(&self) -> bool {
        !self.escape.is_empty()
    }

    fn push(&mut self, byte: u8) -> Vec<InputKey> {
        if self.escape.is_empty() {
            if byte == b'\x1b' {
                self.escape.push(byte);
                return Vec::new();
            }
            return if self.paste { None } else { byte_to_key(byte) }
                .into_iter()
                .collect();
        }
        if self.escape.len() == 1 {
            if matches!(byte, b'[' | b'O') {
                self.escape.push(byte);
            } else {
                // Consume Alt/unknown escape keys without replaying them as shortcuts.
                self.escape.clear();
            }
            return Vec::new();
        }
        let complete = (0x40..=0x7e).contains(&byte);
        if self.escape.len() < 32 {
            self.escape.push(byte);
        }
        if !complete {
            return Vec::new();
        }
        let key = match self.escape.as_slice() {
            b"\x1b[200~" => {
                self.paste = true;
                None
            }
            b"\x1b[201~" => {
                self.paste = false;
                None
            }
            _ if self.paste => None,
            b"\x1b[A" | b"\x1bOA" => Some(InputKey::Up),
            b"\x1b[B" | b"\x1bOB" => Some(InputKey::Down),
            b"\x1b[C" | b"\x1bOC" => Some(InputKey::Right),
            b"\x1b[D" | b"\x1bOD" => Some(InputKey::Left),
            b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" | b"\x1b[7~" => Some(InputKey::Home),
            b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" | b"\x1b[8~" => Some(InputKey::End),
            b"\x1b[5~" => Some(InputKey::PageUp),
            b"\x1b[6~" => Some(InputKey::PageDown),
            _ => None,
        };
        self.escape.clear();
        key.into_iter().collect()
    }

    fn flush_escape(&mut self) -> Option<InputKey> {
        let standalone = self.escape == b"\x1b" && !self.paste;
        self.escape.clear();
        standalone.then_some(InputKey::Escape)
    }
}

fn byte_to_key(byte: u8) -> Option<InputKey> {
    match byte {
        b'\r' | b'\n' => Some(InputKey::Enter),
        b' '..=b'~' => Some(InputKey::Character(byte as char)),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TranscriptTarget {
    thread_id: String,
    turn_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum View {
    Dashboard,
    Transcript(TranscriptTarget),
}

struct ConsoleState {
    view: View,
    transcript: Option<Value>,
    transcript_error: Option<String>,
    transcript_revision: u64,
    scroll: usize,
    follow: bool,
    selected_worker: usize,
    selected_worker_target: Option<TranscriptTarget>,
    active_only: bool,
    help: bool,
    quit: bool,
    page_size: usize,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            view: View::Dashboard,
            transcript: None,
            transcript_error: None,
            transcript_revision: 0,
            scroll: 0,
            follow: true,
            selected_worker: 0,
            selected_worker_target: None,
            active_only: false,
            help: false,
            quit: false,
            page_size: 1,
        }
    }
}

impl ConsoleState {
    fn transcript_target(&self) -> Option<&TranscriptTarget> {
        match &self.view {
            View::Dashboard => None,
            View::Transcript(target) => Some(target),
        }
    }

    fn handle_key(&mut self, key: InputKey, snapshot: Option<&Value>) -> bool {
        if key == InputKey::Character('?') {
            self.help = !self.help;
            return true;
        }
        if self.help {
            if matches!(key, InputKey::Escape | InputKey::Character('q')) {
                self.help = false;
                return true;
            }
            return false;
        }
        match &self.view {
            View::Dashboard => {
                if key == InputKey::Character('q') {
                    self.quit = true;
                    return true;
                }
                if key == InputKey::Character('a') {
                    self.active_only = !self.active_only;
                }
                let turns = visible_workers(snapshot, self.active_only);
                sync_worker_selection(self, &turns);
                let next = match key {
                    InputKey::Up | InputKey::Character('k') => {
                        self.selected_worker.saturating_sub(1)
                    }
                    InputKey::Down | InputKey::Character('j') => {
                        self.selected_worker.saturating_add(1)
                    }
                    InputKey::PageUp => self.selected_worker.saturating_sub(self.page_size),
                    InputKey::PageDown => self.selected_worker.saturating_add(self.page_size),
                    InputKey::Home | InputKey::Character('g') => 0,
                    InputKey::End | InputKey::Character('G') => turns.len().saturating_sub(1),
                    InputKey::Enter | InputKey::Right => {
                        let Some(target) = turns
                            .get(self.selected_worker)
                            .and_then(|turn| transcript_target_for_turn(turn))
                        else {
                            return false;
                        };
                        self.selected_worker_target = Some(target.clone());
                        self.view = View::Transcript(target);
                        self.transcript = None;
                        self.transcript_error = None;
                        self.transcript_revision = 0;
                        self.scroll = 0;
                        self.follow = true;
                        return true;
                    }
                    InputKey::Character('a') => return true,
                    _ => return false,
                }
                .min(turns.len().saturating_sub(1));
                let changed = next != self.selected_worker;
                self.selected_worker = next;
                self.selected_worker_target = turns
                    .get(next)
                    .and_then(|turn| transcript_target_for_turn(turn));
                changed
            }
            View::Transcript(_) => match key {
                InputKey::Escape | InputKey::Left | InputKey::Character('b' | 'q') => {
                    self.view = View::Dashboard;
                    self.transcript_error = None;
                    self.transcript_revision = 0;
                    true
                }
                InputKey::Character('j') | InputKey::Down | InputKey::PageDown => {
                    let step = if key == InputKey::PageDown {
                        self.page_size
                    } else {
                        1
                    };
                    self.scroll = self.scroll.saturating_add(step);
                    self.follow = false;
                    true
                }
                InputKey::Character('k') | InputKey::Up | InputKey::PageUp => {
                    let step = if key == InputKey::PageUp {
                        self.page_size
                    } else {
                        1
                    };
                    self.scroll = self.scroll.saturating_sub(step);
                    self.follow = false;
                    true
                }
                InputKey::Character('G') | InputKey::End => {
                    self.follow = true;
                    true
                }
                InputKey::Character('g') | InputKey::Home => {
                    self.scroll = 0;
                    self.follow = false;
                    true
                }
                _ => false,
            },
        }
    }
}

#[derive(Default)]
struct TranscriptFetch {
    task: Option<tokio::task::JoinHandle<Result<Value>>>,
    target: Option<TranscriptTarget>,
    requested_revision: u64,
    last_success: Option<Instant>,
    next_attempt: Option<Instant>,
    failures: u32,
    refresh_required: bool,
}

impl TranscriptFetch {
    fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    fn reset_for_target(&mut self, target: Option<&TranscriptTarget>) {
        if self.target.as_ref() != target {
            self.cancel();
            self.target = target.cloned();
            self.last_success = None;
            self.next_attempt = None;
            self.failures = 0;
        }
    }

    fn should_fetch(&self, state: &ConsoleState, snapshot: Option<&Value>, now: Instant) -> bool {
        let Some(target) = state.transcript_target() else {
            return false;
        };
        if self.task.is_some() || self.next_attempt.is_some_and(|at| now < at) {
            return false;
        }
        let revision = snapshot
            .and_then(|snapshot| worker_for_target(snapshot, target))
            .and_then(|worker| worker["transcriptRevision"].as_u64())
            .unwrap_or_default();
        self.refresh_required
            || self.failures > 0
            || state.transcript.is_none()
            || revision > state.transcript_revision
    }

    fn schedule(&mut self, backend: BackendClient, state: &ConsoleState, snapshot: Option<&Value>) {
        self.reset_for_target(state.transcript_target());
        if !self.should_fetch(state, snapshot, Instant::now()) {
            return;
        }
        let target = self.target.as_ref().expect("transcript target").clone();
        self.requested_revision = snapshot
            .and_then(|snapshot| worker_for_target(snapshot, &target))
            .and_then(|worker| worker["transcriptRevision"].as_u64())
            .unwrap_or_default();
        self.task = Some(tokio::spawn(async move {
            backend.transcript(&target.thread_id, &target.turn_id).await
        }));
    }

    fn finish(
        &mut self,
        result: std::result::Result<Result<Value>, tokio::task::JoinError>,
        state: &mut ConsoleState,
        snapshot: Option<&Value>,
    ) {
        self.task = None;
        if self.target.as_ref() != state.transcript_target() {
            return;
        }
        match result {
            Ok(Ok(value)) => {
                state.transcript = Some(value);
                state.transcript_error = None;
                state.transcript_revision = self.requested_revision;
                self.last_success = Some(Instant::now());
                self.next_attempt = None;
                self.failures = 0;
                self.refresh_required = false;
                if let Some(snapshot) = snapshot {
                    let _ = sync_transcript_from_snapshot(state, snapshot);
                }
            }
            error => {
                state.transcript_error = Some(match error {
                    Ok(Err(error)) => error.to_string(),
                    Err(error) => format!("transcript task failed: {error}"),
                    Ok(Ok(_)) => unreachable!(),
                });
                self.failures = self.failures.saturating_add(1);
                let delay_ms = (1_000_u64 << self.failures.saturating_sub(1).min(4))
                    .min(TRANSCRIPT_RETRY_MAX_MS);
                self.next_attempt = Some(Instant::now() + Duration::from_millis(delay_ms));
            }
        }
    }
}

fn worker_for_target<'a>(snapshot: &'a Value, target: &TranscriptTarget) -> Option<&'a Value> {
    workers(snapshot)?
        .iter()
        .find(|worker| same_worker(worker, target))
}

fn sync_transcript_from_snapshot(state: &mut ConsoleState, snapshot: &Value) -> bool {
    let Some(target) = state.transcript_target().cloned() else {
        return false;
    };
    let Some(worker) = worker_for_target(snapshot, &target) else {
        return false;
    };
    let transcript_revision = worker["transcriptRevision"].as_u64().unwrap_or_default();
    let transcript_changed = transcript_revision > state.transcript_revision;

    let pending_actions = snapshot["projection"]["pendingActions"]
        .as_array()
        .map(|actions| {
            actions
                .iter()
                .filter(|action| {
                    action["threadId"].as_str() == Some(target.thread_id.as_str())
                        && action["turnId"]
                            .as_str()
                            .is_none_or(|turn_id| turn_id == target.turn_id)
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if let Some(transcript) = state.transcript.as_mut() {
        transcript["status"] = worker["status"].clone();
        transcript["pendingActions"] = Value::Array(pending_actions);
        transcript["activity"] = serde_json::json!({
            "kind": worker["activityKind"],
            "summary": worker["activitySummary"],
            "lastActivityAtMs": worker["lastActivityAtMs"],
            "tokenUsage": worker["tokenUsage"],
        });
    }
    transcript_changed
}

fn workers(snapshot: &Value) -> Option<&Vec<Value>> {
    snapshot["projection"]["workers"].as_array()
}

fn visible_workers(snapshot: Option<&Value>, active_only: bool) -> Vec<&Value> {
    snapshot
        .and_then(workers)
        .into_iter()
        .flatten()
        .filter(|worker| !active_only || !is_terminal_status(worker["status"].as_str()))
        .collect()
}

fn transcript_target_for_turn(turn: &Value) -> Option<TranscriptTarget> {
    Some(TranscriptTarget {
        thread_id: turn["threadId"].as_str()?.to_owned(),
        turn_id: turn["turnId"].as_str()?.to_owned(),
    })
}

fn same_worker(turn: &Value, target: &TranscriptTarget) -> bool {
    turn["threadId"].as_str() == Some(target.thread_id.as_str())
        && turn["turnId"].as_str() == Some(target.turn_id.as_str())
}

fn sync_worker_selection(state: &mut ConsoleState, workers: &[&Value]) {
    if workers.is_empty() {
        state.selected_worker = 0;
        state.selected_worker_target = None;
    } else if let Some(target) = state.selected_worker_target.as_ref() {
        if let Some(index) = workers
            .iter()
            .position(|worker| same_worker(worker, target))
        {
            state.selected_worker = index;
        } else {
            state.selected_worker = state.selected_worker.min(workers.len() - 1);
            state.selected_worker_target = workers
                .get(state.selected_worker)
                .and_then(|turn| transcript_target_for_turn(turn));
        }
    } else {
        state.selected_worker = state.selected_worker.min(workers.len() - 1);
        state.selected_worker_target = workers
            .get(state.selected_worker)
            .and_then(|turn| transcript_target_for_turn(turn));
    }
}

fn render_frame(
    snapshot: Option<&Value>,
    last_error: Option<&str>,
    state: &mut ConsoleState,
    fetch: &TranscriptFetch,
    frame: usize,
    width: usize,
    height: usize,
) -> Vec<String> {
    sync_worker_selection(state, &visible_workers(snapshot, state.active_only));
    if width < 44 || height < 14 {
        state.page_size = 1;
        return render_tiny(snapshot, last_error, state, fetch, width, height);
    }
    let read_error = match (last_error, state.transcript_error.as_deref()) {
        (Some(backend), Some(transcript)) => {
            Some(format!("backend: {backend} · transcript: {transcript}"))
        }
        (Some(backend), None) => Some(format!("backend: {backend}")),
        (None, Some(transcript)) => Some(format!("transcript: {transcript}")),
        (None, None) => None,
    };
    let connection = match (snapshot.is_some(), last_error.is_none()) {
        (true, true) => "live",
        (true, false) => "stale · reconnecting",
        _ => "connecting",
    };
    let mut lines = vec![
        styled(
            row_lr(
                "  codex connect",
                &format!("observer · {connection} · read-only "),
                width,
            ),
            BOLD,
        ),
        styled(rule(width), DIM),
    ];
    let body_height = height.saturating_sub(4 + if read_error.is_some() { 2 } else { 0 });
    let body_start = lines.len();
    if state.help {
        render_help(&mut lines, width);
    } else {
        match &state.view {
            View::Dashboard => match snapshot {
                Some(snapshot) => {
                    render_snapshot(&mut lines, state, snapshot, width, body_height, frame)
                }
                None => {
                    lines.push(styled(
                        row(" Connecting to the local backend…", width),
                        TEXT,
                    ));
                    lines.push(styled(
                        row(" Navigation stays available while reconnecting.", width),
                        DIM,
                    ));
                }
            },
            View::Transcript(target) => {
                let target = target.clone();
                render_transcript(
                    &mut lines,
                    state,
                    snapshot,
                    fetch,
                    &target,
                    RenderArea {
                        width,
                        height: body_height,
                        frame,
                    },
                );
            }
        }
    }
    lines.truncate(body_start + body_height);
    while lines.len() < body_start + body_height {
        lines.push(row("", width));
    }
    if let Some(error) = read_error {
        lines.push(styled(rule(width), RED));
        lines.push(styled(
            row(
                &format!(" Read error · {}", one_line_terminal_text(&error)),
                width,
            ),
            RED,
        ));
    }
    lines.push(styled(rule(width), DIM));
    let footer = if state.help {
        " ? / Esc close help · Ctrl-C exit".to_string()
    } else {
        match state.view {
            View::Dashboard if width >= 90 => {
                " ↑/↓ select · Enter open · a active/all · PgUp/PgDn page · ? help · q exit"
                    .to_string()
            }
            View::Dashboard => " ↑/↓ select · Enter open · ? help · q exit".to_string(),
            View::Transcript(_) => format!(
                " {} · ↑/↓ scroll · {}Esc back · ? help",
                if state.follow {
                    "Following"
                } else {
                    "Paused · G follow"
                },
                if width >= 90 {
                    "PgUp/PgDn page · "
                } else {
                    ""
                }
            ),
        }
    };
    lines.push(styled(row(&footer, width), DIM));
    lines
}

fn render_help(lines: &mut Vec<String>, width: usize) {
    for label in [
        " shortcuts",
        "",
        " workers     ↑/↓ or j/k select · Enter open",
        "             a active/all · Home/End or g/G first/last",
        "             PgUp/PgDn move one page",
        " transcript  ↑/↓ or j/k scroll · PgUp/PgDn page",
        "             Home/g start · End/G follow latest",
        "             Esc/←/b/q back to workers",
        " general     ? help · q exit from workers · Ctrl-C exit",
        "",
        " typography  terminal controls the typeface · Cascadia Mono / JetBrains Mono work well",
        " read-only   resolve actions through ChatGPT",
    ] {
        lines.push(styled(
            row(label, width),
            if label == " shortcuts" { BOLD } else { TEXT },
        ));
    }
}

fn render_tiny(
    snapshot: Option<&Value>,
    last_error: Option<&str>,
    state: &ConsoleState,
    fetch: &TranscriptFetch,
    width: usize,
    height: usize,
) -> Vec<String> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    if state.help {
        return [
            "Keyboard shortcuts",
            "↑/↓ or j/k select · Enter open",
            "a active/all · g/G first/last",
            "PgUp/PgDn page",
            "Transcript: g start · G follow",
            "Esc back · q exit from workers",
            "? / Esc close help · Ctrl-C exit",
        ]
        .into_iter()
        .take(height)
        .map(|line| elide(line, width))
        .collect();
    }
    let mut labels = vec!["codex connect · read-only".to_string()];
    if let Some(error) = last_error {
        labels.push(format!("Backend: {}", one_line_terminal_text(error)));
    }
    match state.transcript_target() {
        Some(target) => {
            if let Some(error) = state.transcript_error.as_deref() {
                labels.push(format!(
                    "Transcript stale: {}",
                    one_line_terminal_text(error)
                ));
            }
            if let Some(snapshot) = snapshot {
                let pending = pending_for_target(snapshot, target);
                if !pending.is_empty() {
                    labels.push(format!("{} action(s) need operator", pending.len()));
                }
            }
            let status = snapshot
                .and_then(|snapshot| worker_for_target(snapshot, target))
                .and_then(|worker| worker["status"].as_str())
                .unwrap_or("unknown");
            labels.push(format!(
                "{status} · {}",
                transcript_fetch_label(state, fetch)
            ));
            labels.push(format!(
                "Worker {} · {}",
                short_id(&target.thread_id),
                short_id(&target.turn_id)
            ));
            if let Some(last) = state.transcript.as_ref().and_then(latest_agent_text) {
                labels.push(format!(
                    "Agent: {}",
                    bounded_one_line_terminal_text(last, MAX_ACTION_ROW_CHARS)
                ));
            }
        }
        None => {
            let projection = snapshot.map(|snapshot| &snapshot["projection"]);
            let workers = projection
                .and_then(|value| value["workers"].as_array())
                .map_or(0, Vec::len);
            let pending = projection
                .and_then(|value| value["pendingActions"].as_array())
                .map_or(0, Vec::len);
            if let Some(error) = projection.and_then(|value| value["usageError"].as_str()) {
                labels.push(format!("Usage stale: {}", one_line_terminal_text(error)));
            }
            labels.push(format!(
                "{workers} workers · {pending} actions{} ",
                if state.active_only {
                    " · active filter"
                } else {
                    ""
                }
            ));
            if let Some(worker) =
                visible_workers(snapshot, state.active_only).get(state.selected_worker)
            {
                labels.push(format!(
                    "Selected {} · {}",
                    short_id(text(&worker["threadId"], "?")),
                    text(&worker["status"], "unknown")
                ));
            }
        }
    }
    labels.push("↑/↓ select · Enter open · Esc back · ? help".to_string());
    labels.push("Ctrl-C exit".to_string());
    labels
        .into_iter()
        .take(height)
        .map(|line| elide(&one_line_terminal_text(&line), width))
        .collect()
}

fn render_snapshot(
    lines: &mut Vec<String>,
    state: &mut ConsoleState,
    snapshot: &Value,
    width: usize,
    height: usize,
    frame: usize,
) {
    let start = lines.len();
    let runtime = &snapshot["runtime"];
    let projection = &snapshot["projection"];
    let ready = runtime["ready"].as_bool().unwrap_or(false);
    lines.push(styled(
        row_lr(
            &format!(
                " {}  {}",
                if ready { "●" } else { "○" },
                text(&projection["cwd"], "unknown workspace")
            ),
            &format!("build {} ", text(&runtime["buildId"], "unknown")),
            width,
        ),
        if ready { DIM } else { RED },
    ));
    if width < 100 && height >= 12 && projection["usageError"].as_str().is_none() {
        let limits = &projection["usage"]["rateLimits"];
        lines.push(styled(
            row(
                &format!(
                    " Quota · {}",
                    quota_summary("Primary", &limits["primary"], false)
                ),
                width,
            ),
            TEXT,
        ));
        lines.push(styled(
            row(
                &format!(
                    "         {} · usage {}",
                    quota_summary("Secondary", &limits["secondary"], false),
                    usage_access(projection)
                ),
                width,
            ),
            TEXT,
        ));
    } else {
        lines.push(styled(row(&account_line(projection, width), width), TEXT));
    }
    if let Some(notice) = projection["notices"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|notice| notice["kind"].as_str() == Some("error"))
    {
        lines.push(styled(
            row(
                &format!(" Error · {}", text(&notice["summary"], "observer error")),
                width,
            ),
            RED,
        ));
    }
    let pending = projection["pendingActions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !pending.is_empty() {
        lines.push(styled(
            row(
                &format!(
                    " Needs operator · {} pending · resolve in ChatGPT",
                    pending.len()
                ),
                width,
            ),
            YELLOW,
        ));
        // Preserve room for a worker even when several actions and errors arrive together.
        if height.saturating_sub(lines.len() - start) >= 6 {
            lines.push(compact_action_row(&pending[0], width));
        }
    }
    let turns = visible_workers(Some(snapshot), state.active_only);
    let active = workers(snapshot)
        .into_iter()
        .flatten()
        .filter(|worker| !is_terminal_status(worker["status"].as_str()))
        .count();
    let total = workers(snapshot).map_or(0, Vec::len);
    if height >= 12 {
        lines.push(row("", width));
    }
    lines.push(styled(
        row_lr(
            &format!(" workers  {active} active · {} recent", total - active),
            &format!(
                "{} ",
                if state.active_only {
                    "active only"
                } else {
                    "all"
                }
            ),
            width,
        ),
        BOLD,
    ));
    if turns.is_empty() {
        lines.push(styled(
            row(
                if state.active_only {
                    " No active workers. Press a to show recent work."
                } else {
                    " No workers observed yet. Start work through ChatGPT."
                },
                width,
            ),
            DIM,
        ));
        return;
    }
    let available = height.saturating_sub(lines.len() - start);
    let detail = turns
        .get(state.selected_worker)
        .map(|worker| {
            let mut detail = vec![styled(
                row(
                    &format!(
                        " {} / {}",
                        text(&worker["threadId"], "?"),
                        text(&worker["turnId"], "?")
                    ),
                    width,
                ),
                DIM,
            )];
            if let Some(tokens) = worker["tokenUsage"]["threadTotalTokens"].as_u64() {
                detail.push(styled(
                    row(
                        &format!(
                            " Thread total {} tok · cumulative across requests",
                            compact_number(tokens)
                        ),
                        width,
                    ),
                    DIM,
                ));
            }
            if let Some(context) = worker_context_detail(&worker["tokenUsage"]) {
                detail.extend(render_wrapped_text(&context, " ", width, DIM));
            }
            detail
        })
        .unwrap_or_default();
    let detail_height = if available >= detail.len() + 6 {
        detail.len() + 1
    } else {
        0
    };
    let list_height = available.saturating_sub(detail_height);
    let capacity = (list_height.saturating_sub(1) / 2).max(1);
    state.page_size = capacity;
    let first = state
        .selected_worker
        .saturating_add(1)
        .saturating_sub(capacity)
        .min(turns.len().saturating_sub(capacity));
    for (index, worker) in turns.iter().enumerate().skip(first).take(capacity) {
        let waiting = pending
            .iter()
            .any(|action| action_matches_worker(action, worker));
        lines.extend(worker_rows(
            index == state.selected_worker,
            worker,
            waiting,
            width,
            frame,
        ));
    }
    if list_height >= 3 {
        lines.push(styled(
            row(
                &format!(
                    " {}–{} of {} · selected {}",
                    first + 1,
                    (first + capacity).min(turns.len()),
                    turns.len(),
                    state.selected_worker + 1
                ),
                width,
            ),
            DIM,
        ));
    }
    if detail_height > 0 {
        while lines.len() - start < height - detail_height {
            lines.push(row("", width));
        }
        lines.push(styled(rule(width), DIM));
        lines.extend(detail);
    }
}

#[derive(Clone, Copy)]
struct RenderArea {
    width: usize,
    height: usize,
    frame: usize,
}

fn render_transcript(
    lines: &mut Vec<String>,
    state: &mut ConsoleState,
    snapshot: Option<&Value>,
    fetch: &TranscriptFetch,
    target: &TranscriptTarget,
    area: RenderArea,
) {
    let RenderArea {
        width,
        height,
        frame,
    } = area;
    let worker = snapshot.and_then(|snapshot| worker_for_target(snapshot, target));
    let status = worker
        .and_then(|worker| worker["status"].as_str())
        .or_else(|| {
            state
                .transcript
                .as_ref()
                .and_then(|value| value["status"].as_str())
        })
        .unwrap_or("unknown");
    let context = state
        .transcript
        .as_ref()
        .and_then(|value| value.get("context"))
        .filter(|value| !value.is_null());
    let mode = worker
        .and_then(|worker| worker["mode"].as_str())
        .or_else(|| context.and_then(|value| value["mode"].as_str()))
        .unwrap_or("worker");
    let model = worker
        .and_then(|worker| worker["model"].as_str())
        .or_else(|| context.and_then(|value| value["model"].as_str()))
        .unwrap_or("default/inherited");
    let effort = worker
        .and_then(|worker| worker["effort"].as_str())
        .or_else(|| context.and_then(|value| value["effort"].as_str()))
        .unwrap_or("default/inherited");
    let mut pinned = vec![styled(
        row_lr(
            &format!(" {mode} · {status}"),
            &format!("{model} · {effort} "),
            width,
        ),
        &format!("{BOLD}{TEXT}"),
    )];
    pinned.push(styled(
        row(
            &format!(" transcript · {}", transcript_fetch_label(state, fetch)),
            width,
        ),
        if state.transcript_error.is_some() {
            YELLOW
        } else {
            DIM
        },
    ));
    if status == "failed" {
        pinned.push(styled(
            row(" ✕ TURN FAILED", width),
            &format!("{BOLD}{RED}"),
        ));
    }
    if let Some(snapshot) = snapshot {
        let pending = pending_for_target(snapshot, target);
        if !pending.is_empty() {
            pinned.push(styled(
                row(&format!(" ⚠ {} NEEDS OPERATOR", pending.len()), width),
                &format!("{BOLD}{YELLOW}"),
            ));
            for action in pending.iter().take(2) {
                pinned.push(compact_action_row(action, width));
            }
        }
        if let Some(notice) = snapshot["projection"]["notices"]
            .as_array()
            .and_then(|notices| {
                notices
                    .iter()
                    .rev()
                    .find(|notice| notice["kind"].as_str() == Some("error"))
            })
        {
            let summary =
                one_line_terminal_text(notice["summary"].as_str().unwrap_or("observer error"));
            pinned.push(styled(row(&format!(" ✕ {summary}"), width), RED));
        }
    }
    let handles = format!(" thread {} · turn {}", target.thread_id, target.turn_id);
    if display_width(&handles) <= width.saturating_sub(2) {
        pinned.push(styled(row(&handles, width), DIM));
    } else {
        pinned.extend(render_wrapped_text(
            &format!("thread {}", target.thread_id),
            " ",
            width,
            DIM,
        ));
        pinned.extend(render_wrapped_text(
            &format!("turn   {}", target.turn_id),
            " ",
            width,
            DIM,
        ));
    }
    let conversation_height = height.saturating_sub(pinned.len());
    lines.extend(pinned.into_iter().take(height));
    if conversation_height == 0 {
        return;
    }
    let mut body = Vec::new();
    match state.transcript.as_ref() {
        Some(transcript) => {
            let prompt = context.and_then(|value| value["prompt"].as_str());
            if let Some(prompt) = prompt {
                body.push(styled(row(" task", width), BOLD));
                body.extend(render_wrapped_text(prompt, "   ", width, TEXT));
            }
            let pending = transcript["pendingActions"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if !pending.is_empty() {
                body.push(styled(
                    row(" NEEDS OPERATOR", width),
                    &format!("{BOLD}{YELLOW}"),
                ));
                for action in pending {
                    body.extend(render_action_card(action, width));
                }
            }

            if transcript["truncated"].as_bool().unwrap_or(false) {
                body.push(styled(
                    row(" ⚠ older transcript history omitted; newest human-visible output preserved", width),
                    YELLOW,
                ));
            }
            body.push(styled(row(" conversation", width), BOLD));
            body.extend(render_transcript_entries(transcript, width));
            body.extend(render_live_activity(transcript, width, frame));
        }
        None => body.push(styled(row(" acquiring live transcript…", width), YELLOW)),
    }

    let available = conversation_height;
    state.page_size = available.saturating_sub(1).max(1);
    let max_scroll = body.len().saturating_sub(available);
    if state.follow {
        state.scroll = max_scroll;
    } else {
        state.scroll = state.scroll.min(max_scroll);
    }
    lines.extend(body.into_iter().skip(state.scroll).take(available));
}

fn pending_for_target<'a>(snapshot: &'a Value, target: &TranscriptTarget) -> Vec<&'a Value> {
    snapshot["projection"]["pendingActions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|action| {
            action["threadId"].as_str() == Some(target.thread_id.as_str())
                && action["turnId"]
                    .as_str()
                    .is_none_or(|id| id == target.turn_id)
        })
        .collect()
}

fn transcript_fetch_label(state: &ConsoleState, fetch: &TranscriptFetch) -> String {
    if let Some(error) = state.transcript_error.as_deref() {
        let retry = if fetch.task.is_some() {
            "retrying".to_string()
        } else {
            let seconds = fetch
                .next_attempt
                .map(|at| {
                    at.saturating_duration_since(Instant::now())
                        .as_secs()
                        .saturating_add(1)
                })
                .unwrap_or(0);
            format!("retry in {seconds}s")
        };
        return format!(
            "{} · {retry} · {}",
            if state.transcript.is_some() {
                "STALE"
            } else {
                "UNAVAILABLE"
            },
            one_line_terminal_text(error)
        );
    }
    if fetch.task.is_some() {
        return if state.transcript.is_some() {
            "refreshing · showing last read"
        } else {
            "loading"
        }
        .to_string();
    }
    match fetch.last_success {
        Some(at) => format!("last read {}s ago", at.elapsed().as_secs()),
        None => "waiting for first read".to_string(),
    }
}

fn render_transcript_entries(transcript: &Value, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(entries) = transcript["entries"].as_array() else {
        return lines;
    };
    for entry in transcript_entries_for_display(entries) {
        let kind = entry["kind"].as_str().unwrap_or("entry");
        if !matches!(kind, "user" | "agent") {
            continue;
        }
        let Some(text) = entry["text"].as_str() else {
            continue;
        };
        if entry["initialTask"].as_bool().unwrap_or(false) {
            continue;
        }
        let label = if kind.eq_ignore_ascii_case("agent") {
            match entry["title"].as_str() {
                Some(title) if title.contains("FINAL") => "agent · final",
                Some(title) if title.contains("REVIEW") => "agent · review",
                _ => "agent",
            }
        } else {
            "operator"
        };
        lines.push(styled(
            row(&format!(" {label}"), width),
            entry_style(kind, None),
        ));
        lines.extend(render_wrapped_text(
            text,
            "   ",
            width,
            entry_style(kind, None),
        ));
    }
    lines
}

fn transcript_entries_for_display(entries: &[Value]) -> Vec<&Value> {
    let mut visible = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(previous) = visible.last().copied() else {
            visible.push(entry);
            continue;
        };
        let same_agent_text = previous["kind"].as_str() == Some("agent")
            && entry["kind"].as_str() == Some("agent")
            && previous["text"]
                .as_str()
                .is_some_and(|text| !text.is_empty() && entry["text"].as_str() == Some(text));
        if !same_agent_text {
            visible.push(entry);
            continue;
        }

        let previous_rank = transcript_handoff_rank(previous);
        let current_rank = transcript_handoff_rank(entry);
        if previous_rank.max(current_rank) < 2 {
            // Repeated ordinary agent messages are real transcript history. Only collapse
            // App Server handoff aliases such as exitedReviewMode + agentMessage.
            visible.push(entry);
        } else if current_rank > previous_rank {
            *visible.last_mut().expect("previous transcript entry") = entry;
        }
    }
    visible
}

fn transcript_handoff_rank(entry: &Value) -> u8 {
    match entry["title"].as_str() {
        Some(title) if title.contains("FINAL") => 3,
        Some(title) if title.contains("REVIEW") => 2,
        _ => 1,
    }
}

fn render_live_activity(transcript: &Value, width: usize, frame: usize) -> Vec<String> {
    if is_terminal_status(transcript["status"].as_str()) {
        return Vec::new();
    }
    let Some(activity) = transcript.get("activity").filter(|value| !value.is_null()) else {
        return Vec::new();
    };
    let kind = activity["kind"].as_str().unwrap_or("working");
    let age = activity["lastActivityAtMs"]
        .as_u64()
        .map(activity_age)
        .unwrap_or_else(|| "now".to_string());
    let pulse = ["◐", "◓", "◑", "◒"][frame % 4];
    let usage = activity["tokenUsage"]["threadTotalTokens"]
        .as_u64()
        .map(|tokens| format!(" · thread total {} tok", compact_number(tokens)))
        .unwrap_or_default();
    if matches!(kind.to_ascii_lowercase().as_str(), "reasoning" | "think") {
        return vec![styled(
            row(&format!(" {pulse} THINKING · {age}{usage}"), width),
            CYAN,
        )];
    }
    if kind.eq_ignore_ascii_case("waiting") {
        return vec![styled(
            row(&format!(" ⚠ WAITING FOR OPERATOR · {age}"), width),
            YELLOW,
        )];
    }
    if kind.eq_ignore_ascii_case("message") {
        let Some(summary) = activity["summary"].as_str().filter(|text| !text.is_empty()) else {
            return vec![styled(
                row(&format!(" {pulse} RESPONDING · {age}{usage}"), width),
                TEXT,
            )];
        };
        if latest_agent_text(transcript).is_some_and(|text| text == summary) {
            return Vec::new();
        }
        let mut lines = vec![styled(
            row(
                &format!(" {pulse} AGENT · responding · {age}{usage}"),
                width,
            ),
            TEXT,
        )];
        lines.extend(render_wrapped_text(summary, "   ", width, TEXT));
        return lines;
    }
    if kind.eq_ignore_ascii_case("error") {
        let summary = activity["summary"].as_str().unwrap_or("worker error");
        let mut lines = vec![styled(row(" ✕ WORKER ERROR", width), RED)];
        lines.extend(render_wrapped_text(summary, "   ", width, RED));
        return lines;
    }
    vec![styled(
        row(&format!(" {pulse} WORKING · {age}{usage}"), width),
        DIM,
    )]
}

fn entry_style(kind: &str, status: Option<&str>) -> &'static str {
    if status.is_some_and(|status| {
        status.eq_ignore_ascii_case("failed") || status.eq_ignore_ascii_case("error")
    }) {
        RED
    } else if kind.eq_ignore_ascii_case("user") {
        CYAN
    } else if kind.eq_ignore_ascii_case("agent") || kind.eq_ignore_ascii_case("assistant") {
        TEXT
    } else {
        DIM
    }
}

fn render_wrapped_text(text: &str, prefix: &str, width: usize, style: &'static str) -> Vec<String> {
    let inner = width.saturating_sub(2);
    let prefix = truncate_display_width(prefix, inner);
    let line_width = inner.saturating_sub(display_width(&prefix)).max(1);
    wrap_terminal_text(text, line_width)
        .into_iter()
        .map(|line| styled(row(&format!("{prefix}{line}"), width), style))
        .collect()
}

fn wrap_terminal_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let safe = safe_terminal_text(text);
    let mut lines = Vec::new();
    for paragraph in safe.split('\n') {
        let mut rest = paragraph;
        while !rest.is_empty() {
            let prefix = truncate_display_width(rest, width);
            if prefix.len() == rest.len() {
                break;
            }
            if prefix.is_empty() {
                // A wide grapheme cannot fit in a one-column viewport.
                let first = rest.graphemes(true).next().expect("nonempty paragraph");
                lines.push("�".to_string());
                rest = &rest[first.len()..];
                continue;
            }
            let suffix = &rest[prefix.len()..];
            if suffix.starts_with(char::is_whitespace) {
                lines.push(prefix.trim_end().to_string());
                rest = suffix.trim_start();
                continue;
            }
            let split = prefix
                .char_indices()
                .rev()
                .find(|(index, ch)| ch.is_whitespace() && !prefix[..*index].trim().is_empty())
                .map(|(index, _)| index);
            match split {
                Some(index) => {
                    lines.push(rest[..index].trim_end().to_string());
                    rest = rest[index..].trim_start();
                }
                None => {
                    let consumed = prefix.len();
                    lines.push(prefix);
                    rest = &rest[consumed..];
                }
            }
        }
        if !rest.is_empty() || paragraph.is_empty() {
            lines.push(rest.to_string());
        }
    }
    lines
}

fn safe_terminal_text(text: &str) -> String {
    text.chars()
        .flat_map(|character| match character {
            '\n' => "\n".chars().collect::<Vec<_>>(),
            '\t' => "    ".chars().collect(),
            '\x1b' => "^[".chars().collect(),
            character if matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') => {
                format!("\\u{{{:04x}}}", character as u32).chars().collect()
            }
            character if character.is_control() => {
                format!("\\x{:02x}", character as u32).chars().collect()
            }
            character => vec![character],
        })
        .collect()
}

fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

fn worker_rows(
    selected: bool,
    worker: &Value,
    waiting: bool,
    width: usize,
    frame: usize,
) -> Vec<String> {
    let mode = text(&worker["mode"], "worker");
    let model = text(&worker["model"], "default");
    let effort = text(&worker["effort"], "default");
    let status = text(&worker["status"], "unknown");
    let kind = text(&worker["activityKind"], "working");
    let pulse = ["◐", "◓", "◑", "◒"][frame % 4];
    let (glyph, label) = if waiting && !is_terminal_status(Some(status)) {
        ("!", "needs operator")
    } else {
        match status {
            "completed" => ("✓", "complete"),
            "failed" => ("×", "failed"),
            "interrupted" => ("■", "interrupted"),
            "inProgress" if matches!(kind, "reasoning" | "think") => (pulse, "thinking"),
            "inProgress" if kind == "message" => (pulse, "responding"),
            "inProgress" => (pulse, "working"),
            _ => ("?", "unknown"),
        }
    };
    let age = worker["lastActivityAtMs"]
        .as_u64()
        .map(activity_age)
        .unwrap_or_else(|| "age unknown".into());
    let prompt = worker["prompt"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(|value| bounded_one_line_terminal_text(value, MAX_ACTION_TEXT_CHARS))
        .unwrap_or_else(|| format!("{mode} · {}", text(&worker["threadId"], "unknown worker")));
    let title = format!(" {} {}", if selected { "▸" } else { " " }, prompt);
    let style = if status == "failed" {
        RED
    } else if waiting {
        YELLOW
    } else if selected {
        BOLD
    } else {
        TEXT
    };
    vec![
        styled(row(&title, width), style),
        styled(
            row_lr(
                &format!("   {glyph} {label:<14} {model} · {effort} · {mode}"),
                &format!("{age} "),
                width,
            ),
            if status == "failed" {
                RED
            } else if waiting {
                YELLOW
            } else {
                DIM
            },
        ),
    ]
}

fn worker_context_detail(usage: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let (Some(input), Some(window)) = (
        usage["lastRequestInputTokens"].as_u64(),
        usage["lastRequestModelContextWindow"].as_u64(),
    ) {
        parts.push(format!(
            "latest request input {} / {} tok window",
            compact_number(input),
            compact_number(window)
        ));
    }
    if let Some(cached) = usage["lastRequestCachedInputTokens"].as_u64() {
        let percent = usage["cacheHitPercent"]
            .as_u64()
            .map(|percent| format!(" · {percent}%"))
            .unwrap_or_default();
        parts.push(format!(
            "latest request cached {} tok{percent}",
            compact_number(cached)
        ));
    }
    if let Some(until) = usage["cacheGuaranteedUntilMs"].as_u64() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let remaining = until.saturating_sub(now) / 1_000;
        let active = until > now && usage["cacheGuaranteeActive"].as_bool().unwrap_or(true);
        parts.push(if !active {
            "thread cache guarantee expired".to_string()
        } else if remaining < 60 {
            "thread cache guarantee <1m left".to_string()
        } else {
            format!("thread cache guarantee {}m left", remaining / 60)
        });
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn one_line_terminal_text(text: &str) -> String {
    safe_terminal_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn bounded_terminal_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let clipped = chars.by_ref().take(max_chars).collect::<String>();
    let truncated = chars.next().is_some();
    let mut safe = safe_terminal_text(&clipped);
    if truncated {
        safe.push('…');
    }
    safe
}

fn bounded_one_line_terminal_text(text: &str, max_chars: usize) -> String {
    bounded_terminal_text(text, max_chars)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_terminal_status(status: Option<&str>) -> bool {
    matches!(status, Some("completed" | "failed" | "interrupted"))
}

fn compact_number(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn action_matches_worker(action: &Value, worker: &Value) -> bool {
    action["threadId"].as_str() == worker["threadId"].as_str()
        && action["turnId"]
            .as_str()
            .is_none_or(|turn_id| worker["turnId"].as_str() == Some(turn_id))
}

fn action_label(action: &Value) -> &'static str {
    match action["method"].as_str().unwrap_or_default() {
        "item/commandExecution/requestApproval" => "COMMAND APPROVAL",
        "item/fileChange/requestApproval" => "FILE APPROVAL",
        "item/permissions/requestApproval" => "PERMISSION REQUEST",
        "item/tool/requestUserInput" => "QUESTION",
        "mcpServer/elicitation/request" => "ELICITATION",
        _ => "OPERATOR ACTION",
    }
}

fn action_summary(action: &Value) -> String {
    let params = &action["params"];
    if let Some(command) = params["command"].as_str().filter(|value| !value.is_empty()) {
        return command.to_string();
    }
    if let Some(reason) = params["reason"].as_str().filter(|value| !value.is_empty()) {
        return reason.to_string();
    }
    if let Some(question) = params["questions"]
        .as_array()
        .and_then(|questions| questions.first())
        .and_then(|question| question["question"].as_str())
    {
        return question.to_string();
    }
    if let Some(message) = params["message"].as_str().filter(|value| !value.is_empty()) {
        return message.to_string();
    }
    if let Some(root) = params["grantRoot"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        return format!("write access under {root}");
    }
    if !params["permissions"].is_null() {
        return serde_json::to_string(&params["permissions"])
            .unwrap_or_else(|_| "additional permissions requested".into());
    }
    "operator response required".into()
}

fn compact_action_row(action: &Value, width: usize) -> String {
    let blocking = if action["isBlocking"].as_bool().unwrap_or(true) {
        "blocking"
    } else {
        "nonblocking"
    };
    styled(
        row(
            &format!(
                " ⚠ {} · {} · {}",
                action_label(action),
                blocking,
                bounded_one_line_terminal_text(&action_summary(action), MAX_ACTION_ROW_CHARS)
            ),
            width,
        ),
        &format!("{BOLD}{YELLOW}"),
    )
}

fn render_action_card(action: &Value, width: usize) -> Vec<String> {
    let blocking = if action["isBlocking"].as_bool().unwrap_or(true) {
        "blocking"
    } else {
        "nonblocking"
    };
    let mut lines = vec![styled(
        row(&format!(" ⚠ {} · {blocking}", action_label(action)), width),
        &format!("{BOLD}{YELLOW}"),
    )];
    let params = &action["params"];
    if action["method"].as_str() == Some("item/tool/requestUserInput") {
        if let Some(questions) = params["questions"].as_array() {
            for question in questions.iter().take(4) {
                if let Some(text) = question["question"].as_str() {
                    lines.extend(render_wrapped_text(
                        &bounded_terminal_text(text, MAX_ACTION_TEXT_CHARS),
                        "   ",
                        width,
                        YELLOW,
                    ));
                }
                if let Some(options) = question["options"].as_array() {
                    let labels = options
                        .iter()
                        .take(8)
                        .filter_map(|option| option["label"].as_str())
                        .map(|label| bounded_one_line_terminal_text(label, 128))
                        .collect::<Vec<_>>()
                        .join(" / ");
                    if !labels.is_empty() {
                        lines.extend(render_wrapped_text(
                            &format!("options: {labels}"),
                            "     ",
                            width,
                            DIM,
                        ));
                    }
                }
            }
        }
    } else {
        lines.extend(render_wrapped_text(
            &bounded_terminal_text(&action_summary(action), MAX_ACTION_TEXT_CHARS),
            "   ",
            width,
            YELLOW,
        ));
        if let Some(reason) = params["reason"]
            .as_str()
            .filter(|reason| !reason.is_empty() && Some(*reason) != params["command"].as_str())
        {
            lines.extend(render_wrapped_text(
                &bounded_terminal_text(reason, MAX_ACTION_TEXT_CHARS),
                "   ",
                width,
                DIM,
            ));
        }
        if let Some(decisions) = params["availableDecisions"].as_array() {
            let decisions = decisions
                .iter()
                .filter_map(|decision| {
                    decision.as_str().map(str::to_string).or_else(|| {
                        decision
                            .as_object()
                            .and_then(|object| object.keys().next().cloned())
                    })
                })
                .map(|decision| {
                    codex_connect_relay::operator_approval_decision(&decision)
                        .unwrap_or(&decision)
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(" / ");
            if !decisions.is_empty() {
                lines.extend(render_wrapped_text(
                    &bounded_terminal_text(&format!("choices: {decisions}"), MAX_ACTION_TEXT_CHARS),
                    "   ",
                    width,
                    DIM,
                ));
            }
        }
    }
    lines.push(styled(
        row("   resolve through ChatGPT operator", width),
        DIM,
    ));
    lines
}

fn latest_agent_text(transcript: &Value) -> Option<&str> {
    transcript["entries"]
        .as_array()?
        .iter()
        .rev()
        .find(|entry| entry["kind"].as_str() == Some("agent"))?
        .get("text")?
        .as_str()
}

fn activity_age(last_activity_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now_ms.saturating_sub(last_activity_ms) / 1_000;
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / 3_600)
    }
}

fn account_line(projection: &Value, width: usize) -> String {
    if let Some(error) = projection["usageError"]
        .as_str()
        .filter(|error| !error.is_empty())
    {
        let age = projection["usageUpdatedAtMs"]
            .as_u64()
            .map(activity_age)
            .map(|age| format!(" · last good {age}"))
            .unwrap_or_default();
        return format!(
            " Quota · quota stale{age} · {}",
            one_line_terminal_text(error)
        );
    }
    let limits = &projection["usage"]["rateLimits"];
    let primary = quota_summary("Primary", &limits["primary"], width >= 130);
    let secondary = quota_summary("Secondary", &limits["secondary"], width >= 130);
    format!(
        " Quota · {primary} · {secondary} · usage {}",
        usage_access(projection)
    )
}

fn usage_access(projection: &Value) -> &'static str {
    projection["usage"]["ordinaryUsageAllowed"]
        .as_bool()
        .map(|allowed| if allowed { "open" } else { "blocked" })
        .unwrap_or("unknown")
}

fn quota_summary(fallback_label: &str, value: &Value, meter: bool) -> String {
    let label = quota_label(value).unwrap_or_else(|| fallback_label.to_string());
    let Some(used) = value["usedPercent"].as_f64() else {
        return format!("{label} unavailable");
    };
    let used = used.clamp(0.0, 100.0);
    let reset = value["resetsAt"]
        .as_u64()
        .map(reset_in)
        .unwrap_or_else(|| "reset ?".into());
    if meter {
        let filled = ((used / 100.0) * 8.0).round() as usize;
        format!(
            "{label} {} {:>3.0}% used · resets {reset}",
            "█".repeat(filled) + &"░".repeat(8 - filled),
            used
        )
    } else {
        format!("{label} {:>3.0}% used · resets {reset}", used)
    }
}

fn quota_label(value: &Value) -> Option<String> {
    let minutes = value["windowDurationMins"]
        .as_u64()
        .filter(|minutes| *minutes > 0)?;
    if minutes % (24 * 60) == 0 {
        Some(format!("{}D", minutes / (24 * 60)))
    } else if minutes % 60 == 0 {
        Some(format!("{}H", minutes / 60))
    } else {
        Some(format!("{minutes}M"))
    }
}

fn reset_in(epoch_seconds: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if epoch_seconds <= now {
        return "due".into();
    }
    let remaining = epoch_seconds - now;
    let hours = remaining / 3_600;
    let minutes = (remaining % 3_600) / 60;
    if hours >= 24 {
        format!("{}d", hours / 24)
    } else if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

fn styled(line: String, style: &str) -> String {
    format!("{style}{line}{RESET}")
}

fn text<'a>(value: &'a Value, fallback: &'a str) -> &'a str {
    value.as_str().unwrap_or(fallback)
}

fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

fn terminal_size() -> (usize, usize) {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 && size.ws_row > 0 {
        (size.ws_col as usize, size.ws_row as usize)
    } else {
        (110, 34)
    }
}

fn rule(width: usize) -> String {
    if width < 4 {
        "─".repeat(width)
    } else {
        format!("  {}  ", "─".repeat(width - 4))
    }
}

fn row(content: &str, width: usize) -> String {
    if width < 2 {
        return " ".repeat(width);
    }
    let inner = width - 2;
    let mut body = elide(&safe_terminal_text(content).replace('\n', " "), inner);
    let used = display_width(&body);
    if used < inner {
        body.push_str(&" ".repeat(inner - used));
    }
    format!(" {body} ")
}

fn row_lr(left: &str, right: &str, width: usize) -> String {
    if width < 2 {
        return " ".repeat(width);
    }
    let left = safe_terminal_text(left).replace('\n', " ");
    let right = safe_terminal_text(right).replace('\n', " ");
    let inner = width.saturating_sub(2);
    let right = elide(&right, inner);
    let right_width = display_width(&right);
    if right_width >= inner {
        return row(&right, width);
    }
    let left_max = inner.saturating_sub(right_width + 1);
    let left = elide(&left, left_max);
    let padding = inner.saturating_sub(display_width(&left) + right_width);
    format!(" {left}{}{right} ", " ".repeat(padding))
}

fn elide(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        value.to_string()
    } else if width == 0 {
        String::new()
    } else {
        format!("{}…", truncate_display_width(value, width - 1))
    }
}

fn truncate_display_width(value: &str, width: usize) -> String {
    let mut end = 0;
    for (index, grapheme) in value.grapheme_indices(true) {
        let next = index + grapheme.len();
        if display_width(&value[..next]) > width {
            break;
        }
        end = next;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &str) -> String {
        let mut parts = line.split("\x1b[");
        let mut visible = parts.next().unwrap_or_default().to_string();
        for part in parts {
            if let Some((_, text)) = part.split_once('m') {
                visible.push_str(text);
            }
        }
        visible
    }

    #[test]
    fn selected_worker_survives_crowded_layouts_and_resize() {
        let snapshot = serde_json::json!({
            "runtime":{"ready":true,"buildId":"build"},
            "projection":{
                "cwd":"/work/界面",
                "workers":(0..18).map(|i| serde_json::json!({
                    "threadId":format!("thread-{i}"),"turnId":format!("turn-{i}"),
                    "prompt":format!("task-{i} café 👩‍💻"),"status":"inProgress",
                    "model":"model","effort":"high",
                    "tokenUsage":{"threadTotalTokens":1000,"lastRequestInputTokens":200,
                        "lastRequestModelContextWindow":10000,"lastRequestCachedInputTokens":100}
                })).collect::<Vec<_>>(),
                "pendingActions":[{"params":{"reason":"needs attention"}}],
                "notices":[{"kind":"error","summary":"observer error"}]
            }
        });
        for width in [44, 80, 132] {
            for height in [14, 16, 24, 40] {
                let mut state = ConsoleState {
                    selected_worker: 17,
                    ..ConsoleState::default()
                };
                let lines = render_frame(
                    Some(&snapshot),
                    Some("disconnected"),
                    &mut state,
                    &TranscriptFetch::default(),
                    0,
                    width,
                    height,
                );
                let rendered = lines.join("\n");
                assert_eq!(lines.len(), height);
                assert!(
                    lines
                        .iter()
                        .all(|line| display_width(&plain(line)) == width),
                    "{width}x{height}"
                );
                assert!(
                    rendered.contains("▸ task-17"),
                    "{width}x{height}: {rendered}"
                );
                assert!(rendered.contains("working"), "{width}x{height}");
                assert!(rendered.contains("stale"));
            }
        }
    }

    #[test]
    fn unicode_layout_preserves_graphemes_and_word_boundaries() {
        for text in ["界面", "cafe\u{301}", "👩‍💻", "👍🏽", "🇵🇭"] {
            for width in 0..10 {
                assert!(display_width(&row(text, width)) <= width);
                assert!(display_width(&row_lr(text, text, width)) <= width);
                let clipped = truncate_display_width(text, width);
                assert!(
                    text.grapheme_indices(true).any(|(i, _)| i == clipped.len()) || clipped == text
                );
                assert!(
                    wrap_terminal_text(text, width)
                        .iter()
                        .all(|line| display_width(line) <= width)
                );
            }
        }
        assert_eq!(truncate_display_width("e\u{301}x", 1), "e\u{301}");
        assert_eq!(truncate_display_width("👩‍💻x", 2), "👩‍💻");
        assert_eq!(
            wrap_terminal_text("hello world again", 11),
            ["hello world", "again"]
        );
        assert_eq!(
            wrap_terminal_text("  indented\n\nnext", 20),
            ["  indented", "", "next"]
        );
    }

    #[test]
    fn metadata_cannot_inject_terminal_controls_or_extra_rows() {
        let malicious = "name\x1b]52;c;payload\x07\nline\r\u{202e}";
        for value in [row(malicious, 100), row_lr(malicious, malicious, 100)] {
            assert!(!value.contains(['\x1b', '\n', '\r', '\x07', '\u{202e}']));
            assert_eq!(display_width(&value), 100);
        }
        let snapshot = serde_json::json!({"projection":{"workers":[{
            "threadId":malicious,"turnId":"turn","status":malicious
        }]}});
        let lines = render_frame(
            Some(&snapshot),
            None,
            &mut ConsoleState::default(),
            &TranscriptFetch::default(),
            0,
            43,
            12,
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.contains(['\x1b', '\n', '\r', '\x07', '\u{202e}']))
        );
    }

    #[test]
    fn decoder_handles_paging_and_ignores_paste_and_unknown_sequences() {
        let mut decoder = InputDecoder::default();
        for (bytes, key) in [
            (b"\x1b[5~".as_slice(), InputKey::PageUp),
            (b"\x1b[6~".as_slice(), InputKey::PageDown),
            (b"\x1bOH".as_slice(), InputKey::Home),
            (b"\x1b[F".as_slice(), InputKey::End),
        ] {
            assert_eq!(
                bytes
                    .iter()
                    .flat_map(|b| decoder.push(*b))
                    .collect::<Vec<_>>(),
                [key]
            );
        }
        for sequence in [
            b"\x1b[99~".as_slice(),
            b"\x1bOP",
            b"\x1bq",
            b"\x1b[200~q?aj\nG\x1b[A\x1b[201~",
        ] {
            assert!(
                sequence
                    .iter()
                    .flat_map(|b| decoder.push(*b))
                    .collect::<Vec<_>>()
                    .is_empty()
            );
        }
        assert_eq!(decoder.push(b'j'), [InputKey::Character('j')]);
        for byte in b"\x1b[" {
            assert!(decoder.push(*byte).is_empty());
        }
        assert_eq!(decoder.flush_escape(), None);
    }

    #[test]
    fn active_filter_paging_help_and_tiny_resize_keep_selection_coherent() {
        let snapshot = serde_json::json!({"projection":{"workers":[
            {"threadId":"done","turnId":"done","status":"completed"},
            {"threadId":"one","turnId":"one","status":"inProgress"},
            {"threadId":"two","turnId":"two","status":"inProgress"}
        ]}});
        let mut state = ConsoleState {
            page_size: 2,
            ..ConsoleState::default()
        };
        state.handle_key(InputKey::PageDown, Some(&snapshot));
        assert_eq!(state.selected_worker, 2);
        state.handle_key(InputKey::Character('a'), Some(&snapshot));
        assert_eq!(state.selected_worker, 1);
        assert_eq!(
            state.selected_worker_target.as_ref().unwrap().thread_id,
            "two"
        );
        let reordered = serde_json::json!({"projection":{"workers":[
            {"threadId":"two","turnId":"two","status":"inProgress"},
            {"threadId":"one","turnId":"one","status":"inProgress"}
        ]}});
        render_frame(
            Some(&reordered),
            None,
            &mut state,
            &TranscriptFetch::default(),
            0,
            30,
            10,
        );
        assert_eq!(state.selected_worker, 0);
        state.handle_key(InputKey::Character('?'), Some(&reordered));
        state.handle_key(InputKey::Character('q'), Some(&reordered));
        assert!(!state.quit && !state.help);
        state.handle_key(InputKey::Enter, Some(&reordered));
        assert_eq!(state.transcript_target().unwrap().thread_id, "two");
        state.handle_key(InputKey::PageUp, None);
        assert!(!state.follow);
        state.handle_key(InputKey::End, None);
        assert!(state.follow);
    }

    #[test]
    fn stale_snapshot_does_not_extend_cache_deadline_or_invent_quota_windows() {
        let usage = serde_json::json!({"cacheGuaranteedUntilMs":1,"cacheGuaranteeActive":true});
        assert_eq!(
            worker_context_detail(&usage).unwrap(),
            "thread cache guarantee expired"
        );
        let summary = account_line(&Value::Null, 100);
        assert!(summary.contains("Primary unavailable"));
        assert!(summary.contains("Secondary unavailable"));
        assert!(!summary.contains("5H") && !summary.contains("7D"));
        assert_eq!(
            quota_label(&serde_json::json!({"windowDurationMins":0})),
            None
        );
    }

    #[test]
    fn reconnect_refreshes_open_transcript_once_after_revision_reset() {
        let target = TranscriptTarget {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
        };
        let mut state = ConsoleState {
            view: View::Transcript(target.clone()),
            transcript: Some(serde_json::json!({"entries":[]})),
            transcript_revision: 100,
            ..ConsoleState::default()
        };
        let snapshot = serde_json::json!({"projection":{"workers":[{
            "threadId":"thread","turnId":"turn","transcriptRevision":1
        }]}});
        let mut fetch = TranscriptFetch {
            target: Some(target),
            requested_revision: 1,
            refresh_required: true,
            ..TranscriptFetch::default()
        };
        assert!(fetch.should_fetch(&state, Some(&snapshot), Instant::now()));
        fetch.finish(
            Ok(Ok(serde_json::json!({"entries":[]}))),
            &mut state,
            Some(&snapshot),
        );
        assert_eq!(state.transcript_revision, 1);
        assert!(!fetch.should_fetch(
            &state,
            Some(&snapshot),
            Instant::now() + Duration::from_secs(60)
        ));
    }

    #[test]
    fn row_is_cropped_to_terminal_width() {
        assert_eq!(row(" abcdef", 6).chars().count(), 6);
    }

    #[test]
    fn worker_rows_expose_model_effort_and_prompt() {
        let turn = serde_json::json!({
            "threadId": "thread-123456789",
            "turnId": "turn-123456789",
            "status": "inProgress",
            "mode": "work",
            "model": "gpt-5.6-sol",
            "effort": "high",
            "prompt": "Refactor the console for a human operator"
        });
        let rendered = worker_rows(true, &turn, false, 120, 0).join("\n");
        assert!(rendered.contains("▸ Refactor the console"));
        assert!(rendered.contains("gpt-5.6-sol"));
        assert!(rendered.contains("high"));
        assert!(rendered.contains("Refactor the console"));
    }

    #[test]
    fn worker_rows_show_thinking_and_usage_without_reasoning_text() {
        let turn = serde_json::json!({
            "threadId": "thread-123456789",
            "turnId": "turn-123456789",
            "status": "inProgress",
            "mode": "work",
            "model": "gpt-5.6-sol",
            "effort": "high",
            "lastActivityAtMs": 0,
            "activityKind": "think",
            "activitySummary": "private chain of thought",
            "tokenUsage": {"threadTotalTokens": 123, "lastRequestModelContextWindow": 456}
        });
        let rendered = worker_rows(false, &turn, false, 160, 0).join("\n");
        assert!(rendered.contains("thinking"));
        assert!(!rendered.contains("private chain of thought"));
    }

    #[test]
    fn worker_task_preview_stays_on_one_terminal_row() {
        let turn = serde_json::json!({
            "threadId": "thread-123456789",
            "turnId": "turn-123456789",
            "status": "inProgress",
            "mode": "work",
            "prompt": "first line\nsecond line"
        });
        let rendered = worker_rows(false, &turn, false, 120, 0);
        assert_eq!(rendered.len(), 2);
        assert!(!rendered[0].contains('\n'));
        assert!(rendered[0].contains("first line second line"));
    }

    #[test]
    fn action_cards_surface_the_human_question_and_options() {
        let action = serde_json::json!({
            "method": "item/tool/requestUserInput",
            "kind": "userInput",
            "isBlocking": true,
            "params": {
                "questions": [{
                    "question": "Which output format?",
                    "options": [{"label": "JSON"}, {"label": "Markdown"}]
                }]
            }
        });
        let rendered = render_action_card(&action, 100).join("\n");
        assert!(rendered.contains("QUESTION · blocking"));
        assert!(rendered.contains("Which output format?"));
        assert!(rendered.contains("JSON / Markdown"));
        assert!(rendered.contains("resolve through ChatGPT operator"));

        let approval = serde_json::json!({
            "method": "item/commandExecution/requestApproval",
            "kind": "approval",
            "isBlocking": true,
            "params": {"command":"echo hi","availableDecisions":["accept","acceptForSession","decline"]}
        });
        let rendered = render_action_card(&approval, 100).join("\n");
        assert!(rendered.contains("choices: approve / approveForSession / decline"));
        assert!(!rendered.contains("choices: accept"));
    }

    #[test]
    fn action_projection_bounds_text_and_keeps_dashboard_rows_single_line() {
        let compact = serde_json::json!({
            "method": "item/commandExecution/requestApproval",
            "isBlocking": true,
            "params": {"command": "echo one\necho two"}
        });
        let row = compact_action_row(&compact, 120);
        assert!(!row.contains('\n'));
        assert!(row.contains("echo one echo two"));

        let question = serde_json::json!({
            "method": "item/tool/requestUserInput",
            "isBlocking": true,
            "params": {"questions": [{"question": "Q".repeat(100_000)}]}
        });
        let rendered = render_action_card(&question, 100);
        assert!(rendered.len() < 40);
        assert!(rendered.join("\n").contains('…'));
    }

    #[test]
    fn narrow_rows_never_expand_the_terminal() {
        assert_eq!(row_lr(" a long left side", "right ", 8).chars().count(), 8);
        assert_eq!(row("a long line", 4).chars().count(), 4);
        assert_eq!(display_width(&row("界界", 6)), 6);
    }

    #[test]
    fn account_line_preserves_compact_quota_telemetry() {
        let projection = serde_json::json!({
            "usage": {
                "ordinaryUsageAllowed": true,
                "rateLimits": {
                    "primary": {"usedPercent": 50, "resetsAt": 0, "windowDurationMins": 300},
                    "secondary": {"usedPercent": 25, "resetsAt": 0, "windowDurationMins": 10080}
                }
            }
        });
        let compact = account_line(&projection, 80);
        assert!(compact.contains(
            "Quota · 5H  50% used · resets due · 7D  25% used · resets due · usage open"
        ));
        assert!(!compact.contains('█'));
        assert!(account_line(&projection, 140).contains("████"));
    }

    #[test]
    fn account_error_marks_cached_quota_stale() {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let projection = serde_json::json!({
            "usage": {"rateLimits":{"primary":{"usedPercent":50}}},
            "usageError":"usage request timed out",
            "usageUpdatedAtMs":now_ms.saturating_sub(5_000)
        });
        let line = account_line(&projection, 120);
        assert!(line.contains("quota stale"));
        assert!(line.contains("last good"));
        assert!(line.contains("usage request timed out"));
        assert!(!line.contains("50%"));
    }

    #[test]
    fn selected_worker_uses_projected_cache_and_context_fields() {
        let worker = serde_json::json!({
            "threadId":"thread","turnId":"turn","status":"inProgress",
            "tokenUsage":{
                "lastRequestInputTokens":12000,"lastRequestModelContextWindow":200000,
                "lastRequestCachedInputTokens":9000,"cacheHitPercent":75,
                "cacheGuaranteedUntilMs":u64::MAX
            }
        });
        let rendered = worker_context_detail(&worker["tokenUsage"]).unwrap();
        assert!(rendered.contains("latest request input 12.0k / 200.0k tok window"));
        assert!(rendered.contains("latest request cached 9.0k tok · 75%"));
        assert!(rendered.contains("thread cache guarantee"));
    }

    #[test]
    fn row_lr_preserves_requested_width() {
        assert_eq!(row_lr(" left", "right ", 30).chars().count(), 30);
    }

    #[test]
    fn arrow_and_enter_select_worker_and_back_returns() {
        let snapshot = serde_json::json!({
            "projection": {
                "workers": [
                    {"threadId": "thread-one", "turnId": "turn-one"},
                    {"threadId": "thread-two", "turnId": "turn-two"}
                ]
            }
        });
        let mut state = ConsoleState::default();

        assert!(state.handle_key(InputKey::Down, Some(&snapshot)));
        assert!(state.handle_key(InputKey::Enter, Some(&snapshot)));
        assert_eq!(
            state.view,
            View::Transcript(TranscriptTarget {
                thread_id: "thread-two".into(),
                turn_id: "turn-two".into(),
            })
        );
        assert!(state.handle_key(InputKey::Left, Some(&snapshot)));
        assert_eq!(state.view, View::Dashboard);
    }

    #[test]
    fn worker_selection_tracks_identity_across_reordering() {
        let mut state = ConsoleState {
            selected_worker: 1,
            selected_worker_target: Some(TranscriptTarget {
                thread_id: "thread-two".into(),
                turn_id: "turn-two".into(),
            }),
            ..ConsoleState::default()
        };
        let reordered = [
            serde_json::json!({"threadId":"thread-two","turnId":"turn-two"}),
            serde_json::json!({"threadId":"thread-one","turnId":"turn-one"}),
        ];
        sync_worker_selection(&mut state, &reordered.iter().collect::<Vec<_>>());
        assert_eq!(state.selected_worker, 0);
        assert_eq!(
            state.selected_worker_target,
            Some(TranscriptTarget {
                thread_id: "thread-two".into(),
                turn_id: "turn-two".into(),
            })
        );
    }

    #[test]
    fn transcript_refreshes_only_when_worker_revision_advances() {
        let target = TranscriptTarget {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
        };
        let mut state = ConsoleState {
            view: View::Transcript(target.clone()),
            transcript: Some(serde_json::json!({
                "status":"inProgress",
                "pendingActions":[],
                "activity":null,
                "entries":[]
            })),
            transcript_revision: 3,
            ..ConsoleState::default()
        };
        let snapshot = serde_json::json!({
            "projection": {
                "workers": [{
                    "threadId":"thread",
                    "turnId":"turn",
                    "status":"inProgress",
                    "activityKind":"message",
                    "activitySummary":"hello",
                    "lastActivityAtMs":10,
                    "transcriptRevision":4,
                    "tokenUsage":{"threadTotalTokens":12,"lastRequestModelContextWindow":100}
                }],
                "pendingActions": [{
                    "threadId":"thread",
                    "turnId":"turn",
                    "kind":"userInput"
                }]
            }
        });

        assert!(sync_transcript_from_snapshot(&mut state, &snapshot));
        assert_eq!(
            state.transcript.as_ref().unwrap()["activity"]["summary"],
            "hello"
        );
        assert_eq!(
            state.transcript.as_ref().unwrap()["pendingActions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        state.transcript_revision = 4;
        assert!(!sync_transcript_from_snapshot(&mut state, &snapshot));
        assert_eq!(state.transcript_target(), Some(&target));
    }

    #[test]
    fn transcript_failure_keeps_last_good_read_and_retries_with_backoff() {
        let target = TranscriptTarget {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
        };
        let last = serde_json::json!({"entries":[{"kind":"agent","text":"last good"}]});
        let mut state = ConsoleState {
            view: View::Transcript(target.clone()),
            transcript: Some(last.clone()),
            transcript_revision: 3,
            ..ConsoleState::default()
        };
        let mut fetch = TranscriptFetch {
            target: Some(target),
            requested_revision: 3,
            ..TranscriptFetch::default()
        };
        let snapshot = serde_json::json!({"projection":{"workers":[{
            "threadId":"thread","turnId":"turn","transcriptRevision":3
        }]}});
        fetch.finish(
            Ok(Err(anyhow::anyhow!("temporary outage"))),
            &mut state,
            Some(&snapshot),
        );
        assert_eq!(state.transcript, Some(last));
        assert!(transcript_fetch_label(&state, &fetch).contains("STALE"));
        assert!(!fetch.should_fetch(&state, Some(&snapshot), Instant::now()));
        assert!(fetch.should_fetch(
            &state,
            Some(&snapshot),
            fetch.next_attempt.unwrap() + Duration::from_millis(1)
        ));
        for _ in 0..10 {
            fetch.finish(
                Ok(Err(anyhow::anyhow!("temporary outage"))),
                &mut state,
                None,
            );
        }
        let delay = fetch.next_attempt.unwrap().duration_since(Instant::now());
        assert!(delay <= Duration::from_millis(TRANSCRIPT_RETRY_MAX_MS));
        assert!(delay >= Duration::from_secs(14));
        fetch.finish(
            Ok(Ok(serde_json::json!({"entries":[]}))),
            &mut state,
            Some(&snapshot),
        );
        assert!(state.transcript_error.is_none());
        assert_eq!(state.transcript_revision, 3);
        assert_eq!(fetch.failures, 0);
    }

    #[test]
    fn transcript_fetches_only_when_revision_advances() {
        let target = TranscriptTarget {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
        };
        let state = ConsoleState {
            view: View::Transcript(target.clone()),
            transcript: Some(serde_json::json!({"entries":[]})),
            transcript_revision: 2,
            ..ConsoleState::default()
        };
        let now = Instant::now();
        let fetch = TranscriptFetch {
            target: Some(target),
            ..TranscriptFetch::default()
        };
        let snapshot = serde_json::json!({"projection":{"workers":[{
            "threadId":"thread","turnId":"turn","transcriptRevision":2
        }]}});
        assert!(!fetch.should_fetch(&state, Some(&snapshot), now + Duration::from_secs(60)));
        let changed = serde_json::json!({"projection":{"workers":[{
            "threadId":"thread","turnId":"turn","transcriptRevision":3
        }]}});
        assert!(fetch.should_fetch(&state, Some(&changed), now));
    }

    #[test]
    fn follow_keeps_actions_errors_and_full_handles_visible() {
        let target = TranscriptTarget {
            thread_id: "thread-full-recovery-handle".into(),
            turn_id: "turn-full-recovery-handle".into(),
        };
        let state = ConsoleState {
            view: View::Transcript(target.clone()),
            transcript: Some(serde_json::json!({
                "status":"inProgress", "entries": (0..40).map(|i| serde_json::json!({"kind":"agent","text":format!("message {i}")})).collect::<Vec<_>>()
            })),
            follow: true,
            ..ConsoleState::default()
        };
        let snapshot = serde_json::json!({"projection":{
            "workers":[{"threadId":target.thread_id,"turnId":target.turn_id,"status":"failed"}],
            "pendingActions":[{"threadId":target.thread_id,"turnId":target.turn_id,"kind":"userInput","method":"item/tool/requestUserInput","params":{"questions":[{"question":"Which format?"}]}}],
            "notices":[{"kind":"error","summary":"terminal worker error"}]
        }});
        let mut state = state;
        let rendered = render_frame(
            Some(&snapshot),
            Some("backend disconnected"),
            &mut state,
            &TranscriptFetch::default(),
            0,
            100,
            24,
        )
        .join("\n");
        assert!(rendered.contains("NEEDS OPERATOR"));
        assert!(rendered.contains("TURN FAILED"));
        assert!(rendered.contains("terminal worker error"));
        assert!(rendered.contains("backend disconnected"));
        assert!(rendered.contains("thread-full-recovery-handle"));
        assert!(rendered.contains("turn-full-recovery-handle"));
        assert!(rendered.contains("message 39"));
        assert!(!rendered.contains("message 0"));
    }

    #[test]
    fn tiny_terminal_uses_bounded_plain_rows() {
        let mut state = ConsoleState::default();
        let snapshot = serde_json::json!({"projection":{"workers":[{}],"pendingActions":[{}]}});
        for (width, height) in [(1, 1), (8, 3), (43, 12)] {
            let lines = render_frame(
                Some(&snapshot),
                None,
                &mut state,
                &TranscriptFetch::default(),
                0,
                width,
                height,
            );
            assert!(lines.len() <= height);
            assert!(
                lines
                    .iter()
                    .all(|line| display_width(line) <= width && !line.contains('│'))
            );
        }
    }

    #[test]
    fn dashboard_keeps_observer_data_visible_with_footer_at_bottom() {
        let snapshot = serde_json::json!({
            "runtime":{"ready":true,"buildId":"abc123","codex":{"release":"0.155.1"}},
            "projection":{
                "cwd":"/work/repo",
                "usage":{"ordinaryUsageAllowed":true,"rateLimits":{
                    "primary":{"usedPercent":50,"resetsAt":0,"windowDurationMins":300},
                    "secondary":{"usedPercent":25,"resetsAt":0,"windowDurationMins":10080}
                }},
                "workers":[{"threadId":"thread-123456789","turnId":"turn-123456789",
                    "status":"inProgress","mode":"work","model":"gpt-6-sol",
                    "effort":"high","prompt":"Review the schema"}],
                "pendingActions":[],"notices":[]
            }
        });
        let lines = render_frame(
            Some(&snapshot),
            None,
            &mut ConsoleState::default(),
            &TranscriptFetch::default(),
            0,
            90,
            28,
        );
        let rendered = lines.join("\n");
        assert_eq!(lines.len(), 28);
        let visible = |line: &str| {
            let mut parts = line.split("\x1b[");
            let mut plain = parts.next().unwrap_or_default().to_string();
            for part in parts {
                if let Some((_, text)) = part.split_once('m') {
                    plain.push_str(text);
                }
            }
            plain
        };
        assert!(lines.iter().all(|line| display_width(&visible(line)) == 90));
        assert!(rendered.contains("codex connect"));
        assert!(rendered.contains("build abc123"));
        assert!(rendered.contains("Quota · 5H"));
        assert!(rendered.contains("gpt-6-sol"));
        assert!(rendered.contains("Review the schema"));
        assert!(rendered.contains("read-only"));
        assert!(lines[27].contains("Enter open"));
        assert!(!rendered.contains('│'));
    }

    #[test]
    fn transcript_input_controls_scroll_and_tail() {
        let mut state = ConsoleState {
            view: View::Transcript(TranscriptTarget {
                thread_id: "thread".into(),
                turn_id: "turn".into(),
            }),
            scroll: 3,
            follow: true,
            ..ConsoleState::default()
        };

        assert!(state.handle_key(InputKey::Character('j'), None));
        assert_eq!(state.scroll, 4);
        assert!(!state.follow);
        assert!(state.handle_key(InputKey::Up, None));
        assert_eq!(state.scroll, 3);
        assert!(state.handle_key(InputKey::Character('G'), None));
        assert!(state.follow);
    }

    #[test]
    fn decoder_keeps_escape_and_arrow_keys_distinct() {
        let mut decoder = InputDecoder::default();
        assert!(decoder.push(b'\x1b').is_empty());
        assert!(decoder.push(b'[').is_empty());
        assert_eq!(decoder.push(b'A'), vec![InputKey::Up]);
        assert!(decoder.push(b'\x1b').is_empty());
        assert!(decoder.push(b'[').is_empty());
        assert_eq!(decoder.push(b'C'), vec![InputKey::Right]);
        assert_eq!(decoder.push(b'\r'), vec![InputKey::Enter]);
        assert!(decoder.push(b'\x1b').is_empty());
        assert_eq!(decoder.flush_escape(), Some(InputKey::Escape));
    }

    #[test]
    fn transcript_rendering_wraps_agent_and_user_text_without_terminal_controls() {
        let user_text = "keep every word\nsecond line\x1b[31m";
        let transcript = serde_json::json!({
            "entries": [
                {"kind": "user", "text": user_text},
                {"kind": "agent", "title": "Answer", "text": "the complete agent response"},
                {"kind": "tool", "text": "raw tool output must stay hidden"}
            ]
        });
        let rendered = render_transcript_entries(&transcript, 32).join("\n");
        assert!(rendered.contains("keep every word"));
        assert!(rendered.contains("second line"));
        assert_eq!(
            safe_terminal_text(user_text),
            "keep every word\nsecond line^[[31m"
        );
        assert!(rendered.contains("^[[31m"));
        assert!(rendered.contains("complete agent"));
        assert!(!rendered.contains("raw tool output"));
        assert!(!rendered.contains("\u{1b}[31m"));
        assert_eq!(wrap_terminal_text("abcdefgh", 3), ["abc", "def", "gh"]);
    }

    #[test]
    fn transcript_collapses_duplicate_review_handoff_aliases() {
        let transcript = serde_json::json!({
            "entries": [
                {"kind": "agent", "title": "AGENT · REVIEW", "text": "same review response"},
                {"kind": "agent", "title": "AGENT", "text": "same review response"}
            ]
        });
        let rendered = render_transcript_entries(&transcript, 80).join("\n");
        assert_eq!(rendered.matches("same review response").count(), 1);
        assert!(rendered.contains("agent · review"));
    }

    #[test]
    fn transcript_prefers_final_handoff_and_preserves_ordinary_repeats() {
        let transcript = serde_json::json!({
            "entries": [
                {"kind": "agent", "title": "AGENT", "text": "ordinary repeat"},
                {"kind": "agent", "title": "AGENT", "text": "ordinary repeat"},
                {"kind": "agent", "title": "AGENT", "text": "terminal response"},
                {"kind": "agent", "title": "AGENT · REVIEW", "text": "terminal response"},
                {"kind": "agent", "title": "AGENT · FINAL", "text": "terminal response"}
            ]
        });
        let rendered = render_transcript_entries(&transcript, 80).join("\n");
        assert_eq!(rendered.matches("ordinary repeat").count(), 2);
        assert_eq!(rendered.matches("terminal response").count(), 1);
        assert!(rendered.contains("agent · final"));
    }

    #[test]
    fn transcript_hides_only_the_relay_tagged_initial_task() {
        let transcript = serde_json::json!({
            "entries": [
                {"kind": "user", "text": "do the task", "initialTask": true},
                {"kind": "user", "text": "do the task"},
                {"kind": "agent", "text": "working on it"}
            ]
        });
        let rendered = render_transcript_entries(&transcript, 80).join("\n");
        assert_eq!(rendered.matches("do the task").count(), 1);
        assert!(rendered.contains("working on it"));
    }

    #[test]
    fn live_reasoning_activity_keeps_think_private() {
        let transcript = serde_json::json!({
            "status": "inProgress",
            "activity": {
                "kind": "reasoning",
                "summary": "private chain of thought",
                "lastActivityAtMs": 0,
                "tokenUsage": {"threadTotalTokens": 12, "lastRequestModelContextWindow": 34}
            }
        });
        let rendered = render_live_activity(&transcript, 100, 0).join("\n");
        assert!(rendered.contains("THINKING"));
        assert!(rendered.contains("12 tok"));
        assert!(!rendered.contains("private chain"));
    }
}
