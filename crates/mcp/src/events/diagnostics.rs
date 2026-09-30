//! Process-local, bounded evidence. Only explicitly selected fields reach logs.
use super::*;
use std::collections::VecDeque;

const MAX_RECENT: usize = 128;

#[derive(Default, Serialize)]
pub(super) struct Diagnostics {
    counters: BTreeMap<&'static str, u64>,
    recent: VecDeque<Value>,
}

impl Events {
    pub(super) fn record(&self, stage: &'static str, mut fields: Value) {
        fields["stage"] = json!(stage);
        fields["timestampMs"] = json!(now_ms());
        let mut diagnostics = self
            .0
            .diagnostics
            .lock()
            .expect("Events diagnostics poisoned");
        let count = diagnostics.counters.entry(stage).or_default();
        *count = count.saturating_add(1);
        diagnostics.recent.push_back(fields.clone());
        if diagnostics.recent.len() > MAX_RECENT {
            diagnostics.recent.pop_front();
        }
        eprintln!("mcp_events {fields}");
    }

    pub(super) fn record_subscription(
        &self,
        stage: &'static str,
        sub: &Subscription,
        outcome: Option<&'static str>,
        status: Option<u16>,
    ) {
        self.record(
            stage,
            json!({
                "subscriptionId": sub.id,
                "threadId": sub.filters.thread_id,
                "turnId": sub.filters.turn_id,
                "eventId": sub.delivery.as_ref().map(|d| &d.event_id),
                "attempt": sub.delivery.as_ref().map(|d| d.attempts),
                "outcome": outcome,
                "httpStatus": status,
            }),
        );
    }

    pub(crate) fn subscription_received(&self) -> Arc<SubscriptionReceipt> {
        let id = self.0.next_receipt.fetch_add(1, Ordering::Relaxed);
        self.record("subscriptionReceived", json!({"requestId":id}));
        Arc::new(SubscriptionReceipt {
            events: self.clone(),
            id,
            finished: AtomicBool::new(false),
        })
    }
}

// Shared with the request extensions; cancellation or dispatch failure cannot
// silently erase evidence that the HTTP boundary received a subscribe request.
pub(crate) struct SubscriptionReceipt {
    events: Events,
    id: u64,
    finished: AtomicBool,
}

impl SubscriptionReceipt {
    pub(crate) fn rejected(&self, reason: &'static str, code: Option<i32>) {
        if !self.finished.swap(true, Ordering::Relaxed) {
            self.events.record(
                "subscriptionRejected",
                json!({
                    "requestId": self.id, "reason": reason, "errorCode": code,
                }),
            );
        }
    }

    pub(crate) fn finish(&self, result: &Result<Value, McpError>) {
        match result {
            Ok(value) => {
                if !self.finished.swap(true, Ordering::Relaxed) {
                    self.events.record(
                        "subscriptionAccepted",
                        json!({
                            "requestId": self.id, "subscriptionId": value["id"],
                        }),
                    );
                }
            }
            Err(error) => {
                let reason = match error.code.0 {
                    -32602 => "invalidSubscriptionOrTurn",
                    -32001 => "authorization",
                    -32015 => "callbackVerification",
                    -32000 => "capacityOrRotation",
                    _ => "internalOrTimeout",
                };
                self.rejected(reason, Some(error.code.0));
            }
        }
    }
}

impl Drop for SubscriptionReceipt {
    fn drop(&mut self) {
        self.rejected("cancelledOrUndispatched", None);
    }
}
