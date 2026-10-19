use cayw_ai::providers::OpenAiProvider;
use cayw_ai::{
    AssistantEvent, CancellationToken, Content, Message, Model, ModelRequest, Provider, StopReason,
    StreamOptions,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

async fn read_request(socket: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 2048];
        let size = socket.read(&mut buffer).await.unwrap();
        assert_ne!(size, 0, "request closed before headers");
        bytes.extend_from_slice(&buffer[..size]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    while bytes.len() < header_end + length {
        let mut buffer = [0; 2048];
        let size = socket.read(&mut buffer).await.unwrap();
        assert_ne!(size, 0, "request closed before body");
        bytes.extend_from_slice(&buffer[..size]);
    }
    (
        headers,
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
    )
}

async fn fixture(
    body: String,
    status: &str,
    fragment_size: usize,
) -> (OpenAiProvider, oneshot::Receiver<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (sender, receiver) = oneshot::channel();
    let status = status.to_owned();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let _ = sender.send(request);
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if socket.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        for part in body.as_bytes().chunks(fragment_size.max(1)) {
            if socket.write_all(part).await.is_err() {
                return;
            }
            tokio::task::yield_now().await;
        }
    });
    (OpenAiProvider::new(url, "fixture-key").unwrap(), receiver)
}

fn request() -> ModelRequest {
    ModelRequest {
        model: Model::new("openai", "fixture-model"),
        messages: vec![Message::user("hello")],
        options: StreamOptions::default(),
    }
}

fn data(value: Value) -> String {
    format!("data: {value}\r\n\r\n")
}
fn chunk(delta: Value, finish: Value) -> Value {
    json!({"id":"response-1","model":"served-model","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
}
fn stopped() -> String {
    data(chunk(json!({}), json!("stop"))) + "data: [DONE]\r\n\r\n"
}

#[tokio::test]
async fn streamed_unicode_text_and_usage_match_the_actual_request() {
    let body = ": heartbeat\r\n\r\n".to_owned()
        + &data(chunk(
            json!({"role":"assistant","content":"你好 🦀"}),
            Value::Null,
        ))
        + &data(chunk(json!({}), json!("stop")))
        + &data(
            json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":5,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":3}}}),
        )
        + "data: [DONE]\r\n\r\n";
    let (provider, received) = fixture(body, "200 OK", 1).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.first(), Some(AssistantEvent::Start { .. })));
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    let Some(AssistantEvent::Done { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!(message.content, vec![Content::text("你好 🦀")]);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("response-1"));
    assert_eq!(message.response_model.as_deref(), Some("served-model"));
    assert_eq!(
        (
            message.usage.input,
            message.usage.output,
            message.usage.cache_read,
            message.usage.cache_write,
            message.usage.reasoning,
            message.usage.total_tokens
        ),
        (13, 7, 5, 2, 3, 27)
    );
    assert!(message.duration_ms.is_some());
    let (headers, body) = received.await.unwrap();
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
    assert!(
        headers
            .to_lowercase()
            .contains("authorization: bearer fixture-key")
    );
    assert_eq!(body["model"], "fixture-model");
    assert_eq!(
        body["messages"],
        json!([{"role":"user","content":[{"type":"text","text":"hello"}]}])
    );
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn fragmented_tool_arguments_are_only_executable_after_complete_json() {
    let body = data(chunk(
        json!({"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"sum","arguments":"{\"a\":"}}]}),
        Value::Null,
    )) + &data(chunk(
        json!({"tool_calls":[{"index":0,"function":{"arguments":"2}"}}]}),
        Value::Null,
    )) + &data(chunk(json!({}), json!("tool_calls")))
        + "data: [DONE]\r\n\r\n";
    let (provider, _) = fixture(body, "200 OK", 3).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Done { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let calls = message.tool_calls();
    assert_eq!(
        (calls[0].id.as_str(), calls[0].name.as_str()),
        ("call-1", "sum")
    );
    assert_eq!(calls[0].arguments, json!({"a":2}));
    assert!(calls[0].arguments_error.is_none());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AssistantEvent::ToolCallStart { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AssistantEvent::ToolCallEnd { .. }))
    );
}

#[tokio::test]
async fn tool_identity_can_arrive_before_its_function_fields() {
    let body = data(chunk(
        json!({"tool_calls":[{"index":0,"id":"call-later","type":"function"}]}),
        Value::Null,
    )) + &data(chunk(
        json!({"tool_calls":[{"index":0,"function":{"name":"sum","arguments":"{}"}}]}),
        Value::Null,
    )) + &data(chunk(json!({}), json!("tool_calls")))
        + "data: [DONE]\n\n";
    let (provider, _) = fixture(body, "200 OK", 17).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Done { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!(message.tool_calls()[0].id, "call-later");
    assert_eq!(message.tool_calls()[0].arguments, json!({}));
}

#[tokio::test]
async fn cancelling_after_a_delta_cannot_be_reported_as_success() {
    let body = data(chunk(json!({"content":"partial"}), Value::Null)) + &stopped();
    let (provider, _) = fixture(body, "200 OK", 8192).await;
    let cancel = CancellationToken::new();
    let mut stream = provider.stream(request(), cancel.clone());
    while let Some(event) = stream.next().await {
        if matches!(event, AssistantEvent::TextDelta { .. }) {
            break;
        }
        assert!(!event.is_terminal());
    }
    cancel.cancel();
    let events = stream.collect::<Vec<_>>().await;
    let Some(AssistantEvent::Error { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!(message.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn tool_calls_keep_source_order_when_deltas_arrive_in_reverse() {
    let body = data(chunk(
        json!({"tool_calls":[{"index":1,"id":"second","type":"function","function":{"name":"sum","arguments":"{\"b\":2}"}}]}),
        Value::Null,
    )) + &data(chunk(
        json!({"tool_calls":[{"index":0,"id":"first","type":"function","function":{"name":"sum","arguments":"{\"a\":1}"}}]}),
        Value::Null,
    )) + &data(chunk(json!({}), json!("tool_calls")))
        + "data: [DONE]\n\n";
    let (provider, _) = fixture(body, "200 OK", 8192).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Done { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!(
        message
            .tool_calls()
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    for event in &events {
        if let AssistantEvent::ToolCallStart { index, partial }
        | AssistantEvent::ToolCallEnd { index, partial } = event
        {
            let Content::ToolCall(call) = &partial.content[*index] else {
                panic!("wrong content index");
            };
            assert_eq!(call.id, message.tool_calls()[*index].id);
        }
    }
}

#[tokio::test]
async fn cache_details_arriving_after_prompt_usage_do_not_inflate_total() {
    let body = data(chunk(json!({"content":"ok"}), Value::Null))
        + &data(json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":7}}))
        + &data(chunk(json!({}), json!("stop")))
        + &data(
            json!({"choices":[],"usage":{"prompt_tokens_details":{"cached_tokens":5,"cache_write_tokens":2}}}),
        )
        + "data: [DONE]\n\n";
    let (provider, _) = fixture(body, "200 OK", 8192).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Done { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!((message.usage.input, message.usage.total_tokens), (13, 27));
}

#[tokio::test]
async fn response_identity_cannot_change_between_chunks() {
    for key in ["id", "model"] {
        let mut changed = chunk(json!({}), json!("stop"));
        changed[key] = json!("different");
        let body = data(chunk(json!({"content":"partial"}), Value::Null))
            + &data(changed)
            + "data: [DONE]\n\n";
        let (provider, _) = fixture(body, "200 OK", 8192).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        let Some(AssistantEvent::Error { message }) = events.last() else {
            panic!("{events:#?}");
        };
        assert!(message.error_message.as_deref().unwrap().contains(key));
    }
}

#[tokio::test]
async fn incomplete_tool_json_is_reported_in_the_call() {
    let body = data(chunk(
        json!({"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"sum","arguments":"{\"a\":"}}]}),
        Value::Null,
    )) + &data(chunk(json!({}), json!("length")))
        + "data: [DONE]\n\n";
    let (provider, _) = fixture(body, "200 OK", 8192).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Done { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!(message.stop_reason, StopReason::Length);
    assert!(message.tool_calls()[0].arguments_error.is_some());
}

#[tokio::test]
async fn malformed_and_incomplete_streams_have_one_error_terminal() {
    let cases = [
        ("data: {bad}\n\n".to_owned(), "JSON"),
        (
            data(chunk(json!({"content":"partial"}), Value::Null)),
            "[DONE]",
        ),
        (
            data(chunk(json!({"content":"partial"}), Value::Null)) + "data: [DONE]\n\n",
            "finish_reason",
        ),
        (data(chunk(json!({}), json!("stop"))), "[DONE]"),
        (
            data(json!({"choices":[{"index":0,"delta":{"refusal":"no"},"finish_reason":"stop"}]}))
                + "data: [DONE]\n\n",
            "refusal",
        ),
        (
            data(chunk(
                json!({"tool_calls":[{"index":0,"function":{"name":"sum","arguments":"{}"}}]}),
                Value::Null,
            )) + &data(chunk(json!({}), json!("tool_calls")))
                + "data: [DONE]\n\n",
            "id",
        ),
    ];
    for (body, expected) in cases {
        let (provider, _) = fixture(body, "200 OK", 11).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
        let Some(AssistantEvent::Error { message }) = events.last() else {
            panic!("{events:#?}");
        };
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(
            message.error_message.as_deref().unwrap().contains(expected),
            "{message:#?}"
        );
    }
}

#[tokio::test]
async fn http_failure_is_an_error_without_retry() {
    let (provider, received) = fixture(
        "{\"error\":{\"message\":\"rate limited\"}}".into(),
        "429 Too Many Requests",
        1,
    )
    .await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Error { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert!(message.error_message.as_deref().unwrap().contains("429"));
    assert!(received.await.is_ok());
}

#[tokio::test]
async fn cancellation_while_waiting_for_headers_or_body_ends_the_stream() {
    for send_headers in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider =
            OpenAiProvider::new(format!("http://{}/v1", listener.local_addr().unwrap()), "")
                .unwrap();
        let (ready_tx, ready_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            if send_headers {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            }
            let _ = ready_tx.send(());
            let mut buffer = [0; 1];
            let _ = socket.read(&mut buffer).await;
        });
        let cancel = CancellationToken::new();
        let mut stream = provider.stream(request(), cancel.clone());
        assert!(matches!(
            stream.next().await,
            Some(AssistantEvent::Start { .. })
        ));
        let task = tokio::spawn(async move { stream.collect::<Vec<_>>().await });
        ready_rx.await.unwrap();
        cancel.cancel();
        let events = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        let Some(AssistantEvent::Error { message }) = events.last() else {
            panic!("{events:#?}");
        };
        assert_eq!(message.stop_reason, StopReason::Aborted);
        server.abort();
    }
}

#[tokio::test]
async fn already_cancelled_request_never_connects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider =
        OpenAiProvider::new(format!("http://{}/v1", listener.local_addr().unwrap()), "").unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let events = provider.stream(request(), cancel).collect::<Vec<_>>().await;
    assert_eq!(events.len(), 2);
    assert_eq!(
        events.last().unwrap().message().stop_reason,
        StopReason::Aborted
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn creating_but_not_polling_stream_does_not_make_a_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider =
        OpenAiProvider::new(format!("http://{}/v1", listener.local_addr().unwrap()), "").unwrap();
    let stream = provider.stream(request(), CancellationToken::new());
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
    drop(stream);
}

#[tokio::test]
async fn user_images_follow_model_capability_and_reasoning_is_explicit() {
    use cayw_ai::{ThinkingLevel, timestamp_ms};
    for images in [false, true] {
        let (provider, received) = fixture(stopped(), "200 OK", 1024).await;
        let mut input = request();
        input.model.supports_images = images;
        input.options.max_tokens = Some(123);
        input.options.temperature = Some(0.4);
        input.options.thinking_level = ThinkingLevel::High;
        input.messages = vec![Message::User {
            content: vec![Content::Image {
                data: "YWJj".into(),
                mime_type: "image/png".into(),
            }],
            timestamp: timestamp_ms(),
        }];
        let events = provider
            .stream(input, CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(AssistantEvent::Done { .. })));
        let (_, body) = received.await.unwrap();
        assert_eq!(body["max_completion_tokens"], 123);
        assert_eq!(body["temperature"], 0.4);
        assert_eq!(body["reasoning_effort"], "high");
        if images {
            assert_eq!(
                body["messages"][0]["content"][0],
                json!({"type":"image_url","image_url":{"url":"data:image/png;base64,YWJj"}})
            );
        } else {
            assert_eq!(
                body["messages"][0]["content"][0],
                json!({"type":"text","text":"[Image omitted: image/png]"})
            );
        }
    }
}

#[tokio::test]
async fn dropping_a_pending_stream_closes_its_http_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider =
        OpenAiProvider::new(format!("http://{}/v1", listener.local_addr().unwrap()), "").unwrap();
    let (ready_tx, ready_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let _ = ready_tx.send(());
        let mut buffer = [0; 1];
        socket.read(&mut buffer).await.unwrap_or(0)
    });
    let mut stream = provider.stream(request(), CancellationToken::new());
    stream.next().await;
    let collector = tokio::spawn(async move { stream.collect::<Vec<_>>().await });
    ready_rx.await.unwrap();
    collector.abort();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn system_tool_changes_and_tool_result_ids_are_preserved_in_request() {
    use cayw_ai::{
        AssistantMessage, SystemMessage, ToolCall, ToolDefinition, ToolResultMessage, timestamp_ms,
    };
    let (provider, received) = fixture(stopped(), "200 OK", 100).await;
    let mut input = request();
    let tool = ToolDefinition {
        name: "sum".into(),
        description: "add".into(),
        parameters: json!({"type":"object"}),
    };
    let mut system = SystemMessage {
        content: "policy".into(),
        tools_added: vec![tool],
        ..Default::default()
    };
    system.sections.insert("task".into(), Some("old".into()));
    let mut update = SystemMessage::default();
    update.sections.insert("task".into(), Some("new".into()));
    let mut assistant = AssistantMessage::new(&Model::new("another-provider", "another-model"));
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![
        Content::Thinking {
            thinking: "private".into(),
            signature: Some("secret-signature".into()),
        },
        Content::ToolCall(ToolCall::new("call-a", "sum", json!({"a":1}))),
    ];
    input.messages = vec![
        Message::System(system),
        Message::user("sum"),
        Message::Assistant(assistant),
        Message::System(update),
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "call-a".into(),
            tool_name: "sum".into(),
            content: vec![Content::text("1")],
            details: None,
            structured_content: Some(json!({"value":1})),
            usage: None,
            is_error: false,
            outcome_unknown: false,
            timestamp: timestamp_ms(),
            duration_ms: None,
        }),
        Message::Custom {
            kind: "ui".into(),
            data: json!("not sent"),
        },
    ];
    let events = provider
        .stream(input, CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(AssistantEvent::Done { .. })));
    let (_, body) = received.await.unwrap();
    assert_eq!(body["tools"][0]["function"]["name"], "sum");
    assert_eq!(
        body["messages"][0],
        json!({"role":"system","content":"policy\n\nnew"})
    );
    assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call-a");
    assert_eq!(
        body["messages"][3],
        json!({"role":"tool","tool_call_id":"call-a","content":[{"type":"text","text":"1"}]})
    );
    assert!(!body.to_string().contains("private"));
    assert!(!body.to_string().contains("secret-signature"));
    assert!(!body.to_string().contains("not sent"));
    assert_eq!(body["messages"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn unknown_tool_effect_cannot_be_replayed_as_an_ordinary_result() {
    use cayw_ai::{AssistantMessage, ToolCall, ToolResultMessage, timestamp_ms};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider =
        OpenAiProvider::new(format!("http://{}/v1", listener.local_addr().unwrap()), "").unwrap();
    let mut input = request();
    let mut assistant = AssistantMessage::new(&input.model);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content.push(Content::ToolCall(ToolCall::new(
        "unknown",
        "sum",
        json!({}),
    )));
    input.messages.push(Message::Assistant(assistant));
    input.messages.push(Message::ToolResult(ToolResultMessage {
        tool_call_id: "unknown".into(),
        tool_name: "sum".into(),
        content: vec![Content::text("cancelled")],
        details: None,
        structured_content: None,
        usage: None,
        is_error: true,
        outcome_unknown: true,
        timestamp: timestamp_ms(),
        duration_ms: None,
    }));
    let events = tokio::time::timeout(
        Duration::from_secs(1),
        provider
            .stream(input, CancellationToken::new())
            .collect::<Vec<_>>(),
    )
    .await
    .expect("unknown effects must fail before networking");
    let Some(AssistantEvent::Error { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert!(
        message
            .error_message
            .as_deref()
            .unwrap()
            .contains("unknown")
    );
}

#[tokio::test]
async fn invalid_unpaired_tool_history_is_rejected_before_network_io() {
    use cayw_ai::{AssistantMessage, ToolCall};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider =
        OpenAiProvider::new(format!("http://{}/v1", listener.local_addr().unwrap()), "").unwrap();
    let mut input = request();
    let mut assistant = AssistantMessage::new(&input.model);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content.push(Content::ToolCall(ToolCall::new(
        "unpaired",
        "sum",
        json!({}),
    )));
    input.messages.push(Message::Assistant(assistant));
    input.messages.push(Message::user("next"));
    let events = provider
        .stream(input, CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        events.last().unwrap().message().stop_reason,
        StopReason::Error
    );
    assert!(
        events
            .last()
            .unwrap()
            .message()
            .error_message
            .as_deref()
            .unwrap()
            .contains("tool result")
    );
}
