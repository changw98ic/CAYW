//! Real loopback transport checks. Every response and key is synthetic.
use cayw_ai::{
    AssistantEvent, CancellationToken, Content, Message, Model, ModelRequest, Provider, StopReason,
    StreamOptions, providers::OpenAiProvider,
};
use cayw_providers::{AnthropicProvider, GeminiMode, GeminiProvider};
use futures::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const WATCHDOG: Duration = Duration::from_secs(20);
const PROFILES: [&str; 5] = ["openai", "magpie", "anthropic", "gemini-sse", "gemini-json"];

fn adapter(profile: &str, base: &str) -> Arc<dyn Provider> {
    match profile {
        "anthropic" => Arc::new(AnthropicProvider::new(base, "fixture-network-key").unwrap()),
        "gemini-sse" => Arc::new(GeminiProvider::new(base, "fixture-network-key").unwrap()),
        "gemini-json" => Arc::new(
            GeminiProvider::new(base, "fixture-network-key")
                .unwrap()
                .with_mode(GeminiMode::GenerateContent),
        ),
        _ => Arc::new(OpenAiProvider::new(base, "fixture-network-key").unwrap()),
    }
}
fn request(profile: &str) -> ModelRequest {
    ModelRequest {
        model: Model::new(
            if profile.starts_with("gemini") {
                "gemini"
            } else {
                profile
            },
            "network-fixture",
        ),
        messages: vec![Message::user("network fixture")],
        options: StreamOptions {
            max_tokens: Some(32),
            ..Default::default()
        },
    }
}
async fn read_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    while bytes.len() < end + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    let input: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
    assert_eq!(
        input
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("network-fixture"),
        "network-fixture"
    );
    head.lines().next().unwrap().into()
}
fn sse(values: Vec<Value>) -> String {
    values
        .into_iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}
fn anthropic_complete() -> String {
    sse(vec![
        json!({"type":"message_start","message":{"id":"late-test","type":"message","role":"assistant","model":"fixture","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"complete"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ])
}

#[tokio::test]
async fn all_five_profiles_send_one_request_without_retry_or_redirect() {
    tokio::time::timeout(WATCHDOG, async {
        for profile in PROFILES {
            for status in ["302 Found", "429 Too Many Requests", "503 Service Unavailable"] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let provider = adapter(profile, &format!("http://{address}/v1"));
                let response = format!("HTTP/1.1 {status}\r\nLocation: http://{address}/redirected\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let (finished, finish) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(async move {
                    tokio::pin!(finish);
                    let mut requests = Vec::new();
                    loop {
                        tokio::select! { biased;
                            connection = listener.accept() => {
                                let (mut socket, _) = connection.unwrap();
                                requests.push(read_request(&mut socket).await);
                                assert!(requests.len() <= 3, "unexpected repeated requests: {requests:?}");
                                socket.write_all(response.as_bytes()).await.unwrap();
                            }
                            _ = &mut finish => break,
                        }
                    }
                    requests
                });
                let events: Vec<_> = provider.stream(request(profile), CancellationToken::new()).collect().await;
                assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
                assert!(matches!(events.last(), Some(AssistantEvent::Error { message }) if message.stop_reason == StopReason::Error));
                finished.send(()).unwrap();
                let requests = server.await.unwrap();
                assert_eq!(requests.len(), 1, "{profile}/{status}: {requests:?}");
                assert!(!requests[0].contains("redirected"));
                println!("{profile}/{status}: observed exactly one original HTTP request");
            }
        }
    }).await.expect("network lifecycle watchdog");
}

#[tokio::test]
async fn anthropic_terminal_ignores_late_frames_and_invalid_bytes_across_write_sizes() {
    tokio::time::timeout(WATCHDOG, async {
        for write_size in [1, 7, 8192] {
            for bad_utf8 in [false, true] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let provider = adapter("anthropic", &format!("http://{}/v1", listener.local_addr().unwrap()));
                let mut body = anthropic_complete().into_bytes();
                if bad_utf8 { body.extend_from_slice(b"data: \xff\n\n"); }
                else { body.extend_from_slice(b"data: {\"type\":\"ping\"}\n\n"); }
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    read_request(&mut socket).await;
                    let header = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    socket.write_all(header.as_bytes()).await.unwrap();
                    for bytes in body.chunks(write_size) {
                        if socket.write_all(bytes).await.is_err() { break; }
                        tokio::task::yield_now().await;
                    }
                });
                let events: Vec<_> = provider.stream(request("anthropic"), CancellationToken::new()).collect().await;
                server.await.unwrap();
                assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
                let Some(AssistantEvent::Done { message }) = events.last() else {
                    panic!("late bytes changed completed Anthropic response: write_size={write_size}, bad_utf8={bad_utf8}, events={events:?}");
                };
                assert_eq!(message.content, vec![Content::text("complete")]);
            }
        }
    }).await.expect("late response watchdog");
}

#[tokio::test]
async fn truncated_http_bodies_fail_for_every_profile_before_tool_success() {
    tokio::time::timeout(WATCHDOG, async {
        for profile in PROFILES {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let provider = adapter(profile, &format!("http://{}/v1", listener.local_addr().unwrap()));
            let (mime, body) = match profile {
                "anthropic" => ("text/event-stream", sse(vec![
                    json!({"type":"message_start","message":{"id":"truncated","type":"message","role":"assistant","model":"fixture","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}}),
                ])),
                "gemini-sse" => ("text/event-stream", sse(vec![json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]})])),
                "gemini-json" => ("application/json", json!({"candidates":[{"content":{"parts":[{"text":"complete-looking"}]},"finishReason":"STOP"}]}).to_string()),
                _ => ("text/event-stream", sse(vec![json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]})])),
            };
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                let header = format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len() + 16);
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(body.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            });
            let events: Vec<_> = provider.stream(request(profile), CancellationToken::new()).collect().await;
            server.await.unwrap();
            assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
            assert!(matches!(events.last(), Some(AssistantEvent::Error { message }) if message.stop_reason == StopReason::Error), "{profile}: truncation became success");
            assert!(!events.iter().any(|event| matches!(event, AssistantEvent::ToolCallEnd { .. })));
            println!("{profile}: truncated Content-Length failed");
        }
    }).await.expect("truncated HTTP watchdog");
}
