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
    let mut ticker = interval(Duration::from_millis(REFRESH_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut snapshot = None;
    let mut last_success = None;
    let mut last_error: Option<String>;
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
                draw(snapshot.as_ref(), last_success, last_error.as_deref(), frame)?;
                frame = frame.wrapping_add(1);
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

fn draw(
    snapshot: Option<&Value>,
    last_success: Option<SystemTime>,
    last_error: Option<&str>,
    frame: usize,
) -> Result<()> {
    let (width, height) = terminal_size();
    let width = width.max(4);
    let mut lines = Vec::new();
    let pulse = ["◐", "◓", "◑", "◒"][frame % 4];

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

    let body_height = height.saturating_sub(3 + if last_error.is_some() { 2 } else { 0 });

    match snapshot {
        Some(snapshot) => render_snapshot(&mut lines, snapshot, width, body_height),
        None => lines.push(styled(row(" acquiring backend projection…", width), YELLOW)),
    }
    lines.truncate(body_height);

    if let Some(error) = last_error {
        lines.push(styled(border('├', '─', '┤', width), RED));
        lines.push(styled(
            row(&format!(" ⚠ BACKEND READ ERROR  {error}"), width),
            &format!("{BOLD}{RED}"),
        ));
    }

    lines.push(styled(border('├', '─', '┤', width), CYAN));
    lines.push(styled(
        row_lr(
            " ◉ OBSERVE ONLY · zero actuation surface",
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
            for turn in active.iter().take(worker_capacity(height)) {
                lines.push(worker_row(turn, width));
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

fn worker_row(turn: &Value, width: usize) -> String {
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
        " {glyph} {} · {} · {} · {} · {} · {}",
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
    key: String,
    text: String,
    style: &'static str,
}

fn semantic_events(events: &[Value]) -> Vec<JournalEntry> {
    let mut entries: Vec<JournalEntry> = Vec::new();
    for event in events {
        let Some(entry) = semantic_event(event) else {
            continue;
        };
        if let Some(previous) = entries.last()
            && previous.key == entry.key
        {
            continue;
        }
        entries.push(entry);
    }
    entries
}

fn semantic_event(event: &Value) -> Option<JournalEntry> {
    let cursor = event["cursor"].as_u64().unwrap_or(0);
    let method = text(&event["method"], "event");
    if is_journal_noise(method) {
        return None;
    }
    let params = &event["params"];
    let subject = event["threadId"].as_str().map(short_id).unwrap_or("system");
    let detail = event_detail(method, params);
    Some(JournalEntry {
        key: format!("{subject}:{detail}"),
        text: format!(" {} #{cursor} {subject} · {detail}", event_glyph(method)),
        style: event_style(event),
    })
}

fn is_journal_noise(method: &str) -> bool {
    let method = method.to_ascii_lowercase();
    method.contains("token")
        || method.contains("ratelimit")
        || method.contains("rate_limit")
        || method.contains("usage")
        || method.contains("delta")
}

fn event_detail(method: &str, params: &Value) -> String {
    let lowered = method.to_ascii_lowercase();
    if lowered.contains("reasoning") {
        return "THINK".into();
    }
    if lowered.contains("turn/started") || lowered.ends_with("turnstarted") {
        return format!("turn started{}", model_detail(params));
    }
    if lowered.contains("turn/completed") {
        return "turn completed".into();
    }
    if lowered.contains("turn/failed") || lowered.contains("error") {
        return format!("turn failed{}", message_detail(params));
    }
    if lowered.contains("approval") || lowered.contains("request") || lowered.contains("input") {
        let kind = params["kind"].as_str().unwrap_or("operator action");
        return format!("{kind} requested{}", message_detail(params));
    }
    if lowered.contains("tool") {
        let tool = params["toolName"]
            .as_str()
            .or_else(|| params["tool"].as_str())
            .unwrap_or("tool");
        return format!("tool {}", clip(tool, 36));
    }
    if let Some(item) = params.get("item") {
        return item_detail(item);
    }
    if let Some(message) = safe_event_message(params) {
        return clip(message, 56);
    }
    clip(method.rsplit('/').next().unwrap_or(method), 56)
}

fn item_detail(item: &Value) -> String {
    match item["type"].as_str().unwrap_or("item") {
        "reasoning" => "THINK".into(),
        "commandExecution" => format!(
            "command {}",
            clip(item["command"].as_str().unwrap_or("started"), 42)
        ),
        "fileChange" => "filesystem change".into(),
        "webSearch" => format!(
            "search {}",
            clip(item["query"].as_str().unwrap_or("started"), 42)
        ),
        "mcpToolCall" => format!(
            "tool {}",
            clip(
                item["tool"]
                    .as_str()
                    .or_else(|| item["name"].as_str())
                    .unwrap_or("started"),
                42
            )
        ),
        "agentMessage" => "message updated".into(),
        other => format!("{other} updated"),
    }
}

fn model_detail(params: &Value) -> String {
    params["model"]
        .as_str()
        .map(|model| format!(" · {model}"))
        .unwrap_or_default()
}

fn message_detail(params: &Value) -> String {
    safe_event_message(params)
        .map(|message| format!(" · {}", clip(message, 42)))
        .unwrap_or_default()
}

fn safe_event_message(params: &Value) -> Option<&str> {
    params["message"]
        .as_str()
        .or_else(|| params["error"]["message"].as_str())
        .or_else(|| params["title"].as_str())
}

fn event_glyph(method: &str) -> &'static str {
    if method.contains("started") {
        "▶"
    } else if method.contains("completed") {
        "✓"
    } else if method.contains("failed") || method.contains("error") {
        "✕"
    } else if method.contains("approval") || method.contains("request") {
        "⚠"
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
    height.saturating_sub(13).clamp(1, 4)
}

fn journal_capacity(height: usize, rendered: usize) -> usize {
    height.saturating_sub(rendered + 2)
}

fn event_style(event: &Value) -> &'static str {
    let method = text(&event["method"], "");
    if method.contains("failed") || method.contains("error") {
        RED
    } else if method.contains("approval") || method.contains("request") {
        YELLOW
    } else if method.contains("completed") {
        GREEN
    } else if method.contains("started") {
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
    let mut body = content.chars().take(inner).collect::<String>();
    let used = body.chars().count();
    if used < inner {
        body.push_str(&" ".repeat(inner - used));
    }
    format!("│{body}│")
}

fn row_lr(left: &str, right: &str, width: usize) -> String {
    let inner = width.saturating_sub(2);
    let right_len = right.chars().count();
    if right_len >= inner {
        return row(right, width);
    }
    let left_max = inner.saturating_sub(right_len + 1);
    let left = clip(left, left_max);
    let padding = inner.saturating_sub(left.chars().count() + right_len);
    format!("│{left}{}{right}│", " ".repeat(padding))
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
        let rendered = worker_row(&turn, 120);
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
        let rendered = worker_row(&turn, 160);
        assert!(rendered.contains("THINK"));
        assert!(rendered.contains("123 / 456 tok"));
        assert!(!rendered.contains("private chain of thought"));
    }

    #[test]
    fn semantic_journal_interprets_items_and_suppresses_noise() {
        let events = vec![
            serde_json::json!({
                "cursor": 1,
                "method": "item/started",
                "threadId": "thread-123456789",
                "params": {"item": {"type": "commandExecution", "command": "cargo test"}}
            }),
            serde_json::json!({
                "cursor": 2,
                "method": "item/started",
                "threadId": "thread-123456789",
                "params": {"item": {"type": "reasoning", "summary": "secret"}}
            }),
            serde_json::json!({"cursor": 3, "method": "thread/tokenUsage/updated", "params": {}}),
            serde_json::json!({
                "cursor": 4,
                "method": "item/started",
                "threadId": "thread-123456789",
                "params": {"item": {"type": "reasoning", "summary": "another secret"}}
            }),
        ];
        let rendered = semantic_events(&events);
        assert_eq!(rendered.len(), 2);
        assert!(rendered[0].text.contains("command cargo test"));
        assert!(rendered[1].text.contains("THINK"));
        assert!(!rendered[1].text.contains("secret"));
    }

    #[test]
    fn narrow_rows_never_expand_the_terminal() {
        assert_eq!(row_lr(" a long left side", "right ", 8).chars().count(), 8);
        assert_eq!(row("a long line", 4).chars().count(), 4);
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
}
