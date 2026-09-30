use super::webhook::{Receipt, public_address, signature};
use super::*;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use base64::Engine;
use std::sync::atomic::{AtomicU16, AtomicUsize};
use tempfile::TempDir;

type CallbackRecord = (String, String, u64, String, Vec<u8>);

pub(super) struct CallbackFixture {
    records: Mutex<Vec<CallbackRecord>>,
    status: AtomicU16,
    echo: AtomicBool,
    unsafe_dns: AtomicBool,
}
impl Default for CallbackFixture {
    fn default() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            status: AtomicU16::new(200),
            echo: AtomicBool::new(true),
            unsafe_dns: AtomicBool::new(false),
        }
    }
}
impl CallbackFixture {
    pub fn post(
        &self,
        id: &str,
        sub: &str,
        time: u64,
        signatures: &str,
        bytes: &[u8],
    ) -> Result<Receipt> {
        if self.unsafe_dns.load(Ordering::Relaxed) {
            bail!("unsafe callback address");
        }
        self.records.lock().unwrap().push((
            id.into(),
            sub.into(),
            time,
            signatures.into(),
            bytes.to_vec(),
        ));
        let body: Value = serde_json::from_slice(bytes)?;
        if id.starts_with("msg_verification_") {
            Ok(Receipt {
                status: 200,
                body: serde_json::to_vec(
                    &json!({"challenge":if self.echo.load(Ordering::Relaxed) {
                body["challenge"].clone() } else { json!("wrong") }}),
                )?,
            })
        } else {
            Ok(Receipt {
                status: self.status.load(Ordering::Relaxed),
                body: Vec::new(),
            })
        }
    }
    fn application_records(&self) -> Vec<CallbackRecord> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, ..)| !id.starts_with("msg_verification_"))
            .cloned()
            .collect()
    }
}

#[derive(Default)]
struct Authority {
    deny: AtomicBool,
    outage: AtomicBool,
    calls: AtomicUsize,
}
struct Fixture {
    directory: TempDir,
    authority: Arc<Authority>,
    callback: Arc<CallbackFixture>,
    ingress: Ingress,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn authorization() -> Authorization {
    Authorization {
        principal: "fixed-local-operator".into(),
        client_id: "chatgpt".into(),
        grant_id: "opaque-grant-1".into(),
        resource: "https://ingress.example/codex-connect/mcp".into(),
        scope: "codex-connect:access".into(),
        grant_context: "fixture-encrypted-grant-context".into(),
    }
}

async fn fixture() -> (Fixture, Events) {
    let directory = tempfile::tempdir().unwrap();
    let authority = Arc::new(Authority::default());
    let callback = Arc::new(CallbackFixture::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/authorize", listener.local_addr().unwrap());
    let router = Router::new()
        .route(
            "/authorize",
            post(
                |State(authority): State<Arc<Authority>>, Json(body): Json<Value>| async move {
                    authority.calls.fetch_add(1, Ordering::Relaxed);
                    if authority.outage.load(Ordering::Relaxed) {
                        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
                    }
                    if authority.deny.load(Ordering::Relaxed) {
                        return (StatusCode::FORBIDDEN, Json(json!({"allowed":false})));
                    }
                    let auth = if body["requestContext"] == "verified-request" {
                        serde_json::to_value(authorization()).unwrap()
                    } else {
                        body["authorization"].clone()
                    };
                    if auth["principal"] != "fixed-local-operator"
                        || auth["scope"] != "codex-connect:access"
                        || auth["grantContext"] != "fixture-encrypted-grant-context"
                    {
                        return (StatusCode::FORBIDDEN, Json(json!({"allowed":false})));
                    }
                    (
                        StatusCode::OK,
                        Json(json!({"allowed":true,"authorization":auth})),
                    )
                },
            ),
        )
        .with_state(authority.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let ingress = Ingress::fixture(endpoint);
    let events = Events::with_network(
        directory.path().join("events"),
        ingress.clone(),
        webhook::Webhook {
            fixture: Some(callback.clone()),
        },
    )
    .unwrap();
    (
        Fixture {
            directory,
            authority,
            callback,
            ingress,
            server,
        },
        events,
    )
}

fn request(url: &str) -> SubscriptionRequest {
    serde_json::from_value(json!({"name":EVENT_NAME,"arguments":{"threadId":"thread-1","turnId":"turn-1"},
        "delivery":{"mode":"webhook","url":url,"secret":format!("whsec_{}",base64::engine::general_purpose::STANDARD.encode([1;32]))}})).unwrap()
}
fn terminal(thread: &str, turn: &str) -> TerminalTurn {
    TerminalTurn {
        thread_id: thread.into(),
        turn_id: turn.into(),
        status: "completed".into(),
        timestamp_ms: 1_790_700_000_000,
    }
}
async fn subscribe(events: &Events, url: &str) -> Value {
    events
        .subscribe(request(url), authorization(), |_| async { Ok(()) })
        .await
        .unwrap()
}
fn due(events: &Events) {
    events
        .update(|store| {
            for s in store.subscriptions.values_mut() {
                s.auth_next_ms = 0;
                if let Some(d) = &mut s.delivery {
                    d.next_attempt_ms = 0;
                }
            }
            Ok(())
        })
        .unwrap();
}
fn cancel_request(url: &str) -> UnsubscribeRequest {
    serde_json::from_value(json!({"name":EVENT_NAME,
    "arguments":{"threadId":"thread-1","turnId":"turn-1"},"delivery":{"mode":"webhook","url":url}}))
    .unwrap()
}

#[test]
fn signature_covers_exact_bytes_and_matches_standard_webhooks_vector() {
    let secret = "whsec_AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
    // Independently calculated with Python hmac.new(key, b'evt_test.1790700000.{"x":1}', hashlib.sha256).
    assert_eq!(
        signature(secret, "evt_test", 1790700000, b"{\"x\":1}").unwrap(),
        "v1,4lypWAVcXo8+jC1PnvMlpRZvb/QwYdDgeo4sqJOmhcw="
    );
    assert_ne!(
        signature(secret, "evt_test", 1790700000, b"{\"x\":1}").unwrap(),
        signature(secret, "evt_test", 1790700000, b"{ \"x\":1}").unwrap()
    );
}

#[test]
fn public_callback_addresses_only() {
    for addr in [
        "0.0.0.0",
        "10.0.0.1",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.31.0.1",
        "192.168.1.1",
        "192.0.0.1",
        "198.18.0.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::ffff:8.8.8.8",
        "fc00::1",
        "fe80::1",
        "2001:db8::1",
        "2002:0808:0808::1",
        "2001:20::1",
        "3fff::1",
    ] {
        assert!(!public_address(addr.parse().unwrap()), "{addr}");
    }
    for addr in ["8.8.8.8", "93.184.216.34", "2606:4700:4700::1111"] {
        assert!(public_address(addr.parse().unwrap()));
    }
    for url in [
        "http://receiver.example/cb",
        "https://user@receiver.example/cb",
        "https://receiver.example:8080/cb",
        "https://receiver.example/cb#fragment",
        "https://127.1/cb",
        "https://[::ffff:8.8.8.8]/cb",
    ] {
        assert!(webhook::callback_url(url).is_err());
    }
}

#[tokio::test]
async fn canonical_identity_idempotency_and_verification_cache() {
    let (fixture, events) = fixture().await;
    let url = "https://receiver.example/a";
    let first = subscribe(&events, url).await;
    let mut refresh = request(url);
    refresh.ttl_ms = Some(1);
    let result = events
        .subscribe(refresh, authorization(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(first["id"], result["id"]);
    assert!(
        chrono::DateTime::parse_from_rfc3339(result["refreshBefore"].as_str().unwrap())
            .unwrap()
            .timestamp_millis() as u64
            <= now_ms() + MIN_TTL_MS
    );
    assert_eq!(fixture.callback.records.lock().unwrap().len(), 1);
    assert_eq!(events.0.store.lock().unwrap().subscriptions.len(), 1);
    let reordered: SubscriptionRequest = serde_json::from_value(json!({"arguments":{"turnId":"turn-1","threadId":"thread-1"},
        "name":EVENT_NAME,"delivery":{"url":url,"mode":"webhook","secret":request(url).delivery.secret}})).unwrap();
    assert_eq!(
        first["id"],
        events
            .subscribe(reordered, authorization(), |_| async { Ok(()) })
            .await
            .unwrap()["id"]
    );
    assert!(first["cursor"].is_null());
}

#[tokio::test]
async fn terminal_dedup_exact_filters_and_shared_user_callbacks() {
    let (fixture, events) = fixture().await;
    let first = subscribe(&events, "https://receiver.example/a").await;
    let second = subscribe(&events, "https://receiver.example/b").await;
    assert_ne!(first["id"], second["id"]);
    events
        .observe(terminal("different-thread", "turn-1"))
        .unwrap();
    events
        .observe(terminal("thread-1", "different-turn"))
        .unwrap();
    events.tick().await.unwrap();
    assert!(fixture.callback.application_records().is_empty());
    let fact = terminal("thread-1", "turn-1");
    events.observe(fact.clone()).unwrap();
    events.observe(fact.clone()).unwrap();
    events.tick().await.unwrap();
    events.tick().await.unwrap();
    let records = fixture.callback.application_records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].0, records[1].0);
    assert_eq!(records[0].4, records[1].4);
    for record in records {
        let event: Value = serde_json::from_slice(&record.4).unwrap();
        assert_eq!(event["eventId"], record.0);
        assert_eq!(
            event["data"],
            json!({"threadId":"thread-1","turnId":"turn-1","status":"completed"})
        );
        assert_eq!(
            record.3,
            signature(
                &request("https://receiver.example/a")
                    .delivery
                    .secret
                    .unwrap(),
                &record.0,
                record.2,
                &record.4
            )
            .unwrap()
        );
    }
    events.observe(fact).unwrap();
    due(&events);
    events.tick().await.unwrap();
    assert_eq!(fixture.callback.application_records().len(), 2);
}

#[tokio::test]
async fn revocation_outage_and_expiry_block_network_and_do_not_auto_rebind() {
    let (fixture, events) = fixture().await;
    subscribe(&events, "https://receiver.example/a").await;
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    fixture.authority.outage.store(true, Ordering::Relaxed);
    events.tick().await.unwrap();
    assert!(fixture.callback.application_records().is_empty());
    assert_eq!(events.diagnostics()["states"]["paused"], 1);
    fixture.authority.outage.store(false, Ordering::Relaxed);
    fixture.authority.deny.store(true, Ordering::Relaxed);
    due(&events);
    events.tick().await.unwrap();
    assert_eq!(events.diagnostics()["states"]["revoked"], 1);
    fixture.authority.deny.store(false, Ordering::Relaxed);
    due(&events);
    events.tick().await.unwrap();
    assert!(fixture.callback.application_records().is_empty());
    let mut new_auth = authorization();
    new_auth.grant_id = "opaque-grant-2".into();
    let refreshed = events
        .subscribe(request("https://receiver.example/a"), new_auth, |_| async {
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(events.0.store.lock().unwrap().subscriptions.len(), 1);
    assert_eq!(
        events.0.store.lock().unwrap().subscriptions[refreshed["id"].as_str().unwrap()]
            .authorization
            .grant_id,
        "opaque-grant-2"
    );
    // Explicit refresh does not revive retired pending work; a new factual observation has the same logical ID.
    events
        .update(|store| {
            for sub in store.subscriptions.values_mut() {
                sub.expires_ms = 0;
            }
            Ok(())
        })
        .unwrap();
    due(&events);
    events.tick().await.unwrap();
    assert_eq!(events.diagnostics()["states"]["expired"], 1);
    assert!(fixture.callback.application_records().is_empty());
}

#[tokio::test]
async fn outage_recovers_same_grant_with_pending_delivery() {
    let (fixture, events) = fixture().await;
    subscribe(&events, "https://receiver.example/a").await;
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    fixture.authority.outage.store(true, Ordering::Relaxed);
    events.tick().await.unwrap();
    fixture.authority.outage.store(false, Ordering::Relaxed);
    due(&events);
    events.tick().await.unwrap();
    assert_eq!(fixture.callback.application_records().len(), 1);
    assert_eq!(events.diagnostics()["states"]["delivery:delivered"], 1);
}

#[tokio::test]
async fn cancellation_with_new_grant_is_idempotent_and_retires_retry() {
    let (fixture, events) = fixture().await;
    subscribe(&events, "https://receiver.example/a").await;
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    fixture.callback.status.store(503, Ordering::Relaxed);
    events.tick().await.unwrap();
    let mut auth = authorization();
    auth.grant_id = "another-current-grant".into();
    events
        .cancel(cancel_request("https://receiver.example/a"), &auth)
        .await
        .unwrap();
    events
        .cancel(cancel_request("https://receiver.example/a"), &auth)
        .await
        .unwrap();
    due(&events);
    events.tick().await.unwrap();
    assert_eq!(fixture.callback.application_records().len(), 1);
    assert_eq!(events.diagnostics()["states"]["delivery:cancelled"], 1);
    let stored = fs::read_to_string(&events.0.path).unwrap();
    assert!(!stored.contains("whsec_"));
}

#[tokio::test]
async fn retries_preserve_bytes_ids_and_exhaust_at_bound() {
    let (fixture, events) = fixture().await;
    subscribe(&events, "https://receiver.example/a").await;
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    fixture.callback.status.store(503, Ordering::Relaxed);
    for _ in 0..MAX_ATTEMPTS + 2 {
        due(&events);
        events.tick().await.unwrap();
    }
    let records = fixture.callback.application_records();
    assert_eq!(records.len(), MAX_ATTEMPTS as usize);
    assert!(
        records
            .iter()
            .all(|r| r.0 == records[0].0 && r.4 == records[0].4)
    );
    assert_eq!(events.diagnostics()["states"]["delivery:exhausted"], 1);
}

#[tokio::test]
async fn nonretryable_responses_and_dns_rebinding() {
    for status in [410, 413, 400] {
        let (fixture, events) = fixture().await;
        subscribe(&events, "https://receiver.example/a").await;
        events.observe(terminal("thread-1", "turn-1")).unwrap();
        fixture.callback.status.store(status, Ordering::Relaxed);
        events.tick().await.unwrap();
        due(&events);
        events.tick().await.unwrap();
        assert_eq!(fixture.callback.application_records().len(), 1);
    }
    let (fixture, events) = fixture().await;
    subscribe(&events, "https://receiver.example/a").await;
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    fixture.callback.unsafe_dns.store(true, Ordering::Relaxed);
    events.tick().await.unwrap();
    assert!(fixture.callback.application_records().is_empty());
    fixture.callback.unsafe_dns.store(false, Ordering::Relaxed);
    due(&events);
    events.tick().await.unwrap();
    assert_eq!(fixture.callback.application_records().len(), 1);
}

#[tokio::test]
async fn challenge_failure_and_authority_denial_never_activate() {
    let (fixture, events) = fixture().await;
    fixture.callback.echo.store(false, Ordering::Relaxed);
    let error = events
        .subscribe(
            request("https://receiver.example/a"),
            authorization(),
            |_| async { Ok(()) },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, rmcp::model::ErrorCode(-32015));
    assert_eq!(error.data.unwrap()["reason"], "challenge_failed");
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    events.tick().await.unwrap();
    assert!(fixture.callback.application_records().is_empty());
    fixture.authority.deny.store(true, Ordering::Relaxed);
    assert!(
        events
            .subscribe(
                request("https://receiver.example/b"),
                authorization(),
                |_| async { Ok(()) }
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.callback.records.lock().unwrap().len(), 1);
    assert!(events.authenticate("forged").await.is_err());
    assert!(events.authenticate("verified-request").await.is_err());
}

#[tokio::test]
async fn refresh_rotates_secrets_and_rechecks_expired_verification() {
    let (fixture, events) = fixture().await;
    let url = "https://receiver.example/a";
    subscribe(&events, url).await;
    let original = request(url).delivery.secret.unwrap();
    let replacement = format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode([2; 32])
    );
    let mut refresh = request(url);
    refresh.delivery.secret = Some(replacement.clone());
    events
        .subscribe(refresh, authorization(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(fixture.callback.records.lock().unwrap().len(), 2);
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    events.tick().await.unwrap();
    let records = fixture.callback.application_records();
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(
        r.3,
        format!(
            "{} {}",
            signature(&replacement, &r.0, r.2, &r.4).unwrap(),
            signature(&original, &r.0, r.2, &r.4).unwrap()
        )
    );
    events
        .update(|store| {
            for s in store.subscriptions.values_mut() {
                s.verified_until_ms = 0;
                s.rotation_until_ms = 0;
            }
            Ok(())
        })
        .unwrap();
    let mut refresh = request(url);
    refresh.delivery.secret = Some(replacement);
    events
        .subscribe(refresh, authorization(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(fixture.callback.records.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn restart_recovers_pending_work_after_live_check_and_never_cancelled_work() {
    let (fixture, events) = fixture().await;
    subscribe(&events, "https://receiver.example/a").await;
    subscribe(&events, "https://receiver.example/b").await;
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    events
        .cancel(
            cancel_request("https://receiver.example/b"),
            &authorization(),
        )
        .await
        .unwrap();
    fixture.callback.status.store(503, Ordering::Relaxed);
    events.tick().await.unwrap();
    let before = fixture.callback.application_records()[0].clone();
    let path = events.0.path.clone();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(events);
    let recovered = Events::with_network(
        fixture.directory.path().join("events"),
        fixture.ingress.clone(),
        webhook::Webhook {
            fixture: Some(fixture.callback.clone()),
        },
    )
    .unwrap();
    fixture.authority.outage.store(true, Ordering::Relaxed);
    due(&recovered);
    recovered.tick().await.unwrap();
    assert_eq!(fixture.callback.application_records().len(), 1);
    fixture.authority.outage.store(false, Ordering::Relaxed);
    fixture.callback.status.store(200, Ordering::Relaxed);
    due(&recovered);
    recovered.tick().await.unwrap();
    let records = fixture.callback.application_records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].0, before.0);
    assert_eq!(records[1].4, before.4);
    assert_eq!(recovered.diagnostics()["states"]["cancelled"], 1);
    assert!(
        !fs::read_to_string(path)
            .unwrap()
            .contains("verified-request")
    );
}

#[tokio::test]
async fn queue_bound_and_single_store_owner_are_visible() {
    let (fixture, events) = fixture().await;
    assert!(Events::open(fixture.directory.path().join("events")).is_err());
    let url = "https://receiver.example/a";
    subscribe(&events, url).await;
    events
        .update(|store| {
            let sub = store.subscriptions.values().next().unwrap().clone();
            for i in 1..MAX_SUBSCRIPTIONS {
                let mut sub = sub.clone();
                sub.id = format!("fixture-{i}");
                store.subscriptions.insert(sub.id.clone(), sub);
            }
            Ok(())
        })
        .unwrap();
    let error = events
        .subscribe(
            request("https://receiver.example/overflow"),
            authorization(),
            |_| async { Ok(()) },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, rmcp::model::ErrorCode(-32000));
    assert_eq!(events.diagnostics()["overflows"], 1);
    assert_eq!(fixture.callback.records.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_filters_secret_cursor_and_failed_resource_validation() {
    let (_fixture, events) = fixture().await;
    let url = "https://receiver.example/a";
    for value in ["", " thread-1", "thread-1\n"] {
        let mut r = request(url);
        r.arguments.thread_id = value.into();
        assert!(
            events
                .subscribe(r, authorization(), |_| async { Ok(()) })
                .await
                .is_err()
        );
    }
    let mut r = request(url);
    r.cursor = Some("history".into());
    assert!(
        events
            .subscribe(r, authorization(), |_| async { Ok(()) })
            .await
            .is_err()
    );
    let mut r = request(url);
    r.delivery.secret = Some("whsec_short".into());
    assert!(
        events
            .subscribe(r, authorization(), |_| async { Ok(()) })
            .await
            .is_err()
    );
    assert!(
        events
            .subscribe(request(url), authorization(), |_| async {
                Err(invalid("not canonical"))
            })
            .await
            .is_err()
    );
    assert_eq!(events.diagnostics()["states"]["verificationFailed"], 1);
}

#[tokio::test]
async fn cancelled_verification_future_is_retired() {
    let (_fixture, events) = fixture().await;
    let future = events.subscribe(
        request("https://receiver.example/a"),
        authorization(),
        |_| async { std::future::pending::<Result<(), McpError>>().await },
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), future)
            .await
            .is_err()
    );
    assert_eq!(events.diagnostics()["states"]["verificationFailed"], 1);
}

#[tokio::test]
async fn production_transport_denies_local_dns_and_private_ip_without_connecting() {
    let transport = webhook::Webhook::default();
    for url in [
        "https://localhost/callback",
        "https://127.0.0.1/callback",
        "https://[::1]/callback",
    ] {
        assert!(
            tokio::time::timeout(
                Duration::from_secs(3),
                transport.post(
                    url,
                    "msg_verification_test",
                    "sub_test",
                    1,
                    "v1,fixture",
                    b"{}"
                )
            )
            .await
            .unwrap()
            .is_err()
        );
    }
}

#[tokio::test]
async fn durable_write_failure_stops_admission_visibly() {
    let (_fixture, events) = fixture().await;
    subscribe(&events, "https://receiver.example/a").await;
    fs::remove_file(&events.0.path).unwrap();
    fs::create_dir(&events.0.path).unwrap();
    assert!(events.observe(terminal("thread-1", "turn-1")).is_err());
    assert!(events.tick().await.is_err());
    assert_eq!(events.diagnostics()["storageFailed"], true);
}

#[tokio::test]
async fn http_boundary_requires_verified_context_and_projects_only_core_event() {
    let (fixture, events) = fixture().await;
    let host = codex_connect_host::Host::open(fixture.directory.path()).unwrap();
    let relay = Relay::start(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/support/fake_codex.py"),
        host.clone(),
    )
    .await
    .unwrap();
    let sink = events.clone();
    relay
        .set_terminal_observer(Arc::new(move |fact| sink.observe(fact)))
        .unwrap();
    let runtime = crate::RuntimeIdentity {
        build_id: "fixture".into(),
        binary_sha256: "fixture".into(),
        executable: "fixture".into(),
        endpoint: "fixture".into(),
        codex_binary: "fixture".into(),
        codex_home: "fixture".into(),
        codex_home_source: "fixture".into(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let router = crate::router(relay.clone(), host, runtime, events.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    async fn call(
        client: &reqwest::Client,
        url: &str,
        method: &str,
        params: Value,
        context: Option<&str>,
    ) -> (u16, Value) {
        let mut params = params;
        params.as_object_mut().unwrap().insert(
            "_meta".into(),
            json!({
            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientInfo":{"name":"events-test","version":"1"},
            "io.modelcontextprotocol/clientCapabilities":{}}),
        );
        let mut request = client
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", method)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(context) = context {
            request = request.header(CONTEXT_HEADER, context);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        let body = if let Ok(body) = serde_json::from_str(&text) {
            body
        } else {
            let data = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("SSE result");
            serde_json::from_str(data).unwrap()
        };
        (status, body)
    }
    for context in [
        None,
        Some("forged"),
        Some("verified-request, verified-request"),
    ] {
        assert_eq!(
            call(&client, &endpoint, "events/list", json!({}), context)
                .await
                .0,
            401
        );
    }
    let discovery = call(
        &client,
        &endpoint,
        "server/discover",
        json!({}),
        Some("verified-request"),
    )
    .await;
    assert_eq!(discovery.0, 200);
    assert_eq!(discovery.1["result"]["capabilities"]["events"], json!({}));
    let unauthenticated_discovery =
        call(&client, &endpoint, "server/discover", json!({}), None).await;
    assert!(
        unauthenticated_discovery.1["result"]["capabilities"]
            .get("events")
            .is_none()
    );
    let catalog = call(
        &client,
        &endpoint,
        "events/list",
        json!({}),
        Some("verified-request"),
    )
    .await;
    assert_eq!(catalog.0, 200);
    assert_eq!(catalog.1["result"]["events"].as_array().unwrap().len(), 1);
    assert_eq!(catalog.1["result"]["events"][0]["name"], EVENT_NAME);
    let worker = relay
        .work_start(
            "idle".into(),
            Some(fixture.directory.path().display().to_string()),
            None,
            None,
            None,
            Some("fixture-model-1".into()),
            None,
            Some(codex_connect_relay::SandboxPolicy::DangerFullAccess),
        )
        .await
        .unwrap();
    let thread = worker["threadId"].as_str().unwrap();
    let turn = worker["turnId"].as_str().unwrap();
    let args = json!({"threadId":thread,"turnId":turn});
    let subscription = json!({"name":EVENT_NAME,"arguments":args,"delivery":{"mode":"webhook","url":"https://receiver.example/real-wire",
        "secret":request("https://receiver.example/a").delivery.secret}});
    let result = call(
        &client,
        &endpoint,
        "events/subscribe",
        subscription.clone(),
        Some("verified-request"),
    )
    .await;
    assert_eq!(result.0, 200);
    assert!(result.1.get("error").is_none(), "{:?}", result.1);
    assert!(
        result.1["result"]["id"]
            .as_str()
            .unwrap()
            .starts_with("sub_")
    );
    let mut wrong = subscription.clone();
    wrong["arguments"]["turnId"] = json!("wrong-turn");
    assert!(
        call(
            &client,
            &endpoint,
            "events/subscribe",
            wrong,
            Some("verified-request")
        )
        .await
        .1
        .get("error")
        .is_some()
    );
    relay
        .work_interrupt(thread.to_owned(), turn.to_owned())
        .await
        .unwrap();
    // Event notification and authoritative read converge at the same durable projection.
    relay.watch_terminal(thread, turn).await.unwrap();
    events.tick().await.unwrap();
    relay.watch_terminal(thread, turn).await.unwrap();
    due(&events);
    events.tick().await.unwrap();
    let records = fixture.callback.application_records();
    assert_eq!(records.len(), 1);
    let event: Value = serde_json::from_slice(&records[0].4).unwrap();
    assert_eq!(event["data"]["status"], "interrupted");
    assert_eq!(event["data"]["threadId"], thread);
    assert_eq!(event["data"]["turnId"], turn);
    let stopped = call(
        &client,
        &endpoint,
        "events/unsubscribe",
        json!({"name":EVENT_NAME,"arguments":args,
        "delivery":{"mode":"webhook","url":"https://receiver.example/real-wire"}}),
        Some("verified-request"),
    )
    .await;
    assert_eq!(stopped.1["result"], json!({"resultType":"complete"}));
    fixture.authority.outage.store(true, Ordering::Relaxed);
    assert_eq!(
        call(
            &client,
            &endpoint,
            "events/list",
            json!({}),
            Some("verified-request")
        )
        .await
        .0,
        401
    );
    server.abort();
}

#[tokio::test]
async fn recovery_checks_authority_before_observation_and_before_delivery() {
    let (fixture, events) = fixture().await;
    subscribe(&events, "https://receiver.example/a").await;
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    events
        .update(|store| {
            for s in store.subscriptions.values_mut() {
                s.needs_observation = true;
                s.auth_next_ms = 0;
            }
            Ok(())
        })
        .unwrap();
    let recovered = Arc::new(AtomicUsize::new(0));
    fixture.authority.outage.store(true, Ordering::Relaxed);
    events
        .process_next(|_| async {
            recovered.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(recovered.load(Ordering::Relaxed), 0);
    assert!(fixture.callback.application_records().is_empty());
    fixture.authority.outage.store(false, Ordering::Relaxed);
    due(&events);
    events
        .process_next(|_| async {
            recovered.fetch_add(1, Ordering::Relaxed);
            Err(anyhow::anyhow!("resource unavailable"))
        })
        .await
        .unwrap();
    assert_eq!(recovered.load(Ordering::Relaxed), 1);
    assert!(fixture.callback.application_records().is_empty());
    due(&events);
    events
        .process_next(|_| async {
            recovered.fetch_add(1, Ordering::Relaxed);
            fixture.authority.deny.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(events.diagnostics()["states"]["revoked"], 1);
    assert!(fixture.callback.application_records().is_empty());
}

#[tokio::test]
async fn idle_ticks_skip_durable_replacement_and_transient_recovery_flags_survive() {
    use std::os::unix::fs::MetadataExt;
    let (_fixture, events) = fixture().await;
    let before = fs::metadata(&events.0.path).unwrap();
    for _ in 0..3 {
        events.tick().await.unwrap();
    }
    let after = fs::metadata(&events.0.path).unwrap();
    assert_eq!(before.ino(), after.ino());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    subscribe(&events, "https://receiver.example/a").await;
    let before = fs::metadata(&events.0.path).unwrap();
    for _ in 0..3 {
        events.tick().await.unwrap();
    }
    events
        .update(|store| {
            for sub in store.subscriptions.values_mut() {
                sub.needs_observation = true;
            }
            Ok(())
        })
        .unwrap();
    let after = fs::metadata(&events.0.path).unwrap();
    assert_eq!(before.ino(), after.ino());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert!(
        events
            .0
            .store
            .lock()
            .unwrap()
            .subscriptions
            .values()
            .all(|s| s.needs_observation)
    );
    events.observe(terminal("thread-1", "turn-1")).unwrap();
    assert_ne!(before.ino(), fs::metadata(&events.0.path).unwrap().ino());
    events.tick().await.unwrap();
    assert_eq!(events.diagnostics()["states"]["delivery:delivered"], 1);
}
