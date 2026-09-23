use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;

const MAX_EVENTS: usize = 256;
const MAX_START_RECEIPTS: usize = 256;
pub(crate) const MAX_DELIVERY: usize = 8;

#[derive(Default)]
struct NotificationState {
    events: VecDeque<(String, Value)>,
    start_receipts: VecDeque<(String, Value)>,
    once_seen: HashSet<String>,
    once_order: VecDeque<String>,
    pending_seen: HashSet<String>,
    overflowed: bool,
}

fn remember_once(state: &mut NotificationState, key: &str) {
    if !state.once_seen.insert(key.to_string()) {
        return;
    }
    state.once_order.push_back(key.to_string());
    while state.once_order.len() > MAX_EVENTS * 2 {
        if let Some(oldest) = state.once_order.pop_front() {
            state.once_seen.remove(&oldest);
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct WorkerNotifications {
    state: Arc<Mutex<NotificationState>>,
}

impl WorkerNotifications {
    pub async fn push_once(&self, key: String, event: Value) {
        let mut state = self.state.lock().await;
        if state.once_seen.contains(&key) {
            return;
        }
        remember_once(&mut state, &key);
        if is_start_receipt(&event) {
            push_start_receipt(&mut state, key, event);
            return;
        }
        push_one_shot(&mut state, key, event);
    }

    pub async fn sync_actions(&self, actions: Vec<(String, Value)>) {
        let mut state = self.state.lock().await;
        let current = actions
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<HashSet<_>>();
        state.events.retain(|(key, event)| {
            event.get("kind").and_then(Value::as_str) != Some("actionRequired")
                || current.contains(key)
        });
        state.pending_seen.retain(|key| current.contains(key));
        for (key, event) in actions {
            if state.pending_seen.insert(key.clone()) {
                push_one_shot(&mut state, key, event);
            }
        }
    }

    pub async fn acknowledge_once(&self, key: String) {
        let mut state = self.state.lock().await;
        remember_once(&mut state, &key);
        state.events.retain(|(candidate, _)| candidate != &key);
        state
            .start_receipts
            .retain(|(candidate, _)| candidate != &key);
    }

    pub async fn acknowledge_actions(&self, keys: Vec<String>) {
        if keys.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        for key in &keys {
            state.pending_seen.insert(key.clone());
        }
        state.events.retain(|(key, _)| !keys.contains(key));
    }

    pub async fn mark_history_lost(&self) {
        self.state.lock().await.overflowed = true;
    }

    pub async fn take(&self) -> Vec<Value> {
        let mut state = self.state.lock().await;
        let mut events = Vec::with_capacity(MAX_DELIVERY);
        if state.overflowed {
            events.push(json!({"kind":"historyLost"}));
            state.overflowed = false;
        }

        let available = MAX_DELIVERY.saturating_sub(events.len());
        if available == 0 {
            return events;
        }

        let mut selected = state
            .events
            .iter()
            .enumerate()
            .map(|(index, (_, event))| (delivery_priority(event), index))
            .collect::<Vec<_>>();
        selected.sort_unstable();
        selected.truncate(available);

        let selected_indices = selected
            .iter()
            .map(|(_, index)| *index)
            .collect::<HashSet<_>>();
        for (_, index) in &selected {
            events.push(state.events[*index].1.clone());
        }

        let mut retained = VecDeque::with_capacity(state.events.len());
        for (index, item) in state.events.drain(..).enumerate() {
            if !selected_indices.contains(&index) {
                retained.push_back(item);
            }
        }
        state.events = retained;

        let receipt_slots = MAX_DELIVERY.saturating_sub(events.len());
        let receipt_count = receipt_slots.min(state.start_receipts.len());
        for _ in 0..receipt_count {
            let receipt = state.start_receipts.pop_front().unwrap();
            events.push(receipt.1.clone());
            // A start receipt may be the only way to recover a worker whose
            // codex.start response was lost. Rotate it after delivery so
            // multiple unclaimed starts remain fair, and keep replaying until
            // a known-turn operation explicitly acknowledges the handle.
            state.start_receipts.push_back(receipt);
        }
        events
    }
}

fn delivery_priority(event: &Value) -> u8 {
    match event.get("kind").and_then(Value::as_str) {
        Some("actionRequired") => 0,
        Some("turnTerminal") => 1,
        _ => 2,
    }
}

fn is_start_receipt(event: &Value) -> bool {
    event.get("kind").and_then(Value::as_str) == Some("workerStarted")
}

fn push_one_shot(state: &mut NotificationState, key: String, event: Value) {
    state.events.push_back((key, event));
    while state.events.len() > MAX_EVENTS {
        state.events.pop_front();
        state.overflowed = true;
    }
}

fn push_start_receipt(state: &mut NotificationState, key: String, event: Value) {
    state.start_receipts.push_back((key, event));
    while state.start_receipts.len() > MAX_START_RECEIPTS {
        state.start_receipts.pop_front();
        state.overflowed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_events_are_delivered_once() {
        let notifications = WorkerNotifications::default();
        notifications
            .push_once(
                "turn:a:b".into(),
                json!({"kind":"turnTerminal","turnId":"b"}),
            )
            .await;
        notifications
            .push_once(
                "turn:a:b".into(),
                json!({"kind":"turnTerminal","turnId":"b"}),
            )
            .await;
        assert_eq!(notifications.take().await.len(), 1);
        assert!(notifications.take().await.is_empty());
    }

    #[tokio::test]
    async fn started_events_replay_until_acknowledged() {
        let notifications = WorkerNotifications::default();
        let event = json!({
            "kind":"workerStarted",
            "threadId":"a",
            "turnId":"b",
            "mode":"work"
        });
        notifications
            .push_once("started:a:b".into(), event.clone())
            .await;

        assert_eq!(notifications.take().await, vec![event.clone()]);
        assert_eq!(notifications.take().await, vec![event]);

        notifications.acknowledge_once("started:a:b".into()).await;
        assert!(notifications.take().await.is_empty());
    }

    #[tokio::test]
    async fn required_actions_and_terminal_events_precede_start_receipts() {
        let notifications = WorkerNotifications::default();
        for index in 0..MAX_DELIVERY {
            notifications
                .push_once(
                    format!("started:a:{index}"),
                    json!({
                        "kind":"workerStarted",
                        "threadId":"a",
                        "turnId":index.to_string(),
                        "mode":"work"
                    }),
                )
                .await;
        }
        notifications
            .push_once(
                "turn:a:terminal".into(),
                json!({"kind":"turnTerminal","turnId":"terminal"}),
            )
            .await;
        notifications
            .sync_actions(vec![(
                "action:1".into(),
                json!({"kind":"actionRequired","requestId":1}),
            )])
            .await;

        let delivered = notifications.take().await;
        assert_eq!(delivered.len(), MAX_DELIVERY);
        assert_eq!(delivered[0]["kind"], "actionRequired");
        assert_eq!(delivered[1]["kind"], "turnTerminal");
    }

    #[tokio::test]
    async fn start_receipts_are_bounded_and_overflow_is_visible() {
        let notifications = WorkerNotifications::default();
        for index in 0..=MAX_START_RECEIPTS {
            notifications
                .push_once(
                    format!("started:a:{index}"),
                    json!({
                        "kind":"workerStarted",
                        "threadId":"a",
                        "turnId":index.to_string(),
                        "mode":"work"
                    }),
                )
                .await;
        }
        let state = notifications.state.lock().await;
        assert_eq!(state.start_receipts.len(), MAX_START_RECEIPTS);
        assert!(
            !state
                .start_receipts
                .iter()
                .any(|(key, _)| key == "started:a:0")
        );
        drop(state);
        assert_eq!(notifications.take().await[0]["kind"], "historyLost");
    }

    #[tokio::test]
    async fn claimed_start_receipts_are_not_reintroduced_by_duplicate_push() {
        let notifications = WorkerNotifications::default();
        let receipt: (String, Value) = (
            "started:a:b".into(),
            json!({
                "kind":"workerStarted",
                "threadId":"a",
                "turnId":"b",
                "mode":"work"
            }),
        );
        notifications
            .push_once(receipt.0.clone(), receipt.1.clone())
            .await;
        notifications.acknowledge_once(receipt.0.clone()).await;
        notifications.push_once(receipt.0, receipt.1).await;

        assert!(notifications.take().await.is_empty());
    }

    #[tokio::test]
    async fn unresolved_actions_are_not_repeated_after_delivery() {
        let notifications = WorkerNotifications::default();
        let action = || {
            vec![(
                "action:1".into(),
                json!({"kind":"actionRequired","requestId":1}),
            )]
        };
        notifications.sync_actions(action()).await;
        assert_eq!(notifications.take().await.len(), 1);
        notifications.sync_actions(action()).await;
        assert!(notifications.take().await.is_empty());
        notifications.sync_actions(Vec::new()).await;
        notifications.sync_actions(action()).await;
        assert_eq!(notifications.take().await.len(), 1);
    }

    #[tokio::test]
    async fn resolved_actions_are_removed_before_delivery() {
        let notifications = WorkerNotifications::default();
        notifications
            .sync_actions(vec![(
                "action:1".into(),
                json!({"kind":"actionRequired","requestId":1}),
            )])
            .await;
        notifications.sync_actions(Vec::new()).await;
        assert!(notifications.take().await.is_empty());
    }

    #[tokio::test]
    async fn explicit_join_acknowledges_terminal_and_pending_events() {
        let notifications = WorkerNotifications::default();
        notifications
            .push_once(
                "turn:a:b".into(),
                json!({"kind":"turnTerminal","turnId":"b"}),
            )
            .await;
        notifications
            .sync_actions(vec![(
                "action:1".into(),
                json!({"kind":"actionRequired","requestId":1}),
            )])
            .await;

        notifications.acknowledge_once("turn:a:b".into()).await;
        notifications
            .acknowledge_actions(vec!["action:1".into()])
            .await;
        assert!(notifications.take().await.is_empty());

        notifications
            .push_once(
                "turn:a:b".into(),
                json!({"kind":"turnTerminal","turnId":"b"}),
            )
            .await;
        notifications
            .sync_actions(vec![(
                "action:1".into(),
                json!({"kind":"actionRequired","requestId":1}),
            )])
            .await;
        assert!(notifications.take().await.is_empty());
    }

    #[tokio::test]
    async fn history_gap_is_delivered_as_a_semantic_interrupt() {
        let notifications = WorkerNotifications::default();
        notifications.mark_history_lost().await;
        assert_eq!(
            notifications.take().await,
            vec![json!({"kind":"historyLost"})]
        );
        assert!(notifications.take().await.is_empty());
    }
}
