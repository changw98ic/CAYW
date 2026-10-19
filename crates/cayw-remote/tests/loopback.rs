use async_trait::async_trait;
use cayw_remote::{
    DeviceCredential, DeviceHandler, Limits, OperatorClient, PendingBind, RemoteDevice,
    RemoteError, RequestState, Secret, ServerConfig, ServerState, ServerUrl, serve,
};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;

struct Harness {
    _dir: TempDir,
    state: ServerState,
    url: ServerUrl,
    operator: OperatorClient,
    token: Secret,
    stop: CancellationToken,
    server: JoinHandle<Result<(), RemoteError>>,
}

impl Harness {
    async fn new() -> Self {
        Self::with_limits(Limits::default()).await
    }

    async fn with_limits(limits: Limits) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let token = Secret::generate().unwrap();
        let state = ServerState::open(
            dir.path().join("remote.sqlite"),
            token.clone(),
            ServerConfig {
                limits,
                ..Default::default()
            },
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            ServerUrl::new(&format!("ws://{}", listener.local_addr().unwrap()), false).unwrap();
        let stop = CancellationToken::new();
        let server = tokio::spawn(serve(listener, state.clone(), stop.clone()));
        let operator = OperatorClient::new(url.clone(), token.clone());
        Self {
            _dir: dir,
            state,
            url,
            operator,
            token,
            stop,
            server,
        }
    }

    async fn bind(&self) -> DeviceCredential {
        let code = self.operator.pair(Duration::from_secs(60)).await.unwrap();
        PendingBind::new(code.code)
            .unwrap()
            .complete(&self.url)
            .await
            .unwrap()
    }

    async fn connected(&self, credential: &DeviceCredential) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self
                    .operator
                    .device_status(&credential.device_id)
                    .await
                    .unwrap()
                    .connected
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn terminal(
        &self,
        credential: &DeviceCredential,
        id: &str,
    ) -> cayw_remote::RequestRecord {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let record = self
                    .operator
                    .status(&credential.device_id, id)
                    .await
                    .unwrap();
                if record.state.is_terminal() {
                    break record;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn shutdown(self) {
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

struct Echo(Arc<AtomicUsize>);
#[async_trait]
impl DeviceHandler for Echo {
    async fn handle(&self, request: Value, _: CancellationToken) -> Result<Value, RemoteError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "device_reply": request }))
    }
}

struct WaitForCancel(Arc<AtomicUsize>);
#[async_trait]
impl DeviceHandler for WaitForCancel {
    async fn handle(&self, _: Value, cancel: CancellationToken) -> Result<Value, RemoteError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        cancel.cancelled().await;
        Err(RemoteError::Cancelled)
    }
}

fn device(
    h: &Harness,
    cred: &DeviceCredential,
    handler: Arc<dyn DeviceHandler>,
) -> (CancellationToken, JoinHandle<Result<(), RemoteError>>) {
    let stop = CancellationToken::new();
    let url = h.url.clone();
    let cred = cred.clone();
    let cancel = stop.clone();
    let task = tokio::spawn(async move { RemoteDevice::run(&url, &cred, handler, cancel).await });
    (stop, task)
}

async fn wait_calls(calls: &AtomicUsize, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while calls.load(Ordering::SeqCst) < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn binds_forwards_and_deduplicates_without_exposing_device_secret() {
    // Removing durable request-id deduplication would execute the handler twice.
    let h = Harness::new().await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (cancel, task) = device(&h, &cred, Arc::new(Echo(calls.clone())));
    h.connected(&cred).await;
    let accepted = h
        .operator
        .invoke(&cred.device_id, "r1", json!({"n": 7}))
        .await
        .unwrap();
    assert_eq!(accepted.state, RequestState::Running);
    let done = h.terminal(&cred, "r1").await;
    assert_eq!(done.state, RequestState::Completed);
    assert_eq!(done.value, Some(json!({"device_reply": {"n": 7}})));
    let repeated = h
        .operator
        .invoke(&cred.device_id, "r1", json!({"n": 7}))
        .await
        .unwrap();
    assert_eq!(repeated, done);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.operator
            .invoke(&cred.device_id, "r1", json!({"n": 8}))
            .await
            .unwrap_err(),
        RemoteError::Conflict
    );
    let serialized =
        serde_json::to_string(&h.operator.device_status(&cred.device_id).await.unwrap()).unwrap();
    assert!(!serialized.contains(cred.secret.expose()));
    assert!(!format!("{cred:?}").contains(cred.secret.expose()));
    cancel.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn lost_binding_response_retries_same_identity_and_single_use_code() {
    // The first socket drops after sending; retries must recover one identity.
    let h = Harness::new().await;
    let code = h.operator.pair(Duration::from_secs(60)).await.unwrap();
    let pending = PendingBind::new(code.code.clone()).unwrap();
    let (mut ws, _) = connect_async(h.url.endpoint("bind")).await.unwrap();
    ws.send(Message::Text(
        json!({"version": 1, "request_id": pending.bind_id,
        "device_id": "", "generation": 0, "body": {"type": "bind", "bind_id": pending.bind_id,
        "code": code.code.expose(), "secret": pending.secret.expose()}})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    // Wait for the server commit, deliberately discard the response bytes.
    let ignored = ws.next().await.unwrap().unwrap();
    drop(ignored);
    drop(ws);
    let mut hijack = h.url.endpoint("operator").into_client_request().unwrap();
    hijack.headers_mut().insert(
        "authorization",
        format!("Bearer {}", h.token.expose()).parse().unwrap(),
    );
    let (mut operator_socket, _) = connect_async(hijack).await.unwrap();
    operator_socket.send(Message::Text(json!({"version":1,"request_id":pending.bind_id,"device_id":"","generation":0,"body":{"type":"bind","bind_id":pending.bind_id,"code":pending.code.expose(),"secret":pending.secret.expose()}}).to_string().into())).await.unwrap();
    let rejected: Value = serde_json::from_str(
        operator_socket
            .next()
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(rejected["body"]["error"], "invalid_request");
    assert!(!rejected.to_string().contains(pending.secret.expose()));
    let first = pending.complete(&h.url).await.unwrap();
    let second = pending.complete(&h.url).await.unwrap();
    assert_eq!(first.device_id, second.device_id);
    assert_eq!(first.secret.expose(), second.secret.expose());
    let stolen = PendingBind::new(code.code).unwrap();
    assert_eq!(
        stolen.complete(&h.url).await.unwrap_err(),
        RemoteError::Conflict
    );
    let bad = PendingBind {
        secret: Secret::generate().unwrap(),
        ..pending
    };
    assert_eq!(
        bad.complete(&h.url).await.unwrap_err(),
        RemoteError::Conflict
    );
    h.shutdown().await;
}

#[tokio::test]
async fn cancel_reaches_handler_and_has_a_confirmed_terminal_state() {
    // Dropping the cancel command would leave this real handler running.
    let h = Harness::new().await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (stop, task) = device(&h, &cred, Arc::new(WaitForCancel(calls.clone())));
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "cancel-me", json!({}))
        .await
        .unwrap();
    wait_calls(&calls, 1).await;
    let record = h
        .operator
        .cancel(&cred.device_id, "cancel-me")
        .await
        .unwrap();
    assert!(matches!(
        record.state,
        RequestState::CancelRequested | RequestState::Cancelled
    ));
    assert_eq!(
        h.terminal(&cred, "cancel-me").await.state,
        RequestState::Cancelled
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn disconnect_is_unknown_and_reconnect_does_not_replay() {
    // Treating disconnect as known cancellation or auto-resending would fail this.
    let h = Harness::new().await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (_, task) = device(&h, &cred, Arc::new(WaitForCancel(calls.clone())));
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "side-effect", json!({"write": true}))
        .await
        .unwrap();
    wait_calls(&calls, 1).await;
    task.abort();
    let _ = task.await;
    assert_eq!(
        h.terminal(&cred, "side-effect").await.state,
        RequestState::OutcomeUnknown
    );
    let (stop, task) = device(&h, &cred, Arc::new(Echo(calls.clone())));
    h.connected(&cred).await;
    assert_eq!(
        h.operator
            .invoke(&cred.device_id, "side-effect", json!({"write": true}))
            .await
            .unwrap()
            .state,
        RequestState::OutcomeUnknown
    );
    h.operator
        .invoke(&cred.device_id, "fresh", json!(2))
        .await
        .unwrap();
    assert_eq!(
        h.terminal(&cred, "fresh").await.state,
        RequestState::Completed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn newer_connection_fences_old_generation_and_late_results() {
    let h = Harness::new().await;
    let cred = h.bind().await;
    let mut request = h.url.endpoint("device").into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", cred.secret.expose()).parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("x-device-id", cred.device_id.parse().unwrap());
    let (mut old, _) = connect_async(request).await.unwrap();
    old.send(Message::Text(json!({"version":1,"request_id":"hello","device_id":cred.device_id,"generation":0,"body":{"type":"hello","max_frame_bytes":65536,"max_inflight":16,"max_records":100000,"queue_capacity":32}}).to_string().into())).await.unwrap();
    let hello: Value =
        serde_json::from_str(old.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let old_gen = hello["generation"].as_u64().unwrap();
    h.operator
        .invoke(&cred.device_id, "old", json!({}))
        .await
        .unwrap();
    let _ = old.next().await;
    let (stop, task) = device(&h, &cred, Arc::new(Echo(Arc::new(AtomicUsize::new(0)))));
    tokio::time::timeout(Duration::from_secs(5), async {
        while h
            .operator
            .device_status(&cred.device_id)
            .await
            .unwrap()
            .generation
            <= old_gen
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        h.terminal(&cred, "old").await.state,
        RequestState::OutcomeUnknown
    );
    let _ = old.send(Message::Text(json!({"version":1,"request_id":"old","device_id":cred.device_id,"generation":old_gen,"body":{"type":"outcome","value":"late","error":null}}).to_string().into())).await;
    assert_eq!(
        h.operator
            .status(&cred.device_id, "old")
            .await
            .unwrap()
            .state,
        RequestState::OutcomeUnknown
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn expired_code_and_binding_recovery_window_are_rejected() {
    let h = Harness::with_limits(Limits {
        bind_ttl: Duration::from_millis(1),
        ..Default::default()
    })
    .await;
    let code = h.operator.pair(Duration::from_millis(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        PendingBind::new(code.code)
            .unwrap()
            .complete(&h.url)
            .await
            .unwrap_err(),
        RemoteError::Expired
    );
    let code = h.operator.pair(Duration::from_secs(60)).await.unwrap();
    let bind = PendingBind::new(code.code).unwrap();
    bind.complete(&h.url).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        bind.complete(&h.url).await.unwrap_err(),
        RemoteError::Expired
    );
    h.shutdown().await;
}

#[tokio::test]
async fn revocation_closes_connection_cancels_work_and_survives_restart() {
    let h = Harness::new().await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (_, task) = device(&h, &cred, Arc::new(WaitForCancel(calls.clone())));
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "revoke-work", json!({}))
        .await
        .unwrap();
    wait_calls(&calls, 1).await;
    h.operator.revoke(&cred.device_id).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        h.operator
            .status(&cred.device_id, "revoke-work")
            .await
            .unwrap()
            .state,
        RequestState::OutcomeUnknown
    );
    let path = h._dir.path().join("remote.sqlite");
    let token = h.token.clone();
    h.stop.cancel();
    h.server.await.unwrap().unwrap();
    drop(h.state);
    let state = ServerState::open(path, token.clone(), ServerConfig::default()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = ServerUrl::new(&format!("ws://{}", listener.local_addr().unwrap()), false).unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(listener, state, stop.clone()));
    let op = OperatorClient::new(url.clone(), token);
    assert!(op.device_status(&cred.device_id).await.unwrap().revoked);
    assert_eq!(
        op.invoke(&cred.device_id, "new", json!({}))
            .await
            .unwrap_err(),
        RemoteError::Revoked
    );
    assert_eq!(
        RemoteDevice::run(&url, &cred, Arc::new(Echo(calls)), CancellationToken::new())
            .await
            .unwrap_err(),
        RemoteError::Unauthorized
    );
    stop.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn authentication_and_device_scoping_are_enforced() {
    let h = Harness::new().await;
    let a = h.bind().await;
    let b = h.bind().await;
    let bad = OperatorClient::new(h.url.clone(), a.secret.clone());
    assert_eq!(
        bad.status(&a.device_id, "x").await.unwrap_err(),
        RemoteError::Unauthorized
    );
    let (stop, task) = device(&h, &a, Arc::new(Echo(Arc::new(AtomicUsize::new(0)))));
    h.connected(&a).await;
    h.operator
        .invoke(&a.device_id, "private", json!(1))
        .await
        .unwrap();
    h.terminal(&a, "private").await;
    assert_eq!(
        h.operator
            .status(&b.device_id, "private")
            .await
            .unwrap_err(),
        RemoteError::NotFound
    );
    let forged = DeviceCredential {
        device_id: b.device_id,
        secret: a.secret,
    };
    assert_eq!(
        RemoteDevice::run(
            &h.url,
            &forged,
            Arc::new(Echo(Arc::new(AtomicUsize::new(0)))),
            CancellationToken::new()
        )
        .await
        .unwrap_err(),
        RemoteError::Unauthorized
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn oversized_and_incompatible_frames_do_not_execute_handlers() {
    let h = Harness::with_limits(Limits {
        max_frame_bytes: 2048,
        ..Default::default()
    })
    .await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (stop, task) = device(&h, &cred, Arc::new(Echo(calls.clone())));
    h.connected(&cred).await;
    let mut request = h.url.endpoint("operator").into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", h.token.expose()).parse().unwrap(),
    );
    let (mut ws, _) = connect_async(request.clone()).await.unwrap();
    ws.send(Message::Text(json!({"version":2,"request_id":"wrong","device_id":cred.device_id,"generation":0,"body":{"type":"invoke","payload":1}}).to_string().into())).await.unwrap();
    let response: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(response["body"]["error"], "incompatible_version");
    let (mut ws, _) = connect_async(request).await.unwrap();
    ws.send(Message::Text("x".repeat(8192).into()))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .is_ok()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn inflight_bound_and_timeout_keep_unknown_requests_from_replaying() {
    let h = Harness::with_limits(Limits {
        max_inflight_per_device: 1,
        request_timeout: Duration::from_millis(150),
        ..Default::default()
    })
    .await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (stop, task) = device(&h, &cred, Arc::new(WaitForCancel(calls.clone())));
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "one", json!({}))
        .await
        .unwrap();
    wait_calls(&calls, 1).await;
    assert_eq!(
        h.operator
            .invoke(&cred.device_id, "two", json!({}))
            .await
            .unwrap_err(),
        RemoteError::Busy
    );
    assert_eq!(
        h.terminal(&cred, "one").await.state,
        RequestState::OutcomeUnknown
    );
    assert_eq!(
        h.operator
            .invoke(&cred.device_id, "one", json!({}))
            .await
            .unwrap()
            .state,
        RequestState::OutcomeUnknown
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn invalid_or_missing_device_limit_offer_cannot_replace_active_generation() {
    let h = Harness::new().await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (stop, task) = device(&h, &cred, Arc::new(WaitForCancel(calls.clone())));
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "keep-running", json!({}))
        .await
        .unwrap();
    wait_calls(&calls, 1).await;
    let generation = h
        .operator
        .device_status(&cred.device_id)
        .await
        .unwrap()
        .generation;
    for body in [
        json!({"type":"hello"}),
        json!({"type":"hello","max_frame_bytes":65536,"max_inflight":0,"max_records":100000,"queue_capacity":32}),
    ] {
        let mut request = h.url.endpoint("device").into_client_request().unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", cred.secret.expose()).parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("x-device-id", cred.device_id.parse().unwrap());
        let (mut ws, _) = connect_async(request).await.unwrap();
        ws.send(Message::Text(json!({"version":1,"request_id":"bad-hello","device_id":cred.device_id,"generation":0,"body":body}).to_string().into())).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap();
        if let Some(Ok(Message::Text(text))) = frame {
            let error: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(error["body"]["type"], "error");
        }
        let status = h.operator.device_status(&cred.device_id).await.unwrap();
        assert_eq!(status.generation, generation);
        assert!(status.connected);
        assert_eq!(
            h.operator
                .status(&cred.device_id, "keep-running")
                .await
                .unwrap()
                .state,
            RequestState::Running
        );
    }
    h.operator
        .cancel(&cred.device_id, "keep-running")
        .await
        .unwrap();
    assert_eq!(
        h.terminal(&cred, "keep-running").await.state,
        RequestState::Cancelled
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn device_inflight_limit_rejects_excess_before_admission_and_preserves_active_work() {
    let h = Harness::new().await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let url = h.url.clone();
    let credential = cred.clone();
    let stop = CancellationToken::new();
    let device_stop = stop.clone();
    let handler = Arc::new(WaitForCancel(calls.clone()));
    let task = tokio::spawn(async move {
        RemoteDevice::run_with_limits(
            &url,
            &credential,
            handler,
            device_stop,
            Limits {
                max_inflight_per_device: 1,
                ..Default::default()
            },
        )
        .await
    });
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "first", json!({}))
        .await
        .unwrap();
    wait_calls(&calls, 1).await;
    assert_eq!(
        h.operator
            .invoke(&cred.device_id, "excess", json!({}))
            .await
            .unwrap_err(),
        RemoteError::Busy
    );
    assert_eq!(
        h.operator
            .status(&cred.device_id, "excess")
            .await
            .unwrap_err(),
        RemoteError::NotFound
    );
    assert_eq!(
        h.operator
            .status(&cred.device_id, "first")
            .await
            .unwrap()
            .state,
        RequestState::Running
    );
    assert!(
        h.operator
            .device_status(&cred.device_id)
            .await
            .unwrap()
            .connected
    );
    h.operator.cancel(&cred.device_id, "first").await.unwrap();
    assert_eq!(
        h.terminal(&cred, "first").await.state,
        RequestState::Cancelled
    );
    h.operator
        .invoke(&cred.device_id, "next", json!({}))
        .await
        .unwrap();
    wait_calls(&calls, 2).await;
    h.operator.cancel(&cred.device_id, "next").await.unwrap();
    assert_eq!(
        h.terminal(&cred, "next").await.state,
        RequestState::Cancelled
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn expired_execution_keeps_device_capacity_until_handler_outcome() {
    struct Held {
        calls: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl DeviceHandler for Held {
        async fn handle(&self, _: Value, _: CancellationToken) -> Result<Value, RemoteError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            Ok(json!("late"))
        }
    }
    let h = Harness::with_limits(Limits {
        request_timeout: Duration::from_millis(50),
        ..Default::default()
    })
    .await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let url = h.url.clone();
    let credential = cred.clone();
    let handler = Arc::new(Held {
        calls: calls.clone(),
        release: release.clone(),
    });
    let stop = CancellationToken::new();
    let device_stop = stop.clone();
    let task = tokio::spawn(async move {
        RemoteDevice::run_with_limits(
            &url,
            &credential,
            handler,
            device_stop,
            Limits {
                max_inflight_per_device: 1,
                ..Default::default()
            },
        )
        .await
    });
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "expired-held", json!({}))
        .await
        .unwrap();
    wait_calls(&calls, 1).await;
    let tombstone = h.terminal(&cred, "expired-held").await;
    assert_eq!(tombstone.state, RequestState::OutcomeUnknown);
    // A deadline does not prove the host stopped executing. Keep its actual
    // Device slot occupied until a final outcome, even though the row is terminal.
    assert_eq!(
        h.operator
            .invoke(&cred.device_id, "while-held", json!({}))
            .await
            .unwrap_err(),
        RemoteError::Busy
    );
    assert_eq!(
        h.operator
            .status(&cred.device_id, "while-held")
            .await
            .unwrap_err(),
        RemoteError::NotFound
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match h
                .operator
                .invoke(&cred.device_id, "after-held", json!({}))
                .await
            {
                Ok(_) => break,
                Err(RemoteError::Busy) => tokio::task::yield_now().await,
                result => panic!("unexpected admission result {result:?}"),
            }
        }
    })
    .await
    .unwrap();
    wait_calls(&calls, 2).await;
    assert_eq!(
        h.operator
            .status(&cred.device_id, "expired-held")
            .await
            .unwrap(),
        tombstone
    );
    assert!(
        h.operator
            .device_status(&cred.device_id)
            .await
            .unwrap()
            .connected
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn device_frame_and_record_limits_reject_new_work_without_disconnect() {
    let h = Harness::new().await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let url = h.url.clone();
    let credential = cred.clone();
    let handler = Arc::new(Echo(calls.clone()));
    let stop = CancellationToken::new();
    let device_stop = stop.clone();
    let task = tokio::spawn(async move {
        RemoteDevice::run_with_limits(
            &url,
            &credential,
            handler,
            device_stop,
            Limits {
                max_frame_bytes: 1024,
                max_records: 1,
                ..Default::default()
            },
        )
        .await
    });
    h.connected(&cred).await;
    assert_eq!(
        h.operator
            .invoke(&cred.device_id, "large-input", json!("x".repeat(2048)))
            .await
            .unwrap_err(),
        RemoteError::TooLarge
    );
    assert_eq!(
        h.operator
            .status(&cred.device_id, "large-input")
            .await
            .unwrap_err(),
        RemoteError::NotFound
    );
    h.operator
        .invoke(&cred.device_id, "allowed", json!(1))
        .await
        .unwrap();
    let done = h.terminal(&cred, "allowed").await;
    assert_eq!(done.state, RequestState::Completed);
    assert_eq!(
        h.operator
            .invoke(&cred.device_id, "record-excess", json!(2))
            .await
            .unwrap_err(),
        RemoteError::Busy
    );
    assert_eq!(
        h.operator
            .status(&cred.device_id, "record-excess")
            .await
            .unwrap_err(),
        RemoteError::NotFound
    );
    assert_eq!(
        h.operator
            .invoke(&cred.device_id, "allowed", json!(1))
            .await
            .unwrap(),
        done
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        h.operator
            .device_status(&cred.device_id)
            .await
            .unwrap()
            .connected
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn durable_running_intent_recovers_as_unknown_without_replay() {
    // Seed the exact durable boundary a process crash can leave: intent was
    // committed before any result. Recovery must not enqueue that intent.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote.sqlite");
    let token = Secret::generate().unwrap();
    let state = ServerState::open(&path, token.clone(), ServerConfig::default()).unwrap();
    drop(state);
    let secret = Secret::generate().unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    use sha2::{Digest, Sha256};
    let digest = |input: &[u8]| {
        Sha256::digest(input)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    db.execute(
        "INSERT INTO devices(id,secret_hash,generation) VALUES(?1,?2,1)",
        rusqlite::params!["crashed-device", digest(secret.expose().as_bytes())],
    )
    .unwrap();
    db.execute("INSERT INTO requests VALUES(?1,?2,?3,?4)", rusqlite::params!["crashed-device", "crash", digest(b"{}"), json!({"request_id":"crash", "device_id":"crashed-device", "generation":1, "state":"running", "error":null}).to_string()]).unwrap();
    drop(db);
    let state = ServerState::open(&path, token.clone(), ServerConfig::default()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = ServerUrl::new(&format!("ws://{}", listener.local_addr().unwrap()), false).unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(listener, state, stop.clone()));
    let operator = OperatorClient::new(url.clone(), token);
    let calls = Arc::new(AtomicUsize::new(0));
    let device_stop = CancellationToken::new();
    let task_stop = device_stop.clone();
    let seen_calls = calls.clone();
    let task = tokio::spawn(async move {
        RemoteDevice::run(
            &url,
            &DeviceCredential {
                device_id: "crashed-device".into(),
                secret,
            },
            Arc::new(Echo(seen_calls)),
            task_stop,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !operator
            .device_status("crashed-device")
            .await
            .unwrap()
            .connected
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        operator
            .invoke("crashed-device", "crash", json!({}))
            .await
            .unwrap()
            .state,
        RequestState::OutcomeUnknown
    );
    assert_eq!(
        operator
            .status("crashed-device", "crash")
            .await
            .unwrap()
            .state,
        RequestState::OutcomeUnknown
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    device_stop.cancel();
    task.await.unwrap().unwrap();
    stop.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn durable_store_never_contains_raw_credentials() {
    let h = Harness::new().await;
    let code = h.operator.pair(Duration::from_secs(60)).await.unwrap();
    let pending = PendingBind::new(code.code).unwrap();
    let credential = pending.complete(&h.url).await.unwrap();
    h.stop.cancel();
    h.server.await.unwrap().unwrap();
    drop(h.state);
    let bytes = std::fs::read(h._dir.path().join("remote.sqlite")).unwrap();
    for secret in [&h.token, &pending.code, &credential.secret] {
        assert!(
            !bytes
                .windows(secret.expose().len())
                .any(|window| window == secret.expose().as_bytes())
        );
    }
}

#[tokio::test]
async fn device_cannot_forge_outcome_for_another_device() {
    let h = Harness::new().await;
    let a = h.bind().await;
    let b = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (stop, task) = device(&h, &b, Arc::new(WaitForCancel(calls.clone())));
    h.connected(&b).await;
    h.operator
        .invoke(&b.device_id, "owned-b", json!({}))
        .await
        .unwrap();
    wait_calls(&calls, 1).await;
    let mut request = h.url.endpoint("device").into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", a.secret.expose()).parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("x-device-id", a.device_id.parse().unwrap());
    let (mut ws, _) = connect_async(request).await.unwrap();
    ws.send(Message::Text(json!({"version":1,"request_id":"hello","device_id":a.device_id,"generation":0,"body":{"type":"hello","max_frame_bytes":65536,"max_inflight":16,"max_records":100000,"queue_capacity":32}}).to_string().into())).await.unwrap();
    let welcome: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    ws.send(Message::Text(json!({"version":1,"request_id":"owned-b","device_id":b.device_id,"generation":welcome["generation"],"body":{"type":"outcome","value":"forged","error":null}}).to_string().into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None | Some(Ok(Message::Close(_))) | Some(Err(_)) => break,
                Some(Ok(Message::Ping(bytes))) => {
                    let _ = ws.send(Message::Pong(bytes)).await;
                }
                Some(Ok(Message::Pong(_))) => {}
                other => panic!("unexpected frame before rejection: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        h.operator
            .status(&b.device_id, "owned-b")
            .await
            .unwrap()
            .state,
        RequestState::Running
    );
    h.operator.cancel(&b.device_id, "owned-b").await.unwrap();
    assert_eq!(
        h.terminal(&b, "owned-b").await.state,
        RequestState::Cancelled
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn new_server_cannot_take_over_an_open_identity_store() {
    let h = Harness::new().await;
    let result = ServerState::open(
        h._dir.path().join("remote.sqlite"),
        h.token.clone(),
        ServerConfig::default(),
    );
    assert!(matches!(result, Err(RemoteError::Storage)));
    h.shutdown().await;
}

#[tokio::test]
async fn offline_revocation_blocks_later_connection() {
    let h = Harness::new().await;
    let cred = h.bind().await;
    h.operator.revoke(&cred.device_id).await.unwrap();
    assert_eq!(
        RemoteDevice::run(
            &h.url,
            &cred,
            Arc::new(Echo(Arc::new(AtomicUsize::new(0)))),
            CancellationToken::new()
        )
        .await
        .unwrap_err(),
        RemoteError::Unauthorized
    );
    h.shutdown().await;
}

#[test]
fn invalid_resource_limits_return_errors_without_allocation_or_panics() {
    let dir = tempfile::tempdir().unwrap();
    for limits in [
        Limits {
            max_connections: usize::MAX,
            ..Default::default()
        },
        Limits {
            queue_capacity: usize::MAX,
            ..Default::default()
        },
        Limits {
            max_records: usize::MAX,
            ..Default::default()
        },
        Limits {
            request_timeout: Duration::MAX,
            ..Default::default()
        },
        Limits {
            heartbeat_interval: Duration::from_micros(500),
            ..Default::default()
        },
        Limits {
            heartbeat_interval: Duration::from_micros(1500),
            ..Default::default()
        },
        Limits {
            heartbeat_timeout: Duration::from_micros(30_000_500),
            ..Default::default()
        },
    ] {
        assert!(matches!(
            ServerState::open(
                dir.path().join("db"),
                Secret::generate().unwrap(),
                ServerConfig {
                    limits,
                    ..Default::default()
                }
            ),
            Err(RemoteError::Configuration)
        ));
    }
}

#[tokio::test]
async fn bounded_connection_slots_reject_then_recover_capacity() {
    let h = Harness::with_limits(Limits {
        max_connections: 1,
        ..Default::default()
    })
    .await;
    let (mut socket, _) = connect_async(h.url.endpoint("bind")).await.unwrap();
    assert!(connect_async(h.url.endpoint("bind")).await.is_err());
    socket.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok((mut socket, _)) = connect_async(h.url.endpoint("bind")).await {
                socket.close(None).await.unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn maximum_handler_reply_still_has_queryable_terminal_status() {
    struct Large;
    #[async_trait]
    impl DeviceHandler for Large {
        async fn handle(&self, _: Value, _: CancellationToken) -> Result<Value, RemoteError> {
            Ok(json!("x".repeat(1750)))
        }
    }
    let h = Harness::with_limits(Limits {
        max_frame_bytes: 2048,
        ..Default::default()
    })
    .await;
    let cred = h.bind().await;
    let (stop, task) = device(&h, &cred, Arc::new(Large));
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "large", json!({}))
        .await
        .unwrap();
    // The durable Record adds overhead beyond the Device's Outcome frame.
    let result = h.terminal(&cred, "large").await;
    assert!(result.state.is_terminal());
    if result.state == RequestState::Completed {
        assert_eq!(result.value.unwrap().as_str().unwrap().len(), 1750);
    }
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn successful_null_is_a_completed_value() {
    struct Null;
    #[async_trait]
    impl DeviceHandler for Null {
        async fn handle(&self, _: Value, _: CancellationToken) -> Result<Value, RemoteError> {
            Ok(Value::Null)
        }
    }
    let h = Harness::new().await;
    let cred = h.bind().await;
    let (stop, task) = device(&h, &cred, Arc::new(Null));
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "null", Value::Null)
        .await
        .unwrap();
    let result = h.terminal(&cred, "null").await;
    assert_eq!(result.state, RequestState::Completed);
    assert_eq!(result.value, Some(Value::Null));
    stop.cancel();
    task.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn heartbeat_loss_fences_unresponsive_device() {
    let h = Harness::with_limits(Limits {
        heartbeat_interval: Duration::from_millis(25),
        heartbeat_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await;
    let cred = h.bind().await;
    let mut request = h.url.endpoint("device").into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", cred.secret.expose()).parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("x-device-id", cred.device_id.parse().unwrap());
    let (mut ws, _) = connect_async(request).await.unwrap();
    ws.send(Message::Text(json!({"version":1,"request_id":"hello","device_id":cred.device_id,"generation":0,"body":{"type":"hello","max_frame_bytes":65536,"max_inflight":16,"max_records":100000,"queue_capacity":32}}).to_string().into())).await.unwrap();
    let _ = ws.next().await.unwrap().unwrap();
    h.operator
        .invoke(&cred.device_id, "silent", json!({}))
        .await
        .unwrap();
    // Keep the socket alive without polling it: no heartbeat pong is produced.
    assert_eq!(
        h.terminal(&cred, "silent").await.state,
        RequestState::OutcomeUnknown
    );
    assert!(
        !h.operator
            .device_status(&cred.device_id)
            .await
            .unwrap()
            .connected
    );
    drop(ws);
    h.shutdown().await;
}

#[tokio::test]
async fn aborting_server_future_closes_upgraded_device_socket() {
    let h = Harness::new().await;
    let cred = h.bind().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (_, task) = device(&h, &cred, Arc::new(WaitForCancel(calls.clone())));
    h.connected(&cred).await;
    h.operator
        .invoke(&cred.device_id, "drop-server", json!({}))
        .await
        .unwrap();
    wait_calls(&calls, 1).await;
    h.server.abort();
    let _ = h.server.await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn cancellation_interrupts_device_welcome_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = ServerUrl::new(&format!("ws://{}", listener.local_addr().unwrap()), false).unwrap();
    let ready = Arc::new(tokio::sync::Notify::new());
    let peer_ready = ready.clone();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap();
        peer_ready.notify_one();
        std::future::pending::<()>().await;
    });
    let stop = CancellationToken::new();
    let device_stop = stop.clone();
    let task = tokio::spawn(async move {
        RemoteDevice::run(
            &url,
            &DeviceCredential {
                device_id: "slow".into(),
                secret: Secret::generate().unwrap(),
            },
            Arc::new(Echo(Arc::new(AtomicUsize::new(0)))),
            device_stop,
        )
        .await
    });
    ready.notified().await;
    stop.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), task).await;
    peer.abort();
    assert!(result.is_ok());
    result.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn redirects_never_forward_operator_credentials() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = ServerUrl::new(&format!("ws://{}", redirect.local_addr().unwrap()), false).unwrap();
    let target_addr = target.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut peer, _) = redirect.accept().await.unwrap();
        let mut bytes = [0; 4096];
        let mut used = 0;
        loop {
            let count = peer.read(&mut bytes[used..]).await.unwrap();
            assert!(count > 0);
            used += count;
            if bytes[..used].windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            assert!(used < bytes.len());
        }
        peer.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: ws://{target_addr}/v1/operator\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
    });
    let operator = OperatorClient::new(url, Secret::generate().unwrap());
    assert_eq!(
        operator.device_status("anything").await.unwrap_err(),
        RemoteError::Disconnected
    );
    task.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), target.accept())
            .await
            .is_err()
    );
}

#[test]
fn explicit_transport_policy_rejects_cleartext_remote_and_credential_urls() {
    assert_eq!(
        ServerUrl::new("ws://example.com", false).unwrap_err(),
        RemoteError::TlsRequired
    );
    assert!(ServerUrl::new("wss://example.com", false).is_ok());
    assert!(ServerUrl::new("ws://example.com", true).is_ok());
    for url in [
        "ws://secret@127.0.0.1",
        "ws://127.0.0.1?token=secret",
        "ws://127.0.0.1/#secret",
    ] {
        assert_eq!(
            ServerUrl::new(url, false).unwrap_err(),
            RemoteError::InvalidRequest
        );
    }
}
