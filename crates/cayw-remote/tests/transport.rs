//! Backpressure regressions against an actual loopback WebSocket peer.
use async_trait::async_trait;
use cayw_remote::{
    CancellationToken, DeviceCredential, DeviceHandler, Limits, RemoteDevice, RemoteError, Secret,
    ServerUrl,
};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Notify};
use tokio_tungstenite::{accept_async, tungstenite::Message};

struct Replies {
    ready: Arc<Notify>,
    cancelled: Arc<Notify>,
    dropped: Arc<Notify>,
}
struct Dropped(Arc<Notify>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
#[async_trait]
impl DeviceHandler for Replies {
    async fn handle(
        &self,
        request: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RemoteError> {
        if request["wait"] == true {
            let _dropped = Dropped(self.dropped.clone());
            cancel.cancelled().await;
            self.cancelled.notify_one();
            Err(RemoteError::Cancelled)
        } else {
            let reply = json!("x".repeat(12 * 1024 * 1024));
            self.ready.notify_one();
            Ok(reply)
        }
    }
}

#[derive(Clone, Copy)]
enum Stop {
    Local,
    ProtocolCancel,
    InvalidFrame,
    WriterTimeout,
    HeartbeatLoss,
    Disconnect,
    DropOwner,
}

async fn backpressure(stop_kind: Stop) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = ServerUrl::new(&format!("ws://{}", listener.local_addr().unwrap()), false).unwrap();
    let ready = Arc::new(Notify::new());
    let peer_ready = ready.clone();
    let cancelled = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let sent = Arc::new(Notify::new());
    let peer_sent = sent.clone();
    let release = Arc::new(Notify::new());
    let peer_release = release.clone();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let hello: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        let (heartbeat_interval_ms, heartbeat_timeout_ms) =
            if matches!(stop_kind, Stop::HeartbeatLoss) {
                (1000, 3000)
            } else {
                (10000, 30000)
            };
        ws.send(Message::Text(json!({"version":1,"request_id":hello["request_id"],"device_id":"blocked-device","generation":1,"body":{"type":"welcome","max_frame_bytes":16*1024*1024,"max_inflight":16,"max_records":100_000,"queue_capacity":32,"heartbeat_interval_ms":heartbeat_interval_ms,"heartbeat_timeout_ms":heartbeat_timeout_ms}}).to_string().into())).await.unwrap();
        ws.send(Message::Text(json!({"version":1,"request_id":"active-wait","device_id":"blocked-device","generation":1,"body":{"type":"execute","payload":{"wait":true}}}).to_string().into())).await.unwrap();
        ws.send(Message::Text(json!({"version":1,"request_id":"large-result","device_id":"blocked-device","generation":1,"body":{"type":"execute","payload":null}}).to_string().into())).await.unwrap();
        peer_ready.notified().await;
        // Leave the peer unread so the valid 12 MiB result exceeds TCP buffers.
        // The exit bound below is much shorter than the configured write timeout.
        tokio::time::sleep(Duration::from_millis(700)).await;
        match stop_kind {
            Stop::ProtocolCancel => ws.send(Message::Text(json!({"version":1,"request_id":"active-wait","device_id":"blocked-device","generation":1,"body":{"type":"cancel"}}).to_string().into())).await.unwrap(),
            Stop::InvalidFrame => ws.send(Message::Text("invalid-json".into())).await.unwrap(),
            _ => {}
        }
        peer_sent.notify_one();
        if matches!(stop_kind, Stop::Disconnect) {
            drop(ws);
            return 0;
        }
        peer_release.notified().await;
        // Once the Device owner has returned/dropped, no detached writer may keep
        // this socket alive. Drain already-written bytes before observing EOF.
        let mut replies = 0;
        while let Some(frame) = ws.next().await {
            match frame {
                Ok(Message::Text(_)) => replies += 1,
                Ok(Message::Ping(bytes)) => {
                    let _ = ws.send(Message::Pong(bytes)).await;
                }
                Err(_) | Ok(Message::Close(_)) => break,
                _ => {}
            }
        }
        replies
    });
    let credential = DeviceCredential {
        device_id: "blocked-device".into(),
        secret: Secret::generate().unwrap(),
    };
    let stop = CancellationToken::new();
    let device_stop = stop.clone();
    let handler = Arc::new(Replies {
        ready,
        cancelled: cancelled.clone(),
        dropped: dropped.clone(),
    });
    let mut task = tokio::spawn(async move {
        RemoteDevice::run_with_limits(
            &url,
            &credential,
            handler,
            device_stop,
            Limits {
                max_frame_bytes: 16 * 1024 * 1024,
                handshake_timeout: if matches!(stop_kind, Stop::WriterTimeout) {
                    Duration::from_secs(3)
                } else {
                    Duration::from_secs(15)
                },
                ..Default::default()
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), sent.notified())
        .await
        .unwrap();
    match stop_kind {
        Stop::Local => stop.cancel(),
        Stop::DropOwner => task.abort(),
        _ => {}
    }
    if matches!(stop_kind, Stop::ProtocolCancel) {
        tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
            .await
            .expect("protocol cancellation must bypass a blocked result write");
        stop.cancel();
    }
    let bound = if matches!(stop_kind, Stop::WriterTimeout | Stop::HeartbeatLoss) {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(2)
    };
    let result = tokio::time::timeout(bound, &mut task)
        .await
        .expect("connection owner must exit without waiting for write plus close timeouts");
    match stop_kind {
        Stop::DropOwner => assert!(result.unwrap_err().is_cancelled()),
        Stop::Local | Stop::ProtocolCancel => result.unwrap().unwrap(),
        Stop::InvalidFrame => assert_eq!(result.unwrap(), Err(RemoteError::InvalidRequest)),
        Stop::WriterTimeout => assert_eq!(result.unwrap(), Err(RemoteError::Timeout)),
        Stop::HeartbeatLoss | Stop::Disconnect => {
            assert_eq!(result.unwrap(), Err(RemoteError::Disconnected))
        }
    }
    if !matches!(stop_kind, Stop::ProtocolCancel | Stop::DropOwner) {
        tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
            .await
            .expect("terminal transport events must cancel active handlers");
    }
    tokio::time::timeout(Duration::from_secs(2), dropped.notified())
        .await
        .expect("handler future must be cleaned up");
    release.notify_one();
    let replies = tokio::time::timeout(Duration::from_secs(3), peer)
        .await
        .expect("socket must not be retained by a detached writer")
        .unwrap();
    assert!(
        replies <= 1,
        "no queued outcomes may replay after termination"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_cancel_interrupts_blocked_result_write_and_closes_socket() {
    backpressure(Stop::Local).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_cancel_reaches_handler_during_blocked_result_write() {
    backpressure(Stop::ProtocolCancel).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_frame_cancels_active_handlers_during_blocked_result_write() {
    backpressure(Stop::InvalidFrame).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writer_timeout_cancels_handlers_and_closes_socket() {
    backpressure(Stop::WriterTimeout).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_loss_cancels_handlers_during_blocked_result_write() {
    backpressure(Stop::HeartbeatLoss).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_cancels_handlers_during_blocked_result_write() {
    backpressure(Stop::Disconnect).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_connection_owner_drops_writer_and_handlers() {
    backpressure(Stop::DropOwner).await;
}
