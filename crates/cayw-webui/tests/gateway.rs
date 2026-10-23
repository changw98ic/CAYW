use async_trait::async_trait;
use cayw_ipc::{RpcError, RpcHandler, ServerConfig};
use cayw_webui::{
    GatewayLimits, IpcEndpoint, MAX_REQUEST_BYTES, ProjectEndpoint, serve_projects_with_limits,
    serve_with_limits,
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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const IPC_TOKEN: &str = "fedcba9876543210fedcba9876543210";

struct RuntimeFixture {
    prompt_started: Semaphore,
    prompt_release: Semaphore,
    prompt_cancelled: AtomicBool,
}

impl Default for RuntimeFixture {
    fn default() -> Self {
        Self {
            prompt_started: Semaphore::new(0),
            prompt_release: Semaphore::new(0),
            prompt_cancelled: AtomicBool::new(false),
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
                json!({"id":"s1","status":"ok","messages":[{"role":"assistant","content":IPC_TOKEN}]}),
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
                self.prompt_release.add_permits(1);
                Ok(json!({"cancelled":true}))
            }
            "session.get" if params["session_id"] == "error" => Err(RpcError::new(
                "needs_reconciliation",
                "tool outcome must be checked",
            )),
            "session.get" if params["session_id"] == "secrets" => Err(RpcError::new(
                "fixture_error",
                format!("do not expose {IPC_TOKEN}"),
            )),
            "session.get" if params["session_id"] == "secret_result" => {
                Ok(json!({"tool_output":format!("{IPC_TOKEN}"),"content":[IPC_TOKEN]}))
            }
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
            "session.get" if params["session_id"] == "slow" => {
                tokio::time::sleep(Duration::from_secs(20)).await;
                Ok(json!({}))
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
        let stop = CancellationToken::new();
        let runtime = Arc::new(RuntimeFixture::default());
        let ipc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ipc_addr = ipc_listener.local_addr().unwrap();
        let mut ipc_config = ServerConfig::new(IPC_TOKEN);
        ipc_config.request_timeout = Duration::from_secs(20);
        let ipc_task = tokio::spawn(cayw_ipc::serve(
            ipc_listener,
            runtime.clone(),
            ipc_config,
            stop.clone(),
        ));
        let ipc = IpcEndpoint::new(ipc_addr, IPC_TOKEN).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let web_task = tokio::spawn(serve_with_limits(listener, ipc, limits, stop.clone()));
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
            .json(&json!({"method":method,"params":params}))
    }
    async fn close(self) {
        self.stop.cancel();
        for task in self.tasks {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("gateway must stop")
                .unwrap()
                .unwrap();
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
    }
    fixture.close().await;
}

#[tokio::test]
async fn host_origin_and_fetch_site_are_checked_before_forwarding() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    for request in [
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
        fixture.http.get(&endpoint).send().await.unwrap(),
        StatusCode::METHOD_NOT_ALLOWED,
    )
    .await;
    envelope(
        fixture
            .http
            .post(format!("{endpoint}?token=secret"))
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
            .rpc("session.cancel", json!({"session_id":"s1"}))
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
    browser.write_all(format!("POST /api/rpc HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", fixture.addr, body.len()).as_bytes()).await.unwrap();
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
            .rpc("session.cancel", json!({"session_id":"s1"}))
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
async fn slow_request_bodies_cannot_hold_all_admission_slots() {
    let limits = GatewayLimits {
        max_pending_requests: 1,
        body_timeout: Duration::from_millis(150),
        ..Default::default()
    };
    let fixture = Fixture::start(limits).await;
    let mut slow = TcpStream::connect(fixture.addr).await.unwrap();
    slow.write_all(format!("POST /api/rpc HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{{", fixture.addr).as_bytes()).await.unwrap();
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
async fn projects_list_and_project_routing_isolate_configured_endpoints() {
    let fixture = Fixture::start(GatewayLimits::default()).await;
    let list = envelope(
        fixture
            .rpc("projects.list", json!({}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(list["result"].as_array().unwrap().len(), 1);
    assert_eq!(list["result"][0]["id"], "default");
    assert_eq!(list["result"][0]["default"], true);
    assert!(!list.to_string().contains(IPC_TOKEN));

    let bad_project = fixture
        .http
        .post(format!("http://{}/api/rpc", fixture.addr))
        .json(&json!({"method": "runtime.health", "params": {}, "project": "missing-project"}))
        .send()
        .await
        .unwrap();
    let err = envelope(bad_project, StatusCode::BAD_REQUEST).await;
    assert_eq!(err["error"]["code"], "invalid_params");
    assert_eq!(err["error"]["outcome_unknown"], false);
    fixture.close().await;
}

#[tokio::test]
async fn multi_project_gateway_routes_to_selected_endpoint_and_redacts_all_project_tokens() {
    const SECOND_IPC_TOKEN: &str = "abcdef0123456789abcdef0123456789";
    let stop = CancellationToken::new();
    let runtime1 = Arc::new(RuntimeFixture::default());
    let runtime2 = Arc::new(RuntimeFixture::default());

    let ipc1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr1 = ipc1.local_addr().unwrap();
    let task1 = tokio::spawn(cayw_ipc::serve(
        ipc1,
        runtime1,
        ServerConfig::new(IPC_TOKEN),
        stop.clone(),
    ));

    let ipc2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = ipc2.local_addr().unwrap();
    let task2 = tokio::spawn(cayw_ipc::serve(
        ipc2,
        runtime2,
        ServerConfig::new(SECOND_IPC_TOKEN),
        stop.clone(),
    ));

    let projects = vec![
        ProjectEndpoint::new(
            "default",
            "默认项目",
            IpcEndpoint::new(addr1, IPC_TOKEN).unwrap(),
        )
        .unwrap(),
        ProjectEndpoint::new(
            "docs",
            "文档项目",
            IpcEndpoint::new(addr2, SECOND_IPC_TOKEN).unwrap(),
        )
        .unwrap(),
    ];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let web_task = tokio::spawn(serve_projects_with_limits(
        listener,
        projects,
        GatewayLimits::default(),
        stop.clone(),
    ));
    let http = HttpClient::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let list = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "projects.list", "params": {}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(list["result"].as_array().unwrap().len(), 2);
    assert_eq!(list["result"][1]["id"], "docs");
    assert_eq!(list["result"][1]["label"], "文档项目");

    let routed = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "session.create", "params": {"label": "第二项目会话"}, "project": "docs"}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(routed["result"]["params"]["label"], "第二项目会话");

    stop.cancel();
    for task in [task1, task2, web_task] {
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn tokenless_web_device_rsa_binding_and_server_account_login_flow() {
    let stop = CancellationToken::new();
    let runtime = Arc::new(RuntimeFixture::default());
    let ipc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ipc_addr = ipc_listener.local_addr().unwrap();
    let ipc_task = tokio::spawn(cayw_ipc::serve(
        ipc_listener,
        runtime,
        ServerConfig::loopback(),
        stop.clone(),
    ));
    let ipc = IpcEndpoint::loopback(ipc_addr).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let web_task = tokio::spawn(serve_with_limits(
        listener,
        ipc,
        GatewayLimits::default(),
        stop.clone(),
    ));
    let http = HttpClient::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    // 1. Web <-> Device local connection works with zero token.
    let health = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "runtime.health", "params": {}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(health["result"]["status"], "ok");

    // 2. Before any remote Device completes the 2-phase RSA challenge-response handshake,
    //    `bound`, `connected`, and `encrypted_channel_verified` are all honestly `false`.
    let binding = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "device.binding.status", "params": {}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(binding["result"]["bound"], false);
    assert_eq!(binding["result"]["connected"], false);
    assert_eq!(binding["result"]["encrypted_channel_verified"], false);

    // 2a. Prepare a bind session from WebUI: still unbound (`waiting_for_hello`).
    let prep = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "devices.prepare_bind", "params": {}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let bind_id = prep["result"]["bind_id"].as_str().unwrap().to_owned();
    assert_eq!(prep["result"]["bound"], false);
    assert_eq!(prep["result"]["encrypted_channel_verified"], false);

    // 2b. External Device generates its own RSA keypair (private key never sent to Server)
    //     and sends `devices.bind_hello` with only its `RsaPublicKey`.
    let dev_kp = cayw_remote::RsaKeyPair::generate().unwrap();
    let dev_pub = dev_kp.public_key();
    let hello = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({
                "method": "devices.bind_hello",
                "params": {
                    "bind_id": bind_id,
                    "label": "Mac Studio 远程工作站",
                    "device_address": "192.168.1.20 · 独立进程握手",
                    "device_public_key": dev_pub,
                }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    // After hello alone, the Device is STILL not bound until it proves it decrypted the challenge!
    assert_eq!(hello["result"]["phase"], "challenge_issued");
    assert_eq!(hello["result"]["bound"], false);
    assert_eq!(hello["result"]["encrypted_channel_verified"], false);

    let srv_pub: cayw_remote::RsaPublicKey =
        serde_json::from_value(hello["result"]["server_public_key"].clone()).unwrap();
    let encrypted_challenge = hello["result"]["encrypted_challenge"].as_str().unwrap();
    let server_sig = hello["result"]["server_signature"].as_str().unwrap();
    assert!(srv_pub.verify_sha256(
        format!("{bind_id}:{encrypted_challenge}").as_bytes(),
        server_sig
    ));

    // Tampered or wrong challenge confirmation is rejected and never marks the device bound.
    let bad_proof = format!("confirm:{bind_id}:wrong_nonce");
    envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({
                "method": "devices.bind_confirm",
                "params": {
                    "bind_id": bind_id,
                    "encrypted_response": srv_pub.encrypt(bad_proof.as_bytes()).unwrap(),
                    "device_signature": dev_kp.sign_sha256(bad_proof.as_bytes()).unwrap(),
                }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;

    // 2c. Device decrypts the Server's challenge with its private key, encrypts the proof to
    //     Server's public key, and signs with Device's private key.
    let challenge_nonce = String::from_utf8(dev_kp.decrypt(encrypted_challenge).unwrap()).unwrap();
    let valid_proof = format!("confirm:{bind_id}:{challenge_nonce}");
    let confirmed = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({
                "method": "devices.bind_confirm",
                "params": {
                    "bind_id": bind_id,
                    "encrypted_response": srv_pub.encrypt(valid_proof.as_bytes()).unwrap(),
                    "device_signature": dev_kp.sign_sha256(valid_proof.as_bytes()).unwrap(),
                }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(confirmed["result"]["created"]["bound"], true);
    assert_eq!(
        confirmed["result"]["created"]["encrypted_channel_verified"],
        true
    );
    assert_eq!(confirmed["result"]["created"]["connected"], true);
    let bound_device_id = confirmed["result"]["created"]["device_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 2d. Signed heartbeat from the Device private key updates live connection status.
    let hb_ts = "1700000000000";
    let hb_sig = dev_kp
        .sign_sha256(format!("heartbeat:{bound_device_id}:{hb_ts}").as_bytes())
        .unwrap();
    let hb = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({
                "method": "devices.heartbeat",
                "params": {
                    "device_id": bound_device_id,
                    "timestamp": hb_ts,
                    "signature": hb_sig,
                }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(hb["result"]["connected"], true);

    let removed = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({
                "method": "devices.remove",
                "params": { "device_id": bound_device_id }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(removed["result"]["devices"].as_array().unwrap().len(), 0);

    // 3. Web <-> Server mode requires account login (default admin/admin) and supports changing credentials.
    envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "server.auth.set_mode", "params": {"mode": "remote_server"}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;

    let blocked = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "session.list", "params": {}}))
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_eq!(blocked["error"]["code"], "server_login_required");

    // Login with default account (admin / admin)
    let login = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "server.auth.login", "params": {"username": "admin", "password": "admin"}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(login["result"]["logged_in"], true);

    // Modify login username and password after logging in
    let updated = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({
                "method": "server.auth.update_credentials",
                "params": {
                    "current_password": "admin",
                    "new_username": "cayw_operator",
                    "new_password": "NewSecurePassword2026!"
                }
            }))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(updated["result"]["username"], "cayw_operator");
    assert_eq!(updated["result"]["is_default_credentials"], false);

    // Logout and verify old default credentials fail while new credentials succeed
    envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "server.auth.logout", "params": {}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "server.auth.login", "params": {"username": "admin", "password": "admin"}}))
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    let relogin = envelope(
        http.post(format!("http://{addr}/api/rpc"))
            .json(&json!({"method": "server.auth.login", "params": {"username": "cayw_operator", "password": "NewSecurePassword2026!"}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(relogin["result"]["logged_in"], true);

    stop.cancel();
    for task in [ipc_task, web_task] {
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
