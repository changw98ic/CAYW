use cayw_ai::providers::OpenAiProvider;
use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, Model, ModelRequest,
    Provider, StopReason, StreamOptions,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::timeout;

const MIB: usize = 1024 * 1024;
const GUARD: Duration = Duration::from_secs(30);

async fn read_request(socket: &mut TcpStream) {
    let mut bytes = Vec::new();
    let (end, length) = loop {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).await.unwrap();
        assert_ne!(size, 0, "HTTP request closed before headers");
        bytes.extend_from_slice(&buffer[..size]);
        assert!(bytes.len() <= 64 * 1024, "unexpected fixture request size");
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let end = end + 4;
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (end, length);
        }
    };
    while bytes.len() < end + length {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).await.unwrap();
        assert_ne!(size, 0, "HTTP request closed before body");
        bytes.extend_from_slice(&buffer[..size]);
    }
    serde_json::from_slice::<Value>(&bytes[end..end + length]).unwrap();
}

async fn fixture(body: String, fragment: usize) -> (OpenAiProvider, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = OpenAiProvider::new(
        format!("http://{}/v1", listener.local_addr().unwrap()),
        "fixture-key",
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        for bytes in body.as_bytes().chunks(fragment.max(1)) {
            if socket.write_all(bytes).await.is_err() {
                break;
            }
        }
    });
    (provider, server)
}

fn request(provider: &str) -> ModelRequest {
    ModelRequest {
        model: Model::new(provider, "fixture-model"),
        messages: vec![Message::user("hello")],
        options: StreamOptions::default(),
    }
}

fn frame(delta: Value, finish: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}

fn stop(reason: &str) -> String {
    frame(json!({}), json!(reason)) + "data: [DONE]\n\n"
}

async fn terminal(kind: &str, body: String, fragment: usize) -> (AssistantMessage, usize) {
    let (provider, server) = fixture(body, fragment).await;
    let mut stream = provider.stream(request(kind), CancellationToken::new());
    let mut result = None;
    let mut content_events = 0;
    while let Some(event) = timeout(GUARD, stream.next())
        .await
        .expect("HTTP fixture stalled")
    {
        if matches!(
            event,
            AssistantEvent::TextStart { .. }
                | AssistantEvent::TextDelta { .. }
                | AssistantEvent::ThinkingStart { .. }
                | AssistantEvent::ThinkingDelta { .. }
                | AssistantEvent::ToolCallStart { .. }
                | AssistantEvent::ToolCallDelta { .. }
                | AssistantEvent::ToolCallEnd { .. }
        ) {
            content_events += 1;
        }
        if event.is_terminal() {
            assert!(result.is_none(), "more than one terminal event");
            result = Some(event.message().clone());
        }
    }
    timeout(GUARD, server).await.unwrap().unwrap();
    (result.expect("missing terminal event"), content_events)
}

async fn limited(kind: &str, case: &str, body: String, error: &str) -> (AssistantMessage, usize) {
    let (message, count) = terminal(kind, body, 64 * 1024).await;
    println!(
        "{kind}/{case}: {:?}: {:?}; content_events={count}",
        message.stop_reason, message.error_message
    );
    assert_eq!(
        message.stop_reason,
        StopReason::Error,
        "{kind}/{case}: wrong terminal"
    );
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|actual| actual.contains(error)),
        "{kind}/{case}: expected {error:?}, got {:?}",
        message.error_message
    );
    (message, count)
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_wire_object_fields_are_rejected_instead_of_overwritten() {
    for kind in ["openai", "magpie"] {
        for json in [
            r#"{"choices":[],"choices":[{"index":0,"delta":{"content":"overwritten"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"first","content":"overwritten"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":1,"prompt_tokens":2}}"#,
        ] {
            let body = format!("data: {json}\n\ndata: [DONE]\n\n");
            let (_, events) = limited(kind, "duplicate-key", body, "duplicate").await;
            assert_eq!(events, 0, "ambiguous frame emitted normalized content");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn response_and_frame_counts_accumulate_even_without_content() {
    for kind in ["openai", "magpie"] {
        let heartbeat = format!(": {}\n\n", "x".repeat(1024 * 1024 - 4));
        let body = heartbeat.repeat(65) + &stop("stop");
        limited(
            kind,
            "response-bytes",
            body,
            "response exceeds the 64 MiB limit",
        )
        .await;
        let body = frame(json!({}), Value::Null).repeat(8193) + &stop("stop");
        limited(kind, "frame-count", body, "frame limit").await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn individual_sse_events_remain_bounded() {
    for kind in ["openai", "magpie"] {
        limited(
            kind,
            "event-bytes",
            format!("data: {}\n\n", "x".repeat(8 * MIB)),
            "8 MiB",
        )
        .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn fragments_and_tool_calls_have_cumulative_and_single_frame_limits() {
    for kind in ["openai", "magpie"] {
        let body = frame(json!({"content":""}), Value::Null).repeat(4097) + &stop("stop");
        limited(kind, "empty-fragments", body, "content fragment limit").await;
        let calls = (0..129).map(|index| json!({"index":index,"id":format!("c{index}"),"type":"function","function":{"name":"sum","arguments":"{}"}})).collect::<Vec<_>>();
        let body = frame(json!({"tool_calls":calls}), Value::Null) + &stop("tool_calls");
        let (message, events) = limited(kind, "single-frame-calls", body, "tool call limit").await;
        assert!(message.content.is_empty());
        assert_eq!(
            events, 0,
            "oversized frame generated snapshots before rejection"
        );
        let calls = (0..129).map(|index| frame(json!({"tool_calls":[{"index":index,"id":format!("c{index}"),"function":{"name":"sum","arguments":"{}"}}]}), Value::Null)).collect::<String>();
        let (message, _) = limited(
            kind,
            "cumulative-calls",
            calls + &stop("tool_calls"),
            "tool call limit",
        )
        .await;
        assert_eq!(message.tool_calls().len(), 128);
        let body = frame(
            json!({"tool_calls":vec![json!({"index":1}); 4097]}),
            Value::Null,
        ) + &stop("stop");
        let (message, events) =
            limited(kind, "deferred-fragments", body, "content fragment limit").await;
        assert!(message.content.is_empty());
        assert_eq!(events, 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn retained_content_and_full_message_snapshot_work_are_bounded() {
    for kind in ["openai", "magpie"] {
        let content = "x".repeat(MIB);
        let body = frame(json!({"content":content}), Value::Null).repeat(9) + &stop("stop");
        limited(
            kind,
            "content-bytes",
            body,
            "content exceeds the 8 MiB limit",
        )
        .await;
        // A large retained message makes each subsequent small delta expensive.
        // Do not hide that cost behind only small fixture inputs.
        let body = frame(json!({"content":"x".repeat(MIB)}), Value::Null)
            + &frame(json!({"content":"y"}), Value::Null).repeat(100)
            + &stop("stop");
        let (message, events) = limited(kind, "snapshot-work", body, "snapshot work limit").await;
        assert!(cayw_ai::content_text(&message.content).len() < MIB + 100);
        assert!(events < 100);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn same_thread_cancel_interrupts_one_ready_frame_before_the_remaining_tool_events() {
    for kind in ["openai", "magpie"] {
        let calls = (0..128).map(|index| json!({"index":index,"id":format!("c{index}"),"function":{"name":"sum","arguments":"{}"}})).collect::<Vec<_>>();
        let body = frame(json!({"tool_calls":calls}), json!("tool_calls")) + "data: [DONE]\n\n";
        let (provider, server) = fixture(body, usize::MAX).await;
        let cancel = CancellationToken::new();
        let (signal, ready) = oneshot::channel();
        let mut signal = Some(signal);
        let canceller = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                ready.await.unwrap();
                cancel.cancel();
            })
        };
        let mut stream = provider.stream(request(kind), cancel);
        let mut terminals = 0;
        let mut tool_events = 0;
        while let Some(event) = timeout(GUARD, stream.next()).await.unwrap() {
            if matches!(event, AssistantEvent::ToolCallStart { .. }) {
                tool_events += 1;
                if let Some(signal) = signal.take() {
                    signal.send(()).unwrap();
                }
            }
            if event.is_terminal() {
                terminals += 1;
                println!(
                    "{kind}/same-thread: {:?}; tool_events={tool_events}; retained_calls={}",
                    event.message().stop_reason,
                    event.message().tool_calls().len()
                );
                assert_eq!(
                    event.message().stop_reason,
                    StopReason::Aborted,
                    "ready frame completed synchronously before the signalled cancellation task ran"
                );
                assert_eq!(
                    event.message().error_message.as_deref(),
                    Some("Request cancelled")
                );
                assert_eq!(
                    event.message().tool_calls().len(),
                    1,
                    "accept consumed the rest of the frame before its first observable tool event"
                );
            }
        }
        assert_eq!(terminals, 1);
        assert_eq!(tool_events, 1);
        canceller.await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn same_thread_cancel_interrupts_ready_frames_that_emit_no_content_events() {
    for kind in ["openai", "magpie"] {
        let usage = format!(
            "data: {}\n\n",
            json!({"choices":[],"usage":{"prompt_tokens":1}})
        );
        let body = usage + &frame(json!({}), Value::Null).repeat(32) + &stop("stop");
        let (provider, server) = fixture(body, usize::MAX).await;
        let cancel = CancellationToken::new();
        let (signal, ready) = oneshot::channel();
        let canceller = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                ready.await.unwrap();
                cancel.cancel();
            })
        };
        let mut stream = provider.stream(request(kind), cancel);
        loop {
            let event = timeout(GUARD, stream.next()).await.unwrap().unwrap();
            assert!(!event.is_terminal(), "missing initial usage update");
            if matches!(event, AssistantEvent::UsageUpdate { .. }) {
                break;
            }
        }
        signal.send(()).unwrap();
        let event = timeout(GUARD, stream.next()).await.unwrap().unwrap();
        println!(
            "{kind}/same-thread-empty-frames: {:?}: {:?}",
            event.message().stop_reason,
            event.message().error_message
        );
        assert!(
            matches!(event, AssistantEvent::Error { .. }),
            "ready empty frames ignored scheduled cancellation"
        );
        assert_eq!(event.message().stop_reason, StopReason::Aborted);
        assert_eq!(
            event.message().error_message.as_deref(),
            Some("Request cancelled")
        );
        assert_eq!(event.message().usage.input, 1);
        assert!(event.message().content.is_empty());
        assert!(stream.next().await.is_none());
        canceller.await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn network_json_tree_limits_reject_unused_structure_before_normalizing_content() {
    for kind in ["openai", "magpie"] {
        for (metadata, reason) in [
            (format!("[{}null]", "null,".repeat(65_536)), "node limit"),
            (
                format!("{}null{}", "[".repeat(129), "]".repeat(129)),
                "depth limit",
            ),
            ("1".repeat(4097), "scalar token limit"),
        ] {
            let body = format!(
                "data: {{\"metadata\":{metadata},\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"unreachable\"}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            let (message, events) = limited(kind, "json-structure", body, reason).await;
            assert!(message.content.is_empty());
            assert_eq!(events, 0);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn protocol_end_stops_before_invalid_or_oversized_tail_bytes() {
    for kind in ["openai", "magpie"] {
        for tail in [vec![0xff, b'\n', b'\n'], vec![b'x'; 8 * MIB + 1]] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let provider = OpenAiProvider::new(
                format!("http://{}/v1", listener.local_addr().unwrap()),
                "fixture-key",
            )
            .unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                let body = [stop("stop").into_bytes(), tail].concat();
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                let _ = socket.write_all(&body).await;
            });
            let mut stream = provider.stream(request(kind), CancellationToken::new());
            let mut terminals = 0;
            while let Some(event) = timeout(GUARD, stream.next()).await.unwrap() {
                if event.is_terminal() {
                    terminals += 1;
                    assert!(
                        matches!(event, AssistantEvent::Done { .. }),
                        "tail changed terminal: {:?}",
                        event.message().error_message
                    );
                }
            }
            assert_eq!(terminals, 1);
            timeout(GUARD, server).await.unwrap().unwrap();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn strict_network_parsing_preserves_unicode_and_incomplete_length_arguments() {
    for kind in ["openai", "magpie"] {
        let body = frame(
            json!({"content":"你好 🦀","reasoning_content":"思考"}),
            Value::Null,
        ) + &frame(
            json!({"tool_calls":[{"index":0,"id":"c0","function":{"name":"sum","arguments":"{\"城市\":\"你好 🦀\"}"}}]}),
            Value::Null,
        ) + &stop("tool_calls");
        let (message, _) = terminal(kind, body, 1).await;
        assert_eq!(
            message.stop_reason,
            StopReason::ToolUse,
            "{:?}",
            message.error_message
        );
        assert_eq!(message.content[0], Content::text("你好 🦀"));
        assert_eq!(message.tool_calls()[0].arguments, json!({"城市":"你好 🦀"}));
        let body = frame(
            json!({"tool_calls":[{"index":0,"id":"c0","function":{"name":"sum","arguments":"{\"a\":"}}]}),
            Value::Null,
        ) + &stop("length");
        let (message, _) = terminal(kind, body, 1).await;
        assert_eq!(message.stop_reason, StopReason::Length);
        assert!(message.tool_calls()[0].arguments_error.is_some());
        let body = frame(
            json!({"tool_calls":[{"index":0,"id":"c0","function":{"name":"sum","arguments":"{\"a\":1,\"a\":2}"}}]}),
            Value::Null,
        ) + &stop("tool_calls");
        let (message, _) = terminal(kind, body, 4096).await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(
            message
                .error_message
                .as_deref()
                .is_some_and(|error| error.contains("duplicate"))
        );
    }
}
