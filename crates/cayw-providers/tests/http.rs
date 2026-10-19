use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, Model, ModelRequest,
    Provider, StopReason, StreamOptions, SystemMessage, ToolDefinition, ToolResultMessage,
};
use cayw_providers::{AnthropicProvider, GeminiMode, GeminiProvider, ProviderRegistry};
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};

const KEY: &str = "fixture-secret-not-for-errors";

struct Received {
    head: String,
    body: Value,
}

async fn read_request(socket: &mut TcpStream) -> Received {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buf = [0; 1024];
        let n = socket.read(&mut buf).await.unwrap();
        assert!(n > 0, "connection closed before request headers");
        bytes.extend_from_slice(&buf[..n]);
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let len: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < header_end + len {
        let mut buf = [0; 1024];
        let n = socket.read(&mut buf).await.unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&buf[..n]);
    }
    Received {
        head,
        body: serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap(),
    }
}

async fn http_server(
    status: &str,
    content_type: &str,
    body: String,
) -> (
    String,
    oneshot::Receiver<Received>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api", listener.local_addr().unwrap());
    let (tx, rx) = oneshot::channel();
    let status = status.to_owned();
    let content_type = content_type.to_owned();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let received = read_request(&mut socket).await;
        tx.send(received).ok();
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        // Deliberately split UTF-8 and SSE delimiters at arbitrary TCP write boundaries.
        for bytes in body.as_bytes().chunks(3) {
            if socket.write_all(bytes).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    });
    (base, rx, task)
}

fn sse(values: Vec<Value>) -> String {
    values
        .into_iter()
        .map(|v| format!("data: {v}\r\n\r\n"))
        .collect()
}

fn anthropic_text() -> String {
    sse(vec![
        json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":"claude-returned","content":[],"usage":{"input_tokens":7,"output_tokens":1,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}}),
        json!({"type":"ping"}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"好"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ])
}

fn gemini_text() -> String {
    sse(vec![
        json!({"responseId":"gem_fixture","modelVersion":"gemini-returned","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"你"}]}}]}),
        json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"好"}]},"finishReason":"STOP"}]}),
        json!({"usageMetadata":{"promptTokenCount":12,"cachedContentTokenCount":3,"candidatesTokenCount":4,"thoughtsTokenCount":2,"totalTokenCount":18}}),
    ])
}

fn request(provider: &str) -> ModelRequest {
    ModelRequest {
        model: Model::new(
            provider,
            if provider == "anthropic" {
                "claude-requested"
            } else {
                "gemini-requested"
            },
        ),
        messages: vec![Message::system("base instruction"), Message::user("hello")],
        options: StreamOptions {
            max_tokens: Some(128),
            temperature: Some(0.5),
            ..Default::default()
        },
    }
}

fn adapter(kind: &str, base: &str) -> Arc<dyn Provider> {
    if kind == "anthropic" {
        Arc::new(AnthropicProvider::new(base, KEY).unwrap())
    } else {
        Arc::new(GeminiProvider::new(base, KEY).unwrap())
    }
}

async fn collect(provider: &dyn Provider, request: ModelRequest) -> Vec<AssistantEvent> {
    timeout(
        Duration::from_secs(10),
        provider.stream(request, CancellationToken::new()).collect(),
    )
    .await
    .unwrap()
}

fn terminal(events: &[AssistantEvent]) -> &AssistantMessage {
    assert!(matches!(events.first(), Some(AssistantEvent::Start { .. })));
    assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
    assert!(events.last().unwrap().is_terminal());
    events.last().unwrap().message()
}

fn assert_error(events: &[AssistantEvent]) {
    let message = terminal(events);
    assert_eq!(message.stop_reason, StopReason::Error, "{events:?}");
    let error = message.error_message.as_ref().unwrap();
    assert!(!error.contains(KEY));
    assert!(!error.contains("http://"));
    assert!(!error.contains("https://"));
}

#[tokio::test]
async fn anthropic_streams_text_usage_and_native_request() {
    let (base, rx, server) = http_server(
        "200 OK",
        "text/event-stream; charset=utf-8",
        anthropic_text(),
    )
    .await;
    let events = collect(adapter("anthropic", &base).as_ref(), request("anthropic")).await;
    let message = terminal(&events);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.content, vec![Content::text("你好")]);
    assert_eq!(message.model, "claude-requested");
    assert_eq!(message.response_model.as_deref(), Some("claude-returned"));
    assert_eq!(message.response_id.as_deref(), Some("msg_fixture"));
    assert_eq!(
        (
            message.usage.input,
            message.usage.output,
            message.usage.cache_read,
            message.usage.cache_write,
            message.usage.total_tokens
        ),
        (7, 4, 3, 2, 16)
    );
    assert_eq!(
        events
            .iter()
            .filter_map(|e| if let AssistantEvent::TextDelta { delta, .. } = e {
                Some(delta.as_str())
            } else {
                None
            })
            .collect::<Vec<_>>(),
        ["你", "好"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AssistantEvent::TextEnd { .. }))
            .count(),
        1
    );
    let received = rx.await.unwrap();
    assert!(received.head.starts_with("POST /api/messages HTTP/1.1"));
    assert!(
        received
            .head
            .to_lowercase()
            .contains("x-api-key: fixture-secret-not-for-errors")
    );
    assert!(
        received
            .head
            .to_lowercase()
            .contains("anthropic-version: 2023-06-01")
    );
    assert_eq!(received.body["model"], "claude-requested");
    assert_eq!(received.body["max_tokens"], 128);
    assert_eq!(received.body["system"], "base instruction");
    assert_eq!(received.body["messages"][0]["role"], "user");
    assert_eq!(received.body["stream"], true);
    server.await.unwrap();
}

#[tokio::test]
async fn gemini_streams_text_usage_and_native_request() {
    let (base, rx, server) = http_server("200 OK", "text/event-stream", gemini_text()).await;
    let events = collect(adapter("gemini", &base).as_ref(), request("gemini")).await;
    let message = terminal(&events);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.content, vec![Content::text("你好")]);
    assert_eq!(message.model, "gemini-requested");
    assert_eq!(message.response_model.as_deref(), Some("gemini-returned"));
    assert_eq!(message.response_id.as_deref(), Some("gem_fixture"));
    assert_eq!(
        (
            message.usage.input,
            message.usage.output,
            message.usage.cache_read,
            message.usage.reasoning,
            message.usage.total_tokens
        ),
        (9, 6, 3, 2, 18)
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AssistantEvent::TextDelta { .. }))
            .count(),
        2
    );
    let received = rx.await.unwrap();
    assert!(
        received.head.starts_with(
            "POST /api/models/gemini-requested:streamGenerateContent?alt=sse HTTP/1.1"
        )
    );
    assert!(
        received
            .head
            .to_lowercase()
            .contains("x-goog-api-key: fixture-secret-not-for-errors")
    );
    assert!(!received.head.contains("?key="));
    assert_eq!(received.body["generationConfig"]["maxOutputTokens"], 128);
    assert_eq!(
        received.body["systemInstruction"]["parts"][0]["text"],
        "base instruction"
    );
    assert_eq!(received.body["contents"][0]["parts"][0]["text"], "hello");
    server.await.unwrap();
}

fn tools_request(kind: &str) -> ModelRequest {
    let mut req = request(kind);
    req.messages.insert(1, Message::System(SystemMessage {
        tools_added: vec![ToolDefinition { name: "weather".into(), description: "Get weather".into(), parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}) }],
        ..Default::default()
    }));
    req
}

fn tool_result(id: &str, name: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: name.into(),
        content: vec![Content::text("sunny")],
        details: None,
        structured_content: Some(json!({"temperature":21})),
        usage: None,
        is_error: false,
        outcome_unknown: false,
        timestamp: 0,
        duration_ms: None,
    })
}

fn anthropic_tool(input_tail: &str) -> String {
    sse(vec![
        json!({"type":"message_start","message":{"id":"msg_tool","type":"message","role":"assistant","model":"claude-returned","content":[],"usage":{"input_tokens":9,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_a","name":"weather","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input_tail}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ])
}

#[tokio::test]
async fn anthropic_tool_round_trip_preserves_schema_ids_names_and_structured_results() {
    let (base, rx, server) =
        http_server("200 OK", "text/event-stream", anthropic_tool("\"Paris\"}")).await;
    let mut req = tools_request("anthropic");
    let events = collect(adapter("anthropic", &base).as_ref(), req.clone()).await;
    let message = terminal(&events).clone();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.tool_calls()[0].arguments, json!({"city":"Paris"}));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AssistantEvent::ToolCallDelta { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AssistantEvent::ToolCallEnd { .. }))
            .count(),
        1
    );
    assert_eq!(
        rx.await.unwrap().body["tools"][0]["input_schema"]["required"],
        json!(["city"])
    );
    server.await.unwrap();
    req.messages.push(Message::Assistant(message));
    req.messages.push(tool_result("call_a", "weather"));
    let (base, rx, server) = http_server("200 OK", "text/event-stream", anthropic_text()).await;
    assert_eq!(
        terminal(&collect(adapter("anthropic", &base).as_ref(), req).await).stop_reason,
        StopReason::Stop
    );
    let body = rx.await.unwrap().body;
    assert_eq!(body["messages"][1]["content"][0]["id"], "call_a");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_a");
    assert!(
        body["messages"][2]["content"][0]["content"]
            .to_string()
            .contains("temperature")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn gemini_parallel_same_name_calls_get_distinct_ids_and_replay_correct_names() {
    let body = sse(vec![
        json!({"responseId":"gem_tools","candidates":[{"index":0,"content":{"role":"model","parts":[{"functionCall":{"name":"weather","args":{"city":"Paris"}}},{"functionCall":{"name":"weather","args":{"city":"Tokyo"}}}]},"finishReason":"STOP"}]}),
    ]);
    let (base, rx, server) = http_server("200 OK", "text/event-stream", body).await;
    let mut req = tools_request("gemini");
    let events = collect(adapter("gemini", &base).as_ref(), req.clone()).await;
    let message = terminal(&events).clone();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let calls = message.tool_calls();
    assert_eq!(calls.len(), 2);
    assert!(!calls[0].id.is_empty());
    assert_ne!(calls[0].id, calls[1].id);
    assert_eq!(calls[1].arguments, json!({"city":"Tokyo"}));
    assert_eq!(
        rx.await.unwrap().body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["required"],
        json!(["city"])
    );
    server.await.unwrap();
    req.messages.push(Message::Assistant(message));
    // Results may arrive in a different order; names and IDs must still pair.
    req.messages.push(tool_result(&calls[1].id, "weather"));
    req.messages.push(tool_result(&calls[0].id, "weather"));
    let (base, rx, server) = http_server("200 OK", "text/event-stream", gemini_text()).await;
    assert_eq!(
        terminal(&collect(adapter("gemini", &base).as_ref(), req).await).stop_reason,
        StopReason::Stop
    );
    let body = rx.await.unwrap().body;
    assert_eq!(body["contents"][1]["role"], "model");
    assert_eq!(
        body["contents"][1]["parts"][1]["functionCall"]["id"],
        calls[1].id
    );
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["id"],
        calls[1].id
    );
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["name"],
        "weather"
    );
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
        json!({"temperature":21})
    );
    server.await.unwrap();
}

#[tokio::test]
async fn gemini_generate_content_uses_json_endpoint_and_preserves_provider_call_id() {
    let body = json!({"modelVersion":"gemini-returned","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"native_call","name":"weather","args":{"city":"Paris"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}}).to_string();
    let (base, rx, server) = http_server("200 OK", "application/json", body).await;
    let provider = GeminiProvider::new(&base, KEY)
        .unwrap()
        .with_mode(GeminiMode::GenerateContent);
    let events = collect(&provider, tools_request("gemini")).await;
    assert_eq!(terminal(&events).tool_calls()[0].id, "native_call");
    assert_eq!(terminal(&events).usage.total_tokens, 5);
    assert!(
        rx.await
            .unwrap()
            .head
            .starts_with("POST /api/models/gemini-requested:generateContent HTTP/1.1")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn both_protocols_report_bad_json_http_errors_and_unsupported_content_once() {
    for kind in ["anthropic", "gemini"] {
        let mut cases = vec![
            (
                "200 OK",
                "text/event-stream",
                "data: {broken\n\n".to_owned(),
            ),
            (
                "401 Unauthorized",
                "application/json",
                format!("{{\"error\":\"{KEY} https://secret.invalid\"}}"),
            ),
            ("302 Found", "text/event-stream", String::new()),
            ("200 OK", "application/json", "{}".to_owned()),
            ("200 OK", "text/event-stream", String::new()),
        ];
        if kind == "anthropic" {
            cases.push(("200 OK", "text/event-stream", anthropic_tool("7}")));
            cases.push(("200 OK", "text/event-stream", anthropic_tool("\"Paris\"")));
            cases.push((
                "200 OK",
                "text/event-stream",
                sse(vec![
                    json!({"type":"error","error":{"type":"api_error","message":KEY}}),
                ]),
            ));
        } else {
            cases.push((
                "200 OK",
                "text/event-stream",
                sse(vec![json!({"error":{"message":KEY}})]),
            ));
            cases.push(("200 OK", "text/event-stream", sse(vec![json!({"candidates":[{"content":{"parts":[{"text":"secret","thought":true,"thoughtSignature":"opaque"}]},"finishReason":"STOP"}]})])));
            cases.push(("200 OK", "text/event-stream", sse(vec![json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"weather","args":[]}}]},"finishReason":"STOP"}]})])));
            cases.push((
                "200 OK",
                "text/event-stream",
                sse(vec![json!({"promptFeedback":{"blockReason":"SAFETY"}})]),
            ));
        }
        for (status, content_type, body) in cases {
            let (base, rx, server) = http_server(status, content_type, body).await;
            let events = collect(adapter(kind, &base).as_ref(), tools_request(kind)).await;
            assert_error(&events);
            rx.await.unwrap();
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn invalid_history_and_unsupported_inputs_fail_before_network() {
    for kind in ["anthropic", "gemini"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = adapter(kind, &format!("http://{}", listener.local_addr().unwrap()));
        let mut cases = Vec::new();
        let mut req = tools_request(kind);
        let mut assistant = AssistantMessage::new(&req.model);
        assistant.stop_reason = StopReason::ToolUse;
        assistant
            .content
            .push(Content::ToolCall(cayw_ai::ToolCall::new(
                "c1",
                "weather",
                json!({"city":"Paris"}),
            )));
        req.messages.push(Message::Assistant(assistant));
        cases.push(req.clone());
        req.messages.push(tool_result("c1", "wrong_name"));
        cases.push(req);
        let mut req = request(kind);
        let mut result = tool_result("orphan", "weather");
        if let Message::ToolResult(result) = &mut result {
            result.outcome_unknown = true;
        }
        req.messages.push(result);
        cases.push(req);
        let mut req = request(kind);
        req.messages.push(Message::User {
            content: vec![Content::Image {
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }],
            timestamp: 0,
        });
        cases.push(req);
        let mut req = request(kind);
        req.options.thinking_level = cayw_ai::ThinkingLevel::High;
        cases.push(req);
        let mut req = tools_request(kind);
        if let Message::System(system) = &mut req.messages[1] {
            system.tools_added[0].parameters = json!({"type":"object","properties":{"x":{"$ref":"https://secret.invalid/schema"}}});
        }
        cases.push(req);
        let mut req = request(kind);
        req.options.max_tokens = Some(0);
        cases.push(req);
        for req in cases {
            assert_error(&collect(provider.as_ref(), req).await);
        }
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "preflight must not open a connection"
        );
    }
}

#[tokio::test]
async fn both_protocols_fold_system_changes_and_send_user_images_explicitly() {
    for kind in ["anthropic", "gemini"] {
        let (base, rx, server) = http_server(
            "200 OK",
            "text/event-stream",
            if kind == "anthropic" {
                anthropic_text()
            } else {
                gemini_text()
            },
        )
        .await;
        let mut req = tools_request(kind);
        req.model.supports_images = true;
        req.messages.push(Message::System(SystemMessage {
            sections: [("policy".into(), Some("current policy".into()))].into(),
            tools_removed: vec!["weather".into()],
            ..Default::default()
        }));
        req.messages.push(Message::User {
            content: vec![Content::Image {
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }],
            timestamp: 0,
        });
        assert_eq!(
            terminal(&collect(adapter(kind, &base).as_ref(), req).await).stop_reason,
            StopReason::Stop
        );
        let body = rx.await.unwrap().body;
        assert!(body.get("tools").is_none());
        if kind == "anthropic" {
            assert_eq!(body["system"], "base instruction\n\ncurrent policy");
            assert_eq!(
                body["messages"][0]["content"][1]["source"]["data"],
                "aGVsbG8="
            );
        } else {
            assert_eq!(
                body["systemInstruction"]["parts"][0]["text"],
                "base instruction\n\ncurrent policy"
            );
            assert_eq!(
                body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
                "image/png"
            );
        }
        server.await.unwrap();
    }
}

async fn hanging_server() -> (
    String,
    oneshot::Receiver<Received>,
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (request_tx, request_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        request_tx.send(read_request(&mut socket).await).ok();
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let mut buf = [0; 1];
        match timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {
                closed_tx.send(()).ok();
            }
            result => panic!("request body was not dropped: {result:?}"),
        }
    });
    (base, request_rx, closed_rx, task)
}

#[tokio::test]
async fn both_protocols_are_lazy_and_cancel_or_drop_the_network_body() {
    for kind in ["anthropic", "gemini"] {
        for drop_instead in [false, true] {
            let (base, request_rx, closed_rx, server) = hanging_server().await;
            let token = CancellationToken::new();
            let mut stream = adapter(kind, &base).stream(request(kind), token.clone());
            assert!(matches!(
                stream.next().await,
                Some(AssistantEvent::Start { .. })
            ));
            let mut pending = Box::pin(stream.next());
            tokio::select! {
                biased;
                result = &mut pending => panic!("unexpected event: {result:?}"),
                received = request_rx => { received.unwrap(); }
            }
            drop(pending);
            if drop_instead {
                drop(stream);
            } else {
                token.cancel();
                let events: Vec<_> = timeout(Duration::from_secs(5), stream.collect())
                    .await
                    .unwrap();
                assert_eq!(events.len(), 1);
                assert!(
                    matches!(&events[0], AssistantEvent::Error { message } if message.stop_reason == StopReason::Aborted)
                );
            }
            timeout(Duration::from_secs(5), closed_rx)
                .await
                .unwrap()
                .unwrap();
            server.await.unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = adapter(kind, &format!("http://{}", listener.local_addr().unwrap()));
        let token = CancellationToken::new();
        let stream = provider.stream(request(kind), token.clone());
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        token.cancel();
        let events: Vec<_> = stream.collect().await;
        assert_eq!(terminal(&events).stop_reason, StopReason::Aborted);
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn ambiguous_duplicate_tool_json_is_rejected_before_tool_completion() {
    let anthropic = anthropic_tool("\"Paris\",\"city\":\"Tokyo\"}");
    let gemini = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"weather\",\"args\":{\"city\":\"Paris\",\"city\":\"Tokyo\"}}}]},\"finishReason\":\"STOP\"}]}\n\n".to_owned();
    for (kind, body) in [("anthropic", anthropic), ("gemini", gemini)] {
        let (base, rx, server) = http_server("200 OK", "text/event-stream", body).await;
        let events = collect(adapter(kind, &base).as_ref(), tools_request(kind)).await;
        assert_error(&events);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AssistantEvent::ToolCallEnd { .. }))
        );
        rx.await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn anthropic_accepts_incremental_message_usage_without_erasing_prior_counters() {
    let body = sse(vec![
        json!({"type":"message_start","message":{"id":"msg_usage","type":"message","role":"assistant","model":"claude-returned","content":[],"usage":{"input_tokens":7,"output_tokens":1,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hello"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":null,"stop_sequence":null},"usage":{"output_tokens":2}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}),
        json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":5,"input_tokens":null,"cache_read_input_tokens":null}}),
        json!({"type":"message_stop"}),
    ]);
    let (base, rx, server) = http_server("200 OK", "text/event-stream", body).await;
    let events = collect(adapter("anthropic", &base).as_ref(), request("anthropic")).await;
    let message = terminal(&events);
    assert_eq!(message.stop_reason, StopReason::Stop, "{events:?}");
    assert_eq!(
        (
            message.usage.input,
            message.usage.output,
            message.usage.cache_read,
            message.usage.cache_write,
            message.usage.total_tokens
        ),
        (7, 5, 3, 2, 17)
    );
    rx.await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn stream_protocol_errors_never_produce_a_success_terminal() {
    let start = json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":"claude-returned","content":[],"usage":{"input_tokens":1,"output_tokens":0}}});
    let cases = vec![
        (
            "anthropic",
            sse(vec![
                start.clone(),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"unopened"}}),
            ]),
        ),
        (
            "anthropic",
            sse(vec![
                start.clone(),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":0}}),
                json!({"type":"message_stop"}),
            ]),
        ),
        ("anthropic", sse(vec![start.clone(), start.clone()])),
        (
            "anthropic",
            sse(vec![
                start.clone(),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"opaque","signature":"opaque"}}),
            ]),
        ),
        ("anthropic", sse(vec![start])),
        (
            "gemini",
            sse(vec![json!({"candidates":[{"finishReason":"STOP"}]})]),
        ),
        (
            "gemini",
            sse(vec![
                json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}),
            ]),
        ),
        (
            "gemini",
            sse(vec![
                json!({"candidates":[{"content":{"parts":[{"text":"blocked"}]},"finishReason":"SAFETY"}]}),
            ]),
        ),
        (
            "gemini",
            sse(vec![
                json!({"responseId":"first","candidates":[{"content":{"parts":[{"text":"partial"}]}}]}),
                json!({"responseId":"second","candidates":[{"finishReason":"STOP"}]}),
            ]),
        ),
        (
            "gemini",
            sse(vec![
                json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"duplicate","name":"weather","args":{"city":"Paris"}}},{"functionCall":{"id":"duplicate","name":"weather","args":{"city":"Tokyo"}}}]},"finishReason":"STOP"}]}),
            ]),
        ),
        (
            "gemini",
            sse(vec![
                json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"undeclared","args":{}}}]},"finishReason":"STOP"}]}),
            ]),
        ),
        (
            "gemini",
            sse(vec![
                json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"weather","args":{"city":7}}}]},"finishReason":"STOP"}]}),
            ]),
        ),
        (
            "gemini",
            sse(vec![
                json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"weather","partialArgs":[{"jsonPath":"$.city","stringValue":"Paris"}],"willContinue":false}}]},"finishReason":"STOP"}]}),
            ]),
        ),
    ];
    for (kind, body) in cases {
        let (base, rx, server) = http_server("200 OK", "text/event-stream", body).await;
        assert_error(&collect(adapter(kind, &base).as_ref(), tools_request(kind)).await);
        rx.await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn exhausted_model_history_is_rejected_before_network() {
    for kind in ["anthropic", "gemini"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = adapter(kind, &format!("http://{}", listener.local_addr().unwrap()));
        let mut req = request(kind);
        let mut assistant = AssistantMessage::new(&req.model);
        assistant.stop_reason = StopReason::Stop;
        assistant.content = vec![Content::text("already answered")];
        req.messages.push(Message::Assistant(assistant));
        let cancel = CancellationToken::new();
        let mut stream = provider.stream(req, cancel.clone());
        assert!(matches!(
            stream.next().await,
            Some(AssistantEvent::Start { .. })
        ));
        tokio::select! {
            event = stream.next() => {
                assert!(matches!(event, Some(AssistantEvent::Error { .. })));
            }
            received = listener.accept() => {
                received.unwrap();
                cancel.cancel();
                panic!("history ending in an assistant answer must fail before networking");
            }
        }
    }
}

#[tokio::test]
async fn both_protocols_never_retry_or_follow_redirects() {
    for kind in ["anthropic", "gemini"] {
        for status in [
            "302 Found",
            "429 Too Many Requests",
            "503 Service Unavailable",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let base = format!("http://{address}/api");
            let provider = adapter(kind, &base);
            let response = format!(
                "HTTP/1.1 {status}\r\nLocation: http://{address}/redirected\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                socket.write_all(response.as_bytes()).await.unwrap();
                drop(socket);
                assert!(
                    timeout(Duration::from_millis(200), listener.accept())
                        .await
                        .is_err(),
                    "unexpected second HTTP request"
                );
            });
            assert_error(&collect(provider.as_ref(), request(kind)).await);
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn cancellation_and_drop_abort_waiting_for_response_headers() {
    for kind in ["anthropic", "gemini"] {
        for drop_instead in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let (received_tx, received_rx) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                received_tx.send(()).ok();
                let mut buf = [0; 1];
                let result = timeout(Duration::from_secs(5), socket.read(&mut buf)).await;
                assert!(
                    matches!(result, Ok(Ok(0)) | Ok(Err(_))),
                    "socket retained while waiting for response headers: {result:?}"
                );
            });
            let cancel = CancellationToken::new();
            let mut stream = adapter(kind, &base).stream(request(kind), cancel.clone());
            assert!(matches!(
                stream.next().await,
                Some(AssistantEvent::Start { .. })
            ));
            let mut pending = Box::pin(stream.next());
            tokio::select! {
                result = &mut pending => panic!("unexpected event: {result:?}"),
                result = received_rx => { result.unwrap(); }
            }
            drop(pending);
            if drop_instead {
                drop(stream);
            } else {
                cancel.cancel();
                let events: Vec<_> = timeout(Duration::from_secs(5), stream.collect())
                    .await
                    .unwrap();
                assert_eq!(events.len(), 1);
                assert!(
                    matches!(&events[0], AssistantEvent::Error { message } if message.stop_reason == StopReason::Aborted)
                );
            }
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn gemini_json_mode_cancel_and_drop_close_a_partial_body() {
    for drop_instead in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (received_tx, received_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"candidates\":[").await.unwrap();
            received_tx.send(()).ok();
            let mut buf = [0; 1];
            let result = timeout(Duration::from_secs(5), socket.read(&mut buf)).await;
            assert!(
                matches!(result, Ok(Ok(0)) | Ok(Err(_))),
                "JSON response connection was retained: {result:?}"
            );
        });
        let provider = GeminiProvider::new(base, KEY)
            .unwrap()
            .with_mode(GeminiMode::GenerateContent);
        let cancel = CancellationToken::new();
        let mut stream = provider.stream(request("gemini"), cancel.clone());
        assert!(matches!(
            stream.next().await,
            Some(AssistantEvent::Start { .. })
        ));
        let mut pending = Box::pin(stream.next());
        tokio::select! {
            result = &mut pending => panic!("unexpected event: {result:?}"),
            result = received_rx => { result.unwrap(); }
        }
        drop(pending);
        if drop_instead {
            drop(stream);
        } else {
            cancel.cancel();
            let events: Vec<_> = timeout(Duration::from_secs(5), stream.collect())
                .await
                .unwrap();
            assert_eq!(events.len(), 1);
            assert!(
                matches!(&events[0], AssistantEvent::Error { message } if message.stop_reason == StopReason::Aborted)
            );
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn text_deltas_arrive_before_http_response_completion() {
    for kind in ["anthropic", "gemini"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (prefix, suffix) = if kind == "anthropic" {
            (
                sse(vec![
                    json!({"type":"message_start","message":{"id":"streaming","type":"message","role":"assistant","model":"returned","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"first"}}),
                ]),
                sse(vec![
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                    json!({"type":"message_stop"}),
                ]),
            )
        } else {
            (
                sse(vec![
                    json!({"candidates":[{"content":{"parts":[{"text":"first"}]}}]}),
                ]),
                sse(vec![json!({"candidates":[{"finishReason":"STOP"}]})]),
            )
        };
        let (release_tx, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                prefix.len() + suffix.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(prefix.as_bytes()).await.unwrap();
            timeout(Duration::from_secs(5), release_rx)
                .await
                .unwrap()
                .unwrap();
            socket.write_all(suffix.as_bytes()).await.unwrap();
        });
        let mut stream = adapter(kind, &base).stream(request(kind), CancellationToken::new());
        let mut events = timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            loop {
                let event = stream.next().await.unwrap();
                let delta =
                    matches!(&event, AssistantEvent::TextDelta { delta, .. } if delta == "first");
                events.push(event);
                if delta {
                    return events;
                }
            }
        })
        .await
        .unwrap();
        release_tx.send(()).unwrap();
        events.extend(
            timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
                .await
                .unwrap(),
        );
        assert_eq!(terminal(&events).stop_reason, StopReason::Stop);
        assert_eq!(terminal(&events).content, vec![Content::text("first")]);
        server.await.unwrap();
    }
}

// Large writes keep these fixtures fast while exercising the real HTTP reader.
async fn bulk_server(content_type: &str, body: String) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let content_type = content_type.to_owned();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        let _ = socket.write_all(body.as_bytes()).await;
    });
    (base, server)
}

fn gemini_body(mode: GeminiMode, value: Value) -> (&'static str, String) {
    if mode == GeminiMode::GenerateContent {
        ("application/json", value.to_string())
    } else {
        ("text/event-stream", sse(vec![value]))
    }
}

#[tokio::test]
async fn gemini_large_frame_preserves_all_text_and_tool_events() {
    for mode in [
        GeminiMode::StreamGenerateContent,
        GeminiMode::GenerateContent,
    ] {
        let mut parts = vec![json!({"text":"fragment"}); 2000];
        parts.push(json!({"functionCall":{"id":"a","name":"weather","args":{"city":"Paris"}}}));
        parts.push(json!({"text":"after tool"}));
        parts.push(json!({"functionCall":{"id":"b","name":"weather","args":{"city":"Tokyo"}}}));
        let value = json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}]});
        let (mime, body) = gemini_body(mode, value);
        let (base, server) = bulk_server(mime, body).await;
        let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(mode);
        let mut stream = provider.stream(tools_request("gemini"), CancellationToken::new());
        let mut deltas = 0;
        let mut text_starts = 0;
        let mut text_ends = 0;
        let mut tools = Vec::new();
        let mut terminal_count = 0;
        while let Some(event) = timeout(Duration::from_secs(10), stream.next())
            .await
            .unwrap()
        {
            match event {
                AssistantEvent::TextStart { .. } => text_starts += 1,
                AssistantEvent::TextDelta { delta, partial, .. } => {
                    deltas += 1;
                    if deltas <= 2000 {
                        assert_eq!(delta, "fragment");
                        assert_eq!(
                            partial.content,
                            vec![Content::text("fragment".repeat(deltas))]
                        );
                    } else {
                        assert_eq!(delta, "after tool");
                    }
                }
                AssistantEvent::TextEnd { .. } => text_ends += 1,
                AssistantEvent::ToolCallStart { index, partial } => {
                    let Content::ToolCall(call) = &partial.content[index] else {
                        panic!("missing call")
                    };
                    assert_eq!(call.arguments, Value::Null);
                    tools.push((index, "start"));
                }
                AssistantEvent::ToolCallDelta {
                    index,
                    delta,
                    partial,
                } => {
                    let expected = if index == 1 {
                        json!({"city":"Paris"})
                    } else {
                        json!({"city":"Tokyo"})
                    };
                    let Content::ToolCall(call) = &partial.content[index] else {
                        panic!("missing call")
                    };
                    assert_eq!(call.arguments, expected);
                    assert_eq!(serde_json::from_str::<Value>(&delta).unwrap(), expected);
                    tools.push((index, "delta"));
                }
                AssistantEvent::ToolCallEnd { index, .. } => tools.push((index, "end")),
                AssistantEvent::Done { message } => {
                    terminal_count += 1;
                    assert_eq!(message.stop_reason, StopReason::ToolUse);
                    assert_eq!(message.content.len(), 4);
                    assert_eq!(message.tool_calls()[1].id, "b");
                }
                AssistantEvent::Error { message } => panic!("unexpected error: {message:?}"),
                _ => {}
            }
        }
        assert_eq!(
            (deltas, text_starts, text_ends, terminal_count),
            (2001, 2, 2, 1)
        );
        assert_eq!(
            tools,
            [
                (1, "start"),
                (1, "delta"),
                (1, "end"),
                (3, "start"),
                (3, "delta"),
                (3, "end")
            ]
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn anthropic_message_stop_releases_an_open_response_before_terminal_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider =
        AnthropicProvider::new(format!("http://{}", listener.local_addr().unwrap()), KEY).unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let prefix = anthropic_text();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
            prefix.len() + 1000
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        socket.write_all(prefix.as_bytes()).await.unwrap();
        let mut byte = [0];
        let closed = timeout(Duration::from_secs(5), socket.read(&mut byte)).await;
        assert!(
            matches!(closed, Ok(Ok(0)) | Ok(Err(_))),
            "response body was not released: {closed:?}"
        );
    });
    let mut stream = provider.stream(request("anthropic"), CancellationToken::new());
    loop {
        let event = timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap();
        if event.is_terminal() {
            assert!(matches!(event, AssistantEvent::Done { .. }));
            break;
        }
    }
    // The HTTP response must already be dropped even with the stream retained.
    server.await.unwrap();
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn gemini_same_thread_cancellation_interrupts_a_single_large_frame() {
    for mode in [
        GeminiMode::StreamGenerateContent,
        GeminiMode::GenerateContent,
    ] {
        let value = json!({"candidates":[{"content":{"parts":vec![json!({"text":"x"}); 3000]},"finishReason":"STOP"}]});
        let (mime, body) = gemini_body(mode, value);
        let (base, server) = bulk_server(mime, body).await;
        let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(mode);
        let cancel = CancellationToken::new();
        let (started_tx, started_rx) = oneshot::channel();
        let mut started_tx = Some(started_tx);
        let canceller = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                started_rx.await.unwrap();
                cancel.cancel();
            })
        };
        let mut stream = provider.stream(request("gemini"), cancel);
        let mut terminal_count = 0;
        while let Some(event) = timeout(Duration::from_secs(10), stream.next())
            .await
            .unwrap()
        {
            if matches!(event, AssistantEvent::TextStart { .. }) {
                started_tx.take().unwrap().send(()).unwrap();
            }
            if event.is_terminal() {
                terminal_count += 1;
                assert_eq!(event.message().stop_reason, StopReason::Aborted);
                assert!(cayw_ai::content_text(&event.message().content).len() < 3000);
            }
        }
        assert_eq!(terminal_count, 1);
        canceller.await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn gemini_cancel_and_drop_between_tool_events_do_not_consume_remaining_parts() {
    for mode in [
        GeminiMode::StreamGenerateContent,
        GeminiMode::GenerateContent,
    ] {
        for after_delta in [false, true] {
            for drop_instead in [false, true] {
                let value = json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"first","name":"weather","args":{"city":"Paris"}}},{"functionCall":{"id":"second","name":"weather","args":{"city":"Tokyo"}}}]},"finishReason":"STOP"}]});
                let (mime, body) = gemini_body(mode, value);
                let (base, server) = bulk_server(mime, body).await;
                let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(mode);
                let cancel = CancellationToken::new();
                let mut stream = provider.stream(tools_request("gemini"), cancel.clone());
                loop {
                    let event = timeout(Duration::from_secs(5), stream.next())
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(!event.is_terminal(), "unexpected terminal {event:?}");
                    let reached = if after_delta {
                        matches!(event, AssistantEvent::ToolCallDelta { .. })
                    } else {
                        matches!(event, AssistantEvent::ToolCallStart { .. })
                    };
                    if reached {
                        break;
                    }
                }
                if drop_instead {
                    drop(stream);
                } else {
                    cancel.cancel();
                    let event = timeout(Duration::from_secs(5), stream.next())
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(matches!(event, AssistantEvent::Error { .. }));
                    assert_eq!(event.message().stop_reason, StopReason::Aborted);
                    let calls = event.message().tool_calls();
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0].id, "first");
                    assert_eq!(
                        calls[0].arguments,
                        if after_delta {
                            json!({"city":"Paris"})
                        } else {
                            Value::Null
                        }
                    );
                    assert!(stream.next().await.is_none());
                }
                server.await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn gemini_rejects_role_only_finish_content_without_parts() {
    for mode in [
        GeminiMode::StreamGenerateContent,
        GeminiMode::GenerateContent,
    ] {
        let value = json!({"candidates":[{"content":{"role":"model"},"finishReason":"STOP"}]});
        let (mime, body) = gemini_body(mode, value);
        let (base, server) = bulk_server(mime, body).await;
        let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(mode);
        assert_error(&collect(&provider, request("gemini")).await);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn response_fragment_and_tool_limits_cover_both_gemini_modes() {
    for mode in [
        GeminiMode::StreamGenerateContent,
        GeminiMode::GenerateContent,
    ] {
        for tool_calls in [false, true] {
            let parts = if tool_calls {
                (0..129).map(|index| json!({"functionCall":{"id":format!("call_{index}"),"name":"weather","args":{"city":"Paris"}}})).collect::<Vec<_>>()
            } else {
                vec![json!({"text":""}); 4097]
            };
            let value = json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]});
            let (mime, body) = gemini_body(mode, value);
            let (base, server) = bulk_server(mime, body).await;
            let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(mode);
            let events = collect(&provider, tools_request("gemini")).await;
            assert_error(&events);
            assert!(
                events
                    .last()
                    .unwrap()
                    .message()
                    .error_message
                    .as_ref()
                    .unwrap()
                    .contains("limit")
            );
            assert!(terminal(&events).tool_calls().len() <= 128);
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn response_fragment_limits_accumulate_across_sse_frames() {
    for kind in ["anthropic", "gemini"] {
        let body = if kind == "gemini" {
            let chunk = json!({"candidates":[{"content":{"parts":[{"text":""}]}}]});
            sse(vec![chunk; 4097]) + &sse(vec![json!({"candidates":[{"finishReason":"STOP"}]})])
        } else {
            let mut values = vec![
                json!({"type":"message_start","message":{"id":"limited","model":"returned","role":"assistant","type":"message","content":[],"usage":{}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ];
            values.extend(vec![json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}); 4097]);
            values.extend([
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
                json!({"type":"message_stop"}),
            ]);
            sse(values)
        };
        let (base, server) = bulk_server("text/event-stream", body).await;
        assert_error(&collect(adapter(kind, &base).as_ref(), request(kind)).await);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn tool_call_limits_accumulate_across_sse_frames() {
    for kind in ["anthropic", "gemini"] {
        let body = if kind == "gemini" {
            let values = (0..129).map(|i| json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":format!("call_{i}"),"name":"weather","args":{"city":"Paris"}}}]}}]})).collect();
            sse(values) + &sse(vec![json!({"candidates":[{"finishReason":"STOP"}]})])
        } else {
            let mut values = vec![
                json!({"type":"message_start","message":{"id":"limited","model":"returned","role":"assistant","type":"message","content":[],"usage":{}}}),
            ];
            for i in 0..129 {
                values.push(json!({"type":"content_block_start","index":i,"content_block":{"type":"tool_use","id":format!("call_{i}"),"name":"weather","input":{"city":"Paris"}}}));
                values.push(json!({"type":"content_block_stop","index":i}));
            }
            values.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}));
            values.push(json!({"type":"message_stop"}));
            sse(values)
        };
        let (base, server) = bulk_server("text/event-stream", body).await;
        let events = collect(adapter(kind, &base).as_ref(), tools_request(kind)).await;
        assert_error(&events);
        assert_eq!(terminal(&events).tool_calls().len(), 128);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AssistantEvent::ToolCallEnd { .. }))
                .count(),
            128
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn gemini_accepts_complete_responses_at_fragment_and_tool_limits() {
    for mode in [
        GeminiMode::StreamGenerateContent,
        GeminiMode::GenerateContent,
    ] {
        for calls in [false, true] {
            let parts = if calls {
                (0..128).map(|i| json!({"functionCall":{"id":format!("call_{i}"),"name":"weather","args":{"city":"Paris"}}})).collect::<Vec<_>>()
            } else {
                vec![json!({"text":""}); 4096]
            };
            let (mime, body) = gemini_body(
                mode,
                json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]}),
            );
            let (base, server) = bulk_server(mime, body).await;
            let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(mode);
            let events = collect(&provider, tools_request("gemini")).await;
            let message = terminal(&events);
            assert_eq!(
                message.stop_reason,
                if calls {
                    StopReason::ToolUse
                } else {
                    StopReason::Stop
                }
            );
            assert_eq!(message.tool_calls().len(), if calls { 128 } else { 0 });
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn frame_limits_count_usage_and_ping_frames_without_content() {
    for kind in ["anthropic", "gemini"] {
        let body = if kind == "gemini" {
            sse(vec![json!({"usageMetadata":{}}); 8193])
        } else {
            sse(vec![json!({"type":"ping"}); 8193])
        };
        let (base, server) = bulk_server("text/event-stream", body).await;
        let events = collect(adapter(kind, &base).as_ref(), request(kind)).await;
        assert_error(&events);
        assert!(
            terminal(&events)
                .error_message
                .as_ref()
                .unwrap()
                .contains("frame limit")
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn wire_size_limits_stop_oversized_frames_json_and_cumulative_sse_bytes() {
    for (mode, body, expected) in [
        (
            GeminiMode::StreamGenerateContent,
            format!(":{}\n\n", "x".repeat(8 * 1024 * 1024)),
            "8 MiB limit",
        ),
        (
            GeminiMode::StreamGenerateContent,
            format!(":{}\n\n", "x".repeat(8192)).repeat(8192),
            "size limit",
        ),
        (
            GeminiMode::GenerateContent,
            " ".repeat(16 * 1024 * 1024 + 1),
            "size limit",
        ),
    ] {
        let mime = if mode == GeminiMode::GenerateContent {
            "application/json"
        } else {
            "text/event-stream"
        };
        let (base, server) = bulk_server(mime, body).await;
        let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(mode);
        let events = collect(&provider, request("gemini")).await;
        assert_error(&events);
        assert!(
            terminal(&events)
                .error_message
                .as_ref()
                .unwrap()
                .contains(expected)
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn gemini_generate_content_rejects_malformed_and_ambiguous_json_once() {
    for body in [
        "{broken".to_owned(),
        "{\"candidates\":[".to_owned(),
        "{\"candidates\":[],\"candidates\":[]}".to_owned(),
        "{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"weather\",\"args\":{\"city\":\"Paris\",\"city\":\"Tokyo\"}}}]},\"finishReason\":\"STOP\"}]}".to_owned(),
        json!({"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}).to_string() + " {}",
    ] {
        let (base, server) = bulk_server("application/json", body).await;
        let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(GeminiMode::GenerateContent);
        let events = collect(&provider, tools_request("gemini")).await;
        assert_error(&events);
        assert!(!events.iter().any(|event| matches!(event, AssistantEvent::ToolCallEnd { .. })));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn gemini_generate_content_maps_explicit_images_and_rejects_disabled_images() {
    let image = Content::Image {
        data: "aGVsbG8=".into(),
        mime_type: "image/png".into(),
    };
    let mut req = request("gemini");
    req.model.supports_images = true;
    req.messages.push(Message::User {
        content: vec![Content::text("describe"), image.clone()],
        timestamp: 0,
    });
    let body = json!({"candidates":[{"content":{"role":"model","parts":[{"text":"fixture image"}]},"finishReason":"STOP"}]}).to_string();
    let (base, rx, server) = http_server("200 OK", "application/json", body).await;
    let provider = GeminiProvider::new(base, KEY)
        .unwrap()
        .with_mode(GeminiMode::GenerateContent);
    assert_eq!(
        terminal(&collect(&provider, req.clone()).await).stop_reason,
        StopReason::Stop
    );
    let received = rx.await.unwrap();
    assert!(
        received
            .head
            .starts_with("POST /api/models/gemini-requested:generateContent HTTP/1.1")
    );
    assert_eq!(
        received.body["contents"][0]["parts"][2],
        json!({"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}})
    );
    server.await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = GeminiProvider::new(format!("http://{}", listener.local_addr().unwrap()), KEY)
        .unwrap()
        .with_mode(GeminiMode::GenerateContent);
    req.model.supports_images = false;
    assert_error(&collect(&provider, req).await);
    assert!(
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn gemini_generate_content_replays_paired_tool_results_with_original_ids() {
    let value = json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"native_1","name":"weather","args":{"city":"Paris"}}},{"functionCall":{"name":"weather","args":{"city":"Tokyo"}}}]},"finishReason":"STOP"}]});
    let (base, rx, server) = http_server("200 OK", "application/json", value.to_string()).await;
    let provider = GeminiProvider::new(base, KEY)
        .unwrap()
        .with_mode(GeminiMode::GenerateContent);
    let mut req = tools_request("gemini");
    let message = terminal(&collect(&provider, req.clone()).await).clone();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let calls = message.tool_calls();
    assert_eq!(calls[0].id, "native_1");
    assert_ne!(calls[0].id, calls[1].id);
    rx.await.unwrap();
    server.await.unwrap();
    req.messages.push(Message::Assistant(message));
    let mut error_result = tool_result(&calls[1].id, "weather");
    if let Message::ToolResult(result) = &mut error_result {
        result.is_error = true;
    }
    req.messages.push(error_result);
    req.messages.push(tool_result(&calls[0].id, "weather"));
    let value =
        json!({"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}]});
    let (base, rx, server) = http_server("200 OK", "application/json", value.to_string()).await;
    let provider = GeminiProvider::new(base, KEY)
        .unwrap()
        .with_mode(GeminiMode::GenerateContent);
    assert_eq!(
        terminal(&collect(&provider, req).await).stop_reason,
        StopReason::Stop
    );
    let received = rx.await.unwrap();
    assert!(
        received
            .head
            .starts_with("POST /api/models/gemini-requested:generateContent HTTP/1.1")
    );
    let body = received.body;
    assert_eq!(
        body["contents"][1]["parts"][0]["functionCall"]["id"],
        "native_1"
    );
    assert_eq!(
        body["contents"][1]["parts"][1]["functionCall"]["id"],
        calls[1].id
    );
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"],
        json!({"id":calls[1].id,"name":"weather","response":{"error":{"temperature":21},"text":"sunny"}})
    );
    assert_eq!(
        body["contents"][2]["parts"][1]["functionResponse"],
        json!({"id":"native_1","name":"weather","response":{"output":{"temperature":21},"text":"sunny"}})
    );
    server.await.unwrap();
}

#[test]
fn constructor_errors_do_not_echo_credentials_or_urls() {
    for (base, key) in [
        ("https://user:password@secret.invalid/api", KEY),
        ("https://secret.invalid/api?key=secret", KEY),
        ("https://secret.invalid/api#secret", KEY),
        ("file:///secret", KEY),
        ("https://secret.invalid/api", ""),
        ("https://secret.invalid/api", "secret\r\ninjection"),
    ] {
        for result in [
            AnthropicProvider::new(base, key).map(|_| ()),
            GeminiProvider::new(base, key).map(|_| ()),
        ] {
            let error = result
                .expect_err("invalid configuration accepted")
                .to_string();
            assert!(!error.contains("secret"));
            assert!(!error.contains(KEY));
            assert!(!error.contains(base));
        }
    }
}

#[tokio::test]
async fn registry_routes_exactly_and_unknown_provider_errors_without_fallback() {
    let (base, rx, server) = http_server("200 OK", "text/event-stream", gemini_text()).await;
    let mut registry = ProviderRegistry::new();
    registry
        .register("gemini", adapter("gemini", &base))
        .unwrap();
    assert_error(&collect(&registry, request("unknown")).await);
    assert_eq!(
        terminal(&collect(&registry, request("gemini")).await).stop_reason,
        StopReason::Stop
    );
    assert!(
        rx.await
            .unwrap()
            .head
            .contains("gemini-requested:streamGenerateContent")
    );
    server.await.unwrap();
}
