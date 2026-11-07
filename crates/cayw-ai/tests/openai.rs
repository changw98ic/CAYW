use cayw_ai::providers::OpenAiProvider;
use cayw_ai::{
    AssistantEvent, CancellationToken, Content, Message, Model, ModelRequest, Provider, StopReason,
    StreamOptions, UsageStatus,
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

// Break caught: Chat silently drops another protocol's replay items and signatures.
#[tokio::test]
async fn chat_rejects_foreign_protocol_metadata_before_networking() {
    let (provider, mut received) = fixture(stopped(), "200 OK", 11).await;
    let mut input = request();
    let mut assistant = cayw_ai::AssistantMessage::new(&input.model);
    assistant.content.push(Content::text("previous"));
    assistant.stop_reason = StopReason::Stop;
    assistant
        .provider_metadata
        .insert("openai_responses".into(), json!({"output":[]}));
    input
        .messages
        .extend([Message::Assistant(assistant), Message::user("next")]);
    let events: Vec<_> = tokio::time::timeout(
        Duration::from_secs(10),
        provider.stream(input, CancellationToken::new()).collect(),
    )
    .await
    .unwrap();
    assert!(
        matches!(events.last(), Some(AssistantEvent::Error { .. })),
        "{events:?}"
    );
    assert!(
        matches!(
            received.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ),
        "unsupported replay must not send a request"
    );
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
async fn usage_status_distinguishes_missing_partial_and_reported_counts() {
    // Break caught: missing fields or total_tokens alone are treated as a
    // complete zero-cost report instead of preserving what was actually sent.
    let cases = [
        (None, "unknown", (0, 0, 0)),
        (Some(Value::Null), "unknown", (0, 0, 0)),
        (Some(json!({})), "unknown", (0, 0, 0)),
        (
            Some(json!({"prompt_tokens":null,"completion_tokens":null})),
            "unknown",
            (0, 0, 0),
        ),
        (Some(json!({"total_tokens":27})), "partial", (0, 0, 27)),
        (Some(json!({"total_tokens":0})), "partial", (0, 0, 0)),
        (Some(json!({"prompt_tokens":20})), "partial", (20, 0, 20)),
        (Some(json!({"completion_tokens":7})), "partial", (0, 7, 7)),
        (Some(json!({"prompt_tokens":0})), "partial", (0, 0, 0)),
        (
            Some(json!({"prompt_tokens":0,"completion_tokens":0})),
            "reported",
            (0, 0, 0),
        ),
        (
            Some(json!({"prompt_tokens":20,"completion_tokens":7})),
            "reported",
            (20, 7, 27),
        ),
    ];
    for (usage, expected_status, expected_counts) in cases {
        let mut value = chunk(json!({"content":"ok"}), Value::Null);
        if let Some(usage) = usage {
            value["usage"] = usage;
        }
        let body = data(value) + &stopped();
        let (provider, _) = fixture(body, "200 OK", 11).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        let Some(AssistantEvent::Done { message }) = events.last() else {
            panic!("{events:#?}");
        };
        assert_eq!(
            serde_json::to_value(message).unwrap()["usage_status"],
            expected_status,
            "{message:#?}"
        );
        assert_eq!(
            (
                message.usage.input,
                message.usage.output,
                message.usage.total_tokens
            ),
            expected_counts
        );
    }
}

#[tokio::test]
async fn usage_status_merges_counts_across_chunks_and_keeps_cache_details_partial() {
    // Break caught: a later partial or empty usage block resets report presence,
    // or cache/reasoning details alone are mistaken for the complete report.
    let cases = [
        (
            vec![json!({"prompt_tokens_details":{"cached_tokens":5}})],
            "partial",
            (0, 0, 5, 0, 0, 5),
        ),
        (
            vec![json!({"completion_tokens_details":{"reasoning_tokens":3}})],
            "partial",
            (0, 0, 0, 0, 3, 0),
        ),
        (
            vec![json!({"prompt_tokens_details":{},"completion_tokens_details":{}})],
            "unknown",
            (0, 0, 0, 0, 0, 0),
        ),
        (
            vec![
                json!({"total_tokens":27}),
                json!({"prompt_tokens_details":{"cached_tokens":5}}),
                json!({}),
            ],
            "partial",
            (0, 0, 5, 0, 0, 27),
        ),
        (
            vec![
                json!({"total_tokens":27}),
                json!({"prompt_tokens":20}),
                json!({"completion_tokens":7}),
            ],
            "reported",
            (20, 7, 0, 0, 0, 27),
        ),
        (
            vec![json!({"prompt_tokens":20}), json!({"completion_tokens":7})],
            "reported",
            (20, 7, 0, 0, 0, 27),
        ),
        (
            vec![json!({"completion_tokens":7}), json!({"prompt_tokens":20})],
            "reported",
            (20, 7, 0, 0, 0, 27),
        ),
        (
            vec![
                json!({"prompt_tokens":20,"completion_tokens":7}),
                json!({"prompt_tokens_details":{"cached_tokens":5,"cache_write_tokens":2}}),
                json!({}),
            ],
            "reported",
            (13, 7, 5, 2, 0, 27),
        ),
    ];
    for (blocks, expected_status, expected_counts) in cases {
        let mut body = data(chunk(json!({"content":"ok"}), Value::Null))
            + &data(chunk(json!({}), json!("stop")));
        for usage in blocks {
            body += &data(json!({"choices":[],"usage":usage}));
        }
        body += "data: [DONE]\n\n";
        let (provider, _) = fixture(body, "200 OK", 13).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        let Some(AssistantEvent::Done { message }) = events.last() else {
            panic!("{events:#?}");
        };
        assert_eq!(
            serde_json::to_value(message).unwrap()["usage_status"],
            expected_status,
            "{message:#?}"
        );
        assert_eq!(
            (
                message.usage.input,
                message.usage.output,
                message.usage.cache_read,
                message.usage.cache_write,
                message.usage.reasoning,
                message.usage.total_tokens
            ),
            expected_counts
        );
    }
}

#[tokio::test]
async fn contradictory_usage_is_invalid_and_retains_reported_fields() {
    for reports in [
        vec![json!({"prompt_tokens":2,"completion_tokens":3,"total_tokens":999})],
        vec![
            json!({"prompt_tokens":2,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":3}}),
        ],
        vec![
            json!({"prompt_tokens":2,"completion_tokens":3,"completion_tokens_details":{"reasoning_tokens":4}}),
        ],
        vec![
            json!({"prompt_tokens":7,"completion_tokens":4}),
            json!({"prompt_tokens":6}),
        ],
        vec![json!({"prompt_tokens":7,"completion_tokens":"4"})],
        vec![json!({"prompt_tokens":u64::MAX,"completion_tokens":1})],
    ] {
        let mut body = data(chunk(json!({"content":"ok"}), Value::Null));
        for report in &reports {
            body.push_str(&data(json!({"choices":[],"usage":report})));
        }
        body.push_str(&stopped());
        let (provider, _) = fixture(body, "200 OK", 3).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        let Some(AssistantEvent::Error { message }) = events.last() else {
            panic!("contradictory usage was accepted: {events:?}");
        };
        assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
        assert_eq!(message.stop_reason, StopReason::Error);
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(value["usage_status"], "invalid");
        for (key, original) in reports.last().unwrap().as_object().unwrap() {
            assert_eq!(&value["usage_fields"][key], original);
        }
    }
}

#[tokio::test]
async fn usage_only_frames_emit_a_snapshot_before_the_terminal() {
    let report = json!({"prompt_tokens":20,"completion_tokens":3,"total_tokens":23});
    let body = data(chunk(json!({"content":"ok"}), Value::Null))
        + &data(json!({"choices":[],"usage":report}))
        + &stopped();
    let (provider, _) = fixture(body, "200 OK", 3).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let update = events
        .iter()
        .find(|event| serde_json::to_value(event).unwrap()["type"] == "usage_update")
        .expect("usage-only frame must be observable before waiting for more HTTP data");
    assert!(!update.is_terminal());
    assert_eq!(update.message().usage.total_tokens, 23);
    assert_eq!(update.message().usage_fields["prompt_tokens"], 20);
    assert_eq!(
        update.message().usage_status,
        cayw_ai::UsageStatus::Reported
    );
    assert!(matches!(events.last(), Some(AssistantEvent::Done { .. })));
}

#[tokio::test]
async fn cancelled_stream_preserves_unknown_and_partial_usage_status() {
    // Break caught: cancellation replaces the current usage report with an
    // apparently complete default, losing already reported counts.
    for (usage, expected_status, expected_input) in [
        (Value::Null, "unknown", 0),
        (json!({"prompt_tokens":20}), "partial", 20),
    ] {
        let mut value = chunk(json!({"content":"partial"}), Value::Null);
        value["usage"] = usage;
        let (provider, _) = fixture(data(value) + &stopped(), "200 OK", 8192).await;
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
        assert_eq!(message.usage.input, expected_input);
        assert_eq!(
            serde_json::to_value(message).unwrap()["usage_status"],
            expected_status
        );
    }
}

#[tokio::test]
async fn failed_stream_keeps_the_partial_usage_report() {
    // Break caught: a malformed later event erases usage already received.
    let mut value = chunk(json!({"content":"partial"}), Value::Null);
    value["usage"] = json!({"completion_tokens":7});
    let (provider, _) = fixture(data(value) + "data: {bad}\n\n", "200 OK", 8192).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Error { message }) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.usage.output, 7);
    assert_eq!(
        serde_json::to_value(message).unwrap()["usage_status"],
        "partial"
    );
}

#[test]
fn deserializing_unmarked_history_does_not_invent_reported_usage() {
    // Break caught: numeric legacy history is interpreted as a complete report
    // even though that history never retained the report's presence.
    let mut value = serde_json::to_value(cayw_ai::AssistantMessage::new(&request().model)).unwrap();
    value.as_object_mut().unwrap().remove("usage_status");
    value["usage"]["input"] = json!(20);
    value["usage"]["total_tokens"] = json!(20);
    let message: cayw_ai::AssistantMessage = serde_json::from_value(value).unwrap();
    assert_eq!(message.usage.input, 20);
    assert_eq!(
        serde_json::to_value(message).unwrap()["usage_status"],
        "unknown"
    );
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
async fn duplicate_tool_fields_fail_before_tool_completion() {
    for fragments in [
        [r#"{"city":"Paris","#, r#""city":"Tokyo"}"#],
        [
            r#"{"city":"Paris","#,
            concat!(r#"""#, r"\", r#"u0063ity":"Tokyo"}"#),
        ],
        [r#"{"items":[{"city":"Paris","#, r#""city":"Tokyo"}]}"#],
    ] {
        let body = data(chunk(
            json!({"tool_calls":[{"index":0,"id":"call-duplicate","type":"function","function":{"name":"weather","arguments":fragments[0]}}]}),
            Value::Null,
        )) + &data(chunk(
            json!({"tool_calls":[{"index":0,"function":{"arguments":fragments[1]}}]}),
            Value::Null,
        )) + &data(chunk(json!({}), json!("tool_calls")))
            + "data: [DONE]\n\n";
        let (provider, received) = fixture(body, "200 OK", 3).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
        let Some(AssistantEvent::Error { message }) = events.last() else {
            panic!("ambiguous tool arguments reached a successful terminal: {events:#?}");
        };
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(
            message
                .error_message
                .as_deref()
                .unwrap()
                .contains("duplicate")
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AssistantEvent::ToolCallEnd { .. }))
        );
        assert!(received.await.is_ok());
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
    {
        let (provider, received) = fixture(stopped(), "200 OK", 1024).await;
        let mut input = request();
        input.model.supports_images = true;
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
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({"type":"image_url","image_url":{"url":"data:image/png;base64,YWJj"}})
        );
    }
}

#[tokio::test]
async fn terminal_event_releases_the_response_even_if_the_stream_is_retained() {
    for outcome in ["success", "failure", "cancel"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = OpenAiProvider::new(
            format!("http://{}/v1", listener.local_addr().unwrap()),
            "fixture-key",
        )
        .unwrap();
        let body = if outcome == "success" {
            data(chunk(json!({"content":"ok"}), Value::Null)) + &stopped()
        } else if outcome == "failure" {
            "data: {invalid}\n\n".into()
        } else {
            data(chunk(json!({"content":"partial"}), Value::Null))
        };
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            socket
                .write_all(format!("{:x}\r\n{body}\r\n", body.len()).as_bytes())
                .await
                .unwrap();
            let mut byte = [0];
            socket.read(&mut byte).await.unwrap_or(0)
        });
        let cancel = CancellationToken::new();
        let mut stream = provider.stream(request(), cancel.clone());
        let terminal = loop {
            let event = stream.next().await.expect("one terminal event");
            if outcome == "cancel" && matches!(event, AssistantEvent::TextDelta { .. }) {
                cancel.cancel();
            }
            if event.is_terminal() {
                break event;
            }
        };
        assert_eq!(
            terminal.message().stop_reason,
            match outcome {
                "success" => StopReason::Stop,
                "cancel" => StopReason::Aborted,
                _ => StopReason::Error,
            }
        );
        let closed = tokio::time::timeout(Duration::from_secs(3), server).await;
        drop(stream);
        assert_eq!(
            closed
                .expect("response connection remained open after terminal event")
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn protocol_end_ignores_late_bytes_with_any_network_fragmentation() {
    for fragment_size in [1, 7, 8192] {
        let body = data(chunk(json!({"content":"complete"}), Value::Null))
            + &stopped()
            + &data(chunk(json!({"content":"late"}), Value::Null));
        let (provider, _) = fixture(body, "200 OK", fragment_size).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
        let Some(AssistantEvent::Done { message }) = events.last() else {
            panic!(
                "late bytes changed completed response with fragment size {fragment_size}: {events:?}"
            );
        };
        assert_eq!(message.content, vec![Content::text("complete")]);
    }
}

#[tokio::test]
async fn stream_errors_do_not_echo_remote_sensitive_values() {
    let marker = "fixture-remote-secret-not-for-history";
    let body =
        data(json!({"error":{"message":format!("key={marker} https://example.invalid/private")}}));
    let (provider, _) = fixture(body, "200 OK", 3).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(events.last(), Some(AssistantEvent::Error { .. })));
    let history = serde_json::to_string(&events).unwrap();
    assert!(
        !history.contains(marker),
        "remote error payload leaked into history"
    );
    assert!(!history.contains("https://example.invalid"));
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
    assistant.content = vec![Content::ToolCall(ToolCall::new(
        "call-a",
        "sum",
        json!({"a":1}),
    ))];
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
        json!({"role":"tool","tool_call_id":"call-a","content":[{"type":"text","text":"1"},{"type":"text","text":"{\"value\":1}"}]})
    );
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

#[tokio::test]
async fn offline_regression_live04_fragmented_arguments_finish_with_usage() {
    let body = include_str!("fixtures/sanitized_live04_snapshot.sse");
    let expected_arguments = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|value| serde_json::from_str::<Value>(value).ok())
        .filter_map(|value| {
            value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<String>();
    let expected: Value = serde_json::from_str(&expected_arguments).unwrap();
    for fragment_size in [7, 4096, 65536] {
        let (provider, _) = fixture(body.to_owned(), "200 OK", fragment_size).await;
        let mut input = request();
        input.model = Model::new("magpie", "opencode-go/deepseek-v4.1-flash");
        let mut stream = provider.stream(input, CancellationToken::new());
        let mut terminal = None;
        let mut starts = 0;
        let mut ends = 0;
        let mut argument_fragments = 0;
        let mut argument_bytes = 0;
        let mut emitted_arguments = String::new();
        while let Some(event) = stream.next().await {
            match &event {
                AssistantEvent::ToolCallStart { .. } => starts += 1,
                AssistantEvent::ToolCallDelta { delta, .. } => {
                    argument_fragments += 1;
                    argument_bytes += delta.len();
                    emitted_arguments.push_str(delta);
                }
                AssistantEvent::ToolCallEnd { .. } => ends += 1,
                _ => {}
            }
            if event.is_terminal() {
                assert!(terminal.is_none(), "duplicate terminal event");
                terminal = Some(event);
            }
        }
        let Some(AssistantEvent::Done { message }) = terminal else {
            let failure = terminal.as_ref().map(|event| {
                let message = event.message();
                (message.stop_reason, message.error_message.as_deref())
            });
            panic!(
                "fragment_size={fragment_size}, arguments={argument_fragments}/{argument_bytes}, terminal={failure:?}"
            );
        };
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!((starts, argument_bytes, ends), (1, 7938, 1));
        assert_eq!(emitted_arguments, expected_arguments);
        assert!((1..=32).contains(&argument_fragments));
        assert_eq!(message.usage_status, UsageStatus::Reported);
        assert_eq!(message.usage.total_tokens, 32300);
        assert_eq!(message.usage.output, 7832);
        assert_eq!(message.usage.reasoning, 5788);
        assert_eq!(message.usage_fields["prompt_tokens"], 24468);
        let calls = message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "execute_code");
        assert_eq!(calls[0].arguments, expected);
        assert_eq!(calls[0].arguments["script"].as_str().unwrap().len(), 7517);
        println!(
            "live04 fragment_size={fragment_size}, raw_argument_fragments=1954, emitted_argument_events={argument_fragments}, argument_bytes={argument_bytes}"
        );
        assert_eq!(cayw_ai::content_text(&message.content).len(), 157);
        let thinking_bytes: usize = message
            .content
            .iter()
            .map(|part| match part {
                Content::Thinking { thinking, .. } => thinking.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(thinking_bytes, 23141);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_interrupts_small_buffered_tool_arguments_before_completion() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider =
        OpenAiProvider::new(format!("http://{}/v1", listener.local_addr().unwrap()), "").unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let body = data(chunk(
            json!({"tool_calls":[{"index":0,"id":"buffered-call","function":{"name":"sum","arguments":"{\"a\":"}}]}),
            Value::Null,
        )) + &data(chunk(json!({"content":"waiting"}), Value::Null));
        stream
            .write_all(format!("{:x}\r\n{body}\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        let mut byte = [0];
        stream.read(&mut byte).await.unwrap_or(0)
    });
    let cancel = CancellationToken::new();
    let mut stream = provider.stream(request(), cancel.clone());
    let mut tool_started = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert!(!event.is_terminal());
        assert!(!matches!(&event, AssistantEvent::ToolCallDelta { .. }));
        if matches!(&event, AssistantEvent::ToolCallStart { .. }) {
            tool_started = true;
        }
        if matches!(&event, AssistantEvent::TextDelta { delta, .. } if delta == "waiting") {
            assert!(tool_started);
            break;
        }
    }
    cancel.cancel();
    let events = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .unwrap();
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    let Some(AssistantEvent::Error { message }) = events.last() else {
        panic!("missing error")
    };
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.usage_status, UsageStatus::Unknown);
    assert!(!events.iter().any(|event| matches!(
        event,
        AssistantEvent::ToolCallEnd { .. } | AssistantEvent::Done { .. }
    )));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn offline_regression_sanitized_request_007_tool_calls_without_calls_retains_usage() {
    // Source: /tmp/cayw-f01-f20-repair-20261028/chat-baseline-capture-02/http-capture/request-007/response.sse
    // Source SHA-256: a27e49904c88516dff6c4dab3a9a7a6cb1317a25915c19c3ffd2dd09f4cb5023
    let sse_body = include_str!("fixtures/sanitized_request_007.sse");
    for fragment_size in [1, 7, 64, 4096] {
        let (provider, _) = fixture(sse_body.to_string(), "200 OK", fragment_size).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        let last = events.last().expect("must yield terminal event");
        let Some(AssistantEvent::Error { message }) = events.last() else {
            panic!(
                "Expected AssistantEvent::Error for fragment size {fragment_size}, got: {last:?}"
            );
        };
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("tool_calls finish_reason without tool calls")
        );
        // Trailing usage in frame 109 following invalid finish_reason frame 107 must NOT be lost!
        assert_eq!(message.usage_status, UsageStatus::Reported);
        assert_eq!(message.usage.total_tokens, 5429);
        assert_eq!(message.usage.input, 394); // 5258 prompt - 4864 cached
        assert_eq!(message.usage.output, 171);
        assert_eq!(message.usage.cache_read, 4864);
        assert_eq!(message.usage.reasoning, 53);
        assert_eq!(message.usage_fields["prompt_tokens"], 5258);
        assert_eq!(message.usage_fields["completion_tokens"], 171);
        assert_eq!(message.usage_fields["total_tokens"], 5429);
        // Tool calls must be empty; no tool call should be emitted or executable.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AssistantEvent::ToolCallStart { .. }))
        );
        assert!(
            !message
                .content
                .iter()
                .any(|c| matches!(c, Content::ToolCall(_)))
        );
    }
}

#[tokio::test]
async fn offline_regression_sanitized_request_001_streaming_tool_call_concatenation() {
    // Source: /tmp/cayw-f01-f20-repair-20261028/chat-baseline-capture-02/http-capture/request-001/response.sse
    // Source SHA-256: 846fa3da681c46f84429175a816712235ea487f5d326b1f14350a0daabd85b29
    let sse_body = include_str!("fixtures/sanitized_request_001.sse");
    for fragment_size in [1, 7, 64, 4096] {
        let (provider, _) = fixture(sse_body.to_string(), "200 OK", fragment_size).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        let Some(AssistantEvent::Done { message }) = events.last() else {
            panic!("Expected Done for fragment size {fragment_size}, got: {events:?}");
        };
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.usage_status, UsageStatus::Reported);
        assert_eq!(message.usage.input, 1156);
        assert_eq!(message.usage.output, 117);
        assert_eq!(message.usage.total_tokens, 1273);
        assert_eq!(message.usage.reasoning, 5);
        let calls = message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-tool-001");
        assert_eq!(calls[0].name, "execute_code");
        assert_eq!(calls[0].arguments["action"], "exec");
        assert_eq!(
            calls[0].arguments["script"],
            "return {tools = ALL_TOOLS, matches = tool_search(\"test.run\", 10)}"
        );
    }
}

#[tokio::test]
async fn fragmented_tool_arguments_and_trailing_usage_across_network_splits() {
    let chunks = [
        chunk(
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call-probe", "type": "function", "function": {"name": "test_run", "arguments": ""}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"profile\":"}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": " \"probe\","}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": " \"limit\": 60}"}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        json!({"choices": [], "usage": {"prompt_tokens": 512, "completion_tokens": 48, "total_tokens": 560}}),
    ];
    let mut body = String::new();
    for c in chunks {
        body.push_str(&data(c));
    }
    body.push_str("data: [DONE]\r\n\r\n");

    for fragment_size in [1, 5, 23, 2048] {
        let (provider, _) = fixture(body.clone(), "200 OK", fragment_size).await;
        let events = provider
            .stream(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        let Some(AssistantEvent::Done { message }) = events.last() else {
            panic!("Expected Done for fragment size {fragment_size}, got: {events:?}");
        };
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.usage_status, UsageStatus::Reported);
        assert_eq!(message.usage.input, 512);
        assert_eq!(message.usage.output, 48);
        assert_eq!(message.usage.total_tokens, 560);
        assert_eq!(message.content.len(), 1);
        let Content::ToolCall(call) = &message.content[0] else {
            panic!("expected ToolCall content");
        };
        assert_eq!(call.id, "call-probe");
        assert_eq!(call.name, "test_run");
        assert_eq!(call.arguments, json!({"profile": "probe", "limit": 60}));
    }
}

#[tokio::test]
async fn invalid_stop_finish_with_calls_retains_trailing_usage_and_rejects() {
    let chunks = [
        chunk(
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call-1", "type": "function", "function": {"name": "test_run", "arguments": "{}"}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("stop")),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}}),
    ];
    let mut body = String::new();
    for c in chunks {
        body.push_str(&data(c));
    }
    body.push_str("data: [DONE]\r\n\r\n");

    let (provider, _) = fixture(body, "200 OK", 1).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Error { message }) = events.last() else {
        panic!("Expected Error, got: {events:?}");
    };
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Tool calls require a tool_calls or length finish_reason")
    );
    assert_eq!(message.usage_status, UsageStatus::Reported);
    assert_eq!(message.usage.input, 100);
    assert_eq!(message.usage.output, 20);
    assert_eq!(message.usage.total_tokens, 120);
}

#[tokio::test]
async fn drain_rejects_foreign_id_and_does_not_settle_unknown_usage() {
    let chunks = [
        json!({"id": "chatcmpl-valid", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hello"}}]}),
        json!({"id": "chatcmpl-valid", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"id": "chatcmpl-foreign", "choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}}),
    ];
    let mut body = String::new();
    for c in chunks {
        body.push_str(&data(c));
    }
    body.push_str("data: [DONE]\r\n\r\n");
    let (provider, _) = fixture(body, "200 OK", 64).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Error { message }) = events.last() else {
        panic!("Expected Error, got: {events:?}");
    };
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("tool_calls finish_reason without tool calls")
    );
    // Foreign ID must NOT settle the request! Usage status must remain Unknown, total tokens 0.
    assert_eq!(message.usage_status, UsageStatus::Unknown);
    assert_eq!(message.usage.total_tokens, 0);
}

#[tokio::test]
async fn drain_rejects_contradictory_usage_and_marks_invalid() {
    let chunks = [
        json!({"id": "chatcmpl-valid", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hello"}}]}),
        json!({"id": "chatcmpl-valid", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"id": "chatcmpl-valid", "choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 99999}}),
    ];
    let mut body = String::new();
    for c in chunks {
        body.push_str(&data(c));
    }
    body.push_str("data: [DONE]\r\n\r\n");
    let (provider, _) = fixture(body, "200 OK", 64).await;
    let events = provider
        .stream(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let Some(AssistantEvent::Error { message }) = events.last() else {
        panic!("Expected Error, got: {events:?}");
    };
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("tool_calls finish_reason without tool calls")
    );
    // Contradictory usage in drain must be marked Invalid, NOT Reported!
    assert_eq!(message.usage_status, UsageStatus::Invalid);
}

#[tokio::test]
async fn drain_stalling_after_error_can_be_cancelled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider =
        OpenAiProvider::new(format!("http://{}/v1", listener.local_addr().unwrap()), "").unwrap();
    let (error_chunk_sent_tx, error_chunk_sent_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        let chunk1 = json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]});
        let data1 = data(chunk1);
        socket
            .write_all(format!("{:x}\r\n{}\r\n", data1.len(), data1).as_bytes())
            .await
            .unwrap();
        let _ = error_chunk_sent_tx.send(());
        // Stall without sending trailing usage or [DONE], until client disconnects or closes
        let mut buf = [0; 64];
        let _ = socket.read(&mut buf).await;
    });
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let stream = provider.stream(request(), cancel_for_task);
    tokio::spawn(async move {
        error_chunk_sent_rx.await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });
    let events = tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>())
        .await
        .expect("stalling drain stream must terminate promptly on cancel");
    let Some(AssistantEvent::Error { message }) = events.last() else {
        panic!("Expected Error event, got: {events:?}");
    };
    assert_eq!(message.stop_reason, StopReason::Aborted);
    server.abort();
}
