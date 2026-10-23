//! Synthetic loopback HTTP plus the public Agent. The fixture tool writes real
//! temporary files; this checks protocol/result pairing, not Runtime authorization.
use async_trait::async_trait;
use cayw_agent::{Agent, AgentEvent, AgentTool, ExecutionMode, ToolContext, ToolResult};
use cayw_ai::{
    AssistantEvent, Content, Message, Model, Provider, StopReason, ToolCall, ToolDefinition,
    UsageStatus,
};
use cayw_providers::{AnthropicProvider, GeminiMode, GeminiProvider};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::timeout,
};

const BOUND: Duration = Duration::from_secs(20);
const MODEL: &str = "pairing-fixture";
const TOOL: &str = "fixture_write";
const IDS: [&str; 2] = ["original-first", "original-second"];
const FILES: [&str; 2] = ["first.txt", "second.txt"];
const TEXTS: [&str; 2] = ["alpha 你好", "beta 🦀"];

#[derive(Clone, Copy, Debug)]
enum Profile {
    OpenAi,
    Magpie,
    Anthropic,
    GeminiSse,
    GeminiJson,
}
impl Profile {
    fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Magpie => "magpie",
            Self::Anthropic => "anthropic",
            Self::GeminiSse => "gemini-sse",
            Self::GeminiJson => "gemini-json",
        }
    }
    fn provider(self) -> &'static str {
        match self {
            Self::GeminiSse | Self::GeminiJson => "gemini",
            _ => self.name(),
        }
    }
    fn adapter(self, base: &str) -> Arc<dyn Provider> {
        match self {
            Self::OpenAi | Self::Magpie => {
                Arc::new(cayw_ai::providers::OpenAiProvider::new(base, "fixed-fake-key").unwrap())
            }
            Self::Anthropic => Arc::new(AnthropicProvider::new(base, "fixed-fake-key").unwrap()),
            Self::GeminiSse | Self::GeminiJson => Arc::new(
                GeminiProvider::new(base, "fixed-fake-key")
                    .unwrap()
                    .with_mode(if matches!(self, Self::GeminiJson) {
                        GeminiMode::GenerateContent
                    } else {
                        GeminiMode::StreamGenerateContent
                    }),
            ),
        }
    }
}
fn arguments(index: usize) -> Value {
    json!({"path":FILES[index],"text":TEXTS[index]})
}
fn output(index: usize, source: &str) -> Value {
    json!({"source":source,"path":FILES[index],"content":TEXTS[index],"call_id":IDS[index]})
}
fn save(root: &Path, name: &str, value: &impl serde::Serialize) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(name))
        .unwrap();
    serde_json::to_writer_pretty(&mut file, value).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
}
fn evidence(profile: Profile) -> (PathBuf, Option<tempfile::TempDir>) {
    if let Some(parent) = std::env::var_os("CAYW_PROTOCOL_PAIRING_EVIDENCE_DIR") {
        let parent = PathBuf::from(parent);
        assert!(parent.is_absolute());
        fs::create_dir_all(&parent).unwrap();
        let root = parent.join(profile.name());
        fs::create_dir(&root).expect("preserve old pairing evidence; use a fresh directory");
        (root, None)
    } else {
        let temp = tempfile::tempdir().unwrap();
        (temp.path().to_owned(), Some(temp))
    }
}

struct FileTool {
    root: PathBuf,
    second_ended: Arc<Semaphore>,
}
#[async_trait]
impl AgentTool for FileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: TOOL.into(),
            description: "write one of two temporary fixture files".into(),
            parameters: json!({"type":"object","properties":{
                "path":{"type":"string","enum":FILES},
                "text":{"type":"string"}},"required":["path","text"],
                "additionalProperties":false}),
        }
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        let index = IDS.iter().position(|id| *id == context.call_id).unwrap();
        assert_eq!(context.arguments, arguments(index));
        if index == 0 {
            timeout(BOUND, self.second_ended.acquire())
                .await
                .expect("second tool never finished")
                .unwrap()
                .forget();
        }
        fs::write(self.root.join(FILES[index]), TEXTS[index]).unwrap();
        Ok(ToolResult {
            content: vec![Content::text(output(index, "text").to_string())],
            structured_content: Some(output(index, "structured_content")),
            ..Default::default()
        })
    }
}

async fn request(socket: &mut TcpStream, profile: Profile) -> Value {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&buffer[..size]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        assert!(bytes.len() <= 16 * 1024);
    };
    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
    let endpoint = match profile {
        Profile::OpenAi | Profile::Magpie => "/v1/chat/completions".into(),
        Profile::Anthropic => "/v1/messages".into(),
        Profile::GeminiSse => format!("/v1/models/{MODEL}:streamGenerateContent?alt=sse"),
        Profile::GeminiJson => format!("/v1/models/{MODEL}:generateContent"),
    };
    assert!(headers.starts_with(&format!("POST {endpoint} HTTP/1.1\r\n")));
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    assert!(length < 128 * 1024);
    while bytes.len() < end + length {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&buffer[..size]);
    }
    serde_json::from_slice(&bytes[end..end + length]).unwrap()
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
fn response(profile: Profile, tools: bool) -> (&'static str, String) {
    let id = if tools {
        "tools-response"
    } else {
        "answer-response"
    };
    let split: Vec<_> = (0..2)
        .map(|index| {
            let value = arguments(index).to_string();
            let middle = value.find(':').unwrap() + 1;
            (value[..middle].to_owned(), value[middle..].to_owned())
        })
        .collect();
    match profile {
        Profile::OpenAi | Profile::Magpie => {
            let chunk = |delta: Value, reason: Value| json!({"id":id,"model":MODEL,"choices":[{"index":0,"delta":delta,"finish_reason":reason}]});
            let mut values = if tools {
                // The higher wire index arrives first. Its fragments must wait
                // for index zero without changing the original call order.
                vec![
                    chunk(
                        json!({"role":"assistant","tool_calls":[{"index":1,"id":IDS[1],"type":"function","function":{"name":TOOL,"arguments":split[1].0}}]}),
                        Value::Null,
                    ),
                    chunk(
                        json!({"tool_calls":[{"index":0,"id":IDS[0],"type":"function","function":{"name":TOOL,"arguments":split[0].0}}]}),
                        Value::Null,
                    ),
                    chunk(
                        json!({"tool_calls":[{"index":1,"function":{"arguments":split[1].1}},{"index":0,"function":{"arguments":split[0].1}}]}),
                        Value::Null,
                    ),
                ]
            } else {
                vec![chunk(
                    json!({"role":"assistant","content":"both original results received"}),
                    Value::Null,
                )]
            };
            values.push(chunk(
                json!({}),
                json!(if tools { "tool_calls" } else { "stop" }),
            ));
            values.push(json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}));
            (
                "text/event-stream",
                sse(values, false) + "data: [DONE]\r\n\r\n",
            )
        }
        Profile::Anthropic => {
            let mut values = vec![
                json!({"type":"message_start","message":{"id":id,"model":MODEL,"type":"message","role":"assistant","content":[],"usage":{"input_tokens":11,"output_tokens":0}}}),
            ];
            if tools {
                // Messages streams blocks in sequence. Each tool has split JSON.
                for index in 0..2 {
                    values.extend([
                        json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":IDS[index],"name":TOOL,"input":{}}}),
                        json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":split[index].0}}),
                        json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":split[index].1}}),
                        json!({"type":"content_block_stop","index":index}),
                    ]);
                }
            } else {
                values.extend([
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"both original results received"}}),
                    json!({"type":"content_block_stop","index":0}),
                ]);
            }
            values.extend([
                json!({"type":"message_delta","delta":{"stop_reason":if tools {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":7}}),
                json!({"type":"message_stop"}),
            ]);
            ("text/event-stream", sse(values, true))
        }
        Profile::GeminiSse | Profile::GeminiJson => {
            // This finite Gemini adapter accepts complete functionCall objects;
            // the HTTP transport still splits their bytes, including UTF-8.
            let parts = if tools {
                (0..2).map(|index| json!({"functionCall":{"id":IDS[index],"name":TOOL,"args":arguments(index)}})).collect::<Vec<_>>()
            } else {
                vec![json!({"text":"both original results received"})]
            };
            let value = json!({"responseId":id,"modelVersion":MODEL,"candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7,"totalTokenCount":18}});
            if matches!(profile, Profile::GeminiJson) {
                ("application/json", value.to_string())
            } else {
                ("text/event-stream", sse(vec![value], false))
            }
        }
    }
}
async fn respond(socket: &mut TcpStream, mime: &str, body: &str) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await.unwrap();
    for fragment in body.as_bytes().chunks(7) {
        socket.write_all(fragment).await.unwrap();
        tokio::task::yield_now().await;
    }
    socket.shutdown().await.unwrap();
}
fn check_history(profile: Profile, body: &Value) {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    let field = if matches!(profile, Profile::GeminiSse | Profile::GeminiJson) {
        "contents"
    } else {
        "messages"
    };
    for message in body[field].as_array().unwrap() {
        match profile {
            Profile::OpenAi | Profile::Magpie => {
                if let Some(items) = message["tool_calls"].as_array() {
                    for call in items {
                        calls.push(ToolCall::new(
                            call["id"].as_str().unwrap(),
                            call["function"]["name"].as_str().unwrap(),
                            serde_json::from_str(call["function"]["arguments"].as_str().unwrap())
                                .unwrap(),
                        ));
                    }
                }
                if message["role"] == "tool" {
                    let values = message["content"].as_array().unwrap();
                    assert_eq!(values.len(), 2);
                    assert!(values.iter().all(|value| value["type"] == "text"));
                    let text: Value =
                        serde_json::from_str(values[0]["text"].as_str().unwrap()).unwrap();
                    let structured: Value =
                        serde_json::from_str(values[1]["text"].as_str().unwrap()).unwrap();
                    results.push((
                        message["tool_call_id"].as_str().unwrap().to_owned(),
                        text,
                        structured,
                    ));
                }
            }
            Profile::Anthropic => {
                for part in message["content"].as_array().unwrap() {
                    if part["type"] == "tool_use" {
                        calls.push(ToolCall::new(
                            part["id"].as_str().unwrap(),
                            part["name"].as_str().unwrap(),
                            part["input"].clone(),
                        ));
                    } else if part["type"] == "tool_result" {
                        assert_eq!(part["is_error"], false);
                        let values = part["content"].as_array().unwrap();
                        assert_eq!(values.len(), 2);
                        assert!(values.iter().all(|value| value["type"] == "text"));
                        let text: Value =
                            serde_json::from_str(values[0]["text"].as_str().unwrap()).unwrap();
                        let structured: Value =
                            serde_json::from_str(values[1]["text"].as_str().unwrap()).unwrap();
                        results.push((
                            part["tool_use_id"].as_str().unwrap().to_owned(),
                            text,
                            structured,
                        ));
                    }
                }
            }
            Profile::GeminiSse | Profile::GeminiJson => {
                for part in message["parts"].as_array().unwrap() {
                    if let Some(call) = part.get("functionCall") {
                        calls.push(ToolCall::new(
                            call["id"].as_str().unwrap(),
                            call["name"].as_str().unwrap(),
                            call["args"].clone(),
                        ));
                    } else if let Some(result) = part.get("functionResponse") {
                        assert_eq!(result["name"], TOOL);
                        let text: Value =
                            serde_json::from_str(result["response"]["text"].as_str().unwrap())
                                .unwrap();
                        results.push((
                            result["id"].as_str().unwrap().to_owned(),
                            text,
                            result["response"]["output"].clone(),
                        ));
                    }
                }
            }
        }
    }
    assert_eq!(
        calls,
        (0..2)
            .map(|index| ToolCall::new(IDS[index], TOOL, arguments(index)))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        results,
        (0..2)
            .map(|index| (
                IDS[index].to_owned(),
                output(index, "text"),
                output(index, "structured_content")
            ))
            .collect::<Vec<_>>()
    );
}

fn check_tool_events(events: &[AgentEvent]) {
    let mut started = [false; 2];
    let mut ended = [false; 2];
    let mut arguments_json = [String::new(), String::new()];
    for event in events {
        let AgentEvent::MessageUpdate { message, event } = event else {
            continue;
        };
        let (index, partial) = match event.as_ref() {
            AssistantEvent::ToolCallStart { index, partial }
            | AssistantEvent::ToolCallDelta { index, partial, .. }
            | AssistantEvent::ToolCallEnd { index, partial } => (*index, partial),
            _ => continue,
        };
        assert!(
            index < IDS.len(),
            "tool event index {index} has no original wire call"
        );
        assert_eq!(
            message, partial,
            "Agent must retain the indexed event snapshot"
        );
        let Some(Content::ToolCall(call)) = partial.content.get(index) else {
            panic!("tool event index {index} must point at its original call");
        };
        assert_eq!(call.id, IDS[index]);
        assert_eq!(call.name, TOOL);
        assert_eq!(partial.response_id.as_deref(), Some("tools-response"));
        match event.as_ref() {
            AssistantEvent::ToolCallStart { .. } => {
                assert!(!started[index] && !ended[index], "duplicate tool start");
                started[index] = true;
            }
            AssistantEvent::ToolCallDelta { delta, .. } => {
                assert!(
                    started[index] && !ended[index],
                    "delta must follow its original start"
                );
                arguments_json[index].push_str(delta);
            }
            AssistantEvent::ToolCallEnd { .. } => {
                assert!(
                    started[index] && !ended[index],
                    "end must pair with its original start"
                );
                assert_eq!(
                    serde_json::from_str::<Value>(&arguments_json[index]).unwrap(),
                    arguments(index)
                );
                assert_eq!(call.arguments, arguments(index));
                ended[index] = true;
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(started, [true; 2], "both original calls must emit a start");
    assert_eq!(ended, [true; 2], "both original calls must emit an end");
}

async fn run(profile: Profile) {
    let (root, _temp) = evidence(profile);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = profile.adapter(&format!("http://{}/v1", listener.local_addr().unwrap()));
    let server_root = root.clone();
    let server = tokio::spawn(async move {
        for turn in 0..2 {
            let (mut socket, _) = timeout(BOUND, listener.accept()).await.unwrap().unwrap();
            let body = request(&mut socket, profile).await;
            save(&server_root, &format!("request-{}.json", turn + 1), &body);
            if turn == 1 {
                check_history(profile, &body);
                for index in 0..2 {
                    assert_eq!(
                        fs::read_to_string(server_root.join(FILES[index])).unwrap(),
                        TEXTS[index]
                    );
                }
            }
            let (mime, wire) = response(profile, turn == 0);
            save(
                &server_root,
                &format!("response-{}.json", turn + 1),
                &json!({"mime":mime,"body":wire}),
            );
            respond(&mut socket, mime, &wire).await;
        }
    });
    let mut model = Model::new(profile.provider(), MODEL);
    model.max_output_tokens = Some(128);
    let agent = Agent::new(provider, model);
    let second_ended = Arc::new(Semaphore::new(0));
    agent
        .set_tools(vec![Arc::new(FileTool {
            root: root.clone(),
            second_ended: second_ended.clone(),
        })])
        .unwrap();
    agent.set_execution_mode(ExecutionMode::Parallel).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = events.clone();
    agent.subscribe(move |event, _| {
        if let AgentEvent::ToolExecutionEnd { call, .. } = &event
            && call.id == IDS[1]
        {
            second_ended.add_permits(1);
        }
        recorded.lock().unwrap().push(event);
        async { Ok(()) }
    });
    let result = timeout(
        BOUND,
        agent.prompt(Message::user("write both fixture files")),
    )
    .await
    .unwrap()
    .unwrap();
    timeout(BOUND, server).await.unwrap().unwrap();
    assert_eq!(result.last_message.stop_reason, StopReason::Stop);
    assert_eq!(
        cayw_ai::content_text(&result.last_message.content),
        "both original results received"
    );
    let tool_results: Vec<_> = result
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 2);
    for index in 0..2 {
        assert_eq!(tool_results[index].tool_call_id, IDS[index]);
        assert_eq!(tool_results[index].tool_name, TOOL);
        assert_eq!(
            tool_results[index].content,
            vec![Content::text(output(index, "text").to_string())]
        );
        assert_eq!(
            tool_results[index].structured_content,
            Some(output(index, "structured_content"))
        );
        assert!(!tool_results[index].is_error && !tool_results[index].outcome_unknown);
    }
    let events = events.lock().unwrap();
    check_tool_events(&events);
    for event in events.iter() {
        if let AgentEvent::ToolExecutionEnd { call, result, .. } = event {
            let index = IDS.iter().position(|id| *id == call.id).unwrap();
            assert_eq!(call.name, TOOL);
            assert_eq!(call.arguments, arguments(index));
            assert_eq!(
                result.content,
                vec![Content::text(output(index, "text").to_string())]
            );
            assert_eq!(
                result.structured_content,
                Some(output(index, "structured_content"))
            );
            assert!(!result.is_error && !result.outcome_unknown);
        }
    }
    let ended: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { call, .. } => Some(call.id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ended,
        vec![IDS[1], IDS[0]],
        "fixture must actually finish in reverse order"
    );
    for message in &result.messages {
        if let Message::Assistant(message) = message {
            assert_eq!(message.usage_status, UsageStatus::Reported);
            assert_eq!(message.usage.total_tokens, 18);
            assert_eq!(message.response_model.as_deref(), Some(MODEL));
        }
    }
    save(&root, "history.json", &result.messages);
    save(&root, "events.json", &*events);
    save(
        &root,
        "scope.json",
        &json!({"source":"synthetic loopback HTTP","profile":profile.name(),"real_service_requests":0,"http_requests":2,"tool_completion_order":ended,"history_order":IDS,"authorization_scope":"public Agent fixture only; public Runtime authorization is tested separately"}),
    );
    println!(
        "{}: reversed actual tool completion retains original HTTP IDs and result order; {}",
        profile.name(),
        root.display()
    );
}

#[tokio::test]
async fn multiple_calls_keep_original_http_pairing_after_reverse_tool_completion() {
    for profile in [
        Profile::OpenAi,
        Profile::Magpie,
        Profile::Anthropic,
        Profile::GeminiSse,
        Profile::GeminiJson,
    ] {
        run(profile).await;
    }
}
