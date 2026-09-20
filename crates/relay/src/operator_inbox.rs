use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;

const MAX_EVENTS: usize = 256;
pub(crate) const MAX_DELIVERY: usize = 8;

#[derive(Default)]
struct InboxState {
    events: VecDeque<(String, Value)>,
    terminal_seen: HashSet<String>,
    terminal_order: VecDeque<String>,
    pending_seen: HashSet<String>,
    overflowed: bool,
}

fn remember_terminal(state: &mut InboxState, key: &str) {
    if !state.terminal_seen.insert(key.to_string()) {
        return;
    }
    state.terminal_order.push_back(key.to_string());
    while state.terminal_order.len() > MAX_EVENTS * 2 {
        if let Some(oldest) = state.terminal_order.pop_front() {
            state.terminal_seen.remove(&oldest);
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct OperatorInbox {
    state: Arc<Mutex<InboxState>>,
}

impl OperatorInbox {
    pub async fn push_terminal(&self, key: String, event: Value) {
        let mut state = self.state.lock().await;
        if state.terminal_seen.contains(&key) {
            return;
        }
        remember_terminal(&mut state, &key);
        push(&mut state, key, event);
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
                push(&mut state, key, event);
            }
        }
    }

    pub async fn acknowledge_terminal(&self, key: String) {
        let mut state = self.state.lock().await;
        remember_terminal(&mut state, &key);
        state.events.retain(|(candidate, _)| candidate != &key);
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
        while events.len() < MAX_DELIVERY {
            let Some((_, event)) = state.events.pop_front() else {
                break;
            };
            events.push(event);
        }
        events
    }
}

fn push(state: &mut InboxState, key: String, event: Value) {
    state.events.push_back((key, event));
    while state.events.len() > MAX_EVENTS {
        state.events.pop_front();
        state.overflowed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_events_are_delivered_once() {
        let inbox = OperatorInbox::default();
        inbox
            .push_terminal(
                "turn:a:b".into(),
                json!({"kind":"turnTerminal","turnId":"b"}),
            )
            .await;
        inbox
            .push_terminal(
                "turn:a:b".into(),
                json!({"kind":"turnTerminal","turnId":"b"}),
            )
            .await;
        assert_eq!(inbox.take().await.len(), 1);
        assert!(inbox.take().await.is_empty());
    }

    #[tokio::test]
    async fn unresolved_actions_are_not_repeated_after_delivery() {
        let inbox = OperatorInbox::default();
        let action = || {
            vec![(
                "action:1".into(),
                json!({"kind":"actionRequired","requestId":1}),
            )]
        };
        inbox.sync_actions(action()).await;
        assert_eq!(inbox.take().await.len(), 1);
        inbox.sync_actions(action()).await;
        assert!(inbox.take().await.is_empty());
        inbox.sync_actions(Vec::new()).await;
        inbox.sync_actions(action()).await;
        assert_eq!(inbox.take().await.len(), 1);
    }

    #[tokio::test]
    async fn resolved_actions_are_removed_before_queued_delivery() {
        let inbox = OperatorInbox::default();
        for index in 0..MAX_DELIVERY {
            inbox
                .push_terminal(
                    format!("turn:a:{index}"),
                    json!({"kind":"turnTerminal","turnId":index.to_string()}),
                )
                .await;
        }
        inbox
            .sync_actions(vec![(
                "action:1".into(),
                json!({"kind":"actionRequired","requestId":1}),
            )])
            .await;

        assert_eq!(inbox.take().await.len(), MAX_DELIVERY);
        inbox.sync_actions(Vec::new()).await;
        assert!(inbox.take().await.is_empty());
    }

    #[tokio::test]
    async fn explicit_join_acknowledges_terminal_and_pending_events() {
        let inbox = OperatorInbox::default();
        inbox
            .push_terminal(
                "turn:a:b".into(),
                json!({"kind":"turnTerminal","turnId":"b"}),
            )
            .await;
        inbox
            .sync_actions(vec![(
                "action:1".into(),
                json!({"kind":"actionRequired","requestId":1}),
            )])
            .await;

        inbox.acknowledge_terminal("turn:a:b".into()).await;
        inbox.acknowledge_actions(vec!["action:1".into()]).await;
        assert!(inbox.take().await.is_empty());

        inbox
            .push_terminal(
                "turn:a:b".into(),
                json!({"kind":"turnTerminal","turnId":"b"}),
            )
            .await;
        inbox
            .sync_actions(vec![(
                "action:1".into(),
                json!({"kind":"actionRequired","requestId":1}),
            )])
            .await;
        assert!(inbox.take().await.is_empty());
    }

    #[tokio::test]
    async fn history_gap_is_delivered_as_a_semantic_interrupt() {
        let inbox = OperatorInbox::default();
        inbox.mark_history_lost().await;
        assert_eq!(inbox.take().await, vec![json!({"kind":"historyLost"})]);
        assert!(inbox.take().await.is_empty());
    }
}
