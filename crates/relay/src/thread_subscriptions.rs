use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::futures::OwnedNotified;

#[derive(Default)]
pub(super) struct ThreadSubscriptions {
    threads: HashMap<String, ThreadSubscription>,
}

struct ThreadSubscription {
    starting: usize,
    active_turns: HashSet<String>,
    subscribed: bool,
    unsubscribing: bool,
    notify: Arc<Notify>,
}

impl Default for ThreadSubscription {
    fn default() -> Self {
        Self {
            starting: 0,
            active_turns: HashSet::new(),
            subscribed: false,
            unsubscribing: false,
            notify: Arc::new(Notify::new()),
        }
    }
}

impl ThreadSubscriptions {
    pub(super) fn is_subscribed(&self, thread_id: &str) -> bool {
        self.threads
            .get(thread_id)
            .is_some_and(|state| state.subscribed && !state.unsubscribing)
    }

    pub(super) fn begin_start(&mut self, thread_id: &str) -> Option<OwnedNotified> {
        let state = self.threads.entry(thread_id.to_string()).or_default();
        if state.unsubscribing {
            return Some(state.notify.clone().notified_owned());
        }
        state.starting += 1;
        None
    }

    pub(super) fn mark_subscribed(&mut self, thread_id: &str) {
        self.threads
            .entry(thread_id.to_string())
            .or_default()
            .subscribed = true;
    }

    pub(super) fn finish_start(&mut self, thread_id: &str, turn_id: Option<&str>) -> bool {
        let (should_unsubscribe, remove_inactive) = {
            let Some(state) = self.threads.get_mut(thread_id) else {
                return false;
            };
            state.starting = state.starting.saturating_sub(1);
            if let Some(turn_id) = turn_id {
                state.active_turns.insert(turn_id.to_string());
            }
            let should_unsubscribe = Self::claim_unsubscribe(state);
            let remove_inactive = !state.subscribed
                && !state.unsubscribing
                && state.starting == 0
                && state.active_turns.is_empty();
            (should_unsubscribe, remove_inactive)
        };
        if remove_inactive {
            self.threads.remove(thread_id);
        }
        should_unsubscribe
    }

    pub(super) fn finish_turn(&mut self, thread_id: &str, turn_id: &str) -> bool {
        let Some(state) = self.threads.get_mut(thread_id) else {
            return false;
        };
        state.active_turns.remove(turn_id);
        Self::claim_unsubscribe(state)
    }

    fn claim_unsubscribe(state: &mut ThreadSubscription) -> bool {
        if state.subscribed
            && !state.unsubscribing
            && state.starting == 0
            && state.active_turns.is_empty()
        {
            state.unsubscribing = true;
            true
        } else {
            false
        }
    }

    pub(super) fn finish_unsubscribe(
        &mut self,
        thread_id: &str,
        success: bool,
    ) -> Option<Arc<Notify>> {
        let state = self.threads.get_mut(thread_id)?;
        state.unsubscribing = false;
        let notify = state.notify.clone();
        if success {
            self.threads.remove(thread_id);
        }
        Some(notify)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_terminal_event_releases_subscription_after_start_registration() {
        let mut subscriptions = ThreadSubscriptions::default();
        assert!(subscriptions.begin_start("thread").is_none());
        subscriptions.mark_subscribed("thread");
        assert!(!subscriptions.finish_turn("thread", "turn"));
        assert!(!subscriptions.finish_start("thread", Some("turn")));
        assert!(subscriptions.finish_turn("thread", "turn"));
        assert!(!subscriptions.finish_turn("thread", "turn"));
        subscriptions.finish_unsubscribe("thread", true);
        assert!(!subscriptions.is_subscribed("thread"));
    }
}
