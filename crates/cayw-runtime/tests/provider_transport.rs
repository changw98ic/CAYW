#![cfg(target_os = "macos")]
//! Real configured adapters against local deployment/proxy fixtures only.
use cayw_ai::{CancellationToken, Message, ModelRequest, StopReason, StreamOptions};
use cayw_runtime::ProviderConfig;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
};

const WATCHDOG: Duration = Duration::from_secs(20);
const PROFILES: [(&str, &str, &str); 6] = [
    ("openai", "chat_completions", "v1"),
    ("magpie", "chat_completions", "v1"),
    ("openai", "responses", "v1"),
    ("anthropic", "messages", "2023-06-01"),
    ("gemini", "stream_generate_content", "v1beta"),
    ("gemini", "generate_content", "v1beta"),
];

fn response(api: &str) -> (String, &'static str) {
    let frame = |value: Value| format!("data: {value}\n\n");
    match api {
        "chat_completions" => (
            frame(json!({"id":"proxy-chat","model":"returned","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}))
                + "data: [DONE]\n\n",
            "text/event-stream",
        ),
        "responses" => (
            frame(json!({"type":"response.created","response":{"object":"response","id":"proxy-response","model":"returned","status":"in_progress","output":[]}}))
                + &frame(json!({"type":"response.output_item.added","output_index":0,"item":{"id":"message-1","type":"message","role":"assistant","status":"in_progress","content":[]}}))
                + &frame(json!({"type":"response.content_part.added","output_index":0,"item_id":"message-1","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}))
                + &frame(json!({"type":"response.output_text.delta","output_index":0,"item_id":"message-1","content_index":0,"delta":"ok"}))
                + &frame(json!({"type":"response.output_text.done","output_index":0,"item_id":"message-1","content_index":0,"text":"ok"}))
                + &frame(json!({"type":"response.content_part.done","output_index":0,"item_id":"message-1","content_index":0,"part":{"type":"output_text","text":"ok","annotations":[]}}))
                + &frame(json!({"type":"response.output_item.done","output_index":0,"item":{"id":"message-1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]}}))
                + &frame(json!({"type":"response.completed","response":{"object":"response","id":"proxy-response","model":"returned","status":"completed","output":[{"type":"message","id":"message-1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ok","annotations":[]}]}]}})),
            "text/event-stream",
        ),
        "messages" => (
            frame(json!({"type":"message_start","message":{"id":"proxy-message","type":"message","role":"assistant","model":"returned","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}))
                + &frame(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}))
                + &frame(json!({"type":"content_block_stop","index":0}))
                + &frame(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}))
                + &frame(json!({"type":"message_stop"})),
            "text/event-stream",
        ),
        "stream_generate_content" => (
            frame(json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]})),
            "text/event-stream",
        ),
        "generate_content" => (
            json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}).to_string(),
            "application/json",
        ),
        _ => unreachable!(),
    }
}

// Separate process: changing proxy variables cannot affect parallel tests.
#[tokio::test]
#[ignore = "subprocess helper invoked explicitly by the parent tests"]
async fn configured_transport_child() {
    let profile: usize = std::env::var("CAYW_TRANSPORT_PROFILE")
        .unwrap()
        .parse()
        .unwrap();
    let status: u16 = std::env::var("CAYW_TRANSPORT_STATUS")
        .unwrap()
        .parse()
        .unwrap();
    let base = std::env::var("CAYW_TRANSPORT_BASE").unwrap();
    let (service, api, version) = PROFILES[profile];
    let config: ProviderConfig = serde_json::from_value(json!({
        "service":service,"api":api,"id":"fixture-model","base_url":base,
        "api_version":version,"max_output_tokens":32,"api_key_env":"CAYW_SYNTHETIC_KEY"
    }))
    .unwrap();
    let configured = config
        .build_with_environment(|name| {
            assert_eq!(name, "CAYW_SYNTHETIC_KEY");
            Some("synthetic-transport-key".into())
        })
        .unwrap();
    let events = tokio::time::timeout(
        WATCHDOG,
        configured
            .provider
            .stream(
                ModelRequest {
                    model: configured.model,
                    messages: vec![Message::user("local synthetic transport fixture")],
                    options: StreamOptions::default(),
                },
                CancellationToken::new(),
            )
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    assert_eq!(
        events.iter().filter(|event| event.is_terminal()).count(),
        1,
        "{events:?}"
    );
    let message = events.last().unwrap().message();
    if status == 200 {
        assert_eq!(message.stop_reason, StopReason::Stop, "{events:?}");
    } else {
        assert_eq!(message.stop_reason, StopReason::Error, "{events:?}");
        assert!(
            message
                .error_message
                .as_ref()
                .unwrap()
                .contains(&status.to_string())
        );
    }
    assert!(!format!("{events:?}").contains("synthetic-transport-key"));
}

async fn read(socket: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        assert!(bytes.len() < 65536);
    };
    let header = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let length = header
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    while bytes.len() < header_end + length {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    (
        header,
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
    )
}

async fn check(profile: usize, status: u16, bypass: bool) {
    check_at(profile, status, bypass, "/deploy/team/v1").await;
}

async fn check_at(profile: usize, status: u16, bypass: bool, prefix: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = if bypass {
        format!("http://localhost:{}{prefix}", address.port())
    } else {
        format!("http://127.0.0.1:9{prefix}")
    };
    let proxy = if bypass {
        "http://127.0.0.1:9".into()
    } else {
        format!("http://{address}")
    };
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "configured_transport_child",
            "--nocapture",
        ])
        .env_clear()
        .env("CAYW_TRANSPORT_PROFILE", profile.to_string())
        .env("CAYW_TRANSPORT_STATUS", status.to_string())
        .env("CAYW_TRANSPORT_BASE", &base)
        .env("HTTP_PROXY", &proxy)
        .env("http_proxy", &proxy)
        .env("NO_PROXY", if bypass { "localhost" } else { "" })
        .env("no_proxy", if bypass { "localhost" } else { "" })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();
    let output = child.wait_with_output();
    tokio::pin!(output);
    let (body, content_type) = response(PROFILES[profile].1);
    let mut received = Vec::new();
    let result = tokio::time::timeout(WATCHDOG, async {
        loop {
            tokio::select! {
                result = &mut output => break result.unwrap(),
                connection = listener.accept() => {
                    let (mut socket, _) = connection.unwrap();
                    received.push(read(&mut socket).await);
                    let redirect = format!("Location: http://{address}/must-not-follow\r\n");
                    let header = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n", body.len(), if status == 307 { &redirect } else { "" });
                    socket.write_all(header.as_bytes()).await.unwrap();
                    socket.write_all(body.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                }
            }
        }
    }).await.expect("transport child did not finish");
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(received.len(), 1, "must neither retry nor follow redirects");
    let (header, value) = &received[0];
    let api = PROFILES[profile].1;
    let suffix = match api {
        "chat_completions" => "chat/completions",
        "responses" => "responses",
        "messages" => "messages",
        "stream_generate_content" => "models/fixture-model:streamGenerateContent?alt=sse",
        "generate_content" => "models/fixture-model:generateContent",
        _ => unreachable!(),
    };
    let target = if bypass { prefix.to_owned() } else { base };
    let target = target.trim_end_matches('/');
    assert_eq!(
        header.lines().next().unwrap(),
        format!("POST {target}/{suffix} HTTP/1.1")
    );
    if api.starts_with("stream_") || api == "generate_content" {
        assert!(
            header
                .to_ascii_lowercase()
                .contains("x-goog-api-key: synthetic-transport-key")
        );
    } else if api == "messages" {
        assert!(
            header
                .to_ascii_lowercase()
                .contains("anthropic-version: 2023-06-01")
        );
    } else {
        assert_eq!(value["model"], "fixture-model");
    }
}

#[tokio::test]
async fn configured_adapters_keep_deployment_prefix_and_use_environment_proxy() {
    for profile in 0..PROFILES.len() {
        check(profile, 200, false).await;
    }
}

#[tokio::test]
async fn configured_adapters_keep_encoded_and_root_deployment_paths() {
    for profile in 0..PROFILES.len() {
        for prefix in ["/", "/deploy/tenant%2Falpha/v1/"] {
            check_at(profile, 200, true, prefix).await;
        }
    }
}

#[tokio::test]
async fn configured_adapters_honor_no_proxy_with_localhost_deployment() {
    for profile in 0..PROFILES.len() {
        check(profile, 200, true).await;
    }
}

#[tokio::test]
async fn proxied_adapters_never_follow_redirects_or_retry_service_errors() {
    for profile in 0..PROFILES.len() {
        for status in [307, 503] {
            check(profile, status, false).await;
        }
    }
}
