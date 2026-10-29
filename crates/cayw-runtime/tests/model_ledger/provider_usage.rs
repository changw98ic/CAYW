//! Synthetic local HTTP evidence for persistent model reservations. No live API.
use cayw_ai::{AssistantStream, CancellationToken, Model, ModelRequest, Provider, UsageStatus};
use cayw_runtime::{
    LocalAgents, ModelCostStatus, ModelRequestLedger, ModelRequestTerminal, Runtime, RuntimeConfig,
    ScopedPermission, TaskState,
};
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

const BOUND: Duration = Duration::from_secs(20);
struct HttpProvider {
    adapter: cayw_ai::providers::OpenAiProvider,
    calls: AtomicUsize,
}
impl Provider for HttpProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.adapter.stream(request, cancel)
    }
}
fn open(root: &Path, provider: Arc<HttpProvider>, limit: u64) -> Runtime {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut model = Model::new("openai", "ledger-fixture");
    model.max_output_tokens = Some(16);
    Runtime::open(
        root.join("session.db"),
        workspace,
        provider,
        model,
        RuntimeConfig {
            max_total_tokens: limit,
            ..Default::default()
        },
    )
    .unwrap()
}
async fn setup(limit: u64) -> (tempfile::TempDir, Runtime, Arc<HttpProvider>, TcpListener) {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = Arc::new(HttpProvider {
        adapter: cayw_ai::providers::OpenAiProvider::new(
            format!("http://{}/v1", listener.local_addr().unwrap()),
            "fixed-local-fake-key",
        )
        .unwrap(),
        calls: AtomicUsize::new(0),
    });
    let runtime = open(temp.path(), provider.clone(), limit);
    (temp, runtime, provider, listener)
}
async fn request(listener: &TcpListener) -> TcpStream {
    let (mut socket, _) = timeout(BOUND, listener.accept()).await.unwrap().unwrap();
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&buffer[..size]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        assert!(bytes.len() < 16 * 1024);
    };
    let head = std::str::from_utf8(&bytes[..header_end]).unwrap();
    assert!(head.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    assert!(length < 128 * 1024);
    while bytes.len() < header_end + length {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&buffer[..size]);
    }
    let body: Value = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
    assert_eq!(body["model"], "ledger-fixture");
    socket
}
fn body(usage: Option<Value>, complete: bool) -> String {
    let mut frames = vec![
        json!({"id":"fixture-response","model":"fixture-response-model","choices":[{"index":0,"delta":{"role":"assistant","content":"done"},"finish_reason":null}]}),
    ];
    if complete {
        frames.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
    }
    if let Some(usage) = usage {
        frames.push(json!({"choices":[],"usage":usage}));
    }
    let mut body: String = frames
        .into_iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect();
    if complete {
        body.push_str("data: [DONE]\n\n");
    }
    body
}
async fn respond(mut socket: TcpStream, status: &str, body: &str) {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await.unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
    socket.shutdown().await.unwrap();
}
fn full_usage(input: u64, output: u64) -> Value {
    json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input.saturating_add(output)})
}
fn assert_unknown_money(ledger: &ModelRequestLedger) {
    assert_eq!(ledger.cost_status, ModelCostStatus::Unknown);
    assert!(ledger.cost_amount.is_none() && ledger.cost_currency.is_none());
    assert!(
        ledger
            .requests
            .iter()
            .all(|record| record.cost_amount.is_none()
                && record.cost_currency.is_none()
                && record.cost_status == ModelCostStatus::Unknown)
    );
}
async fn prompt(runtime: &Runtime, id: &str, text: &str) {
    timeout(BOUND, runtime.prompt(id, text, CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
}
fn fault(root: &Path, predicate: &str) {
    let connection = rusqlite::Connection::open(root.join("session.db")).unwrap();
    connection.execute_batch(&format!("CREATE TRIGGER model_usage_fixture_failure BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='host' AND json_extract(NEW.record,'$.event.kind')='model_request' AND ({predicate}) BEGIN SELECT RAISE(ABORT,'fixture model accounting write failure'); END;")).unwrap();
}

#[tokio::test]
async fn reported_success_settles_tokens_and_reopen_preserves_identity_and_unknown_money() {
    let (temp, runtime, provider, listener) = setup(1000).await;
    let session = runtime.create(None).unwrap();
    let inspect = runtime.clone();
    let session_id = session.id.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let socket = request(&listener).await;
            let ledger = inspect.model_requests(&session_id).unwrap();
            assert_eq!(
                ledger.reserved_tokens,
                ledger.token_limit - ledger.settled_tokens
            );
            let active = ledger
                .requests
                .iter()
                .find(|record| record.terminal.is_none())
                .unwrap();
            assert!(active.reservation_cursor.is_some() && active.started_cursor.is_some());
            assert_eq!(active.identity.lease.session_id, session_id);
            respond(socket, "200 OK", &body(Some(full_usage(11, 7)), true)).await;
        }
    });
    prompt(&runtime, &session.id, "first user turn").await;
    prompt(&runtime, &session.id, "second user turn").await;
    server.await.unwrap();
    let ledger = runtime.model_requests(&session.id).unwrap();
    assert_eq!(ledger.requests.len(), 2);
    assert_eq!(ledger.settled_tokens, 36);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.remaining_tokens, 964);
    assert!(!ledger.budget_unknown);
    assert_unknown_money(&ledger);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    for record in &ledger.requests {
        assert_eq!(record.terminal, Some(ModelRequestTerminal::Completed));
        assert_eq!(record.settled_tokens, Some(18));
        assert_eq!(record.identity.turn, 1);
        assert_eq!(record.identity.provider, "openai");
        assert_eq!(record.identity.model, "ledger-fixture");
        let last = record.observations.last().unwrap();
        assert_eq!(last.observation.usage_status, UsageStatus::Reported);
        assert_eq!(
            last.observation.response_id.as_deref(),
            Some("fixture-response")
        );
        assert_eq!(last.observation.usage_fields["prompt_tokens"], 11);
        assert!(
            record
                .observations
                .iter()
                .any(|observation| observation.observation.event == "usage_update")
        );
        assert!(record.observations.windows(2).all(
            |pair| pair[0].cursor < pair[1].cursor && pair[0].sequence + 1 == pair[1].sequence
        ));
        let task = runtime.task(&record.identity.task_id).unwrap();
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(
            task.attempt_id.as_deref(),
            Some(record.identity.lease.attempt_id.as_str())
        );
    }
    assert_ne!(
        ledger.requests[0].identity.request_id,
        ledger.requests[1].identity.request_id
    );
    assert_ne!(
        ledger.requests[0].identity.task_id,
        ledger.requests[1].identity.task_id
    );
    drop(runtime);
    let reopened = open(temp.path(), provider.clone(), 2000);
    let raised = reopened.model_requests(&session.id).unwrap();
    assert_eq!(
        raised.token_limit, 2000,
        "a trusted configured allocation raises the same session tree"
    );
    assert_eq!(
        raised.settled_tokens, ledger.settled_tokens,
        "raising configuration must not reset consumed tokens"
    );
    assert_eq!(raised.reserved_tokens, 0);
    assert_eq!(raised.remaining_tokens, 1964);
    assert!(!raised.budget_unknown);
    assert_eq!(
        raised.requests, ledger.requests,
        "historical records keep their original token limits and usage"
    );
    assert_unknown_money(&raised);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn trusted_configuration_increase_continues_the_same_session_after_settlement() {
    let (temp, runtime, provider, listener) = setup(1000).await;
    let session = runtime.create(None).unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            respond(
                request(&listener).await,
                "200 OK",
                &body(Some(full_usage(11, 7)), true),
            )
            .await;
        }
    });
    prompt(&runtime, &session.id, "first settled request").await;
    let before = runtime.model_requests(&session.id).unwrap();
    assert_eq!(before.token_limit, 1000);
    assert_eq!(before.settled_tokens, 18);
    assert_eq!(before.remaining_tokens, 982);
    assert_eq!(before.reserved_tokens, 0);
    drop(runtime);
    // The trusted host explicitly increases the same session tree's allocation
    // after every old request settled. This is not a per-task reset.
    let reopened = open(temp.path(), provider.clone(), 2000);
    let raised = reopened.model_requests(&session.id).unwrap();
    assert_eq!(raised.token_limit, 2000);
    assert_eq!(raised.remaining_tokens, 1982);
    assert_eq!(raised.requests, before.requests);
    prompt(
        &reopened,
        &session.id,
        "continue under the trusted allocation",
    )
    .await;
    server.await.unwrap();
    let after = reopened.model_requests(&session.id).unwrap();
    assert_eq!(after.settled_tokens, 36);
    assert_eq!(after.reserved_tokens, 0);
    assert_eq!(after.remaining_tokens, 1964);
    assert!(!after.budget_unknown);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        after.requests[0].token_limit, 1000,
        "the first record keeps the limit it was admitted under"
    );
    assert_eq!(after.requests[1].token_limit, 2000);
    assert_unknown_money(&after);
}

#[tokio::test]
async fn usage_only_reported_then_cancel_keeps_known_values_and_the_full_reservation() {
    let (temp, runtime, provider, listener) = setup(1000).await;
    let session = runtime.create(None).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = request(&listener).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
        let usage = format!(
            "data: {}\n\n",
            json!({"id":"usage-only-response","choices":[],"usage":full_usage(11,7)})
        );
        socket
            .write_all(format!("{:x}\r\n{usage}\r\n", usage.len()).as_bytes())
            .await
            .unwrap();
        let _ = stopped.await;
    });
    let runner = runtime.clone();
    let id = session.id.clone();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let task =
        tokio::spawn(async move { runner.prompt(&id, "cancel after usage only", token).await });
    timeout(BOUND, async {
        loop {
            let ledger = runtime.model_requests(&session.id).unwrap();
            if ledger
                .requests
                .iter()
                .flat_map(|record| &record.observations)
                .any(|observation| {
                    observation.observation.event == "usage_update"
                        && observation.observation.usage_status == UsageStatus::Reported
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    let _ = timeout(BOUND, task).await.unwrap().unwrap();
    let _ = stop.send(());
    server.await.unwrap();
    let ledger = runtime.model_requests(&session.id).unwrap();
    assert!(ledger.budget_unknown);
    assert_eq!(ledger.reserved_tokens, 1000);
    assert_eq!(ledger.settled_tokens, 0);
    let record = &ledger.requests[0];
    assert!(matches!(
        record.terminal,
        Some(ModelRequestTerminal::Cancelled | ModelRequestTerminal::Dropped)
    ));
    assert!(
        record
            .observations
            .iter()
            .any(
                |observation| observation.observation.usage_status == UsageStatus::Reported
                    && observation.observation.usage.total_tokens == 18
            )
    );
    let view = runtime.get(&session.id).unwrap();
    assert!(view.unresolved_calls.is_empty() && view.unresolved_capabilities.is_empty());
    assert_eq!(
        runtime.tasks(&session.id).unwrap()[0].state,
        TaskState::Cancelled
    );
    assert_unknown_money(&ledger);
    drop(runtime);
    let unchanged = open(temp.path(), provider.clone(), 1000);
    assert_eq!(unchanged.model_requests(&session.id).unwrap(), ledger);
    drop(unchanged);
    let reopened = open(temp.path(), provider.clone(), 2000);
    let raised = reopened.model_requests(&session.id).unwrap();
    assert_eq!(raised.token_limit, 2000);
    assert_eq!(raised.reserved_tokens, ledger.reserved_tokens);
    assert_eq!(raised.settled_tokens, ledger.settled_tokens);
    assert_eq!(raised.requests, ledger.requests);
    assert!(raised.budget_unknown);
    assert_eq!(raised.remaining_tokens, 1000);
    prompt(
        &reopened,
        &session.id,
        "new explicit user task cannot clear old unknown usage",
    )
    .await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let after = reopened.model_requests(&session.id).unwrap();
    assert_eq!(after.reserved_tokens, 1000);
    assert_eq!(after.requests.len(), 2);
    assert!(
        after
            .requests
            .iter()
            .any(|record| record.terminal == Some(ModelRequestTerminal::Rejected))
    );
    assert!(
        reopened
            .tasks(&session.id)
            .unwrap()
            .iter()
            .all(|task| task.state != TaskState::Completed)
    );
}

#[tokio::test]
async fn failed_http_usage_settles_only_complete_reported_counts() {
    for truncated in [false, true] {
        let (temp, runtime, provider, listener) = setup(1000).await;
        let session = runtime.create(None).unwrap();
        let server = tokio::spawn(async move {
            let socket = request(&listener).await;
            if truncated {
                respond(socket, "200 OK", &body(Some(full_usage(11, 7)), false)).await;
            } else {
                respond(socket, "429 Too Many Requests", "fixed fixture rate limit").await;
            }
        });
        prompt(&runtime, &session.id, "model failure").await;
        server.await.unwrap();
        let ledger = runtime.model_requests(&session.id).unwrap();
        assert_eq!(ledger.reserved_tokens, if truncated { 0 } else { 1000 });
        assert_eq!(ledger.settled_tokens, if truncated { 18 } else { 0 });
        assert_eq!(ledger.budget_unknown, !truncated);
        assert_eq!(
            ledger.requests[0].terminal,
            Some(ModelRequestTerminal::Failed)
        );
        let last = &ledger.requests[0].observations.last().unwrap().observation;
        assert_eq!(
            last.usage_status,
            if truncated {
                UsageStatus::Reported
            } else {
                UsageStatus::Unknown
            }
        );
        if truncated {
            assert_eq!(last.usage.total_tokens, 18);
        }
        assert_eq!(
            runtime.tasks(&session.id).unwrap()[0].state,
            TaskState::Failed
        );
        assert_unknown_money(&ledger);
        drop(runtime);
        let reopened = open(temp.path(), provider.clone(), 1000);
        assert_eq!(reopened.model_requests(&session.id).unwrap(), ledger);
        if !truncated {
            prompt(&reopened, &session.id, "do not retry the original request").await;
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            reopened.tasks(&session.id).unwrap()[0].state,
            TaskState::Failed
        );
    }
}

#[tokio::test]
async fn missing_partial_and_invalid_usage_never_settle_default_zeroes() {
    for (usage, status) in [
        (None, UsageStatus::Unknown),
        (Some(json!({"prompt_tokens":11})), UsageStatus::Partial),
        (
            Some(json!({"prompt_tokens":11,"completion_tokens":7,"total_tokens":3})),
            UsageStatus::Invalid,
        ),
        (
            Some(json!({"prompt_tokens":u64::MAX,"completion_tokens":7})),
            UsageStatus::Invalid,
        ),
    ] {
        let (_temp, runtime, _provider, listener) = setup(1000).await;
        let session = runtime.create(None).unwrap();
        let server = tokio::spawn(async move {
            respond(request(&listener).await, "200 OK", &body(usage, true)).await;
        });
        prompt(
            &runtime,
            &session.id,
            "availability is not inferred from positive counts",
        )
        .await;
        server.await.unwrap();
        let ledger = runtime.model_requests(&session.id).unwrap();
        assert_eq!(ledger.settled_tokens, 0);
        assert_eq!(ledger.reserved_tokens, 1000);
        assert!(ledger.budget_unknown);
        assert_eq!(
            ledger.requests[0]
                .observations
                .last()
                .unwrap()
                .observation
                .usage_status,
            status
        );
        assert_unknown_money(&ledger);
    }
}

#[tokio::test]
async fn reported_zero_settles_and_over_reservation_retains_the_original_hold() {
    for (input, output, settles) in [(0, 0, true), (1001, 7, false)] {
        let (_temp, runtime, _provider, listener) = setup(1000).await;
        let session = runtime.create(None).unwrap();
        let server = tokio::spawn(async move {
            respond(
                request(&listener).await,
                "200 OK",
                &body(Some(full_usage(input, output)), true),
            )
            .await;
        });
        prompt(
            &runtime,
            &session.id,
            "explicit zero differs from absent usage",
        )
        .await;
        server.await.unwrap();
        let ledger = runtime.model_requests(&session.id).unwrap();
        let record = &ledger.requests[0];
        assert_eq!(record.settled_tokens, settles.then_some(0));
        assert_eq!(ledger.reserved_tokens, if settles { 0 } else { 1000 });
        assert_eq!(
            record.terminal,
            Some(if settles {
                ModelRequestTerminal::Completed
            } else {
                ModelRequestTerminal::BudgetExceeded
            })
        );
        assert_unknown_money(&ledger);
    }
}

#[tokio::test]
async fn exhausted_persistent_budget_never_invokes_a_second_http_request() {
    let (_temp, runtime, provider, listener) = setup(18).await;
    let session = runtime.create(None).unwrap();
    let server = tokio::spawn(async move {
        respond(
            request(&listener).await,
            "200 OK",
            &body(Some(full_usage(11, 7)), true),
        )
        .await;
    });
    prompt(&runtime, &session.id, "consume the fixed token budget").await;
    server.await.unwrap();
    let before = runtime.model_requests(&session.id).unwrap();
    assert_eq!(before.settled_tokens, 18);
    assert_eq!(before.remaining_tokens, 0);
    prompt(
        &runtime,
        &session.id,
        "reject before a second HTTP connection",
    )
    .await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let ledger = runtime.model_requests(&session.id).unwrap();
    assert_eq!(ledger.requests.len(), 2);
    assert_eq!(ledger.reserved_tokens, 0);
    assert!(
        ledger
            .requests
            .iter()
            .any(|record| record.terminal == Some(ModelRequestTerminal::Rejected))
    );
    assert_eq!(
        runtime
            .tasks(&session.id)
            .unwrap()
            .iter()
            .filter(|task| task.state == TaskState::Completed)
            .count(),
        1
    );
}

#[tokio::test]
async fn failed_start_record_never_invokes_the_http_adapter() {
    let (temp, runtime, provider, _listener) = setup(1000).await;
    let session = runtime.create(None).unwrap();
    fault(
        temp.path(),
        "json_extract(NEW.record,'$.event.data.record')='started'",
    );
    assert!(
        timeout(
            BOUND,
            runtime.prompt(&session.id, "reject before HTTP", CancellationToken::new())
        )
        .await
        .unwrap()
        .is_err()
    );
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        0,
        "no Provider::stream means no HTTP request was initiated"
    );
    assert_ne!(
        runtime.tasks(&session.id).unwrap()[0].state,
        TaskState::Completed
    );
    let ledger = runtime.model_requests(&session.id).unwrap();
    assert_eq!(ledger.reserved_tokens, 1000);
    assert!(ledger.budget_unknown);
}

#[tokio::test]
async fn parent_and_child_share_one_persistent_budget_without_duplicate_token_charges() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = Arc::new(HttpProvider {
        adapter: cayw_ai::providers::OpenAiProvider::new(
            format!("http://{}/v1", listener.local_addr().unwrap()),
            "fixed-local-fake-key",
        )
        .unwrap(),
        calls: AtomicUsize::new(0),
    });
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let mut model = Model::new("openai", "ledger-fixture");
    model.max_output_tokens = Some(16);
    let config = RuntimeConfig {
        max_total_tokens: 1000,
        local_agents: Some(LocalAgents {
            shared_total_tokens: 500,
            ..Default::default()
        }),
        permissions: vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"child ledger work"}),
        )],
        ..Default::default()
    };
    let runtime = Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        provider.clone(),
        model.clone(),
        config.clone(),
    )
    .unwrap();
    let session = runtime.create(None).unwrap();
    let server = tokio::spawn(async move {
        let socket = request(&listener).await;
        let arguments=json!({"action":"exec","script":"return cayw.call('agent.run', {prompt='child ledger work'})","yield_time_ms":30000,"max_output_tokens":16384}).to_string();
        let tool = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":"parent-tool-response","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"parent-ledger-call","type":"function","function":{"name":"execute_code","arguments":arguments}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":full_usage(11,7)})
        );
        respond(socket, "200 OK", &tool).await;
        respond(
            request(&listener).await,
            "200 OK",
            &body(Some(full_usage(11, 7)), true),
        )
        .await;
        respond(
            request(&listener).await,
            "200 OK",
            &body(Some(full_usage(11, 7)), true),
        )
        .await;
    });
    prompt(&runtime, &session.id, "run the child through the real Host").await;
    server.await.unwrap();
    let ledger = runtime.model_requests(&session.id).unwrap();
    assert_eq!(ledger.token_limit, 500);
    assert_eq!(ledger.settled_tokens, 54);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.remaining_tokens, 446);
    assert_eq!(ledger.requests.len(), 3);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    let child = ledger
        .requests
        .iter()
        .find(|record| record.identity.lease.session_id != session.id)
        .unwrap();
    assert_eq!(child.identity.budget_session_id, session.id);
    assert_eq!(
        runtime.task(&child.identity.task_id).unwrap().state,
        TaskState::Completed
    );
    assert_eq!(
        runtime
            .model_requests(&child.identity.lease.session_id)
            .unwrap(),
        ledger
    );
    assert!(
        runtime
            .tasks(&session.id)
            .unwrap()
            .iter()
            .all(|task| task.state == TaskState::Completed)
    );
    drop(runtime);
    let reopened = Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        provider.clone(),
        model,
        config,
    )
    .unwrap();
    assert_eq!(reopened.model_requests(&session.id).unwrap(), ledger);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert_unknown_money(&ledger);
}

#[tokio::test]
async fn failed_reservation_and_observation_writes_preserve_failure_and_known_usage() {
    for reservation in [true, false] {
        let (temp, runtime, provider, listener) = setup(1000).await;
        let session = runtime.create(None).unwrap();
        fault(
            temp.path(),
            if reservation {
                "json_extract(NEW.record,'$.event.data.record')='reserved'"
            } else {
                "json_extract(NEW.record,'$.event.data.record')='observed' AND json_extract(NEW.record,'$.event.data.observation.usage_status')='reported'"
            },
        );
        let server = if reservation {
            None
        } else {
            Some(tokio::spawn(async move {
                respond(
                    request(&listener).await,
                    "200 OK",
                    &body(Some(full_usage(11, 7)), true),
                )
                .await;
            }))
        };
        assert!(
            timeout(
                BOUND,
                runtime.prompt(
                    &session.id,
                    "failed accounting cannot lose received usage",
                    CancellationToken::new()
                )
            )
            .await
            .unwrap()
            .is_err()
        );
        if let Some(server) = server {
            server.await.unwrap();
        }
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            usize::from(!reservation)
        );
        assert_ne!(
            runtime.tasks(&session.id).unwrap()[0].state,
            TaskState::Completed
        );
        let ledger = runtime.model_requests(&session.id).unwrap();
        if reservation {
            assert!(
                ledger.requests.is_empty(),
                "provider was never invoked and reservation never committed"
            );
        } else {
            assert_eq!(ledger.reserved_tokens, 1000);
            assert_eq!(ledger.settled_tokens, 0);
            let last = &ledger.requests[0].observations.last().unwrap().observation;
            assert_eq!(last.usage_status, UsageStatus::Reported);
            assert_eq!(last.usage.total_tokens, 18);
            assert_eq!(
                ledger.requests[0].terminal,
                Some(ModelRequestTerminal::RecordingFailed)
            );
            let view = runtime.get(&session.id).unwrap();
            let Some(cayw_ai::Message::Assistant(message)) = view.messages.last() else {
                panic!("missing failed assistant history");
            };
            assert_eq!(message.usage_status, UsageStatus::Reported);
            assert_eq!(message.usage.total_tokens, 18);
            assert_eq!(message.usage_fields["completion_tokens"], 7);
            assert_eq!(message.response_id.as_deref(), Some("fixture-response"));
            assert!(message.stop_reason.is_failure());
            assert!(view.unresolved_calls.is_empty() && view.unresolved_capabilities.is_empty());
        }
        assert_unknown_money(&ledger);
    }
}

#[tokio::test]
async fn failed_terminal_write_cannot_claim_completion_or_release_the_reservation() {
    let (temp, runtime, provider, listener) = setup(1000).await;
    let session = runtime.create(None).unwrap();
    fault(
        temp.path(),
        "json_extract(NEW.record,'$.event.data.record')='finished' AND json_extract(NEW.record,'$.event.data.terminal')='completed'",
    );
    let server = tokio::spawn(async move {
        respond(
            request(&listener).await,
            "200 OK",
            &body(Some(full_usage(11, 7)), true),
        )
        .await;
    });
    assert!(
        timeout(
            BOUND,
            runtime.prompt(
                &session.id,
                "terminal record must commit first",
                CancellationToken::new()
            )
        )
        .await
        .unwrap()
        .is_err()
    );
    server.await.unwrap();
    let ledger = runtime.model_requests(&session.id).unwrap();
    assert_eq!(ledger.settled_tokens, 0);
    assert_eq!(ledger.reserved_tokens, 1000);
    assert_eq!(
        ledger.requests[0].terminal,
        Some(ModelRequestTerminal::RecordingFailed)
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_ne!(
        runtime.tasks(&session.id).unwrap()[0].state,
        TaskState::Completed
    );
    let view = runtime.get(&session.id).unwrap();
    assert!(view.unresolved_calls.is_empty() && view.unresolved_capabilities.is_empty());
    assert_unknown_money(&ledger);
}
