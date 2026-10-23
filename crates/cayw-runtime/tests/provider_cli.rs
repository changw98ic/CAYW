#![cfg(target_os = "macos")]
use cayw_ipc::Client;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

const TOKEN: &str = "synthetic-config-selection-ipc-token";
const KEY: &str = "synthetic-config-selection-key";
const WATCHDOG: Duration = Duration::from_secs(20);

fn config(service: &str, api: &str, version: &str, base: &str) -> Value {
    json!({"model": {
        "service": service, "api": api, "id": "fixture-model",
        "base_url": base, "api_version": version,
        "max_output_tokens": 64, "api_key_env": "CAYW_SELECTION_TEST_KEY"
    }})
}

fn command(root: &Path, input: &Value) -> tokio::process::Command {
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    std::fs::write(root.join("config.json"), serde_json::to_vec(input).unwrap()).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_cayw"));
    command
        .env_clear()
        .env("CAYW_SELECTION_TEST_TOKEN", TOKEN)
        .env("CAYW_SELECTION_TEST_KEY", KEY)
        .args(["serve", "--db"])
        .arg(root.join("session.db"))
        .arg("--workspace")
        .arg(root.join("workspace"))
        .args([
            "--listen",
            "127.0.0.1:0",
            "--token-env",
            "CAYW_SELECTION_TEST_TOKEN",
            "--config",
        ])
        .arg(root.join("config.json"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn read_request(socket: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < end + length {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    (
        head,
        serde_json::from_slice(&bytes[end..end + length]).unwrap(),
    )
}

fn response(_service: &str, api: &str) -> (&'static str, String) {
    if api == "messages" {
        let frames = [
            json!({"type":"message_start","message":{"type":"message","role":"assistant","id":"response-1","model":"fixture-model","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"configured response"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ];
        return (
            "text/event-stream",
            frames
                .iter()
                .map(|value| format!("data: {value}\n\n"))
                .collect(),
        );
    }
    if matches!(api, "generate_content" | "stream_generate_content") {
        let body = json!({"responseId":"response-1","modelVersion":"fixture-model","candidates":[{"content":{"role":"model","parts":[{"text":"configured response"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":2,"totalTokenCount":4}});
        return if api == "generate_content" {
            ("application/json", body.to_string())
        } else {
            ("text/event-stream", format!("data: {body}\n\n"))
        };
    }
    if api == "responses" {
        let item = json!({"id":"msg-configured","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"configured response","annotations":[],"logprobs":[]}]});
        let frames = [
            json!({"type":"response.created","response":{"id":"response-1","object":"response","model":"fixture-model","status":"in_progress","output":[],"usage":null}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg-configured","type":"message","role":"assistant","status":"in_progress","content":[]}}),
            json!({"type":"response.content_part.added","output_index":0,"item_id":"msg-configured","content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg-configured","content_index":0,"delta":"configured response"}),
            json!({"type":"response.output_text.done","output_index":0,"item_id":"msg-configured","content_index":0,"text":"configured response"}),
            json!({"type":"response.content_part.done","output_index":0,"item_id":"msg-configured","content_index":0,"part":item["content"][0]}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":"response-1","model":"fixture-model","status":"completed","output":[item],"usage":{"input_tokens":2,"output_tokens":2,"total_tokens":4},"error":null,"incomplete_details":null}}),
        ];
        return (
            "text/event-stream",
            frames
                .iter()
                .map(|value| {
                    format!(
                        "event: {}\ndata: {value}\n\n",
                        value["type"].as_str().unwrap()
                    )
                })
                .collect(),
        );
    }
    let body = json!({"id":"response-1","model":"fixture-model","choices":[{"index":0,"delta":{"content":"configured response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":2,"total_tokens":4}});
    (
        "text/event-stream",
        format!("data: {body}\n\ndata: [DONE]\n\n"),
    )
}

// Break caught: the selected service/API is ignored, a different key is loaded,
// or startup dispatches a model request before a user prompt.
#[tokio::test]
async fn cli_selects_each_protocol_and_only_the_configured_fake_credential() {
    tokio::time::timeout(WATCHDOG, async {
        for (service, api, version, prefix, endpoint, auth) in [
            ("openai", "chat_completions", "v1", "v1", "/v1/chat/completions", "authorization: Bearer"),
            ("magpie", "chat_completions", "v1", "v1", "/v1/chat/completions", "authorization: Bearer"),
            ("openai", "responses", "v1", "v1", "/v1/responses", "authorization: Bearer"),
            ("magpie", "responses", "v1", "v1", "/v1/responses", "authorization: Bearer"),
            ("magpie", "messages", "2023-06-01", "v1", "/v1/messages", "x-api-key:"),
            ("magpie", "stream_generate_content", "v1beta", "v1beta", "/v1beta/models/fixture-model:streamGenerateContent?alt=sse", "x-goog-api-key:"),
            ("magpie", "generate_content", "v1beta", "v1beta", "/v1beta/models/fixture-model:generateContent", "x-goog-api-key:"),
            ("anthropic", "messages", "2023-06-01", "v1", "/v1/messages", "x-api-key:"),
            ("gemini", "stream_generate_content", "v1beta", "v1beta", "/v1beta/models/fixture-model:streamGenerateContent?alt=sse", "x-goog-api-key:"),
            ("gemini", "generate_content", "v1beta", "v1beta", "/v1beta/models/fixture-model:generateContent", "x-goog-api-key:"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/{prefix}", listener.local_addr().unwrap());
            let root = tempfile::tempdir().unwrap();
            let mut child = command(root.path(), &config(service, api, version, &base)).spawn().unwrap();
            let ready = BufReader::new(child.stdout.take().unwrap()).lines().next_line().await.unwrap().expect("configured server must start");
            let ready: Value = serde_json::from_str(&ready).unwrap();
            assert_eq!(ready["provider"], service);
            assert_eq!(ready["model"], "fixture-model");
            assert_eq!(ready["model_configuration"]["api"], api);
            assert_eq!(ready["model_configuration"]["api_version"], version);
            assert_eq!(ready["model_configuration"]["max_output_tokens"], 64);
            assert!(!ready.to_string().contains(KEY));
            let client = Client::connect(ready["listening"].as_str().unwrap().parse().unwrap(), TOKEN).await.unwrap();
            let session = client.request("session.create", json!({})).await.unwrap();
            // The queued connection probe is checked before the prompt is started.
            let mut accept = Box::pin(listener.accept());
            assert!(futures::poll!(accept.as_mut()).is_pending());
            drop(accept);
            let selected_service = service.to_owned();
            let selected_api = api.to_owned();
            let provider = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let received = read_request(&mut socket).await;
                let (content_type, body) = response(&selected_service, &selected_api);
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                received
            });
            let view = client.request("session.prompt", json!({"session_id":session["id"],"text":"respond with configured response"})).await.unwrap();
            assert_eq!(view["messages"].as_array().unwrap().last().unwrap()["stop_reason"], "stop");
            let (head, body) = provider.await.unwrap();
            assert!(head.starts_with(&format!("POST {endpoint} HTTP/1.1")));
            let auth = format!("{auth} {KEY}");
            assert!(head.lines().any(|line| line.eq_ignore_ascii_case(&auth)));
            if api == "chat_completions" {
                assert_eq!(body["model"], "fixture-model");
                assert_eq!(body["max_completion_tokens"], 64);
            } else if api == "responses" {
                assert_eq!(body["model"], "fixture-model");
                assert_eq!(body["max_output_tokens"], 64);
                assert!(body.get("messages").is_none());
            } else if matches!(api, "generate_content" | "stream_generate_content") {
                assert_eq!(body["generationConfig"]["maxOutputTokens"], 64);
            } else {
                assert_eq!(body["model"], "fixture-model");
                assert_eq!(body["max_tokens"], 64);
                assert!(head.lines().any(|line| line.eq_ignore_ascii_case("anthropic-version: 2023-06-01")));
            }
            client.close().await;
            child.kill().await.unwrap();
            child.wait().await.unwrap();
        }
    }).await.expect("configuration protocol watchdog");
}

// Break caught: removed environment settings still launch a live server.
#[tokio::test]
async fn legacy_environment_cannot_replace_the_model_configuration() {
    let root = tempfile::tempdir().unwrap();
    let mut command = command(root.path(), &json!({}));
    command
        .env("CAYW_PROVIDER_BASE_URL", "http://127.0.0.1:9/v1")
        .env("CAYW_PROVIDER_API_KEY", "synthetic-old-key")
        .env("CAYW_MODEL", "synthetic-old-model");
    let mut child = command.spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = tokio::time::timeout(WATCHDOG, lines.next_line())
        .await
        .unwrap()
        .unwrap();
    if ready.is_some() {
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        panic!("legacy environment unexpectedly launched a model server");
    }
    let output = child.wait_with_output().await.unwrap();
    assert!(!output.status.success());
    assert!(!root.path().join("session.db").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("model configuration"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-old-key"));
}

// Break caught: --offline silently ignores an explicitly selected real service.
#[tokio::test]
async fn offline_and_model_configuration_are_rejected_as_conflicting() {
    let root = tempfile::tempdir().unwrap();
    let input = config("openai", "chat_completions", "v1", "http://127.0.0.1:9/v1");
    let output = tokio::time::timeout(
        WATCHDOG,
        command(root.path(), &input).arg("--offline").output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(!root.path().join("session.db").exists());
}
