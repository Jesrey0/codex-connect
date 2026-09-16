//! Bounded JSONL transport with cancellation-safe writes and one server-request registry.

use crate::{AppServerError, MAX_WIRE_BYTES, PendingServerRequest, protocol::RpcId, remote_error};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};

const MAX_PENDING: usize = 128;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

struct PendingCall {
    method: &'static str,
    sender: oneshot::Sender<Result<Value, AppServerError>>,
}

pub(crate) struct DeferredCall {
    connection: Arc<Connection>,
    id: RpcId,
    receiver: Option<oneshot::Receiver<Result<Value, AppServerError>>>,
}

impl DeferredCall {
    pub async fn wait(mut self) -> Result<Value, AppServerError> {
        self.receiver
            .take()
            .expect("deferred call receiver is consumed once")
            .await
            .map_err(|_| AppServerError::Disconnected)?
    }
}

impl Drop for DeferredCall {
    fn drop(&mut self) {
        self.connection.calls.lock().unwrap().remove(&self.id);
    }
}

struct PendingAction {
    request: Arc<PendingServerRequest>,
    deadline: Instant,
}

struct Outgoing {
    message: Vec<u8>,
    action: Option<Arc<PendingServerRequest>>,
    done: oneshot::Sender<Result<(), AppServerError>>,
}

pub(crate) struct Connection {
    available: AtomicBool,
    next_id: AtomicI64,
    calls: Mutex<HashMap<RpcId, PendingCall>>,
    actions: Mutex<HashMap<RpcId, PendingAction>>,
    writes: mpsc::Sender<Outgoing>,
    events: broadcast::Sender<Value>,
    changed: watch::Sender<u64>,
}

impl Connection {
    pub fn start<R, W>(reader: R, writer: W) -> Arc<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (writes, rx) = mpsc::channel(MAX_PENDING);
        let (events, _) = broadcast::channel(512);
        let (changed, _) = watch::channel(0);
        let connection = Arc::new(Self {
            available: AtomicBool::new(true),
            next_id: AtomicI64::new(1),
            calls: Mutex::new(HashMap::new()),
            actions: Mutex::new(HashMap::new()),
            writes,
            events,
            changed,
        });
        tokio::spawn(read_loop(connection.clone(), reader));
        tokio::spawn(write_loop(connection.clone(), writer, rx));
        tokio::spawn(expire_actions(connection.clone()));
        connection
    }

    pub fn available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn wake(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn disconnect(&self) {
        if !self.available.swap(false, Ordering::AcqRel) {
            return;
        }
        for (_, pending) in self.calls.lock().unwrap().drain() {
            let _ = pending.sender.send(Err(AppServerError::Disconnected));
        }
        self.actions.lock().unwrap().clear();
        let _ = self
            .events
            .send(json!({"method":"codexConnect/appServerStopped","params":{}}));
        self.wake();
    }

    fn fail_call_for_oversized_frame(&self, id: &RpcId) {
        if let Some(call) = self.calls.lock().unwrap().remove(id) {
            let _ = call.sender.send(Err(AppServerError::MessageTooLarge));
        }
        self.wake();
    }

    async fn reject_oversized_server_request(&self, id: RpcId) -> Result<(), AppServerError> {
        self.send(
            json!({"id":id,"error":{
                "code":-32000,
                "message":"App Server request exceeds the operator transport size limit"
            }}),
            None,
        )
        .await
    }

    fn signal_history_gap(&self) {
        let _ = self.events.send(json!({
            "method":"codexConnect/appServerHistoryGap",
            "params":{}
        }));
        self.wake();
    }

    pub async fn call(
        self: &Arc<Self>,
        method: &'static str,
        params: Value,
        duration: Duration,
    ) -> Result<Value, AppServerError> {
        timeout(duration, async {
            let call = self.start_call(method, params).await?;
            call.wait().await
        })
        .await
        .map_err(|_| AppServerError::Timeout {
            method: method.into(),
            timeout_ms: duration.as_millis().try_into().unwrap_or(u64::MAX),
        })?
    }

    pub async fn start_call(
        self: &Arc<Self>,
        method: &'static str,
        params: Value,
    ) -> Result<DeferredCall, AppServerError> {
        if !self.available() {
            return Err(AppServerError::Disconnected);
        }
        let id = RpcId::Integer(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = oneshot::channel();
        {
            let mut calls = self.calls.lock().unwrap();
            if calls.len() >= MAX_PENDING {
                return Err(AppServerError::Overloaded);
            }
            calls.insert(id.clone(), PendingCall { method, sender });
        }
        let mut guard = CallGuard {
            connection: self.clone(),
            id: id.clone(),
            armed: true,
        };
        self.send(json!({"id":id,"method":method,"params":params}), None)
            .await?;
        guard.armed = false;
        Ok(DeferredCall {
            connection: self.clone(),
            id,
            receiver: Some(receiver),
        })
    }

    pub async fn send(
        &self,
        value: Value,
        action: Option<Arc<PendingServerRequest>>,
    ) -> Result<(), AppServerError> {
        if !self.available() {
            return Err(AppServerError::Disconnected);
        }
        let mut message = serde_json::to_vec(&value)?;
        if message.len() > MAX_WIRE_BYTES {
            return Err(AppServerError::MessageTooLarge);
        }
        message.push(b'\n');
        let (done, result) = oneshot::channel();
        self.writes
            .send(Outgoing {
                message,
                action,
                done,
            })
            .await
            .map_err(|_| AppServerError::Disconnected)?;
        // Once enqueued, the writer owns delivery and commit, even if this future is cancelled.
        result.await.map_err(|_| AppServerError::Disconnected)?
    }

    pub fn actions(&self, thread_id: Option<&str>) -> Vec<Arc<PendingServerRequest>> {
        let mut actions: Vec<_> = self
            .actions
            .lock()
            .unwrap()
            .values()
            .filter(|a| thread_id.is_none_or(|id| a.request.thread_id == id))
            .map(|a| a.request.clone())
            .collect();
        actions.sort_by_key(|a| serde_json::to_string(&a.request_id).unwrap());
        actions
    }

    pub async fn respond(
        &self,
        request: Arc<PendingServerRequest>,
        result: Value,
    ) -> Result<(), AppServerError> {
        self.send(
            json!({"id":request.request_id,"result":result}),
            Some(request),
        )
        .await
    }

    fn current_action(&self, request: &Arc<PendingServerRequest>) -> bool {
        self.actions
            .lock()
            .unwrap()
            .get(&request.request_id)
            .is_some_and(|a| Arc::ptr_eq(&a.request, request))
    }

    fn remove_action(&self, request: &Arc<PendingServerRequest>) {
        let mut actions = self.actions.lock().unwrap();
        if actions
            .get(&request.request_id)
            .is_some_and(|a| Arc::ptr_eq(&a.request, request))
        {
            actions.remove(&request.request_id);
        }
        drop(actions);
        self.wake();
    }

    async fn receive(&self, message: Value) {
        let id = message
            .get("id")
            .cloned()
            .and_then(|id| serde_json::from_value::<RpcId>(id).ok());
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = id {
                let action = PendingServerRequest::parse(id.clone(), method, params);
                let rejection = match action {
                    Ok(request) => {
                        let mut actions = self.actions.lock().unwrap();
                        if actions.contains_key(&id) {
                            // A duplicate envelope must neither replace nor reject the original.
                            return;
                        }
                        if actions.len() >= MAX_PENDING {
                            Some("too many pending server requests".into())
                        } else {
                            actions.insert(
                                id.clone(),
                                PendingAction {
                                    request: Arc::new(request),
                                    deadline: Instant::now() + SERVER_REQUEST_TIMEOUT,
                                },
                            );
                            None
                        }
                    }
                    Err(error) => Some(error),
                };
                if let Some(error) = rejection {
                    let _ = self
                        .send(
                            json!({"id":id,"error":{"code":-32601,"message":error}}),
                            None,
                        )
                        .await;
                }
                self.wake();
                return;
            }
            if method == "serverRequest/resolved" {
                if let Some(id) = params
                    .get("requestId")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<RpcId>(v).ok())
                {
                    self.actions.lock().unwrap().remove(&id);
                }
            }
            let _ = self.events.send(message);
            self.wake();
        } else if let Some(id) = id {
            if let Some(pending) = self.calls.lock().unwrap().remove(&id) {
                let result = message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| remote_error(&message, pending.method));
                let _ = pending.sender.send(result);
            }
        }
    }
}

struct CallGuard {
    connection: Arc<Connection>,
    id: RpcId,
    armed: bool,
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        if self.armed {
            self.connection.calls.lock().unwrap().remove(&self.id);
        }
    }
}

enum OversizedEnvelope {
    Response(RpcId),
    ServerRequest(RpcId),
    Notification,
    Unknown,
}

fn parse_prefix_value<T: DeserializeOwned>(bytes: &[u8], position: &mut usize) -> Option<T> {
    let mut stream = serde_json::Deserializer::from_slice(&bytes[*position..]).into_iter::<T>();
    let value = stream.next()?.ok()?;
    *position += stream.byte_offset();
    Some(value)
}

fn skip_ascii_whitespace(bytes: &[u8], position: &mut usize) {
    while bytes
        .get(*position)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        *position += 1;
    }
}

fn classify_oversized_prefix(bytes: &[u8]) -> OversizedEnvelope {
    let mut position = 0;
    skip_ascii_whitespace(bytes, &mut position);
    if bytes.get(position) != Some(&b'{') {
        return OversizedEnvelope::Unknown;
    }
    position += 1;
    let mut id = None;
    let mut method_seen = false;
    loop {
        skip_ascii_whitespace(bytes, &mut position);
        let Some(key) = parse_prefix_value::<String>(bytes, &mut position) else {
            return OversizedEnvelope::Unknown;
        };
        skip_ascii_whitespace(bytes, &mut position);
        if bytes.get(position) != Some(&b':') {
            return OversizedEnvelope::Unknown;
        }
        position += 1;
        skip_ascii_whitespace(bytes, &mut position);
        match key.as_str() {
            "id" => {
                let Some(value) = parse_prefix_value::<RpcId>(bytes, &mut position) else {
                    return OversizedEnvelope::Unknown;
                };
                id = Some(value);
            }
            "method" => {
                if parse_prefix_value::<String>(bytes, &mut position).is_none() {
                    return OversizedEnvelope::Unknown;
                }
                method_seen = true;
            }
            "params" if method_seen => {
                return id.map_or(
                    OversizedEnvelope::Notification,
                    OversizedEnvelope::ServerRequest,
                );
            }
            "result" | "error" => {
                return id.map_or(OversizedEnvelope::Unknown, OversizedEnvelope::Response);
            }
            "jsonrpc" => {
                if parse_prefix_value::<String>(bytes, &mut position).is_none() {
                    return OversizedEnvelope::Unknown;
                }
            }
            _ => return OversizedEnvelope::Unknown,
        }
        skip_ascii_whitespace(bytes, &mut position);
        match bytes.get(position) {
            Some(b',') => position += 1,
            Some(b'}') => {
                return if method_seen {
                    id.map_or(
                        OversizedEnvelope::Notification,
                        OversizedEnvelope::ServerRequest,
                    )
                } else {
                    OversizedEnvelope::Unknown
                };
            }
            _ => return OversizedEnvelope::Unknown,
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(connection: Arc<Connection>, reader: R) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut discarding_oversized = false;
    let mut changes = connection.changes();
    loop {
        if !connection.available() {
            break;
        }
        let bytes = tokio::select! {
            result = reader.fill_buf() => match result { Ok(bytes) => bytes, Err(_) => break },
            _ = changes.changed() => continue,
        };
        if bytes.is_empty() {
            break;
        }
        let count = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |n| n + 1);
        let ends_line = bytes.get(count.saturating_sub(1)) == Some(&b'\n');
        if discarding_oversized {
            reader.consume(count);
            if ends_line {
                discarding_oversized = false;
            }
            continue;
        }
        if line.len() + count > MAX_WIRE_BYTES {
            let mut prefix = line.clone();
            let remaining = MAX_WIRE_BYTES.saturating_sub(prefix.len());
            prefix.extend_from_slice(&bytes[..remaining.min(count)]);
            match classify_oversized_prefix(&prefix) {
                OversizedEnvelope::Response(id) => {
                    connection.fail_call_for_oversized_frame(&id);
                }
                OversizedEnvelope::ServerRequest(id) => {
                    if connection
                        .reject_oversized_server_request(id)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                OversizedEnvelope::Notification => connection.signal_history_gap(),
                OversizedEnvelope::Unknown => break,
            }
            line.clear();
            reader.consume(count);
            discarding_oversized = !ends_line;
            continue;
        }
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if line.last() == Some(&b'\n') {
            match serde_json::from_slice::<Value>(&line) {
                Ok(message) => connection.receive(message).await,
                Err(_) => break,
            }
            line.clear();
        }
    }
    connection.disconnect();
}

async fn write_loop<W: AsyncWrite + Unpin>(
    connection: Arc<Connection>,
    mut writer: W,
    mut writes: mpsc::Receiver<Outgoing>,
) {
    let mut changes = connection.changes();
    loop {
        if !connection.available() {
            break;
        }
        let outgoing = tokio::select! {
            value = writes.recv() => match value { Some(value) => value, None => break },
            _ = changes.changed() => continue,
        };
        if outgoing
            .action
            .as_ref()
            .is_some_and(|a| !connection.current_action(a))
        {
            let _ = outgoing
                .done
                .send(Err(AppServerError::ServerRequestNotPending));
            continue;
        }
        let mut written = 0;
        let result = timeout(WRITE_TIMEOUT, async {
            while written < outgoing.message.len() {
                let count = writer.write(&outgoing.message[written..]).await?;
                if count == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
                }
                written += count;
            }
            writer.flush().await
        })
        .await;
        let result = match result {
            Ok(Ok(())) => {
                if let Some(request) = &outgoing.action {
                    connection.remove_action(request);
                }
                Ok(())
            }
            failure => {
                // Zero-byte failures leave the action retryable. Partial JSONL delivery is
                // ambiguous, so close the connection and require an official thread resume.
                if written > 0 {
                    connection.disconnect();
                }
                Err(match failure {
                    Ok(Err(error)) => AppServerError::Io(error),
                    Err(_) => AppServerError::Timeout {
                        method: "transport/write".into(),
                        timeout_ms: 5000,
                    },
                    Ok(Ok(())) => unreachable!(),
                })
            }
        };
        let _ = outgoing.done.send(result);
    }
    connection.disconnect();
}

async fn expire_actions(connection: Arc<Connection>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut changes = connection.changes();
    loop {
        if !connection.available() {
            break;
        }
        tokio::select! { _ = interval.tick() => {}, _ = changes.changed() => continue }
        let expired: Vec<_> = connection
            .actions
            .lock()
            .unwrap()
            .values()
            .filter(|a| a.deadline <= Instant::now())
            .map(|a| a.request.clone())
            .collect();
        for action in expired {
            let response = json!({"id": action.request_id, "error": {
                "code": -32000, "message": "Operator response deadline exceeded (30 minutes)"
            }});
            let _ = connection.send(response, Some(action)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[derive(Clone, Default)]
    struct TestWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        fail: Arc<AtomicBool>,
        partial: bool,
    }
    impl AsyncWrite for TestWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.fail.swap(false, Ordering::AcqRel) {
                return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            let count = if self.partial {
                self.fail.store(true, Ordering::Release);
                1
            } else {
                bytes.len()
            };
            self.bytes
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn setup(writer: TestWriter) -> (Arc<Connection>, tokio::io::DuplexStream) {
        let (reader, peer) = tokio::io::duplex(1024);
        (Connection::start(reader, writer), peer)
    }

    async fn question(connection: &Connection, id: Value) {
        connection
            .receive(
                json!({"id":id,"method":"item/tool/requestUserInput","params":{
                    "threadId":"t","turnId":"u","itemId":"i","isBlocking":true,
                    "questions":[{"id":"format","header":"Format","question":"Which format?"}]
                }}),
            )
            .await;
    }

    #[tokio::test]
    async fn failed_send_keeps_action_retryable_and_commits_only_after_success() {
        let writer = TestWriter::default();
        let (connection, _peer) = setup(writer.clone());
        question(&connection, json!(7)).await;
        let request = connection.actions(None).pop().unwrap();
        writer.fail.store(true, Ordering::Release);
        assert!(
            connection
                .respond(request.clone(), json!({"answers":{}}))
                .await
                .is_err()
        );
        assert_eq!(connection.actions(None).len(), 1);
        assert!(writer.bytes.lock().unwrap().is_empty());
        connection
            .respond(request.clone(), json!({"answers":{}}))
            .await
            .unwrap();
        assert!(connection.actions(None).is_empty());
        assert!(connection.respond(request, json!({})).await.is_err());
        assert_eq!(
            writer
                .bytes
                .lock()
                .unwrap()
                .iter()
                .filter(|b| **b == b'\n')
                .count(),
            1
        );
        connection.disconnect();
    }

    #[tokio::test]
    async fn deferred_call_owns_pending_response_until_completion_or_drop() {
        let writer = TestWriter::default();
        let (connection, mut peer) = setup(writer.clone());
        let deferred = connection
            .start_call("command/exec", json!({"command":["sleep","1"]}))
            .await
            .unwrap();
        assert_eq!(connection.calls.lock().unwrap().len(), 1);
        drop(deferred);
        assert!(connection.calls.lock().unwrap().is_empty());

        peer.write_all(b"{\"id\":1,\"result\":{\"exitCode\":0,\"stdout\":\"\",\"stderr\":\"\"}}\n")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(connection.available());
        connection.disconnect();
    }

    #[tokio::test]
    async fn oversized_incoming_frame_fails_calls_without_disconnecting() {
        let writer = TestWriter::default();
        let (connection, mut peer) = setup(writer.clone());
        let first = {
            let connection = connection.clone();
            tokio::spawn(async move {
                connection
                    .call("fs/readFile", json!({}), Duration::from_secs(5))
                    .await
            })
        };
        timeout(Duration::from_secs(1), async {
            loop {
                if writer.bytes.lock().unwrap().contains(&b'\n') {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let mut oversized = b"{\"id\":1,\"result\":{\"data\":\"".to_vec();
        oversized.extend(std::iter::repeat_n(b'x', MAX_WIRE_BYTES));
        oversized.extend_from_slice(b"\"}}\n");
        peer.write_all(&oversized).await.unwrap();
        assert!(matches!(
            first.await.unwrap(),
            Err(AppServerError::MessageTooLarge)
        ));
        assert!(connection.available());

        let second = {
            let connection = connection.clone();
            tokio::spawn(async move {
                connection
                    .call("thread/read", json!({}), Duration::from_secs(5))
                    .await
            })
        };
        timeout(Duration::from_secs(1), async {
            loop {
                if writer
                    .bytes
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count()
                    >= 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        peer.write_all(b"{\"id\":2,\"result\":{\"ok\":true}}\n")
            .await
            .unwrap();
        assert_eq!(second.await.unwrap().unwrap(), json!({"ok":true}));
        assert!(connection.available());
        connection.disconnect();
    }

    #[tokio::test]
    async fn oversized_server_request_is_rejected_without_disconnecting() {
        let writer = TestWriter::default();
        let (connection, mut peer) = setup(writer.clone());
        let mut oversized =
            b"{\"id\":\"request-1\",\"method\":\"item/tool/requestUserInput\",\"params\":{\"data\":\""
                .to_vec();
        oversized.extend(std::iter::repeat_n(b'x', MAX_WIRE_BYTES));
        oversized.extend_from_slice(b"\"}}\n");
        peer.write_all(&oversized).await.unwrap();
        timeout(Duration::from_secs(1), async {
            loop {
                if writer.bytes.lock().unwrap().contains(&b'\n') {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let response: Value = serde_json::from_slice(&writer.bytes.lock().unwrap()).unwrap();
        assert_eq!(response["id"], "request-1");
        assert_eq!(response["error"]["code"], -32000);
        assert!(connection.available());
        connection.disconnect();
    }

    #[tokio::test]
    async fn oversized_notification_emits_history_gap_without_disconnecting() {
        let (connection, mut peer) = setup(TestWriter::default());
        let mut events = connection.subscribe();
        let mut oversized = b"{\"method\":\"item/completed\",\"params\":{\"data\":\"".to_vec();
        oversized.extend(std::iter::repeat_n(b'x', MAX_WIRE_BYTES));
        oversized.extend_from_slice(b"\"}}\n");
        peer.write_all(&oversized).await.unwrap();
        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event["method"], "codexConnect/appServerHistoryGap");
        assert!(connection.available());
        connection.disconnect();
    }

    #[tokio::test]
    async fn partial_send_fails_the_connection_without_retrying_ambiguous_delivery() {
        let (connection, _peer) = setup(TestWriter {
            partial: true,
            ..TestWriter::default()
        });
        question(&connection, json!(7)).await;
        let request = connection.actions(None).pop().unwrap();
        assert!(connection.respond(request, json!({})).await.is_err());
        assert!(!connection.available());
        assert!(connection.actions(None).is_empty());
    }

    #[tokio::test]
    async fn concurrent_replies_send_once_and_duplicate_requests_do_not_replace_the_original() {
        let writer = TestWriter::default();
        let (connection, _peer) = setup(writer.clone());
        question(&connection, json!("7")).await;
        let request = connection.actions(None).pop().unwrap();
        question(&connection, json!("7")).await;
        assert!(Arc::ptr_eq(&request, &connection.actions(None)[0]));
        let (a, b) = tokio::join!(
            connection.respond(request.clone(), json!({})),
            connection.respond(request, json!({}))
        );
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(
            writer
                .bytes
                .lock()
                .unwrap()
                .iter()
                .filter(|b| **b == b'\n')
                .count(),
            1
        );
        connection.disconnect();
    }

    #[tokio::test]
    async fn resolution_and_disconnect_clear_actions_even_when_notification_consumers_lag() {
        let (connection, peer) = setup(TestWriter::default());
        let mut events = connection.subscribe();
        question(&connection, json!(-7)).await;
        for n in 0..600 {
            connection
                .receive(json!({"method":"item/agentMessage/delta","params":{"n":n}}))
                .await;
        }
        assert!(matches!(
            events.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        assert_eq!(connection.actions(None).len(), 1);
        connection
            .receive(
                json!({"method":"serverRequest/resolved","params":{"threadId":"t","requestId":-7}}),
            )
            .await;
        assert!(connection.actions(None).is_empty());
        question(&connection, json!(8)).await;
        let mut changed = connection.changes();
        drop(peer);
        while connection.available() {
            changed.changed().await.unwrap();
        }
        assert!(connection.actions(None).is_empty());
    }

    #[tokio::test]
    async fn rpc_timeout_and_cancellation_remove_transport_calls() {
        let (connection, _peer) = setup(TestWriter::default());
        assert!(matches!(
            connection
                .call("thread/read", json!({}), Duration::from_millis(1))
                .await,
            Err(AppServerError::Timeout { .. })
        ));
        assert!(connection.calls.lock().unwrap().is_empty());
        let call = {
            let c = connection.clone();
            tokio::spawn(async move {
                c.call("thread/read", json!({}), Duration::from_secs(30))
                    .await
            })
        };
        tokio::task::yield_now().await;
        call.abort();
        let _ = call.await;
        assert!(connection.calls.lock().unwrap().is_empty());
        connection.disconnect();
    }

    #[tokio::test]
    async fn expired_action_is_rejected_and_removed() {
        let writer = TestWriter::default();
        let (connection, _peer) = setup(writer.clone());
        question(&connection, json!(1)).await;
        connection
            .actions
            .lock()
            .unwrap()
            .get_mut(&RpcId::Integer(1))
            .unwrap()
            .deadline = Instant::now();
        let mut changes = connection.changes();
        timeout(Duration::from_secs(2), async {
            while !connection.actions(None).is_empty() {
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let response: Value = serde_json::from_slice(&writer.bytes.lock().unwrap()).unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["error"]["code"], -32000);
        connection.disconnect();
    }

    #[tokio::test]
    async fn queued_response_survives_caller_cancellation() {
        let writer = TestWriter::default();
        let (connection, _peer) = setup(writer.clone());
        question(&connection, json!(1)).await;
        let request = connection.actions(None).pop().unwrap();
        let (done, receiver) = oneshot::channel();
        connection
            .writes
            .send(Outgoing {
                message: b"{\"id\":1,\"result\":{}}\n".to_vec(),
                action: Some(request),
                done,
            })
            .await
            .unwrap();
        drop(receiver);
        let mut changed = connection.changes();
        while !connection.actions(None).is_empty() {
            changed.changed().await.unwrap();
        }
        assert!(!writer.bytes.lock().unwrap().is_empty());
        connection.disconnect();
    }
}
