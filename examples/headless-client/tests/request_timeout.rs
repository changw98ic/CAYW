use cayw_ai::CancellationToken;
use cayw_ipc::{RpcError, RpcHandler, ServerConfig, serve};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, process::Command};

const TOKEN: &str = "request-timeout-test-token";
const WATCHDOG: Duration = Duration::from_secs(10);

struct MockHealthHandler {
    health: Value,
}

#[async_trait::async_trait]
impl RpcHandler for MockHealthHandler {
    async fn call(
        &self,
        method: &str,
        _params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method {
            "runtime.health" => Ok(self.health.clone()),
            "session.create" => Ok(json!({
                "id": "mock-session-001",
                "label": "mock",
                "messages": [],
                "cursor": 0,
                "state": "idle",
                "attempt": null,
                "pending_approvals": [],
                "unresolved_calls": [],
                "executions": [],
                "unresolved_capabilities": [],
                "frozen": false,
                "binding": null,
            })),
            _ => Err(RpcError::new(
                "not_implemented",
                format!("unhandled method {method}"),
            )),
        }
    }
}

async fn spawn_mock_server(
    health: Value,
) -> (
    std::net::SocketAddr,
    CancellationToken,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let mut config = ServerConfig::new(TOKEN);
    config.request_timeout = Duration::from_secs(10);
    let handler = Arc::new(MockHealthHandler { health });
    let task = tokio::spawn(serve(listener, handler, config, cancel.clone()));
    (address, cancel, task)
}

#[tokio::test]
async fn client_rejects_missing_request_timeout_ms() {
    let (address, cancel, task) = spawn_mock_server(json!({
        "status": "ok",
        "protocol_version": 1,
        "model": "test-model",
    }))
    .await;

    let output = tokio::time::timeout(
        WATCHDOG,
        Command::new(env!("CARGO_BIN_EXE_cayw-client"))
            .args([
                "--mode",
                "ipc",
                "--address",
                &address.to_string(),
                "--token-env",
                "TEST_TOKEN",
                "create",
            ])
            .env("TEST_TOKEN", TOKEN)
            .output(),
    )
    .await
    .expect("command must not hang")
    .unwrap();

    cancel.cancel();
    let _ = task.await;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("server health response missing valid integer request_timeout_ms"),
        "unexpected stderr: {stderr}"
    );
}

#[tokio::test]
async fn client_rejects_zero_request_timeout_ms() {
    let (address, cancel, task) = spawn_mock_server(json!({
        "status": "ok",
        "protocol_version": 1,
        "model": "test-model",
        "request_timeout_ms": 0,
    }))
    .await;

    let output = tokio::time::timeout(
        WATCHDOG,
        Command::new(env!("CARGO_BIN_EXE_cayw-client"))
            .args([
                "--mode",
                "ipc",
                "--address",
                &address.to_string(),
                "--token-env",
                "TEST_TOKEN",
                "create",
            ])
            .env("TEST_TOKEN", TOKEN)
            .output(),
    )
    .await
    .expect("command must not hang")
    .unwrap();

    cancel.cancel();
    let _ = task.await;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("server request_timeout_ms must be non-zero"),
        "unexpected stderr: {stderr}"
    );
}

#[tokio::test]
async fn client_rejects_non_integer_request_timeout_ms() {
    let (address, cancel, task) = spawn_mock_server(json!({
        "status": "ok",
        "protocol_version": 1,
        "model": "test-model",
        "request_timeout_ms": "not_an_integer",
    }))
    .await;

    let output = tokio::time::timeout(
        WATCHDOG,
        Command::new(env!("CARGO_BIN_EXE_cayw-client"))
            .args([
                "--mode",
                "ipc",
                "--address",
                &address.to_string(),
                "--token-env",
                "TEST_TOKEN",
                "create",
            ])
            .env("TEST_TOKEN", TOKEN)
            .output(),
    )
    .await
    .expect("command must not hang")
    .unwrap();

    cancel.cancel();
    let _ = task.await;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("server health response missing valid integer request_timeout_ms"),
        "unexpected stderr: {stderr}"
    );
}

#[tokio::test]
async fn client_rejects_negative_request_timeout_ms() {
    let (address, cancel, task) = spawn_mock_server(json!({
        "status": "ok",
        "protocol_version": 1,
        "model": "test-model",
        "request_timeout_ms": -60000,
    }))
    .await;

    let output = tokio::time::timeout(
        WATCHDOG,
        Command::new(env!("CARGO_BIN_EXE_cayw-client"))
            .args([
                "--mode",
                "ipc",
                "--address",
                &address.to_string(),
                "--token-env",
                "TEST_TOKEN",
                "create",
            ])
            .env("TEST_TOKEN", TOKEN)
            .output(),
    )
    .await
    .expect("command must not hang")
    .unwrap();

    cancel.cancel();
    let _ = task.await;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("server health response missing valid integer request_timeout_ms"),
        "unexpected stderr: {stderr}"
    );
}

#[tokio::test]
async fn client_rejects_float_request_timeout_ms() {
    let (address, cancel, task) = spawn_mock_server(json!({
        "status": "ok",
        "protocol_version": 1,
        "model": "test-model",
        "request_timeout_ms": 360000.5,
    }))
    .await;

    let output = tokio::time::timeout(
        WATCHDOG,
        Command::new(env!("CARGO_BIN_EXE_cayw-client"))
            .args([
                "--mode",
                "ipc",
                "--address",
                &address.to_string(),
                "--token-env",
                "TEST_TOKEN",
                "create",
            ])
            .env("TEST_TOKEN", TOKEN)
            .output(),
    )
    .await
    .expect("command must not hang")
    .unwrap();

    cancel.cancel();
    let _ = task.await;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("server health response missing valid integer request_timeout_ms"),
        "unexpected stderr: {stderr}"
    );
}

#[tokio::test]
async fn client_adopts_server_request_timeout_and_executes() {
    // 960,000 ms corresponds to 900s + 60s
    let (address, cancel, task) = spawn_mock_server(json!({
        "status": "ok",
        "protocol_version": 1,
        "model": "test-model",
        "request_timeout_ms": 960_000,
    }))
    .await;

    let output = tokio::time::timeout(
        WATCHDOG,
        Command::new(env!("CARGO_BIN_EXE_cayw-client"))
            .args([
                "--mode",
                "ipc",
                "--address",
                &address.to_string(),
                "--token-env",
                "TEST_TOKEN",
                "create",
            ])
            .env("TEST_TOKEN", TOKEN)
            .output(),
    )
    .await
    .expect("command must not hang")
    .unwrap();

    cancel.cancel();
    let _ = task.await;

    assert!(
        output.status.success(),
        "status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("mock-session-001"),
        "unexpected stdout: {stdout}"
    );
}
