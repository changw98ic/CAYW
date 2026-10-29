use cayw_ipc::Client;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

const TOKEN: &str = "cli-serve-timeout-test-token-123456";

#[tokio::test]
async fn default_serve_advertises_360s_timeout_and_accepts_requests() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cayw"))
        .arg("serve")
        .arg("--db")
        .arg(dir.path().join("session.db"))
        .arg("--workspace")
        .arg(dir.path().join("workspace"))
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--token-env")
        .arg("TEST_TOKEN")
        .arg("--offline")
        .env("TEST_TOKEN", TOKEN)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut stdout_lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let banner_line = tokio::time::timeout(Duration::from_secs(10), stdout_lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let banner: Value = serde_json::from_str(&banner_line).unwrap();
    let address = banner["listening"].as_str().unwrap();

    let client = Client::connect(address.parse().unwrap(), TOKEN)
        .await
        .unwrap();
    let health = client.request("runtime.health", json!({})).await.unwrap();

    // Default max_run_time is 300s, so request_timeout = 300s + 60s = 360s = 360_000 ms.
    assert_eq!(health["request_timeout_ms"], 360_000);

    // Ensure ordinary RPC methods work through CliRpcHandler
    let session = client
        .request("session.create", json!({"label": "timeout-test"}))
        .await
        .unwrap();
    assert_eq!(session["state"], "idle");

    client.close().await;
    child.kill().await.unwrap();
}

#[tokio::test]
async fn config_900s_serve_advertises_960s_timeout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&json!({
            "budgets": {
                "max_run_time_ms": 900_000
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cayw"))
        .arg("serve")
        .arg("--db")
        .arg(dir.path().join("session.db"))
        .arg("--workspace")
        .arg(dir.path().join("workspace"))
        .arg("--config")
        .arg(&config_path)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--token-env")
        .arg("TEST_TOKEN")
        .arg("--offline")
        .env("TEST_TOKEN", TOKEN)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut stdout_lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let banner_line = tokio::time::timeout(Duration::from_secs(10), stdout_lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let banner: Value = serde_json::from_str(&banner_line).unwrap();
    let address = banner["listening"].as_str().unwrap();

    let client = Client::connect(address.parse().unwrap(), TOKEN)
        .await
        .unwrap();
    let health = client.request("runtime.health", json!({})).await.unwrap();

    // 900s task max_run_time -> request_timeout = 900s + 60s = 960s = 960_000 ms.
    assert_eq!(health["request_timeout_ms"], 960_000);

    let session = client
        .request("session.create", json!({"label": "timeout-test-900"}))
        .await
        .unwrap();
    assert_eq!(session["state"], "idle");

    client.close().await;
    child.kill().await.unwrap();
}
