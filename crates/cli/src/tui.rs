use crate::config::ConfigStore;
use crate::management;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{MissedTickBehavior, interval};

const REFRESH_MS: u64 = 750;

pub async fn run() -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("the TUI requires an interactive terminal");
    }
    let config = ConfigStore::default()?.load()?;
    let _screen = ScreenGuard::enter()?;
    let mut ticker = interval(Duration::from_millis(REFRESH_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut snapshot = None;

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
                draw(snapshot.as_ref(), last_error.as_deref())?;
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

fn draw(snapshot: Option<&Value>, last_error: Option<&str>) -> Result<()> {
    let (width, height) = terminal_size();
    let width = width.max(48);
    let mut lines = Vec::new();
    lines.push(border('┌', '─', '┐', width));
    lines.push(row(" CODEX CONNECT · OBSERVER ", width));
    lines.push(border('├', '─', '┤', width));

    match snapshot {
        Some(snapshot) => render_snapshot(&mut lines, snapshot, width),
        None => {
            lines.push(row(" waiting for backend projection…", width));
        }
    }

    if let Some(error) = last_error {
        lines.push(border('├', '─', '┤', width));
        lines.push(row(&format!(" backend read error: {error}"), width));
    }
    lines.push(border('├', '─', '┤', width));
    lines.push(row(
        " OBSERVE ONLY · control remains with ChatGPT / Codex Connect · Ctrl-C exits",
        width,
    ));
    lines.push(border('└', '─', '┘', width));

    let visible = height.max(8);
    if lines.len() > visible {
        lines.truncate(visible);
        if let Some(last) = lines.last_mut() {
            *last = border('└', '─', '┘', width);
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
    let status = &snapshot["status"];
    let projection = &snapshot["projection"];
    let health = if status["healthy"].as_bool().unwrap_or(false) {
        "● healthy"
    } else {
        "○ unavailable"
    };
    let build = text(&status["buildId"], "unknown");
    let release = text(&status["codex"]["release"], "unknown");
    lines.push(row(
        &format!(" {health}   build {build}   Codex {release}"),
        width,
    ));
    lines.push(row(
        &format!(" cwd {}", text(&projection["defaultCwd"], "unknown")),
        width,
    ));

    lines.push(border('├', '─', '┤', width));
    lines.push(row(" USAGE", width));
    let limits = &projection["usage"]["rateLimits"];
    lines.push(row(&quota_line("5h", &limits["primary"]), width));
    lines.push(row(&quota_line("7d", &limits["secondary"]), width));
    let allowed = projection["usage"]["ordinaryUsageAllowed"]
        .as_bool()
        .map(|value| if value { "allowed" } else { "blocked" })
        .unwrap_or("unknown");
    let credits = projection["usage"]["rateLimitResetCredits"]["availableCount"]
        .as_u64()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".into());
    lines.push(row(
        &format!(" ordinary usage: {allowed}   reset credits: {credits}   quota sample ≤5s old"),
        width,
    ));

    lines.push(border('├', '─', '┤', width));
    let active = projection["activeTurns"].as_array();
    let pending = projection["pendingActions"].as_array();
    lines.push(row(
        &format!(
            " ACTIVE WORK · {} turn(s) · {} pending action(s)",
            active.map_or(0, Vec::len),
            pending.map_or(0, Vec::len)
        ),
        width,
    ));
    if let Some(active) = active {
        if active.is_empty() {
            lines.push(row(" idle — no active observed turns", width));
        } else {
            for turn in active.iter().take(8) {
                lines.push(row(
                    &format!(
                        " {}  {}  {}",
                        short_id(text(&turn["threadId"], "?")),
                        short_id(text(&turn["turnId"], "?")),
                        text(&turn["status"], "unknown")
                    ),
                    width,
                ));
            }
        }
    }
    if let Some(pending) = pending {
        for action in pending.iter().take(4) {
            lines.push(row(
                &format!(
                    " ⚠ pending {} · resolve through ChatGPT operator",
                    text(&action["kind"], "action")
                ),
                width,
            ));
        }
    }

    lines.push(border('├', '─', '┤', width));
    lines.push(row(" LIVE ACTIVITY", width));
    let events = projection["events"].as_array();
    match events {
        Some(events) if !events.is_empty() => {
            for event in events.iter().rev().take(12).rev() {
                lines.push(row(&event_line(event), width));
            }
        }
        _ => lines.push(row(" no observed worker events yet", width)),
    }
    let cursor = projection["cursor"].as_u64().unwrap_or(0);
    let history = if projection["historyLost"].as_bool().unwrap_or(false) {
        " · older journal history evicted"
    } else {
        ""
    };
    lines.push(row(&format!(" journal cursor: {cursor}{history}"), width));
}

fn quota_line(label: &str, value: &Value) -> String {
    let used = value["usedPercent"]
        .as_f64()
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    let filled = ((used / 100.0) * 24.0).round() as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(24 - filled));
    let reset = value["resetsAt"]
        .as_u64()
        .map(reset_in)
        .unwrap_or_else(|| "reset unknown".into());
    format!(" {label:<3} {bar} {:>5.1}% used   {reset}", used)
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
        format!("resets in {days}d {hours}h")
    } else if hours > 0 {
        format!("resets in {hours}h {minutes}m")
    } else {
        format!("resets in {minutes}m")
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
    format!(" #{cursor:<5} {thread} {turn}  {method}")
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
        (100, 30)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_line_has_fixed_bar_width() {
        let value = serde_json::json!({"usedPercent": 50.0, "resetsAt": 0});
        let line = quota_line("5h", &value);
        assert!(line.contains("████████████░░░░░░░░░░░░"));
        assert!(line.contains("50.0% used"));
    }

    #[test]
    fn row_is_cropped_to_terminal_width() {
        assert_eq!(row(" abcdef", 6).chars().count(), 6);
    }
}
