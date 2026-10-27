//! Real loopback HTTP checks for the trace debug gateway.
//!
//! These tests exercise the listener, header policy, body bound and graceful
//! shutdown. They use an empty [`TraceStore`] and never start a Runtime task.

use cayw_codeintel::{SymbolInput, SymbolVersionBridge, content_sha256};
use cayw_core::{AttemptLease, CoreEvent, EventRecord, ExecutionRecord, ExecutionState};
use cayw_trace_debug::{
    Document, Edge, Error, GraphPlan, Node, NodeTrace, Project, Revision, Status, TraceEnvelope,
    TraceStore, server::serve,
};
use serde_json::json;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Running {
    address: SocketAddr,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), Error>>,
    _directory: tempfile::TempDir,
}

impl Running {
    async fn shutdown(self) {
        self.cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), self.task)
            .await
            .expect("server did not stop after cancellation");
        assert!(matches!(&result, Ok(Ok(()))), "server failed: {result:?}");
    }
}

async fn start() -> Running {
    start_with(TOKEN.to_string()).await
}

async fn start_with(token: String) -> Running {
    let directory = tempfile::tempdir().unwrap();
    let store = TraceStore::open(directory.path().join("trace.json")).unwrap();
    start_store(directory, store, token).await
}

async fn start_store(
    directory: tempfile::TempDir,
    store: std::sync::Arc<TraceStore>,
    token: String,
) -> Running {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve(listener, store, token, cancel.clone()));
    Running {
        address,
        cancel,
        task,
        _directory: directory,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn url(address: SocketAddr, path: &str) -> String {
    format!("http://{address}{path}")
}

async fn raw(address: SocketAddr, request: String) -> String {
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut output = String::new();
        stream.read_to_string(&mut output).unwrap();
        output
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn refuses_public_listener_and_unusable_web_tokens() {
    let directory = tempfile::tempdir().unwrap();
    let store = TraceStore::open(directory.path().join("trace.json")).unwrap();
    let listener = TcpListener::bind(("0.0.0.0", 0)).await.unwrap();
    let error = serve(listener, store, TOKEN.to_string(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(&error, Error::Invalid(_)), "{error:?}");

    for token in [
        String::new(),
        "short".to_string(),
        "with space 0123456789abcdef0123456789".to_string(),
        "秘密0123456789abcdef0123456789abcdef".to_string(),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = TraceStore::open(directory.path().join("trace.json")).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let error = serve(listener, store, token, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(&error, Error::Invalid(_)), "{error:?}");
    }
}

#[tokio::test]
async fn static_page_is_host_guarded_and_api_requires_the_web_token() {
    let running = start().await;

    // The page itself must load before a token can be typed into it.
    let response = client()
        .get(url(running.address, "/"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("default-src 'none'"))
    );

    // A page from another origin cannot read or edit loopback state.
    let response = client()
        .get(url(running.address, "/"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

    let response = client()
        .get(url(running.address, "/"))
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

    // DNS rebinding must not pass even with a same-looking origin.
    let port = running.address.port();
    let body = raw(
        running.address,
        format!(
            "GET / HTTP/1.1\r\nHost: evil.example\r\nOrigin: http://evil.example:{port}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(body.starts_with("HTTP/1.1 403"), "{body}");

    // Static resources remain reachable for the exact bound authority.
    let response = client()
        .get(url(running.address, "/"))
        .header("origin", format!("http://{}", running.address))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let state = url(running.address, "/api/state");
    assert_eq!(
        client().get(state.as_str()).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client()
            .get(state.as_str())
            .header("authorization", "Bearer 0123456789abcdef0123456789abcdee")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let response = client()
        .get(state.as_str())
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let document: serde_json::Value = response.json().await.unwrap();
    assert_eq!(document["schema"], "cayw.trace.v1");
    assert_eq!(document["sequence"], 0);
    assert!(document["projects"].is_object());

    // The token never travels in a URL.
    assert_eq!(
        client()
            .get(url(running.address, &format!("/api/state?token={TOKEN}")))
            .header("authorization", format!("Bearer {TOKEN}"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::BAD_REQUEST
    );

    running.shutdown().await;
}

#[tokio::test]
async fn workbench_navigation_opens_only_the_login_document() {
    let running = start().await;
    let navigate = |request: reqwest::RequestBuilder| {
        request
            .header("sec-fetch-site", "same-site")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-dest", "document")
            .header("sec-fetch-user", "?1")
    };
    let response = navigate(client().get(url(running.address, "/")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()["content-security-policy"],
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'"
    );
    assert!(response.text().await.unwrap().contains("id=\"web-token\""));

    for path in [
        "/app.js",
        "/styles.css",
        "/api/state",
        "/api/run-inspector",
        "/?token=must-not-be-consumed",
    ] {
        let response = navigate(client().get(url(running.address, path)))
            .header("authorization", format!("Bearer {TOKEN}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN, "{path}");
    }
    for (name, value) in [
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-mode", "cors"),
        ("sec-fetch-dest", "iframe"),
        ("sec-fetch-user", "?0"),
        ("origin", "http://evil.example"),
        ("host", "evil.example"),
    ] {
        let response = navigate(client().get(url(running.address, "/")))
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN, "{name}");
    }
    let response = navigate(client().post(url(running.address, "/")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(
        client()
            .get(url(running.address, "/api/state"))
            .header("sec-fetch-site", "same-origin")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let response = client()
        .get(url(running.address, "/api/state"))
        .header("sec-fetch-site", "same-origin")
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.json::<Document>().await.unwrap().sequence, 0);
    running.shutdown().await;
}

#[tokio::test]
async fn edits_reject_bad_input_without_touching_the_store() {
    let running = start().await;
    let edit = url(running.address, "/api/projects/demo/edit");
    let undo = url(running.address, "/api/projects/demo/undo");
    let authorized = |builder: reqwest::RequestBuilder| {
        builder.header("authorization", format!("Bearer {TOKEN}"))
    };

    // Malformed JSON and schema violations are unprocessable, not a silent edit.
    for body in [
        "{",
        r#"{"expected_version":0,"node_id":"n","source":"s","params":{},"extra":1}"#,
    ] {
        let response = authorized(client().post(edit.as_str()))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }

    // A URL parameter is refused even when the credential itself is valid.
    assert_eq!(
        authorized(client().post(format!("{edit}?expected_version=0")))
            .json(&serde_json::json!({
                "expected_version": 0,
                "node_id": "n",
                "source": "return {}",
                "params": {},
                "input_pointer": "",
            }))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::BAD_REQUEST
    );

    // Oversized bodies are rejected before the store sees them.
    let oversized = serde_json::json!({
        "expected_version": 0,
        "node_id": "n",
        "source": "x".repeat(cayw_trace_debug::server::MAX_BODY_BYTES),
        "params": {},
        "input_pointer": "",
    });
    let response = authorized(client().post(edit.as_str()))
        .json(&oversized)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    // Unknown routes and wrong methods have explicit JSON errors.
    assert_eq!(
        authorized(client().get(url(running.address, "/api/nope")))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    assert_eq!(
        authorized(client().get(edit.as_str()))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::METHOD_NOT_ALLOWED
    );

    // A syntactically valid undo still requires the credential.
    assert_eq!(
        client()
            .post(undo.as_str())
            .json(&serde_json::json!({"expected_version": 0, "target_version": 0}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    running.shutdown().await;
}

#[tokio::test]
async fn store_errors_map_to_conflict_and_unprocessable_entity() {
    let directory = tempfile::tempdir().unwrap();
    let plan = GraphPlan {
        id: "demo-plan".into(),
        label: "demo".into(),
        entry: "node".into(),
        nodes: vec![Node {
            id: "node".into(),
            label: "node".into(),
            module: "demo".into(),
            source: "return {output={},state={}}".into(),
            params: serde_json::json!({}),
            input_pointer: String::new(),
            implementation: "lua".into(),
        }],
        edges: vec![],
        input: serde_json::json!({}),
        initial_state: serde_json::json!({}),
        context: serde_json::json!({}),
    };
    let mut document = Document::default();
    document.projects.insert(
        "demo".into(),
        Project {
            id: "demo".into(),
            session_id: "session".into(),
            attempt_id: "attempt".into(),
            task_id: "task".into(),
            source_hash: "sha256:demo".into(),
            parent_call_id: "parent".into(),
            lease: None,
            revisions: vec![Revision {
                version: 1,
                parent: None,
                changed_node: None,
                plan,
                created_at_ms: 1,
                reason: "capture".into(),
            }],
            evaluations: vec![],
            calls: vec![],
            gaps: vec![],
            active_node: None,
            trace_error: None,
            runtime_status: Status::Completed,
            symbol_hangs: Default::default(),
        },
    );
    std::fs::write(
        directory.path().join("trace.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let store = TraceStore::open(directory.path().join("trace.json")).unwrap();
    let running = start_store(directory, store, TOKEN.to_string()).await;

    let http = client();
    let auth = |builder: reqwest::RequestBuilder| {
        builder.header("authorization", format!("Bearer {TOKEN}"))
    };
    let edit = serde_json::json!({
        "expected_version": 999,
        "node_id": "node",
        "source": "return {output={},state={}}",
        "params": {},
        "input_pointer": "",
    });
    let response = auth(http.post(url(running.address, "/api/projects/demo/edit")))
        .json(&edit)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);

    let response = auth(http.post(url(running.address, "/api/projects/demo/undo")))
        .json(&serde_json::json!({"expected_version": 999, "target_version": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);

    let response = auth(http.post(url(running.address, "/api/projects/missing/edit")))
        .json(&serde_json::json!({
            "expected_version": 0,
            "node_id": "node",
            "source": "return {output={},state={}}",
            "params": {},
            "input_pointer": "",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

    running.shutdown().await;
}

#[tokio::test]
async fn long_poll_returns_immediately_on_shutdown() {
    let running = start().await;
    let state = url(running.address, "/api/state?after=0");
    let pending = tokio::spawn(async move {
        client()
            .get(state)
            .header("authorization", format!("Bearer {TOKEN}"))
            .send()
            .await
            .map(|response| response.status())
    });

    // Let the request reach the waiting state, then prove cancellation wakes it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    running.cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .expect("long poll ignored shutdown")
        .unwrap();
    // The connection may close before the 503 is written.
    if let Ok(status) = outcome {
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    }

    let result = tokio::time::timeout(Duration::from_secs(3), running.task)
        .await
        .expect("server did not stop after cancellation");
    assert!(matches!(&result, Ok(Ok(()))), "server failed: {result:?}");
}

#[tokio::test]
async fn ingest_symbol_hangs_surface_through_http_state() {
    let directory = tempfile::tempdir().unwrap();
    let bridge = Arc::new(SymbolVersionBridge::new());
    let digest = content_sha256(b"fn input() {}\nfn condition() {}\n");
    bridge
        .publish_file(
            "examples/demo.rs",
            &digest,
            "rust",
            vec![
                SymbolInput {
                    name: "input".into(),
                    kind: "function".into(),
                    byte_start: 0,
                    byte_end: 12,
                    start_line: Some(1),
                    end_line: Some(1),
                    parent_index: None,
                },
                SymbolInput {
                    name: "condition".into(),
                    kind: "function".into(),
                    byte_start: 14,
                    byte_end: 30,
                    start_line: Some(2),
                    end_line: Some(2),
                    parent_index: None,
                },
            ],
        )
        .unwrap();

    let plan = GraphPlan {
        id: "demo".into(),
        label: "F16 http".into(),
        entry: "input".into(),
        nodes: vec![
            Node {
                id: "input".into(),
                label: "读取输入".into(),
                module: "io".into(),
                source: "return {output=input,state=state}".into(),
                params: json!({}),
                input_pointer: String::new(),
                implementation: "examples/demo.rs::input".into(),
            },
            Node {
                id: "condition".into(),
                label: "条件".into(),
                module: "logic".into(),
                source: "error('bad')".into(),
                params: json!({}),
                input_pointer: String::new(),
                implementation: "examples/demo.rs::condition".into(),
            },
            Node {
                id: "luau_only".into(),
                label: "纯 Luau".into(),
                module: "logic".into(),
                source: "return {output=1,state=state}".into(),
                params: json!({}),
                input_pointer: String::new(),
                implementation: "luau".into(),
            },
        ],
        edges: vec![
            Edge {
                from: "input".into(),
                to: "condition".into(),
                branch: None,
            },
            Edge {
                from: "condition".into(),
                to: "luau_only".into(),
                branch: None,
            },
        ],
        input: json!({"value": 10}),
        initial_state: json!({}),
        context: json!({}),
    };
    let source_hash = {
        use sha2::{Digest, Sha256};
        format!(
            "sha256:{:x}",
            Sha256::digest(
                cayw_trace_debug::engine::script_for_plan(&plan)
                    .unwrap()
                    .as_bytes()
            )
        )
    };
    let lease = AttemptLease {
        session_id: "session".into(),
        attempt_id: "attempt".into(),
        worker_id: "worker".into(),
        fence: 1,
    };
    let event = |cursor, event| EventRecord {
        protocol_version: 3,
        session_id: "session".into(),
        cursor,
        timestamp: cayw_trace_debug::now_ms(),
        attempt: Some(lease.clone()),
        event,
    };

    let store =
        TraceStore::open_with_bridge(directory.path().join("trace.json"), Arc::clone(&bridge))
            .unwrap();
    let envelope = TraceEnvelope {
        schema: cayw_trace_debug::TRACE_SCHEMA.into(),
        trace: NodeTrace::Manifest { plan },
    };
    store
        .ingest(&[
            event(
                1,
                CoreEvent::ExecutionStarted {
                    execution: ExecutionRecord {
                        execution_id: "exec".into(),
                        parent_call_id: "tool".into(),
                        task_id: "task".into(),
                        source_hash: source_hash.clone(),
                        lease: lease.clone(),
                        state: ExecutionState::Running,
                        terminal: None,
                    },
                },
            ),
            event(
                2,
                CoreEvent::Host {
                    kind: "trace_node".into(),
                    data: json!({
                        "execution_id": "exec",
                        "task_id": "task",
                        "source_hash": source_hash,
                        "parent_call_id": "tool",
                        "entry": {
                            "cursor": 1,
                            "output": {"type": "json", "value": envelope}
                        }
                    }),
                },
            ),
        ])
        .unwrap();

    let running = start_store(directory, store, TOKEN.to_string()).await;
    let response = client()
        .get(url(running.address, "/api/state"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let document: Document = response.json().await.unwrap();
    let hangs = &document.projects["exec"].symbol_hangs;
    assert_eq!(
        hangs.len(),
        2,
        "HTTP state must carry ingest-linked hangs for UI"
    );
    assert_eq!(hangs["input"].file.path, "examples/demo.rs");
    assert_eq!(hangs["input"].file.version, 1);
    assert_eq!(hangs["input"].symbol.as_ref().unwrap().name, "input");
    assert_eq!(
        hangs["condition"].symbol.as_ref().unwrap().name,
        "condition"
    );
    assert!(!hangs.contains_key("luau_only"));

    running.shutdown().await;
}

#[tokio::test]
async fn run_inspector_route_returns_timeline_and_page_metadata() {
    let running = start().await;
    let http = client();
    let auth = |builder: reqwest::RequestBuilder| {
        builder.header("authorization", format!("Bearer {TOKEN}"))
    };
    let unauth = http
        .get(url(running.address, "/api/run-inspector"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    let bad_query = auth(http.get(url(running.address, "/api/run-inspector?limit=0")))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_query.status(), reqwest::StatusCode::BAD_REQUEST);
    let response = auth(http.get(url(running.address, "/api/run-inspector?after=0&limit=100")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["protocol_version"],
        cayw_run_inspector::RUN_INSPECTOR_PROTOCOL_VERSION
    );
    assert_eq!(body["events_total"], 0);
    assert_eq!(body["events_scanned"], 0);
    assert_eq!(body["has_more"], false);
    assert_eq!(body["next_after"], 0);
    assert!(body["timelines"].as_array().unwrap().is_empty());

    running.shutdown().await;
}
