use async_trait::async_trait;
use cayw_ipc::{RpcError, RpcHandler, ServerConfig};
use cayw_webui::{
    DeviceRouter, GatewayLimits, IpcEndpoint, MAX_REQUEST_BYTES, OwnershipStore, serve_with_limits,
    serve_with_ownership,
};
use reqwest::{Client as HttpClient, StatusCode};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tempfile::tempdir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const WEB_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const IPC_TOKEN: &str = "fedcba9876543210fedcba9876543210";

struct RuntimeFixture {
    prompt_started: Semaphore,
    prompt_release: Semaphore,
    prompt_cancelled: AtomicBool,
    artifact_started: Semaphore,
    ipc_cancel: CancellationToken,
}

impl Default for RuntimeFixture {
    fn default() -> Self {
        Self {
            prompt_started: Semaphore::new(0),
            prompt_release: Semaphore::new(0),
            prompt_cancelled: AtomicBool::new(false),
            artifact_started: Semaphore::new(0),
            ipc_cancel: CancellationToken::new(),
        }
    }
}

#[async_trait]
impl RpcHandler for RuntimeFixture {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method {
            "runtime.health" => Ok(json!({"status":"ok", "protocol_version":3})),
            "session.prompt" if params["text"] == "secret_receipt" => Ok(
                json!({"id":"s1","status":"ok","messages":[{"role":"assistant","content":WEB_TOKEN}]}),
            ),
            "session.prompt" => {
                self.prompt_started.add_permits(1);
                tokio::select! {
                    _ = cancel.cancelled() => {
                        self.prompt_cancelled.store(true, Ordering::SeqCst);
                        Err(RpcError::new("cancelled", "runtime settled cancellation"))
                    }
                    permit = self.prompt_release.acquire() => {
                        permit.unwrap().forget();
                        Ok(json!({"id":"s1","state":"idle","cursor":2,"messages":[]}))
                    }
                }
            }
            "session.events" => {
                Ok(json!([{"cursor":1,"event":{"type":"text_delta","delta":"正在执行"}}]))
            }
            "session.cancel" => {
                assert_eq!(
                    params,
                    json!({"session_id":"s1","task_id":"original-task","worker_id":"worker-1","attempt_id":"attempt-1","fence":7})
                );
                self.prompt_release.add_permits(1);
                Ok(json!({"cancelled":true}))
            }
            "session.get" if params["session_id"] == "error" => Err(RpcError::new(
                "needs_reconciliation",
                "tool outcome must be checked",
            )),
            "session.get" if params["session_id"] == "secrets" => Err(RpcError::new(
                "fixture_error",
                format!("do not expose {IPC_TOKEN} or {WEB_TOKEN}"),
            )),
            "session.get" if params["session_id"] == "secret_result" => Ok(
                json!({"tool_output":format!("{IPC_TOKEN} / {WEB_TOKEN}"),"content":[IPC_TOKEN]}),
            ),
            "session.get" if params["session_id"] == "secret_key" => {
                Ok(json!({IPC_TOKEN: "must not rename keys or disclose the credential"}))
            }
            "session.get" if params["session_id"] == "protocol_fields" => Ok(
                json!({"id":"s1","status":"ok","token_usage":42,"tool_output":{"ok":true,"id":"call-1"}}),
            ),
            "session.get" if params["session_id"] == "unknown" => Err(RpcError::new(
                "tool_outcome_unknown",
                "inspect durable tool record",
            )),
            "session.create" | "session.fork"
                if params.get("id").and_then(Value::as_str).is_some() =>
            {
                Ok(json!({"id": params["id"], "state": "idle"}))
            }
            "task.enqueue" if params.get("id").and_then(Value::as_str).is_some() => {
                Ok(json!({"id": params["id"], "state": "queued"}))
            }
            _ if params.get("drop_connection").and_then(Value::as_bool) == Some(true) => {
                self.artifact_started.add_permits(1);
                self.ipc_cancel.cancel();
                cancel.cancelled().await;
                Err(RpcError::new("cancelled", "fixture connection closed"))
            }
            _ if params.get("slow").and_then(Value::as_bool) == Some(true)
                || params.get("artifact_id").and_then(Value::as_str) == Some("slow")
                || params.get("session_id").and_then(Value::as_str) == Some("slow") =>
            {
                self.artifact_started.add_permits(1);
                cancel.cancelled().await;
                Err(RpcError::new("cancelled", "fixture request cancelled"))
            }
            "session.get" => Ok(json!({"id":"s1","state":"idle","cursor":0,"messages":[]})),
            _ => Ok(json!({"method":method,"params":params})),
        }
    }
}

struct Fixture {
    addr: SocketAddr,
    http: HttpClient,
    runtime: Arc<RuntimeFixture>,
    stop: CancellationToken,
    tasks: Vec<JoinHandle<std::io::Result<()>>>,
}
impl Fixture {
    async fn start(limits: GatewayLimits) -> Self {
        Self::start_inner(limits, None).await
    }

    /// Ownership + DeviceRouter that registers `device_id` against the fixture IPC
    /// so Session/Task routes actually select that endpoint (same process in tests).
    async fn start_with_ownership_device(
        limits: GatewayLimits,
        ownership: Arc<OwnershipStore>,
        device_id: &str,
    ) -> Self {
        let stop = CancellationToken::new();
        let runtime = Arc::new(RuntimeFixture::default());
        let ipc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ipc_addr = ipc_listener.local_addr().unwrap();
        let mut ipc_config = ServerConfig::new(IPC_TOKEN);
        ipc_config.request_timeout = Duration::from_secs(20);
        let ipc_stop = stop.child_token();
        let ipc_cancel = runtime.ipc_cancel.clone();
        let watcher_stop = ipc_stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = ipc_cancel.cancelled() => {
                    watcher_stop.cancel();
                }
                _ = watcher_stop.cancelled() => {}
            }
        });
        let ipc_task = tokio::spawn(cayw_ipc::serve(
            ipc_listener,
            runtime.clone(),
            ipc_config,
            ipc_stop,
        ));
        let ipc = IpcEndpoint::new(ipc_addr, IPC_TOKEN).unwrap();
        let devices = Arc::new(DeviceRouter::new(ipc.clone()));
        devices.register(device_id, ipc.clone()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let web_task = tokio::spawn(serve_with_ownership(
            listener,
            devices,
            WEB_TOKEN,
            limits,
            ownership,
            stop.clone(),
        ));
        Self {
            addr,
            http: HttpClient::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            runtime,
            stop,
            tasks: vec![ipc_task, web_task],
        }
    }

    async fn start_inner(limits: GatewayLimits, ownership: Option<Arc<OwnershipStore>>) -> Self {
        let stop = CancellationToken::new();
        let runtime = Arc::new(RuntimeFixture::default());
        let ipc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ipc_addr = ipc_listener.local_addr().unwrap();
        let mut ipc_config = ServerConfig::new(IPC_TOKEN);
        ipc_config.request_timeout = Duration::from_secs(20);
        let ipc_stop = stop.child_token();
        let ipc_cancel = runtime.ipc_cancel.clone();
        let watcher_stop = ipc_stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = ipc_cancel.cancelled() => {
                    watcher_stop.cancel();
                }
                _ = watcher_stop.cancelled() => {}
            }
        });
        let ipc_task = tokio::spawn(cayw_ipc::serve(
            ipc_listener,
            runtime.clone(),
            ipc_config,
            ipc_stop,
        ));
        let ipc = IpcEndpoint::new(ipc_addr, IPC_TOKEN).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let web_task = if let Some(store) = ownership.clone() {
            let devices = Arc::new(DeviceRouter::new(ipc));
            tokio::spawn(serve_with_ownership(
                listener,
                devices,
                WEB_TOKEN,
                limits,
                store,
                stop.clone(),
            ))
        } else {
            tokio::spawn(serve_with_limits(
                listener,
                ipc,
                WEB_TOKEN,
                limits,
                stop.clone(),
            ))
        };
        Self {
            addr,
            http: HttpClient::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            runtime,
            stop,
            tasks: vec![ipc_task, web_task],
        }
    }
    fn rpc(&self, method: &str, params: Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("http://{}/api/rpc", self.addr))
            .bearer_auth(WEB_TOKEN)
            .json(&json!({"method":method,"params":params}))
    }
    async fn dispatched_artifact_rpc(
        &self,
        method: &str,
        params: Value,
        expected: StatusCode,
    ) -> Value {
        let (started, response) = tokio::join!(
            tokio::time::timeout(
                Duration::from_secs(5),
                self.runtime.artifact_started.acquire()
            ),
            self.rpc(method, params).send(),
        );
        started
            .expect("fixture must observe the dispatched request")
            .unwrap()
            .forget();
        envelope(response.unwrap(), expected).await
    }

    async fn close(mut self) {
        self.stop.cancel();
        for task in self.tasks.drain(..) {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("gateway must stop")
                .unwrap()
                .unwrap();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn envelope(response: reqwest::Response, expected: StatusCode) -> Value {
    assert_eq!(response.status(), expected);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    let value: Value = response.json().await.unwrap();
    assert!(value.is_object());
    assert_eq!(value.as_object().unwrap().len(), 1);
    assert!(value.get("result").is_some() ^ value.get("error").is_some());
    value
}

#[tokio::test]
async fn static_assets_load_before_login_and_never_contain_credentials() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    for (path, content_type) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/styles.css", "text/css"),
    ] {
        let response = fixture
            .http
            .get(format!("http://{}{path}", fixture.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with(content_type)
        );
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        let text = response.text().await.unwrap();
        assert!(!text.contains(IPC_TOKEN));
        assert!(!text.contains(WEB_TOKEN));
    }
    fixture.close().await;
}

#[tokio::test]
async fn bearer_host_origin_and_fetch_site_are_checked_before_forwarding() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let endpoint = format!("http://{}/api/rpc", fixture.addr);
    let payload = json!({"method":"runtime.health","params":{}});
    for request in [
        fixture.http.post(&endpoint).json(&payload),
        fixture
            .http
            .post(&endpoint)
            .bearer_auth("wrong")
            .json(&payload),
        fixture
            .rpc("runtime.health", json!({}))
            .header("host", "attacker.example:80"),
        fixture
            .rpc("runtime.health", json!({}))
            .header("origin", "https://attacker.example"),
        fixture
            .rpc("runtime.health", json!({}))
            .header("origin", "null"),
        fixture
            .rpc("runtime.health", json!({}))
            .header("sec-fetch-site", "cross-site"),
        fixture
            .rpc("runtime.health", json!({}))
            .header("origin", format!("http://{}", fixture.addr))
            .header("origin", format!("http://{}", fixture.addr)),
        fixture
            .rpc("runtime.health", json!({}))
            .header("authorization", "Bearer second-token"),
        fixture
            .rpc("runtime.health", json!({}))
            .header("host", "127.0.0.1:1"),
        fixture
            .rpc("runtime.health", json!({}))
            .header("origin", "http://127.0.0.1:1"),
    ] {
        envelope(request.send().await.unwrap(), StatusCode::FORBIDDEN).await;
    }
    let result = envelope(
        fixture
            .rpc("runtime.health", json!({}))
            .header("origin", format!("http://{}", fixture.addr))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        result["result"],
        json!({"status":"ok","protocol_version":3})
    );
    let result = envelope(
        fixture
            .rpc("runtime.health", json!({}))
            .header("host", format!("localhost:{}", fixture.addr.port()))
            .header(
                "origin",
                format!("http://localhost:{}", fixture.addr.port()),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(result["result"]["status"], "ok");
    fixture.close().await;
}

#[tokio::test]
async fn methods_json_and_large_requests_fail_with_one_error_envelope() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    for method in [
        "tool.execute",
        "session.reconcile",
        "plugin.load",
        "runtime.shutdown",
    ] {
        let result = envelope(
            fixture.rpc(method, json!({})).send().await.unwrap(),
            StatusCode::FORBIDDEN,
        )
        .await;
        assert_eq!(result["error"]["code"], "method_not_allowed");
    }
    let endpoint = format!("http://{}/api/rpc", fixture.addr);
    envelope(
        fixture
            .http
            .get(&endpoint)
            .bearer_auth(WEB_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::METHOD_NOT_ALLOWED,
    )
    .await;
    envelope(
        fixture
            .http
            .post(format!("{endpoint}?token=secret"))
            .bearer_auth(WEB_TOKEN)
            .json(&json!({"method":"runtime.health","params":{}}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    envelope(
        fixture
            .http
            .post(&endpoint)
            .bearer_auth(WEB_TOKEN)
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    envelope(
        fixture
            .rpc("session.create", json!([]))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    envelope(
        fixture
            .rpc(
                "session.prompt",
                json!({"text":"x".repeat(MAX_REQUEST_BYTES)}),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
    )
    .await;
    envelope(
        fixture
            .http
            .request(reqwest::Method::OPTIONS, &endpoint)
            .bearer_auth(WEB_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::METHOD_NOT_ALLOWED,
    )
    .await;
    fixture.close().await;
}

#[tokio::test]
async fn forwarding_preserves_structured_runtime_errors() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let result = envelope(
        fixture
            .rpc("session.get", json!({"session_id":"error"}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_GATEWAY,
    )
    .await;
    assert_eq!(result["error"]["code"], "needs_reconciliation");
    assert_eq!(result["error"]["message"], "tool outcome must be checked");
    assert_eq!(result["error"]["outcome_unknown"], false);
    let unknown = envelope(
        fixture
            .rpc("session.get", json!({"session_id":"unknown"}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_GATEWAY,
    )
    .await;
    assert_eq!(unknown["error"]["code"], "tool_outcome_unknown");
    assert_eq!(unknown["error"]["outcome_unknown"], true);
    let result = envelope(
        fixture
            .rpc("session.create", json!({"label":"中文会话"}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        result["result"],
        json!({"method":"session.create","params":{"label":"中文会话"}})
    );
    fixture.close().await;
}

#[tokio::test]
async fn upstream_error_messages_cannot_disclose_gateway_credentials() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let value = envelope(
        fixture
            .rpc("session.get", json!({"session_id":"secrets"}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_GATEWAY,
    )
    .await;
    let text = value.to_string();
    assert!(!text.contains(IPC_TOKEN));
    assert!(!text.contains(WEB_TOKEN));
    assert!(text.contains("[redacted]"));
    for id in ["secret_result", "secret_key"] {
        let result = envelope(
            fixture
                .rpc("session.get", json!({"session_id":id}))
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_GATEWAY,
        )
        .await;
        let text = result.to_string();
        assert!(!text.contains(IPC_TOKEN));
        assert!(!text.contains(WEB_TOKEN));
        assert_eq!(result["error"]["code"], "credential_in_result");
        assert_eq!(result["error"]["outcome_unknown"], false);
    }
    fixture.close().await;
}

#[tokio::test]
async fn withheld_write_result_keeps_its_effect_unknown_instead_of_fabricating_success() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let value = envelope(
        fixture
            .rpc(
                "session.prompt",
                json!({"session_id":"s1","text":"secret_receipt"}),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_GATEWAY,
    )
    .await;
    assert_eq!(value["error"]["code"], "credential_in_result");
    assert_eq!(value["error"]["outcome_unknown"], true);
    assert!(!value.to_string().contains(WEB_TOKEN));
    assert!(!value.to_string().contains(IPC_TOKEN));
    fixture.close().await;
}

#[tokio::test]
async fn successful_protocol_fields_and_business_keys_remain_unchanged() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let value = envelope(
        fixture
            .rpc("session.get", json!({"session_id":"protocol_fields"}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        value["result"],
        json!({"id":"s1","status":"ok","token_usage":42,"tool_output":{"ok":true,"id":"call-1"}})
    );
    fixture.close().await;
}

#[tokio::test]
async fn long_prompt_does_not_block_events_cancel_or_approval() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let pending = fixture.rpc("session.prompt", json!({"session_id":"s1","text":"执行"}));
    let prompt = tokio::spawn(pending.send());
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.runtime.prompt_started.acquire(),
    )
    .await
    .unwrap()
    .unwrap()
    .forget();
    let events = envelope(
        fixture
            .rpc("session.events", json!({"session_id":"s1","after":0}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(events["result"][0]["cursor"], 1);
    envelope(
        fixture
            .rpc(
                "approval.decide",
                json!({"session_id":"s1","approval_id":"a1","decision":"allow_once"}),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    envelope(
        fixture
            .rpc("session.cancel", json!({"session_id":"s1","task_id":"original-task","worker_id":"worker-1","attempt_id":"attempt-1","fence":7}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        envelope(prompt.await.unwrap().unwrap(), StatusCode::OK).await["result"]["state"],
        "idle"
    );
    fixture.close().await;
}

#[tokio::test]
async fn timed_out_dispatched_prompt_is_not_retried_and_preserves_unknown_status() {
    let fixture = Fixture::start(GatewayLimits {
        prompt_timeout: Duration::from_millis(150),
        ..Default::default()
    })
    .await;
    let result = envelope(
        fixture
            .rpc("session.prompt", json!({"session_id":"s1","text":"执行"}))
            .send()
            .await
            .unwrap(),
        StatusCode::GATEWAY_TIMEOUT,
    )
    .await;
    assert_eq!(result["error"]["code"], "request_timeout");
    assert_eq!(result["error"]["outcome_unknown"], true);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fixture.runtime.prompt_cancelled.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping the request must cancel IPC execution");
    assert_eq!(fixture.runtime.prompt_started.available_permits(), 1);
    let events = envelope(
        fixture
            .rpc("session.events", json!({"session_id":"s1","after":0}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        events["result"][0]["cursor"], 1,
        "a timeout must not break subsequent event consumption"
    );
    fixture.close().await;
}

#[tokio::test]
async fn disconnected_browser_cancels_only_its_own_dispatched_prompt() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let mut browser = TcpStream::connect(fixture.addr).await.unwrap();
    let body =
        json!({"method":"session.prompt","params":{"session_id":"s1","text":"执行"}}).to_string();
    browser.write_all(format!("POST /api/rpc HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {WEB_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", fixture.addr, body.len()).as_bytes()).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.runtime.prompt_started.acquire(),
    )
    .await
    .unwrap()
    .unwrap()
    .forget();
    drop(browser);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fixture.runtime.prompt_cancelled.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("browser disconnect must cancel the dispatched runtime call");
    let events = envelope(
        fixture
            .rpc("session.events", json!({"session_id":"s1","after":0}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(events["result"][0]["cursor"], 1);
    fixture.close().await;
}

#[tokio::test]
async fn saturated_prompt_lane_rejects_queue_timeout_but_keeps_control_lane_available() {
    let limits = GatewayLimits {
        max_in_flight: 2,
        max_prompt_in_flight: 1,
        max_pending_requests: 3,
        queue_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let fixture = Fixture::start(limits).await;
    let prompt = tokio::spawn(
        fixture
            .rpc("session.prompt", json!({"session_id":"s1","text":"执行"}))
            .send(),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.runtime.prompt_started.acquire(),
    )
    .await
    .unwrap()
    .unwrap()
    .forget();
    let queued = fixture
        .rpc("session.prompt", json!({"session_id":"s2","text":"排队"}))
        .send()
        .await
        .unwrap();
    let value = envelope(queued, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_eq!(value["error"]["code"], "queue_timeout");
    assert_eq!(value["error"]["outcome_unknown"], false);
    envelope(
        fixture
            .rpc("session.events", json!({"session_id":"s1","after":0}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    envelope(
        fixture
            .rpc("session.cancel", json!({"session_id":"s1","task_id":"original-task","worker_id":"worker-1","attempt_id":"attempt-1","fence":7}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    envelope(prompt.await.unwrap().unwrap(), StatusCode::OK).await;
    assert_eq!(
        fixture.runtime.prompt_started.available_permits(),
        0,
        "queued prompt must not have reached runtime"
    );
    fixture.close().await;
}

#[tokio::test]
async fn full_pending_queue_rejects_new_work_while_static_login_still_loads() {
    let fixture = Fixture::start(GatewayLimits {
        max_pending_requests: 1,
        ..Default::default()
    })
    .await;
    let prompt = tokio::spawn(
        fixture
            .rpc("session.prompt", json!({"session_id":"s1","text":"执行"}))
            .send(),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.runtime.prompt_started.acquire(),
    )
    .await
    .unwrap()
    .unwrap()
    .forget();
    let value = envelope(
        fixture
            .rpc("runtime.health", json!({}))
            .send()
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(value["error"]["code"], "queue_full");
    assert_eq!(value["error"]["outcome_unknown"], false);
    let static_page = fixture
        .http
        .get(format!("http://{}/", fixture.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(static_page.status(), StatusCode::OK);
    fixture.runtime.prompt_release.add_permits(1);
    envelope(prompt.await.unwrap().unwrap(), StatusCode::OK).await;
    fixture.close().await;
}

#[tokio::test]
async fn ordinary_rpc_timeout_keeps_its_outcome_known_and_future_calls_work() {
    let fixture = Fixture::start(GatewayLimits {
        request_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await;
    let value = envelope(
        fixture
            .rpc("session.get", json!({"session_id":"slow"}))
            .send()
            .await
            .unwrap(),
        StatusCode::GATEWAY_TIMEOUT,
    )
    .await;
    assert_eq!(value["error"]["outcome_unknown"], false);
    envelope(
        fixture
            .rpc("runtime.health", json!({}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    fixture.close().await;
}

#[tokio::test]
async fn artifact_mutation_timeout_reports_outcome_unknown_while_read_only_timeout_is_known() {
    let fixture = Fixture::start(GatewayLimits {
        request_timeout: Duration::from_secs(1),
        ..Default::default()
    })
    .await;
    let put_value = fixture
        .dispatched_artifact_rpc(
            "artifacts.put",
            json!({
                "artifact_id": "slow",
                "session_id": "s1",
                "kind": "html_card",
                "title": "Test",
                "body": "<div>test</div>",
                "at_ms": 1
            }),
            StatusCode::GATEWAY_TIMEOUT,
        )
        .await;
    assert_eq!(put_value["error"]["code"], "request_timeout");
    assert_eq!(
        put_value["error"]["outcome_unknown"], true,
        "dispatched artifact mutation timeout must report outcome_unknown=true"
    );

    let preview_value = fixture
        .dispatched_artifact_rpc(
            "artifacts.preview",
            json!({"session_id": "s1", "artifact_id": "slow"}),
            StatusCode::GATEWAY_TIMEOUT,
        )
        .await;
    assert_eq!(preview_value["error"]["code"], "request_timeout");
    assert_eq!(
        preview_value["error"]["outcome_unknown"], false,
        "read-only artifact query timeout must report outcome_unknown=false"
    );

    fixture.close().await;
}

#[tokio::test]
async fn all_artifact_mutations_and_reads_timeout_classification() {
    let fixture = Fixture::start(GatewayLimits {
        request_timeout: Duration::from_secs(1),
        ..Default::default()
    })
    .await;

    let write_methods = [
        "artifacts.put",
        "artifacts.update",
        "artifacts.action",
        "artifacts.action_cancel",
        "artifacts.recovery_save",
        "artifacts.recovery_load",
    ];
    for method in write_methods {
        let value = fixture
            .dispatched_artifact_rpc(method, json!({"slow": true}), StatusCode::GATEWAY_TIMEOUT)
            .await;
        assert_eq!(
            value["error"]["code"], "request_timeout",
            "method {method} should time out"
        );
        assert_eq!(
            value["error"]["outcome_unknown"], true,
            "dispatched artifact mutation {method} timeout must report outcome_unknown=true"
        );
    }

    let read_methods = [
        "artifacts.preview",
        "artifacts.list",
        "artifacts.action_list",
        "artifacts.action_get",
    ];
    for method in read_methods {
        let value = fixture
            .dispatched_artifact_rpc(method, json!({"slow": true}), StatusCode::GATEWAY_TIMEOUT)
            .await;
        assert_eq!(
            value["error"]["code"], "request_timeout",
            "method {method} should time out"
        );
        assert_eq!(
            value["error"]["outcome_unknown"], false,
            "read-only artifact query {method} timeout must report outcome_unknown=false"
        );
    }

    fixture.close().await;
}

#[tokio::test]
async fn artifact_mutation_dispatched_and_disconnected_reports_outcome_unknown_while_read_only_is_known()
 {
    let write_fixture = Fixture::start(GatewayLimits::default()).await;
    let write_value = write_fixture
        .dispatched_artifact_rpc(
            "artifacts.put",
            json!({"drop_connection": true}),
            StatusCode::BAD_GATEWAY,
        )
        .await;
    assert_eq!(write_value["error"]["code"], "connection_lost");
    assert_eq!(
        write_value["error"]["outcome_unknown"], true,
        "dispatched write request with lost connection must report outcome_unknown=true"
    );
    write_fixture.close().await;

    let read_fixture = Fixture::start(GatewayLimits::default()).await;
    let read_value = read_fixture
        .dispatched_artifact_rpc(
            "artifacts.preview",
            json!({"drop_connection": true}),
            StatusCode::BAD_GATEWAY,
        )
        .await;
    assert_eq!(read_value["error"]["code"], "connection_lost");
    assert_eq!(
        read_value["error"]["outcome_unknown"], false,
        "dispatched read-only request with lost connection must report outcome_unknown=false"
    );
    read_fixture.close().await;
}

#[tokio::test]
async fn background_and_feedback_writes_keep_lost_receipts_unknown() {
    let fixture = Fixture::start(GatewayLimits {
        request_timeout: Duration::from_secs(1),
        ..Default::default()
    })
    .await;
    for method in [
        "runtime.background.kill",
        "runtime.background.recover",
        "plannotator.submit",
    ] {
        let value = fixture
            .dispatched_artifact_rpc(method, json!({"slow": true}), StatusCode::GATEWAY_TIMEOUT)
            .await;
        assert_eq!(value["error"]["code"], "request_timeout", "{method}");
        assert_eq!(value["error"]["outcome_unknown"], true, "{method}");

        let disconnected = Fixture::start(GatewayLimits::default()).await;
        let value = disconnected
            .dispatched_artifact_rpc(
                method,
                json!({"drop_connection": true}),
                StatusCode::BAD_GATEWAY,
            )
            .await;
        assert_eq!(value["error"]["code"], "connection_lost", "{method}");
        assert_eq!(value["error"]["outcome_unknown"], true, "{method}");
        disconnected.close().await;
    }
    fixture.close().await;
}

#[tokio::test]
async fn undispatched_requests_keep_outcome_known() {
    let fixture = Fixture::start(GatewayLimits::default()).await;

    // Invalid JSON body -> 400 invalid_json -> outcome_unknown: false
    let mut stream = TcpStream::connect(fixture.addr).await.unwrap();
    stream
        .write_all(
            format!(
                "POST /api/rpc HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {WEB_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\nnot valid json",
                fixture.addr
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.contains("\"outcome_unknown\":false"));

    // Invalid params -> 400 invalid_params -> outcome_unknown: false
    let invalid_params = envelope(
        fixture
            .rpc("artifacts.put", json!("not an object"))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_eq!(invalid_params["error"]["code"], "invalid_params");
    assert_eq!(invalid_params["error"]["outcome_unknown"], false);

    // Method not allowed -> 403 method_not_allowed -> outcome_unknown: false
    let forbidden_method = envelope(
        fixture
            .rpc("nonexistent.method", json!({}))
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
    )
    .await;
    assert_eq!(forbidden_method["error"]["code"], "method_not_allowed");
    assert_eq!(forbidden_method["error"]["outcome_unknown"], false);

    fixture.close().await;
}

#[tokio::test]
async fn slow_request_bodies_cannot_hold_all_admission_slots() {
    let limits = GatewayLimits {
        max_pending_requests: 1,
        body_timeout: Duration::from_millis(150),
        ..Default::default()
    };
    let fixture = Fixture::start(limits).await;
    let mut slow = TcpStream::connect(fixture.addr).await.unwrap();
    slow.write_all(format!("POST /api/rpc HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {WEB_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{{", fixture.addr).as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), slow.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 408"));
    assert!(response.contains("body_timeout"));
    envelope(
        fixture
            .rpc("runtime.health", json!({}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    fixture.close().await;
}

#[tokio::test]
async fn observability_methods_are_allowed_and_forwarded() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let overview = envelope(
        fixture
            .rpc("session.context_overview", json!({"session_id":"s1"}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        overview["result"],
        json!({"method":"session.context_overview","params":{"session_id":"s1"}})
    );
    let inspector = envelope(
        fixture
            .rpc(
                "session.run_inspector",
                json!({"session_id":"s1","after":0,"limit":50}),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        inspector["result"],
        json!({"method":"session.run_inspector","params":{"session_id":"s1","after":0,"limit":50}})
    );
    fixture.close().await;
}

#[tokio::test]
async fn registered_panels_are_reachable_from_webui_product_entry() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let html = fixture
        .http
        .get(format!("http://{}/", fixture.addr))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        html.contains("id=\"panel-sidebar\""),
        "product HTML must expose the sidebar panel surface"
    );
    assert!(
        html.contains("id=\"panel-bottom\""),
        "product HTML must expose the bottom panel surface"
    );
    let script = fixture
        .http
        .get(format!("http://{}/app.js", fixture.addr))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        script.contains("/api/panels"),
        "shipped UI must call the panel workbench API"
    );
    assert!(!html.contains(WEB_TOKEN) && !script.contains(WEB_TOKEN));

    let catalog = envelope(
        fixture
            .http
            .post(format!("http://{}/api/panels", fixture.addr))
            .bearer_auth(WEB_TOKEN)
            .json(&json!({"method":"panels.catalog","params":{}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let result = catalog.get("result").unwrap();
    let sidebar: Vec<&str> = result["sidebar"]
        .as_array()
        .unwrap()
        .iter()
        .map(|panel| panel["id"].as_str().unwrap())
        .collect();
    let bottom: Vec<&str> = result["bottom"]
        .as_array()
        .unwrap()
        .iter()
        .map(|panel| panel["id"].as_str().unwrap())
        .collect();
    assert!(
        sidebar.contains(&"files"),
        "official files panel must appear in sidebar guide"
    );
    assert!(
        sidebar.contains(&"vendor:demo"),
        "third-party sample panel must appear in sidebar guide"
    );
    assert!(
        bottom.contains(&"terminal-bottom"),
        "terminal panel must appear in bottom guide"
    );

    let opened = envelope(
        fixture
            .http
            .post(format!("http://{}/api/panels", fixture.addr))
            .bearer_auth(WEB_TOKEN)
            .json(&json!({
                "method":"panels.open",
                "params":{
                    "session_id":"webui-local",
                    "panel_id":"files",
                    "zone":"sidebar",
                    "path":"README.md",
                    "activate":true
                }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(opened["result"]["panel_id"], "files");
    assert_eq!(opened["result"]["path"], "README.md");
    let instance_id = opened["result"]["instance_id"].as_str().unwrap().to_owned();

    let revealed = envelope(
        fixture
            .http
            .post(format!("http://{}/api/panels", fixture.addr))
            .bearer_auth(WEB_TOKEN)
            .json(&json!({
                "method":"panels.interact",
                "params":{
                    "session_id":"webui-local",
                    "instance_id":instance_id,
                    "kind":"editor",
                    "path":"src/lib.rs"
                }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(revealed["result"]["path"], "src/lib.rs");

    let terminal = envelope(
        fixture
            .http
            .post(format!("http://{}/api/panels", fixture.addr))
            .bearer_auth(WEB_TOKEN)
            .json(&json!({
                "method":"panels.open",
                "params":{
                    "session_id":"webui-local",
                    "panel_id":"terminal-bottom",
                    "activate":true
                }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let terminal_id = terminal["result"]["instance_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let focused = envelope(
        fixture
            .http
            .post(format!("http://{}/api/panels", fixture.addr))
            .bearer_auth(WEB_TOKEN)
            .json(&json!({
                "method":"panels.interact",
                "params":{
                    "session_id":"webui-local",
                    "instance_id":terminal_id,
                    "kind":"terminal"
                }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(focused["result"]["panel_id"], "terminal-bottom");
    assert_eq!(focused["result"]["active"], true);

    let listed = envelope(
        fixture
            .http
            .post(format!("http://{}/api/panels", fixture.addr))
            .bearer_auth(WEB_TOKEN)
            .json(&json!({"method":"panels.list_open","params":{"session_id":"webui-local"}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let open_ids: Vec<&str> = listed["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|panel| panel["panel_id"].as_str().unwrap())
        .collect();
    assert!(open_ids.contains(&"files"));
    assert!(open_ids.contains(&"terminal-bottom"));
    fixture.close().await;
}

#[tokio::test]
async fn serve_with_ownership_binds_session_and_reopen_restores_it() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ownership.json");
    let store = Arc::new(OwnershipStore::open(&path).unwrap());
    // Register device-alpha against the fixture IPC so route selection is real.
    let fixture = Fixture::start_with_ownership_device(
        GatewayLimits::default(),
        store.clone(),
        "device-alpha",
    )
    .await;

    let created = envelope(
        fixture
            .rpc(
                "session.create",
                json!({"id":"sess-original","device_id":"device-alpha"}),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(created["result"]["id"], "sess-original");
    assert_eq!(
        store.session_device("sess-original").as_deref(),
        Some("device-alpha")
    );

    // Route consults the store (session.get with session_id).
    let _ = envelope(
        fixture
            .rpc("session.get", json!({"session_id":"sess-original"}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;

    let enqueued = envelope(
        fixture
            .rpc(
                "task.enqueue",
                json!({"id":"task-child","session_id":"sess-original"}),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(enqueued["result"]["id"], "task-child");
    assert_eq!(
        store.task_device("task-child").as_deref(),
        Some("device-alpha"),
        "task bind inherits restored Session device"
    );

    fixture.close().await;

    // Gateway restart: reopen the same durable file.
    let restored = OwnershipStore::open(&path).unwrap();
    assert_eq!(
        restored.session_device("sess-original").as_deref(),
        Some("device-alpha"),
        "reopen must restore original Session ownership"
    );
    assert_eq!(
        restored.task_device("task-child").as_deref(),
        Some("device-alpha")
    );
}

#[tokio::test]
async fn ownership_persistence_failure_reports_unknown_after_runtime_acceptance() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ownership.json");
    let store = Arc::new(OwnershipStore::open(&path).unwrap());
    let fixture = Fixture::start_with_ownership_device(
        GatewayLimits::default(),
        store.clone(),
        "device-alpha",
    )
    .await;
    // An existing directory prevents replacing the ownership file after the
    // actual IPC handler has accepted creation. No private /tmp script is used.
    std::fs::create_dir(&path).unwrap();
    let response = fixture
        .rpc(
            "session.create",
            json!({
                "id":"accepted-without-durable-owner", "device_id":"device-alpha"
            }),
        )
        .send()
        .await
        .unwrap();
    let body = envelope(response, StatusCode::BAD_GATEWAY).await;
    assert_eq!(body["error"]["code"], "ownership_persist_outcome_unknown");
    assert_eq!(body["error"]["outcome_unknown"], true);
    assert!(body.get("result").is_none());
    assert_eq!(
        store
            .session_device("accepted-without-durable-owner")
            .as_deref(),
        Some("device-alpha")
    );
    fixture.close().await;
}

const ALPHA_IPC_TOKEN: &str = "aabbccddeeff00112233445566778899";

/// Distinct Runtime markers so the test can prove which IPC endpoint was used.
struct MarkerRuntime {
    marker: &'static str,
}

#[async_trait]
impl RpcHandler for MarkerRuntime {
    async fn call(
        &self,
        method: &str,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        assert!(
            params.get("device_id").is_none(),
            "gateway must consume routing metadata before Runtime dispatch"
        );
        match method {
            "session.create" | "session.fork"
                if params.get("id").and_then(Value::as_str).is_some() =>
            {
                Ok(json!({
                    "id": params["id"],
                    "state": "idle",
                    "runtime_marker": self.marker,
                }))
            }
            "session.get" => Ok(json!({
                "id": params.get("session_id").cloned().unwrap_or(json!("")),
                "state": "idle",
                "runtime_marker": self.marker,
            })),
            "task.enqueue" if params.get("id").and_then(Value::as_str).is_some() => Ok(json!({
                "id": params["id"],
                "state": "queued",
                "runtime_marker": self.marker,
            })),
            "runtime.health" => Ok(json!({
                "status": "ok",
                "runtime_marker": self.marker,
            })),
            _ => Ok(json!({
                "method": method,
                "params": params,
                "runtime_marker": self.marker,
            })),
        }
    }
}

async fn serve_marker_ipc(
    marker: &'static str,
    token: &str,
    stop: CancellationToken,
) -> (SocketAddr, JoinHandle<std::io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::new(token);
    config.request_timeout = Duration::from_secs(20);
    let task = tokio::spawn(cayw_ipc::serve(
        listener,
        Arc::new(MarkerRuntime { marker }),
        config,
        stop,
    ));
    (addr, task)
}

/// Restore + route: OwnershipStore device id must select the registered device
/// IPC, not the fixed default `state.ipc` connect target.
#[tokio::test]
async fn ownership_restore_routes_rpc_to_registered_device_endpoint() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ownership.json");
    {
        let store = OwnershipStore::open(&path).unwrap();
        store.bind_session("sess-original", "device-alpha").unwrap();
        store.bind_task("task-child", "device-alpha").unwrap();
    }

    let stop = CancellationToken::new();
    let (default_addr, default_task) =
        serve_marker_ipc("default-runtime", IPC_TOKEN, stop.clone()).await;
    let (alpha_addr, alpha_task) =
        serve_marker_ipc("alpha-runtime", ALPHA_IPC_TOKEN, stop.clone()).await;

    let devices = Arc::new(DeviceRouter::new(
        IpcEndpoint::new(default_addr, IPC_TOKEN).unwrap(),
    ));
    devices
        .register(
            "device-alpha",
            IpcEndpoint::new(alpha_addr, ALPHA_IPC_TOKEN).unwrap(),
        )
        .unwrap();

    // Gateway "restart": reopen the durable store and inject the same router.
    let store = Arc::new(OwnershipStore::open(&path).unwrap());
    assert_eq!(
        store.session_device("sess-original").as_deref(),
        Some("device-alpha")
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let web_addr = listener.local_addr().unwrap();
    let web_task = tokio::spawn(serve_with_ownership(
        listener,
        devices.clone(),
        WEB_TOKEN,
        GatewayLimits::default(),
        store.clone(),
        stop.clone(),
    ));

    let http = HttpClient::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let created = envelope(http.post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({"method":"session.create","params":{"id":"sess-on-alpha","device_id":"device-alpha"}}))
        .send().await.unwrap(), StatusCode::OK).await;
    assert_eq!(
        created["result"]["runtime_marker"], "alpha-runtime",
        "creation must execute on the selected device"
    );
    assert_eq!(
        store.session_device("sess-on-alpha").as_deref(),
        Some("device-alpha")
    );

    let forked = envelope(http.post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({"method":"session.fork","params":{"id":"fork-on-alpha","session_id":"sess-original","device_id":"device-alpha"}}))
        .send().await.unwrap(), StatusCode::OK).await;
    assert_eq!(forked["result"]["runtime_marker"], "alpha-runtime");
    assert_eq!(
        store.session_device("fork-on-alpha").as_deref(),
        Some("device-alpha")
    );

    let cross_device = envelope(http.post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({"method":"session.fork","params":{"id":"wrong-fork","session_id":"sess-original","device_id":"local"}}))
        .send().await.unwrap(), StatusCode::BAD_REQUEST).await;
    assert_eq!(cross_device["error"]["code"], "invalid_params");
    assert!(store.session_device("wrong-fork").is_none());

    let missing_device = envelope(http.post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({"method":"session.create","params":{"id":"wrong-create","device_id":"device-missing"}}))
        .send().await.unwrap(), StatusCode::BAD_GATEWAY).await;
    assert_eq!(missing_device["error"]["code"], "unknown_device");
    assert!(store.session_device("wrong-create").is_none());

    let routed = http
        .post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({"method":"session.get","params":{"session_id":"sess-original"}}))
        .send()
        .await
        .unwrap();
    let body = envelope(routed, StatusCode::OK).await;
    assert_eq!(
        body["result"]["runtime_marker"], "alpha-runtime",
        "restored ownership must connect to the registered device IPC, not default"
    );

    let task_routed = http
        .post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({"method":"task.get","params":{"task_id":"task-child"}}))
        .send()
        .await
        .unwrap();
    let task_body = envelope(task_routed, StatusCode::OK).await;
    assert_eq!(
        task_body["result"]["runtime_marker"], "alpha-runtime",
        "task ownership must also select the device endpoint"
    );

    // No store entry → fallback to default IPC only.
    let fallback = http
        .post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({"method":"runtime.health","params":{}}))
        .send()
        .await
        .unwrap();
    let fallback_body = envelope(fallback, StatusCode::OK).await;
    assert_eq!(
        fallback_body["result"]["runtime_marker"], "default-runtime",
        "fallback to default IPC only when OwnershipStore has no entry"
    );

    stop.cancel();
    for task in [web_task, default_task, alpha_task] {
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("shutdown")
            .unwrap()
            .unwrap();
    }
}

/// Owned non-local device without a registered endpoint must not silently use default.
#[tokio::test]
async fn owned_unregistered_device_does_not_fall_back_to_default_ipc() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ownership.json");
    let store = Arc::new(OwnershipStore::open(&path).unwrap());
    store.bind_session("sess-orphan", "device-missing").unwrap();

    let stop = CancellationToken::new();
    let (default_addr, default_task) =
        serve_marker_ipc("default-runtime", IPC_TOKEN, stop.clone()).await;
    let devices = Arc::new(DeviceRouter::new(
        IpcEndpoint::new(default_addr, IPC_TOKEN).unwrap(),
    ));
    // Intentionally do not register device-missing.

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let web_addr = listener.local_addr().unwrap();
    let web_task = tokio::spawn(serve_with_ownership(
        listener,
        devices,
        WEB_TOKEN,
        GatewayLimits::default(),
        store.clone(),
        stop.clone(),
    ));

    let http = HttpClient::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let response = http
        .post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({"method":"session.get","params":{"session_id":"sess-orphan"}}))
        .send()
        .await
        .unwrap();
    let body = envelope(response, StatusCode::BAD_GATEWAY).await;
    assert_eq!(body["error"]["code"], "unknown_device");
    assert!(
        body["result"].is_null() || body.get("result").is_none(),
        "must not return default-runtime result for an unregistered owned device"
    );

    stop.cancel();
    for task in [web_task, default_task] {
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("shutdown")
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn background_and_fleet_methods_pass_gateway_allowlist() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    for method in [
        "runtime.background.list",
        "runtime.background.inspect",
        "runtime.background.logs",
        "runtime.background.kill",
        "runtime.background.result",
        "runtime.background.recover",
        "runtime.fleet",
        "runtime.fleet.stop",
        "runtime.fleet.steer",
    ] {
        let response = fixture
            .rpc(method, json!({"id":"test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "method {method} should be allowed by gateway"
        );
        let body = envelope(response, StatusCode::OK).await;
        assert_eq!(body["result"]["method"], method);
    }
    fixture.close().await;
}

#[tokio::test]
async fn unified_artifacts_and_observability_rpc_allowlist_enforced() {
    let fixture = Fixture::start(GatewayLimits::default()).await;

    // Pruned legacy aliases and old namespace methods must be rejected
    for method in [
        "artifact.list",
        "artifact.action",
        "artifacts.action_dispatch",
        "artifacts.actions_list",
    ] {
        let result = envelope(
            fixture.rpc(method, json!({})).send().await.unwrap(),
            StatusCode::FORBIDDEN,
        )
        .await;
        assert_eq!(
            result["error"]["code"], "method_not_allowed",
            "legacy method {method} must be forbidden"
        );
    }

    // Canonical 10 artifacts.* methods must be permitted through the gateway
    for method in [
        "artifacts.put",
        "artifacts.update",
        "artifacts.preview",
        "artifacts.list",
        "artifacts.action",
        "artifacts.action_list",
        "artifacts.action_get",
        "artifacts.action_cancel",
        "artifacts.recovery_save",
        "artifacts.recovery_load",
    ] {
        let result = envelope(
            fixture
                .rpc(method, json!({"test": true}))
                .send()
                .await
                .unwrap(),
            StatusCode::OK,
        )
        .await;
        assert_eq!(
            result["result"]["method"], method,
            "canonical method {method} must be allowed"
        );
    }

    // Observability and plannotator methods must be permitted through the gateway
    for method in [
        "session.context_overview",
        "session.run_inspector",
        "session.plan",
        "plannotator.submit",
        "plannotator.list",
    ] {
        let result = envelope(
            fixture
                .rpc(method, json!({"test": true}))
                .send()
                .await
                .unwrap(),
            StatusCode::OK,
        )
        .await;
        assert_eq!(
            result["result"]["method"], method,
            "observability/plannotator method {method} must be allowed"
        );
    }

    fixture.close().await;
}
