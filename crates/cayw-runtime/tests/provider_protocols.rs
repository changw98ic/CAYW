//! Synthetic loopback HTTP protocols through the public Runtime and real Host.
//! Re-run with CAYW_RUNTIME_PROTOCOL_EVIDENCE_DIR set to a new directory to keep
//! requests, responses, Runtime snapshots, events, tasks and the actual database.
use cayw_agent::AgentEvent;
use cayw_ai::{CancellationToken, Message, Model, Provider, StopReason, ToolCall};
use cayw_cells::{CellPage, CellStatus};
use cayw_core::{CapabilityOutcome, CoreEvent, EventRecord, ExecutionState, SessionState};
use cayw_providers::{AnthropicProvider, GeminiMode, GeminiProvider};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, SessionView, TaskState};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const KEY: &str = "local-protocol-fixture-key";
const MODEL: &str = "protocol-fixture-model";
const PROMPTS: [&str; 2] = [
    "Write the first fixture file and read it through execute_code.",
    "Read the first fixture file, then write and read the second fixture file.",
];
const FILES: [&str; 2] = ["first.txt", "second.txt"];
const CONTENTS: [&str; 2] = ["first fixture 你好", "second fixture 已完成"];
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
enum Protocol {
    OpenAi,
    Anthropic,
    Gemini(GeminiMode),
}

#[derive(Clone, Copy, Debug)]
struct Profile {
    name: &'static str,
    provider: &'static str,
    protocol: Protocol,
}

impl Profile {
    fn adapter(self, base: &str) -> Arc<dyn Provider> {
        match self.protocol {
            Protocol::OpenAi => {
                Arc::new(cayw_ai::providers::OpenAiProvider::new(base, KEY).unwrap())
            }
            Protocol::Anthropic => Arc::new(AnthropicProvider::new(base, KEY).unwrap()),
            Protocol::Gemini(mode) => {
                Arc::new(GeminiProvider::new(base, KEY).unwrap().with_mode(mode))
            }
        }
    }

    fn endpoint(self) -> String {
        match self.protocol {
            Protocol::OpenAi => "/v1/chat/completions".into(),
            Protocol::Anthropic => "/v1/messages".into(),
            Protocol::Gemini(GeminiMode::StreamGenerateContent) => {
                format!("/v1/models/{MODEL}:streamGenerateContent?alt=sse")
            }
            Protocol::Gemini(GeminiMode::GenerateContent) => {
                format!("/v1/models/{MODEL}:generateContent")
            }
        }
    }
}

struct Evidence {
    root: PathBuf,
    _temporary: Option<tempfile::TempDir>,
}

impl Evidence {
    fn new(profile: Profile) -> Self {
        let (root, temporary) = match std::env::var_os("CAYW_RUNTIME_PROTOCOL_EVIDENCE_DIR") {
            Some(parent) => {
                let parent = PathBuf::from(parent);
                assert!(parent.is_absolute(), "evidence directory must be absolute");
                fs::create_dir_all(&parent).unwrap();
                let root = parent.join(profile.name);
                fs::create_dir(&root)
                    .expect("preserve previous protocol evidence; use a new directory");
                (root, None)
            }
            None => {
                let temporary = tempfile::tempdir().unwrap();
                (temporary.path().to_path_buf(), Some(temporary))
            }
        };
        Self {
            root,
            _temporary: temporary,
        }
    }
}

fn save(root: &Path, name: &str, value: Value) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(name))
        .unwrap();
    serde_json::to_writer_pretty(&mut file, &value).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
}

fn script(round: usize) -> String {
    let previous = if round == 1 {
        "local previous = cayw.call('fs.read', {path='first.txt'})\n"
    } else {
        ""
    };
    let returned = if round == 1 {
        "previous=previous, "
    } else {
        ""
    };
    format!(
        "{previous}cayw.call('fs.write', {{path='{}', content='{}'}})\nyield_control()\nreturn {{{returned}current=cayw.call('fs.read', {{path='{}'}})}}",
        FILES[round], CONTENTS[round], FILES[round],
    )
}

fn exec_arguments(round: usize) -> Value {
    json!({"action":"exec", "script":script(round), "yield_time_ms":1000, "max_output_tokens":16384})
}

fn final_text(round: usize, content: &str) -> String {
    format!("Confirmed user round {}: {content}", round + 1)
}

#[derive(Clone, Debug, PartialEq)]
enum WireItem {
    User(String),
    Text(String),
    Opaque(Value),
    Call(ToolCall),
    Result { id: String, page: Value },
}

#[derive(Default)]
struct Trace {
    requests: Vec<Vec<WireItem>>,
    calls: Vec<ToolCall>,
}

fn text_parts(parts: &Value) -> Vec<String> {
    parts
        .as_array()
        .unwrap()
        .iter()
        .map(|part| {
            assert_eq!(part["type"], "text");
            part["text"].as_str().unwrap().to_owned()
        })
        .collect()
}

fn wire_items(profile: Profile, body: &Value) -> Vec<WireItem> {
    let mut items = Vec::new();
    let field = if matches!(profile.protocol, Protocol::Gemini(_)) {
        "contents"
    } else {
        "messages"
    };
    for message in body[field].as_array().unwrap() {
        match profile.protocol {
            Protocol::OpenAi => match message["role"].as_str().unwrap() {
                "system" => continue,
                "user" => items.extend(
                    text_parts(&message["content"])
                        .into_iter()
                        .map(WireItem::User),
                ),
                "assistant" => {
                    if let Some(text) = message["content"].as_str() {
                        items.push(WireItem::Text(text.into()));
                    }
                    if let Some(calls) = message["tool_calls"].as_array() {
                        for call in calls {
                            assert_eq!(call["type"], "function");
                            items.push(WireItem::Call(ToolCall::new(
                                call["id"].as_str().unwrap(),
                                call["function"]["name"].as_str().unwrap(),
                                serde_json::from_str::<Value>(
                                    call["function"]["arguments"].as_str().unwrap(),
                                )
                                .unwrap(),
                            )));
                        }
                    }
                }
                "tool" => {
                    let texts = text_parts(&message["content"]);
                    assert_eq!(
                        texts.len(),
                        2,
                        "text and structured result must both survive"
                    );
                    assert_eq!(
                        serde_json::from_str::<Value>(&texts[0]).unwrap(),
                        serde_json::from_str::<Value>(&texts[1]).unwrap(),
                    );
                    items.push(WireItem::Result {
                        id: message["tool_call_id"].as_str().unwrap().into(),
                        page: serde_json::from_str(&texts[0]).unwrap(),
                    });
                }
                role => panic!("unexpected OpenAI role {role}"),
            },
            Protocol::Anthropic => {
                for part in message["content"].as_array().unwrap() {
                    match part["type"].as_str().unwrap() {
                        "text" => {
                            let text = part["text"].as_str().unwrap().to_owned();
                            items.push(if message["role"] == "user" {
                                WireItem::User(text)
                            } else {
                                WireItem::Text(text)
                            });
                        }
                        "thinking" | "redacted_thinking" => {
                            items.push(WireItem::Opaque(part.clone()));
                        }
                        "tool_use" => items.push(WireItem::Call(ToolCall::new(
                            part["id"].as_str().unwrap(),
                            part["name"].as_str().unwrap(),
                            part["input"].clone(),
                        ))),
                        "tool_result" => {
                            assert_eq!(part["is_error"], false);
                            let pages: Vec<Value> = text_parts(&part["content"])
                                .iter()
                                .map(|text| serde_json::from_str(text).unwrap())
                                .collect();
                            assert_eq!(
                                pages.len(),
                                2,
                                "text and structured result must both survive"
                            );
                            assert_eq!(pages[0], pages[1]);
                            items.push(WireItem::Result {
                                id: part["tool_use_id"].as_str().unwrap().into(),
                                page: pages[0].clone(),
                            });
                        }
                        kind => panic!("unexpected Anthropic part {kind}"),
                    }
                }
            }
            Protocol::Gemini(_) => {
                for part in message["parts"].as_array().unwrap() {
                    if let Some(text) = part["text"].as_str() {
                        items.push(if message["role"] == "user" {
                            WireItem::User(text.into())
                        } else {
                            WireItem::Text(text.into())
                        });
                    } else if let Some(call) = part.get("functionCall") {
                        items.push(WireItem::Call(ToolCall::new(
                            call["id"].as_str().unwrap(),
                            call["name"].as_str().unwrap(),
                            call["args"].clone(),
                        )));
                    } else {
                        let result = &part["functionResponse"];
                        assert_eq!(result["name"], "execute_code");
                        let page = result["response"]["output"].clone();
                        assert_eq!(
                            serde_json::from_str::<Value>(
                                result["response"]["text"].as_str().unwrap()
                            )
                            .unwrap(),
                            page
                        );
                        items.push(WireItem::Result {
                            id: result["id"].as_str().unwrap().into(),
                            page,
                        });
                    }
                }
            }
        }
    }
    items
}

async fn read_request(socket: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).await.unwrap();
        assert!(size > 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..size]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        assert!(
            bytes.len() <= 16 * 1024,
            "request headers exceed fixture limit"
        );
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    assert!(
        length <= 2 * 1024 * 1024,
        "request body exceeds fixture limit"
    );
    while bytes.len() < header_end + length {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).await.unwrap();
        assert!(size > 0, "request ended before body");
        bytes.extend_from_slice(&buffer[..size]);
    }
    (
        head,
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
    )
}

fn check_headers(profile: Profile, head: &str, body: &Value) -> String {
    assert!(head.starts_with(&format!("POST {} HTTP/1.1\r\n", profile.endpoint())));
    let (credential, expected) = match profile.protocol {
        Protocol::OpenAi => ("authorization", format!("Bearer {KEY}")),
        Protocol::Anthropic => ("x-api-key", KEY.to_owned()),
        Protocol::Gemini(_) => ("x-goog-api-key", KEY.to_owned()),
    };
    let mut redacted = Vec::new();
    let mut authenticated = false;
    for line in head.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case(credential)
        {
            assert_eq!(value.trim(), expected);
            authenticated = true;
            redacted.push(format!("{name}: [redacted fixture key]"));
        } else {
            redacted.push(line.to_owned());
        }
    }
    assert!(authenticated);
    assert!(!head.lines().next().unwrap().contains(KEY));
    let tool = match profile.protocol {
        Protocol::OpenAi => {
            assert_eq!(body["model"], MODEL);
            assert_eq!(body["stream"], true);
            assert_eq!(body["stream_options"]["include_usage"], true);
            &body["tools"][0]["function"]
        }
        Protocol::Anthropic => {
            assert!(
                head.to_lowercase()
                    .contains("anthropic-version: 2023-06-01")
            );
            assert_eq!(body["model"], MODEL);
            assert!(body["max_tokens"].as_u64().unwrap() > 0);
            &body["tools"][0]
        }
        Protocol::Gemini(_) => &body["tools"][0]["functionDeclarations"][0],
    };
    assert_eq!(tool["name"], "execute_code");
    redacted.join("\r\n")
}

fn sse(values: Vec<Value>, named: bool) -> String {
    values
        .into_iter()
        .map(|value| {
            if named {
                format!(
                    "event: {}\r\ndata: {value}\r\n\r\n",
                    value["type"].as_str().unwrap()
                )
            } else {
                format!("data: {value}\r\n\r\n")
            }
        })
        .collect()
}

fn response(
    profile: Profile,
    sequence: usize,
    call: Option<&ToolCall>,
    text: &str,
) -> (&'static str, String) {
    let id = format!("{}-response-{sequence}", profile.name);
    let arguments = call
        .map(|call| call.arguments.to_string())
        .unwrap_or_default();
    let mut middle = arguments.len() / 2;
    while !arguments.is_char_boundary(middle) {
        middle -= 1;
    }
    let (first, second) = arguments.split_at(middle);
    match profile.protocol {
        Protocol::OpenAi => {
            let chunk = |delta: Value, finish: Value| json!({"id":id,"model":MODEL,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
            let mut values = if let Some(call) = call {
                vec![
                    chunk(
                        json!({"role":"assistant","tool_calls":[{"index":0,"id":call.id,"type":"function","function":{"name":call.name,"arguments":first}}]}),
                        Value::Null,
                    ),
                    chunk(
                        json!({"tool_calls":[{"index":0,"function":{"arguments":second}}]}),
                        Value::Null,
                    ),
                ]
            } else {
                vec![chunk(
                    json!({"role":"assistant","content":text}),
                    Value::Null,
                )]
            };
            values.push(chunk(
                json!({}),
                json!(if call.is_some() { "tool_calls" } else { "stop" }),
            ));
            values.push(json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}));
            (
                "text/event-stream",
                sse(values, false) + "data: [DONE]\r\n\r\n",
            )
        }
        Protocol::Anthropic => {
            let mut values = vec![
                json!({"type":"message_start","message":{"id":id,"model":MODEL,"type":"message","role":"assistant","content":[],"usage":{"input_tokens":11,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":format!("synthetic-signature-{id}")}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":format!("synthetic-encrypted-{id}")}}),
                json!({"type":"content_block_stop","index":1}),
            ];
            if let Some(call) = call {
                values.extend([
                    json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":call.id,"name":call.name,"input":{}}}),
                    json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":first}}),
                    json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":second}}),
                ]);
            } else {
                values.extend([
                    json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":text}}),
                ]);
            }
            values.extend([
                json!({"type":"content_block_stop","index":2}),
                json!({"type":"message_delta","delta":{"stop_reason":if call.is_some() { "tool_use" } else { "end_turn" }},"usage":{"output_tokens":7}}),
                json!({"type":"message_stop"}),
            ]);
            ("text/event-stream", sse(values, true))
        }
        Protocol::Gemini(mode) => {
            let part = if let Some(call) = call {
                json!({"functionCall":{"id":call.id,"name":call.name,"args":call.arguments}})
            } else {
                json!({"text":text})
            };
            let mut value = json!({"responseId":id,"modelVersion":MODEL,"candidates":[{"index":0,"content":{"role":"model","parts":[part]},"finishReason":"STOP"}]});
            let usage =
                json!({"promptTokenCount":11,"candidatesTokenCount":7,"totalTokenCount":18});
            if mode == GeminiMode::GenerateContent {
                value["usageMetadata"] = usage;
                ("application/json", value.to_string())
            } else {
                (
                    "text/event-stream",
                    sse(vec![value, json!({"usageMetadata":usage})], false),
                )
            }
        }
    }
}

async fn serve(listener: TcpListener, profile: Profile, root: PathBuf, trace: Arc<Mutex<Trace>>) {
    for round in 0..2 {
        let mut calls_this_round = 0;
        let mut cell_id = None;
        let mut cursor = 0;
        let mut outputs = Vec::new();
        loop {
            let (mut socket, _) = timeout(IO_TIMEOUT, listener.accept())
                .await
                .unwrap()
                .unwrap();
            let (head, body) = timeout(IO_TIMEOUT, read_request(&mut socket))
                .await
                .unwrap();
            let sequence = trace.lock().unwrap().requests.len() + 1;
            save(
                &root,
                &format!("request-{sequence:03}.json"),
                json!({"head":check_headers(profile,&head,&body),"body":body}),
            );
            let items = wire_items(profile, &body);
            let users: Vec<_> = items
                .iter()
                .filter_map(|item| {
                    if let WireItem::User(text) = item {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(users, PROMPTS[..=round]);
            {
                let mut trace = trace.lock().unwrap();
                if let Some(previous) = trace.requests.last() {
                    assert!(
                        items.starts_with(previous),
                        "follow-up lost or changed original history"
                    );
                }
                if matches!(profile.protocol, Protocol::Anthropic) {
                    let opaque: Vec<_> = items
                        .iter()
                        .filter_map(|item| {
                            if let WireItem::Opaque(value) = item {
                                Some(value.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    let expected: Vec<_> = (1..sequence).flat_map(|number| {
                        let id = format!("{}-response-{number}", profile.name);
                        [json!({"type":"thinking","thinking":"","signature":format!("synthetic-signature-{id}")}),
                         json!({"type":"redacted_thinking","data":format!("synthetic-encrypted-{id}")})]
                    }).collect();
                    assert_eq!(
                        opaque, expected,
                        "original signed blocks must survive every request and reopen"
                    );
                }
                let actual_calls: Vec<_> = items
                    .iter()
                    .filter_map(|item| {
                        if let WireItem::Call(call) = item {
                            Some(call.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(
                    actual_calls, trace.calls,
                    "wire call IDs or arguments changed"
                );
                trace.requests.push(items.clone());
            }
            let (arguments, answer) = if calls_this_round == 0 {
                assert_eq!(items.last(), Some(&WireItem::User(PROMPTS[round].into())));
                if round == 1 {
                    assert!(
                        items.ends_with(&[
                            WireItem::Text(final_text(0, CONTENTS[0])),
                            WireItem::User(PROMPTS[round].into()),
                        ]),
                        "second user round must carry the first final answer"
                    );
                }
                (Some(exec_arguments(round)), String::new())
            } else {
                let Some(WireItem::Result { id, page }) = items.last() else {
                    panic!("expected actual execute_code result: {items:?}");
                };
                assert_eq!(id, &trace.lock().unwrap().calls.last().unwrap().id);
                let page: CellPage = serde_json::from_value(page.clone()).unwrap();
                assert!(!page.outcome_unknown);
                assert!(page.error.is_none(), "failed actual cell: {page:?}");
                assert!(page.required_budget.is_none());
                assert_eq!(page.cursor, cursor);
                if let Some(id) = &cell_id {
                    assert_eq!(&page.cell_id, id);
                } else {
                    cell_id = Some(page.cell_id.clone());
                }
                cursor = page.next_cursor;
                outputs.extend(
                    page.outputs
                        .iter()
                        .map(|output| serde_json::to_value(output).unwrap()),
                );
                if page.status.is_terminal() && !page.has_more {
                    assert_eq!(page.status, CellStatus::Completed);
                    assert!(
                        calls_this_round >= 2,
                        "exec must yield, then wait without replay"
                    );
                    assert_eq!(outputs.len(), 1);
                    let value = &outputs[0]["output"]["value"];
                    assert_eq!(outputs[0]["output"]["type"], "json");
                    assert_eq!(value["current"]["path"], FILES[round]);
                    assert_eq!(value["current"]["content"], CONTENTS[round]);
                    if round == 1 {
                        assert_eq!(value["previous"]["path"], FILES[0]);
                        assert_eq!(value["previous"]["content"], CONTENTS[0]);
                    }
                    (
                        None,
                        final_text(round, value["current"]["content"].as_str().unwrap()),
                    )
                } else {
                    assert!(matches!(
                        page.status,
                        CellStatus::NotStarted
                            | CellStatus::Running
                            | CellStatus::Yielded
                            | CellStatus::Completed
                    ));
                    assert!(
                        calls_this_round < 12,
                        "cell did not settle within bounded fixture requests"
                    );
                    (
                        Some(
                            json!({"action":"wait","cell_id":page.cell_id,"cursor":page.next_cursor,"yield_time_ms":1000,"max_output_tokens":16384,"continue_execution":true}),
                        ),
                        String::new(),
                    )
                }
            };
            let call = arguments.map(|arguments| {
                let call = ToolCall::new(
                    format!(
                        "{}-round-{}-call-{}",
                        profile.name,
                        round + 1,
                        calls_this_round + 1
                    ),
                    "execute_code",
                    arguments,
                );
                trace.lock().unwrap().calls.push(call.clone());
                calls_this_round += 1;
                call
            });
            let (mime, wire) = response(profile, sequence, call.as_ref(), &answer);
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                wire.len()
            );
            save(
                &root,
                &format!("response-{sequence:03}.json"),
                json!({"head":headers,"body":wire,"call":call,"confirmed_terminal":call.is_none()}),
            );
            timeout(IO_TIMEOUT, async {
                socket.write_all(headers.as_bytes()).await.unwrap();
                for bytes in wire.as_bytes().chunks(7) {
                    socket.write_all(bytes).await.unwrap();
                    tokio::task::yield_now().await;
                }
                socket.shutdown().await.unwrap();
            })
            .await
            .unwrap();
            if call.is_none() {
                break;
            }
        }
    }
}

fn events(runtime: &Runtime, id: &str) -> Vec<EventRecord> {
    let mut all = Vec::new();
    loop {
        let cursor = all.last().map_or(0, |event: &EventRecord| event.cursor);
        let page = runtime.events(id, cursor, 1000).unwrap();
        if page.is_empty() {
            return all;
        }
        all.extend(page);
    }
}

fn assert_round(runtime: &Runtime, view: &SessionView, round: usize, trace: &Trace) {
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.attempt.is_none());
    assert!(!view.frozen);
    assert!(
        view.pending_approvals.is_empty()
            && view.unresolved_calls.is_empty()
            && view.unresolved_capabilities.is_empty()
    );
    let users: Vec<_> = view
        .messages
        .iter()
        .filter(|message| matches!(message, Message::User { .. }))
        .map(|message| message.text())
        .collect();
    assert_eq!(users, PROMPTS[..=round]);
    assert!(
        matches!(view.messages.last(),Some(Message::Assistant(message)) if message.stop_reason==StopReason::Stop&&cayw_ai::content_text(&message.content)==final_text(round,CONTENTS[round]))
    );
    let calls: Vec<_> = view
        .messages
        .iter()
        .filter_map(|message| {
            if let Message::Assistant(message) = message {
                Some(message.tool_calls())
            } else {
                None
            }
        })
        .flatten()
        .collect();
    assert_eq!(calls, trace.calls);
    let results: Vec<_> = view
        .messages
        .iter()
        .filter_map(|message| {
            if let Message::ToolResult(result) = message {
                Some(result)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(results.len(), calls.len());
    let mut ids = BTreeSet::new();
    for (call, result) in calls.iter().zip(&results) {
        assert!(ids.insert(&call.id));
        assert_eq!(result.tool_call_id, call.id);
        assert_eq!(result.tool_name, call.name);
        assert!(!result.is_error && !result.outcome_unknown);
        let wire_results: Vec<_> = trace
            .requests
            .last()
            .unwrap()
            .iter()
            .filter_map(|item| {
                if let WireItem::Result { id, page } = item {
                    (id == &call.id).then_some(page)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(wire_results.len(), 1);
        assert_eq!(Some(wire_results[0]), result.structured_content.as_ref());
        assert_eq!(
            serde_json::from_str::<Value>(&cayw_ai::content_text(&result.content)).unwrap(),
            *wire_results[0]
        );
    }
    let tasks = runtime.tasks(&view.id).unwrap();
    assert_eq!(tasks.len(), round + 1);
    assert!(
        tasks
            .iter()
            .all(|task| task.state == TaskState::Completed && task.error.is_none())
    );
    let records = events(runtime, &view.id);
    assert_eq!(records.last().unwrap().cursor, view.cursor);
    assert!(
        records
            .iter()
            .enumerate()
            .all(|(index, event)| event.cursor == index as u64 + 1 && event.session_id == view.id)
    );
    assert_eq!(
        records
            .iter()
            .filter(|event| matches!(event.event, CoreEvent::AttemptFinished { .. }))
            .count(),
        round + 1
    );
    let ended: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::Agent { event } => match event.as_ref() {
                AgentEvent::ToolExecutionEnd { call, result, .. } => {
                    assert!(!result.is_error && !result.outcome_unknown);
                    let history_result = results
                        .iter()
                        .find(|item| item.tool_call_id == call.id)
                        .unwrap();
                    assert_eq!(result.content, history_result.content);
                    assert_eq!(result.structured_content, history_result.structured_content);
                    Some(call.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(ended, calls);
    assert_eq!(view.executions.len(), round + 1);
    let capabilities: Vec<_> = records
        .iter()
        .filter_map(|record| {
            if let CoreEvent::CapabilitySettled { call } = &record.event {
                Some(call)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(capabilities.len(), if round == 0 { 2 } else { 5 });
    for user_round in 0..=round {
        let task = tasks
            .iter()
            .find(|task| task.input.text == PROMPTS[user_round])
            .unwrap();
        let execution = view
            .executions
            .iter()
            .find(|execution| execution.task_id == task.id)
            .unwrap();
        assert_eq!(execution.state, ExecutionState::Completed);
        assert!(execution.terminal.is_some());
        let parent = calls
            .iter()
            .find(|call| call.id == execution.parent_call_id)
            .unwrap();
        assert_eq!(parent.arguments, exec_arguments(user_round));
        let actual: Vec<_> = capabilities
            .iter()
            .filter(|call| call.task_id == task.id)
            .collect();
        let mut expected = Vec::new();
        if user_round == 1 {
            expected.push(("fs.read", json!({"path":FILES[0]})));
        }
        expected.push((
            "fs.write",
            json!({"path":FILES[user_round],"content":CONTENTS[user_round]}),
        ));
        expected.push(("fs.read", json!({"path":FILES[user_round]})));
        assert_eq!(actual.len(), expected.len(), "wait must not replay writes");
        for (sequence, (call, (name, arguments))) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(&call.name, name);
            assert_eq!(&call.arguments, arguments);
            assert_eq!(call.parent_call_id, parent.id);
            assert_eq!(call.key.execution_id, execution.execution_id);
            assert_eq!(call.key.sequence, sequence as u64);
            assert!(call.authorization.is_some());
            let Some(CapabilityOutcome::Completed { result }) = &call.outcome else {
                panic!("capability lacks success: {call:?}");
            };
            if *name == "fs.read" {
                let expected_round = if arguments["path"] == FILES[0] { 0 } else { 1 };
                assert_eq!(result["content"], CONTENTS[expected_round]);
            }
        }
    }
    for call in calls
        .iter()
        .filter(|call| call.arguments["action"] == "wait")
    {
        let binding = records
            .iter()
            .find_map(|record| match &record.event {
                CoreEvent::ExecutionWaitRegistered { binding } if binding.call_id == call.id => {
                    Some(binding)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(binding.arguments, call.arguments);
        assert_eq!(
            binding.execution_id,
            call.arguments["cell_id"].as_str().unwrap()
        );
    }
}

async fn run(profile: Profile) {
    let evidence = Evidence::new(profile);
    let root = &evidence.root;
    fs::create_dir(root.join("workspace")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let provider = profile.adapter(&base);
    let mut model = Model::new(profile.provider, MODEL);
    model.max_output_tokens = Some(1024);
    let mut config = RuntimeConfig::default();
    for round in 0..2 {
        config.permissions.push(ScopedPermission::exact(
            "fs.write",
            json!({"path":FILES[round],"content":CONTENTS[round]}),
        ));
        config.permissions.push(ScopedPermission::exact(
            "fs.read",
            json!({"path":FILES[round]}),
        ));
    }
    save(
        root,
        "fixture.json",
        json!({"source":"synthetic loopback HTTP; real Runtime, Cells and Host","profile":profile.name,"protocol":format!("{:?}",profile.protocol),"provider":profile.provider,"model":MODEL,"prompts":PROMPTS,"scripts":[script(0),script(1)],"files":FILES,"contents":CONTENTS,"credentials":"fixed fake key; request header redacted","evidence_environment":"CAYW_RUNTIME_PROTOCOL_EVIDENCE_DIR"}),
    );
    let trace = Arc::new(Mutex::new(Trace::default()));
    let server = tokio::spawn(serve(listener, profile, root.clone(), trace.clone()));
    let open = || {
        Runtime::open(
            root.join("session.db"),
            root.join("workspace"),
            provider.clone(),
            model.clone(),
            config.clone(),
        )
        .unwrap()
    };
    let mut runtime = open();
    let session = runtime
        .create(Some(format!("{} two user rounds", profile.name)))
        .unwrap();
    for round in 0..2 {
        let view = timeout(
            IO_TIMEOUT,
            runtime.prompt(&session.id, PROMPTS[round], CancellationToken::new()),
        )
        .await
        .expect("Runtime protocol fixture stalled")
        .unwrap();
        let records = events(&runtime, &session.id);
        save(
            root,
            &format!("round-{}-runtime.json", round + 1),
            json!({"view":view,"tasks":runtime.tasks(&session.id).unwrap(),"events":records,"audit":runtime.audit(&session.id,0,1000).unwrap(),"files":FILES[..=round].iter().map(|name|json!({"path":name,"content":fs::read_to_string(root.join("workspace").join(name)).unwrap()})).collect::<Vec<_>>()}),
        );
        assert_round(&runtime, &view, round, &trace.lock().unwrap());
        for file_round in 0..=round {
            assert_eq!(
                fs::read_to_string(root.join("workspace").join(FILES[file_round])).unwrap(),
                CONTENTS[file_round]
            );
        }
        if round == 0 {
            assert!(!root.join("workspace").join(FILES[1]).exists());
        }
        let tasks = runtime.tasks(&session.id).unwrap();
        let requests = trace.lock().unwrap().requests.len();
        drop(runtime);
        runtime = open();
        assert_eq!(runtime.get(&session.id).unwrap(), view);
        assert_eq!(runtime.tasks(&session.id).unwrap(), tasks);
        assert_eq!(events(&runtime, &session.id), records);
        assert_eq!(
            trace.lock().unwrap().requests.len(),
            requests,
            "reopen must not call the provider"
        );
        save(
            root,
            &format!("round-{}-reopened.json", round + 1),
            json!({"view":runtime.get(&session.id).unwrap(),"tasks":runtime.tasks(&session.id).unwrap(),"events":events(&runtime,&session.id),"http_requests":requests}),
        );
    }
    timeout(IO_TIMEOUT, server).await.unwrap().unwrap();
    let trace = trace.lock().unwrap();
    assert!(trace.requests.len() >= 6);
    save(
        root,
        "result.json",
        json!({"passed":true,"scope":"synthetic macOS loopback HTTP","profile":profile.name,"session_id":session.id,"user_rounds":2,"tasks":runtime.tasks(&session.id).unwrap(),"model_requests":trace.requests.len(),"original_calls":trace.calls,"capability_writes":2,"capability_reads":3,"reopened_history_and_events_equal":true}),
    );
    println!(
        "{}: two completed user rounds; evidence {}",
        profile.name,
        root.display()
    );
}

#[tokio::test]
async fn openai_and_magpie_two_user_rounds_through_public_runtime() {
    for name in ["openai", "magpie"] {
        run(Profile {
            name,
            provider: name,
            protocol: Protocol::OpenAi,
        })
        .await;
    }
}

#[tokio::test]
async fn anthropic_two_user_rounds_through_public_runtime() {
    run(Profile {
        name: "anthropic",
        provider: "anthropic",
        protocol: Protocol::Anthropic,
    })
    .await;
}

#[tokio::test]
async fn gemini_sse_two_user_rounds_through_public_runtime() {
    run(Profile {
        name: "gemini-sse",
        provider: "gemini",
        protocol: Protocol::Gemini(GeminiMode::StreamGenerateContent),
    })
    .await;
}

#[tokio::test]
async fn gemini_json_two_user_rounds_through_public_runtime() {
    run(Profile {
        name: "gemini-json",
        provider: "gemini",
        protocol: Protocol::Gemini(GeminiMode::GenerateContent),
    })
    .await;
}
