use crate::protocol::*;
use async_trait::async_trait;
use futures::{FutureExt, SinkExt, StreamExt};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    task::JoinSet,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{
        Error, Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};
use tokio_util::sync::CancellationToken;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The host owns permissions, execution and side-effect reconciliation. Returning
/// `Err(Cancelled)` means a known cancellation; return `OutcomeUnknown` whenever
/// the host cannot establish whether effects occurred. No implicit executors.
#[async_trait]
pub trait DeviceHandler: Send + Sync + 'static {
    async fn handle(&self, request: Value, cancel: CancellationToken)
    -> Result<Value, RemoteError>;
}

#[derive(Clone, Debug)]
pub struct OperatorClient {
    url: ServerUrl,
    token: Secret,
    limits: Limits,
}
impl OperatorClient {
    pub fn new(url: ServerUrl, token: Secret) -> Self {
        Self {
            url,
            token,
            limits: Limits::default(),
        }
    }
    pub fn with_limits(mut self, limits: Limits) -> Result<Self, RemoteError> {
        limits.validate()?;
        self.limits = limits;
        Ok(self)
    }
    async fn request(&self, id: &str, device: &str, body: Body) -> Result<Envelope, RemoteError> {
        if !valid_id(id) || (!device.is_empty() && !valid_id(device)) {
            return Err(RemoteError::InvalidRequest);
        }
        let response = exchange(
            &self.url,
            "operator",
            Some(&self.token),
            None,
            Envelope::new(id.into(), device.into(), 0, body),
            &self.limits,
        )
        .await?;
        if response.device_id != device {
            return Err(RemoteError::InvalidRequest);
        }
        Ok(response)
    }
    pub async fn pair(&self, ttl: Duration) -> Result<PairingCode, RemoteError> {
        let response = self
            .request(
                &random_id()?,
                "",
                Body::Pair {
                    ttl_ms: u64::try_from(ttl.as_millis())
                        .map_err(|_| RemoteError::InvalidRequest)?,
                },
            )
            .await?;
        match response.body {
            Body::Pairing {
                code,
                expires_at_ms,
            } => Ok(PairingCode {
                code: Secret::new(code)?,
                expires_at_ms,
            }),
            _ => Err(RemoteError::InvalidRequest),
        }
    }
    /// Returns an accepted durable record. Poll `status` to get its terminal
    /// result. Never allocates a new ID or retries after a transport failure.
    pub async fn invoke(
        &self,
        device: &str,
        request_id: &str,
        payload: Value,
    ) -> Result<RequestRecord, RemoteError> {
        record(
            self.request(request_id, device, Body::Invoke { payload })
                .await?,
        )
    }
    pub async fn status(
        &self,
        device: &str,
        request_id: &str,
    ) -> Result<RequestRecord, RemoteError> {
        record(self.request(request_id, device, Body::Status).await?)
    }
    pub async fn cancel(
        &self,
        device: &str,
        request_id: &str,
    ) -> Result<RequestRecord, RemoteError> {
        record(self.request(request_id, device, Body::Cancel).await?)
    }
    pub async fn revoke(&self, device: &str) -> Result<(), RemoteError> {
        match self
            .request(&random_id()?, device, Body::Revoke)
            .await?
            .body
        {
            Body::Ack => Ok(()),
            _ => Err(RemoteError::InvalidRequest),
        }
    }
    pub async fn device_status(&self, device: &str) -> Result<DeviceStatus, RemoteError> {
        match self
            .request(&random_id()?, device, Body::DeviceStatus)
            .await?
            .body
        {
            Body::DeviceInfo { status } => Ok(status),
            _ => Err(RemoteError::InvalidRequest),
        }
    }
}
fn record(response: Envelope) -> Result<RequestRecord, RemoteError> {
    match response.body {
        Body::Record { record }
            if record.request_id == response.request_id
                && record.device_id == response.device_id
                && record.generation == response.generation =>
        {
            Ok(record)
        }
        _ => Err(RemoteError::InvalidRequest),
    }
}
impl PendingBind {
    /// Idempotent for the binding recovery TTL. The Device creates and retains
    /// its own secret; the server response never sends a Device secret.
    pub async fn complete(&self, url: &ServerUrl) -> Result<DeviceCredential, RemoteError> {
        if !valid_id(&self.bind_id) {
            return Err(RemoteError::InvalidRequest);
        }
        let response = exchange(
            url,
            "bind",
            None,
            None,
            Envelope::new(
                self.bind_id.clone(),
                String::new(),
                0,
                Body::Bind {
                    bind_id: self.bind_id.clone(),
                    code: self.code.expose().into(),
                    secret: self.secret.expose().into(),
                },
            ),
            &Limits::default(),
        )
        .await?;
        if !matches!(response.body, Body::Bound)
            || !valid_id(&response.device_id)
            || response.generation != 0
        {
            return Err(RemoteError::InvalidRequest);
        }
        Ok(DeviceCredential {
            device_id: response.device_id,
            secret: self.secret.clone(),
        })
    }
}

/// One connection attempt. A caller may establish another connection after an
/// error, but no previously sent request is ever replayed by this client.
pub struct RemoteDevice;
impl RemoteDevice {
    pub async fn run(
        url: &ServerUrl,
        credential: &DeviceCredential,
        handler: Arc<dyn DeviceHandler>,
        cancel: CancellationToken,
    ) -> Result<(), RemoteError> {
        Self::run_with_limits(url, credential, handler, cancel, Limits::default()).await
    }
    /// Offers frame, inflight, queue and per-connection deduplication bounds in
    /// the version 1 hello. The server admits work using the agreed minima;
    /// heartbeat timing comes from the server. Cancelling/dropping this Future
    /// closes both transport halves and cancels/drops its owned handler tasks.
    pub async fn run_with_limits(
        url: &ServerUrl,
        credential: &DeviceCredential,
        handler: Arc<dyn DeviceHandler>,
        cancel: CancellationToken,
        limits: Limits,
    ) -> Result<(), RemoteError> {
        limits.validate()?;
        if !valid_id(&credential.device_id) {
            return Err(RemoteError::InvalidRequest);
        }
        let child = cancel.child_token();
        let _guard = CancelOnDrop(child.clone());
        let mut socket = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            result = connect(url, "device", Some(&credential.secret), Some(&credential.device_id), &limits) => result?,
        };
        let hello_id = random_id()?;
        let hello = Envelope::new(
            hello_id.clone(),
            credential.device_id.clone(),
            0,
            Body::Hello {
                max_frame_bytes: limits.max_frame_bytes,
                max_inflight: limits.max_inflight_per_device,
                max_records: limits.max_records,
                queue_capacity: limits.queue_capacity,
            },
        );
        let welcome = tokio::select! {
            biased;
            _ = child.cancelled() => return Ok(()),
            result = async {
                send(&mut socket, hello, &limits).await?;
                receive(&mut socket, &limits).await
            } => result?,
        };
        welcome.validate()?;
        if let Body::Error { error } = welcome.body {
            return Err(error);
        }
        if welcome.request_id != hello_id
            || welcome.device_id != credential.device_id
            || welcome.generation == 0
        {
            return Err(RemoteError::InvalidRequest);
        }
        let generation = welcome.generation;
        let Body::Welcome {
            max_frame_bytes,
            max_inflight,
            max_records,
            queue_capacity,
            heartbeat_interval_ms,
            heartbeat_timeout_ms,
        } = welcome.body
        else {
            return Err(RemoteError::InvalidRequest);
        };
        if max_frame_bytes > limits.max_frame_bytes
            || max_inflight > limits.max_inflight_per_device
            || max_records > limits.max_records
            || queue_capacity > limits.queue_capacity
        {
            return Err(RemoteError::InvalidRequest);
        }
        let mut limits = limits;
        limits.max_frame_bytes = max_frame_bytes;
        limits.max_inflight_per_device = max_inflight;
        limits.max_records = max_records;
        limits.queue_capacity = queue_capacity;
        limits.heartbeat_interval = Duration::from_millis(heartbeat_interval_ms);
        limits.heartbeat_timeout = Duration::from_millis(heartbeat_timeout_ms);
        limits.validate()?;
        let mut tasks = JoinSet::new();
        let mut tokens: HashMap<String, CancellationToken> = HashMap::new();
        let mut seen = HashSet::new();
        let (sink, mut reader) = socket.split();
        let (outbound, writes) = mpsc::channel::<(String, Message)>(limits.queue_capacity);
        let (control, controls) = mpsc::channel(limits.queue_capacity);
        let (written, mut acknowledgements) = mpsc::channel(limits.max_inflight_per_device);
        // Keep this future in the owner, rather than spawning it. Dropping or
        // cancelling the connection drops both socket halves and every queued
        // frame; no detached writer or close handshake can retain the socket.
        let writer = write_device_frames(sink, writes, controls, written, limits.handshake_timeout);
        tokio::pin!(writer);
        let mut tick = tokio::time::interval(limits.heartbeat_interval);
        let mut last_seen = Instant::now();
        let result = loop {
            tokio::select! {
                biased;
                _ = child.cancelled() => break Ok(()),
                result = &mut writer => break result,
                // Only release an execution slot after its outcome is written.
                // A blocked peer therefore cannot grow an unbounded reply queue.
                id = acknowledgements.recv() => {
                    let Some(id) = id else { break Err(RemoteError::Disconnected); };
                    tokens.remove(&id);
                }
                _ = tick.tick() => {
                    if last_seen.elapsed() >= limits.heartbeat_timeout { break Err(RemoteError::Disconnected); }
                    if control.try_send(Message::Ping(Vec::new().into())).is_err() { break Err(RemoteError::Busy); }
                }
                frame = reader.next() => {
                    match frame {
                        Some(Ok(Message::Pong(_))) => last_seen = Instant::now(),
                        Some(Ok(Message::Ping(payload))) => {
                            last_seen = Instant::now();
                            if control.try_send(Message::Pong(payload)).is_err() {break Err(RemoteError::Busy);}
                        }
                        Some(Ok(Message::Text(text))) => {
                            last_seen = Instant::now();
                            if text.len() > limits.max_frame_bytes { break Err(RemoteError::TooLarge); }
                            let request: Envelope = match serde_json::from_str(&text) {Ok(r) => r, Err(_) => break Err(RemoteError::InvalidRequest)};
                            if let Err(e) = request.validate() { break Err(e); }
                            if request.device_id != credential.device_id || request.generation != generation { break Err(RemoteError::Unauthorized); }
                            match request.body {
                                Body::Execute {payload} => {
                                    if seen.contains(&request.request_id) { continue; }
                                    if seen.len() >= limits.max_records || tokens.len() >= limits.max_inflight_per_device { break Err(RemoteError::Busy); }
                                    seen.insert(request.request_id.clone());
                                    let token = child.child_token();
                                    tokens.insert(request.request_id.clone(), token.clone());
                                    let handler = handler.clone();
                                    tasks.spawn(async move {
                                        // Catch a handler panic in this owned task, so cleanup
                                        // can join it without leaving a nested worker detached.
                                        let result = std::panic::AssertUnwindSafe(async move { handler.handle(payload, token).await })
                                            .catch_unwind().await.unwrap_or(Err(RemoteError::OutcomeUnknown));
                                        (request.request_id, result)
                                    });
                                }
                                Body::Cancel => { if let Some(token) = tokens.get(&request.request_id) { token.cancel(); } }
                                _ => break Err(RemoteError::InvalidRequest),
                            }
                        }
                        _ => break Err(RemoteError::Disconnected),
                    }
                }
                // Leave completed results in the bounded task set while the
                // writer queue is full. Reading cancels never waits on a send.
                completed = async {
                    let permit = outbound.reserve().await.map_err(|_| RemoteError::Disconnected)?;
                    Ok::<_, RemoteError>((permit, tasks.join_next().await))
                }, if !tasks.is_empty() => {
                    let Ok((permit, Some(Ok((id, result))))) = completed else {break Err(RemoteError::OutcomeUnknown)};
                    let (value, error) = match result {Ok(value) => (Some(value), None), Err(error) => (None, Some(error))};
                    let mut reply = Envelope::new(id.clone(), credential.device_id.clone(), generation, Body::Outcome {value, error});
                    let wire = match reply.encode(limits.max_frame_bytes) {
                        Ok(wire) => wire,
                        Err(RemoteError::TooLarge) => {
                            reply.body = Body::Outcome {value:None,error:Some(RemoteError::OutcomeUnknown)};
                            match reply.encode(limits.max_frame_bytes) {Ok(wire) => wire, Err(error) => break Err(error)}
                        }
                        Err(error) => break Err(error),
                    };
                    permit.send((id, Message::Text(wire.into())));
                }
            }
        };
        child.cancel();
        for token in tokens.values() {
            token.cancel();
        }
        // Give cancelled handlers a bounded opportunity to finish before aborting
        // any remaining work.
        let _ = timeout(Duration::from_millis(50), async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        tasks.shutdown().await;
        result
    }
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
async fn write_device_frames(
    mut sink: futures::stream::SplitSink<Socket, Message>,
    mut writes: mpsc::Receiver<(String, Message)>,
    mut controls: mpsc::Receiver<Message>,
    written: mpsc::Sender<String>,
    write_timeout: Duration,
) -> Result<(), RemoteError> {
    loop {
        let (id, frame) = tokio::select! {
            biased;
            frame = controls.recv() => (None, frame.ok_or(RemoteError::Disconnected)?),
            frame = writes.recv() => {
                let (id, frame) = frame.ok_or(RemoteError::Disconnected)?;
                (Some(id), frame)
            }
        };
        timeout(write_timeout, sink.send(frame))
            .await
            .map_err(|_| RemoteError::Timeout)?
            .map_err(|_| RemoteError::Disconnected)?;
        if let Some(id) = id {
            written.try_send(id).map_err(|_| RemoteError::Busy)?;
        }
    }
}

async fn connect(
    url: &ServerUrl,
    role: &str,
    token: Option<&Secret>,
    device: Option<&str>,
    limits: &Limits,
) -> Result<Socket, RemoteError> {
    let mut request = url
        .endpoint(role)
        .into_client_request()
        .map_err(|_| RemoteError::InvalidRequest)?;
    if let Some(token) = token {
        let mut header = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
            .map_err(|_| RemoteError::Configuration)?;
        header.set_sensitive(true);
        request.headers_mut().insert("authorization", header);
    }
    if let Some(device) = device {
        request.headers_mut().insert(
            "x-device-id",
            HeaderValue::from_str(device).map_err(|_| RemoteError::InvalidRequest)?,
        );
    }
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(limits.max_frame_bytes);
    config.max_frame_size = Some(limits.max_frame_bytes);
    config.write_buffer_size = 0;
    config.max_write_buffer_size = limits.max_frame_bytes * 2 + 1024;
    // connect_async_with_config performs exactly one handshake: redirects are
    // returned as HTTP errors; no credential is forwarded to a redirect target.
    timeout(
        limits.handshake_timeout,
        connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| RemoteError::Timeout)?
    .map(|(socket, _)| socket)
    .map_err(|e| match e {
        Error::Http(response) if response.status().as_u16() == 401 => RemoteError::Unauthorized,
        _ => RemoteError::Disconnected,
    })
}
async fn exchange(
    url: &ServerUrl,
    role: &str,
    token: Option<&Secret>,
    device: Option<&str>,
    request: Envelope,
    limits: &Limits,
) -> Result<Envelope, RemoteError> {
    let request_id = request.request_id.clone();
    let mut socket = connect(url, role, token, device, limits).await?;
    send(&mut socket, request, limits).await?;
    let response = receive(&mut socket, limits).await?;
    response.validate()?;
    if response.request_id != request_id {
        return Err(RemoteError::InvalidRequest);
    }
    if let Body::Error { error } = response.body {
        return Err(error);
    }
    Ok(response)
}
async fn receive(socket: &mut Socket, limits: &Limits) -> Result<Envelope, RemoteError> {
    timeout(limits.handshake_timeout, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text).map_err(|_| RemoteError::InvalidRequest);
                }
                Some(Ok(Message::Ping(payload))) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|_| RemoteError::Disconnected)?;
                }
                Some(Ok(Message::Pong(_))) => {}
                _ => return Err(RemoteError::Disconnected),
            }
        }
    })
    .await
    .map_err(|_| RemoteError::Timeout)?
}
async fn send(socket: &mut Socket, message: Envelope, limits: &Limits) -> Result<(), RemoteError> {
    send_frame(
        socket,
        Message::Text(message.encode(limits.max_frame_bytes)?.into()),
        limits,
    )
    .await
}
async fn send_frame(
    socket: &mut Socket,
    frame: Message,
    limits: &Limits,
) -> Result<(), RemoteError> {
    timeout(limits.handshake_timeout, socket.send(frame))
        .await
        .map_err(|_| RemoteError::Timeout)?
        .map_err(|_| RemoteError::Disconnected)
}
