use crate::config::ConfigStore;
use crate::management;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{MissedTickBehavior, interval};

const REFRESH_MS: u64 = 750;
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const MAGENTA: &str = "\x1b[35m";

pub async fn run() -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("the console requires an interactive terminal");
    }
    let config = ConfigStore::default()?.load()?;
    let _screen = ScreenGuard::enter()?;
    let mut ticker = interval(Duration::from_millis(REFRESH_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut snapshot = None;
    let mut frame = 0usize;

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("unable to listen for Ctrl-C")?;
                break;
            }
            _ = ticker.tick() => {
                let last_error = match management::backend_observer_once(&config.backend).await {
                    Ok(value) => {
                        snapshot = Some(value);
                        None
                    }
                    Err(error) => Some(error.to_string()),
                };
                draw(snapshot.as_ref(), last_error.as_deref(), frame)?;
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

fn draw(snapshot: Option<&Value>, last_error: Option<&str>, frame: usize) -> Result<()> {
    let (width, height) = terminal_size();
    let width = width.max(72);
    let mut lines = Vec::new();
    let pulse = ["◐", "◓", "◑", "◒"][frame % 4];

    lines.push(styled(border('╭', '─', '╮', width), CYAN));
    lines.push(styled(
        row_lr(
            " CODEX CONNECT // CONSOLE",
            &format!("{pulse} LIVE PROJECTION "),
            width,
        ),
        &format!("{BOLD}{CYAN}"),
    ));
    lines.push(styled(border('├', '─', '┤', width), CYAN));

    match snapshot {
        Some(snapshot) => render_snapshot(&mut lines, snapshot, width),
        None => lines.push(styled(row(" acquiring backend projection…", width), YELLOW)),
    }

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

    let visible = height.max(10);
    if lines.len() > visible {
        lines.truncate(visible);
        if let Some(last) = lines.last_mut() {
            *last = styled(border('╰', '─', '╯', width), CYAN);
        }
    }

    let mut stdout = io::stdout();
    write!(stdout, "\x1b[H")?;
    for line in lines {
        writeln!(stdout, "{line}")?;
    }
    write!(stdout, "\x1b[J")?;
    stdout.flush()?;
    Ok(())
}

fn render_snapshot(lines: &mut Vec<String>, snapshot: &Value, width: usize) {
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

    section(lines, "QUOTA // ACCOUNT TELEMETRY", width);
    let limits = &projection["usage"]["rateLimits"];
    lines.push(styled(
        row(&quota_line("5H", &limits["primary"]), width),
        CYAN,
    ));
    lines.push(styled(
        row(&quota_line("7D", &limits["secondary"]), width),
        MAGENTA,
    ));
    let allowed = projection["usage"]["ordinaryUsageAllowed"]
        .as_bool()
        .map(|value| if value { "OPEN" } else { "BLOCKED" })
        .unwrap_or("UNKNOWN");
    let credits = projection["usage"]["rateLimitResetCredits"]["availableCount"]
        .as_u64()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".into());
    lines.push(styled(
        row_lr(
            &format!(" ordinary usage {allowed}   reset credits {credits}"),
            "quota sample ≤5s ",
            width,
        ),
        DIM,
    ));

    let active = projection["activeTurns"].as_array();
    let pending = projection["pendingActions"].as_array();
    section(
        lines,
        &format!(
            "WORKERS // {} ACTIVE // {} ACTIONS",
            active.map_or(0, Vec::len),
            pending.map_or(0, Vec::len)
        ),
        width,
    );
    lines.push(styled(
        row(
            " THREAD    TURN      MODE    MODEL                    REASONING    STATE",
            width,
        ),
        DIM,
    ));
    if let Some(active) = active {
        if active.is_empty() {
            lines.push(styled(
                row(" ◌ IDLE · no active observed turns", width),
                DIM,
            ));
        } else {
            for turn in active.iter().take(8) {
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

    section(lines, "EVENT STREAM // RECENT ACTIVITY", width);
    let events = projection["events"].as_array();
    match events {
        Some(events) if !events.is_empty() => {
            for event in events.iter().rev().take(10).rev() {
                lines.push(styled(row(&event_line(event), width), event_style(event)));
            }
        }
        _ => lines.push(styled(row(" · no observed worker events yet", width), DIM)),
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
}

fn worker_row(turn: &Value, width: usize) -> String {
    let mode = text(&turn["mode"], "unknown");
    let model = turn["model"].as_str().unwrap_or("default/inherited");
    let effort = turn["effort"].as_str().unwrap_or("default/inherited");
    let state = text(&turn["status"], "unknown");
    let glyph = match state {
        "inProgress" => "▶",
        "completed" => "✓",
        "failed" => "✕",
        "interrupted" => "■",
        _ => "•",
    };
    let line = format!(
        " {:<8}  {:<8}  {:<6}  {:<23}  {:<11}  {glyph} {}",
        short_id(text(&turn["threadId"], "?")),
        short_id(text(&turn["turnId"], "?")),
        clip(mode, 6),
        clip(model, 23),
        clip(effort, 11),
        clip(state, 12),
    );
    styled(
        row(&line, width),
        if state == "failed" { RED } else { GREEN },
    )
}

fn quota_line(label: &str, value: &Value) -> String {
    let used = value["usedPercent"]
        .as_f64()
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    let remaining = 100.0 - used;
    let filled = ((used / 100.0) * 28.0).round() as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(28 - filled));
    let reset = value["resetsAt"]
        .as_u64()
        .map(reset_in)
        .unwrap_or_else(|| "reset unknown".into());
    format!(
        " {label:<2}  {bar}  {:>5.1}% used  {:>5.1}% left  · {reset}",
        used, remaining
    )
}

fn reset_in(epoch_seconds: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if epoch_seconds <= now {
        return "reset due".into();
    }
    let remaining = epoch_seconds - now;
    let days = remaining / 86_400;
    let hours = (remaining % 86_400) / 3_600;
    let minutes = (remaining % 3_600) / 60;
    if days > 0 {
        format!("reset in {days}d {hours}h")
    } else if hours > 0 {
        format!("reset in {hours}h {minutes}m")
    } else {
        format!("reset in {minutes}m")
    }
}

fn event_line(event: &Value) -> String {
    let cursor = event["cursor"].as_u64().unwrap_or(0);
    let method = text(&event["method"], "event");
    let thread = event["threadId"]
        .as_str()
        .map(short_id)
        .unwrap_or("--------");
    let turn = event["turnId"].as_str().map(short_id).unwrap_or("--------");
    format!(
        " {} #{cursor:<5} {thread} {turn}  {}",
        event_glyph(method),
        clip(method, 46)
    )
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
    fn quota_line_has_fixed_bar_width() {
        let value = serde_json::json!({"usedPercent": 50.0, "resetsAt": 0});
        let line = quota_line("5H", &value);
        assert!(line.contains("██████████████░░░░░░░░░░░░░░"));
        assert!(line.contains("50.0% used"));
        assert!(line.contains("50.0% left"));
    }

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
    fn row_lr_preserves_requested_width() {
        assert_eq!(row_lr(" left", "right ", 30).chars().count(), 30);
    }
}
