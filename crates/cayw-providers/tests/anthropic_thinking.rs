//! Synthetic Messages SSE over loopback HTTP; no model endpoint or real key is used.
use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, Model, ModelRequest,
    Provider, StopReason, StreamOptions, SystemMessage, ToolCall, ToolDefinition,
    ToolResultMessage, UsageStatus,
};
use cayw_providers::AnthropicProvider;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

const KEY: &str = "anthropic-synthetic-key";
const WATCHDOG: Duration = Duration::from_secs(20);

fn save(case: &str, name: &str, value: &impl serde::Serialize) {
    if let Some(root) = std::env::var_os("CAYW_ANTHROPIC_EVIDENCE_DIR") {
        let dir = PathBuf::from(root).join(case);
        std::fs::create_dir_all(&dir).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(name))
            .unwrap();
        serde_json::to_writer_pretty(file, value).unwrap();
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        model: Model::new("anthropic", "requested-model"),
        messages: vec![
            Message::System(SystemMessage {
                tools_added: vec![ToolDefinition {
                    name: "weather".into(),
                    description: "Read weather".into(),
                    parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}),
                }],
                ..Default::default()
            }),
            Message::user("Read the weather"),
        ],
        options: StreamOptions {
            max_tokens: Some(128),
            ..Default::default()
        },
    }
}

fn start() -> Value {
    json!({"type":"message_start","message":{"id":"msg-synthetic","type":"message","role":"assistant","model":"returned-model","content":[],"usage":{"input_tokens":7,"output_tokens":0}}})
}
fn block_start(index: usize, block: Value) -> Value {
    json!({"type":"content_block_start","index":index,"content_block":block})
}
fn delta(index: usize, delta: Value) -> Value {
    json!({"type":"content_block_delta","index":index,"delta":delta})
}
fn block_stop(index: usize) -> Value {
    json!({"type":"content_block_stop","index":index})
}
fn finish(reason: &str) -> Vec<Value> {
    vec![
        json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":11}}),
        json!({"type":"message_stop"}),
    ]
}
fn signed_prefix() -> Vec<Value> {
    vec![
        start(),
        block_start(0, json!({"type":"thinking","thinking":"","signature":""})),
        delta(0, json!({"type":"thinking_delta","thinking":""})),
        delta(
            0,
            json!({"type":"signature_delta","signature":"opaque-synthetic-signature+/="}),
        ),
        block_stop(0),
    ]
}
fn answer() -> Vec<Value> {
    let mut events = vec![
        start(),
        block_start(0, json!({"type":"text","text":"done"})),
        block_stop(0),
    ];
    events.extend(finish("end_turn"));
    events
}
fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|value| {
            format!(
                "event: {}\r\ndata: {value}\r\n\r\n",
                value["type"].as_str().unwrap()
            )
        })
        .collect::<String>()
        .into_bytes()
}
async fn read_request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(i) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let len: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < end + len {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
    }
    json!({"head":head,"body":serde_json::from_slice::<Value>(&bytes[end..end+len]).unwrap()})
}
async fn server(
    case: &str,
    events: Vec<Value>,
) -> (
    String,
    oneshot::Receiver<Value>,
    tokio::task::JoinHandle<()>,
) {
    let body = sse(&events);
    save(
        case,
        "response.json",
        &json!({"body_bytes":body,"events":events}),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, rx) = oneshot::channel();
    let case = case.to_owned();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let received = read_request(&mut socket).await;
        save(&case, "request.json", &received);
        tx.send(received).ok();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if socket.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        for chunk in body.chunks(7) {
            if socket.write_all(chunk).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    });
    (base, rx, task)
}
async fn collect(provider: &AnthropicProvider, request: ModelRequest) -> Vec<AssistantEvent> {
    timeout(
        WATCHDOG,
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
fn assert_error(events: &[AssistantEvent]) {
    assert_eq!(
        terminal(events).stop_reason,
        StopReason::Error,
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AssistantEvent::Done { .. }))
    );
}
fn result(id: &str, city: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "weather".into(),
        content: vec![Content::text(city)],
        details: None,
        structured_content: Some(json!({"city":city})),
        usage: None,
        is_error: false,
        outcome_unknown: false,
        timestamp: 0,
        duration_ms: None,
    })
}

// Rejecting an empty thinking string, exposing it as answer text or losing a
// signature must fail this round trip.
#[tokio::test]
async fn empty_signed_thinking_precedes_visible_text_without_becoming_an_answer() {
    let case = "empty-thinking";
    let mut wire = signed_prefix();
    wire.extend([
        block_start(1, json!({"type":"text","text":""})),
        delta(1, json!({"type":"text_delta","text":"你好"})),
        block_stop(1),
    ]);
    wire.extend(finish("end_turn"));
    let (base, rx, task) = server(case, wire).await;
    let events = collect(&AnthropicProvider::new(base, KEY).unwrap(), request()).await;
    save(case, "events.json", &events);
    let message = terminal(&events);
    assert_eq!(message.stop_reason, StopReason::Stop, "{events:?}");
    assert_eq!(message.content, vec![Content::text("你好")]);
    assert_eq!(message.response_id.as_deref(), Some("msg-synthetic"));
    assert_eq!(message.response_model.as_deref(), Some("returned-model"));
    assert_eq!(message.usage_status, UsageStatus::Reported);
    assert_eq!(message.usage.total_tokens, 18);
    assert_eq!(
        message.provider_metadata["anthropic"],
        json!({"content":[
            {"type":"thinking","thinking":"","signature":"opaque-synthetic-signature+/="},
            {"type":"text","text":"你好"}
        ]})
    );
    assert!(events.iter().all(|event| {
        !event
            .message()
            .content
            .iter()
            .any(|part| matches!(part, Content::Thinking { .. }))
    }));
    assert!(events.iter().any(|event| matches!(event, AssistantEvent::TextDelta { index: 0, delta, .. } if delta == "你好")));
    assert!(
        rx.await.unwrap()["head"]
            .as_str()
            .unwrap()
            .starts_with("POST /v1/messages HTTP/1.1")
    );
    task.await.unwrap();
}

#[tokio::test]
async fn signed_and_redacted_blocks_replay_in_order_after_serialization_and_tool_results() {
    let parts = json!([
        {"type":"thinking","thinking":"private 思考","signature":"signature-final"},
        {"type":"redacted_thinking","data":"opaque-encrypted-original"},
        {"type":"text","text":"Checking"},
        {"type":"tool_use","id":"call-paris","name":"weather","input":{"city":"Paris"},"caller":{"type":"direct"}},
        {"type":"tool_use","id":"call-tokyo","name":"weather","input":{"city":"Tokyo"}}
    ]);
    let mut wire = vec![
        start(),
        block_start(
            0,
            json!({"type":"thinking","thinking":"private ","signature":""}),
        ),
        delta(0, json!({"type":"thinking_delta","thinking":"思考"})),
        delta(
            0,
            json!({"type":"signature_delta","signature":"signature-final"}),
        ),
        block_stop(0),
        block_start(1, parts[1].clone()),
        block_stop(1),
        block_start(2, parts[2].clone()),
        block_stop(2),
        block_start(
            3,
            json!({"type":"tool_use","id":"call-paris","name":"weather","input":{},"caller":{"type":"direct"}}),
        ),
        delta(
            3,
            json!({"type":"input_json_delta","partial_json":"{\"city\":"}),
        ),
        delta(
            3,
            json!({"type":"input_json_delta","partial_json":"\"Paris\"}"}),
        ),
        block_stop(3),
        block_start(4, parts[4].clone()),
        block_stop(4),
    ];
    wire.extend(finish("tool_use"));
    let (base, rx, task) = server("signed-tools", wire).await;
    let events = collect(&AnthropicProvider::new(base, KEY).unwrap(), request()).await;
    save("signed-tools", "events.json", &events);
    let message = terminal(&events);
    assert_eq!(message.stop_reason, StopReason::ToolUse, "{events:?}");
    assert_eq!(
        message.content,
        vec![
            Content::text("Checking"),
            Content::ToolCall(ToolCall::new(
                "call-paris",
                "weather",
                json!({"city":"Paris"})
            )),
            Content::ToolCall(ToolCall::new(
                "call-tokyo",
                "weather",
                json!({"city":"Tokyo"})
            )),
        ]
    );
    assert_eq!(message.provider_metadata["anthropic"]["content"], parts);
    assert!(!cayw_ai::content_text(&message.content).contains("private"));
    rx.await.unwrap();
    task.await.unwrap();
    let restored: AssistantMessage =
        serde_json::from_slice(&serde_json::to_vec(message).unwrap()).unwrap();
    let mut req = request();
    req.messages.extend([
        Message::Assistant(restored),
        result("call-tokyo", "Tokyo"),
        result("call-paris", "Paris"),
    ]);
    let (base, rx, task) = server("signed-replay", answer()).await;
    let events = collect(&AnthropicProvider::new(base, KEY).unwrap(), req.clone()).await;
    save("signed-replay", "events.json", &events);
    assert_eq!(
        terminal(&events).stop_reason,
        StopReason::Stop,
        "{events:?}"
    );
    let sent = rx.await.unwrap();
    assert_eq!(sent["body"]["messages"][1]["content"], parts);
    assert_eq!(
        sent["body"]["messages"][2]["content"][0]["tool_use_id"],
        "call-tokyo"
    );
    assert_eq!(
        sent["body"]["messages"][2]["content"][1]["tool_use_id"],
        "call-paris"
    );
    task.await.unwrap();
    req.messages
        .push(Message::Assistant(terminal(&events).clone()));
    req.messages.push(Message::user("Continue"));
    let (base, rx, task) = server("signed-next-user", answer()).await;
    let events = collect(&AnthropicProvider::new(base, KEY).unwrap(), req).await;
    save("signed-next-user", "events.json", &events);
    assert_eq!(terminal(&events).stop_reason, StopReason::Stop);
    assert_eq!(rx.await.unwrap()["body"]["messages"][1]["content"], parts);
    task.await.unwrap();
}

#[tokio::test]
async fn signature_delta_replaces_the_previous_value_instead_of_concatenating() {
    let mut wire = vec![
        start(),
        block_start(
            0,
            json!({"type":"thinking","thinking":"private","signature":"initial"}),
        ),
        delta(
            0,
            json!({"type":"signature_delta","signature":"replacement"}),
        ),
        delta(
            0,
            json!({"type":"signature_delta","signature":"final-opaque"}),
        ),
        block_stop(0),
        block_start(1, json!({"type":"text","text":"answer"})),
        block_stop(1),
    ];
    wire.extend(finish("end_turn"));
    let (base, rx, task) = server("signature-replaces", wire).await;
    let events = collect(&AnthropicProvider::new(base, KEY).unwrap(), request()).await;
    save("signature-replaces", "events.json", &events);
    let message = terminal(&events);
    assert_eq!(message.stop_reason, StopReason::Stop, "{events:?}");
    assert_eq!(
        message.provider_metadata["anthropic"]["content"][0]["signature"],
        "final-opaque"
    );
    rx.await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn malformed_signed_blocks_and_incomplete_streams_never_succeed() {
    let open = block_start(
        0,
        json!({"type":"thinking","thinking":"private-secret","signature":""}),
    );
    let sign = delta(
        0,
        json!({"type":"signature_delta","signature":"opaque-signature"}),
    );
    let with_answer = |mut prefix: Vec<Value>| {
        prefix.extend([
            block_stop(0),
            block_start(1, json!({"type":"text","text":"visible"})),
            block_stop(1),
        ]);
        prefix.extend(finish("end_turn"));
        prefix
    };
    let mut cases = vec![
        with_answer(vec![start(), open.clone()]),
        with_answer(vec![
            start(),
            open.clone(),
            sign.clone(),
            delta(0, json!({"type":"signature_delta","signature":""})),
        ]),
        with_answer(vec![
            start(),
            open.clone(),
            sign.clone(),
            delta(
                0,
                json!({"type":"thinking_delta","thinking":"changed-after-signature"}),
            ),
        ]),
        vec![start(), open.clone(), block_stop(0)],
        vec![start(), open.clone(), sign.clone()],
        signed_prefix(),
        vec![
            start(),
            open.clone(),
            sign.clone(),
            delta(
                0,
                json!({"type":"thinking_delta","thinking":"changed-after-signature"}),
            ),
        ],
        vec![
            start(),
            open.clone(),
            delta(0, json!({"type":"text_delta","text":"wrong-type"})),
        ],
        vec![
            start(),
            open.clone(),
            delta(0, json!({"type":"signature_delta","signature":7})),
        ],
        vec![
            start(),
            open.clone(),
            delta(
                1,
                json!({"type":"signature_delta","signature":"wrong-index"}),
            ),
        ],
        vec![
            start(),
            open.clone(),
            block_start(1, json!({"type":"text","text":"overlap"})),
        ],
        vec![
            start(),
            block_start(1, json!({"type":"text","text":"skipped-index"})),
            block_stop(1),
        ],
        vec![
            start(),
            block_start(0, json!({"type":"thinking","thinking":"x"})),
        ],
        vec![
            start(),
            block_start(0, json!({"type":"redacted_thinking","data":null})),
        ],
        vec![
            start(),
            block_start(0, json!({"type":"redacted_thinking","data":"opaque"})),
            sign.clone(),
        ],
        vec![
            start(),
            block_start(
                0,
                json!({"type":"server_tool_use","id":"server","name":"unregistered","input":{}}),
            ),
        ],
    ];
    let mut only_thinking = signed_prefix();
    only_thinking.extend(finish("end_turn"));
    cases.push(only_thinking);
    for (index, wire) in cases.into_iter().enumerate() {
        let case = format!("invalid-{index:02}");
        let (base, rx, task) = server(&case, wire).await;
        let events = collect(&AnthropicProvider::new(base, KEY).unwrap(), request()).await;
        save(&case, "events.json", &events);
        assert_error(&events);
        let error = terminal(&events).error_message.as_deref().unwrap();
        assert!(
            !error.contains("private-secret")
                && !error.contains("opaque-signature")
                && !error.contains(KEY)
        );
        rx.await.unwrap();
        task.await.unwrap();
    }
}

#[tokio::test]
async fn inconsistent_or_foreign_metadata_is_rejected_before_network() {
    let mut assistant = AssistantMessage::new(&request().model);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![
        Content::text("visible"),
        Content::ToolCall(ToolCall::new("call-1", "weather", json!({"city":"Paris"}))),
    ];
    let original = json!({"content":[
        {"type":"thinking","thinking":"private","signature":"opaque"},
        {"type":"redacted_thinking","data":"encrypted"},
        {"type":"text","text":"visible"},
        {"type":"tool_use","id":"call-1","name":"weather","input":{"city":"Paris"}}
    ]});
    let mut cases = Vec::new();
    for mutation in 0..13 {
        let mut value = original.clone();
        match mutation {
            0 => value["content"][2]["text"] = json!("changed"),
            1 => value["content"][3]["id"] = json!("changed"),
            2 => value["content"][3]["input"] = json!({"city":"Tokyo"}),
            3 => value["content"][0]["signature"] = Value::Null,
            4 => value["content"][0]["signature"] = json!(""),
            5 => value["content"][1]["data"] = json!(""),
            6 => value["content"].as_array_mut().unwrap().swap(2, 3),
            7 => value["extra"] = json!(true),
            8 => value["content"][0]["extra"] = json!(true),
            9 => {
                value["content"][3]["caller"] =
                    json!({"type":"server_tool","tool_id":"unregistered"})
            }
            10 => value["content"] = json!([]),
            11 => value = Value::Null,
            12 => value["content"][3]["name"] = json!("changed"),
            _ => unreachable!(),
        }
        let mut invalid = assistant.clone();
        invalid.provider_metadata.insert("anthropic".into(), value);
        cases.push(invalid);
    }
    let mut foreign = assistant.clone();
    foreign
        .provider_metadata
        .insert("gemini".into(), original.clone());
    cases.push(foreign);
    let mut mixed = assistant;
    mixed.provider_metadata.insert("anthropic".into(), original);
    mixed
        .provider_metadata
        .insert("openai_responses".into(), json!({"output":[]}));
    cases.push(mixed);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider =
        AnthropicProvider::new(format!("http://{}", listener.local_addr().unwrap()), KEY).unwrap();
    for (index, assistant) in cases.into_iter().enumerate() {
        let mut req = request();
        req.messages
            .extend([Message::Assistant(assistant), result("call-1", "Paris")]);
        let events = timeout(WATCHDOG, async {
            tokio::select! {
                biased;
                events = collect(&provider, req) => events,
                connection = listener.accept() => panic!("invalid metadata opened a socket: {connection:?}"),
            }
        }).await.unwrap();
        save(
            &format!("invalid-history-{index:02}"),
            "events.json",
            &events,
        );
        assert_error(&events);
    }
}

#[tokio::test]
async fn cancellation_and_drop_after_signed_blocks_release_the_socket() {
    for drop_instead in [false, true] {
        let case = if drop_instead {
            "drop-thinking"
        } else {
            "cancel-thinking"
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut prefix = signed_prefix();
        prefix.extend([
            block_start(1, json!({"type":"text","text":"checkpoint"})),
            block_stop(1),
        ]);
        let body = sse(&prefix);
        save(
            case,
            "response.json",
            &json!({"body_bytes":body,"events":prefix}),
        );
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let received = read_request(&mut socket).await;
            save(case, "request.json", &received);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            socket
                .write_all(format!("{:x}\r\n", body.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(&body).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            ready_tx.send(()).ok();
            let mut buffer = [0; 1];
            let result = timeout(WATCHDOG, socket.read(&mut buffer)).await;
            assert!(
                matches!(result, Ok(Ok(0)) | Ok(Err(_))),
                "socket retained: {result:?}"
            );
            save(case, "closed.json", &json!({"socket_closed":true}));
        });
        let cancel = CancellationToken::new();
        let provider = AnthropicProvider::new(base, KEY).unwrap();
        let mut stream = provider.stream(request(), cancel.clone());
        assert!(matches!(
            stream.next().await,
            Some(AssistantEvent::Start { .. })
        ));
        assert!(matches!(
            stream.next().await,
            Some(AssistantEvent::UsageUpdate { .. })
        ));
        let mut observed = Vec::new();
        loop {
            let event = timeout(WATCHDOG, stream.next()).await.unwrap().unwrap();
            let ended = matches!(event, AssistantEvent::TextEnd { .. });
            assert!(
                !event.is_terminal(),
                "signed block failed before cancellation: {event:?}"
            );
            observed.push(event);
            if ended {
                break;
            }
        }
        assert_eq!(
            observed.last().unwrap().message().provider_metadata["anthropic"]["content"][0]["signature"],
            "opaque-synthetic-signature+/="
        );
        save(case, "prefix-events.json", &observed);
        timeout(WATCHDOG, ready_rx).await.unwrap().unwrap();
        if drop_instead {
            drop(stream);
        } else {
            cancel.cancel();
            let events: Vec<_> = timeout(WATCHDOG, stream.collect()).await.unwrap();
            save(case, "events.json", &events);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].message().stop_reason, StopReason::Aborted);
            assert!(!matches!(events[0], AssistantEvent::Done { .. }));
        }
        timeout(WATCHDOG, task).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn empty_thinking_deltas_still_obey_the_shared_fragment_limit() {
    let mut wire = vec![
        start(),
        block_start(0, json!({"type":"thinking","thinking":"","signature":""})),
    ];
    wire.extend((0..4096).map(|_| delta(0, json!({"type":"thinking_delta","thinking":""}))));
    let (base, rx, task) = server("thinking-fragment-limit", wire).await;
    let events = collect(&AnthropicProvider::new(base, KEY).unwrap(), request()).await;
    save("thinking-fragment-limit", "events.json", &events);
    assert_error(&events);
    assert!(
        terminal(&events)
            .error_message
            .as_deref()
            .unwrap()
            .contains("fragment limit")
    );
    rx.await.unwrap();
    task.await.unwrap();
}
