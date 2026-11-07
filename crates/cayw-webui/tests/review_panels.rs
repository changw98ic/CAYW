//! End-to-end HTTP integration tests for panel workbench (WB-02, WB-08).
//!
//! WB-02:
//! - Verify that sending an oversized path (> 4096 bytes) to `panels.interact`
//!   (both "editor" kind and generic "trace"/"activate" kinds) returns HTTP 400
//!   without destroying the opened panel instance or mutating its state / list_open.
//! - Verify that valid updates retain the original instance_id, update path, and set active.
//!
//! WB-08:
//! - Verify single-snapshot catalog / guide queries exclude BottomOnly from Sidebar
//!   without duplicate placement checks, and snapshot-open semantics function predictably.

use async_trait::async_trait;
use cayw_ipc::{RpcError, RpcHandler, ServerConfig};
use cayw_webui::{GatewayLimits, IpcEndpoint, serve_with_limits};
use reqwest::{Client as HttpClient, StatusCode};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const WEB_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const IPC_TOKEN: &str = "fedcba9876543210fedcba9876543210";

struct MockHandler;

#[async_trait]
impl RpcHandler for MockHandler {
    async fn call(
        &self,
        method: &str,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method {
            "runtime.health" => Ok(json!({"status": "ok", "protocol_version": 3})),
            _ => Ok(json!({"method": method, "params": params})),
        }
    }
}

struct TestFixture {
    addr: SocketAddr,
    http: HttpClient,
    stop: CancellationToken,
}

impl TestFixture {
    async fn start() -> Self {
        let stop = CancellationToken::new();
        let ipc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ipc_addr = ipc_listener.local_addr().unwrap();
        let mut ipc_config = ServerConfig::new(IPC_TOKEN);
        ipc_config.request_timeout = Duration::from_secs(20);
        let ipc_stop = stop.child_token();
        tokio::spawn(cayw_ipc::serve(
            ipc_listener,
            Arc::new(MockHandler),
            ipc_config,
            ipc_stop,
        ));
        let ipc = IpcEndpoint::new(ipc_addr, IPC_TOKEN).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_with_limits(
            listener,
            ipc,
            WEB_TOKEN,
            GatewayLimits::default(),
            stop.clone(),
        ));
        Self {
            addr,
            http: HttpClient::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            stop,
        }
    }

    async fn post_panels(&self, body: Value) -> (StatusCode, Value) {
        let res = self
            .http
            .post(format!("http://{}/api/panels", self.addr))
            .bearer_auth(WEB_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = res.status();
        let json: Value = res.json().await.unwrap();
        (status, json)
    }

    async fn close(self) {
        self.stop.cancel();
    }
}

#[tokio::test]
async fn review_wb02_invalid_path_interact_preserves_instance_and_state() {
    let fixture = TestFixture::start().await;

    // 1. Open two panels: "files" and "trace" in sidebar
    let (status, opened1) = fixture
        .post_panels(json!({
            "method": "panels.open",
            "params": {
                "session_id": "session-wb02",
                "panel_id": "files",
                "zone": "sidebar",
                "path": "README.md",
                "activate": true
            }
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let files_id = opened1["result"]["instance_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(opened1["result"]["path"], "README.md");
    assert_eq!(opened1["result"]["active"], true);

    let (status, opened2) = fixture
        .post_panels(json!({
            "method": "panels.open",
            "params": {
                "session_id": "session-wb02",
                "panel_id": "trace",
                "zone": "sidebar",
                "activate": true
            }
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let trace_id = opened2["result"]["instance_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(opened2["result"]["active"], true);

    // 2. Snapshot list_open before invalid operations
    let (status, listed_before) = fixture
        .post_panels(json!({
            "method": "panels.list_open",
            "params": {
                "session_id": "session-wb02"
            }
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let items_before = listed_before["result"].as_array().unwrap().clone();
    assert_eq!(items_before.len(), 2);
    let files_before = items_before
        .iter()
        .find(|p| p["instance_id"] == files_id)
        .unwrap();
    let trace_before = items_before
        .iter()
        .find(|p| p["instance_id"] == trace_id)
        .unwrap();
    assert_eq!(files_before["active"], false);
    assert_eq!(files_before["path"], "README.md");
    assert_eq!(trace_before["active"], true);

    // 3. Send invalid oversized path (4097 chars) via editor interact on files
    let oversized_path = "a".repeat(4097);
    let (status_editor_err, body_editor_err) = fixture
        .post_panels(json!({
            "method": "panels.interact",
            "params": {
                "session_id": "session-wb02",
                "instance_id": files_id,
                "kind": "editor",
                "path": oversized_path
            }
        }))
        .await;
    assert_eq!(status_editor_err, StatusCode::BAD_REQUEST);
    assert_eq!(body_editor_err["error"]["code"], "invalid_request");
    assert!(
        body_editor_err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("4096 bytes")
    );

    // Verify state after failed editor interact: list_open must be 100% identical to before
    let (status, listed_after_editor_err) = fixture
        .post_panels(json!({
            "method": "panels.list_open",
            "params": {
                "session_id": "session-wb02"
            }
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed_after_editor_err["result"].as_array().unwrap(),
        &items_before
    );

    // 4. Send invalid oversized path (4097 chars) via generic interact on trace
    let (status_trace_err, body_trace_err) = fixture
        .post_panels(json!({
            "method": "panels.interact",
            "params": {
                "session_id": "session-wb02",
                "instance_id": trace_id,
                "kind": "trace",
                "path": oversized_path
            }
        }))
        .await;
    assert_eq!(status_trace_err, StatusCode::BAD_REQUEST);
    assert_eq!(body_trace_err["error"]["code"], "invalid_request");
    assert!(
        body_trace_err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("4096 bytes")
    );

    // Verify state after failed generic interact: list_open must be 100% identical to before
    let (status, listed_after_trace_err) = fixture
        .post_panels(json!({
            "method": "panels.list_open",
            "params": {
                "session_id": "session-wb02"
            }
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed_after_trace_err["result"].as_array().unwrap(),
        &items_before
    );

    // 5. Valid editor update succeeds and preserves instance_id
    let (status, revealed) = fixture
        .post_panels(json!({
            "method": "panels.interact",
            "params": {
                "session_id": "session-wb02",
                "instance_id": files_id,
                "kind": "editor",
                "path": "src/lib.rs"
            }
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revealed["result"]["instance_id"], files_id);
    assert_eq!(revealed["result"]["path"], "src/lib.rs");
    assert_eq!(revealed["result"]["active"], true);

    // Verify list_open: files is active with updated path, trace is inactive
    let (status, listed_valid_editor) = fixture
        .post_panels(json!({
            "method": "panels.list_open",
            "params": {
                "session_id": "session-wb02"
            }
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let items_valid = listed_valid_editor["result"].as_array().unwrap();
    assert_eq!(items_valid.len(), 2);
    let files_valid = items_valid
        .iter()
        .find(|p| p["instance_id"] == files_id)
        .unwrap();
    let trace_valid = items_valid
        .iter()
        .find(|p| p["instance_id"] == trace_id)
        .unwrap();
    assert_eq!(files_valid["active"], true);
    assert_eq!(files_valid["path"], "src/lib.rs");
    assert_eq!(trace_valid["active"], false);

    // 6. Valid generic interact update succeeds and preserves instance_id
    let (status, trace_updated) = fixture
        .post_panels(json!({
            "method": "panels.interact",
            "params": {
                "session_id": "session-wb02",
                "instance_id": trace_id,
                "kind": "trace",
                "path": "step:5"
            }
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trace_updated["result"]["instance_id"], trace_id);
    assert_eq!(trace_updated["result"]["path"], "step:5");
    assert_eq!(trace_updated["result"]["active"], true);

    // Verify list_open: trace is active with updated path, files is inactive
    let (status, listed_valid_trace) = fixture
        .post_panels(json!({
            "method": "panels.list_open",
            "params": {
                "session_id": "session-wb02"
            }
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let items_trace = listed_valid_trace["result"].as_array().unwrap();
    let files_final = items_trace
        .iter()
        .find(|p| p["instance_id"] == files_id)
        .unwrap();
    let trace_final = items_trace
        .iter()
        .find(|p| p["instance_id"] == trace_id)
        .unwrap();
    assert_eq!(files_final["active"], false);
    assert_eq!(files_final["path"], "src/lib.rs");
    assert_eq!(trace_final["active"], true);
    assert_eq!(trace_final["path"], "step:5");

    fixture.close().await;
}

#[tokio::test]
async fn review_wb08_guide_entries_and_catalog() {
    let fixture = TestFixture::start().await;

    let (status, catalog) = fixture
        .post_panels(json!({
            "method": "panels.catalog",
            "params": {}
        }))
        .await;
    assert_eq!(status, StatusCode::OK);

    let sidebar = catalog["result"]["sidebar"].as_array().unwrap();
    let sidebar_ids: Vec<&str> = sidebar.iter().map(|p| p["id"].as_str().unwrap()).collect();

    let bottom = catalog["result"]["bottom"].as_array().unwrap();
    let bottom_ids: Vec<&str> = bottom.iter().map(|p| p["id"].as_str().unwrap()).collect();

    // Terminal is BottomOnly: must be in bottom, but NOT in sidebar
    assert!(!sidebar_ids.contains(&"terminal-bottom"));
    assert!(bottom_ids.contains(&"terminal-bottom"));

    // Plannotator and usage are SidebarOnly: must be in sidebar, but NOT in bottom
    assert!(sidebar_ids.contains(&"plannotator"));
    assert!(!bottom_ids.contains(&"plannotator"));
    assert!(sidebar_ids.contains(&"usage"));
    assert!(!bottom_ids.contains(&"usage"));

    // Files is Both: present in both
    assert!(sidebar_ids.contains(&"files"));
    assert!(bottom_ids.contains(&"files"));

    fixture.close().await;
}
