//! Gateway HTTP-to-IPC regression test verifying default ownership persistence (F13 / SLICE P).
//!
//! Spawns the WebUI gateway with its default ownership configuration (no CAYW_OWNERSHIP_PATH override),
//! executes `session.create` and `task.enqueue` over HTTP `/api/rpc`, verifies that the mock IPC
//! runtime confirms them, and asserts that a fresh `OwnershipStore` instance re-reading `ownership.json`
//! from the process's working directory restores the expected Session/Task device affinity.

use async_trait::async_trait;
use cayw_ipc::{RpcError, RpcHandler, ServerConfig};
use cayw_webui::OwnershipStore;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, time::Instant};
use tokio_util::sync::CancellationToken;

const IPC_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const WEB_TOKEN: &str = "fedcba9876543210fedcba9876543210";

struct MockRpcHandler;

#[async_trait]
impl RpcHandler for MockRpcHandler {
    async fn call(
        &self,
        method: &str,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method {
            "session.create" => Ok(json!({
                "id": "sess-default-ownership-1",
                "label": params.get("label").and_then(Value::as_str).unwrap_or("test-session"),
                "state": "idle"
            })),
            "task.enqueue" => Ok(json!({
                "id": "task-default-ownership-9",
                "session_id": params.get("session_id").and_then(Value::as_str).unwrap_or(""),
                "state": "queued"
            })),
            _ => Ok(json!({"method": method, "params": params})),
        }
    }
}

struct ProcessGuard(Option<Child>);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_gateway_process(
    temp_dir: &Path,
    ipc_addr: SocketAddr,
) -> Result<(ProcessGuard, SocketAddr), String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cayw-webui"));

    cmd.current_dir(temp_dir)
        .env("CAYW_IPC_ADDR", ipc_addr.to_string())
        .env("CAYW_IPC_TOKEN", IPC_TOKEN)
        .env("CAYW_WEB_TOKEN", WEB_TOKEN)
        .env("CAYW_WEB_ADDR", "127.0.0.1:0")
        .env_remove("CAYW_OWNERSHIP_PATH")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn child: {e}"))?;
    let stdout = child.stdout.take().ok_or("missing child stdout")?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let res = reader
            .read_line(&mut line)
            .map_err(|e| format!("read stdout: {e}"))
            .and_then(|bytes_read| {
                if bytes_read == 0 {
                    return Err("child stdout closed before announcing URL".to_string());
                }
                let prefix = "网页界面：http://";
                let start = line
                    .find(prefix)
                    .ok_or_else(|| format!("unexpected child output: {line:?}"))?
                    + prefix.len();
                let rest = &line[start..];
                let end = rest
                    .find('/')
                    .ok_or_else(|| format!("malformed url: {rest:?}"))?;
                rest[..end]
                    .parse::<SocketAddr>()
                    .map_err(|e| format!("parse address from {rest:?}: {e}"))
            });
        let _ = tx.send(res);
    });

    let addr = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(addr)) => addr,
        Ok(Err(err)) => {
            let _ = child.kill();
            let _ = child.wait();
            let mut stderr_msg = String::new();
            if let Some(mut err_pipe) = child.stderr.take() {
                use std::io::Read;
                let _ = err_pipe.read_to_string(&mut stderr_msg);
            }
            return Err(format!(
                "failed to start gateway: {err}; stderr: {stderr_msg}"
            ));
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("timeout waiting for webui to announce listening address".to_string());
        }
    };

    Ok((ProcessGuard(Some(child)), addr))
}

#[tokio::test]
async fn gateway_http_to_ipc_default_ownership_lifecycle() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

    // 1. Start local mock IPC server
    let ipc_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ipc listener");
    let ipc_addr = ipc_listener.local_addr().expect("ipc local addr");
    let mut ipc_config = ServerConfig::new(IPC_TOKEN);
    ipc_config.request_timeout = Duration::from_secs(20);
    let cancel = CancellationToken::new();
    let ipc_cancel = cancel.child_token();
    let ipc_handle = tokio::spawn(cayw_ipc::serve(
        ipc_listener,
        Arc::new(MockRpcHandler),
        ipc_config,
        ipc_cancel,
    ));

    // 2. Spawn WebUI gateway in temp_dir with default ownership path
    let (child_guard, web_addr) =
        spawn_gateway_process(temp_dir.path(), ipc_addr).expect("spawn gateway process");

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build reqwest client");

    // 3. Wait for WebUI HTTP server to be ready
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("http://{web_addr}/")).send().await
            && resp.status().is_success()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "WebUI gateway HTTP endpoint failed to become ready");

    // 4. Call /api/rpc session.create
    let create_resp = client
        .post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({
            "method": "session.create",
            "params": {
                "label": "default-ownership-session",
                "device_id": "local"
            }
        }))
        .send()
        .await
        .expect("send session.create");
    assert_eq!(create_resp.status(), reqwest::StatusCode::OK);
    let create_body: Value = create_resp.json().await.expect("json session.create");
    let session_id = create_body["result"]["id"]
        .as_str()
        .expect("session id in response");
    assert_eq!(session_id, "sess-default-ownership-1");

    // 5. Call /api/rpc task.enqueue for the created session
    let enqueue_resp = client
        .post(format!("http://{web_addr}/api/rpc"))
        .bearer_auth(WEB_TOKEN)
        .json(&json!({
            "method": "task.enqueue",
            "params": {
                "session_id": session_id
            }
        }))
        .send()
        .await
        .expect("send task.enqueue");
    assert_eq!(enqueue_resp.status(), reqwest::StatusCode::OK);
    let enqueue_body: Value = enqueue_resp.json().await.expect("json task.enqueue");
    let task_id = enqueue_body["result"]["id"]
        .as_str()
        .expect("task id in response");
    assert_eq!(task_id, "task-default-ownership-9");

    // 6. Verify durable ownership.json was created at the process's default path (in temp_dir)
    let ownership_file = temp_dir.path().join("ownership.json");
    assert!(
        ownership_file.is_file(),
        "ownership.json must be persisted in cwd by default"
    );

    // 7. Open a fresh OwnershipStore to read back session and task device affinity
    let reloaded = OwnershipStore::open(&ownership_file).expect("open reloaded ownership store");
    assert_eq!(
        reloaded.session_device(session_id).as_deref(),
        Some("local"),
        "reloaded store must map session to local device"
    );
    assert_eq!(
        reloaded.task_device(task_id).as_deref(),
        Some("local"),
        "reloaded store must map task to local device"
    );
    assert_eq!(reloaded.session_count(), 1);
    assert_eq!(reloaded.task_count(), 1);

    // 8. Cleanup
    drop(child_guard);
    cancel.cancel();
    let _ = ipc_handle.await;
}
