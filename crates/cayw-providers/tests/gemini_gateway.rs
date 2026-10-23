use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, Model, ModelRequest,
    Provider, StopReason, StreamOptions, SystemMessage, ToolCall, ToolDefinition,
    ToolResultMessage, UsageStatus,
};
use cayw_providers::{GeminiMode, GeminiProvider};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};

const MODES: [GeminiMode; 2] = [
    GeminiMode::StreamGenerateContent,
    GeminiMode::GenerateContent,
];
const KEY: &str = "gemini-fixture-key";

struct Received {
    head: String,
    body: Value,
}

async fn read_request(socket: &mut TcpStream) -> Received {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 2048];
        let n = socket.read(&mut buffer).await.unwrap();
        assert!(n > 0, "connection closed before request headers");
        bytes.extend_from_slice(&buffer[..n]);
        if let Some(at) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
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
        let mut buffer = [0; 2048];
        let n = socket.read(&mut buffer).await.unwrap();
        assert!(n > 0, "connection closed before request body");
        bytes.extend_from_slice(&buffer[..n]);
    }
    Received {
        head,
        body: serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap(),
    }
}

async fn server(
    mode: GeminiMode,
    frames: Vec<Value>,
) -> (
    String,
    oneshot::Receiver<Received>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1beta", listener.local_addr().unwrap());
    let (tx, rx) = oneshot::channel();
    let (mime, body) = if mode == GeminiMode::GenerateContent {
        assert_eq!(frames.len(), 1);
        ("application/json", frames[0].to_string())
    } else {
        (
            "text/event-stream",
            frames
                .iter()
                .map(|value| format!("data: {value}\r\n\r\n"))
                .collect(),
        )
    };
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        tx.send(read_request(&mut socket).await).ok();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        // Exercise UTF-8 and frame decoding independently of TCP write boundaries.
        for chunk in body.as_bytes().chunks(7) {
            if socket.write_all(chunk).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    });
    (base, rx, task)
}

fn request() -> ModelRequest {
    ModelRequest {
        model: Model::new("gemini", "gemini-fixture"),
        messages: vec![
            Message::System(SystemMessage {
                tools_added: vec![ToolDefinition {
                    name: "weather".into(),
                    description: "Get weather".into(),
                    parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}),
                }],
                ..Default::default()
            }),
            Message::user("查两地天气"),
        ],
        options: StreamOptions::default(),
    }
}

fn result(id: &str, city: &str, is_error: bool) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "weather".into(),
        content: vec![Content::text(city)],
        details: None,
        structured_content: Some(json!({"city":city})),
        usage: None,
        is_error,
        outcome_unknown: false,
        timestamp: 0,
        duration_ms: None,
    })
}

async fn collect(provider: &GeminiProvider, request: ModelRequest) -> Vec<AssistantEvent> {
    timeout(
        Duration::from_secs(10),
        provider.stream(request, CancellationToken::new()).collect(),
    )
    .await
    .unwrap()
}

fn terminal(events: &[AssistantEvent]) -> &AssistantMessage {
    assert!(matches!(events.first(), Some(AssistantEvent::Start { .. })));
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    assert!(events.last().unwrap().is_terminal());
    events.last().unwrap().message()
}

fn answer() -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":[{"text":"完成"}]},"finishReason":"STOP"}]})
}

// Dropping signatures, moving them to merged text, or copying generated local IDs
// onto the wire must fail this real HTTP round trip in both REST modes.
#[tokio::test]
async fn signed_parts_round_trip_without_changing_signatures_or_missing_call_ids() {
    let parts = json!([
        {"text":"先检查 ","thoughtSignature":"c2lnLXRleHQrLw=="},
        {"text":"两地天气"},
        {"functionCall":{"name":"weather","args":{"city":"Paris"}},"thoughtSignature":"c2lnLWNhbGwrLw=="},
        {"functionCall":{"id":"native_tokyo","name":"weather","args":{"city":"Tokyo"}}},
        {"text":"调用已提交"},
        {"text":"","thoughtSignature":"c2lnLWVtcHR5Lw=="}
    ]);
    for mode in MODES {
        let frames = if mode == GeminiMode::GenerateContent {
            vec![
                json!({"responseId":"signed-response","modelVersion":"gemini-returned","candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":4,"totalTokenCount":11}}),
            ]
        } else {
            let mut frames: Vec<_> = parts
                .as_array()
                .unwrap()
                .iter()
                .map(|part| json!({"responseId":"signed-response","modelVersion":"gemini-returned","candidates":[{"content":{"role":"model","parts":[part]}}]}))
                .collect();
            frames.push(json!({"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":4,"totalTokenCount":11}}));
            frames
        };
        let (base, received, task) = server(mode, frames).await;
        let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(mode);
        let mut req = request();
        let events = collect(&provider, req.clone()).await;
        let message = terminal(&events);
        assert_eq!(
            message.stop_reason,
            StopReason::ToolUse,
            "{mode:?}: {events:?}"
        );
        let calls = message.tool_calls();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].id.is_empty());
        assert_ne!(calls[0].id, "native_tokyo");
        assert_eq!(calls[1].id, "native_tokyo");
        assert_eq!(message.content[0], Content::text("先检查 两地天气"));
        assert_eq!(message.content[3], Content::text("调用已提交"));
        assert_eq!(
            message.provider_metadata["gemini"],
            json!({"parts":parts,"call_ids":[calls[0].id,"native_tokyo"]})
        );
        assert_eq!(message.usage_status, UsageStatus::Reported);
        assert_eq!(message.usage.total_tokens, 11);
        assert!(!events.iter().any(|event| matches!(
            event,
            AssistantEvent::ThinkingStart { .. }
                | AssistantEvent::ThinkingDelta { .. }
                | AssistantEvent::ThinkingEnd { .. }
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantEvent::ToolCallEnd { .. }))
                .count(),
            2
        );
        received.await.unwrap();
        task.await.unwrap();

        // Recreate persisted history, then deliver same-name results in reverse order.
        let persisted = serde_json::from_value(serde_json::to_value(message).unwrap()).unwrap();
        req.messages.push(Message::Assistant(persisted));
        req.messages.push(result("native_tokyo", "Tokyo", true));
        req.messages.push(result(&calls[0].id, "Paris", false));
        let (base, received, task) = server(mode, vec![answer()]).await;
        let provider = GeminiProvider::new(base, KEY).unwrap().with_mode(mode);
        let events = collect(&provider, req).await;
        assert_eq!(
            terminal(&events).stop_reason,
            StopReason::Stop,
            "{events:?}"
        );
        let body = received.await.unwrap().body;
        assert_eq!(body["contents"][1], json!({"role":"model","parts":parts}));
        assert_eq!(
            body["contents"][2],
            json!({"role":"user","parts":[
                {"functionResponse":{"name":"weather","response":{"output":{"city":"Paris"},"text":"Paris"}}},
                {"functionResponse":{"id":"native_tokyo","name":"weather","response":{"error":{"city":"Tokyo"},"text":"Tokyo"}}}
            ]})
        );
        assert!(
            !body
                .to_string()
                .contains("skip_thought_signature_validator")
        );
        task.await.unwrap();
    }
}

fn history(metadata: Value) -> ModelRequest {
    let mut req = request();
    let mut assistant = AssistantMessage::new(&req.model);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![
        Content::text("before"),
        Content::ToolCall(ToolCall::new("native", "weather", json!({"city":"Paris"}))),
    ];
    assistant
        .provider_metadata
        .insert("gemini".into(), metadata);
    req.messages.push(Message::Assistant(assistant));
    req.messages.push(result("native", "Paris", false));
    req
}

async fn assert_preflight_error(mode: GeminiMode, request: ModelRequest) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = GeminiProvider::new(format!("http://{}", listener.local_addr().unwrap()), KEY)
        .unwrap()
        .with_mode(mode);
    let events = timeout(Duration::from_secs(10), async {
        tokio::select! {
            biased;
            accepted = listener.accept() => panic!("invalid history opened a connection: {accepted:?}"),
            events = collect(&provider, request) => events,
        }
    }).await.unwrap();
    let message = terminal(&events);
    assert_eq!(message.stop_reason, StopReason::Error, "{events:?}");
    let error = message.error_message.as_deref().unwrap();
    assert!(!error.contains(KEY));
    assert!(!error.contains("http://"));
}

// Ignoring metadata or trusting it without comparing to the typed history makes
// these requests open a socket, which this test catches before any response.
#[tokio::test]
async fn unknown_or_altered_gemini_metadata_is_rejected_before_network() {
    let valid = json!({"parts":[
        {"text":"before","thoughtSignature":"c2lnLXRleHQ="},
        {"functionCall":{"id":"native","name":"weather","args":{"city":"Paris"}},"thoughtSignature":"c2lnLWNhbGw="}
    ],"call_ids":["native"]});
    let mut malformed = vec![
        Value::Null,
        json!({}),
        json!({"parts":valid["parts"]}),
        json!({"parts":valid["parts"],"call_ids":null}),
        json!({"parts":[],"call_ids":[]}),
        json!({"parts":valid["parts"],"call_ids":[]}),
        json!({"parts":valid["parts"],"call_ids":["altered"]}),
        json!({"parts":valid["parts"],"call_ids":["native","extra"]}),
        json!({"parts":valid["parts"],"call_ids":[7]}),
    ];
    for (path, replacement) in [
        (vec!["unknown"], json!(true)),
        (vec!["parts", "0", "text"], json!("altered")),
        (vec!["parts", "0", "thought"], json!(true)),
        (vec!["parts", "0", "thoughtSignature"], json!(7)),
        (vec!["parts", "0", "thoughtSignature"], Value::Null),
        (vec!["parts", "0", "thoughtSignature"], json!("")),
        (
            vec!["parts", "0", "thoughtSignature"],
            json!("skip_thought_signature_validator"),
        ),
        (vec!["parts", "1", "functionCall", "id"], json!("different")),
        (vec!["parts", "1", "functionCall", "name"], json!("other")),
        (
            vec!["parts", "1", "functionCall", "args"],
            json!({"city":"Tokyo"}),
        ),
        (vec!["parts", "1", "functionCall", "partialArgs"], json!([])),
        (
            vec!["parts", "1", "inlineData"],
            json!({"mimeType":"image/png","data":"AA=="}),
        ),
    ] {
        let mut value = valid.clone();
        let mut target = &mut value;
        for field in path {
            target = if let Ok(index) = field.parse::<usize>() {
                &mut target[index]
            } else {
                &mut target[field]
            };
        }
        *target = replacement;
        malformed.push(value);
    }
    for mode in MODES {
        for value in &malformed {
            assert_preflight_error(mode, history(value.clone())).await;
        }
        let mut req = history(valid.clone());
        if let Message::Assistant(assistant) = &mut req.messages[2] {
            assistant
                .provider_metadata
                .insert("unknown".into(), json!({}));
        }
        assert_preflight_error(mode, req).await;
    }
}

// A gateway route must be explicit and must never change the request URL's structure.
#[tokio::test]
async fn routed_model_paths_require_gateway_opt_in_and_keep_native_authentication() {
    for mode in MODES {
        for id in [
            "antigravity/gemini-3.8-flash",
            "models/antigravity/gemini-3.8-flash",
        ] {
            let (base, received, task) = server(mode, vec![answer()]).await;
            let provider = GeminiProvider::new(base, KEY)
                .unwrap()
                .with_gateway_routes()
                .with_mode(mode);
            let mut req = request();
            req.model = Model::new("magpie", id);
            let events = collect(&provider, req).await;
            assert_eq!(
                terminal(&events).stop_reason,
                StopReason::Stop,
                "{events:?}"
            );
            let received = received.await.unwrap();
            let method = if mode == GeminiMode::GenerateContent {
                "generateContent"
            } else {
                "streamGenerateContent?alt=sse"
            };
            assert!(
                received.head.starts_with(&format!(
                    "POST /v1beta/models/antigravity/gemini-3.8-flash:{method} HTTP/1.1"
                )),
                "{}",
                received.head
            );
            assert!(
                received
                    .head
                    .to_ascii_lowercase()
                    .contains("x-goog-api-key: gemini-fixture-key")
            );
            assert!(!received.head.contains("?key="));
            assert!(
                received.body["generationConfig"]
                    .get("candidateCount")
                    .is_none(),
                "Gemini 3 requests must omit the unsupported candidate count: {}",
                received.body
            );
            task.await.unwrap();
        }
        for gateway in [false, true] {
            for id in [
                "../model",
                "route/../model",
                "route//model",
                "route/model/extra",
                "route/model%2fother",
                "route/model:generateContent",
                "/route/model",
                "route/model?x",
                "route/model#x",
                "route/.",
                "route/..",
            ] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let mut provider =
                    GeminiProvider::new(format!("http://{}", listener.local_addr().unwrap()), KEY)
                        .unwrap()
                        .with_mode(mode);
                if gateway {
                    provider = provider.with_gateway_routes();
                }
                let mut req = request();
                req.model.id = id.into();
                tokio::select! {
                    biased;
                    accepted = listener.accept() => panic!("invalid model opened a connection: {accepted:?}"),
                    events = collect(&provider, req) => assert_eq!(terminal(&events).stop_reason, StopReason::Error, "{id}: {events:?}"),
                }
            }
        }
        let mut req = request();
        req.model.id = "antigravity/gemini-3.8-flash".into();
        assert_preflight_error(mode, req).await;
    }
}

// Without original parts the adapter cannot tell native IDs from locally generated IDs.
#[tokio::test]
async fn tool_history_without_original_parts_is_rejected_before_network() {
    for mode in MODES {
        let mut req = history(json!({"parts":[],"call_ids":[]}));
        if let Message::Assistant(assistant) = &mut req.messages[2] {
            assistant.provider_metadata.clear();
        }
        assert_preflight_error(mode, req).await;
    }
}
