//! Synthetic Messages responses over loopback; no external model is called.
use cayw_ai::{
    AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider, StopReason,
    UsageStatus,
};
use cayw_core::CoreEvent;
use cayw_providers::AnthropicProvider;
use cayw_runtime::{ModelRequestTerminal, Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
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
    response_with_start_usage(sequence, call, json!({"input_tokens":10,"output_tokens":0}))
}

fn response_with_start_usage(
    sequence: usize,
    call: Option<(&str, &Value)>,
    usage: Value,
) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":format!("recovery-{sequence}"),"model":"synthetic-model","type":"message","role":"assistant","content":[],"usage":usage}}),
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

struct CountedProvider {
    adapter: AnthropicProvider,
    calls: AtomicUsize,
}

impl Provider for CountedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.adapter.stream(request, cancel)
    }
}

fn open_with_budget(root: &Path, base: &str, max_turns: usize, tokens: u64) -> Runtime {
    open_with_provider(
        root,
        Arc::new(AnthropicProvider::new(base, "synthetic-recovery-key").unwrap()),
        max_turns,
        tokens,
    )
}

fn open_with_provider(
    root: &Path,
    provider: Arc<dyn Provider>,
    max_turns: usize,
    tokens: u64,
) -> Runtime {
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
        provider,
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

async fn truncated_arguments(reported: bool, evidence: &str) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let arguments = json!("bad input");
    let wire = if reported {
        let complete = response(0, Some(("invalid-incomplete", &arguments)));
        complete
            .split_once("event: message_stop\n")
            .unwrap()
            .0
            .to_owned()
    } else {
        // Explicit zero output already counts as Reported. Omit the field to
        // exercise a genuinely Partial report before message_delta arrives.
        let complete = response_with_start_usage(
            0,
            Some(("invalid-incomplete", &arguments)),
            json!({"input_tokens":10}),
        );
        complete
            .split_once("event: message_delta\n")
            .unwrap()
            .0
            .to_owned()
    };
    save(&format!("{evidence}-wire.json"), &wire);
    let body = wire.clone();
    let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        loop {
            let (mut socket, _) = tokio::select! {
                incoming = listener.accept() => incoming.unwrap(),
                _ = &mut stopped => break,
            };
            requests.push(request(&mut socket).await);
            if requests.len() == 1 {
                respond(&mut socket, &body).await;
            } else {
                respond(&mut socket, &response(requests.len() - 1, None)).await;
            }
        }
        requests
    });
    let provider = Arc::new(CountedProvider {
        adapter: AnthropicProvider::new(&base, "synthetic-recovery-key").unwrap(),
        calls: AtomicUsize::new(0),
    });
    let runtime = open_with_provider(root.path(), provider.clone(), 6, 1000);
    let session = runtime.create(None).unwrap();
    let view = runtime
        .prompt(&session.id, "Do the fixture.", CancellationToken::new())
        .await
        .unwrap();
    let failed = runtime.tasks(&session.id).unwrap()[0].clone();
    let ledger = runtime.model_requests(&session.id).unwrap();
    let records = runtime.events(&session.id, 0, 1000).unwrap();
    save(
        &format!("{evidence}-failed.json"),
        &json!({"view":view,"events":records,"ledger":ledger,"task":failed,
            "provider_calls":provider.calls.load(Ordering::SeqCst)}),
    );
    assert_eq!(failed.state, TaskState::Failed);
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("before message_stop")
    );
    assert!(view.executions.is_empty());
    assert!(
        !view
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)))
    );
    assert_eq!(
        records
            .iter()
            .filter(|row| matches!(&row.event, CoreEvent::CapabilitySettled { call } if call.name == "fs.write"))
            .count(),
        0
    );
    assert!(!root.path().join("workspace/result.txt").exists());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(ledger.requests.len(), 1);
    let first = &ledger.requests[0];
    assert!(first.started_cursor.is_some());
    assert_eq!(first.terminal, Some(ModelRequestTerminal::Failed));
    let observation = &first.observations.last().unwrap().observation;
    assert_eq!(observation.event, "error");
    assert_eq!(observation.stop_reason, StopReason::Error);
    assert_eq!(observation.usage.input, 10);
    assert_eq!(observation.usage_fields["input_tokens"], 10);
    if reported {
        assert_eq!(observation.usage_status, UsageStatus::Reported);
        assert_eq!(observation.usage.output, 5);
        assert_eq!(observation.usage.total_tokens, 15);
        assert_eq!(observation.usage_fields["output_tokens"], 5);
        assert_eq!(first.settled_tokens, Some(15));
        assert_eq!(first.outstanding_reservation_tokens, 0);
        assert_eq!(ledger.settled_tokens, 15);
        assert_eq!(ledger.reserved_tokens, 0);
        assert!(!ledger.budget_unknown);
    } else {
        assert_eq!(observation.usage_status, UsageStatus::Partial);
        assert_eq!(observation.usage.output, 0);
        assert_eq!(observation.usage.total_tokens, 10);
        assert!(!observation.usage_fields.contains_key("output_tokens"));
        assert!(
            first
                .observations
                .iter()
                .all(|entry| entry.observation.usage_status != UsageStatus::Reported)
        );
        assert_eq!(first.settled_tokens, None);
        assert_eq!(first.outstanding_reservation_tokens, ledger.token_limit);
        assert_eq!(ledger.settled_tokens, 0);
        assert_eq!(ledger.reserved_tokens, ledger.token_limit);
        assert!(ledger.budget_unknown);
    }
    drop(runtime);
    let reopened = open_with_provider(root.path(), provider.clone(), 6, 1000);
    let reopened_view = reopened.get(&session.id).unwrap();
    let reopened_ledger = reopened.model_requests(&session.id).unwrap();
    save(
        &format!("{evidence}-reopened.json"),
        &json!({"view":reopened_view,"events":reopened.events(&session.id,0,1000).unwrap(),
            "ledger":reopened_ledger,"task":reopened.task(&failed.id).unwrap(),
            "provider_calls":provider.calls.load(Ordering::SeqCst)}),
    );
    assert_eq!(reopened_ledger, ledger);
    assert_eq!(reopened_view.messages, view.messages);
    assert_eq!(reopened.task(&failed.id).unwrap().state, TaskState::Failed);
    let next = reopened
        .enqueue(&session.id, "Continue without executing the failed tool.")
        .unwrap();
    let after_view = reopened
        .resume(&next.id, CancellationToken::new())
        .await
        .unwrap();
    let after = reopened.model_requests(&session.id).unwrap();
    let after_records = reopened.events(&session.id, 0, 1000).unwrap();
    let _ = stop.send(());
    let requests = server.await.unwrap();
    save(
        &format!("{evidence}-next-task.json"),
        &json!({"view":after_view,"events":after_records,"ledger":after,
            "tasks":reopened.tasks(&session.id).unwrap(),"requests":requests,
            "provider_calls":provider.calls.load(Ordering::SeqCst)}),
    );
    assert_eq!(after.requests[0], *first);
    assert_eq!(reopened.task(&failed.id).unwrap().state, TaskState::Failed);
    assert!(after_view.executions.is_empty());
    assert!(
        !after_view
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)))
    );
    assert_eq!(
        after_records
            .iter()
            .filter(|row| matches!(&row.event, CoreEvent::CapabilitySettled { call } if call.name == "fs.write"))
            .count(),
        0
    );
    assert!(!root.path().join("workspace/result.txt").exists());
    if reported {
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "the settled failed response must allow the next Provider request"
        );
        assert_eq!(reopened.task(&next.id).unwrap().state, TaskState::Completed);
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]["system"]
                .to_string()
                .contains("Runtime input notice")
        );
        assert!(
            !requests[1]["messages"]
                .to_string()
                .contains("invalid-incomplete")
        );
        assert!(after_view.messages.starts_with(&view.messages));
        assert_eq!(after.requests.len(), 2);
        let next_request = &after.requests[1];
        assert!(next_request.started_cursor.is_some());
        assert_eq!(next_request.terminal, Some(ModelRequestTerminal::Completed));
        assert_eq!(next_request.settled_tokens, Some(15));
        assert_eq!(next_request.outstanding_reservation_tokens, 0);
        assert_eq!(after.settled_tokens, 30);
        assert_eq!(after.reserved_tokens, 0);
        assert!(!after.budget_unknown);
    } else {
        assert_eq!(reopened.task(&next.id).unwrap().state, TaskState::Failed);
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "the held reservation must reject before the next Provider request"
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(after.requests.len(), 2);
        let rejected = &after.requests[1];
        assert_eq!(rejected.terminal, Some(ModelRequestTerminal::Rejected));
        assert!(rejected.started_cursor.is_none());
        assert_eq!(
            rejected.rejection.as_deref(),
            Some("unsettled_model_reservation")
        );
        assert_eq!(rejected.settled_tokens, None);
        assert_eq!(rejected.outstanding_reservation_tokens, 0);
        assert!(rejected.observations.is_empty());
        assert_eq!(after.settled_tokens, 0);
        assert_eq!(after.reserved_tokens, ledger.token_limit);
        assert!(after.budget_unknown);
    }
}

#[tokio::test]
async fn invalid_arguments_without_message_stop_settle_reported_usage_after_reopen() {
    timeout(WATCHDOG, truncated_arguments(true, "reported-incomplete"))
        .await
        .expect("reported truncated argument response stalled");
}

#[tokio::test]
async fn invalid_arguments_before_message_delta_keep_partial_usage_reserved_after_reopen() {
    timeout(WATCHDOG, truncated_arguments(false, "partial-incomplete"))
        .await
        .expect("partial truncated argument response stalled");
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
