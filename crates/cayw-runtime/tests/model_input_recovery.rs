//! Fixed Chat responses over real loopback HTTP and Host file writes; no live model.
use cayw_ai::{CancellationToken, Message, Model, StopReason, providers::OpenAiProvider};
use cayw_core::CoreEvent;
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const CONTENT: &str = "one completed write before truncated output";

async fn read_request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < end + length {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    serde_json::from_slice(&bytes[end..end + length]).unwrap()
}

fn wire(sequence: usize, call: Option<(&str, String)>, truncated: bool) -> String {
    let delta = match call {
        Some((id, arguments)) => {
            json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":"execute_code","arguments":arguments}}]})
        }
        None => json!({"content":"The earlier effect is complete."}),
    };
    let reason = if truncated {
        "length"
    } else if delta.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    let frames = [
        json!({"id":format!("input-{sequence}"),"model":"synthetic-chat","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
        json!({"id":format!("input-{sequence}"),"model":"synthetic-chat","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":format!("input-{sequence}"),"model":"synthetic-chat","choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":4,"completion_tokens":4,"total_tokens":8}}),
    ];
    let mut body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    body
}

async fn respond(stream: &mut TcpStream, body: &str) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await.unwrap();
    for part in body.as_bytes().chunks(7) {
        stream.write_all(part).await.unwrap();
        tokio::task::yield_now().await;
    }
    stream.shutdown().await.unwrap();
}

fn open(root: &Path, base: &str) -> Runtime {
    let mut config = RuntimeConfig {
        max_total_tokens: 1000,
        max_turns: 12,
        ..Default::default()
    };
    config.permissions.push(ScopedPermission::exact(
        "fs.write",
        json!({"path":"result.txt","content":CONTENT}),
    ));
    let mut model = Model::new("openai", "synthetic-chat");
    model.max_output_tokens = Some(128);
    Runtime::open(
        root.join("session.db"),
        root.join("workspace"),
        Arc::new(OpenAiProvider::new(base, "").unwrap()),
        model,
        config,
    )
    .unwrap()
}

fn latest_page(body: &Value) -> Value {
    let tool = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] != "truncated-call")
        .expect("prior completed or waiting tool result");
    tool["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|part| part["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|value| value.get("cell_id").is_some())
        .expect("one serialized Cell page")
}

#[tokio::test]
async fn empty_aborted_history_is_explained_without_changing_its_durable_prefix() {
    tokio::time::timeout(Duration::from_secs(30), async {
        use cayw_agent::AgentEvent;
        use cayw_ai::AssistantMessage;

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let body = read_request(&mut stream).await;
            assert!(
                body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "system"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("Runtime input notice")
                                && text.contains("aborted")
                                && text.contains("Agent run cancelled")))
            );
            assert!(
                !body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "assistant")
            );
            respond(&mut stream, &wire(0, None, false)).await;
            body
        });
        let runtime = open(root.path(), &base);
        let id = runtime.create(None).unwrap().id;
        // Seed only an explicit Core lifecycle fixture, not an unrecorded model call.
        let lease = runtime
            .store()
            .begin_attempt(&id, "synthetic-stopped-owner")
            .unwrap();
        let assistant = AssistantMessage::new(&Model::new("openai", "synthetic-chat"))
            .failure(StopReason::Aborted, "Agent run cancelled");
        let message = Message::Assistant(assistant.clone());
        let user = Message::user("old task");
        for event in [
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::MessageStart {
                message: user.clone(),
            },
            AgentEvent::MessageEnd { message: user },
            AgentEvent::MessageStart {
                message: message.clone(),
            },
            AgentEvent::MessageEnd { message },
            AgentEvent::TurnEnd {
                message: assistant,
                tool_results: vec![],
            },
            AgentEvent::AgentEnd { messages: vec![] },
        ] {
            runtime.store().append_agent_event(&lease, &event).unwrap();
        }
        runtime.store().finish_attempt(&lease).unwrap();
        let before = runtime.get(&id).unwrap();
        let events = runtime.events(&id, 0, 1000).unwrap();
        let after = runtime
            .prompt(&id, "Continue with a new answer.", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Completed);
        assert!(after.messages.starts_with(&before.messages));
        assert!(runtime.events(&id, 0, 1000).unwrap().starts_with(&events));
        assert!(after.executions.is_empty());
        assert_eq!(runtime.model_requests(&id).unwrap().settled_tokens, 8);
        drop(runtime);
        let runtime = open(root.path(), &base);
        assert_eq!(runtime.get(&id).unwrap().messages, after.messages);
        server.await.unwrap();
    })
    .await
    .expect("aborted input fixture stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fragmented_arguments_write_once_complete_wait_and_survive_reopen() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let mut arguments = json!({"action":"exec","script":format!("return cayw.call('fs.write', {{path='result.txt',content='{CONTENT}'}})"),"yield_time_ms":1000,"max_output_tokens":16384});
        let padding = 7938 - arguments.to_string().len();
        let script = format!("{}{}", arguments["script"].as_str().unwrap(), " ".repeat(padding));
        arguments["script"] = script.into();
        let encoded = arguments.to_string();
        assert_eq!(encoded.len(), 7938);
        let mut offset = 0;
        let mut captured = String::new();
        for line in include_str!("../../cayw-ai/tests/fixtures/sanitized_live04_snapshot.sse").lines() {
            let Some(data) = line.strip_prefix("data: ") else { continue };
            if data == "[DONE]" {
                captured.push_str("data: [DONE]\n\n");
                continue;
            }
            let mut frame: Value = serde_json::from_str(data).unwrap();
            if let Some(fragment) = frame["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str() {
                let end = offset + fragment.len();
                frame["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] = encoded[offset..end].into();
                offset = end;
            }
            captured.push_str(&format!("data: {frame}\n\n"));
        }
        assert_eq!(offset, encoded.len());
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let mut count = 0;
            loop {
                let (mut stream, _) = tokio::select! {
                    incoming = listener.accept() => incoming.unwrap(),
                    _ = &mut stopped => break,
                };
                let body = read_request(&mut stream).await;
                let response = if count == 0 {
                    captured.clone()
                } else {
                    let page = latest_page(&body);
                    if page["vm_alive"] == true || page["has_more"] == true || matches!(page["status"].as_str(), Some("running" | "yielded")) {
                        let arguments = json!({"action":"wait","cell_id":page["cell_id"],"cursor":page["next_cursor"],"yield_time_ms":1000,"max_output_tokens":16384});
                        wire(count, Some((&format!("wait-fragments-{count}"), arguments.to_string())), false)
                    } else {
                        wire(count, None, false)
                    }
                };
                count += 1;
                assert!(count < 10);
                respond(&mut stream, &response).await;
            }
            count
        });
        let mut config = RuntimeConfig { max_total_tokens: 100_000, max_turns: 12, ..Default::default() };
        config.permissions.push(ScopedPermission::exact("fs.write", json!({"path":"result.txt","content":CONTENT})));
        let mut model = Model::new("magpie", "opencode-go/deepseek-v4.1-flash");
        model.max_output_tokens = Some(8192);
        let open = || Runtime::open(root.path().join("session.db"), root.path().join("workspace"), Arc::new(OpenAiProvider::new(&base, "").unwrap()), model.clone(), config.clone()).unwrap();
        let runtime = open();
        let id = runtime.create(None).unwrap().id;
        let final_view = runtime.prompt(&id, "Write once, wait for completion, and finish.", CancellationToken::new()).await.unwrap();
        assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Completed);
        assert_eq!(std::fs::read_to_string(root.path().join("workspace/result.txt")).unwrap(), CONTENT);
        let mut events = Vec::new();
        while events.len() < final_view.cursor as usize {
            let after = events.last().map_or(0, |event: &cayw_core::EventRecord| event.cursor);
            let page = runtime.events(&id, after, 1000).unwrap();
            assert!(!page.is_empty());
            events.extend(page);
        }
        assert_eq!(events.iter().filter(|row| matches!(&row.event, CoreEvent::CapabilitySettled { call } if call.name == "fs.write")).count(), 1);
        assert_eq!(final_view.executions.len(), 1);
        let ledger = runtime.model_requests(&id).unwrap();
        assert_eq!(ledger.reserved_tokens, 0);
        assert!(!ledger.budget_unknown);
        assert_eq!(ledger.settled_tokens, 32300 + (ledger.requests.len() as u64 - 1) * 8);
        drop(runtime);
        let runtime = open();
        assert_eq!(runtime.get(&id).unwrap(), final_view);
        assert_eq!(runtime.model_requests(&id).unwrap(), ledger);
        let _ = stop.send(());
        assert_eq!(server.await.unwrap(), ledger.requests.len());
    }).await.expect("fragmented Host write loop stalled");
}

#[tokio::test]
async fn truncated_tool_input_continues_without_repeating_a_completed_host_write() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let write = json!({"action":"exec","script":format!("return cayw.call('fs.write', {{path='result.txt',content='{CONTENT}'}})"),"yield_time_ms":1000,"max_output_tokens":16384});
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            loop {
                let (mut stream, _) = tokio::select! {
                    incoming = listener.accept() => incoming.unwrap(),
                    _ = &mut stopped => break,
                };
                let body = read_request(&mut stream).await;
                let sequence = bodies.len();
                let response = match sequence {
                    0 => wire(sequence, Some(("write-call", write.to_string())), false),
                    1 => wire(sequence, Some(("truncated-call", "{\"action\": ".into())), true),
                    _ => {
                        let messages = body["messages"].as_array().unwrap();
                        assert!(messages.iter().any(|message| message["role"] == "system" && message["content"].as_str().is_some_and(|text| text.contains("Runtime input notice") && text.contains("truncated-call"))));
                        assert!(!messages.iter().any(|message| message["role"] == "tool" && message["tool_call_id"] == "truncated-call"));
                        assert!(messages.iter().any(|message| message["role"] == "assistant" && message["tool_calls"].as_array().is_some_and(|calls| calls.iter().any(|call| call["id"] == "write-call"))));
                        let page = latest_page(&body);
                        if page["vm_alive"] == true || page["has_more"] == true || matches!(page["status"].as_str(), Some("running" | "yielded")) {
                            let arguments = json!({"action":"wait","cell_id":page["cell_id"],"cursor":page["next_cursor"],"yield_time_ms":1000,"max_output_tokens":16384});
                            wire(sequence, Some((&format!("wait-{sequence}"), arguments.to_string())), false)
                        } else {
                            wire(sequence, None, false)
                        }
                    }
                };
                assert!(sequence < 10, "bounded fixture did not finish");
                bodies.push(body);
                respond(&mut stream, &response).await;
            }
            bodies
        });
        let runtime = open(root.path(), &base);
        let id = runtime.create(None).unwrap().id;
        let first = runtime.prompt(&id, "Write the exact fixture once.", CancellationToken::new()).await.unwrap();
        assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Completed);
        assert_eq!(std::fs::read_to_string(root.path().join("workspace/result.txt")).unwrap(), CONTENT);
        assert_eq!(first.executions.len(), 1);
        let assistant = first.messages.iter().find_map(|message| match message {
            Message::Assistant(assistant) if assistant.stop_reason == StopReason::Length => Some(assistant),
            _ => None,
        }).unwrap();
        assert!(assistant.tool_calls()[0].arguments_error.is_some());
        assert!(first.messages.iter().any(|message| matches!(message, Message::ToolResult(result) if result.tool_call_id == "truncated-call" && result.is_error && !result.outcome_unknown && result.duration_ms.is_none())));
        let before = runtime.events(&id, 0, 1000).unwrap();
        assert_eq!(before.iter().filter(|row| matches!(&row.event, CoreEvent::CapabilitySettled { call } if call.name == "fs.write")).count(), 1);
        assert!(!before.iter().any(|row| matches!(&row.event, CoreEvent::ToolStarted { call_id } if call_id == "truncated-call")));
        let ledger = runtime.model_requests(&id).unwrap();
        assert_eq!(ledger.reserved_tokens, 0);
        assert!(!ledger.budget_unknown);
        drop(runtime);
        let runtime = open(root.path(), &base);
        assert_eq!(runtime.get(&id).unwrap().messages, first.messages);
        assert_eq!(runtime.model_requests(&id).unwrap(), ledger);
        let next = runtime.prompt(&id, "Confirm without another write.", CancellationToken::new()).await.unwrap();
        assert!(next.messages.starts_with(&first.messages));
        assert_eq!(runtime.tasks(&id).unwrap()[1].state, TaskState::Completed);
        assert_eq!(runtime.events(&id, 0, 1000).unwrap().iter().filter(|row| matches!(&row.event, CoreEvent::CapabilitySettled { call } if call.name == "fs.write")).count(), 1);
        assert_eq!(next.executions.len(), 1);
        let _ = stop.send(());
        let bodies = server.await.unwrap();
        let after = runtime.model_requests(&id).unwrap();
        assert_eq!(bodies.len(), after.requests.len());
        assert_eq!(after.settled_tokens, bodies.len() as u64 * 8);
        assert_eq!(after.reserved_tokens, 0);
        assert!(!after.budget_unknown);
    }).await.expect("model input recovery stalled");
}
