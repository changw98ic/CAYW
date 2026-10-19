use crate::{
    ClientConfig, ClientError, PROTOCOL_VERSION,
    wire::{self, Frames, HelloResult, Request, Response},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fmt,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::{
    io::AsyncWriteExt,
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{Notify, Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep_until, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;

type Reply = Result<Value, ClientError>;
// None is a transport-owned cancellation acknowledgment. It is still paired exactly once.
struct Pending {
    reply: Option<oneshot::Sender<Reply>>,
    deadline: Instant,
    sent: bool,
}

struct Metadata {
    pending: HashMap<String, Pending>,
    next: usize,
    closed: bool,
}

struct Outbound {
    id: String,
    bytes: Vec<u8>,
    deadline: Instant,
}

struct State {
    config: ClientConfig,
    metadata: Mutex<Metadata>,
    slots: Arc<Semaphore>,
    outgoing: mpsc::Sender<Outbound>,
    changed: Notify,
    stop: CancellationToken,
}

impl State {
    fn lock(&self) -> std::sync::MutexGuard<'_, Metadata> {
        self.metadata
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fail(&self, error: ClientError) {
        let pending = {
            let mut metadata = self.lock();
            metadata.closed = true;
            std::mem::take(&mut metadata.pending)
        };
        self.stop.cancel();
        for (_, pending) in pending {
            if let Some(reply) = pending.reply {
                let _ = reply.send(Err(error.clone()));
            }
        }
    }

    fn next_id(&self, metadata: &Metadata) -> Result<String, ClientError> {
        if metadata.closed {
            return Err(ClientError::known(
                "connection_closed",
                "connection is closed; this request was not sent",
            ));
        }
        if metadata.next >= self.config.max_requests_per_connection {
            return Err(ClientError::known(
                "connection_limit",
                "connection request ID budget exhausted; explicitly open a new connection",
            ));
        }
        // A fresh 128-bit nonce on every frame prevents a peer from predicting the
        // next business/cancel ID, even after observing earlier IDs on this connection.
        // The counter guarantees uniqueness without retaining an extra seen-ID set.
        let counter = format!("{:x}", metadata.next);
        if counter.len() + 33 > self.config.max_id_bytes {
            return Err(ClientError::known(
                "connection_limit",
                "connection ID width exhausted; explicitly open a new connection",
            ));
        }
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|_| {
            ClientError::known("id_unavailable", "cannot generate a fresh IPC request ID")
        })?;
        Ok(format!("{counter}-{:032x}", u128::from_be_bytes(nonce)))
    }

    fn admit(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(String, oneshot::Receiver<Reply>), ClientError> {
        wire::validate_value(&params, 1).map_err(|_| {
            ClientError::known(
                "invalid_params",
                "request params exceed the JSON nesting limit; request was not sent",
            )
        })?;
        let permit = self.outgoing.try_reserve().map_err(|_| {
            ClientError::known(
                "client_busy",
                "connection write queue is full or closed; request was not sent",
            )
        })?;
        let mut metadata = self.lock();
        let id = self.next_id(&metadata)?;
        if metadata.pending.len() >= self.config.max_in_flight.saturating_mul(2) {
            return Err(ClientError::known(
                "client_busy",
                "pending response limit reached; request was not sent",
            ));
        }
        let request = Request {
            version: PROTOCOL_VERSION,
            id: id.clone(),
            method: method.into(),
            params,
        };
        let bytes = wire::encode(&request, self.config.max_frame_bytes).map_err(|_| {
            ClientError::known(
                "request_too_large",
                "request exceeds the negotiated frame limit; request was not sent",
            )
        })?;
        let (reply, receiver) = oneshot::channel();
        metadata.next += 1;
        metadata.pending.insert(
            id.clone(),
            Pending {
                reply: Some(reply),
                deadline: Instant::now() + self.config.request_timeout,
                sent: false,
            },
        );
        permit.send(Outbound {
            id: id.clone(),
            bytes,
            deadline: Instant::now() + self.config.write_timeout,
        });
        self.changed.notify_one();
        Ok((id, receiver))
    }

    fn cancel_request(&self, target: &str) -> Result<(), ClientError> {
        let permit = self.outgoing.try_reserve().map_err(|_| {
            ClientError::unknown(
                "cancel_failed",
                "cannot queue cancellation; reconcile runtime state",
            )
        })?;
        let mut metadata = self.lock();
        // It is valid for the terminal result to have arrived just before cancellation.
        if !metadata.pending.contains_key(target) {
            return Ok(());
        }
        if metadata.pending.len() >= self.config.max_in_flight.saturating_mul(2) {
            return Err(ClientError::unknown(
                "cancel_failed",
                "pending response limit reached; reconcile runtime state",
            ));
        }
        let id = self.next_id(&metadata).map_err(|_| {
            ClientError::unknown(
                "cancel_failed",
                "cannot allocate a cancellation ID; reconcile runtime state",
            )
        })?;
        let request = Request {
            version: PROTOCOL_VERSION,
            id: id.clone(),
            method: wire::CANCEL_METHOD.into(),
            params: json!({"request_id":target}),
        };
        let bytes = wire::encode(&request, self.config.max_frame_bytes).map_err(|_| {
            ClientError::unknown(
                "cancel_failed",
                "cannot encode cancellation; reconcile runtime state",
            )
        })?;
        metadata.next += 1;
        metadata.pending.insert(
            id.clone(),
            Pending {
                reply: None,
                deadline: Instant::now() + self.config.request_timeout,
                sent: false,
            },
        );
        permit.send(Outbound {
            id,
            bytes,
            deadline: Instant::now() + self.config.write_timeout,
        });
        self.changed.notify_one();
        Ok(())
    }

    fn complete(&self, response: Response) -> bool {
        if response.version != PROTOCOL_VERSION
            || !wire::valid_id(&response.id, self.config.max_id_bytes)
        {
            return false;
        }
        let pending = {
            let mut metadata = self.lock();
            if !metadata
                .pending
                .get(&response.id)
                .is_some_and(|pending| pending.sent)
            {
                return false;
            }
            metadata.pending.remove(&response.id)
        };
        self.changed.notify_one();
        match pending {
            Some(Pending {
                reply: Some(reply), ..
            }) => {
                let _ = reply.send(response.into_result().map_err(ClientError::from));
                true
            }
            Some(Pending { reply: None, .. }) => match response.into_result() {
                Ok(result) => result.as_object().is_some_and(|object| {
                    object.len() == 1 && object.get("cancelled").is_some_and(Value::is_boolean)
                }),
                Err(_) => false,
            },
            None => false,
        }
    }
}

struct Owner {
    state: Arc<State>,
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.state.fail(ClientError::unknown(
            "connection_closed",
            "last client was dropped; reconcile any admitted calls",
        ));
    }
}

/// Cloneable handle to one authenticated connection. Clones share limits and closure.
/// Last-handle drop signals shutdown; [`Client::close`] also waits for socket tasks to exit.
#[derive(Clone)]
pub struct Client {
    owner: Arc<Owner>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("closed", &self.owner.state.lock().closed)
            .finish_non_exhaustive()
    }
}

// If an admitted request future is dropped, no caller can consume its terminal response.
// Close the shared connection, cancel remote work, and fail all remaining pending calls.
struct RequestGuard {
    state: Arc<State>,
    armed: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state.fail(ClientError::unknown(
                "request_dropped",
                "admitted request future was dropped; reconcile runtime state",
            ));
        }
    }
}

impl Client {
    /// Connect and authenticate. Only loopback addresses are accepted, without DNS lookup.
    pub async fn connect(
        addr: std::net::SocketAddr,
        token: impl Into<String>,
    ) -> Result<Self, ClientError> {
        Self::connect_with_config(addr, token, ClientConfig::default()).await
    }

    pub async fn connect_with_config(
        addr: SocketAddr,
        token: impl Into<String>,
        mut config: ClientConfig,
    ) -> Result<Self, ClientError> {
        config.validate()?;
        if !addr.ip().is_loopback() {
            return Err(ClientError::known(
                "invalid_address",
                "IPC client requires a loopback address",
            ));
        }
        let token = token.into();
        if token.is_empty() {
            return Err(ClientError::known(
                "invalid_config",
                "IPC token must not be empty",
            ));
        }
        let hello = wire::encode(&Request::hello(token), config.max_frame_bytes).map_err(|_| {
            ClientError::known(
                "invalid_config",
                "authentication frame exceeds configured limit",
            )
        })?;
        let (mut frames, writer, limits) = timeout(config.connect_timeout, async {
            let stream = TcpStream::connect(addr).await.map_err(|_| {
                ClientError::known("connect_failed", "cannot connect to IPC server")
            })?;
            let _ = stream.set_nodelay(true);
            let (reader, mut writer) = stream.into_split();
            writer
                .write_all(&hello)
                .await
                .map_err(|_| ClientError::known("handshake_failed", "cannot write IPC hello"))?;
            // Retain this reader after hello, including any buffered extra frames, so a
            // malicious peer cannot hide an unpaired response across the handshake.
            let mut frames = Frames::new(reader, config.max_frame_bytes, None);
            let bytes = frames
                .next()
                .await
                .map_err(|_| {
                    ClientError::known("handshake_failed", "cannot read IPC hello response")
                })?
                .ok_or_else(|| {
                    ClientError::known("handshake_failed", "connection closed during IPC hello")
                })?;
            let response: Response = wire::decode(&bytes, config.max_frame_bytes)
                .map_err(|_| ClientError::known("protocol_error", "invalid IPC hello response"))?;
            if response.version != PROTOCOL_VERSION || response.id != wire::HELLO_ID {
                return Err(ClientError::known(
                    "protocol_error",
                    "IPC hello version or ID mismatch",
                ));
            }
            let value = response.into_result().map_err(ClientError::from)?;
            let limits: HelloResult = serde_json::from_value(value)
                .map_err(|_| ClientError::known("protocol_error", "invalid IPC hello limits"))?;
            if limits.version != PROTOCOL_VERSION
                || limits.max_frame_bytes < 256
                || limits.max_in_flight == 0
                || limits.max_requests_per_connection < 2
                || limits.max_id_bytes == 0
            {
                return Err(ClientError::known(
                    "protocol_error",
                    "unsupported IPC version or limits",
                ));
            }
            Ok((frames, writer, limits))
        })
        .await
        .map_err(|_| ClientError::known("connect_timeout", "IPC connect/hello timed out"))??;
        config.max_frame_bytes = config.max_frame_bytes.min(limits.max_frame_bytes);
        config.max_in_flight = config.max_in_flight.min(limits.max_in_flight);
        config.max_requests_per_connection = config
            .max_requests_per_connection
            .min(limits.max_requests_per_connection);
        config.max_id_bytes = config.max_id_bytes.min(limits.max_id_bytes);
        config.validate()?;
        if config.max_id_bytes < 34 {
            return Err(ClientError::known(
                "protocol_error",
                "negotiated ID limit is too small for unpredictable request IDs",
            ));
        }
        frames.set_limit(config.max_frame_bytes);
        // No business or cancel request exists yet. Reject every buffered suffix,
        // including a partial frame, before a usable Client can admit any request.
        // Keep the same reader; later bytes still go through normal pairing checks.
        if frames.has_buffered_bytes() {
            return Err(ClientError::known(
                "protocol_error",
                "unsolicited IPC bytes after hello; no request has been sent",
            ));
        }
        let (outgoing, receiver) = mpsc::channel(config.write_queue_capacity);
        let state = Arc::new(State {
            slots: Arc::new(Semaphore::new(config.max_in_flight)),
            config,
            metadata: Mutex::new(Metadata {
                pending: HashMap::new(),
                next: 1,
                closed: false,
            }),
            outgoing,
            changed: Notify::new(),
            stop: CancellationToken::new(),
        });
        let task = tokio::spawn(run(frames, writer, receiver, state.clone()));
        Ok(Self {
            owner: Arc::new(Owner {
                state,
                task: tokio::sync::Mutex::new(Some(task)),
            }),
        })
    }

    /// Send one call. Admission fails immediately if local capacity or ID budget is exhausted.
    /// Dropping this future after admission closes the connection; use `request_with_cancel`
    /// when cancellation should preserve it and await a terminal response.
    pub async fn request(&self, method: &str, params: Value) -> Reply {
        self.request_with_cancel(method, params, CancellationToken::new())
            .await
    }

    /// Cancellation sends an authenticated `rpc.cancel` frame and waits for the original
    /// call's unique terminal response. The original request deadline remains in force.
    pub async fn request_with_cancel(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Reply {
        if !wire::valid_method(method) || method == "hello" || method == wire::CANCEL_METHOD {
            return Err(ClientError::known(
                "invalid_method",
                "invalid or reserved RPC method; request was not sent",
            ));
        }
        if cancel.is_cancelled() {
            return Err(ClientError::known(
                "request_cancelled",
                "cancelled before admission; request was not sent",
            ));
        }
        let state = &self.owner.state;
        let _slot = state.slots.clone().try_acquire_owned().map_err(|_| {
            ClientError::known(
                "client_busy",
                "client in-flight limit reached; request was not sent",
            )
        })?;
        let deadline = Instant::now() + state.config.request_timeout;
        let (id, mut reply) = state.admit(method, params)?;
        let mut guard = RequestGuard {
            state: state.clone(),
            armed: true,
        };
        let mut cancellation_sent = false;
        let result = loop {
            tokio::select! {
                biased;
                result = &mut reply => break result.unwrap_or_else(|_| Err(ClientError::unknown("connection_lost", "connection ended without a terminal response"))),
                _ = sleep_until(deadline) => {
                    let error = ClientError::unknown("request_timeout", "client deadline exceeded; reconcile runtime state before retrying");
                    state.fail(error.clone());
                    break Err(error);
                },
                _ = cancel.cancelled(), if !cancellation_sent => {
                    cancellation_sent = true;
                    if let Err(error) = state.cancel_request(&id) {
                        state.fail(error.clone());
                        break Err(error);
                    }
                },
            }
        };
        guard.armed = false;
        result
    }

    /// Close this connection for all clones, fail pending calls as unknown, and wait for
    /// both I/O halves to be dropped. This never opens a replacement connection.
    pub async fn close(&self) {
        self.owner.state.fail(ClientError::unknown(
            "connection_closed",
            "client closed the connection; reconcile admitted calls",
        ));
        let mut task = self.owner.task.lock().await;
        if let Some(handle) = task.as_mut() {
            let _ = handle.await;
        }
        *task = None;
    }
}

async fn run(
    frames: Frames<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    outgoing: mpsc::Receiver<Outbound>,
    state: Arc<State>,
) {
    tokio::join!(
        read_responses(frames, &state),
        write_requests(writer, outgoing, &state),
        expire_responses(&state),
    );
    state.fail(ClientError::unknown(
        "connection_lost",
        "IPC connection ended; reconcile admitted calls",
    ));
}

// Every pending response has a deadline, including internal cancel acknowledgments
// whose original caller has already received a terminal response.
async fn expire_responses(state: &State) {
    loop {
        let changed = state.changed.notified();
        let deadline = state
            .lock()
            .pending
            .values()
            .map(|pending| pending.deadline)
            .min();
        tokio::select! {
            biased;
            _ = state.stop.cancelled() => return,
            _ = changed => {},
            _ = async {
                match deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                // A response may have arrived just as the timer fired. Recheck atomically
                // against the pending set before closing the connection.
                let expired = state.lock().pending.values().any(|pending| pending.deadline <= Instant::now());
                if expired {
                    state.fail(ClientError::unknown("request_timeout", "response deadline exceeded; reconcile runtime state before retrying"));
                    return;
                }
            },
        }
    }
}

async fn read_responses(mut frames: Frames<OwnedReadHalf>, state: &State) {
    loop {
        let frame = tokio::select! {
            biased;
            _ = state.stop.cancelled() => return,
            frame = frames.next() => frame,
        };
        match frame {
            Ok(Some(bytes)) => {
                if let Ok(response) = wire::decode::<Response>(&bytes, state.config.max_frame_bytes)
                    && state.complete(response)
                {
                    continue;
                }
                state.fail(ClientError::unknown(
                    "protocol_error",
                    "invalid, duplicate or unpaired IPC response; connection closed",
                ));
                return;
            }
            _ => {
                state.fail(ClientError::unknown(
                    "connection_lost",
                    "IPC connection lost before a terminal response; reconcile runtime state",
                ));
                return;
            }
        }
    }
}

async fn write_requests(
    mut writer: OwnedWriteHalf,
    mut outgoing: mpsc::Receiver<Outbound>,
    state: &State,
) {
    loop {
        let frame = tokio::select! {
            biased;
            _ = state.stop.cancelled() => return,
            frame = outgoing.recv() => match frame { Some(frame) => frame, None => return },
        };
        let result = tokio::select! {
            biased;
            _ = state.stop.cancelled() => return,
            result = timeout_at(frame.deadline, async {
                writer.write_all(&frame.bytes).await?;
                // Publish sent in the same poll as successful write completion: a
                // fast valid response must not race an unrecorded successful write.
                if let Some(pending) = state.lock().pending.get_mut(&frame.id) {
                    pending.sent = true;
                }
                Ok::<(), std::io::Error>(())
            }) => result,
        };
        if !matches!(result, Ok(Ok(()))) {
            state.fail(ClientError::unknown(
                "write_failed",
                "IPC write failed or timed out; reconcile admitted calls",
            ));
            return;
        }
    }
}
