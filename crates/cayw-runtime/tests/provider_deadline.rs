//! Synthetic responses over real loopback HTTP and the public persistent Runtime.
//! Runtime deadlines end the tasks; no user cancellation or live service is used.
use cayw_ai::{
    AssistantStream, CancellationToken, Model, ModelRequest, Provider, UsageStatus,
    providers::OpenAiProvider,
};
use cayw_core::CoreEvent;
use cayw_runtime::{
    ModelCostStatus, ModelRequestLedger, ModelRequestTerminal, Runtime, RuntimeConfig,
    RuntimeError, TaskState,
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
    sync::oneshot,
    time::timeout,
};

const RUN_TIME: Duration = Duration::from_secs(5);
const WATCHDOG: Duration = Duration::from_secs(60);
const TOKEN_LIMIT: u64 = 1000;
const MODEL: &str = "deadline-fixture";

#[derive(Clone, Copy, Debug)]
enum Stage {
    Headers,
    ReportedUsage,
}
struct HttpProvider {
    adapter: OpenAiProvider,
    calls: AtomicUsize,
}
impl Provider for HttpProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.adapter.stream(request, cancel)
    }
}
fn open(root: &Path, provider: Arc<HttpProvider>) -> Runtime {
    let mut model = Model::new("openai", MODEL);
    model.max_output_tokens = Some(16);
    Runtime::open(
        root.join("session.db"),
        root.join("workspace"),
        provider,
        model,
        RuntimeConfig {
            max_run_time: RUN_TIME,
            max_total_tokens: TOKEN_LIMIT,
            ..Default::default()
        },
    )
    .unwrap()
}
async fn read_request(socket: &mut TcpStream) {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "HTTP request ended before its headers");
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 128 * 1024);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    assert!(length <= 128 * 1024);
    while bytes.len() < end + length {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "HTTP request ended before its body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    let request: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
    assert_eq!(request["model"], MODEL);
}
fn assert_unknown_money(ledger: &ModelRequestLedger) {
    assert_eq!(ledger.cost_status, ModelCostStatus::Unknown);
    assert!(ledger.cost_amount.is_none() && ledger.cost_currency.is_none());
    for record in &ledger.requests {
        assert_eq!(record.cost_status, ModelCostStatus::Unknown);
        assert!(record.cost_amount.is_none() && record.cost_currency.is_none());
    }
}
async fn observed_usage(runtime: &Runtime, session: &str) {
    let mut cursor = 0;
    loop {
        let page = runtime
            .wait_events(
                session,
                cursor,
                1000,
                Duration::from_secs(1),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        for event in page {
            cursor = event.cursor;
            if let CoreEvent::Host { kind, data } = event.event
                && kind == "model_request"
                && data["record"] == "observed"
                && data["observation"]["event"] == "usage_update"
                && data["observation"]["usage_status"] == "reported"
            {
                assert_eq!(data["observation"]["usage"]["total_tokens"], 18);
                return;
            }
        }
    }
}
async fn deadline_case(stage: Stage) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = Arc::new(HttpProvider {
        adapter: OpenAiProvider::new(
            format!("http://{}/v1", listener.local_addr().unwrap()),
            "fixed-local-fake-key",
        )
        .unwrap(),
        calls: AtomicUsize::new(0),
    });
    let runtime = open(root.path(), provider.clone());
    let session = runtime.create(None).unwrap().id;
    let admitted = runtime
        .enqueue(&session, "wait for the model deadline")
        .unwrap();
    let (entered, request_received) = oneshot::channel();
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        if matches!(stage, Stage::ReportedUsage) {
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
            let usage = format!(
                "data: {}\n\n",
                json!({"id":"deadline-usage-response","choices":[],"usage":{
                    "prompt_tokens":11,"completion_tokens":7,"total_tokens":18
                }})
            );
            socket
                .write_all(format!("{:x}\r\n{usage}\r\n", usage.len()).as_bytes())
                .await
                .unwrap();
        }
        entered.send(()).unwrap();
        // Hold the original connection open without a final chunk or model
        // terminal. Keep accepting until the test has checked reopening too.
        tokio::select! { biased;
            connection = listener.accept() => {
                let _ = connection.unwrap();
                panic!("a deadline or reopen initiated a second HTTP connection");
            }
            result = stopped => { result.unwrap(); }
        }
        drop(socket);
        1_usize
    });
    let user_cancel = CancellationToken::new();
    let (held, result) = {
        let run = runtime.resume(&admitted.id, user_cancel.clone());
        tokio::pin!(run);
        tokio::select! {
            received = request_received => { received.unwrap(); }
            result = &mut run => panic!("task ended before the HTTP waiting stage: {result:?}"),
        }
        if matches!(stage, Stage::ReportedUsage) {
            tokio::select! { biased;
                _ = observed_usage(&runtime, &session) => {}
                result = &mut run => panic!("task ended before the durable UsageUpdate: {result:?}"),
            }
        }
        let active = runtime.task(&admitted.id).unwrap();
        assert_eq!(active.state, TaskState::Running);
        assert!(!active.cancel_requested && !user_cancel.is_cancelled());
        let held = runtime.model_requests(&session).unwrap();
        assert_eq!(held.requests.len(), 1);
        assert!(held.requests[0].started_cursor.is_some());
        assert!(held.requests[0].terminal.is_none());
        assert_eq!(held.reserved_tokens, TOKEN_LIMIT);

        let result = (&mut run).await;
        (held, result)
    };
    assert!(
        matches!(result, Err(RuntimeError::Budget("time"))),
        "the Runtime deadline must preserve the time-budget reason: {result:?}"
    );
    assert!(!user_cancel.is_cancelled());
    let task = runtime.task(&admitted.id).unwrap();
    assert_eq!(task.state, TaskState::Failed);
    assert!(!task.cancel_requested);
    let time_reason = RuntimeError::Budget("time").to_string();
    assert_eq!(task.error.as_deref(), Some(time_reason.as_str()));
    let audit = runtime.audit(&session, 0, 1000).unwrap();
    let terminals = audit
        .iter()
        .filter(|entry| entry.kind.starts_with("run_"))
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].kind, "run_failed");
    assert_eq!(runtime.health().unwrap().running, 0);

    let ledger = runtime.model_requests(&session).unwrap();
    assert_eq!(ledger.requests.len(), 1);
    assert!(ledger.budget_unknown);
    assert_eq!(ledger.reserved_tokens, TOKEN_LIMIT);
    assert_eq!(ledger.settled_tokens, 0);
    assert_eq!(ledger.remaining_tokens, 0);
    let record = &ledger.requests[0];
    assert_eq!(record.identity, held.requests[0].identity);
    assert_eq!(record.identity.task_id, task.id);
    assert_eq!(record.outstanding_reservation_tokens, TOKEN_LIMIT);
    assert!(record.settled_tokens.is_none());
    // The model stream can be internally cancelled or dropped while the Task
    // retains its distinct time-budget failure. Neither outcome settles tokens.
    assert!(matches!(
        record.terminal,
        Some(ModelRequestTerminal::Cancelled | ModelRequestTerminal::Dropped)
    ));
    match stage {
        Stage::Headers => {
            assert!(record.observations.iter().all(|entry| {
                entry.observation.usage_status == UsageStatus::Unknown
                    && entry.observation.usage_fields.is_empty()
            }));
        }
        Stage::ReportedUsage => {
            let reported = record
                .observations
                .iter()
                .find(|entry| entry.observation.event == "usage_update")
                .expect("the received usage event must remain in the durable ledger");
            assert_eq!(reported.observation.usage_status, UsageStatus::Reported);
            assert_eq!(reported.observation.usage.input, 11);
            assert_eq!(reported.observation.usage.output, 7);
            assert_eq!(reported.observation.usage.total_tokens, 18);
            assert_eq!(reported.observation.usage_fields["prompt_tokens"], 11);
            assert_eq!(reported.observation.usage_fields["completion_tokens"], 7);
            assert_eq!(reported.observation.usage_fields["total_tokens"], 18);
        }
    }
    assert_unknown_money(&ledger);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    drop(runtime);

    let reopened = open(root.path(), provider.clone());
    assert_eq!(reopened.model_requests(&session).unwrap(), ledger);
    assert_eq!(reopened.task(&task.id).unwrap(), task);
    assert!(!user_cancel.is_cancelled());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    reopened
        .prompt(
            &session,
            "a new user task must not clear the deadline reservation",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let after = reopened.model_requests(&session).unwrap();
    assert_eq!(after.reserved_tokens, TOKEN_LIMIT);
    assert_eq!(after.settled_tokens, 0);
    assert!(after.budget_unknown);
    assert_eq!(after.requests.len(), 2);
    assert_eq!(
        after
            .requests
            .iter()
            .find(|entry| entry.identity.request_id == record.identity.request_id),
        Some(record)
    );
    assert!(
        after
            .requests
            .iter()
            .any(|entry| entry.terminal == Some(ModelRequestTerminal::Rejected))
    );
    assert_eq!(reopened.task(&task.id).unwrap(), task);
    assert_unknown_money(&after);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    stop.send(()).unwrap();
    assert_eq!(server.await.unwrap(), 1);
    println!(
        "{stage:?}: Runtime time-budget failure; user cancellation false; original {TOKEN_LIMIT}-token reservation and observations preserved after reopen; one HTTP request; money unknown"
    );
}

#[tokio::test]
async fn runtime_deadline_while_waiting_for_http_headers_preserves_unknown_reservation() {
    timeout(WATCHDOG, deadline_case(Stage::Headers))
        .await
        .expect("HTTP header deadline watchdog");
}

#[tokio::test]
async fn runtime_deadline_after_usage_only_http_event_preserves_known_usage_and_reservation() {
    timeout(WATCHDOG, deadline_case(Stage::ReportedUsage))
        .await
        .expect("HTTP usage deadline watchdog");
}
