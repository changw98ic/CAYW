use cayw_ai::CancellationToken;
use cayw_ipc::{RpcError, RpcHandler, ServerConfig, serve};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, process::Command, sync::Notify};

const TOKEN: &str = "request-timeout-test-token";
const WATCHDOG: Duration = Duration::from_secs(10);

struct HandlerGate {
    entered: Notify,
    release: Notify,
}

struct MockHealthHandler {
    health: Value,
    gate: Option<Arc<HandlerGate>>,
}

#[async_trait::async_trait]
impl RpcHandler for MockHealthHandler {
    async fn call(
        &self,
        method: &str,
        _params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method {
            "runtime.health" => Ok(self.health.clone()),
            "session.create" => {
                if let Some(gate) = &self.gate {
                    gate.entered.notify_one();
                    tokio::select! {
                        _ = gate.release.notified() => {}
                        _ = cancel.cancelled() => return Err(RpcError::new("cancelled", "request cancelled")),
                    }
                }
                Ok(json!({
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
                }))
            }
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
    spawn_mock_server_with_gate(health, None).await
}

async fn spawn_mock_server_with_gate(
    health: Value,
    gate: Option<Arc<HandlerGate>>,
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
    let handler = Arc::new(MockHealthHandler { health, gate });
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
    assert!(
        stdout.contains("\"request_timeout_ms\":960000")
            || stdout.contains("\"request_timeout_ms\": 960000"),
        "stdout must record adopted request_timeout_ms: {stdout}"
    );
}

#[tokio::test]
async fn client_adopts_server_request_timeout_and_times_out_on_slow_request() {
    let gate = Arc::new(HandlerGate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    // Server advertises 600 ms timeout.
    let (address, cancel, task) = spawn_mock_server_with_gate(
        json!({
            "status": "ok",
            "protocol_version": 1,
            "model": "test-model",
            "request_timeout_ms": 600,
        }),
        Some(gate.clone()),
    )
    .await;

    // Keep the handler pending until the client returns. A fallback to the
    // default 30-second client timeout fails the broad test watchdog.
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

    tokio::time::timeout(WATCHDOG, gate.entered.notified())
        .await
        .expect("request must reach the server handler");
    gate.release.notify_one();
    cancel.cancel();
    let _ = task.await;

    // The server did not release the request, so only the adopted deadline can end it.
    assert!(
        !output.status.success(),
        "client should have failed on 600ms timeout; output: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("request_timeout")
            || stderr.contains("timed out")
            || stderr.contains("deadline"),
        "stderr should indicate timeout: {stderr}"
    );
}

#[tokio::test]
async fn client_adopts_server_request_timeout_and_succeeds_within_timeout() {
    let gate = Arc::new(HandlerGate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    // Server advertises a generous 5000 ms timeout.
    let (address, cancel, task) = spawn_mock_server_with_gate(
        json!({
            "status": "ok",
            "protocol_version": 1,
            "model": "test-model",
            "request_timeout_ms": 5000,
        }),
        Some(gate.clone()),
    )
    .await;

    let gate_task = {
        let gate = gate.clone();
        tokio::spawn(async move {
            gate.entered.notified().await;
            gate.release.notify_one();
        })
    };

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

    let _ = gate_task.await;
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

#[tokio::test]
async fn ipc_client_with_custom_request_timeout_enforces_deadline() {
    use cayw_ipc::{Client, ClientConfig};

    let gate = Arc::new(HandlerGate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let (address, cancel, task) = spawn_mock_server_with_gate(
        json!({
            "status": "ok",
            "protocol_version": 1,
            "model": "test-model",
            "request_timeout_ms": 500,
        }),
        Some(gate.clone()),
    )
    .await;

    let short_config = ClientConfig {
        request_timeout: Duration::from_millis(500),
        ..ClientConfig::default()
    };
    let client = Client::connect_with_config(address, TOKEN, short_config)
        .await
        .unwrap();

    let (err, _) = tokio::join!(client.request("session.create", json!({})), async {
        gate.entered.notified().await;
        // Hold the gate until the client times out
    });
    let err = err.unwrap_err();
    assert_eq!(err.code, "request_timeout");
    assert!(err.outcome_unknown);

    gate.release.notify_one();
    client.close().await;
    cancel.cancel();
    let _ = task.await;
}
