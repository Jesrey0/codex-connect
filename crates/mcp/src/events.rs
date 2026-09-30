//! Single-user terminal subscriptions and a bounded durable webhook outbox.
mod ingress;
#[cfg(test)]
mod tests;
mod webhook;
pub use ingress::{Authorization, CONTEXT_HEADER, Ingress};

use anyhow::{Context, Result, bail};
use codex_connect_host::storage::atomic_write;
use codex_connect_relay::{Relay, TerminalTurn};
use rmcp::ErrorData as McpError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const EVENT_NAME: &str = "codex.turn.terminal";
const MAX_SUBSCRIPTIONS: usize = 128;
const MAX_STORE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ATTEMPTS: u32 = 8;
const MIN_TTL_MS: u64 = 60_000;
const DEFAULT_TTL_MS: u64 = 3_600_000;
const MAX_TTL_MS: u64 = 86_400_000;
const RETENTION_MS: u64 = 86_400_000;
const VERIFY_CACHE_MS: u64 = 300_000;
const ROTATION_MS: u64 = 300_000;
const AUTH_INTERVAL_MS: u64 = 30_000;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Filters {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Destination {
    mode: String,
    url: String,
    secret: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubscriptionRequest {
    name: String,
    arguments: Filters,
    delivery: Destination,
    #[serde(default, deserialize_with = "deserialize_ttl")]
    ttl_ms: TtlMs,
    cursor: Option<String>,
}

#[derive(Clone, Copy, Default)]
enum TtlMs {
    #[default]
    Default,
    Finite(u64),
    NonExpiring,
}

fn deserialize_ttl<'de, D>(deserializer: D) -> Result<TtlMs, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer)
        .map(|ttl| ttl.map_or(TtlMs::NonExpiring, TtlMs::Finite))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnsubscribeRequest {
    name: String,
    arguments: Filters,
    delivery: Destination,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delivery {
    event_id: String,
    bytes: String,
    attempts: u32,
    next_attempt_ms: u64,
    state: String,
}

// No Debug on any structure containing a destination, context, or key.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscription {
    id: String,
    authorization: Authorization,
    filters: Filters,
    url: String,
    secret: String,
    old_secret: Option<String>,
    rotation_until_ms: u64,
    expires_ms: Option<u64>,
    retired_ms: Option<u64>,
    verified_until_ms: u64,
    auth_next_ms: u64,
    state: String,
    #[serde(skip)]
    needs_observation: bool,
    delivery: Option<Delivery>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    subscriptions: BTreeMap<String, Subscription>,
    overflows: u64,
}

struct Inner {
    path: PathBuf,
    _lock: codex_connect_host::storage::ExclusiveLock,
    store: Mutex<Store>,
    failed: AtomicBool,
    admission: tokio::sync::Mutex<()>,
    ingress: Ingress,
    webhook: webhook::Webhook,
}

#[derive(Clone)]
pub struct Events(Arc<Inner>);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before Unix epoch")
        .as_millis() as u64
}
fn iso(ms: u64) -> Result<String> {
    Ok(chrono::DateTime::from_timestamp_millis(i64::try_from(ms)?)
        .context("invalid event timestamp")?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
fn digest(prefix: &str, value: &Value) -> String {
    format!(
        "{prefix}{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("JSON serializes"))
    )
}
fn identity(auth: &Authorization, filters: &Filters, url: &str) -> String {
    // Tuple serialization is canonical and unambiguous; no grant/chat dimension.
    digest(
        "sub_",
        &json!([
            auth.principal,
            url,
            EVENT_NAME,
            filters.thread_id,
            filters.turn_id
        ]),
    )
}
fn invalid(message: &str) -> McpError {
    McpError::invalid_params(message.to_owned(), None)
}
fn internal() -> McpError {
    McpError::internal_error("Events operation failed", None)
}
fn callback_error(reason: &str) -> McpError {
    McpError::new(
        rmcp::model::ErrorCode(-32015),
        "CallbackEndpointError",
        Some(json!({"reason":reason})),
    )
}
fn auth_error() -> McpError {
    McpError::new(
        rmcp::model::ErrorCode(-32001),
        "Events authorization denied or unavailable",
        None,
    )
}
fn live_state(state: &str) -> bool {
    matches!(state, "active" | "paused" | "verifying")
}
fn retire(sub: &mut Subscription, state: &str) {
    sub.retired_ms.get_or_insert_with(now_ms);
    sub.state = state.to_owned();
    sub.secret.clear();
    sub.old_secret = None;
    sub.verified_until_ms = 0;
    if let Some(delivery) = &mut sub.delivery
        && delivery.state == "pending"
    {
        delivery.state = state.to_owned();
    }
}
fn retention_deadline(sub: &Subscription) -> Option<u64> {
    sub.expires_ms
        .or(sub.retired_ms)
        .map(|timestamp| timestamp.saturating_add(RETENTION_MS))
}
fn validate(name: &str, filters: &Filters, delivery: &Destination) -> Result<(), McpError> {
    if name != EVENT_NAME || delivery.mode != "webhook" {
        return Err(invalid("unsupported event or delivery mode"));
    }
    for id in [&filters.thread_id, &filters.turn_id] {
        if id.is_empty() || id.len() > 256 || id.trim() != id || id.chars().any(char::is_control) {
            return Err(invalid("threadId and turnId must be exact canonical IDs"));
        }
    }
    webhook::callback_url(&delivery.url).map_err(|_| callback_error("unsafe_url"))?;
    Ok(())
}

impl Events {
    pub fn open(directory: PathBuf) -> Result<Self> {
        Self::with_network(directory, Ingress::local()?, webhook::Webhook::default())
    }

    fn with_network(
        directory: PathBuf,
        ingress: Ingress,
        webhook: webhook::Webhook,
    ) -> Result<Self> {
        if directory.exists() && fs::symlink_metadata(&directory)?.file_type().is_symlink() {
            bail!("Events directory cannot be a symlink");
        }
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("store.lock"))?;
        let lock = codex_connect_host::storage::lock_exclusive(lock, true)
            .context("Events store is already owned by another process")?;
        let path = directory.join("store.json");
        let mut store: Store = if path.exists() {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            if file.metadata()?.permissions().mode() & 0o077 != 0 {
                bail!("Events store must be private");
            }
            if file.metadata()?.len() > MAX_STORE_BYTES {
                bail!("Events store exceeds bound");
            }
            serde_json::from_reader(file).context("invalid Events store")?
        } else {
            Store::default()
        };
        if store.subscriptions.len() > MAX_SUBSCRIPTIONS {
            bail!("Events subscription bound exceeded");
        }
        let now = now_ms();
        for sub in store.subscriptions.values_mut() {
            if sub.state == "verifying" {
                retire(sub, "verificationFailed");
            } else if live_state(&sub.state) {
                if sub.expires_ms.is_some_and(|expires| expires <= now) {
                    retire(sub, "expired");
                } else {
                    sub.state = "paused".into();
                    sub.auth_next_ms = 0;
                    sub.needs_observation = true;
                }
            }
            // Preserve pending IDs/attempts, but never trust persisted admission.
        }
        let events = Self(Arc::new(Inner {
            path,
            _lock: lock,
            store: Mutex::new(store),
            failed: AtomicBool::new(false),
            admission: tokio::sync::Mutex::new(()),
            ingress,
            webhook,
        }));
        events.update(|_| Ok(()))?;
        Ok(events)
    }

    fn update<T>(&self, change: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        if self.0.failed.load(Ordering::Acquire) {
            bail!("Events storage failed");
        }
        let mut current = self
            .0
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("Events lock poisoned"))?;
        let mut store = current.clone();
        let result = change(&mut store)?;
        let bytes = serde_json::to_vec(&store)?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            bail!("Events store exceeds bound");
        }
        // Recovery flags are transient; preserve them in memory without rewriting
        // an unchanged durable record on every deadline tick or duplicate fact.
        if self.0.path.is_file() && bytes == serde_json::to_vec(&*current)? {
            *current = store;
            return Ok(result);
        }
        if atomic_write(&self.0.path, ".events-", 0o600, &bytes).is_err() {
            self.0.failed.store(true, Ordering::Release);
            bail!("Events storage failed");
        }
        *current = store;
        Ok(result)
    }

    pub async fn authenticate(&self, context: &str) -> Result<Authorization, McpError> {
        self.0
            .ingress
            .request(context)
            .await
            .map_err(|_| auth_error())?
            .ok_or_else(auth_error)
    }

    pub fn catalog() -> Value {
        json!({"events":[{"name":EVENT_NAME,
            "description":"An authoritative Codex turn reached completed, interrupted, or failed status. Use codex.inspect for its result.",
            "delivery":["webhook"],
            "inputSchema":{"type":"object","properties":{"threadId":{"type":"string"},"turnId":{"type":"string"}},
                "required":["threadId","turnId"],"additionalProperties":false},
                "payloadSchema":{"type":"object","properties":{"threadId":{"type":"string"},"turnId":{"type":"string"},
                "status":{"type":"string","enum":["completed","interrupted","failed"]}},
                "required":["threadId","turnId","status"],"additionalProperties":false}}]})
    }

    pub async fn dispatch(
        &self,
        method: &str,
        mut params: Value,
        auth: Authorization,
        relay: &Relay,
    ) -> Result<Value, McpError> {
        if let Some(object) = params.as_object_mut() {
            object.remove("_meta");
        }
        match method {
            "events/list" => {
                if params != json!({}) && params != json!({"cursor":null}) {
                    return Err(invalid("event catalog has no cursor"));
                }
                Ok(Self::catalog())
            }
            "events/subscribe" => {
                let request: SubscriptionRequest =
                    serde_json::from_value(params).map_err(|_| invalid("invalid subscription"))?;
                self.subscribe(request, auth, |filters| async move {
                    relay
                        .watch_terminal(&filters.thread_id, &filters.turn_id)
                        .await
                        .map_err(|_| invalid("canonical threadId/turnId not found or unavailable"))
                })
                .await
            }
            "events/unsubscribe" => {
                let request: UnsubscribeRequest =
                    serde_json::from_value(params).map_err(|_| invalid("invalid cancellation"))?;
                self.cancel(request, &auth).await
            }
            _ => Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                "Method not found",
                None,
            )),
        }
    }

    async fn cancel(
        &self,
        request: UnsubscribeRequest,
        auth: &Authorization,
    ) -> Result<Value, McpError> {
        validate(&request.name, &request.arguments, &request.delivery)?;
        if request.delivery.secret.is_some() {
            return Err(invalid("unsubscribe does not take a signing secret"));
        }
        let _admission = tokio::time::timeout(Duration::from_secs(10), self.0.admission.lock())
            .await
            .map_err(|_| internal())?;
        let id = identity(auth, &request.arguments, &request.delivery.url);
        self.update(|store| {
            if let Some(sub) = store.subscriptions.get_mut(&id) {
                retire(sub, "cancelled");
            }
            Ok(())
        })
        .map_err(|_| internal())?;
        Ok(json!({}))
    }

    async fn subscribe<F, Fut>(
        &self,
        request: SubscriptionRequest,
        auth: Authorization,
        validate_turn: F,
    ) -> Result<Value, McpError>
    where
        F: FnOnce(Filters) -> Fut,
        Fut: std::future::Future<Output = Result<(), McpError>>,
    {
        validate(&request.name, &request.arguments, &request.delivery)?;
        if request.cursor.is_some() {
            return Err(invalid("terminal events do not support replay cursors"));
        }
        let secret = request
            .delivery
            .secret
            .clone()
            .ok_or_else(|| invalid("signing secret required"))?;
        webhook::key(&secret).map_err(|_| invalid("invalid signing secret"))?;
        let _admission = tokio::time::timeout(Duration::from_secs(10), self.0.admission.lock())
            .await
            .map_err(|_| internal())?;
        if !self
            .0
            .ingress
            .valid(&auth)
            .await
            .map_err(|_| auth_error())?
        {
            return Err(auth_error());
        }
        let now = now_ms();
        let ttl_ms = match request.ttl_ms {
            TtlMs::Default => Some(DEFAULT_TTL_MS),
            TtlMs::Finite(ttl) => Some(ttl.clamp(MIN_TTL_MS, MAX_TTL_MS)),
            TtlMs::NonExpiring => None,
        };
        let expires_ms = ttl_ms.map(|ttl| now.saturating_add(ttl));
        let id = identity(&auth, &request.arguments, &request.delivery.url);
        let filters = request.arguments;
        let capacity = self
            .update(|store| {
                store.subscriptions.retain(|_, sub| {
                    live_state(&sub.state)
                        || retention_deadline(sub).is_some_and(|until| until > now)
                });
                if !store.subscriptions.contains_key(&id)
                    && store.subscriptions.len() >= MAX_SUBSCRIPTIONS
                {
                    store.overflows = store.overflows.saturating_add(1);
                    return Ok(false);
                }
                Ok(true)
            })
            .map_err(|_| internal())?;
        if !capacity {
            return Err(McpError::new(
                rmcp::model::ErrorCode(-32000),
                "Events subscription capacity exceeded",
                None,
            ));
        }
        let (verified, sub) = self
            .update(|store| {
                store.subscriptions.retain(|_, sub| {
                    live_state(&sub.state)
                        || retention_deadline(sub).is_some_and(|until| until > now)
                });
                if !store.subscriptions.contains_key(&id)
                    && store.subscriptions.len() >= MAX_SUBSCRIPTIONS
                {
                    bail!("subscription queue full");
                }
                let existing = store.subscriptions.get(&id);
                if existing.is_some_and(|s| {
                    s.secret != secret && s.old_secret.is_some() && s.rotation_until_ms > now
                }) {
                    bail!("signing rotation already in progress");
                }
                let verified_until = store
                    .subscriptions
                    .values()
                    .filter(|s| {
                        s.authorization.principal == auth.principal
                            && s.url == request.delivery.url
                            && s.verified_until_ms > now
                            && live_state(&s.state)
                    })
                    .map(|s| s.verified_until_ms)
                    .max()
                    .unwrap_or(0);
                let verified = verified_until > now;
                let old_secret = existing
                    .filter(|s| !s.secret.is_empty() && s.secret != secret)
                    .map(|s| s.secret.clone())
                    .or_else(|| {
                        existing
                            .filter(|s| s.rotation_until_ms > now)
                            .and_then(|s| s.old_secret.clone())
                    });
                let sub = Subscription {
                    id: id.clone(),
                    authorization: auth,
                    filters: filters.clone(),
                    url: request.delivery.url,
                    secret,
                    old_secret,
                    rotation_until_ms: existing
                        .filter(|s| s.secret == request.delivery.secret.clone().unwrap_or_default())
                        .map(|s| s.rotation_until_ms)
                        .unwrap_or(now + ROTATION_MS),
                    expires_ms,
                    retired_ms: None,
                    verified_until_ms: verified_until,
                    auth_next_ms: 0,
                    state: "verifying".into(),
                    needs_observation: false,
                    delivery: existing.and_then(|s| s.delivery.clone()),
                };
                store.subscriptions.insert(id.clone(), sub.clone());
                Ok((verified, sub))
            })
            .map_err(|_| {
                McpError::new(
                    rmcp::model::ErrorCode(-32000),
                    "Events rotation limit",
                    None,
                )
            })?;
        let _cleanup = SubscriptionCleanup {
            events: self.clone(),
            id: id.clone(),
        };
        let result = async {
            tokio::time::timeout(Duration::from_secs(10), validate_turn(filters)).await.map_err(|_| internal())??;
            if !verified { self.verify(&sub).await?; }
            if !self.0.ingress.valid(&sub.authorization).await.map_err(|_| auth_error())? { return Err(auth_error()); }
            self.update(|store| {
                let sub = store.subscriptions.get_mut(&id).context("subscription disappeared")?;
                if sub.expires_ms.is_some_and(|expires| expires <= now_ms()) { retire(sub, "expired"); bail!("subscription expired during verification"); }
                sub.state = "active".into();
                if !verified { sub.verified_until_ms = now_ms() + VERIFY_CACHE_MS; }
                sub.auth_next_ms = now_ms() + AUTH_INTERVAL_MS;
                Ok(json!({"id":id,"refreshBefore":sub.expires_ms.map(iso).transpose()? ,"cursor":null,"truncated":false}))
            }).map_err(|_| internal())
        }.await;
        if result.is_err() {
            self.update(|store| {
                if let Some(sub) = store.subscriptions.get_mut(&id) {
                    retire(sub, "verificationFailed");
                }
                Ok(())
            })
            .map_err(|_| internal())?;
        }
        result
    }

    async fn verify(&self, sub: &Subscription) -> Result<(), McpError> {
        // Recheck immediately before admitting even a verification request.
        if !self
            .0
            .ingress
            .valid(&sub.authorization)
            .await
            .map_err(|_| auth_error())?
        {
            return Err(auth_error());
        }
        let mut random = [0; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|_| internal())?;
        let challenge = format!("{:x}", Sha256::digest(random));
        let id = format!("msg_verification_{challenge}");
        let bytes = serde_json::to_vec(&json!({"type":"verification","challenge":challenge}))
            .map_err(|_| internal())?;
        let timestamp = now_ms() / 1000;
        let signatures =
            webhook::signature(&sub.secret, &id, timestamp, &bytes).map_err(|_| internal())?;
        let receipt = tokio::time::timeout(
            Duration::from_secs(10),
            self.0
                .webhook
                .post(&sub.url, &id, &sub.id, timestamp, &signatures, &bytes),
        )
        .await
        .map_err(|_| callback_error("timeout"))?
        .map_err(|_| callback_error("connection_failed"))?;
        let echo = serde_json::from_slice::<Value>(&receipt.body)
            .ok()
            .and_then(|v| v["challenge"].as_str().map(str::to_owned));
        if !(200..300).contains(&receipt.status)
            || !echo.is_some_and(|echo| challenge_matches(&challenge, &echo))
        {
            return Err(callback_error("challenge_failed"));
        }
        Ok(())
    }

    pub fn observe(&self, fact: TerminalTurn) -> Result<(), String> {
        if !matches!(fact.status.as_str(), "completed" | "interrupted" | "failed") {
            return Err("nonterminal projection".into());
        }
        let event_id = digest("evt_", &json!([EVENT_NAME, fact.thread_id, fact.turn_id]));
        self.update(|store| {
            let now = now_ms();
            for sub in store.subscriptions.values_mut() {
                if sub.expires_ms.is_some_and(|expires| expires <= now) && live_state(&sub.state) { retire(sub, "expired"); }
                if live_state(&sub.state) && sub.filters.thread_id == fact.thread_id && sub.filters.turn_id == fact.turn_id && sub.delivery.is_none() {
                    let bytes = serde_json::to_string(&json!({"eventId":event_id,"name":EVENT_NAME,"timestamp":iso(fact.timestamp_ms)?,
                        "data":{"threadId":fact.thread_id,"turnId":fact.turn_id,"status":fact.status},"cursor":null}))?;
                    sub.delivery = Some(Delivery { event_id: event_id.clone(), bytes, attempts: 0, next_attempt_ms: now, state: "pending".into() });
                }
            }
            Ok(())
        }).map_err(|_| "terminal delivery storage failed".into())
    }

    pub fn diagnostics(&self) -> Value {
        let Ok(store) = self.0.store.lock() else {
            return json!({"state":"storageFailed"});
        };
        let mut states = BTreeMap::<String, usize>::new();
        for sub in store.subscriptions.values() {
            *states.entry(sub.state.clone()).or_default() += 1;
            if let Some(delivery) = &sub.delivery {
                *states
                    .entry(format!("delivery:{}", delivery.state))
                    .or_default() += 1;
            }
        }
        json!({"storageFailed":self.0.failed.load(Ordering::Acquire),"states":states,"capacity":MAX_SUBSCRIPTIONS,"overflows":store.overflows})
    }

    pub async fn run(&self, relay: Relay) -> Result<()> {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let relay = &relay;
            self.process_next(|filters| async move {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    relay.watch_terminal(&filters.thread_id, &filters.turn_id),
                )
                .await
                .map_err(|_| anyhow::anyhow!("turn recovery timed out"))??;
                Ok(())
            })
            .await?;
        }
    }

    #[cfg(test)]
    async fn tick(&self) -> Result<()> {
        self.process_next(|_| async { Ok(()) }).await
    }

    async fn process_next<F, Fut>(&self, recover: F) -> Result<()>
    where
        F: FnOnce(Filters) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let _admission = self.0.admission.lock().await;
        let now = now_ms();
        let candidates = self.update(|store| {
            store.subscriptions.retain(|_, sub| {
                live_state(&sub.state) || retention_deadline(sub).is_some_and(|until| until > now)
            });
            for sub in store.subscriptions.values_mut() {
                if live_state(&sub.state) && sub.expires_ms.is_some_and(|expires| expires <= now) {
                    retire(sub, "expired");
                }
                if sub.rotation_until_ms <= now {
                    sub.old_secret = None;
                }
            }
            Ok(store
                .subscriptions
                .values()
                .filter(|s| {
                    matches!(s.state.as_str(), "active" | "paused")
                        && (s.auth_next_ms <= now
                            || s.delivery
                                .as_ref()
                                .is_some_and(|d| d.state == "pending" && d.next_attempt_ms <= now))
                })
                .cloned()
                .collect::<Vec<_>>())
        })?;
        // One attempt per tick; one outbound connection admitted at a time.
        let Some(mut sub) = candidates.into_iter().min_by_key(|s| {
            s.delivery
                .as_ref()
                .filter(|d| d.state == "pending")
                .map(|d| d.next_attempt_ms)
                .unwrap_or(s.auth_next_ms)
        }) else {
            return Ok(());
        };
        let decision = self.0.ingress.valid(&sub.authorization).await;
        match decision {
            Ok(false) => {
                self.update(|store| {
                    retire(store.subscriptions.get_mut(&sub.id).unwrap(), "revoked");
                    Ok(())
                })?;
                return Ok(());
            }
            Err(_) => {
                self.update(|store| {
                    let sub = store.subscriptions.get_mut(&sub.id).unwrap();
                    sub.state = "paused".into();
                    sub.auth_next_ms = now + AUTH_INTERVAL_MS;
                    if let Some(d) = &mut sub.delivery {
                        d.next_attempt_ms = now + AUTH_INTERVAL_MS;
                    }
                    Ok(())
                })?;
                return Ok(());
            }
            Ok(true) => {}
        }
        if sub.needs_observation {
            let recovered = recover(sub.filters.clone()).await;
            if recovered.is_err() {
                self.update(|store| {
                    let current = store.subscriptions.get_mut(&sub.id).unwrap();
                    current.state = "paused".into();
                    current.auth_next_ms = now_ms() + AUTH_INTERVAL_MS;
                    if let Some(d) = &mut current.delivery {
                        d.next_attempt_ms = current.auth_next_ms;
                    }
                    Ok(())
                })?;
                return Ok(());
            }
            match self.0.ingress.valid(&sub.authorization).await {
                Ok(true) => {}
                Ok(false) => {
                    self.update(|store| {
                        retire(store.subscriptions.get_mut(&sub.id).unwrap(), "revoked");
                        Ok(())
                    })?;
                    return Ok(());
                }
                Err(_) => {
                    self.update(|store| {
                        let current = store.subscriptions.get_mut(&sub.id).unwrap();
                        current.state = "paused".into();
                        current.auth_next_ms = now_ms() + AUTH_INTERVAL_MS;
                        if let Some(d) = &mut current.delivery {
                            d.next_attempt_ms = current.auth_next_ms;
                        }
                        Ok(())
                    })?;
                    return Ok(());
                }
            }
            self.update(|store| {
                store
                    .subscriptions
                    .get_mut(&sub.id)
                    .unwrap()
                    .needs_observation = false;
                Ok(())
            })?;
        }
        if sub.expires_ms.is_some_and(|expires| expires <= now_ms()) {
            self.update(|store| {
                retire(store.subscriptions.get_mut(&sub.id).unwrap(), "expired");
                Ok(())
            })?;
            return Ok(());
        }
        // Observation can append an outbox entry while the private check is in flight.
        // Read the latest state under the storage lock before consuming an attempt.
        let (updated, admitted) = self.update(|store| {
            let sub = store.subscriptions.get_mut(&sub.id).unwrap();
            if !live_state(&sub.state) {
                return Ok((sub.clone(), false));
            }
            if sub.expires_ms.is_some_and(|expires| expires <= now_ms()) {
                retire(sub, "expired");
                return Ok((sub.clone(), false));
            }
            sub.state = "active".into();
            sub.auth_next_ms = now_ms() + AUTH_INTERVAL_MS;
            let mut admitted = false;
            if let Some(delivery) = &mut sub.delivery
                && delivery.state == "pending"
                && delivery.next_attempt_ms <= now_ms()
            {
                if delivery.attempts >= MAX_ATTEMPTS {
                    delivery.state = "exhausted".into();
                } else {
                    admitted = true;
                    delivery.attempts += 1;
                    delivery.next_attempt_ms = now_ms() + (1_u64 << delivery.attempts) * 1000;
                }
            }
            Ok((sub.clone(), admitted))
        })?;
        sub = updated;
        let attempt = if admitted { sub.delivery.clone() } else { None };
        if !live_state(&sub.state) {
            return Ok(());
        }
        if let Some(mut delivery) = attempt {
            let timestamp = now_ms() / 1000;
            let mut signatures = webhook::signature(
                &sub.secret,
                &delivery.event_id,
                timestamp,
                delivery.bytes.as_bytes(),
            )?;
            if let Some(old) = &sub.old_secret
                && sub.rotation_until_ms > now_ms()
            {
                signatures.push(' ');
                signatures.push_str(&webhook::signature(
                    old,
                    &delivery.event_id,
                    timestamp,
                    delivery.bytes.as_bytes(),
                )?);
            }
            let receipt = tokio::time::timeout(
                Duration::from_secs(10),
                self.0.webhook.post(
                    &sub.url,
                    &delivery.event_id,
                    &sub.id,
                    timestamp,
                    &signatures,
                    delivery.bytes.as_bytes(),
                ),
            )
            .await;
            let status = receipt.ok().and_then(Result::ok).map(|r| r.status);
            if status.is_some_and(|s| (200..300).contains(&s)) {
                delivery.state = "delivered".into();
            } else if status.is_some_and(|s| {
                (300..400).contains(&s)
                    || s == 410
                    || s == 413
                    || ((400..500).contains(&s) && s != 408 && s != 429)
            }) || delivery.attempts >= MAX_ATTEMPTS
            {
                delivery.state = "exhausted".into();
            }
            self.update(|store| {
                let current = store.subscriptions.get_mut(&sub.id).unwrap();
                if live_state(&current.state) {
                    current.delivery = Some(delivery);
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}

struct SubscriptionCleanup {
    events: Events,
    id: String,
}
impl Drop for SubscriptionCleanup {
    fn drop(&mut self) {
        let result = self.events.update(|store| {
            if let Some(sub) = store.subscriptions.get_mut(&self.id)
                && sub.state == "verifying"
            {
                retire(sub, "verificationFailed");
            }
            Ok(())
        });
        if result.is_err() {
            self.events.0.failed.store(true, Ordering::Release);
        }
    }
}

fn challenge_matches(expected: &str, received: &str) -> bool {
    use hmac::{Hmac, Mac};
    if expected.len() != received.len() {
        return false;
    }
    let mut expected_mac =
        Hmac::<Sha256>::new_from_slice(b"MCP callback challenge comparison").unwrap();
    expected_mac.update(expected.as_bytes());
    let mut received_mac =
        Hmac::<Sha256>::new_from_slice(b"MCP callback challenge comparison").unwrap();
    received_mac.update(received.as_bytes());
    received_mac
        .verify_slice(&expected_mac.finalize().into_bytes())
        .is_ok()
}
