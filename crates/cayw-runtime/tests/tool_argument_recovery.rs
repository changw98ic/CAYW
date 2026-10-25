//! Synthetic Messages responses over loopback; no external model is called.
use cayw_ai::{CancellationToken, Message, Model, StopReason};
use cayw_core::CoreEvent;
use cayw_providers::AnthropicProvider;
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const WATCHDOG: Duration = Duration::from_secs(30);
const CONTENT: &str = "written once after valid arguments";

fn save(name: &str, value: &impl serde::Serialize) {
    if let Some(root) = std::env::var_os("CAYW_ARGUMENT_RECOVERY_EVIDENCE_DIR") {
        let path = std::path::PathBuf::from(root);
        std::fs::create_dir_all(&path).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.join(name))
            .unwrap();
        serde_json::to_writer_pretty(file, value).unwrap();
    }
}

async fn request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
    assert!(headers.starts_with("POST /v1/messages HTTP/1.1"));
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
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    serde_json::from_slice(&bytes[end..end + length]).unwrap()
}

fn response(sequence: usize, call: Option<(&str, &Value)>) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":format!("recovery-{sequence}"),"model":"synthetic-model","type":"message","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":"synthetic-opaque-signature"}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    if let Some((id, arguments)) = call {
        events.extend([
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":id,"name":"execute_code","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":arguments.to_string()}}),
        ]);
    } else {
        events.push(json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"recovered"}}));
    }
    events.extend([
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":if call.is_some(){"tool_use"}else{"end_turn"}},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ]);
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

async fn respond(socket: &mut TcpStream, body: &str) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await.unwrap();
    for part in body.as_bytes().chunks(7) {
        if socket.write_all(part).await.is_err() {
            return;
        }
        tokio::task::yield_now().await;
    }
    socket.shutdown().await.unwrap();
}

fn open(root: &Path, base: &str, max_turns: usize) -> Runtime {
    open_with_budget(
        root,
        base,
        max_turns,
        RuntimeConfig::default().max_total_tokens,
    )
}

fn open_with_budget(root: &Path, base: &str, max_turns: usize, tokens: u64) -> Runtime {
    let mut config = RuntimeConfig {
        max_turns,
        max_total_tokens: tokens,
        ..Default::default()
    };
    config.permissions.push(ScopedPermission::exact(
        "fs.write",
        json!({"path":"result.txt","content":CONTENT}),
    ));
    let mut model = Model::new("anthropic", "synthetic-model");
    model.max_output_tokens = Some(128);
    Runtime::open(
        root.join("session.db"),
        root.join("workspace"),
        Arc::new(AnthropicProvider::new(base, "synthetic-recovery-key").unwrap()),
        model,
        config,
    )
    .unwrap()
}

#[tokio::test]
async fn invalid_arguments_return_to_model_before_one_real_write_and_survive_reopen() {
    timeout(WATCHDOG, async {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let valid = json!({"action":"exec","script":format!("return cayw.call('fs.write', {{path='result.txt',content='{CONTENT}'}})"),"yield_time_ms":1000,"max_output_tokens":16384});
        // Matches the failure shape: a JSON string whose contents also have an extra brace.
        let invalid = json!(format!("{valid}}}"));
        let expected_invalid = invalid.clone();
        let workspace = root.path().join("workspace");
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for sequence in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let body = request(&mut socket).await;
                if sequence == 1 {
                    assert!(!workspace.join("result.txt").exists(), "invalid arguments reached the Host");
                }
                if sequence > 0 {
                    let original = &body["messages"][1]["content"];
                    assert_eq!(original[0]["signature"], "synthetic-opaque-signature");
                    let results = body["messages"][2]["content"].as_array().unwrap();
                    assert_eq!(results[0]["type"], "tool_result");
                    assert_eq!(results[0]["tool_use_id"], "invalid-call");
                    assert_eq!(results[0]["is_error"], true);
                    assert!(results[0]["content"].to_string().contains("argument"));
                }
                bodies.push(body);
                let call = match sequence {
                    0 => Some(("invalid-call", &invalid)),
                    1 => Some(("valid-call", &valid)),
                    _ => None,
                };
                respond(&mut socket, &response(sequence, call)).await;
            }
            bodies
        });
        let runtime = open(root.path(), &base, 6);
        let session = runtime.create(None).unwrap();
        let view = runtime
            .prompt(&session.id, "Write the fixture once.", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(runtime.tasks(&session.id).unwrap()[0].state, TaskState::Completed);
        assert!(matches!(view.messages.last(),Some(Message::Assistant(message)) if message.stop_reason==StopReason::Stop));
        let calls: Vec<_> = view.messages.iter().filter_map(|message| {
            if let Message::Assistant(message) = message {Some(message.tool_calls())}else{None}
        }).flatten().collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, expected_invalid);
        assert!(calls[0].arguments_error.is_some());
        assert!(calls[1].arguments_error.is_none());
        assert_eq!(view.executions.len(), 1);
        assert_eq!(view.executions[0].parent_call_id, "valid-call");
        assert_eq!(std::fs::read_to_string(root.path().join("workspace/result.txt")).unwrap(), CONTENT);
        let records = runtime.events(&session.id, 0, 1000).unwrap();
        assert_eq!(records.iter().filter(|row| matches!(&row.event, CoreEvent::CapabilitySettled{call} if call.name=="fs.write")).count(), 1);
        let ledger = runtime.model_requests(&session.id).unwrap();
        save("recovery-first-round.json", &json!({"view":view,"events":records,"ledger":ledger,"tasks":runtime.tasks(&session.id).unwrap()}));
        assert!(!ledger.budget_unknown);
        assert_eq!(ledger.requests.len(), 3);
        assert_eq!(ledger.settled_tokens, 45);
        assert_eq!(ledger.reserved_tokens, 0);
        assert!(ledger.requests.iter().all(|request| request.settled_tokens == Some(15)));
        drop(runtime);
        let runtime = open(root.path(), &base, 6);
        let view = runtime.prompt(&session.id, "Confirm the previous result without tools.", CancellationToken::new()).await.unwrap();
        assert_eq!(runtime.tasks(&session.id).unwrap()[1].state, TaskState::Completed);
        assert_eq!(view.executions.len(), 1);
        assert_eq!(runtime.model_requests(&session.id).unwrap().requests.len(), 4);
        let bodies = server.await.unwrap();
        save("recovery-requests.json", &bodies);
        save("recovery-reopened.json", &json!({"view":view,"ledger":runtime.model_requests(&session.id).unwrap(),"tasks":runtime.tasks(&session.id).unwrap()}));
        assert_eq!(bodies.len(), 4);
    }).await.expect("tool argument recovery stalled");
}

#[tokio::test]
async fn invalid_arguments_with_incomplete_response_keep_the_model_reservation() {
    timeout(WATCHDOG, async {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            request(&mut socket).await;
            let wire = response(0, Some(("invalid-incomplete", &json!("bad input"))));
            let prefix = wire.rsplit_once("event: message_stop\n").unwrap().0;
            respond(&mut socket, prefix).await;
        });
        let runtime = open(root.path(), &base, 6);
        let session = runtime.create(None).unwrap();
        let view = runtime
            .prompt(&session.id, "Do the fixture.", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            runtime.tasks(&session.id).unwrap()[0].state,
            TaskState::Failed
        );
        assert!(view.executions.is_empty());
        assert!(
            !view
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResult(_)))
        );
        let ledger = runtime.model_requests(&session.id).unwrap();
        save(
            "incomplete-response.json",
            &json!({"view":view,"ledger":ledger,"tasks":runtime.tasks(&session.id).unwrap()}),
        );
        assert_eq!(ledger.requests.len(), 1);
        assert!(ledger.budget_unknown);
        assert_eq!(ledger.settled_tokens, 0);
        assert_eq!(ledger.reserved_tokens, ledger.token_limit);
        assert!(!root.path().join("workspace/result.txt").exists());
        server.await.unwrap();
    })
    .await
    .expect("incomplete argument response stalled");
}

#[tokio::test]
async fn argument_correction_cannot_exceed_the_existing_token_budget() {
    timeout(WATCHDOG, async {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            request(&mut socket).await;
            respond(
                &mut socket,
                &response(0, Some(("invalid-budget", &json!("bad input")))),
            )
            .await;
            listener
        });
        let runtime = open_with_budget(root.path(), &base, 6, 15);
        let session = runtime.create(None).unwrap();
        let view = runtime
            .prompt(&session.id, "Do the fixture.", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            runtime.tasks(&session.id).unwrap()[0].state,
            TaskState::Failed
        );
        assert!(view.executions.is_empty());
        let ledger = runtime.model_requests(&session.id).unwrap();
        assert_eq!(ledger.settled_tokens, 15);
        assert_eq!(ledger.reserved_tokens, 0);
        assert_eq!(ledger.remaining_tokens, 0);
        assert!(!ledger.budget_unknown);
        assert_eq!(
            ledger
                .requests
                .iter()
                .filter(|request| request.started_cursor.is_some())
                .count(),
            1
        );
        save(
            "token-limit.json",
            &json!({"view":view,"ledger":ledger,"tasks":runtime.tasks(&session.id).unwrap()}),
        );
        let listener = server.await.unwrap();
        drop(listener);
    })
    .await
    .expect("argument correction token limit stalled");
}

#[tokio::test]
async fn repeated_invalid_arguments_stop_at_the_existing_turn_limit_without_execution() {
    timeout(WATCHDOG, async {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for sequence in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let body = request(&mut socket).await;
                if sequence == 1 {
                    assert_eq!(body["messages"][2]["content"][0]["is_error"], true);
                }
                let id = format!("invalid-{sequence}");
                respond(&mut socket, &response(sequence, Some((&id, &json!("bad input"))))).await;
            }
        });
        let runtime = open(root.path(), &base, 2);
        let session = runtime.create(None).unwrap();
        let view = runtime.prompt(&session.id, "Do the fixture.", CancellationToken::new()).await.unwrap();
        assert_eq!(runtime.tasks(&session.id).unwrap()[0].state, TaskState::Cancelled);
        assert!(matches!(view.messages.last(),Some(Message::Assistant(message)) if message.error_message.as_deref().is_some_and(|error|error.contains("max_turns"))));
        assert!(view.executions.is_empty());
        assert!(!root.path().join("workspace/result.txt").exists());
        let results: Vec<_> = view.messages.iter().filter_map(|message| {
            if let Message::ToolResult(result) = message {Some(result)}else{None}
        }).collect();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result|result.is_error&&!result.outcome_unknown));
        let ledger = runtime.model_requests(&session.id).unwrap();
        save("turn-limit.json", &json!({"view":view,"ledger":ledger,"tasks":runtime.tasks(&session.id).unwrap()}));
        assert_eq!(ledger.requests.len(), 2);
        assert!(!ledger.budget_unknown);
        server.await.unwrap();
    }).await.expect("invalid argument turn limit stalled");
}
