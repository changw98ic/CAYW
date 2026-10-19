use crate::{
    PROTOCOL_VERSION, RpcError, RpcHandler, ServerConfig,
    wire::{self, CancelParams, Frames, HelloParams, HelloResult, Request, Response},
};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    io,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream, tcp::OwnedWriteHalf},
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;

/// Accept authenticated connections on a loopback-only listener until cancelled.
/// The returned future owns all connection tasks; normal shutdown waits for their cleanup.
pub async fn serve(
    listener: TcpListener,
    handler: Arc<dyn RpcHandler>,
    config: ServerConfig,
    cancel: CancellationToken,
) -> io::Result<()> {
    config.validate()?;
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC listener must bind to a loopback address",
        ));
    }
    let config = Arc::new(config);
    let stop = cancel.child_token();
    let mut connections = JoinSet::new();
    // Declared after the JoinSet, so dropping serve cancels tokens before aborting connections.
    let _on_drop = stop.clone().drop_guard();
    let result = loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break Ok(()),
            _ = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, address) = match accepted { Ok(value) => value, Err(error) => break Err(error) };
                if !address.ip().is_loopback() || connections.len() >= config.max_connections { continue; }
                connections.spawn(connection(stream, handler.clone(), config.clone(), stop.child_token()));
            }
        }
    };
    stop.cancel();
    let _ = timeout(config.cancellation_grace, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.shutdown().await;
    result
}

async fn connection(
    stream: TcpStream,
    handler: Arc<dyn RpcHandler>,
    config: Arc<ServerConfig>,
    stop: CancellationToken,
) {
    let _on_drop = stop.clone().drop_guard();
    let _ = stream.set_nodelay(true);
    let (reader, mut writer) = stream.into_split();
    let mut frames = Frames::new(reader, config.max_frame_bytes, Some(config.frame_timeout));
    let hello_id = tokio::select! {
        biased;
        _ = stop.cancelled() => return,
        result = timeout(config.handshake_timeout, handshake(&mut frames, &mut writer, &config)) => {
            match result { Ok(Ok(id)) => id, _ => return }
        }
    };
    let (tx, rx) = mpsc::channel(config.response_queue_capacity);
    tokio::join!(
        async {
            dispatch(frames, tx, handler, &config, &stop, hello_id).await;
            stop.cancel();
        },
        async {
            write_responses(writer, rx, &stop).await;
            stop.cancel();
        }
    );
}

async fn handshake<R: tokio::io::AsyncRead + Unpin>(
    frames: &mut Frames<R>,
    writer: &mut OwnedWriteHalf,
    config: &ServerConfig,
) -> io::Result<String> {
    let bytes = frames
        .next()
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing IPC hello"))?;
    let request: Request =
        wire::decode(&bytes, config.max_frame_bytes).map_err(|_| protocol_error())?;
    if !wire::valid_id(&request.id, config.max_id_bytes) {
        return Err(protocol_error());
    }
    let result = if request.version != PROTOCOL_VERSION {
        Err(RpcError::new(
            "unsupported_version",
            "unsupported IPC version; expected version 1",
        ))
    } else if request.method != "hello" {
        Err(RpcError::new(
            "handshake_required",
            "hello authentication must be the first request",
        ))
    } else {
        match serde_json::from_value::<HelloParams>(request.params) {
            Ok(params) if token_matches(&params.token, &config.token) => Ok(json!(HelloResult {
                version: PROTOCOL_VERSION,
                max_frame_bytes: config.max_frame_bytes,
                max_in_flight: config.max_in_flight,
                max_requests_per_connection: config.max_requests_per_connection,
                max_id_bytes: config.max_id_bytes,
            })),
            _ => Err(RpcError::new("unauthorized", "IPC authentication failed")),
        }
    };
    let accepted = result.is_ok();
    let response = Response::new(request.id.clone(), result);
    writer
        .write_all(&wire::encode(&response, config.max_frame_bytes)?)
        .await?;
    if !accepted {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC hello rejected",
        ));
    }
    Ok(request.id)
}

// No diagnostics contain either token. This comparison avoids an early exit on the
// first differing byte; the local transport does not claim to be a cryptographic channel.
fn token_matches(actual: &str, expected: &str) -> bool {
    let mut difference = actual.len() ^ expected.len();
    for (index, byte) in expected.bytes().enumerate() {
        difference |= usize::from(byte ^ actual.as_bytes().get(index).copied().unwrap_or(0));
    }
    difference == 0
}

fn protocol_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid IPC protocol frame")
}

struct Outbound {
    bytes: Vec<u8>,
    deadline: Instant,
    written: Option<oneshot::Sender<()>>,
}

fn queue_response(
    tx: &mpsc::Sender<Outbound>,
    response: Response,
    config: &ServerConfig,
    written: Option<oneshot::Sender<()>>,
) -> bool {
    let valid = match &response.payload {
        wire::Payload::Result { result } => wire::validate_value(result, 1),
        wire::Payload::Error { .. } => Ok(()),
    };
    let bytes = valid
        .and_then(|_| wire::encode(&response, config.max_frame_bytes))
        .or_else(|_| {
            wire::encode(
                &Response::new(
                    response.id,
                    Err(RpcError::new(
                        "response_too_large_outcome_unknown",
                        "response exceeded the JSON depth or frame limit; reconcile runtime state",
                    )),
                ),
                config.max_frame_bytes,
            )
        });
    let Ok(bytes) = bytes else {
        return false;
    };
    tx.try_send(Outbound {
        bytes,
        deadline: Instant::now() + config.write_timeout,
        written,
    })
    .is_ok()
}

async fn write_responses(
    mut writer: OwnedWriteHalf,
    mut rx: mpsc::Receiver<Outbound>,
    stop: &CancellationToken,
) {
    loop {
        let frame = tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            frame = rx.recv() => match frame { Some(frame) => frame, None => return },
        };
        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            result = timeout_at(frame.deadline, writer.write_all(&frame.bytes)) => result,
        };
        if !matches!(result, Ok(Ok(()))) {
            return;
        }
        if let Some(written) = frame.written {
            let _ = written.send(());
        }
    }
}

type CallResult = (String, Result<Value, RpcError>);

/// Owned by the connection, never spawned. Drop cancels before dropping the inner future.
struct OwnedCall {
    cancel: CancellationToken,
    future: Pin<Box<dyn Future<Output = CallResult> + Send>>,
}

impl Future for OwnedCall {
    type Output = CallResult;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}
impl Drop for OwnedCall {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn call_handler(
    request: Request,
    handler: Arc<dyn RpcHandler>,
    token: CancellationToken,
    config: &ServerConfig,
) -> OwnedCall {
    let request_timeout = config.request_timeout;
    let grace = config.cancellation_grace;
    let cancel = token.clone();
    let future = Box::pin(async move {
        let work = AssertUnwindSafe(handler.call(&request.method, request.params, token.clone()))
            .catch_unwind();
        tokio::pin!(work);
        let interruption = tokio::select! {
            biased;
            _ = token.cancelled() => RpcError::new("cancelled_outcome_unknown", "request cancelled; side effects may have happened"),
            _ = tokio::time::sleep(request_timeout) => RpcError::new("timeout_outcome_unknown", "request timed out; side effects may have happened"),
            result = &mut work => return (request.id, result.unwrap_or_else(|_| Err(RpcError::new("handler_panic_outcome_unknown", "handler panicked; reconcile runtime state")))),
        };
        token.cancel();
        let _ = timeout(grace, &mut work).await;
        (request.id, Err(interruption))
    });
    OwnedCall { cancel, future }
}

async fn dispatch<R: tokio::io::AsyncRead + Unpin>(
    mut frames: Frames<R>,
    tx: mpsc::Sender<Outbound>,
    handler: Arc<dyn RpcHandler>,
    config: &ServerConfig,
    stop: &CancellationToken,
    hello_id: String,
) {
    // bool marks IDs belonging to host calls (including rejected-busy calls), so cancellation
    // can never target hello, another cancel frame or a request from another connection.
    let mut seen = HashMap::new();
    seen.insert(hello_id, false);
    let mut active: HashMap<String, CancellationToken> = HashMap::new();
    let mut calls = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            Some((id, result)) = calls.next(), if !calls.is_empty() => {
                active.remove(&id);
                if !queue_response(&tx, Response::new(id, result), config, None) { break; }
            },
            frame = frames.next() => {
                let bytes = match frame { Ok(Some(bytes)) => bytes, _ => break };
                let request: Request = match wire::decode(&bytes, config.max_frame_bytes) { Ok(request) => request, Err(_) => break };
                if !wire::valid_id(&request.id, config.max_id_bytes)
                    || seen.contains_key(&request.id)
                    || seen.len() >= config.max_requests_per_connection
                { break; }
                if request.version != PROTOCOL_VERSION {
                    for token in active.values() { token.cancel(); }
                    let (written, wait) = oneshot::channel();
                    if queue_response(&tx, Response::new(request.id, Err(RpcError::new("unsupported_version", "unsupported IPC version; expected version 1"))), config, Some(written)) {
                        let _ = timeout(config.write_timeout, wait).await;
                    }
                    break;
                }
                if !wire::valid_method(&request.method) || request.method == "hello" { break; }
                if request.method == wire::CANCEL_METHOD {
                    seen.insert(request.id.clone(), false);
                    let params: CancelParams = match serde_json::from_value(request.params) { Ok(params) => params, Err(_) => break };
                    if seen.get(&params.request_id) != Some(&true) { break; }
                    let cancelled = if let Some(token) = active.get(&params.request_id) { token.cancel(); true } else { false };
                    if !queue_response(&tx, Response::new(request.id, Ok(json!({"cancelled":cancelled}))), config, None) { break; }
                } else {
                    seen.insert(request.id.clone(), true);
                    if active.len() >= config.max_in_flight {
                        if !queue_response(&tx, Response::new(request.id, Err(RpcError::new("busy", "connection handler limit reached; request was not dispatched"))), config, None) { break; }
                        continue;
                    }
                    let token = stop.child_token();
                    active.insert(request.id.clone(), token.clone());
                    calls.push(call_handler(request, handler.clone(), token, config));
                }
            }
        }
    }
    // Stop writes immediately on disconnect/protocol/queue failure, notify handlers, and
    // poll them during the grace period. Dropping FuturesUnordered drops every future.
    stop.cancel();
    for token in active.values() {
        token.cancel();
    }
    let _ = timeout(config.cancellation_grace, async {
        while calls.next().await.is_some() {}
    })
    .await;
}
