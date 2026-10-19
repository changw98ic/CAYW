use crate::{protocol::*, store::Store};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::SinkExt;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc},
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ServerState {
    inner: Arc<Mutex<Inner>>,
    config: Arc<ServerConfig>,
    slots: Arc<Semaphore>,
    shutdown: CancellationToken,
}
struct Inner {
    store: Store,
    sessions: HashMap<String, Session>,
    active: HashMap<(String, String), Active>,
    poisoned: bool,
}
#[derive(Clone)]
struct Session {
    generation: u64,
    cancel: CancellationToken,
    queue: mpsc::Sender<String>,
    limits: Limits,
    admitted: usize,
    // Execution occupancy survives a durable deadline tombstone until the
    // Device reports an outcome or the whole generation is disconnected.
    executing: HashSet<String>,
}
struct Active {
    generation: u64,
    expires: Instant,
}

impl ServerState {
    /// A single live server must own this SQLite file. Existing credentials must
    /// match the stored operator hash; changing the token is never implicit.
    pub fn open(
        path: impl AsRef<Path>,
        operator_token: Secret,
        config: ServerConfig,
    ) -> Result<Self, RemoteError> {
        config.limits.validate()?;
        let store = Store::open(path.as_ref(), &operator_token, &config.limits)?;
        let slots = Arc::new(Semaphore::new(config.limits.max_connections));
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                store,
                sessions: HashMap::new(),
                active: HashMap::new(),
                poisoned: false,
            })),
            config: Arc::new(config),
            slots,
            shutdown: CancellationToken::new(),
        })
    }
    fn with<T>(
        &self,
        operation: impl FnOnce(&mut Inner) -> Result<T, RemoteError>,
    ) -> Result<T, RemoteError> {
        let mut inner = self.inner.lock().map_err(|_| RemoteError::Storage)?;
        if inner.poisoned {
            return Err(RemoteError::Storage);
        }
        let result = operation(&mut inner);
        if matches!(result, Err(RemoteError::Storage)) {
            inner.poisoned = true;
            for session in inner.sessions.values() {
                session.cancel.cancel();
            }
            self.shutdown.cancel();
        }
        result
    }
    fn disconnect(&self, device: &str, generation: u64) -> Result<(), RemoteError> {
        self.with(|inner| {
            if inner
                .sessions
                .get(device)
                .is_some_and(|s| s.generation == generation)
                && let Some(session) = inner.sessions.remove(device)
            {
                session.cancel.cancel();
            }
            inner.unknown_generation(device, generation)
        })
    }
    fn close_all(&self) {
        self.shutdown.cancel();
        let _ = self.with(|inner| {
            let sessions = inner.sessions.drain().collect::<Vec<_>>();
            for (id, session) in sessions {
                session.cancel.cancel();
                inner.unknown_generation(&id, session.generation)?;
            }
            Ok(())
        });
    }
    fn attach(
        &self,
        device: &str,
        secret: &str,
        limits: Limits,
    ) -> Result<(u64, CancellationToken, mpsc::Receiver<String>), RemoteError> {
        self.with(|inner| {
            if !inner.store.device_auth(device, secret) {
                return Err(RemoteError::Unauthorized);
            }
            if let Some(old) = inner.sessions.remove(device) {
                old.cancel.cancel();
                inner.unknown_generation(device, old.generation)?;
            }
            let generation = inner.store.next_generation(device)?;
            let cancel = self.shutdown.child_token();
            let (queue, receive) = mpsc::channel(limits.queue_capacity);
            inner.sessions.insert(
                device.to_owned(),
                Session {
                    generation,
                    cancel: cancel.clone(),
                    queue,
                    limits,
                    admitted: 0,
                    executing: HashSet::new(),
                },
            );
            Ok((generation, cancel, receive))
        })
    }
    fn operator(&self, request: Envelope) -> Result<Envelope, RemoteError> {
        let Envelope {
            request_id,
            device_id,
            generation,
            body,
            ..
        } = request;
        let limit = &self.config.limits;
        self.with(|inner| {
            let reply = match body {
                Body::Pair { ttl_ms } => {
                    if !device_id.is_empty()
                        || generation != 0
                        || ttl_ms == 0
                        || Duration::from_millis(ttl_ms) > limit.max_pair_ttl
                    {
                        return Err(RemoteError::InvalidRequest);
                    }
                    let (code, expires_at_ms) = inner.store.pair(Duration::from_millis(ttl_ms))?;
                    Body::Pairing {
                        code,
                        expires_at_ms,
                    }
                }
                _ if device_id.is_empty() => return Err(RemoteError::InvalidRequest),
                Body::DeviceStatus => {
                    let mut status = inner.store.device(&device_id)?;
                    status.connected = inner.sessions.contains_key(&device_id);
                    Body::DeviceInfo { status }
                }
                Body::Status => Body::Record {
                    record: inner.store.request(&device_id, &request_id)?.1,
                },
                Body::Revoke => {
                    inner.store.revoke(&device_id)?;
                    if let Some(session) = inner.sessions.remove(&device_id) {
                        session.cancel.cancel();
                        inner.unknown_generation(&device_id, session.generation)?;
                    }
                    Body::Ack
                }
                Body::Invoke { payload } => {
                    if inner.store.device(&device_id)?.revoked {
                        return Err(RemoteError::Revoked);
                    }
                    let payload_hash = hash(
                        &serde_json::to_vec(&payload).map_err(|_| RemoteError::InvalidRequest)?,
                    );
                    match inner.store.request(&device_id, &request_id) {
                        Ok((old_hash, record)) => {
                            if old_hash != payload_hash {
                                return Err(RemoteError::Conflict);
                            }
                            return Ok(Envelope::new(
                                request_id,
                                device_id,
                                record.generation,
                                Body::Record { record },
                            ));
                        }
                        Err(RemoteError::NotFound) => {}
                        Err(e) => return Err(e),
                    }
                    let session = inner
                        .sessions
                        .get(&device_id)
                        .ok_or(RemoteError::Offline)?
                        .clone();
                    if generation != 0 && generation != session.generation {
                        return Err(RemoteError::Conflict);
                    }
                    if session.executing.len() >= session.limits.max_inflight_per_device
                        || session.admitted >= session.limits.max_records
                    {
                        return Err(RemoteError::Busy);
                    }
                    // Reserve queue capacity before committing the intent. No await
                    // occurs between the durable insert and enqueue.
                    let permit = session.queue.try_reserve().map_err(|_| RemoteError::Busy)?;
                    let wire = Envelope::new(
                        request_id.clone(),
                        device_id.clone(),
                        session.generation,
                        Body::Execute { payload },
                    )
                    .encode(session.limits.max_frame_bytes)?;
                    let record = RequestRecord {
                        request_id: request_id.clone(),
                        device_id: device_id.clone(),
                        generation: session.generation,
                        state: RequestState::Running,
                        value: None,
                        error: None,
                    };
                    inner.store.insert(&payload_hash, &record)?;
                    let live = inner.sessions.get_mut(&device_id).unwrap();
                    live.admitted += 1;
                    live.executing.insert(request_id.clone());
                    inner.active.insert(
                        (device_id.clone(), request_id.clone()),
                        Active {
                            generation: session.generation,
                            expires: Instant::now() + limit.request_timeout,
                        },
                    );
                    permit.send(wire);
                    Body::Record { record }
                }
                Body::Cancel => {
                    let mut record = inner.store.request(&device_id, &request_id)?.1;
                    if !record.state.is_terminal() {
                        let session = inner
                            .sessions
                            .get(&device_id)
                            .filter(|s| s.generation == record.generation)
                            .cloned();
                        if let Some(session) = session {
                            let wire = Envelope::new(
                                request_id.clone(),
                                device_id.clone(),
                                record.generation,
                                Body::Cancel,
                            )
                            .encode(limit.max_frame_bytes)?;
                            let permit =
                                session.queue.try_reserve().map_err(|_| RemoteError::Busy)?;
                            record.state = RequestState::CancelRequested;
                            inner.store.update(&record)?;
                            permit.send(wire);
                        } else {
                            inner.unknown(&device_id, &request_id)?;
                            record = inner.store.request(&device_id, &request_id)?.1;
                        }
                    }
                    Body::Record { record }
                }
                _ => return Err(RemoteError::InvalidRequest),
            };
            let generation = match &reply {
                Body::Record { record } => record.generation,
                Body::DeviceInfo { status } => status.generation,
                _ => generation,
            };
            Ok(Envelope::new(request_id, device_id, generation, reply))
        })
    }
    fn outcome(&self, device: &str, generation: u64, request: Envelope) -> Result<(), RemoteError> {
        request.validate()?;
        if request.device_id != device || request.generation != generation {
            return Err(RemoteError::Unauthorized);
        }
        let Body::Outcome { value, error } = request.body else {
            return Err(RemoteError::InvalidRequest);
        };
        if (value.is_some() && error.is_some()) || (value.is_none() && error.is_none()) {
            return Err(RemoteError::InvalidRequest);
        }
        self.with(|inner| {
            if inner.store.device(device)?.revoked
                || inner
                    .sessions
                    .get(device)
                    .is_none_or(|s| s.generation != generation)
            {
                return Err(RemoteError::Revoked);
            }
            inner
                .sessions
                .get_mut(device)
                .unwrap()
                .executing
                .remove(&request.request_id);
            let key = (device.to_owned(), request.request_id.clone());
            // Terminal rows and earlier generations can never be overwritten.
            if inner
                .active
                .get(&key)
                .is_none_or(|a| a.generation != generation)
            {
                return Ok(());
            }
            let mut record = inner.store.request(device, &request.request_id)?.1;
            if record.generation != generation || record.state.is_terminal() {
                return Ok(());
            }
            // The maintenance tick is only housekeeping. Settlement must test
            // the actual deadline under the same lock as generation/revocation
            // and the durable transition, even between maintenance ticks.
            if inner.active[&key].expires <= Instant::now() {
                inner.unknown(device, &request.request_id)?;
                return Ok(());
            }
            record.state = match error {
                None => RequestState::Completed,
                Some(RemoteError::Cancelled) => RequestState::Cancelled,
                Some(RemoteError::OutcomeUnknown) => RequestState::OutcomeUnknown,
                Some(_) => RequestState::Failed,
            };
            record.value = value;
            record.error = error;
            let query_reply = Envelope::new(
                record.request_id.clone(),
                record.device_id.clone(),
                record.generation,
                Body::Record {
                    record: record.clone(),
                },
            );
            if matches!(
                query_reply.encode(self.config.limits.max_frame_bytes),
                Err(RemoteError::TooLarge)
            ) {
                // Execution may already have changed state: losing its full
                // result cannot honestly be reported as a known failure.
                record.state = RequestState::OutcomeUnknown;
                record.value = None;
                record.error = Some(RemoteError::OutcomeUnknown);
            }
            inner.store.update(&record)?;
            inner.active.remove(&key);
            Ok(())
        })
    }
    fn expire(&self) -> Result<(), RemoteError> {
        self.with(|inner| {
            let expired = inner
                .active
                .iter()
                .filter(|(_, a)| a.expires <= Instant::now())
                .map(|(k, a)| (k.clone(), a.generation))
                .collect::<Vec<_>>();
            for ((device, id), generation) in expired {
                inner.unknown(&device, &id)?;
                if let Some(session) = inner
                    .sessions
                    .get(&device)
                    .filter(|s| s.generation == generation)
                {
                    let wire = Envelope::new(id, device.clone(), generation, Body::Cancel)
                        .encode(self.config.limits.max_frame_bytes)?;
                    if session.queue.try_send(wire).is_err() {
                        session.cancel.cancel();
                    }
                }
            }
            Ok(())
        })
    }
}
impl Inner {
    fn unknown(&mut self, device: &str, id: &str) -> Result<(), RemoteError> {
        let mut record = self.store.request(device, id)?.1;
        if !record.state.is_terminal() {
            record.state = RequestState::OutcomeUnknown;
            record.value = None;
            record.error = Some(RemoteError::OutcomeUnknown);
            self.store.update(&record)?;
        }
        self.active.remove(&(device.to_owned(), id.to_owned()));
        Ok(())
    }
    fn unknown_generation(&mut self, device: &str, generation: u64) -> Result<(), RemoteError> {
        let keys = self
            .active
            .iter()
            .filter(|((d, _), a)| d == device && a.generation == generation)
            .map(|((_, id), _)| id.clone())
            .collect::<Vec<_>>();
        for id in keys {
            self.unknown(device, &id)?;
        }
        Ok(())
    }
}

/// Serves an already-bound listener. Non-loopback listeners require an explicit
/// existing TLS termination declaration. Forwarded headers are never trusted.
pub async fn serve(
    listener: TcpListener,
    state: ServerState,
    cancel: CancellationToken,
) -> Result<(), RemoteError> {
    let _guard = ServerDropGuard(state.clone());
    if !listener
        .local_addr()
        .map_err(|_| RemoteError::Configuration)?
        .ip()
        .is_loopback()
        && !state.config.tls_terminated
    {
        return Err(RemoteError::TlsRequired);
    }
    let router = Router::new()
        .route("/v1/bind", get(bind_route))
        .route("/v1/operator", get(operator_route))
        .route("/v1/device", get(device_route))
        .with_state(state.clone());
    let cleanup = state.clone();
    let maintenance = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        loop {
            tokio::select! {
                biased;
                _ = cleanup.shutdown.cancelled() => break,
                _ = tick.tick() => { if cleanup.expire().is_err() { break; } }
            }
        }
    });
    let shutdown = state.clone();
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! { _ = cancel.cancelled() => {}, _ = shutdown.shutdown.cancelled() => {} }
            shutdown.close_all();
        })
        .await
        .map_err(|_| RemoteError::Disconnected);
    state.close_all();
    let _ = maintenance.await;
    // Upgraded sockets have their own tasks; wait for their bounded shutdown.
    if let Ok(permit) = timeout(
        state.config.limits.handshake_timeout,
        state
            .slots
            .clone()
            .acquire_many_owned(state.config.limits.max_connections as u32),
    )
    .await
    {
        drop(permit);
    }
    result
}

struct ServerDropGuard(ServerState);
impl Drop for ServerDropGuard {
    fn drop(&mut self) {
        self.0.close_all();
    }
}

fn bearer(headers: &HeaderMap) -> Result<&str, RemoteError> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(RemoteError::Unauthorized)
}
fn preflight(
    state: &ServerState,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<(), StatusCode> {
    if state.shutdown.is_cancelled() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if uri.query().is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(origin) = headers.get("origin")
        && (origin.to_str().is_err()
            || state.config.allowed_origin.as_deref() != origin.to_str().ok())
    {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}
fn websocket(ws: WebSocketUpgrade, state: &ServerState) -> WebSocketUpgrade {
    let max = state.config.limits.max_frame_bytes;
    ws.max_message_size(max)
        .max_frame_size(max)
        .write_buffer_size(0)
        .max_write_buffer_size(max * 2 + 1024)
}
async fn bind_route(
    State(state): State<ServerState>,
    uri: Uri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(code) = preflight(&state, &headers, &uri) {
        return code.into_response();
    }
    let Ok(permit) = state.slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    websocket(ws, &state).on_upgrade(move |mut ws| async move {
        let _permit = permit;
        let request = receive(&mut ws, &state).await;
        let response = match request {
            Ok(request) => {
                let request_id = request.request_id.clone();
                let result = (|| {
                    request.validate()?;
                    if !request.device_id.is_empty() || request.generation != 0 {
                        return Err(RemoteError::InvalidRequest);
                    }
                    let Body::Bind {
                        bind_id,
                        code,
                        secret,
                    } = request.body
                    else {
                        return Err(RemoteError::InvalidRequest);
                    };
                    if bind_id != request_id {
                        return Err(RemoteError::InvalidRequest);
                    }
                    let device = state.with(|inner| {
                        inner
                            .store
                            .bind(&bind_id, &code, &secret, state.config.limits.bind_ttl)
                    })?;
                    Ok(Envelope::new(request_id.clone(), device, 0, Body::Bound))
                })();
                result.unwrap_or_else(|error| {
                    Envelope::new(request_id, String::new(), 0, Body::Error { error })
                })
            }
            Err(error) => Envelope::new("error".into(), String::new(), 0, Body::Error { error }),
        };
        let _ = send(&mut ws, &state, response).await;
        let _ = timeout(state.config.limits.handshake_timeout, ws.close()).await;
    })
}
async fn operator_route(
    State(state): State<ServerState>,
    uri: Uri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(code) = preflight(&state, &headers, &uri) {
        return code.into_response();
    }
    if !bearer(&headers).is_ok_and(|token| {
        state
            .with(|inner| Ok(inner.store.operator_auth(token)))
            .unwrap_or(false)
    }) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(permit) = state.slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    websocket(ws, &state).on_upgrade(move |mut ws| async move {
        let _permit = permit;
        let response = match receive(&mut ws, &state).await {
            Ok(request) => {
                let (id, device, generation) = (
                    request.request_id.clone(),
                    request.device_id.clone(),
                    request.generation,
                );
                request
                    .validate()
                    .and_then(|()| state.operator(request))
                    .unwrap_or_else(|error| {
                        Envelope::new(id, device, generation, Body::Error { error })
                    })
            }
            Err(error) => Envelope::new("error".into(), String::new(), 0, Body::Error { error }),
        };
        let _ = send(&mut ws, &state, response).await;
        let _ = timeout(state.config.limits.handshake_timeout, ws.close()).await;
    })
}
async fn device_route(
    State(state): State<ServerState>,
    uri: Uri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(code) = preflight(&state, &headers, &uri) {
        return code.into_response();
    }
    let device = headers
        .get("x-device-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let secret = match bearer(&headers) {
        Ok(s) => s.to_owned(),
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !valid_id(&device)
        || !state
            .with(|inner| Ok(inner.store.device_auth(&device, &secret)))
            .unwrap_or(false)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(permit) = state.slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    websocket(ws, &state).on_upgrade(move |mut ws| async move {
        let _permit = permit;
        let hello = match receive(&mut ws, &state).await { Ok(r) => r, Err(_) => return };
        let negotiated = (|| {
            hello.validate()?;
            if hello.device_id != device || hello.generation != 0 {
                return Err(RemoteError::InvalidRequest);
            }
            let Body::Hello {max_frame_bytes, max_inflight, max_records, queue_capacity} = hello.body else {
                return Err(RemoteError::InvalidRequest);
            };
            let mut offered = state.config.limits.clone();
            offered.max_frame_bytes = max_frame_bytes;
            offered.max_inflight_per_device = max_inflight;
            offered.max_records = max_records;
            offered.queue_capacity = queue_capacity;
            offered.validate()?;
            let configured = &state.config.limits;
            offered.max_frame_bytes = max_frame_bytes.min(configured.max_frame_bytes);
            offered.max_inflight_per_device = max_inflight.min(configured.max_inflight_per_device);
            offered.max_records = max_records.min(configured.max_records);
            offered.queue_capacity = queue_capacity.min(configured.queue_capacity);
            Ok(offered)
        })();
        let limits = match negotiated {
            Ok(limits) => limits,
            Err(error) => {
                let _ = send(&mut ws, &state, Envelope::new(hello.request_id, device, 0, Body::Error {error})).await;
                return;
            }
        };
        // Store the negotiated limits before the session becomes visible to
        // operators. Excess work cannot acquire a durable running record.
        let (generation, cancel, mut queue) = match state.attach(&device, &secret, limits.clone()) {Ok(s) => s, Err(_) => return};
        let welcome = Envelope::new(hello.request_id, device.clone(), generation, Body::Welcome {max_frame_bytes: limits.max_frame_bytes, max_inflight:limits.max_inflight_per_device, max_records:limits.max_records, queue_capacity:limits.queue_capacity, heartbeat_interval_ms: limits.heartbeat_interval.as_millis() as u64, heartbeat_timeout_ms: limits.heartbeat_timeout.as_millis() as u64});
        if send(&mut ws, &state, welcome).await.is_err() {
            let _ = state.disconnect(&device, generation);
            return;
        }
        let mut tick = tokio::time::interval(limits.heartbeat_interval);
        let mut seen = Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = tick.tick() => {
                    if seen.elapsed() >= limits.heartbeat_timeout { break; }
                    if timeout(limits.handshake_timeout, ws.send(Message::Ping(Vec::new().into()))).await.map_or(true, |r| r.is_err()) { break; }
                }
                wire = queue.recv() => {
                    let Some(wire) = wire else { break; };
                    if timeout(limits.handshake_timeout, ws.send(Message::Text(wire.into()))).await.map_or(true, |r| r.is_err()) { break; }
                }
                frame = ws.recv() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            seen = Instant::now();
                            if text.len() > limits.max_frame_bytes { break; }
                            let request = serde_json::from_str::<Envelope>(&text).map_err(|_| RemoteError::InvalidRequest);
                            if request.and_then(|r| state.outcome(&device, generation, r)).is_err() { break; }
                        }
                        Some(Ok(Message::Pong(_))) => seen = Instant::now(),
                        Some(Ok(Message::Ping(bytes))) => {
                            seen = Instant::now();
                            if timeout(limits.handshake_timeout, ws.send(Message::Pong(bytes))).await.map_or(true, |r| r.is_err()) { break; }
                        }
                        _ => break,
                    }
                }
            }
        }
        // Fence durable state before close can block on the peer.
        let _ = state.disconnect(&device, generation);
        let _ = timeout(limits.handshake_timeout, ws.close()).await;
    })
}
async fn receive(ws: &mut WebSocket, state: &ServerState) -> Result<Envelope, RemoteError> {
    let frame = tokio::select! {
        _ = state.shutdown.cancelled() => return Err(RemoteError::Disconnected),
        value = timeout(state.config.limits.handshake_timeout, ws.recv()) => value.map_err(|_| RemoteError::Timeout)?,
    };
    let Some(Ok(Message::Text(text))) = frame else {
        return Err(RemoteError::InvalidRequest);
    };
    serde_json::from_str(&text).map_err(|_| RemoteError::InvalidRequest)
}
async fn send(
    ws: &mut WebSocket,
    state: &ServerState,
    response: Envelope,
) -> Result<(), RemoteError> {
    let wire = response.encode(state.config.limits.max_frame_bytes)?;
    timeout(
        state.config.limits.handshake_timeout,
        ws.send(Message::Text(wire.into())),
    )
    .await
    .map_err(|_| RemoteError::Timeout)?
    .map_err(|_| RemoteError::Disconnected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outcome_settlement_enforces_exact_deadline_without_maintenance_and_keeps_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let state = ServerState::open(
            dir.path().join("db"),
            Secret::generate().unwrap(),
            ServerConfig::default(),
        )
        .unwrap();
        let secret = Secret::generate().unwrap();
        let device = state
            .with(|inner| {
                let code = inner.store.pair(Duration::from_secs(60))?.0;
                inner
                    .store
                    .bind("bind", &code, secret.expose(), Duration::from_secs(60))
            })
            .unwrap();
        let (generation, _, _queue) = state
            .attach(&device, secret.expose(), Limits::default())
            .unwrap();
        for (id, error) in [
            ("late-success", None),
            ("late-cancel", Some(RemoteError::Cancelled)),
            ("late-failure", Some(RemoteError::HandlerFailed)),
        ] {
            state
                .operator(Envelope::new(
                    id.into(),
                    device.clone(),
                    0,
                    Body::Invoke { payload: json!({}) },
                ))
                .unwrap();
            // No maintenance task exists. The deadline equals the current instant,
            // so only outcome settlement itself can fence this response.
            state
                .with(|inner| {
                    inner
                        .active
                        .get_mut(&(device.clone(), id.into()))
                        .unwrap()
                        .expires = Instant::now();
                    Ok(())
                })
                .unwrap();
            let outcome = || {
                Envelope::new(
                    id.into(),
                    device.clone(),
                    generation,
                    Body::Outcome {
                        value: error.is_none().then(|| json!("late")),
                        error,
                    },
                )
            };
            state.outcome(&device, generation, outcome()).unwrap();
            let record = state
                .with(|inner| Ok(inner.store.request(&device, id)?.1))
                .unwrap();
            assert_eq!(record.state, RequestState::OutcomeUnknown);
            assert_eq!(record.value, None);
            assert_eq!(record.error, Some(RemoteError::OutcomeUnknown));
            state.outcome(&device, generation, outcome()).unwrap();
            assert_eq!(
                state
                    .with(|inner| Ok(inner.store.request(&device, id)?.1))
                    .unwrap(),
                record
            );
            assert!(
                !state
                    .with(|inner| Ok(inner.active.contains_key(&(device.clone(), id.into()))))
                    .unwrap()
            );
        }
        state
            .operator(Envelope::new(
                "on-time".into(),
                device.clone(),
                0,
                Body::Invoke { payload: json!({}) },
            ))
            .unwrap();
        state
            .outcome(
                &device,
                generation,
                Envelope::new(
                    "on-time".into(),
                    device.clone(),
                    generation,
                    Body::Outcome {
                        value: Some(json!(42)),
                        error: None,
                    },
                ),
            )
            .unwrap();
        assert_eq!(
            state
                .with(|inner| Ok(inner.store.request(&device, "on-time")?.1.state))
                .unwrap(),
            RequestState::Completed
        );
    }
}
