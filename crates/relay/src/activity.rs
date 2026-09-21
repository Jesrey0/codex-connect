use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Activity {
    pub kind: String,
    pub summary: Option<String>,
    pub phase: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticEvent {
    pub cursor: u64,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub kind: String,
    pub summary: Option<String>,
    pub phase: Option<String>,
}

pub(crate) fn semantic_event(cursor: u64, method: &str, params: &Value) -> Option<SemanticEvent> {
    let activity = activity(method, params)?;
    Some(SemanticEvent {
        cursor,
        thread_id: params
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::to_string),
        turn_id: params
            .get("turnId")
            .and_then(Value::as_str)
            .or_else(|| {
                params
                    .get("turn")
                    .and_then(|turn| turn.get("id"))
                    .and_then(Value::as_str)
            })
            .map(str::to_string),
        kind: activity.kind,
        summary: activity.summary,
        phase: activity.phase,
    })
}

pub(crate) fn activity(method: &str, params: &Value) -> Option<Activity> {
    let lowered = method.to_ascii_lowercase();
    if lowered.contains("token")
        || lowered.contains("ratelimit")
        || lowered.contains("rate_limit")
        || lowered.contains("usage")
        || lowered.contains("delta")
    {
        return None;
    }

    if method == "turn/started" {
        return Some(Activity {
            kind: "turn".into(),
            summary: Some("started".into()),
            phase: Some("started".into()),
        });
    }
    if method == "turn/completed" {
        let status = params["turn"]["status"].as_str().unwrap_or("completed");
        return Some(Activity {
            kind: "turn".into(),
            summary: Some(status.to_string()),
            phase: Some(
                if matches!(status, "failed" | "interrupted") {
                    "failed"
                } else {
                    "completed"
                }
                .into(),
            ),
        });
    }
    if lowered.contains("requestapproval")
        || lowered.contains("requestuserinput")
        || method == "mcpServer/elicitation/request"
    {
        return Some(Activity {
            kind: "waiting".into(),
            summary: Some("operator action required".into()),
            phase: None,
        });
    }
    if method == "codexConnect/threadUnsubscribed" {
        let status = params["status"].as_str().unwrap_or("completed");
        return Some(Activity {
            kind: "system".into(),
            summary: Some(format!("thread unsubscribed · {status}")),
            phase: Some("completed".into()),
        });
    }
    if method == "codexConnect/threadUnsubscribeFailed" {
        let error = params["error"].as_str().unwrap_or("unknown error");
        return Some(Activity {
            kind: "error".into(),
            summary: Some(format!(
                "thread unsubscribe failed · {}",
                compact_text(error, 240)
            )),
            phase: Some("failed".into()),
        });
    }

    if matches!(method, "item/started" | "item/completed") {
        let item = &params["item"];
        let phase = Some(if method == "item/started" {
            "started".into()
        } else {
            "completed".into()
        });
        return match item["type"].as_str().unwrap_or("item") {
            "reasoning" => Some(Activity {
                kind: "think".into(),
                summary: None,
                phase,
            }),
            "commandExecution" => Some(Activity {
                kind: "tool".into(),
                summary: item["command"]
                    .as_str()
                    .map(|value| compact_text(value, 320)),
                phase,
            }),
            "agentMessage" => Some(Activity {
                kind: "message".into(),
                summary: item["text"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(|value| compact_text(value, 240)),
                phase,
            }),
            "userMessage" => Some(Activity {
                kind: "message".into(),
                summary: message_content(item).map(|value| compact_text(&value, 240)),
                phase,
            }),
            "fileChange" => Some(Activity {
                kind: "file".into(),
                summary: Some("filesystem change".into()),
                phase,
            }),
            "mcpToolCall" => Some(Activity {
                kind: "tool".into(),
                summary: item["tool"]
                    .as_str()
                    .or_else(|| item["name"].as_str())
                    .map(|value| compact_text(value, 160)),
                phase,
            }),
            "webSearch" => Some(Activity {
                kind: "search".into(),
                summary: item["query"].as_str().map(|value| compact_text(value, 200)),
                phase,
            }),
            other => Some(Activity {
                kind: "item".into(),
                summary: Some(other.to_string()),
                phase,
            }),
        };
    }

    if lowered.contains("failed") || lowered.contains("error") {
        return Some(Activity {
            kind: "error".into(),
            summary: params["message"]
                .as_str()
                .or_else(|| params["error"]["message"].as_str())
                .map(|value| compact_text(value, 240)),
            phase: Some("failed".into()),
        });
    }

    None
}

pub(crate) fn compact_text(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let clipped = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

fn message_content(item: &Value) -> Option<String> {
    if let Some(text) = item["text"].as_str() {
        return Some(text.to_string());
    }
    let content = item["content"].as_array()?;
    let text = content
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reasoning_never_exposes_private_content() {
        let event = semantic_event(
            7,
            "item/completed",
            &json!({"item":{"type":"reasoning","summary":"private reasoning"}}),
        )
        .unwrap();
        assert_eq!(event.kind, "think");
        assert_eq!(event.summary, None);
    }

    #[test]
    fn message_deltas_are_not_semantic_events() {
        assert!(semantic_event(1, "item/agentMessage/delta", &json!({"delta":"hello"}),).is_none());
    }

    #[test]
    fn terminal_failure_statuses_use_failed_phase() {
        for status in ["failed", "interrupted"] {
            let event = semantic_event(
                1,
                "turn/completed",
                &json!({"threadId":"a","turn":{"id":"one","status":status}}),
            )
            .unwrap();
            assert_eq!(event.summary.as_deref(), Some(status));
            assert_eq!(event.phase.as_deref(), Some("failed"));
        }
    }

    #[test]
    fn elicitation_is_an_operator_wait_state() {
        let event = semantic_event(
            3,
            "mcpServer/elicitation/request",
            &json!({"threadId":"a","turnId":"one","message":"Authorize"}),
        )
        .unwrap();
        assert_eq!(event.kind, "waiting");
        assert_eq!(event.summary.as_deref(), Some("operator action required"));
    }
}
