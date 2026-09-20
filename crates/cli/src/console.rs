use crate::config::ConfigStore;
use crate::management;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{MissedTickBehavior, interval};

const REFRESH_MS: u64 = 750;
const STALE_AFTER: Duration = Duration::from_secs(3);
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

pub async fn run() -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("the console requires an interactive terminal");
    }
    let config = ConfigStore::default()?.load()?;
    let _screen = ScreenGuard::enter()?;
    let mut input = TerminalInput::enter()?;
    let mut ticker = interval(Duration::from_millis(REFRESH_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut input_ticker = interval(Duration::from_millis(50));
    input_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut snapshot = None;
    let mut last_success = None;
    let mut last_error = None;
    let mut state = ConsoleState::default();
    let mut frame = 0usize;

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("unable to listen for Ctrl-C")?;
                break;
            }
            _ = ticker.tick() => {
                match management::backend_observer_once(&config.backend).await {
                    Ok(value) => {
                        snapshot = Some(value);
                        last_success = Some(SystemTime::now());
                        last_error = None;
                    }
                    Err(error) => last_error = Some(error.to_string()),
                }
                if let Some(target) = state.transcript_target() {
                    match management::backend_transcript_once(
                        &config.backend,
                        &target.thread_id,
                        &target.turn_id,
                    ).await {
                        Ok(value) => {
                            state.transcript = Some(value);
                            state.transcript_error = None;
                        }
                        Err(error) => state.transcript_error = Some(error.to_string()),
                    }
                }
                draw(snapshot.as_ref(), last_success, last_error.as_deref(), &mut state, frame)?;
                frame = frame.wrapping_add(1);
            }
            _ = input_ticker.tick() => {
                let mut changed = false;
                for key in input.read_keys()? {
                    changed |= state.handle_key(key, snapshot.as_ref());
                }
                if changed {
                    draw(snapshot.as_ref(), last_success, last_error.as_deref(), &mut state, frame)?;
                }
            }
        }
    }
    Ok(())
}

struct ScreenGuard;

impl ScreenGuard {
    fn enter() -> Result<Self> {
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H")?;
        stdout.flush()?;
        Ok(Self)
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
    }
}

struct TerminalInput {
    original: libc::termios,
    decoder: InputDecoder,
}

impl TerminalInput {
    fn enter() -> Result<Self> {
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Err(io::Error::last_os_error()).context("unable to read terminal settings");
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        // Keep ISIG enabled: Ctrl-C must continue to be delivered as SIGINT.
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error()).context("unable to configure terminal input");
        }
        Ok(Self {
            original,
            decoder: InputDecoder::default(),
        })
    }

    fn read_keys(&mut self) -> Result<Vec<InputKey>> {
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
            if error.kind() != io::ErrorKind::WouldBlock
                && error.kind() != io::ErrorKind::Interrupted
            {
                return Err(error).context("unable to read terminal input");
            }
            return Ok(self.decoder.flush_escape().into_iter().collect());
        }
        if read == 0 {
            return Ok(self.decoder.flush_escape().into_iter().collect());
        }
        let mut keys = Vec::new();
        for byte in &bytes[..read as usize] {
            keys.extend(self.decoder.push(*byte));
        }
        Ok(keys)
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original) };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputKey {
    Character(char),
    Up,
    Down,
    Escape,
}

#[derive(Default)]
struct InputDecoder {
    escape: Vec<u8>,
}

impl InputDecoder {
    fn push(&mut self, byte: u8) -> Vec<InputKey> {
        match self.escape.as_slice() {
            [] if byte == b'\x1b' => {
                self.escape.push(byte);
                Vec::new()
            }
            [b'\x1b'] if byte == b'[' => {
                self.escape.push(byte);
                Vec::new()
            }
            [b'\x1b', b'['] if byte == b'A' => {
                self.escape.clear();
                vec![InputKey::Up]
            }
            [b'\x1b', b'['] if byte == b'B' => {
                self.escape.clear();
                vec![InputKey::Down]
            }
            [] => byte_to_key(byte).into_iter().collect(),
            _ => {
                self.escape.clear();
                let mut keys = vec![InputKey::Escape];
                keys.extend(byte_to_key(byte));
                keys
            }
        }
    }

    fn flush_escape(&mut self) -> Option<InputKey> {
        (!self.escape.is_empty()).then(|| {
            self.escape.clear();
            InputKey::Escape
        })
    }
}

fn byte_to_key(byte: u8) -> Option<InputKey> {
    char::from_u32(byte as u32).map(InputKey::Character)
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
    scroll: usize,
    follow: bool,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            view: View::Dashboard,
            transcript: None,
            transcript_error: None,
            scroll: 0,
            follow: true,
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
        match &self.view {
            View::Dashboard => {
                let InputKey::Character(key) = key else {
                    return false;
                };
                let Some(index) = key.to_digit(10).filter(|index| *index > 0) else {
                    return false;
                };
                let target = snapshot
                    .and_then(active_turns)
                    .and_then(|turns| turns.get(index as usize - 1))
                    .and_then(transcript_target_for_turn);
                let Some(target) = target else {
                    return false;
                };
                self.view = View::Transcript(target);
                self.transcript = None;
                self.transcript_error = None;
                self.scroll = 0;
                self.follow = true;
                true
            }
            View::Transcript(_) => match key {
                InputKey::Escape | InputKey::Character('b') => {
                    self.view = View::Dashboard;
                    self.transcript_error = None;
                    true
                }
                InputKey::Character('j') | InputKey::Down => {
                    self.scroll = self.scroll.saturating_add(1);
                    self.follow = false;
                    true
                }
                InputKey::Character('k') | InputKey::Up => {
                    self.scroll = self.scroll.saturating_sub(1);
                    self.follow = false;
                    true
                }
                InputKey::Character('G') => {
                    self.follow = true;
                    true
                }
                _ => false,
            },
        }
    }
}

fn active_turns(snapshot: &Value) -> Option<&Vec<Value>> {
    snapshot["projection"]["activeTurns"].as_array()
}

fn transcript_target_for_turn(turn: &Value) -> Option<TranscriptTarget> {
    Some(TranscriptTarget {
        thread_id: turn["threadId"].as_str()?.to_owned(),
        turn_id: turn["turnId"].as_str()?.to_owned(),
    })
}

fn draw(
    snapshot: Option<&Value>,
    last_success: Option<SystemTime>,
    last_error: Option<&str>,
    state: &mut ConsoleState,
    frame: usize,
) -> Result<()> {
    let (width, height) = terminal_size();
    let width = width.max(4);
    let mut lines = Vec::new();
    let pulse = ["◐", "◓", "◑", "◒"][frame % 4];
    let read_error = match (last_error, state.transcript_error.as_deref()) {
        (Some(backend), Some(transcript)) => {
            Some(format!("backend: {backend} · transcript: {transcript}"))
        }
        (Some(backend), None) => Some(format!("backend: {backend}")),
        (None, Some(transcript)) => Some(format!("transcript: {transcript}")),
        (None, None) => None,
    };

    lines.push(styled(border('╭', '─', '╮', width), CYAN));
    lines.push(styled(
        row_lr(
            " CODEX CONNECT // CONSOLE",
            &format!(
                "{pulse} {} ",
                freshness_label(last_success, SystemTime::now())
            ),
            width,
        ),
        &format!("{BOLD}{CYAN}"),
    ));
    lines.push(styled(border('├', '─', '┤', width), CYAN));

    let body_height = height.saturating_sub(6 + if read_error.is_some() { 2 } else { 0 });
    let body_start = lines.len();

    match &state.view {
        View::Dashboard => match snapshot {
            Some(snapshot) => render_snapshot(&mut lines, snapshot, width, body_height),
            None => lines.push(styled(row(" acquiring backend projection…", width), YELLOW)),
        },
        View::Transcript(target) => {
            let target = target.clone();
            render_transcript(&mut lines, state, &target, width, body_height);
        }
    }
    lines.truncate(body_start.saturating_add(body_height));

    if let Some(error) = read_error {
        lines.push(styled(border('├', '─', '┤', width), RED));
        lines.push(styled(
            row(&format!(" ⚠ BACKEND READ ERROR  {error}"), width),
            &format!("{BOLD}{RED}"),
        ));
    }

    lines.push(styled(border('├', '─', '┤', width), CYAN));
    lines.push(styled(
        row_lr(
            match state.view {
                View::Dashboard => " ◉ OBSERVE ONLY · 1-9 opens worker transcript",
                View::Transcript(_) => " ◉ OBSERVE ONLY · j/k scroll · G tail · Esc/b back",
            },
            "Ctrl-C exits ",
            width,
        ),
        DIM,
    ));
    lines.push(styled(border('╰', '─', '╯', width), CYAN));

    let mut stdout = io::stdout();
    write!(stdout, "\x1b[H")?;
    for line in lines {
        writeln!(stdout, "{line}")?;
    }
    write!(stdout, "\x1b[J")?;
    stdout.flush()?;
    Ok(())
}

fn render_snapshot(lines: &mut Vec<String>, snapshot: &Value, width: usize, height: usize) {
    let runtime = &snapshot["runtime"];
    let projection = &snapshot["projection"];
    let ready = runtime["ready"].as_bool().unwrap_or(false);
    let health = if ready { "● ONLINE" } else { "○ OFFLINE" };
    let build = text(&runtime["buildId"], "unknown");
    let release = text(&runtime["codex"]["release"], "unknown");
    let health_style = if ready { GREEN } else { RED };

    lines.push(styled(
        row_lr(
            &format!(" {health}   build {build}"),
            &format!("Codex {release} "),
            width,
        ),
        health_style,
    ));
    lines.push(styled(
        row_lr(
            &format!(" ⌂ {}", text(&projection["cwd"], "unknown")),
            &format!("refresh {}ms ", REFRESH_MS),
            width,
        ),
        DIM,
    ));

    let active = projection["activeTurns"].as_array();
    let pending = projection["pendingActions"].as_array();
    section(
        lines,
        &format!(
            "NOW // {} WORKERS // {} ACTIONS",
            active.map_or(0, Vec::len),
            pending.map_or(0, Vec::len)
        ),
        width,
    );
    if let Some(active) = active {
        if active.is_empty() {
            lines.push(styled(
                row(" ◌ IDLE · no active observed turns", width),
                DIM,
            ));
        } else {
            for (index, turn) in active.iter().take(worker_capacity(height)).enumerate() {
                lines.push(worker_row(index + 1, turn, width));
            }
        }
    }
    if let Some(pending) = pending {
        for action in pending.iter().take(4) {
            lines.push(styled(
                row(
                    &format!(
                        " ⚠ ACTION PENDING · {} · resolve through ChatGPT operator",
                        text(&action["kind"], "unknown")
                    ),
                    width,
                ),
                YELLOW,
            ));
        }
    }

    section(lines, "JOURNAL // RECENT ACTIVITY", width);
    let events = projection["events"].as_array();
    let journal_rows = journal_capacity(height, lines.len());
    let journal = events
        .map(|events| semantic_events(events))
        .unwrap_or_default();
    if journal.is_empty() {
        lines.push(styled(row(" · no observed worker events yet", width), DIM));
    } else {
        for entry in journal.iter().rev().take(journal_rows).rev() {
            lines.push(styled(row(&entry.text, width), entry.style));
        }
    }
    let cursor = projection["cursor"].as_u64().unwrap_or(0);
    let history = if projection["historyLost"].as_bool().unwrap_or(false) {
        "   ⚠ older history evicted"
    } else {
        ""
    };
    lines.push(styled(
        row_lr(
            &format!(" journal cursor #{cursor}{history}"),
            "bounded relay projection ",
            width,
        ),
        DIM,
    ));
    lines.push(styled(row(&account_line(projection, width), width), DIM));
}

fn render_transcript(
    lines: &mut Vec<String>,
    state: &mut ConsoleState,
    target: &TranscriptTarget,
    width: usize,
    height: usize,
) {
    let mut body = Vec::new();
    body.push(styled(
        row(
            &format!(
                " TRANSCRIPT · thread {} · turn {}",
                short_id(&target.thread_id),
                short_id(&target.turn_id)
            ),
            width,
        ),
        &format!("{BOLD}{CYAN}"),
    ));
    match state.transcript.as_ref() {
        Some(transcript) => {
            if transcript["truncated"].as_bool().unwrap_or(false) {
                body.push(styled(
                    row(" ⚠ transcript output truncated to observer bounds", width),
                    YELLOW,
                ));
            }
            if let Some(activity) = transcript.get("activity").filter(|value| !value.is_null()) {
                body.extend(render_live_activity(activity, width));
            }
            body.extend(render_transcript_entries(transcript, width));
        }
        None => body.push(styled(row(" acquiring live transcript…", width), YELLOW)),
    }
    if body.len() == 1 && state.transcript.is_some() {
        body.push(styled(row(" · no transcript entries yet", width), DIM));
    }

    let available = height.max(1);
    let max_scroll = body.len().saturating_sub(available);
    if state.follow {
        state.scroll = max_scroll;
    } else {
        state.scroll = state.scroll.min(max_scroll);
    }
    lines.extend(body.into_iter().skip(state.scroll).take(available));
}

fn render_transcript_entries(transcript: &Value, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(entries) = transcript["entries"].as_array() else {
        return lines;
    };
    for entry in entries {
        let kind = entry["kind"].as_str().unwrap_or("entry");
        if kind.eq_ignore_ascii_case("reasoning") || kind.eq_ignore_ascii_case("think") {
            lines.push(styled(
                row(&format!(" ◌ THINK{}", status_suffix(entry)), width),
                DIM,
            ));
            continue;
        }
        let label = safe_terminal_text(&kind.to_ascii_uppercase());
        lines.push(styled(
            row(&format!(" {label}{}", status_suffix(entry)), width),
            entry_style(kind, entry["status"].as_str()),
        ));
        if let Some(title) = entry["title"].as_str() {
            lines.extend(render_wrapped_text(title, "   ", width, DIM));
        }
        if let Some(text) = entry["text"].as_str() {
            lines.extend(render_wrapped_text(
                text,
                "   ",
                width,
                entry_style(kind, None),
            ));
        }
    }
    lines
}

fn render_live_activity(activity: &Value, width: usize) -> Vec<String> {
    let kind = activity["kind"].as_str().unwrap_or("working");
    let age = activity["lastActivityAtMs"]
        .as_u64()
        .map(activity_age)
        .unwrap_or_else(|| "now".to_string());
    let usage = activity["tokenUsage"]
        .as_object()
        .and_then(|usage| {
            Some(format!(
                " · {} / {} tok",
                usage.get("totalTokens")?.as_u64()?,
                usage.get("modelContextWindow")?.as_u64()?
            ))
        })
        .unwrap_or_default();
    let reasoning = kind.eq_ignore_ascii_case("reasoning") || kind.eq_ignore_ascii_case("think");
    let detail = if reasoning {
        "THINK".to_string()
    } else {
        let summary = activity["summary"].as_str().unwrap_or("working");
        format!(
            "{}: {}",
            safe_terminal_text(kind),
            safe_terminal_text(summary)
        )
    };
    vec![styled(
        row(&format!(" ▶ LIVE · {detail} · {age}{usage}"), width),
        if reasoning { DIM } else { GREEN },
    )]
}

fn status_suffix(entry: &Value) -> String {
    entry["status"]
        .as_str()
        .map(|status| format!(" · {}", safe_terminal_text(status)))
        .unwrap_or_default()
}

fn entry_style(kind: &str, status: Option<&str>) -> &'static str {
    if status.is_some_and(|status| {
        status.eq_ignore_ascii_case("failed") || status.eq_ignore_ascii_case("error")
    }) {
        RED
    } else if kind.eq_ignore_ascii_case("user") {
        CYAN
    } else if kind.eq_ignore_ascii_case("agent") || kind.eq_ignore_ascii_case("assistant") {
        GREEN
    } else {
        DIM
    }
}

fn render_wrapped_text(text: &str, prefix: &str, width: usize, style: &'static str) -> Vec<String> {
    let inner = width.saturating_sub(2);
    let prefix = clip(prefix, inner);
    let line_width = inner.saturating_sub(display_width(&prefix)).max(1);
    wrap_terminal_text(text, line_width)
        .into_iter()
        .map(|line| styled(row(&format!("{prefix}{line}"), width), style))
        .collect()
}

fn wrap_terminal_text(text: &str, width: usize) -> Vec<String> {
    let safe = safe_terminal_text(text);
    let mut lines = Vec::new();
    for paragraph in safe.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for character in paragraph.chars() {
            let character_width = char_display_width(character);
            if used > 0 && used + character_width > width {
                lines.push(line);
                line = String::new();
                used = 0;
            }
            line.push(character);
            used += character_width;
        }
        lines.push(line);
    }
    lines
}

fn safe_terminal_text(text: &str) -> String {
    text.chars()
        .flat_map(|character| match character {
            '\n' => "\n".chars().collect::<Vec<_>>(),
            '\t' => "    ".chars().collect(),
            '\x1b' => "^[".chars().collect(),
            character if character.is_control() => {
                format!("\\x{:02x}", character as u32).chars().collect()
            }
            character => vec![character],
        })
        .collect()
}

fn display_width(value: &str) -> usize {
    value.chars().map(char_display_width).sum()
}

fn char_display_width(character: char) -> usize {
    if character.is_control() {
        0
    } else if matches!(character as u32,
        0x1100..=0x115f | 0x2329..=0x232a | 0x2e80..=0xa4cf | 0xac00..=0xd7a3 |
        0xf900..=0xfaff | 0xfe10..=0xfe19 | 0xfe30..=0xfe6f | 0xff00..=0xff60 |
        0xffe0..=0xffe6 | 0x1f300..=0x1faff | 0x20000..=0x3fffd
    ) {
        2
    } else {
        1
    }
}

fn worker_row(number: usize, turn: &Value, width: usize) -> String {
    let mode = text(&turn["mode"], "unknown");
    let model = turn["model"].as_str().unwrap_or("default/inherited");
    let effort = turn["effort"].as_str().unwrap_or("default/inherited");
    let state = text(&turn["status"], "unknown");
    let tier = turn["serviceTier"].as_str().unwrap_or("default");
    let activity = activity_line(turn);
    let glyph = match state {
        "inProgress" => "▶",
        "completed" => "✓",
        "failed" => "✕",
        "interrupted" => "■",
        _ => "•",
    };
    let line = format!(
        " {number} {glyph} {} · {} · {} · {} · {} · {}",
        clip(mode, 12),
        clip(model, 24),
        clip(effort, 12),
        clip(tier, 12),
        clip(&activity, 48),
        state,
    );
    styled(
        row(&line, width),
        if state == "failed" { RED } else { GREEN },
    )
}

fn activity_line(turn: &Value) -> String {
    let kind = turn["activityKind"].as_str().unwrap_or("working");
    let summary = turn["activitySummary"].as_str();
    let age = turn["lastActivityAtMs"]
        .as_u64()
        .map(activity_age)
        .unwrap_or_else(|| "age unknown".into());
    let usage = turn["tokenUsage"]
        .as_object()
        .and_then(|usage| {
            Some(format!(
                " · {} / {} tok",
                usage.get("totalTokens")?.as_u64()?,
                usage.get("modelContextWindow")?.as_u64()?
            ))
        })
        .unwrap_or_default();
    let hidden_reasoning = matches!(kind.to_ascii_lowercase().as_str(), "reasoning" | "think");
    let label = if hidden_reasoning { "THINK" } else { kind };
    match summary {
        Some(summary) if !hidden_reasoning => {
            format!("{label}: {} · {age}{usage}", clip(summary, 42))
        }
        _ => format!("{label} · {age}{usage}"),
    }
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
    let limits = &projection["usage"]["rateLimits"];
    let primary = quota_summary("5H", &limits["primary"], width >= 100);
    let secondary = quota_summary("7D", &limits["secondary"], width >= 100);
    let access = projection["usage"]["ordinaryUsageAllowed"]
        .as_bool()
        .map(|allowed| if allowed { "open" } else { "blocked" })
        .unwrap_or("unknown");
    format!(" ACCOUNT · {primary} · {secondary} · usage {access}")
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
            "{label} {} {:>3.0}% {reset}",
            "█".repeat(filled) + &"░".repeat(8 - filled),
            used
        )
    } else {
        format!("{label} {:>3.0}% {reset}", used)
    }
}

fn quota_label(value: &Value) -> Option<String> {
    let minutes = value["windowDurationMins"].as_u64()?;
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

struct JournalEntry {
    text: String,
    style: &'static str,
}

fn semantic_events(events: &[Value]) -> Vec<JournalEntry> {
    events.iter().filter_map(semantic_event).collect()
}

fn semantic_event(event: &Value) -> Option<JournalEntry> {
    let cursor = event["cursor"].as_u64().unwrap_or(0);
    let kind = event["kind"].as_str()?;
    let phase = event["phase"].as_str();
    let subject = event["threadId"].as_str().map(short_id).unwrap_or("system");
    let summary = event["summary"].as_str();
    let hidden_reasoning = matches!(kind.to_ascii_lowercase().as_str(), "think" | "reasoning");
    let detail = if hidden_reasoning {
        "THINK".to_string()
    } else {
        match summary {
            Some(summary) if !summary.is_empty() => format!("{kind} {}", clip(summary, 56)),
            _ => kind.to_string(),
        }
    };
    Some(JournalEntry {
        text: format!(
            " {} #{cursor} {subject} · {detail}",
            semantic_event_glyph(kind, phase)
        ),
        style: semantic_event_style(kind, phase),
    })
}

fn semantic_event_glyph(kind: &str, phase: Option<&str>) -> &'static str {
    if kind == "error" || phase == Some("failed") {
        "✕"
    } else if kind == "waiting" {
        "⚠"
    } else if kind == "think" || kind == "reasoning" {
        "◌"
    } else if phase == Some("completed") {
        "✓"
    } else if phase == Some("started") {
        "▶"
    } else {
        "·"
    }
}

fn freshness_label(last_success: Option<SystemTime>, now: SystemTime) -> &'static str {
    match last_success.and_then(|time| now.duration_since(time).ok()) {
        Some(age) if age <= STALE_AFTER => "LIVE",
        _ => "STALE",
    }
}

fn worker_capacity(height: usize) -> usize {
    height.saturating_sub(13).clamp(1, 9)
}

fn journal_capacity(height: usize, rendered: usize) -> usize {
    height.saturating_sub(rendered + 2)
}

fn semantic_event_style(kind: &str, phase: Option<&str>) -> &'static str {
    if kind == "error" || phase == Some("failed") {
        RED
    } else if kind == "waiting" {
        YELLOW
    } else if phase == Some("completed") {
        GREEN
    } else if phase == Some("started") {
        CYAN
    } else {
        DIM
    }
}

fn section(lines: &mut Vec<String>, title: &str, width: usize) {
    lines.push(styled(border('├', '─', '┤', width), CYAN));
    lines.push(styled(
        row(&format!(" {title}"), width),
        &format!("{BOLD}{CYAN}"),
    ));
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

fn clip(value: &str, max: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max {
        value.to_string()
    } else if max <= 1 {
        "…".into()
    } else {
        format!("{}…", chars[..max - 1].iter().collect::<String>())
    }
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

fn border(left: char, fill: char, right: char, width: usize) -> String {
    format!(
        "{left}{}{right}",
        fill.to_string().repeat(width.saturating_sub(2))
    )
}

fn row(content: &str, width: usize) -> String {
    let inner = width.saturating_sub(2);
    let mut body = truncate_display_width(content, inner);
    let used = display_width(&body);
    if used < inner {
        body.push_str(&" ".repeat(inner - used));
    }
    format!("│{body}│")
}

fn row_lr(left: &str, right: &str, width: usize) -> String {
    let inner = width.saturating_sub(2);
    let right = truncate_display_width(right, inner);
    let right_width = display_width(&right);
    if right_width >= inner {
        return row(&right, width);
    }
    let left_max = inner.saturating_sub(right_width + 1);
    let left = truncate_display_width(left, left_max);
    let padding = inner.saturating_sub(display_width(&left) + right_width);
    format!("│{left}{}{right}│", " ".repeat(padding))
}

fn truncate_display_width(value: &str, width: usize) -> String {
    let mut truncated = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = char_display_width(character);
        if used + character_width > width {
            break;
        }
        truncated.push(character);
        used += character_width;
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_is_cropped_to_terminal_width() {
        assert_eq!(row(" abcdef", 6).chars().count(), 6);
    }

    #[test]
    fn worker_row_exposes_model_and_effort() {
        let turn = serde_json::json!({
            "threadId": "thread-123456789",
            "turnId": "turn-123456789",
            "status": "inProgress",
            "mode": "work",
            "model": "gpt-5.6-sol",
            "effort": "high"
        });
        let rendered = worker_row(1, &turn, 120);
        assert!(rendered.contains(" 1 ▶"));
        assert!(rendered.contains("gpt-5.6-sol"));
        assert!(rendered.contains("high"));
    }

    #[test]
    fn worker_row_shows_activity_age_and_usage_without_thinking_text() {
        let turn = serde_json::json!({
            "threadId": "thread-123456789",
            "turnId": "turn-123456789",
            "status": "inProgress",
            "mode": "work",
            "model": "gpt-5.6-sol",
            "effort": "high",
            "serviceTier": "priority",
            "lastActivityAtMs": 0,
            "activityKind": "think",
            "activitySummary": "private chain of thought",
            "tokenUsage": {"totalTokens": 123, "modelContextWindow": 456}
        });
        let rendered = worker_row(1, &turn, 160);
        assert!(rendered.contains("THINK"));
        assert!(rendered.contains("123 / 456 tok"));
        assert!(!rendered.contains("private chain of thought"));
    }

    #[test]
    fn semantic_journal_renders_relay_projection_and_hides_reasoning_text() {
        let events = vec![
            serde_json::json!({
                "cursor": 1,
                "threadId": "thread-123456789",
                "turnId": "turn-1",
                "kind": "tool",
                "summary": "cargo test",
                "phase": "started"
            }),
            serde_json::json!({
                "cursor": 2,
                "threadId": "thread-123456789",
                "turnId": "turn-1",
                "kind": "think",
                "summary": "secret reasoning",
                "phase": "started"
            }),
        ];
        let rendered = semantic_events(&events);
        assert_eq!(rendered.len(), 2);
        assert!(rendered[0].text.contains("tool cargo test"));
        assert!(rendered[1].text.contains("THINK"));
        assert!(!rendered[1].text.contains("secret"));
    }

    #[test]
    fn narrow_rows_never_expand_the_terminal() {
        assert_eq!(row_lr(" a long left side", "right ", 8).chars().count(), 8);
        assert_eq!(row("a long line", 4).chars().count(), 4);
        assert_eq!(display_width(&row("界界", 6)), 6);
    }

    #[test]
    fn freshness_is_live_only_after_a_recent_successful_refresh() {
        let now = SystemTime::now();
        assert_eq!(freshness_label(Some(now), now), "LIVE");
        assert_eq!(
            freshness_label(Some(now - STALE_AFTER - Duration::from_secs(1)), now),
            "STALE"
        );
        assert_eq!(freshness_label(None, now), "STALE");
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
        assert!(compact.contains("ACCOUNT · 5H  50% due · 7D  25% due · usage open"));
        assert!(!compact.contains('█'));
        assert!(account_line(&projection, 120).contains("████"));
    }

    #[test]
    fn row_lr_preserves_requested_width() {
        assert_eq!(row_lr(" left", "right ", 30).chars().count(), 30);
    }

    #[test]
    fn number_key_selects_the_matching_active_worker_and_back_returns() {
        let snapshot = serde_json::json!({
            "projection": {
                "activeTurns": [
                    {"threadId": "thread-one", "turnId": "turn-one"},
                    {"threadId": "thread-two", "turnId": "turn-two"}
                ]
            }
        });
        let mut state = ConsoleState::default();

        assert!(state.handle_key(InputKey::Character('2'), Some(&snapshot)));
        assert_eq!(
            state.view,
            View::Transcript(TranscriptTarget {
                thread_id: "thread-two".into(),
                turn_id: "turn-two".into(),
            })
        );
        assert!(state.handle_key(InputKey::Character('b'), Some(&snapshot)));
        assert_eq!(state.view, View::Dashboard);
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
        assert_eq!(decoder.flush_escape(), Some(InputKey::Escape));
    }

    #[test]
    fn transcript_rendering_wraps_agent_and_user_text_without_terminal_controls() {
        let user_text = "keep every word\nsecond line\x1b[31m";
        let transcript = serde_json::json!({
            "entries": [
                {"kind": "user", "text": user_text},
                {"kind": "agent", "title": "Answer", "text": "the complete agent response"},
                {"kind": "reasoning", "text": "private chain of thought"}
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
        assert!(rendered.contains("THINK"));
        assert!(!rendered.contains("private chain"));
        assert!(!rendered.contains("\u{1b}[31m"));
        assert_eq!(wrap_terminal_text("abcdefgh", 3), ["abc", "def", "gh"]);
    }

    #[test]
    fn live_reasoning_activity_keeps_think_private() {
        let activity = serde_json::json!({
            "kind": "reasoning",
            "summary": "private chain of thought",
            "lastActivityAtMs": 0,
            "tokenUsage": {"totalTokens": 12, "modelContextWindow": 34}
        });
        let rendered = render_live_activity(&activity, 100).join("\n");
        assert!(rendered.contains("LIVE · THINK"));
        assert!(rendered.contains("12 / 34 tok"));
        assert!(!rendered.contains("private chain"));
    }
}
