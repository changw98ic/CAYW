use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

async fn exchange(request: Value) -> Value {
    let mut child = Command::new(Path::new(env!("CARGO_BIN_EXE_cayw-bridge-local-fixture")))
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut bytes = serde_json::to_vec(&request).unwrap();
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.unwrap();
    stdin.flush().await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    drop(stdin);
    child.wait().await.unwrap();
    serde_json::from_str(&line).unwrap()
}

fn meta(version: &str) -> Value {
    json!({"io.modelcontextprotocol/protocolVersion":version,
        "io.modelcontextprotocol/clientCapabilities":{},
        "io.modelcontextprotocol/clientInfo":{"name":"protocol-test","version":"1"}})
}

#[tokio::test]
async fn versioned_tool_discovery_works_without_initialization() {
    let response = exchange(json!({"jsonrpc":"2.0","id":1,"method":"tools/list",
        "params":{"_meta":meta("2026-07-28")}}))
    .await;
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["resultType"], "complete");
    assert_eq!(response["result"]["tools"][0]["name"], "add");
    assert_eq!(
        response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "cayw-local-fixture"
    );
}

#[tokio::test]
async fn missing_request_metadata_is_invalid_parameters() {
    let response =
        exchange(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})).await;
    assert_eq!(response["error"]["code"], -32602, "{response}");
}

#[tokio::test]
async fn unsupported_version_reports_requested_and_supported_versions() {
    let response = exchange(json!({"jsonrpc":"2.0","id":3,"method":"tools/list",
        "params":{"_meta":meta("1900-01-01")}}))
    .await;
    assert_eq!(response["error"]["code"], -32022, "{response}");
    assert_eq!(response["error"]["data"]["requested"], "1900-01-01");
    assert_eq!(
        response["error"]["data"]["supported"],
        json!(["2026-07-28"])
    );
}

#[tokio::test]
async fn removed_initialize_method_is_not_a_second_protocol_path() {
    let response = exchange(json!({"jsonrpc":"2.0","id":4,"method":"initialize",
        "params":{"_meta":meta("2026-07-28"),"protocolVersion":"2025-11-25","capabilities":{}}}))
    .await;
    assert_eq!(response["error"]["code"], -32601, "{response}");
}
