use serde::Serialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, watch};

const MAX_EVENTS: usize = 512;
const MAX_EVENT_BYTES: usize = 128 * 1024;
const MAX_BATCH_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEvent {
    pub cursor: u64,
    pub method: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub params: Value,
    pub truncated: bool,
}

#[derive(Default)]
struct JournalState {
    cursor: u64,
    dropped_through: u64,
    events: VecDeque<(JournalEvent, usize)>,
}

#[derive(Clone)]
pub struct EventJournal {
    state: Arc<Mutex<JournalState>>,
    changed: watch::Sender<u64>,
}

impl Default for EventJournal {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(JournalState::default())),
            changed: watch::channel(0).0,
        }
    }
}

pub struct JournalBatch {
    pub events: Vec<JournalEvent>,
    pub cursor: u64,
    pub history_lost: bool,
}

impl EventJournal {
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub async fn push(&self, method: &str, params: &Value) {
        let size = serde_json::to_vec(params).map_or(MAX_EVENT_BYTES + 1, |v| v.len());
        let thread_id = string(params, "threadId")
            .or_else(|| params.get("thread").and_then(|t| string(t, "id")));
        let turn_id =
            string(params, "turnId").or_else(|| params.get("turn").and_then(|t| string(t, "id")));
        let truncated = size > MAX_EVENT_BYTES;
        let mut state = self.state.lock().await;
        state.cursor += 1;
        let event = JournalEvent {
            cursor: state.cursor,
            method: method.into(),
            thread_id,
            turn_id,
            truncated,
            params: if truncated {
                json!({"omittedBytes":size})
            } else {
                params.clone()
            },
        };
        let retained_bytes = serde_json::to_vec(&event).unwrap().len();
        state.events.push_back((event, retained_bytes));
        while state.events.len() > MAX_EVENTS {
            state.dropped_through = state.events.pop_front().unwrap().0.cursor;
        }
        self.changed.send_replace(state.cursor);
    }

    pub async fn mark_gap(&self) {
        let mut state = self.state.lock().await;
        state.cursor += 1;
        state.dropped_through = state.cursor;
        self.changed.send_replace(state.cursor);
    }

    pub async fn cursor(&self) -> u64 {
        self.state.lock().await.cursor
    }

    pub async fn read_after(
        &self,
        after: u64,
        thread_id: &str,
        turn_id: Option<&str>,
    ) -> Result<JournalBatch, String> {
        let state = self.state.lock().await;
        if after > state.cursor {
            return Err("cursor is ahead of this backend; after a backend restart, read again with afterCursor: 0".into());
        }
        let mut batch = JournalBatch {
            events: Vec::new(),
            cursor: after,
            history_lost: after < state.dropped_through,
        };
        let mut bytes = 0;
        for (event, size) in &state.events {
            if event.cursor <= after {
                continue;
            }
            let matches = event.thread_id.as_deref() == Some(thread_id)
                && turn_id.is_none_or(|id| event.turn_id.as_deref() == Some(id));
            if matches {
                if !batch.events.is_empty() && bytes + size > MAX_BATCH_BYTES {
                    return Ok(batch);
                }
                bytes += size;
                batch.events.push(event.clone());
            }
            batch.cursor = event.cursor;
        }
        batch.cursor = state.cursor;
        Ok(batch)
    }
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retention_filtering_and_oversized_events_preserve_cursors() {
        let journal = EventJournal::default();
        for n in 0..MAX_EVENTS + 20 {
            journal
                .push(
                    "turn/started",
                    &json!({"threadId":"a","turn":{"id":"one"},"n":n}),
                )
                .await;
        }
        let batch = journal.read_after(0, "a", Some("one")).await.unwrap();
        assert!(batch.history_lost);
        assert_eq!(batch.events.len(), MAX_EVENTS);
        assert_eq!(batch.cursor, (MAX_EVENTS + 20) as u64);
        journal
            .push(
                "turn/completed",
                &json!({"threadId":"b","turn":{"id":"two"},"data":"x".repeat(MAX_EVENT_BYTES)}),
            )
            .await;
        let batch = journal
            .read_after(batch.cursor, "b", Some("two"))
            .await
            .unwrap();
        assert_eq!(batch.events.len(), 1);
        assert!(batch.events[0].truncated);
        assert!(
            journal
                .read_after(0, "b", Some("one"))
                .await
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[tokio::test]
    async fn subscription_before_read_cannot_miss_a_notification() {
        let journal = EventJournal::default();
        let mut changed = journal.changes();
        assert!(
            journal
                .read_after(0, "a", None)
                .await
                .unwrap()
                .events
                .is_empty()
        );
        journal
            .push("item/completed", &json!({"threadId":"a"}))
            .await;
        changed.changed().await.unwrap();
        assert_eq!(
            journal.read_after(0, "a", None).await.unwrap().events.len(),
            1
        );
    }

    #[tokio::test]
    async fn batches_do_not_skip_events_at_the_output_limit() {
        let journal = EventJournal::default();
        for _ in 0..10 {
            journal
                .push(
                    "item/completed",
                    &json!({"threadId":"a","data":"x".repeat(100_000)}),
                )
                .await;
        }
        let mut cursor = 0;
        let mut count = 0;
        loop {
            let batch = journal.read_after(cursor, "a", None).await.unwrap();
            count += batch.events.len();
            cursor = batch.cursor;
            if cursor == journal.cursor().await {
                break;
            }
        }
        assert_eq!(count, 10);
        assert!(journal.read_after(cursor + 1, "a", None).await.is_err());
    }
}
