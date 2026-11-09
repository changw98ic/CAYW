//! Offline transport fixtures. These tests do not call a model provider.
use cayw_tui::transport;

use serde_json::json;
use std::net::SocketAddr;
use transport::{Request, Transport, Update, Watch};

#[tokio::test]
async fn shutdown_interrupts_reconnect_and_closes_owned_work() {
    let address: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let mut transport = Transport::start(address, "secret fixture token".into());
    let update = tokio::time::timeout(std::time::Duration::from_secs(6), transport.next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(update, Update::Disconnected(_)));
    tokio::time::timeout(std::time::Duration::from_secs(1), transport.shutdown())
        .await
        .unwrap();
    assert!(transport.next().await.is_none());
}

#[tokio::test]
async fn disconnected_budget_check_is_rejected_without_queueing_a_prompt() {
    use cayw_tui::{App, driver::Driver};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let address: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let mut transport = Transport::start(address, "fixture-token".into());
    let mut app = App::default();
    app.set_connected();
    app.upsert_snapshot(json!({"id":"s1","label":"fixture","state":"idle","cursor":0,"messages":[],"pending_approvals":[],"unresolved_calls":[],"frozen":false,"binding":null}), true).unwrap();
    app.input.insert("fixture prompt");
    let commands = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let mut driver = Driver::default();
    driver.dispatch(&mut app, &transport, commands);
    loop {
        let update = tokio::time::timeout(std::time::Duration::from_secs(6), transport.next())
            .await
            .unwrap()
            .unwrap();
        if let Update::Response { request, result } = &update
            && request.method == "model.budget.prepare_prompt"
        {
            assert!(
                result
                    .as_ref()
                    .unwrap_err()
                    .contains("request was not sent")
            );
            driver.update(&mut app, update);
            break;
        }
    }
    assert_eq!(app.input.text(), "fixture prompt");
    assert!(app.selected().unwrap().messages.is_empty());
    assert!(app.selected().unwrap().can_prompt());
    transport
        .watch(Some(Watch {
            session_id: "s1".into(),
            after: 19,
        }))
        .unwrap();
    transport.shutdown().await;
}

use async_trait::async_trait;
use cayw_ipc::{RpcError, RpcHandler, ServerConfig, serve};
use serde_json::Value;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Fixture {
    prompts: AtomicUsize,
    polls: Mutex<Vec<u64>>,
    changed: Notify,
    released: CancellationToken,
    exited: CancellationToken,
    drop_after_admission: bool,
    slow_list: bool,
}

#[async_trait]
impl RpcHandler for Fixture {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method {
            "task.enqueue" => {
                assert_eq!(params["session_id"], "s1");
                assert_eq!(params["text"], "fixture prompt");
                Ok(
                    json!({"id":"original-task","session_id":"s1","state":"queued","worker_id":null,"attempt_id":null,"fence":0}),
                )
            }
            "task.get" => {
                assert_eq!(params["task_id"], "original-task");
                Ok(
                    json!({"id":"original-task","session_id":"s1","state":"running","worker_id":"worker-1","attempt_id":"attempt-1","fence":7}),
                )
            }
            "task.resume" => {
                assert_eq!(params["task_id"], "original-task");
                self.prompts.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_one();
                if self.drop_after_admission {
                    cancel.cancelled().await;
                } else {
                    tokio::select! { _ = self.released.cancelled() => {}, _ = cancel.cancelled() => {} }
                }
                self.exited.cancel();
                Ok(
                    json!({"id":"s1","label":"fixture","state":"idle","cursor":12,"messages":[],"pending_approvals":[],"unresolved_calls":[],"frozen":false,"binding":null}),
                )
            }
            "session.events" => {
                assert_eq!(params["session_id"], "s1");
                let after = params["after"].as_u64().unwrap();
                self.polls.lock().unwrap().push(after);
                if after < 11 {
                    Ok(
                        json!([{"cursor":11,"event":{"type":"agent","event":{"type":"turn_start"}}}]),
                    )
                } else {
                    Ok(json!([]))
                }
            }
            "session.cancel" => {
                assert_eq!(
                    params,
                    json!({"session_id":"s1","task_id":"original-task","worker_id":"worker-1","attempt_id":"attempt-1","fence":7})
                );
                self.released.cancel();
                Ok(json!({"cancelled":true}))
            }
            "approval.decide" => Ok(params),
            "session.get" => Ok(
                json!({"id":"s1","label":"fixture","state":"idle","cursor":12,"messages":[],"pending_approvals":[],"unresolved_calls":[],"frozen":false,"binding":null}),
            ),
            "session.list" if self.slow_list => {
                self.changed.notify_one();
                self.released.cancelled().await;
                Ok(json!([]))
            }
            _ => Err(RpcError::new(
                "fixture_rejected",
                "fixture-token must be redacted",
            )),
        }
    }
}

#[derive(Clone, Copy)]
enum BudgetCleanupTask {
    Queued,
    Running,
    StaleAfterRead,
}

struct BudgetCleanupFixture {
    task: BudgetCleanupTask,
    calls: Mutex<Vec<(String, Value)>>,
}

#[async_trait]
impl RpcHandler for BudgetCleanupFixture {
    async fn call(
        &self,
        method: &str,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_owned(), params.clone()));
        match method {
            "task.enqueue" => {
                assert_eq!(params, json!({"session_id":"s1","text":"budget prompt"}));
                Ok(json!({
                    "id":"budget-task","session_id":"s1","state":"queued",
                    "worker_id":null,"attempt_id":null,"fence":0,"dispatches":0,
                    "holds_slot":false,"cancel_requested":false
                }))
            }
            "model.budget.continue" => {
                assert_eq!(params["session_id"], "s1");
                assert_eq!(params["task_id"], "budget-task");
                Err(RpcError::new(
                    "invalid_params",
                    "fixture continuation grant rejected",
                ))
            }
            "task.get" => {
                assert_eq!(params, json!({"task_id":"budget-task"}));
                match self.task {
                    BudgetCleanupTask::Queued | BudgetCleanupTask::StaleAfterRead => Ok(json!({
                        "id":"budget-task","session_id":"s1","state":"queued",
                        "worker_id":null,"attempt_id":null,"fence":0,"dispatches":0,
                        "holds_slot":false,"cancel_requested":false
                    })),
                    BudgetCleanupTask::Running => Ok(json!({
                        "id":"budget-task","session_id":"s1","state":"running",
                        "worker_id":"worker-1","attempt_id":"attempt-1","fence":7,
                        "dispatches":1,"holds_slot":true,"cancel_requested":false
                    })),
                }
            }
            "session.cancel" => {
                assert!(!matches!(self.task, BudgetCleanupTask::Running));
                assert_eq!(
                    params,
                    json!({
                        "session_id":"s1","task_id":"budget-task","worker_id":null,
                        "attempt_id":null,"fence":0
                    })
                );
                if matches!(self.task, BudgetCleanupTask::StaleAfterRead) {
                    Err(RpcError::new(
                        "stale_task",
                        "fixture task was claimed after task.get",
                    ))
                } else {
                    Ok(json!({"cancelled":true}))
                }
            }
            "task.resume" => panic!("budget cleanup must not resume after a rejected grant"),
            other => Err(RpcError::new(
                "fixture_rejected",
                format!("unexpected budget cleanup method {other}"),
            )),
        }
    }
}

#[derive(Default)]
struct WorkspaceRoutingFixture {
    calls: Mutex<Vec<Value>>,
}

#[async_trait]
impl RpcHandler for WorkspaceRoutingFixture {
    async fn call(
        &self,
        method: &str,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        if method != "client.workspace.call" {
            return Err(RpcError::new(
                "direct_runtime_call",
                format!("TUI bypassed the Client workspace router with {method}"),
            ));
        }
        self.calls.lock().unwrap().push(params.clone());
        let rpc = &params["rpc"];
        match rpc["method"].as_str().unwrap_or("") {
            "session.get" | "task.resume" => Ok(json!({
                "id":"routed-session",
                "label":"routed fixture",
                "state":"idle",
                "cursor":8,
                "messages":[],
                "pending_approvals":[],
                "unresolved_calls":[],
                "frozen":false,
                "binding":null
            })),
            "task.enqueue" => Ok(json!({
                "id":"routed-task",
                "session_id":"routed-session",
                "state":"queued",
                "worker_id":null,
                "attempt_id":null,
                "fence":0
            })),
            "task.get" => Ok(json!({
                "id":"routed-task",
                "session_id":"routed-session",
                "state":"running",
                "worker_id":"routed-worker",
                "attempt_id":"routed-attempt",
                "fence":9
            })),
            "session.cancel" => Ok(json!({"cancelled":true})),
            "session.events" => Ok(json!([
                {"cursor":8,"event":{"type":"agent","event":{"type":"turn_start"}}}
            ])),
            other => Err(RpcError::new(
                "fixture_rejected",
                format!("unexpected routed method {other}"),
            )),
        }
    }
}

async fn server(
    listener: TcpListener,
    fixture: Arc<Fixture>,
) -> (CancellationToken, JoinHandle<std::io::Result<()>>) {
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        serve(
            listener,
            fixture,
            ServerConfig::new("fixture-token"),
            task_cancel,
        )
        .await
    });
    (cancel, task)
}

async fn budget_cleanup_server(
    listener: TcpListener,
    fixture: Arc<BudgetCleanupFixture>,
) -> (CancellationToken, JoinHandle<std::io::Result<()>>) {
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        serve(
            listener,
            fixture,
            ServerConfig::new("fixture-token"),
            task_cancel,
        )
        .await
    });
    (cancel, task)
}

async fn next(transport: &mut Transport) -> Update {
    tokio::time::timeout(Duration::from_secs(8), transport.next())
        .await
        .unwrap()
        .unwrap()
}

async fn connect(transport: &mut Transport) {
    loop {
        if matches!(next(transport).await, Update::Connected) {
            return;
        }
    }
}

async fn response(transport: &mut Transport, tag: u64) -> Result<Value, String> {
    loop {
        if let Update::Response { request, result } = next(transport).await
            && request.tag == tag
        {
            return result;
        }
    }
}

async fn run_budget_cleanup(task: BudgetCleanupTask) -> (String, Vec<(String, Value)>, bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = Arc::new(BudgetCleanupFixture {
        task,
        calls: Mutex::new(Vec::new()),
    });
    let (stop, server) = budget_cleanup_server(listener, fixture.clone()).await;
    let mut transport = Transport::start(address, "fixture-token".into());
    connect(&mut transport).await;
    transport
        .call(Request {
            tag: 40,
            method: "session.prompt_with_model_budget",
            params: json!({
                "session_id":"s1",
                "text":"budget prompt",
                "expected":[],
                "additional_tokens":1,
                "token_limit":1,
                "confirm":true
            }),
        })
        .unwrap();
    let mut disconnected = false;
    let error = loop {
        match next(&mut transport).await {
            Update::TaskAdmitted {
                session_id,
                task_id,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(task_id, "budget-task");
            }
            Update::Disconnected(_) => disconnected = true,
            Update::Response { request, result } if request.tag == 40 => {
                break result.unwrap_err();
            }
            _ => {}
        }
    };
    transport.shutdown().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let calls = fixture.calls.lock().unwrap().clone();
    (error, calls, disconnected)
}

#[tokio::test]
async fn failed_budget_grant_cancels_only_the_exact_unclaimed_task() {
    let (error, calls, disconnected) = run_budget_cleanup(BudgetCleanupTask::Queued).await;
    assert!(!disconnected);
    assert!(
        error.starts_with("budget_continuation_not_executed:"),
        "{error}"
    );
    assert!(error.ends_with("(outcome_unknown=false)"), "{error}");
    assert!(!error.contains("outcome_unknown=true"), "{error}");
    assert_eq!(
        calls
            .iter()
            .map(|(method, _)| method.as_str())
            .collect::<Vec<_>>(),
        [
            "task.enqueue",
            "model.budget.continue",
            "task.get",
            "session.cancel"
        ]
    );
}

#[tokio::test]
async fn failed_budget_grant_never_cleans_up_a_running_task_as_known() {
    let (error, calls, disconnected) = run_budget_cleanup(BudgetCleanupTask::Running).await;
    assert!(disconnected);
    assert!(
        error.contains("budget_continuation_cleanup_unknown"),
        "{error}"
    );
    assert!(error.contains("outcome_unknown=true"), "{error}");
    assert!(
        !error.contains("budget_continuation_not_executed"),
        "{error}"
    );
    assert!(calls.iter().all(|(method, _)| method != "session.cancel"));
    assert!(calls.iter().all(|(method, _)| method != "task.resume"));
}

#[tokio::test]
async fn stale_cancellation_after_queued_read_keeps_budget_cleanup_unknown() {
    let (error, calls, disconnected) = run_budget_cleanup(BudgetCleanupTask::StaleAfterRead).await;
    assert!(disconnected);
    assert!(
        error.contains("budget_continuation_cleanup_unknown"),
        "{error}"
    );
    assert!(error.contains("outcome_unknown=true"), "{error}");
    assert!(
        !error.contains("budget_continuation_not_executed"),
        "{error}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| method == "session.cancel")
            .count(),
        1
    );
    assert!(calls.iter().all(|(method, _)| method != "task.resume"));
}

#[tokio::test]
async fn real_ipc_prompt_events_approval_and_cancel_run_concurrently_with_consumed_cursor() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = Arc::new(Fixture::default());
    let (stop, server) = server(listener, fixture.clone()).await;
    let mut transport = Transport::start(address, "fixture-token".into());
    connect(&mut transport).await;
    transport
        .call(Request {
            tag: 1,
            method: "session.prompt",
            params: json!({"session_id":"s1","text":"fixture prompt"}),
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), fixture.changed.notified())
        .await
        .unwrap();
    transport
        .watch(Some(Watch {
            session_id: "s1".into(),
            after: 10,
        }))
        .unwrap();
    loop {
        if let Update::Events { watch, result } = next(&mut transport).await {
            assert_eq!(watch.after, 10);
            assert_eq!(result.unwrap()[0]["cursor"], 11);
            break;
        }
    }
    // No polling with cursor 11 occurs until the UI acknowledges the page.
    transport
        .call(Request {
            tag: 2,
            method: "approval.decide",
            params: json!({"session_id":"s1","approval_id":"a1","decision":"allow_task"}),
        })
        .unwrap();
    assert_eq!(
        response(&mut transport, 2).await.unwrap()["decision"],
        "allow_task"
    );
    assert_eq!(&*fixture.polls.lock().unwrap(), &[10]);
    transport
        .watch(Some(Watch {
            session_id: "s1".into(),
            after: 11,
        }))
        .unwrap();
    loop {
        if let Update::Events { watch, result } = next(&mut transport).await {
            assert_eq!(watch.after, 11);
            assert!(result.unwrap().as_array().unwrap().is_empty());
            break;
        }
    }
    transport
        .call(Request {
            tag: 3,
            method: "session.cancel",
            params: json!({"session_id":"s1","task_id":"original-task"}),
        })
        .unwrap();
    let mut prompt_done = false;
    let mut cancel_done = false;
    while !prompt_done || !cancel_done {
        if let Update::Response { request, result } = next(&mut transport).await {
            let result = result.unwrap();
            if request.tag == 1 {
                assert_eq!(result["state"], "idle");
                prompt_done = true;
            }
            if request.tag == 3 {
                assert_eq!(result["cancelled"], true);
                cancel_done = true;
            }
        }
    }
    assert_eq!(fixture.prompts.load(Ordering::SeqCst), 1);
    transport.shutdown().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn non_primary_workspace_routes_all_rpc_and_reconnects_without_replaying_writes() {
    const WORKSPACE_ID: &str =
        "workspace-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = Arc::new(WorkspaceRoutingFixture::default());
    let stop = CancellationToken::new();
    let server_stop = stop.clone();
    let server_fixture = fixture.clone();
    let server = tokio::spawn(async move {
        serve(
            listener,
            server_fixture,
            ServerConfig::new("fixture-token"),
            server_stop,
        )
        .await
    });
    let mut transport =
        Transport::start_for_workspace(address, "fixture-token".into(), Some(WORKSPACE_ID.into()));
    connect(&mut transport).await;

    transport
        .call(Request {
            tag: 10,
            method: "session.get",
            params: json!({"session_id":"routed-session"}),
        })
        .unwrap();
    assert_eq!(
        response(&mut transport, 10).await.unwrap()["id"],
        "routed-session"
    );

    transport
        .call(Request {
            tag: 11,
            method: "session.prompt",
            params: json!({"session_id":"routed-session","text":"routed prompt"}),
        })
        .unwrap();
    assert_eq!(
        response(&mut transport, 11).await.unwrap()["id"],
        "routed-session"
    );

    transport
        .call(Request {
            tag: 12,
            method: "session.cancel",
            params: json!({"session_id":"routed-session","task_id":"routed-task"}),
        })
        .unwrap();
    assert_eq!(
        response(&mut transport, 12).await.unwrap()["cancelled"],
        true
    );

    transport
        .watch(Some(Watch {
            session_id: "routed-session".into(),
            after: 7,
        }))
        .unwrap();
    loop {
        if let Update::Events { watch, result } = next(&mut transport).await {
            assert_eq!(watch.after, 7);
            assert_eq!(result.unwrap()[0]["cursor"], 8);
            break;
        }
    }

    transport.reconnect().unwrap();
    connect(&mut transport).await;
    loop {
        if let Update::Events { watch, result } = next(&mut transport).await {
            assert_eq!(watch.after, 7);
            assert_eq!(result.unwrap()[0]["cursor"], 8);
            break;
        }
    }
    transport
        .call(Request {
            tag: 13,
            method: "session.get",
            params: json!({"session_id":"routed-session"}),
        })
        .unwrap();
    response(&mut transport, 13).await.unwrap();

    transport.shutdown().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let calls = fixture.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 8);
    for envelope in &calls {
        assert_eq!(envelope["workspace_id"], WORKSPACE_ID);
        assert_eq!(envelope.as_object().unwrap().len(), 2);
        assert_eq!(envelope["rpc"].as_object().unwrap().len(), 2);
    }
    let routed = calls
        .iter()
        .map(|envelope| {
            (
                envelope["rpc"]["method"].as_str().unwrap(),
                envelope["rpc"]["params"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        routed,
        vec![
            ("session.get", json!({"session_id":"routed-session"})),
            (
                "task.enqueue",
                json!({"session_id":"routed-session","text":"routed prompt"}),
            ),
            ("task.resume", json!({"task_id":"routed-task"})),
            ("task.get", json!({"task_id":"routed-task"})),
            (
                "session.cancel",
                json!({
                    "session_id":"routed-session",
                    "task_id":"routed-task",
                    "worker_id":"routed-worker",
                    "attempt_id":"routed-attempt",
                    "fence":9
                }),
            ),
            (
                "session.events",
                json!({"session_id":"routed-session","after":7,"limit":100}),
            ),
            (
                "session.events",
                json!({"session_id":"routed-session","after":7,"limit":100}),
            ),
            ("session.get", json!({"session_id":"routed-session"})),
        ]
    );
}

#[tokio::test]
async fn reconnect_retains_consumed_cursor_and_never_replays_an_admitted_prompt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let first = Arc::new(Fixture {
        drop_after_admission: true,
        ..Fixture::default()
    });
    let (stop, server_task) = server(listener, first.clone()).await;
    let mut transport = Transport::start(address, "fixture-token".into());
    connect(&mut transport).await;
    transport
        .watch(Some(Watch {
            session_id: "s1".into(),
            after: 19,
        }))
        .unwrap();
    transport
        .call(Request {
            tag: 4,
            method: "session.prompt",
            params: json!({"session_id":"s1","text":"fixture prompt"}),
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), first.changed.notified())
        .await
        .unwrap();
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(4), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    loop {
        if matches!(next(&mut transport).await, Update::Disconnected(_)) {
            break;
        }
    }
    let second = Arc::new(Fixture::default());
    let listener = TcpListener::bind(address).await.unwrap();
    let (stop, server_task) = server(listener, second.clone()).await;
    transport.reconnect().unwrap();
    connect(&mut transport).await;
    loop {
        if let Update::Events { watch, result } = next(&mut transport).await {
            assert_eq!(watch.after, 19);
            result.unwrap();
            break;
        }
    }
    assert_eq!(first.prompts.load(Ordering::SeqCst), 1);
    assert_eq!(second.prompts.load(Ordering::SeqCst), 0);
    assert_eq!(&*second.polls.lock().unwrap(), &[19]);
    transport.shutdown().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(4), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn shutdown_cancels_active_prompt_and_joins_its_owned_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = Arc::new(Fixture::default());
    let (stop, server) = server(listener, fixture.clone()).await;
    let mut transport = Transport::start(address, "fixture-token".into());
    connect(&mut transport).await;
    transport
        .call(Request {
            tag: 5,
            method: "session.prompt",
            params: json!({"session_id":"s1","text":"fixture prompt"}),
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), fixture.changed.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(4), transport.shutdown())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), fixture.exited.cancelled())
        .await
        .unwrap();
    assert!(transport.next().await.is_none());
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn runtime_rejection_redacts_token_and_keeps_healthy_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, server) = server(listener, Arc::new(Fixture::default())).await;
    let mut transport = Transport::start(address, "fixture-token".into());
    connect(&mut transport).await;
    transport
        .call(Request {
            tag: 6,
            method: "fixture.reject",
            params: json!({}),
        })
        .unwrap();
    assert!(
        !response(&mut transport, 6)
            .await
            .unwrap_err()
            .contains("fixture-token")
    );
    transport
        .call(Request {
            tag: 7,
            method: "session.get",
            params: json!({"session_id":"s1"}),
        })
        .unwrap();
    assert_eq!(response(&mut transport, 7).await.unwrap()["id"], "s1");
    transport.shutdown().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn stale_success_response_from_a_closed_connection_cannot_replace_reconnected_snapshot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = Arc::new(Fixture {
        slow_list: true,
        ..Fixture::default()
    });
    let (stop, server) = server(listener, fixture.clone()).await;
    let mut transport = Transport::start(address, "fixture-token".into());
    connect(&mut transport).await;
    transport
        .call(Request {
            tag: 8,
            method: "session.list",
            params: json!({}),
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), fixture.changed.notified())
        .await
        .unwrap();
    transport.reconnect().unwrap();
    loop {
        match next(&mut transport).await {
            Update::Connected => break,
            Update::Response { request, result } if request.tag == 8 => {
                assert!(result.is_err(), "old connection must not publish a success");
            }
            _ => {}
        }
    }
    fixture.released.cancel();
    transport
        .call(Request {
            tag: 9,
            method: "session.get",
            params: json!({"session_id":"s1"}),
        })
        .unwrap();
    loop {
        if let Update::Response { request, result } = next(&mut transport).await {
            if request.tag == 8 {
                assert!(result.is_err(), "old connection must not publish a success");
            }
            if request.tag == 9 {
                assert_eq!(result.unwrap()["id"], "s1");
                break;
            }
        }
    }
    transport.shutdown().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
