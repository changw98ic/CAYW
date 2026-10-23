//! Native Responses SSE over real loopback sockets. All inputs and keys are synthetic.
use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, Model, ModelRequest,
    Provider, StopReason, StreamOptions, SystemMessage, ToolDefinition, ToolResultMessage,
    UsageStatus,
};
use cayw_providers::ResponsesProvider;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const KEY: &str = "responses-synthetic-key";
const WATCHDOG: Duration = Duration::from_secs(20);

fn save(case: &str, name: &str, value: &impl serde::Serialize) {
    if let Some(root) = std::env::var_os("CAYW_RESPONSES_EVIDENCE_DIR") {
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
        model: Model::new("responses", "requested-model"),
        messages: vec![
            Message::System(SystemMessage {
                content: "Use the declared tool.".into(),
                tools_added: vec![ToolDefinition {
                    name: "weather".into(),
                    description: "Read weather".into(),
                    parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}),
                }],
                ..Default::default()
            }),
            Message::user("Weather in Paris and 北京?"),
        ],
        options: StreamOptions {
            max_tokens: Some(128),
            temperature: Some(0.5),
            ..Default::default()
        },
    }
}

fn usage() -> Value {
    json!({"input_tokens":12,"input_tokens_details":{"cached_tokens":3},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":19})
}
fn response(id: &str, status: &str, output: Vec<Value>, usage: Value) -> Value {
    json!({"id":id,"object":"response","model":"returned-model","status":status,"output":output,"usage":usage,"error":null,"incomplete_details":null})
}
fn created() -> Value {
    json!({"type":"response.created","response":response("resp-native", "in_progress", vec![], Value::Null)})
}
fn call_item(id: &str, call_id: &str, arguments: &str) -> Value {
    json!({"id":id,"type":"function_call","call_id":call_id,"name":"weather","arguments":arguments,"status":"completed"})
}
fn call_added(index: usize, id: &str, call_id: &str) -> Value {
    let mut item = call_item(id, call_id, "");
    item["status"] = json!("in_progress");
    json!({"type":"response.output_item.added","output_index":index,"item":item})
}
fn argument_delta(index: usize, id: &str, delta: &str) -> Value {
    json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":id,"delta":delta})
}
fn call_done(index: usize, id: &str, call_id: &str, arguments: &str) -> Vec<Value> {
    vec![
        json!({"type":"response.function_call_arguments.done","output_index":index,"item_id":id,"arguments":arguments}),
        json!({"type":"response.output_item.done","output_index":index,"item":call_item(id,call_id,arguments)}),
    ]
}
fn calls_wire() -> Vec<Value> {
    let a = r#"{"city":"Paris"}"#;
    let b = r#"{"city":"北京"}"#;
    let mut events = vec![
        created(),
        call_added(0, "fc-first", "call-first"),
        call_added(1, "fc-second", "call-second"),
        argument_delta(1, "fc-second", r#"{"city":"#),
        argument_delta(0, "fc-first", r#"{"city":"#),
        argument_delta(1, "fc-second", "\"北京\"}"),
        argument_delta(0, "fc-first", "\"Paris\"}"),
    ];
    events.extend(call_done(1, "fc-second", "call-second", b));
    events.extend(call_done(0, "fc-first", "call-first", a));
    events.push(json!({"type":"response.completed","response":response("resp-native","completed",vec![call_item("fc-first","call-first",a),call_item("fc-second","call-second",b)],usage())}));
    events
}
fn text_item(text: &str) -> Value {
    json!({"id":"msg-native","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[],"logprobs":[]}]})
}
fn text_wire() -> Vec<Value> {
    vec![
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg-native","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"item_id":"msg-native","content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg-native","content_index":0,"delta":"你"}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg-native","content_index":0,"delta":"好"}),
        json!({"type":"response.output_text.done","output_index":0,"item_id":"msg-native","content_index":0,"text":"你好"}),
        json!({"type":"response.content_part.done","output_index":0,"item_id":"msg-native","content_index":0,"part":{"type":"output_text","text":"你好","annotations":[],"logprobs":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":text_item("你好")}),
        json!({"type":"response.completed","response":response("resp-native","completed",vec![text_item("你好")],usage())}),
    ]
}
fn reasoning_item() -> Value {
    json!({
        "id":"rs-native",
        "type":"reasoning",
        "summary":[],
        "content":[{"type":"reasoning_text","text":"opaque thought"}],
        "encrypted_content":"opaque-encrypted-original",
        "status":"completed"
    })
}
fn reasoning_added(index: usize) -> Value {
    json!({"type":"response.output_item.added","output_index":index,"item":{"id":"rs-native","type":"reasoning","summary":[]}})
}
fn reasoning_wire() -> Vec<Value> {
    let reasoning = reasoning_item();
    let mut events = vec![
        created(),
        reasoning_added(0),
        json!({"type":"response.content_part.added","output_index":0,"item_id":"rs-native","content_index":0,"part":{"type":"reasoning_text","text":""}}),
        json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"rs-native","content_index":0,"delta":"opaque thought"}),
        json!({"type":"response.reasoning_text.done","output_index":0,"item_id":"rs-native","content_index":0,"text":"opaque thought"}),
        json!({"type":"response.content_part.done","output_index":0,"item_id":"rs-native","content_index":0,"part":{"type":"reasoning_text","text":"opaque thought"}}),
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning.clone()}),
    ];
    let mut text = text_wire();
    text.remove(0);
    text.pop();
    for event in &mut text {
        event["output_index"] = json!(1);
    }
    events.extend(text);
    events.push(json!({
        "type":"response.completed",
        "response":response("resp-native","completed",vec![reasoning.clone(),text_item("你好")],usage())
    }));
    events
}
fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|v| {
            format!(
                "event: {}\r\ndata: {v}\r\n\r\n",
                v["type"].as_str().unwrap()
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
        if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
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
    status: &str,
    mime: &str,
    body: Vec<u8>,
    chunk_size: usize,
) -> (
    String,
    oneshot::Receiver<Value>,
    tokio::task::JoinHandle<()>,
) {
    save(
        case,
        "response.json",
        &json!({"status":status,"mime":mime,"body_utf8":String::from_utf8_lossy(&body),"body_bytes":body}),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, rx) = oneshot::channel();
    let status = status.to_owned();
    let mime = mime.to_owned();
    let case = case.to_owned();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let received = read_request(&mut socket).await;
        save(&case, "request.json", &received);
        tx.send(received).ok();
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if socket.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        for chunk in body.chunks(chunk_size) {
            if socket.write_all(chunk).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    });
    (base, rx, task)
}
async fn run(case: &str, wire: Vec<Value>) -> (Vec<AssistantEvent>, Value) {
    let (base, received, server) = server(case, "200 OK", "text/event-stream", sse(&wire), 3).await;
    let events: Vec<_> = tokio::time::timeout(
        WATCHDOG,
        ResponsesProvider::new(base, KEY)
            .unwrap()
            .stream(request(), CancellationToken::new())
            .collect(),
    )
    .await
    .unwrap();
    save(case, "events.json", &events);
    assert!(matches!(events.first(), Some(AssistantEvent::Start { .. })));
    assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
    let received = tokio::time::timeout(WATCHDOG, received)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(WATCHDOG, server)
        .await
        .unwrap()
        .unwrap();
    (events, received)
}
fn terminal(events: &[AssistantEvent]) -> &AssistantMessage {
    assert!(events.last().unwrap().is_terminal());
    assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
    events.last().unwrap().message()
}
fn assert_error(events: &[AssistantEvent]) {
    assert!(
        matches!(events.last(), Some(AssistantEvent::Error { .. })),
        "{events:?}"
    );
    assert_eq!(terminal(events).stop_reason, StopReason::Error);
    let text = terminal(events).error_message.as_ref().unwrap();
    assert!(!text.contains(KEY));
    assert!(!text.contains("https://"));
}
fn assert_error_contains(events: &[AssistantEvent], expected: &str) {
    assert_error(events);
    let text = terminal(events).error_message.as_ref().unwrap();
    assert!(text.contains(expected), "expected {expected:?} in {text:?}");
}

#[tokio::test]
async fn native_arguments_keep_distinct_item_and_call_ids_and_exact_fragments() {
    let (events, received) = run("native-calls", calls_wire()).await;
    let final_message = terminal(&events);
    assert!(
        matches!(events.last(), Some(AssistantEvent::Done { .. })),
        "{events:?}"
    );
    assert_eq!(final_message.stop_reason, StopReason::ToolUse);
    assert_eq!(final_message.model, "requested-model");
    assert_eq!(
        final_message.response_model.as_deref(),
        Some("returned-model")
    );
    assert_eq!(final_message.response_id.as_deref(), Some("resp-native"));
    let calls = final_message.tool_calls();
    assert_eq!(
        calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["call-first", "call-second"]
    );
    assert_eq!(calls[0].name, "weather");
    assert_eq!(calls[0].arguments, json!({"city":"Paris"}));
    assert_eq!(calls[1].arguments, json!({"city":"北京"}));
    let deltas: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let AssistantEvent::ToolCallDelta { index, delta, .. } = e {
                Some((*index, delta.as_str()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        deltas,
        [
            (1, r#"{"city":"#),
            (0, r#"{"city":"#),
            (1, "\"北京\"}"),
            (0, "\"Paris\"}")
        ]
    );
    for index in 0..2 {
        assert_eq!(
            events
                .iter()
                .filter(
                    |e| matches!(e, AssistantEvent::ToolCallStart { index: i, .. } if *i == index)
                )
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(
                    |e| matches!(e, AssistantEvent::ToolCallEnd { index: i, .. } if *i == index)
                )
                .count(),
            1
        );
    }
    assert!(
        received["head"]
            .as_str()
            .unwrap()
            .starts_with("POST /v1/responses HTTP/1.1")
    );
    assert!(
        received["head"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer responses-synthetic-key")
    );
    assert_eq!(
        received["body"],
        json!({"model":"requested-model","input":[{"role":"user","content":[{"type":"input_text","text":"Weather in Paris and 北京?"}]}],"instructions":"Use the declared tool.","tools":[{"type":"function","name":"weather","description":"Read weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false},"strict":false}],"stream":true,"store":false,"include":["reasoning.encrypted_content"],"max_output_tokens":128,"temperature":0.5})
    );
}

#[tokio::test]
async fn native_text_and_nested_usage_preserve_original_counters() {
    let (events, _) = run("native-text", text_wire()).await;
    let message = terminal(&events);
    assert!(
        matches!(events.last(), Some(AssistantEvent::Done { .. })),
        "{events:?}"
    );
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.content, vec![Content::text("你好")]);
    assert_eq!(message.usage_status, UsageStatus::Reported);
    assert_eq!(
        (
            message.usage.input,
            message.usage.cache_read,
            message.usage.cache_write,
            message.usage.output,
            message.usage.reasoning,
            message.usage.total_tokens
        ),
        (9, 3, 0, 7, 2, 19)
    );
    assert_eq!(
        serde_json::to_value(&message.usage_fields).unwrap(),
        usage()
    );
    let last_usage = events
        .iter()
        .rev()
        .find_map(|event| {
            if let AssistantEvent::UsageUpdate { partial } = event {
                Some(partial)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(last_usage.usage, message.usage);
    assert_eq!(last_usage.response_id, message.response_id);
    assert!(!events.iter().any(|e| matches!(
        e,
        AssistantEvent::ThinkingStart { .. }
            | AssistantEvent::ThinkingDelta { .. }
            | AssistantEvent::ThinkingEnd { .. }
    )));
}

#[tokio::test]
async fn reasoning_text_content_parts_stay_private_and_replay_the_original_item() {
    let (events, _) = run("reasoning-text-parts", reasoning_wire()).await;
    let message = terminal(&events).clone();
    assert!(
        matches!(events.last(), Some(AssistantEvent::Done { .. })),
        "{events:?}"
    );
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.content, vec![Content::text("你好")]);
    assert_eq!(
        message.provider_metadata["openai_responses"],
        json!({"output":[reasoning_item(), text_item("你好")]})
    );
    assert!(!events.iter().any(|e| matches!(
        e,
        AssistantEvent::ThinkingStart { .. }
            | AssistantEvent::ThinkingDelta { .. }
            | AssistantEvent::ThinkingEnd { .. }
    )));
    let visible: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let AssistantEvent::TextDelta { delta, .. } = event {
                Some(delta.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(visible, ["你", "好"]);

    let encoded = serde_json::to_vec(&message).unwrap();
    let message: AssistantMessage = serde_json::from_slice(&encoded).unwrap();
    let mut follow = request();
    follow
        .messages
        .extend([Message::Assistant(message), Message::user("Continue")]);
    let (base, received, task) = server(
        "reasoning-text-parts-second",
        "200 OK",
        "text/event-stream",
        sse(&text_wire()),
        5,
    )
    .await;
    let events: Vec<_> = ResponsesProvider::new(base, KEY)
        .unwrap()
        .stream(follow, CancellationToken::new())
        .collect()
        .await;
    save("reasoning-text-parts-second", "events.json", &events);
    assert!(
        matches!(events.last(), Some(AssistantEvent::Done { .. })),
        "{events:?}"
    );
    let received = received.await.unwrap();
    task.await.unwrap();
    assert_eq!(received["body"]["input"][1], reasoning_item());
    assert_eq!(received["body"]["input"][2], text_item("你好"));
}

#[tokio::test]
async fn reasoning_text_without_content_parts_stays_private_and_accepted() {
    let mut wire = reasoning_wire();
    wire.remove(5);
    wire.remove(2);
    let (events, _) = run("reasoning-text-without-parts", wire).await;
    let message = terminal(&events);
    assert!(
        matches!(events.last(), Some(AssistantEvent::Done { .. })),
        "{events:?}"
    );
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.content, vec![Content::text("你好")]);
    assert_eq!(
        message.provider_metadata["openai_responses"],
        json!({"output":[reasoning_item(), text_item("你好")]})
    );
    assert!(!events.iter().any(|e| matches!(
        e,
        AssistantEvent::ThinkingStart { .. }
            | AssistantEvent::ThinkingDelta { .. }
            | AssistantEvent::ThinkingEnd { .. }
    )));
}

#[tokio::test]
async fn reasoning_text_content_parts_require_matching_identity_order_and_completion() {
    for (case, expected) in [
        (
            "wrong-item",
            "Responses content part item ID does not match its output index",
        ),
        (
            "wrong-output",
            "Responses content part references an unknown output item",
        ),
        (
            "wrong-index",
            "Responses reasoning content parts must be added in order",
        ),
        (
            "wrong-type-on-reasoning",
            "Unsupported Responses content part for a reasoning item",
        ),
        (
            "reasoning-on-message",
            "Unsupported Responses content part for a message item",
        ),
        (
            "done-before-added",
            "Responses reasoning content part done references an unknown content part",
        ),
        (
            "added-after-text",
            "Responses reasoning content part was added after its text streamed",
        ),
        (
            "duplicate-part-done",
            "Duplicate Responses reasoning content part done event",
        ),
        (
            "item-before-part-done",
            "Responses reasoning item ended before its content part",
        ),
        (
            "part-text-mismatch",
            "Responses reasoning content part done does not match its deltas",
        ),
        (
            "item-content-mismatch",
            "Responses done reasoning text does not match its deltas",
        ),
        (
            "delta-wrong-item",
            "Responses reasoning text delta item ID does not match its output index",
        ),
        (
            "delta-after-done",
            "Responses reasoning text delta arrived after its done event",
        ),
        (
            "missing-encrypted",
            "Responses reasoning item has no encrypted content",
        ),
    ] {
        let mut wire = reasoning_wire();
        match case {
            "wrong-item" => wire[2]["item_id"] = json!("msg-native"),
            "wrong-output" => wire[2]["output_index"] = json!(1),
            "wrong-index" => wire[2]["content_index"] = json!(1),
            "wrong-type-on-reasoning" => wire[2]["part"]["type"] = json!("output_text"),
            "reasoning-on-message" => wire[8]["part"]["type"] = json!("reasoning_text"),
            "done-before-added" => {
                wire.swap(2, 5);
            }
            "added-after-text" => {
                let added = wire.remove(2);
                wire.insert(4, added);
            }
            "duplicate-part-done" => {
                let done = wire[5].clone();
                wire.insert(6, done);
            }
            "item-before-part-done" => {
                wire.remove(5);
            }
            "part-text-mismatch" => wire[5]["part"]["text"] = json!("other"),
            "item-content-mismatch" => wire[6]["item"]["content"][0]["text"] = json!("other"),
            "delta-wrong-item" => wire[3]["item_id"] = json!("msg-native"),
            "delta-after-done" => {
                let delta = wire[3].clone();
                wire.insert(5, delta);
            }
            "missing-encrypted" => {
                wire[6]["item"]
                    .as_object_mut()
                    .unwrap()
                    .remove("encrypted_content");
            }
            _ => unreachable!(),
        }
        let (events, _) = run(&format!("reasoning-text-reject-{case}"), wire).await;
        assert_error_contains(&events, expected);
    }
}

#[tokio::test]
async fn failed_incomplete_and_missing_completion_never_succeed_with_complete_calls() {
    for kind in ["response.failed", "response.incomplete", "missing"] {
        let mut wire = calls_wire();
        wire.pop();
        if kind != "missing" {
            let mut value = response(
                "resp-native",
                if kind == "response.failed" {
                    "failed"
                } else {
                    "incomplete"
                },
                vec![],
                usage(),
            );
            if kind == "response.failed" {
                value["error"] =
                    json!({"code":"server_error","message":"https://secret.example/key"});
            } else {
                value["incomplete_details"] = json!({"reason":"max_output_tokens"});
            }
            wire.push(json!({"type":kind,"response":value}));
        }
        let (events, _) = run(&format!("terminal-{kind}"), wire).await;
        assert_error(&events);
        assert_eq!(
            terminal(&events).response_id.as_deref(),
            Some("resp-native")
        );
        if kind != "missing" {
            assert_eq!(terminal(&events).usage_status, UsageStatus::Reported);
        }
    }
}

#[tokio::test]
async fn malformed_identity_arguments_and_final_output_are_rejected() {
    for case in [
        "item-id",
        "call-id",
        "duplicate-call",
        "undeclared",
        "done-args",
        "final-output",
        "response-id",
        "model",
        "duplicate-json",
        "schema",
        "missing-item-done",
        "event-type",
    ] {
        let mut wire = calls_wire();
        match case {
            "item-id" => wire[3]["item_id"] = json!("fc-first"),
            "call-id" => wire[8]["item"]["call_id"] = json!("changed"),
            "duplicate-call" => wire[2]["item"]["call_id"] = json!("call-first"),
            "undeclared" => wire[1]["item"]["name"] = json!("shell"),
            "done-args" => wire[7]["arguments"] = json!(r#"{"city":"other"}"#),
            "final-output" => {
                wire.last_mut().unwrap()["response"]["output"][0]["arguments"] = json!("{}")
            }
            "response-id" => wire.last_mut().unwrap()["response"]["id"] = json!("other-response"),
            "model" => wire.last_mut().unwrap()["response"]["model"] = json!("other-model"),
            "duplicate-json" => {
                wire[3]["delta"] = json!(r#"{"city":"x","city":"#);
            }
            "schema" => {
                wire[3]["delta"] = json!(r#"{"number":"#);
                wire[7]["arguments"] = json!(r#"{"number":"北京"}"#);
            }
            "missing-item-done" => {
                wire.remove(8);
            }
            "event-type" => wire[3]["type"] = json!("response.output_text.delta"),
            _ => unreachable!(),
        }
        let (events, _) = run(&format!("reject-{case}"), wire).await;
        assert_error(&events);
    }
}

#[tokio::test]
async fn cache_write_tokens_are_subtracted_once_and_kept_in_raw_usage() {
    let raw = json!({
        "input_tokens":100,
        "input_tokens_details":{"cached_tokens":10,"cache_write_tokens":20},
        "output_tokens":10,
        "output_tokens_details":{"reasoning_tokens":0},
        "total_tokens":110
    });
    let mut wire = text_wire();
    wire.last_mut().unwrap()["response"]["usage"] = raw.clone();
    let (events, _) = run("usage-cache-write", wire).await;
    let message = terminal(&events);
    assert!(
        matches!(events.last(), Some(AssistantEvent::Done { .. })),
        "{events:?}"
    );
    assert_eq!(message.usage_status, UsageStatus::Reported);
    assert_eq!(message.usage.input, 70);
    assert_eq!(message.usage.cache_read, 10);
    assert_eq!(message.usage.cache_write, 20);
    assert_eq!(message.usage.output, 10);
    assert_eq!(message.usage.reasoning, 0);
    assert_eq!(message.usage.total_tokens, 110);
    assert_eq!(
        message.usage.input
            + message.usage.cache_read
            + message.usage.cache_write
            + message.usage.output,
        message.usage.total_tokens
    );
    assert_eq!(serde_json::to_value(&message.usage_fields).unwrap(), raw);
}

#[tokio::test]
async fn invalid_usage_is_retained_as_invalid_instead_of_reported_zero() {
    for (case, u) in [
        (
            "negative",
            json!({"input_tokens":-1,"output_tokens":7,"total_tokens":6}),
        ),
        (
            "cache",
            json!({"input_tokens":2,"input_tokens_details":{"cached_tokens":3},"output_tokens":7,"total_tokens":9}),
        ),
        (
            "reasoning",
            json!({"input_tokens":2,"output_tokens":1,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":3}),
        ),
        (
            "cache-write",
            json!({"input_tokens":2,"input_tokens_details":{"cache_write_tokens":3},"output_tokens":7,"total_tokens":9}),
        ),
        (
            "cache-combined",
            json!({"input_tokens":20,"input_tokens_details":{"cached_tokens":15,"cache_write_tokens":10},"output_tokens":10,"total_tokens":30}),
        ),
        (
            "cache-overflow",
            json!({"input_tokens":u64::MAX,"input_tokens_details":{"cached_tokens":u64::MAX,"cache_write_tokens":1},"output_tokens":0,"total_tokens":u64::MAX}),
        ),
        (
            "cache-write-without-input",
            json!({"input_tokens_details":{"cache_write_tokens":1},"output_tokens":1,"total_tokens":1}),
        ),
        (
            "total",
            json!({"input_tokens":12,"output_tokens":7,"total_tokens":18}),
        ),
    ] {
        let mut wire = text_wire();
        wire.last_mut().unwrap()["response"]["usage"] = u.clone();
        let (events, _) = run(&format!("usage-invalid-{case}"), wire).await;
        assert_error(&events);
        assert_eq!(terminal(&events).usage_status, UsageStatus::Invalid);
        assert_eq!(
            serde_json::to_value(&terminal(&events).usage_fields).unwrap(),
            u
        );
    }
}

#[tokio::test]
async fn absent_partial_and_explicit_zero_usage_remain_distinct() {
    for (case, u, status) in [
        ("absent", Value::Null, UsageStatus::Unknown),
        ("partial", json!({"input_tokens":4}), UsageStatus::Partial),
        (
            "zero",
            json!({"input_tokens":0,"output_tokens":0,"total_tokens":0}),
            UsageStatus::Reported,
        ),
    ] {
        let mut wire = text_wire();
        wire.last_mut().unwrap()["response"]["usage"] = u;
        let (events, _) = run(&format!("usage-{case}"), wire).await;
        assert!(
            matches!(events.last(), Some(AssistantEvent::Done { .. })),
            "{events:?}"
        );
        assert_eq!(terminal(&events).usage_status, status);
    }
}

#[tokio::test]
async fn stream_error_and_refusal_do_not_echo_remote_payload_or_thinking() {
    for (case, replacement) in [
        (
            "error",
            json!({"type":"error","code":"bad","message":"https://secret.example/key"}),
        ),
        (
            "refusal",
            json!({"type":"response.content_part.added","output_index":0,"item_id":"msg-native","content_index":0,"part":{"type":"refusal","refusal":"https://secret.example/key"}}),
        ),
    ] {
        let mut wire = text_wire();
        wire.truncate(2);
        wire.push(replacement);
        let (events, _) = run(&format!("reject-{case}"), wire).await;
        assert_error(&events);
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("secret.example")
        );
    }
}

#[tokio::test]
async fn cancellation_and_dropping_release_open_http_sockets() {
    tokio::time::timeout(WATCHDOG, async {
        for drop_stream in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            let (ready, ready_rx) = oneshot::channel();
            let case = if drop_stream { "drop" } else { "cancel" };
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let received = read_request(&mut socket).await;
                save(case, "request.json", &received);
                let body = sse(&text_wire()[..4]);
                save(case, "response.json", &json!({"mime":"text/event-stream","body_utf8":String::from_utf8_lossy(&body),"unfinished":true}));
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                socket.write_all(format!("{:x}\r\n", body.len()).as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
                socket.write_all(b"\r\n").await.unwrap();
                ready.send(()).unwrap();
                let mut byte = [0];
                match socket.read(&mut byte).await { Ok(0)|Err(_) => {}, other => panic!("socket remained active: {other:?}") }
            });
            let cancel = CancellationToken::new();
            let mut stream = ResponsesProvider::new(base, KEY).unwrap().stream(request(), cancel.clone());
            let mut events = Vec::new();
            loop {
                let event = stream.next().await.unwrap();
                let text = matches!(&event, AssistantEvent::TextDelta { .. });
                assert!(!event.is_terminal(), "unexpected early terminal: {event:?}");
                events.push(event);
                if text { break; }
            }
            ready_rx.await.unwrap();
            if drop_stream { drop(stream); }
            else {
                cancel.cancel();
                events.extend(stream.collect::<Vec<_>>().await);
                assert_eq!(terminal(&events).stop_reason, StopReason::Aborted);
                assert!(!events.iter().any(|e| matches!(e, AssistantEvent::Done { .. })));
            }
            save(case, "events.json", &events);
            server.await.unwrap();
        }
    }).await.expect("socket cleanup watchdog");
}

#[tokio::test]
async fn complete_response_stops_before_late_garbage_and_releases_socket() {
    for chunk in [1, 7, 8192] {
        let case = format!("late-{chunk}");
        let mut body = sse(&text_wire());
        body.extend_from_slice(b"data: \xff\n\n");
        let (base, _, server) = server(&case, "200 OK", "text/event-stream", body, chunk).await;
        let events: Vec<_> = tokio::time::timeout(
            WATCHDOG,
            ResponsesProvider::new(base, KEY)
                .unwrap()
                .stream(request(), CancellationToken::new())
                .collect(),
        )
        .await
        .unwrap();
        save(&case, "events.json", &events);
        assert!(
            matches!(events.last(), Some(AssistantEvent::Done { .. })),
            "{events:?}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn response_fragment_limit_stops_empty_argument_flood() {
    let mut wire = vec![created(), call_added(0, "fc-flood", "call-flood")];
    for _ in 0..4097 {
        wire.push(argument_delta(0, "fc-flood", ""));
    }
    let (events, _) = run("fragment-limit", wire).await;
    assert_error(&events);
    assert!(
        terminal(&events)
            .error_message
            .as_ref()
            .unwrap()
            .contains("limit")
    );
}

fn result(call_id: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: call_id.into(),
        tool_name: "weather".into(),
        content: vec![Content::text("result text")],
        details: None,
        structured_content: Some(json!({"temperature":21})),
        usage: None,
        is_error: false,
        outcome_unknown: false,
        timestamp: 0,
        duration_ms: None,
    })
}

#[tokio::test]
async fn encrypted_reasoning_replays_original_items_without_thinking_events() {
    let reasoning = json!({"id":"rs-native","type":"reasoning","summary":[],"encrypted_content":"opaque-encrypted-original"});
    let mut wire = vec![
        created(),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs-native","type":"reasoning","summary":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
    ];
    let mut text = text_wire();
    text.remove(0);
    text.pop();
    for event in &mut text {
        event["output_index"] = json!(1);
    }
    wire.extend(text);
    wire.push(json!({"type":"response.completed","response":response("resp-native","completed",vec![reasoning.clone(),text_item("你好")],usage())}));
    let (events, _) = run("encrypted-first", wire).await;
    let message = terminal(&events).clone();
    assert!(
        matches!(events.last(), Some(AssistantEvent::Done { .. })),
        "{events:?}"
    );
    assert_eq!(
        message.provider_metadata["openai_responses"],
        json!({"output":[reasoning.clone(),text_item("你好")]})
    );
    assert!(!events.iter().any(|e| matches!(
        e,
        AssistantEvent::ThinkingStart { .. }
            | AssistantEvent::ThinkingDelta { .. }
            | AssistantEvent::ThinkingEnd { .. }
    )));
    let encoded = serde_json::to_vec(&message).unwrap();
    let message = serde_json::from_slice(&encoded).unwrap();
    let mut follow = request();
    follow
        .messages
        .extend([Message::Assistant(message), Message::user("Continue")]);
    let (base, received, task) = server(
        "encrypted-second",
        "200 OK",
        "text/event-stream",
        sse(&text_wire()),
        5,
    )
    .await;
    let events: Vec<_> = ResponsesProvider::new(base, KEY)
        .unwrap()
        .stream(follow, CancellationToken::new())
        .collect()
        .await;
    assert!(
        matches!(events.last(), Some(AssistantEvent::Done { .. })),
        "{events:?}"
    );
    let received = received.await.unwrap();
    assert_eq!(received["body"]["input"][1], reasoning);
    assert_eq!(received["body"]["input"][2], text_item("你好"));
    task.await.unwrap();
}

#[tokio::test]
async fn lazy_invalid_and_cancelled_requests_never_open_a_socket() {
    tokio::time::timeout(WATCHDOG, async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let provider = ResponsesProvider::new(base, KEY).unwrap();
        let stream = provider.stream(request(), CancellationToken::new());
        drop(stream);
        for case in [
            "empty-model",
            "thinking",
            "schema",
            "image",
            "unpaired",
            "unknown",
            "metadata",
            "cancelled",
        ] {
            let mut bad = request();
            let cancel = CancellationToken::new();
            match case {
                "empty-model" => bad.model.id.clear(),
                "thinking" => bad.options.temperature = Some(f64::NAN),
                "schema" => {
                    if let Message::System(system) = &mut bad.messages[0] {
                        system.tools_added[0].parameters =
                            json!({"type":"object","$ref":"https://external.example/schema"});
                    }
                }
                "image" => {
                    if let Message::User { content, .. } = &mut bad.messages[1] {
                        *content = vec![Content::Image {
                            data: "not base64".into(),
                            mime_type: "image/png".into(),
                        }];
                    }
                }
                "unpaired" => bad.messages.push(result("missing")),
                "unknown" => {
                    let Message::ToolResult(mut result) = result("missing") else {
                        unreachable!()
                    };
                    result.outcome_unknown = true;
                    bad.messages.push(Message::ToolResult(result));
                }
                "metadata" => {
                    let mut assistant = AssistantMessage::new(&bad.model);
                    assistant.content.push(Content::text("previous"));
                    assistant.stop_reason = StopReason::Stop;
                    assistant
                        .provider_metadata
                        .insert("gemini".into(), json!({"parts":[]}));
                    bad.messages
                        .extend([Message::Assistant(assistant), Message::user("next")]);
                }
                "cancelled" => cancel.cancel(),
                _ => unreachable!(),
            }
            let events: Vec<_> = provider.stream(bad, cancel).collect().await;
            save(&format!("preflight-{case}"), "events.json", &events);
            assert_eq!(events.len(), 2, "{case}: {events:?}");
            assert!(
                matches!(events.last(), Some(AssistantEvent::Error { .. })),
                "{case}: {events:?}"
            );
        }
        // Accept must still be pending when an independently scheduled barrier runs.
        tokio::select! {
            biased;
            connection = listener.accept() => panic!("preflight made a request: {connection:?}"),
            _ = tokio::task::yield_now() => {},
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn replay_rejects_missing_or_contradictory_original_output_before_http() {
    let (events, _) = run("replay-invalid-source", calls_wire()).await;
    let original = terminal(&events).clone();
    for case in [
        "missing",
        "call-id",
        "item-id",
        "arguments",
        "extra",
        "other-key",
        "duplicate-id",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let mut assistant = original.clone();
        match case {
            "missing" => assistant.provider_metadata.clear(),
            "call-id" => {
                assistant
                    .provider_metadata
                    .get_mut("openai_responses")
                    .unwrap()["output"][0]["call_id"] = json!("other")
            }
            "item-id" => {
                assistant
                    .provider_metadata
                    .get_mut("openai_responses")
                    .unwrap()["output"][0]["id"] = Value::Null
            }
            "arguments" => {
                assistant
                    .provider_metadata
                    .get_mut("openai_responses")
                    .unwrap()["output"][0]["arguments"] = json!(r#"{"city":"other"}"#)
            }
            "extra" => assistant
                .provider_metadata
                .get_mut("openai_responses")
                .unwrap()["output"]
                .as_array_mut()
                .unwrap()
                .push(text_item("extra")),
            "other-key" => {
                assistant
                    .provider_metadata
                    .insert("gemini".into(), json!({}));
            }
            "duplicate-id" => {
                assistant
                    .provider_metadata
                    .get_mut("openai_responses")
                    .unwrap()["output"][1]["id"] = json!("fc-first")
            }
            _ => unreachable!(),
        }
        let mut follow = request();
        follow.messages.extend([
            Message::Assistant(assistant),
            result("call-first"),
            result("call-second"),
        ]);
        let events: Vec<_> = tokio::time::timeout(
            WATCHDOG,
            ResponsesProvider::new(base, KEY)
                .unwrap()
                .stream(follow, CancellationToken::new())
                .collect(),
        )
        .await
        .unwrap();
        save(&format!("replay-invalid-{case}"), "events.json", &events);
        assert_error(&events);
        tokio::select! {
            biased;
            connection = listener.accept() => panic!("invalid replay made a request: {connection:?}"),
            _ = tokio::task::yield_now() => {},
        }
    }
}

#[tokio::test]
async fn raw_calls_and_paired_outputs_round_trip_through_second_native_request() {
    let (events, _) = run("roundtrip-first", calls_wire()).await;
    let assistant = terminal(&events).clone();
    assert_eq!(assistant.stop_reason, StopReason::ToolUse);
    let mut follow = request();
    follow.messages.extend([
        Message::Assistant(assistant),
        result("call-first"),
        result("call-second"),
    ]);
    let (base, received, task) = server(
        "roundtrip-second",
        "200 OK",
        "text/event-stream",
        sse(&text_wire()),
        3,
    )
    .await;
    let events: Vec<_> = tokio::time::timeout(
        WATCHDOG,
        ResponsesProvider::new(base, KEY)
            .unwrap()
            .stream(follow, CancellationToken::new())
            .collect(),
    )
    .await
    .unwrap();
    save("roundtrip-second", "events.json", &events);
    assert!(
        matches!(events.last(), Some(AssistantEvent::Done { .. })),
        "{events:?}"
    );
    let received = received.await.unwrap();
    task.await.unwrap();
    let items = received["body"]["input"].as_array().unwrap();
    assert_eq!(
        items[1],
        call_item("fc-first", "call-first", r#"{"city":"Paris"}"#)
    );
    assert_eq!(
        items[2],
        call_item("fc-second", "call-second", r#"{"city":"北京"}"#)
    );
    for (index, id) in [(3, "call-first"), (4, "call-second")] {
        assert_eq!(items[index]["type"], "function_call_output");
        assert_eq!(items[index]["call_id"], id);
        assert_eq!(
            items[index]["output"],
            json!([{"type":"input_text","text":"result text"},{"type":"input_text","text":"{\"temperature\":21}"}])
        );
    }
}
