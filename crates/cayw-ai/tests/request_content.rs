use cayw_ai::providers::OpenAiProvider;
use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, Model, ModelRequest,
    Provider, StopReason, StreamOptions, SystemMessage, ToolCall, ToolDefinition,
    ToolResultMessage, timestamp_ms,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn request(provider: &str) -> ModelRequest {
    ModelRequest {
        model: Model::new(provider, "content-fixture-model"),
        messages: vec![Message::user("read the result")],
        options: StreamOptions::default(),
    }
}

fn result() -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: "original-call".into(),
        tool_name: "lookup".into(),
        content: vec![Content::text("ordinary result")],
        details: None,
        structured_content: None,
        usage: None,
        is_error: false,
        outcome_unknown: false,
        timestamp: timestamp_ms(),
        duration_ms: None,
    }
}

fn paired_request(provider: &str, result: ToolResultMessage) -> ModelRequest {
    let mut input = request(provider);
    let mut assistant = AssistantMessage::new(&input.model);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content.push(Content::ToolCall(ToolCall::new(
        "original-call",
        "lookup",
        json!({}),
    )));
    input.messages.push(Message::Assistant(assistant));
    input.messages.push(Message::ToolResult(result));
    input
}

// The listener records a real request if an unsupported input is accidentally
// sent. A ready response keeps the regression deterministic without a timeout.
async fn exchange(mut input: ModelRequest) -> (Vec<AssistantEvent>, Option<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = OpenAiProvider::new(
        format!("http://{}/v1", listener.local_addr().unwrap()),
        "fixture-content-key",
    )
    .unwrap();
    input.options.max_tokens = Some(32);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let end = loop {
            let mut buffer = [0; 2048];
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while bytes.len() < end + length {
            let mut buffer = [0; 2048];
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        let input: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        input
    });
    let events = provider
        .stream(input, CancellationToken::new())
        .collect()
        .await;
    tokio::task::yield_now().await;
    let sent = if server.is_finished() {
        Some(server.await.unwrap())
    } else {
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        None
    };
    (events, sent)
}

#[tokio::test]
async fn disabled_user_images_fail_before_network_instead_of_sending_a_placeholder() {
    for service in ["openai", "magpie"] {
        let mut input = request(service);
        input.messages = vec![Message::User {
            content: vec![Content::Image {
                data: "YWJj".into(),
                mime_type: "image/png".into(),
            }],
            timestamp: timestamp_ms(),
        }];
        let (events, sent) = exchange(input).await;
        assert!(
            matches!(events.last(), Some(AssistantEvent::Error { message })
            if message.stop_reason == StopReason::Error),
            "{service}: disabled image was treated as supported"
        );
        assert!(sent.is_none(), "{service}: unsupported image reached HTTP");
    }
}

#[tokio::test]
async fn tool_images_fail_before_network_even_when_user_images_are_enabled() {
    for service in ["openai", "magpie"] {
        let mut output = result();
        output.content.push(Content::Image {
            data: "YWJj".into(),
            mime_type: "image/png".into(),
        });
        let mut input = paired_request(service, output);
        input.model.supports_images = true;
        let (events, sent) = exchange(input).await;
        assert!(
            matches!(events.last(), Some(AssistantEvent::Error { message })
            if message.stop_reason == StopReason::Error),
            "{service}: tool image was silently replaced"
        );
        assert!(
            sent.is_none(),
            "{service}: unsupported tool image reached HTTP"
        );
    }
}

#[tokio::test]
async fn signed_thinking_history_fails_before_network_instead_of_losing_its_signature() {
    for service in ["openai", "magpie"] {
        let mut input = request(service);
        let mut assistant = AssistantMessage::new(&input.model);
        assistant.stop_reason = StopReason::Stop;
        assistant.content = vec![
            Content::Thinking {
                thinking: "private reasoning".into(),
                signature: Some("signed-opaque-value".into()),
            },
            Content::text("previous answer"),
        ];
        input.messages.push(Message::Assistant(assistant));
        input.messages.push(Message::user("continue"));
        let (events, sent) = exchange(input).await;
        assert!(
            matches!(events.last(), Some(AssistantEvent::Error { message })
            if message.stop_reason == StopReason::Error),
            "{service}: signed history was silently discarded"
        );
        assert!(
            sent.is_none(),
            "{service}: unsupported signed history reached HTTP"
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("signed-opaque-value")
        );
    }
}

#[tokio::test]
async fn enabled_images_require_valid_mime_and_standard_base64_before_network() {
    for service in ["openai", "magpie"] {
        for (mime, data) in [
            ("image/svg+xml", "YWJj"),
            ("text/plain", "YWJj"),
            ("image/png", ""),
            ("image/png", "not base64!"),
            ("image/png", "dt=="),
        ] {
            let mut input = request(service);
            input.model.supports_images = true;
            input.messages = vec![Message::User {
                content: vec![Content::Image {
                    data: data.into(),
                    mime_type: mime.into(),
                }],
                timestamp: timestamp_ms(),
            }];
            let (events, sent) = exchange(input).await;
            assert!(
                matches!(events.last(), Some(AssistantEvent::Error { .. })),
                "{service}/{mime}/{data}: invalid image reached the service"
            );
            assert!(sent.is_none());
        }
    }
}

async fn reasoning_history(service: &str) {
    for tools in [false, true] {
        let mut input = request(service);
        if tools {
            input.messages.insert(
                0,
                Message::System(SystemMessage {
                    tools_added: vec![ToolDefinition {
                        name: "lookup".into(),
                        description: "look up".into(),
                        parameters: json!({"type":"object"}),
                    }],
                    ..Default::default()
                }),
            );
        }
        let mut assistant = AssistantMessage::new(&input.model);
        assistant.stop_reason = StopReason::Stop;
        assistant.content = vec![
            Content::Thinking {
                thinking: "original unsigned reasoning 你好".into(),
                signature: None,
            },
            Content::text("previous answer"),
        ];
        input.messages.push(Message::Assistant(assistant));
        input.messages.push(Message::user("continue"));
        let (events, sent) = exchange(input).await;
        if service == "openai" {
            assert!(
                matches!(events.last(), Some(AssistantEvent::Error { .. })),
                "official Chat Completions has no configured unsigned reasoning replay contract"
            );
            assert!(sent.is_none());
        } else {
            assert!(matches!(events.last(), Some(AssistantEvent::Done { .. })));
            let body = sent.unwrap();
            let history = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "assistant")
                .unwrap();
            assert_eq!(
                history["reasoning_content"],
                "original unsigned reasoning 你好"
            );
            assert_eq!(history["content"], "previous answer");
            assert_eq!(body.get("tools").is_some(), tools);
        }
    }
}

#[tokio::test]
async fn openai_rejects_unsigned_reasoning_history_before_network() {
    reasoning_history("openai").await;
}

#[tokio::test]
async fn magpie_preserves_unsigned_reasoning_history_with_and_without_tools() {
    reasoning_history("magpie").await;
}

// Break caught: failed/aborted replies vanish from the next HTTP request, so
// a new user turn silently reuses incomplete history as if it were complete.
#[tokio::test]
async fn failed_history_is_rejected_before_network_even_after_new_user_input() {
    let mut accepted = Vec::new();
    for service in ["openai", "magpie"] {
        for reason in [StopReason::Error, StopReason::Aborted] {
            let mut input = request(service);
            let mut failed = AssistantMessage::new(&input.model);
            failed
                .content
                .push(Content::text("original interrupted observation"));
            failed = failed.failure(reason, "fixture failure detail");
            input.messages.push(Message::Assistant(failed));
            input
                .messages
                .push(Message::user("a new explicit user turn"));
            let (events, sent) = exchange(input).await;
            let rejected = matches!(events.last(), Some(AssistantEvent::Error { message })
                if message.stop_reason == StopReason::Error);
            eprintln!(
                "{service}/{reason:?}: rejected={rejected}, reached_http={}",
                sent.is_some()
            );
            if !rejected || sent.is_some() {
                accepted.push(format!("{service}/{reason:?}"));
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "Failed history was accepted: {accepted:?}"
    );
}

// Break caught: malformed conversation order or incomplete calls reach HTTP
// even though no valid next model turn exists.
#[tokio::test]
async fn invalid_conversation_order_and_call_stops_fail_before_network() {
    let mut accepted = Vec::new();
    for service in ["openai", "magpie"] {
        let model = Model::new(service, "content-fixture-model");
        let mut answer = AssistantMessage::new(&model);
        answer.stop_reason = StopReason::Stop;
        answer.content.push(Content::text("prior answer"));
        let mut no_call = answer.clone();
        no_call.stop_reason = StopReason::ToolUse;
        let mut pending = answer.clone();
        pending.stop_reason = StopReason::Pending;
        let mut empty = answer.clone();
        empty.content.clear();
        let mut wrong_stop = AssistantMessage::new(&model);
        wrong_stop.stop_reason = StopReason::Stop;
        wrong_stop.content.push(Content::ToolCall(ToolCall::new(
            "original-call",
            "lookup",
            json!({}),
        )));
        let mut truncated = wrong_stop.clone();
        truncated.stop_reason = StopReason::Length;
        let mut bad_arguments = wrong_stop.clone();
        bad_arguments.stop_reason = StopReason::ToolUse;
        if let Content::ToolCall(call) = &mut bad_arguments.content[0] {
            call.arguments_error = Some("incomplete fixture JSON".into());
        }
        let cases = [
            (
                "assistant-first",
                vec![Message::Assistant(answer.clone()), Message::user("next")],
            ),
            (
                "adjacent-assistants",
                vec![
                    Message::user("first"),
                    Message::Assistant(answer.clone()),
                    Message::Assistant(answer.clone()),
                    Message::user("next"),
                ],
            ),
            (
                "assistant-prefill",
                vec![Message::user("first"), Message::Assistant(answer)],
            ),
            (
                "tool-use-without-calls",
                vec![
                    Message::user("first"),
                    Message::Assistant(no_call),
                    Message::user("next"),
                ],
            ),
            (
                "calls-with-stop",
                vec![
                    Message::user("first"),
                    Message::Assistant(wrong_stop),
                    Message::ToolResult(result()),
                ],
            ),
            (
                "calls-with-length",
                vec![
                    Message::user("first"),
                    Message::Assistant(truncated),
                    Message::ToolResult(result()),
                    Message::user("next"),
                ],
            ),
            (
                "invalid-call-arguments",
                vec![
                    Message::user("first"),
                    Message::Assistant(bad_arguments),
                    Message::ToolResult(result()),
                ],
            ),
            (
                "empty-user",
                vec![Message::User {
                    content: vec![],
                    timestamp: timestamp_ms(),
                }],
            ),
            (
                "pending-assistant",
                vec![
                    Message::user("first"),
                    Message::Assistant(pending),
                    Message::user("next"),
                ],
            ),
            (
                "empty-assistant",
                vec![
                    Message::user("first"),
                    Message::Assistant(empty),
                    Message::user("next"),
                ],
            ),
        ];
        for (label, messages) in cases {
            let mut input = request(service);
            input.messages = messages;
            let (events, sent) = exchange(input).await;
            let rejected = matches!(events.last(), Some(AssistantEvent::Error { .. }));
            eprintln!(
                "{service}/{label}: rejected={rejected}, reached_http={}",
                sent.is_some()
            );
            if !rejected || sent.is_some() {
                accepted.push(format!("{service}/{label}"));
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "Invalid histories were accepted: {accepted:?}"
    );
}

#[tokio::test]
async fn structured_tool_results_reach_http_with_the_original_call_id() {
    for service in ["openai", "magpie"] {
        let mut output = result();
        output.structured_content = Some(json!({"answer":42,"nested":{"ok":true},"text":"你好"}));
        let (events, sent) = exchange(paired_request(service, output)).await;
        assert!(matches!(events.last(), Some(AssistantEvent::Done { .. })));
        let sent = sent.expect("supported result must reach HTTP");
        assert_eq!(sent["messages"][2]["tool_call_id"], "original-call");
        let parts = sent["messages"][2]["content"].as_array().unwrap();
        assert_eq!(
            parts.first(),
            Some(&json!({"type":"text","text":"ordinary result"}))
        );
        assert_eq!(parts.len(), 2, "{service}: structured result was lost");
        let actual: Value = serde_json::from_str(parts[1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            actual,
            json!({"answer":42,"nested":{"ok":true},"text":"你好"})
        );
    }
}
