//! Real loopback-TCP acceptance for the headless scheduler and its session
//! boundaries.
//!
//! The provider is an offline fixture. `script_response` emits a real Luau
//! `fs.write`, and the Runtime Host performs it only after the exact permission
//! or explicit approval. The gated response is synthetic and touches no file.
//! Ordering comes from durable task/scheduler state and gate notifications;
//! timeouts only guard against a hang.

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus,
};
use cayw_ipc::{Client, ClientError, ServerConfig, serve};
use cayw_runtime::{
    HeadlessConfig, Runtime, RuntimeConfig, ScopedPermission, fixture::script_response,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Semaphore;

const TOKEN: &str = "scheduler-ipc-acceptance-secret";
const WORKSPACE_FILE: &str = "approved.txt";

/// Optional acceptance receipts. When the variable is unset, tests write
/// nothing. Receipts contain fixture state and capability results only; the
/// authenticated token is never recorded.
fn evidence_root() -> Option<PathBuf> {
    std::env::var("CAYW_SCHEDULER_EVIDENCE_DIR")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn write_evidence(root: &Option<PathBuf>, file: &str, value: &Value) {
    let Some(root) = root else {
        return;
    };
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join(file), serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn synthetic_usage() -> Usage {
    Usage {
        input: 4,
        output: 4,
        total_tokens: 8,
        ..Default::default()
    }
}

fn synthetic_message(request: &ModelRequest, text: &str) -> AssistantMessage {
    let mut message = AssistantMessage::new(&request.model);
    message.usage = synthetic_usage();
    message.usage_status = UsageStatus::Reported;
    message.content.push(Content::text(text));
    message.stop_reason = StopReason::Stop;
    message
}

fn synthetic_failure(request: &ModelRequest, reason: &str) -> AssistantMessage {
    let mut message = AssistantMessage::new(&request.model).failure(StopReason::Aborted, reason);
    message.usage = synthetic_usage();
    message.usage_status = UsageStatus::Reported;
    message
}

fn last_user_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User { .. } => Some(message.text()),
            _ => None,
        })
        .unwrap_or_default()
}

fn done(message: AssistantMessage) -> AssistantStream {
    Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
}

fn failed(message: AssistantMessage) -> AssistantStream {
    Box::pin(futures::stream::iter([AssistantEvent::Error { message }]))
}

/// One notification for the gated provider and one permit that releases it.
struct Gate {
    entered: Semaphore,
    release: Semaphore,
}

impl Gate {
    fn new() -> Self {
        Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
}

struct ScriptedProvider {
    gate: Arc<Gate>,
}

impl Provider for ScriptedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        if cancel.is_cancelled() {
            return failed(synthetic_failure(&request, "scheduler fixture cancelled"));
        }
        let text = last_user_text(&request);
        let call_id = format!("script-{}", request.messages.len());
        match text.as_str() {
            "hold" => {
                let gate = self.gate.clone();
                Box::pin(futures::stream::once(async move {
                    if cancel.is_cancelled() {
                        return AssistantEvent::Error {
                            message: synthetic_failure(&request, "gated fixture cancelled"),
                        };
                    }
                    gate.entered.add_permits(1);
                    tokio::select! {
                        _ = cancel.cancelled() => AssistantEvent::Error {
                            message: synthetic_failure(&request, "gated fixture cancelled"),
                        },
                        permit = gate.release.acquire() => {
                            if let Ok(permit) = permit {
                                permit.forget();
                            }
                            AssistantEvent::Done {
                                message: synthetic_message(&request, "hold-completed"),
                            }
                        }
                    }
                }))
            }
            "approval" => done(script_response(
                &request,
                "return cayw.call('fs.write', {path='approval.txt', content='approval'})",
                &call_id,
            )),
            "approved" => done(script_response(
                &request,
                "return cayw.call('fs.write', {path='approved.txt', content='approved'})",
                &call_id,
            )),
            "value-store" => done(script_response(
                &request,
                "local stored = cayw.call('cells.store', {key='shared', value={owner='s1'}, expected_version=0})\n\
                 local loaded = cayw.call('cells.load', {key='shared'})\n\
                 return loaded",
                &call_id,
            )),
            "value-load" => done(script_response(
                &request,
                "return cayw.call('cells.load', {key='shared'})",
                &call_id,
            )),
            _ => done(synthetic_message(
                &request,
                &format!("plain-completed-{text}"),
            )),
        }
    }
}

async fn create_session(client: &Client, label: &str) -> String {
    client
        .request("session.create", json!({"label": label}))
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn enqueue(client: &Client, session: &str, text: &str, priority: &str) -> Value {
    client
        .request(
            "task.enqueue_scheduled",
            json!({
                "session_id": session,
                "text": text,
                "intent": {"kind": "general"},
                "priority": priority,
            }),
        )
        .await
        .unwrap()
}

fn spawn_resume(
    client: &Client,
    task_id: &str,
) -> tokio::task::JoinHandle<Result<Value, ClientError>> {
    let client = client.clone();
    let task_id = task_id.to_owned();
    tokio::spawn(async move {
        client
            .request("task.resume", json!({"task_id": task_id}))
            .await
    })
}

async fn wait_task(
    client: &Client,
    task_id: &str,
    what: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(task) = client
                .request("task.get", json!({"task_id": task_id}))
                .await
                && predicate(&task)
            {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn wait_session(
    client: &Client,
    session_id: &str,
    what: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(view) = client
                .request("session.get", json!({"session_id": session_id}))
                .await
                && predicate(&view)
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn wait_scheduler(client: &Client, what: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(view) = client
                .request("runtime.scheduler", json!({"limit": 10}))
                .await
                && predicate(&view)
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn wait_drained(client: &Client) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(health) = client.request("runtime.health", json!({})).await
                && health["running"].as_u64() == Some(0)
                && health["queued"].as_u64() == Some(0)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("runtime must drain");
}

fn task_with<'a>(view: &'a Value, task_id: &str) -> Option<&'a Value> {
    view["tasks"]
        .as_array()?
        .iter()
        .find(|task| task["id"].as_str() == Some(task_id))
}

/// Checks the event envelope, cursor monotonicity and tool/capability pairing
/// for exactly one Session and Task. It returns the event types and the set of
/// tool-origin call ids so the caller can assert per-session absence.
fn assert_events_and_pairing(
    records: &[Value],
    session_id: &str,
    task_id: &str,
) -> (BTreeSet<String>, BTreeSet<String>) {
    assert!(!records.is_empty(), "session {session_id} must have events");
    let mut previous = 0;
    let mut types = BTreeSet::new();
    let mut bound_calls = BTreeSet::new();
    let mut parents = BTreeMap::<(String, u64), String>::new();
    let mut stages = BTreeMap::<(String, u64), BTreeSet<String>>::new();
    for record in records {
        assert_eq!(
            record["protocol_version"].as_u64(),
            Some(cayw_core::PROTOCOL_VERSION as u64),
            "event protocol version must match the Core envelope"
        );
        assert_eq!(
            record["session_id"].as_str(),
            Some(session_id),
            "each event must belong to the queried Session"
        );
        let cursor = record["cursor"]
            .as_u64()
            .expect("event cursor must be an unsigned integer");
        assert!(
            cursor > previous,
            "events cursor must increase: previous={previous}, current={cursor}"
        );
        previous = cursor;

        let event = &record["event"];
        let kind = event["type"].as_str().expect("event type");
        types.insert(kind.to_owned());
        match kind {
            "attempt_started" | "attempt_finished" => {
                assert_eq!(event["lease"]["session_id"].as_str(), Some(session_id));
            }
            "tool_origin_bound" => {
                assert_eq!(event["lease"]["session_id"].as_str(), Some(session_id));
                assert_eq!(event["task_id"].as_str(), Some(task_id));
                bound_calls.insert(event["call_id"].as_str().unwrap().to_owned());
            }
            "capability_planned" | "capability_started" | "capability_settled" => {
                let call = &event["call"];
                assert_eq!(
                    call["lease"]["session_id"].as_str(),
                    Some(session_id),
                    "capability lease must stay inside its Session"
                );
                assert_eq!(
                    call["task_id"].as_str(),
                    Some(task_id),
                    "capability must stay inside its Task"
                );
                let key = (
                    call["key"]["execution_id"].as_str().unwrap().to_owned(),
                    call["key"]["sequence"].as_u64().unwrap(),
                );
                parents.insert(
                    key.clone(),
                    call["parent_call_id"].as_str().unwrap().to_owned(),
                );
                stages.entry(key).or_default().insert(kind.to_owned());
            }
            "approval_requested" | "approval_decided" => {
                assert_eq!(event["approval"]["session_id"].as_str(), Some(session_id));
                assert_eq!(
                    event["approval"]["input"]["task_id"].as_str(),
                    Some(task_id)
                );
            }
            _ => {}
        }
    }
    for (key, seen) in stages {
        assert!(
            seen.contains("capability_planned"),
            "planned missing for {key:?}"
        );
        assert!(
            seen.contains("capability_started"),
            "started missing for {key:?}"
        );
        assert!(
            seen.contains("capability_settled"),
            "settled missing for {key:?}"
        );
        let parent = parents
            .get(&key)
            .expect("every capability keeps its model tool origin");
        assert!(
            bound_calls.contains(parent),
            "capability {key:?} must pair with a tool origin bound in the same Session"
        );
    }
    (types, bound_calls)
}

async fn events(client: &Client, session_id: &str) -> Vec<Value> {
    client
        .request(
            "session.events",
            json!({"session_id": session_id, "after": 0, "limit": 1000}),
        )
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn scheduler_ipc_enforces_capacity_fairness_approval_and_session_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let gate = Arc::new(Gate::new());
    let rt = Runtime::open(
        directory.path().join("sessions.sqlite"),
        &workspace,
        Arc::new(ScriptedProvider { gate: gate.clone() }),
        Model::new("fixture", "scheduler-ipc"),
        RuntimeConfig {
            max_concurrent_runs: 1,
            max_queued_runs: 3,
            fairness_after_dispatches: 2,
            max_queue_time: Duration::from_secs(20),
            max_run_time: Duration::from_secs(40),
            permissions: vec![ScopedPermission::exact(
                "fs.write",
                json!({"path": WORKSPACE_FILE, "content": "approved"}),
            )],
            ..Default::default()
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt),
        ServerConfig::new(TOKEN),
        shutdown.clone(),
    ));

    let unauthorized = Client::connect(address, "wrong-token")
        .await
        .expect_err("a wrong token must not authenticate");
    assert_eq!(unauthorized.code, "unauthorized");
    assert!(!unauthorized.outcome_unknown);

    let client = Client::connect(address, TOKEN).await.unwrap();

    let before = client
        .request("runtime.scheduler", json!({"limit": 10}))
        .await
        .unwrap();
    assert_eq!(before["max_concurrent_runs"], 1);
    assert_eq!(before["max_accepted_tasks"], 3);
    assert_eq!(before["fairness_after_dispatches"], 2);
    assert_eq!(before["max_bypasses"], 4);
    assert_eq!(before["dispatch_count"], 0);
    assert!(before["tasks"].as_array().unwrap().is_empty());
    let mut limit_rejections = Vec::new();
    for limit in [0, 1001] {
        let error = client
            .request("runtime.scheduler", json!({"limit": limit}))
            .await
            .expect_err("scheduler limit must be bounded");
        assert_eq!(error.code, "invalid_params");
        assert!(!error.outcome_unknown);
        limit_rejections.push(json!({
            "limit": limit,
            "error_code": error.code,
            "outcome_unknown": error.outcome_unknown,
        }));
    }
    let after_invalid = client
        .request("runtime.scheduler", json!({"limit": 10}))
        .await
        .unwrap();
    assert_eq!(
        after_invalid["dispatch_count"], before["dispatch_count"],
        "a rejected query must not claim a dispatch"
    );

    let bad_priority_session = create_session(&client, "bad priority").await;
    let invalid_priority = client
        .request(
            "task.enqueue_scheduled",
            json!({
                "session_id": bad_priority_session,
                "text": "must not be stored",
                "intent": {"kind": "general"},
                "priority": "urgent",
            }),
        )
        .await
        .expect_err("an unknown priority must be rejected");
    assert_eq!(invalid_priority.code, "invalid_params");
    assert!(!invalid_priority.outcome_unknown);
    assert!(
        client
            .request("session.tasks", json!({"session_id": bad_priority_session}),)
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty(),
        "a rejected priority must not leave a task"
    );

    let approval_session = create_session(&client, "approval wait").await;
    let hold_session = create_session(&client, "normal hold").await;
    let approved_session = create_session(&client, "approved file").await;
    let overflow_session = create_session(&client, "overflow").await;

    let approval_task = enqueue(&client, &approval_session, "approval", "normal").await;
    let approval_task_id = approval_task["id"].as_str().unwrap().to_owned();
    let approval_handle = spawn_resume(&client, &approval_task_id);
    let parked_task = wait_task(
        &client,
        &approval_task_id,
        "the approval task to release its slot",
        |task| task["state"] == "waiting_approval" && task["holds_slot"] == false,
    )
    .await;
    let waiting = wait_session(
        &client,
        &approval_session,
        "the original approval request",
        |view| !view["pending_approvals"].as_array().unwrap().is_empty(),
    )
    .await;
    let approval = waiting["pending_approvals"][0].clone();
    let approval_id = approval["id"].as_str().unwrap().to_owned();
    assert_eq!(approval["input"]["task_id"], approval_task_id);
    assert!(approval["decision"].is_null());
    assert!(!workspace.join("approval.txt").exists());
    assert!(!workspace.join(WORKSPACE_FILE).exists());
    let parked_scheduler = client
        .request("runtime.scheduler", json!({"limit": 10}))
        .await
        .unwrap();

    let hold_task = enqueue(&client, &hold_session, "hold", "normal").await;
    let hold_task_id = hold_task["id"].as_str().unwrap().to_owned();
    let hold_handle = spawn_resume(&client, &hold_task_id);
    tokio::time::timeout(Duration::from_secs(10), gate.entered.acquire())
        .await
        .expect("the normal task must reach the provider")
        .unwrap()
        .forget();
    wait_task(
        &client,
        &hold_task_id,
        "the normal task to hold the only slot",
        |task| task["state"] == "running" && task["holds_slot"] == true,
    )
    .await;

    let approved_task = enqueue(&client, &approved_session, "approved", "normal").await;
    let approved_task_id = approved_task["id"].as_str().unwrap().to_owned();
    let approved_handle = spawn_resume(&client, &approved_task_id);
    wait_scheduler(
        &client,
        "the approved task to become ready behind the held slot",
        |view| {
            task_with(view, &approved_task_id)
                .is_some_and(|task| task["state"] == "queued" && !task["ready_order"].is_null())
        },
    )
    .await;
    assert!(
        !workspace.join(WORKSPACE_FILE).exists(),
        "one slot must serialize the approved write behind the held task"
    );

    let overflow = client
        .request(
            "task.enqueue_scheduled",
            json!({
                "session_id": overflow_session,
                "text": "overflow",
                "intent": {"kind": "general"},
                "priority": "high",
            }),
        )
        .await
        .expect_err("a fourth live task must hit the accepted capacity");
    assert_eq!(overflow.code, "queue_full");
    assert!(!overflow.outcome_unknown);
    assert!(
        client
            .request("session.tasks", json!({"session_id": overflow_session}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty(),
        "queue_full must not create a task"
    );
    let full = client
        .request("runtime.scheduler", json!({"limit": 10}))
        .await
        .unwrap();
    assert_eq!(full["tasks"].as_array().unwrap().len(), 3);
    assert_eq!(
        full["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|task| task["holds_slot"] == true)
            .count(),
        1,
        "K=1 must be visible in the scheduler view"
    );
    assert!(
        full["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["session_id"] != overflow_session.as_str()),
        "queue_full must not leave a live task for the rejected Session"
    );

    let wrong_cancel = json!({
        "session_id": hold_session,
        "task_id": approval_task_id,
        "worker_id": parked_task["worker_id"],
        "attempt_id": parked_task["attempt_id"],
        "fence": parked_task["fence"],
    });
    let wrong_cancel_error = client
        .request("session.cancel", wrong_cancel.clone())
        .await
        .expect_err("a cancellation for another Session must be stale");
    assert_eq!(wrong_cancel_error.code, "stale_cancellation");
    assert!(!wrong_cancel_error.outcome_unknown);

    let wrong_approval_error = client
        .request(
            "approval.decide",
            json!({
                "session_id": hold_session,
                "approval_id": approval_id,
                "decision": "allow_once",
            }),
        )
        .await
        .expect_err("an approval for another Session must be stale");
    assert_eq!(wrong_approval_error.code, "stale_approval");
    assert!(!wrong_approval_error.outcome_unknown);
    let cross_session_control = json!({
        "wrong_cancel": {
            "request": wrong_cancel,
            "error_code": wrong_cancel_error.code,
            "error_message": wrong_cancel_error.message,
            "outcome_unknown": wrong_cancel_error.outcome_unknown,
        },
        "wrong_approval": {
            "request": {
                "session_id": hold_session,
                "approval_id": approval_id,
                "decision": "allow_once",
            },
            "error_code": wrong_approval_error.code,
            "error_message": wrong_approval_error.message,
            "outcome_unknown": wrong_approval_error.outcome_unknown,
        },
    });

    let unchanged_task = client
        .request("task.get", json!({"task_id": approval_task_id}))
        .await
        .unwrap();
    assert_eq!(
        unchanged_task, parked_task,
        "wrong-Session control calls must not alter the original task"
    );
    let unchanged_session = client
        .request("session.get", json!({"session_id": approval_session}))
        .await
        .unwrap();
    assert_eq!(
        unchanged_session["pending_approvals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(unchanged_session["pending_approvals"][0]["id"], approval_id);

    gate.release.add_permits(1);
    let hold_result = tokio::time::timeout(Duration::from_secs(20), hold_handle)
        .await
        .expect("the held task must finish")
        .unwrap()
        .expect("task.resume must succeed");
    assert_eq!(hold_result["id"], hold_session);
    let hold_terminal = wait_task(&client, &hold_task_id, "the held task terminal", |task| {
        task["state"] == "completed"
    })
    .await;
    assert_eq!(hold_terminal["holds_slot"], false);

    let approved_result = tokio::time::timeout(Duration::from_secs(20), approved_handle)
        .await
        .expect("the approved task must finish")
        .unwrap()
        .expect("task.resume must succeed");
    assert_eq!(approved_result["id"], approved_session);
    let approved_file = std::fs::read_to_string(workspace.join(WORKSPACE_FILE)).unwrap();
    assert_eq!(approved_file, "approved");
    let approved_terminal = wait_task(
        &client,
        &approved_task_id,
        "the approved task terminal",
        |task| task["state"] == "completed",
    )
    .await;
    assert_eq!(approved_terminal["holds_slot"], false);

    let decided = client
        .request(
            "approval.decide",
            json!({
                "session_id": approval_session,
                "approval_id": approval_id,
                "decision": "allow_once",
            }),
        )
        .await
        .unwrap();
    assert_eq!(decided["decision"]["actor"], "rpc-user");
    assert_eq!(decided["decision"]["decision"], "approve");

    let approval_result = tokio::time::timeout(Duration::from_secs(20), approval_handle)
        .await
        .expect("the approved wait must finish")
        .unwrap()
        .expect("task.resume must succeed");
    assert_eq!(approval_result["id"], approval_session);
    let approval_file = std::fs::read_to_string(workspace.join("approval.txt")).unwrap();
    assert_eq!(approval_file, "approval");
    let approval_terminal = client
        .request("task.get", json!({"task_id": approval_task_id}))
        .await
        .unwrap();
    assert_eq!(approval_terminal["state"], "completed");
    assert_eq!(approval_terminal["attempt_id"], parked_task["attempt_id"]);
    assert_eq!(approval_terminal["worker_id"], parked_task["worker_id"]);
    assert_eq!(approval_terminal["fence"], parked_task["fence"]);
    assert_eq!(approval_terminal["holds_slot"], false);
    assert!(
        approval_terminal["dispatches"].as_u64().unwrap() >= 2,
        "waiting for approval must require a second dispatch"
    );

    let approval_events = events(&client, &approval_session).await;
    let hold_events = events(&client, &hold_session).await;
    let approved_events = events(&client, &approved_session).await;
    let (approval_types, approval_calls) =
        assert_events_and_pairing(&approval_events, &approval_session, &approval_task_id);
    let (hold_types, hold_calls) =
        assert_events_and_pairing(&hold_events, &hold_session, &hold_task_id);
    let (approved_types, approved_calls) =
        assert_events_and_pairing(&approved_events, &approved_session, &approved_task_id);
    assert!(approval_types.contains("approval_requested"));
    assert!(approval_types.contains("approval_decided"));
    assert!(approval_types.contains("capability_settled"));
    assert!(!approval_calls.is_empty());
    assert!(!hold_types.contains("approval_requested"));
    assert!(!hold_types.contains("capability_planned"));
    assert!(hold_calls.is_empty());
    assert!(approved_types.contains("capability_settled"));
    assert!(!approved_types.contains("approval_requested"));
    assert!(!approved_calls.is_empty());

    for (session, task_id) in [
        (&approval_session, &approval_task_id),
        (&hold_session, &hold_task_id),
        (&approved_session, &approved_task_id),
    ] {
        let tasks = client
            .request("session.tasks", json!({"session_id": session}))
            .await
            .unwrap();
        let tasks = tasks.as_array().unwrap();
        assert_eq!(tasks.len(), 1, "each Session owns exactly its own task");
        assert_eq!(tasks[0]["id"], task_id.as_str());
        assert_eq!(tasks[0]["session_id"], session.as_str());
    }

    let approval_grants = client
        .request("session.grants", json!({"session_id": approval_session}))
        .await
        .unwrap();
    let approval_grants = approval_grants.as_array().unwrap();
    assert_eq!(approval_grants.len(), 1);
    assert_eq!(
        approval_grants[0]["session_id"].as_str(),
        Some(approval_session.as_str())
    );
    assert_eq!(
        approval_grants[0]["approval_id"].as_str(),
        Some(approval_id.as_str())
    );
    assert_eq!(approval_grants[0]["capability"], "fs.write");
    for session in [&hold_session, &approved_session] {
        assert!(
            client
                .request("session.grants", json!({"session_id": session}))
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty(),
            "a grant/ledger entry must not leak to another Session"
        );
    }

    for (session, task_id) in [
        (&approval_session, &approval_task_id),
        (&hold_session, &hold_task_id),
        (&approved_session, &approved_task_id),
    ] {
        let audit = client
            .request(
                "session.audit",
                json!({"session_id": session, "after": 0, "limit": 1000}),
            )
            .await
            .unwrap();
        let audit = audit.as_array().unwrap();
        assert!(
            audit.iter().any(|record| record["kind"] == "run_finished"),
            "each completed Session keeps its own terminal ledger"
        );
        for record in audit {
            assert_eq!(record["session_id"], session.as_str());
            if let Some(owner) = record["task_id"].as_str() {
                assert_eq!(owner, task_id.as_str());
            }
        }
    }

    let final_view = client
        .request("runtime.scheduler", json!({"limit": 10}))
        .await
        .unwrap();
    assert!(
        final_view["tasks"].as_array().unwrap().is_empty(),
        "terminal history must leave the live scheduler view"
    );
    assert!(final_view["dispatch_count"].as_u64().unwrap() >= 4);
    wait_drained(&client).await;
    let final_health = client.request("runtime.health", json!({})).await.unwrap();

    let mut session_evidence = BTreeMap::new();
    for (role, session, session_events) in [
        ("approval_wait", &approval_session, &approval_events),
        ("normal_hold", &hold_session, &hold_events),
        ("approved_file", &approved_session, &approved_events),
    ] {
        let tasks = client
            .request("session.tasks", json!({"session_id": session}))
            .await
            .unwrap();
        let grants = client
            .request("session.grants", json!({"session_id": session}))
            .await
            .unwrap();
        let audit = client
            .request(
                "session.audit",
                json!({"session_id": session, "after": 0, "limit": 1000}),
            )
            .await
            .unwrap();
        session_evidence.insert(
            session.clone(),
            json!({
                "role": role,
                "session_id": session,
                "events": session_events,
                "tasks": tasks,
                "grants": grants,
                "audit": audit,
            }),
        );
    }
    let bad_priority_tasks = client
        .request("session.tasks", json!({"session_id": bad_priority_session}))
        .await
        .unwrap();
    let overflow_tasks = client
        .request("session.tasks", json!({"session_id": overflow_session}))
        .await
        .unwrap();
    let receipt = json!({
        "scenario": "scheduler_ipc_main",
        "fixture_provider": {
            "kind": "synthetic",
            "response_source": "cayw_runtime::fixture::script_response",
            "scripted_effects": ["approval.txt", WORKSPACE_FILE],
            "note": "offline fixture; no real model or network call",
        },
        "transport": {
            "kind": "authenticated-loopback-tcp",
            "rpc_protocol_version": cayw_ipc::PROTOCOL_VERSION,
            "core_protocol_version": cayw_core::PROTOCOL_VERSION,
        },
        "configuration": {
            "max_concurrent_runs": 1,
            "max_accepted_tasks": 3,
            "fairness_after_dispatches": 2,
            "max_queue_time_ms": 20_000,
            "max_run_time_ms": 40_000,
            "exact_permission": {"capability": "fs.write", "path": WORKSPACE_FILE, "content": "approved"},
        },
        "priority_inputs": {
            "invalid_priority": "urgent",
            "approval_task": "normal",
            "normal_hold_task": "normal",
            "approved_task": "normal",
            "overflow_task": "high",
        },
        "rejected_inputs": {
            "authentication": {
                "error_code": unauthorized.code,
                "outcome_unknown": unauthorized.outcome_unknown,
            },
            "invalid_priority": {
                "request_priority": "urgent",
                "error_code": invalid_priority.code,
                "error_message": invalid_priority.message,
                "outcome_unknown": invalid_priority.outcome_unknown,
                "session_id": bad_priority_session,
                "tasks_after": bad_priority_tasks,
            },
            "scheduler_limits": limit_rejections,
            "queue_full": {
                "request_priority": "high",
                "error_code": overflow.code,
                "error_message": overflow.message,
                "outcome_unknown": overflow.outcome_unknown,
                "session_id": overflow_session,
                "tasks_after": overflow_tasks,
            },
        },
        "scheduler_limit_query": {
            "before": before,
            "after_invalid_limits": after_invalid,
        },
        "parked_scheduler": parked_scheduler,
        "parked_task": parked_task,
        "parked_pending_approval": approval,
        "full_scheduler": full,
        "cross_session_control": cross_session_control,
        "task_after_cross_session_rejection": unchanged_task,
        "session_after_cross_session_rejection": unchanged_session,
        "terminal_tasks": {
            "approval": approval_terminal,
            "normal_hold": hold_terminal,
            "approved_file": approved_terminal,
        },
        "actual_effects": {
            "approved.txt": approved_file,
            "approval.txt": approval_file,
        },
        "sessions": session_evidence,
        "final_scheduler": final_view,
        "final_health": final_health,
    });
    write_evidence(&evidence_root(), "scheduler_ipc_main.json", &receipt);

    client.close().await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn cell_values_remain_bound_to_their_original_session_over_ipc() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let rt = Runtime::open(
        directory.path().join("sessions.sqlite"),
        &workspace,
        Arc::new(ScriptedProvider {
            gate: Arc::new(Gate::new()),
        }),
        Model::new("fixture", "session-values"),
        RuntimeConfig {
            max_concurrent_runs: 1,
            max_queued_runs: 3,
            fairness_after_dispatches: 2,
            max_run_time: Duration::from_secs(20),
            permissions: vec![
                ScopedPermission::exact(
                    "cells.store",
                    json!({"key": "shared", "value": {"owner": "s1"}, "expected_version": 0}),
                ),
                ScopedPermission::exact("cells.load", json!({"key": "shared"})),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt),
        ServerConfig::new("session-values-secret"),
        shutdown.clone(),
    ));
    let client = Client::connect(address, "session-values-secret")
        .await
        .unwrap();

    let first = create_session(&client, "value owner").await;
    let second = create_session(&client, "value other").await;
    let first_result = client
        .request(
            "session.prompt",
            json!({"session_id": first, "text": "value-store"}),
        )
        .await
        .unwrap();
    assert_eq!(first_result["state"], "idle");
    // The authenticated host controller may read any owner Session. This test
    // checks the model/capability boundary instead: the CellIdentity Session is
    // the only source of the value namespace.
    let second_result = client
        .request(
            "session.prompt",
            json!({"session_id": second, "text": "value-load"}),
        )
        .await
        .unwrap();
    assert_eq!(second_result["state"], "idle");

    let first_tasks = client
        .request("session.tasks", json!({"session_id": first}))
        .await
        .unwrap();
    let second_tasks = client
        .request("session.tasks", json!({"session_id": second}))
        .await
        .unwrap();
    let first_task = first_tasks[0]["id"].as_str().unwrap().to_owned();
    let second_task = second_tasks[0]["id"].as_str().unwrap().to_owned();
    let first_events = events(&client, &first).await;
    let second_events = events(&client, &second).await;
    assert_events_and_pairing(&first_events, &first, &first_task);
    assert_events_and_pairing(&second_events, &second, &second_task);

    let load_result = |records: &[Value]| -> Value {
        records
            .iter()
            .find(|record| {
                record["event"]["type"] == "capability_settled"
                    && record["event"]["call"]["name"] == "cells.load"
            })
            .map(|record| record["event"]["call"]["outcome"]["result"].clone())
            .expect("cells.load must settle in this Session")
    };
    let first_loaded = load_result(&first_events);
    assert_eq!(first_loaded["value"]["owner"], "s1");
    assert!(first_loaded["version"].as_u64().unwrap() >= 1);
    let second_loaded = load_result(&second_events);
    assert!(
        second_loaded.is_null(),
        "a different Session must not observe the first Session's value: {second_loaded}"
    );

    let first_grants = client
        .request("session.grants", json!({"session_id": first}))
        .await
        .unwrap();
    let second_grants = client
        .request("session.grants", json!({"session_id": second}))
        .await
        .unwrap();
    let first_audit = client
        .request(
            "session.audit",
            json!({"session_id": first, "after": 0, "limit": 1000}),
        )
        .await
        .unwrap();
    let second_audit = client
        .request(
            "session.audit",
            json!({"session_id": second, "after": 0, "limit": 1000}),
        )
        .await
        .unwrap();
    let receipt = json!({
        "scenario": "scheduler_ipc_values",
        "fixture_provider": {
            "kind": "synthetic",
            "response_source": "cayw_runtime::fixture::script_response",
            "note": "offline fixture; no real model or network call",
        },
        "configuration": {
            "max_concurrent_runs": 1,
            "max_accepted_tasks": 3,
            "fairness_after_dispatches": 2,
            "exact_permissions": [
                {"capability": "cells.store", "arguments": {"key": "shared", "value": {"owner": "s1"}, "expected_version": 0}},
                {"capability": "cells.load", "arguments": {"key": "shared"}},
            ],
        },
        "actual_effects": {
            "owner_session_cells_load": first_loaded,
            "other_session_cells_load": second_loaded,
        },
        "sessions": {
            "value_owner": {
                "role": "value_owner",
                "session_id": first,
                "events": first_events,
                "tasks": first_tasks,
                "grants": first_grants,
                "audit": first_audit,
            },
            "other_session": {
                "role": "other_session",
                "session_id": second,
                "events": second_events,
                "tasks": second_tasks,
                "grants": second_grants,
                "audit": second_audit,
            },
        },
    });
    write_evidence(&evidence_root(), "scheduler_ipc_values.json", &receipt);

    client.close().await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn scheduler_pagination_sdk_and_authenticated_ipc_share_terminal_cursor_semantics() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let rt = Runtime::open(
        directory.path().join("sessions.sqlite"),
        &workspace,
        Arc::new(ScriptedProvider {
            gate: Arc::new(Gate::new()),
        }),
        Model::new("fixture", "scheduler-pagination"),
        RuntimeConfig {
            max_concurrent_runs: 1,
            max_queued_runs: 4,
            fairness_after_dispatches: 2,
            max_run_time: Duration::from_secs(20),
            ..Default::default()
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt.clone()),
        ServerConfig::new("scheduler-pagination-secret"),
        shutdown.clone(),
    ));
    let client = Client::connect(address, "scheduler-pagination-secret")
        .await
        .unwrap();

    let first_session = create_session(&client, "page first").await;
    let second_session = create_session(&client, "page second").await;
    let third_session = create_session(&client, "page third").await;
    let first_task = enqueue(&client, &first_session, "page-1", "normal").await;
    let second_task = enqueue(&client, &second_session, "page-2", "normal").await;
    let third_task = enqueue(&client, &third_session, "page-3", "normal").await;
    let first_task_id = first_task["id"].as_str().unwrap().to_owned();
    let second_task_id = second_task["id"].as_str().unwrap().to_owned();
    let third_task_id = third_task["id"].as_str().unwrap().to_owned();
    let expected_order = vec![
        first_task_id.clone(),
        second_task_id.clone(),
        third_task_id.clone(),
    ];

    // SDK and IPC accept the same inclusive 1..=1000 page bound.
    let sdk_zero_error = rt.scheduler_page(None, 0).unwrap_err().to_string();
    let sdk_1001_error = rt.scheduler_page(None, 1001).unwrap_err().to_string();
    let rpc_zero_error = client
        .request("runtime.scheduler", json!({"limit": 0}))
        .await
        .unwrap_err();
    let rpc_1001_error = client
        .request("runtime.scheduler", json!({"limit": 1001}))
        .await
        .unwrap_err();
    assert_eq!(rpc_zero_error.code, "invalid_params");
    assert_eq!(rpc_1001_error.code, "invalid_params");
    let sdk_max = rt.scheduler_page(None, 1000).unwrap();
    assert_eq!(sdk_max.tasks.len(), 3);
    assert!(sdk_max.next_after.is_none());
    let rpc_max = client
        .request("runtime.scheduler", json!({"limit": 1000}))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&sdk_max).unwrap(),
        rpc_max,
        "SDK and authenticated IPC must return the same scheduler page"
    );

    // A missing cursor is rejected without claiming a dispatch or changing
    // the live page.
    let before_bad_cursor = serde_json::to_value(rt.scheduler_page(None, 10).unwrap()).unwrap();
    let sdk_bad_cursor = rt
        .scheduler_page(Some("missing-task-id"), 10)
        .unwrap_err()
        .to_string();
    let bad_cursor = client
        .request(
            "runtime.scheduler",
            json!({"after": "missing-task-id", "limit": 10}),
        )
        .await
        .expect_err("an unknown cursor must be rejected");
    assert_eq!(bad_cursor.code, "invalid_params");
    assert!(!bad_cursor.outcome_unknown);
    let after_bad_cursor = serde_json::to_value(rt.scheduler_page(None, 10).unwrap()).unwrap();
    assert_eq!(
        after_bad_cursor, before_bad_cursor,
        "an invalid cursor must have no side effect"
    );

    // Walk every live entry one at a time. Each task appears once, in ticket
    // order, and the terminal empty page ends the walk.
    let mut enumerated = Vec::new();
    let mut walk_evidence = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let page = rt.scheduler_page(after.as_deref(), 1).unwrap();
        let page_value = serde_json::to_value(&page).unwrap();
        walk_evidence.push(json!({
            "after": after.clone(),
            "page": page_value,
        }));
        if page.tasks.is_empty() {
            assert!(page.next_after.is_none());
            break;
        }
        enumerated.extend(page.tasks.iter().map(|task| task.id.clone()));
        after = page.next_after;
    }
    assert_eq!(
        enumerated, expected_order,
        "pages must not repeat or reorder tasks"
    );
    assert_eq!(
        enumerated.iter().collect::<BTreeSet<_>>().len(),
        enumerated.len(),
        "each live task must appear exactly once"
    );

    let first_page_sdk = rt.scheduler_page(None, 2).unwrap();
    assert_eq!(
        first_page_sdk
            .tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec![first_task_id.as_str(), second_task_id.as_str()]
    );
    assert_eq!(
        first_page_sdk.next_after.as_deref(),
        Some(second_task_id.as_str())
    );
    let first_page_rpc = client
        .request("runtime.scheduler", json!({"limit": 2}))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&first_page_sdk).unwrap(),
        first_page_rpc,
        "the first SDK and IPC pages must be identical"
    );

    // The cursor task becomes terminal between pages. Its retained ticket must
    // still position the next page exactly after it.
    let cursor_task = client
        .request("task.get", json!({"task_id": second_task_id}))
        .await
        .unwrap();
    assert_eq!(cursor_task["state"], "queued");
    let cancelled = client
        .request(
            "session.cancel",
            json!({
                "session_id": second_session,
                "task_id": second_task_id,
                "worker_id": cursor_task["worker_id"],
                "attempt_id": cursor_task["attempt_id"],
                "fence": cursor_task["fence"],
            }),
        )
        .await
        .unwrap();
    assert_eq!(cancelled["cancelled"], true);
    let terminal_cursor = client
        .request("task.get", json!({"task_id": second_task_id}))
        .await
        .unwrap();
    assert_eq!(terminal_cursor["state"], "cancelled");

    let second_page_sdk = rt
        .scheduler_page(first_page_sdk.next_after.as_deref(), 2)
        .unwrap();
    assert_eq!(
        second_page_sdk
            .tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec![third_task_id.as_str()],
        "the terminal cursor must still advance to the following live task"
    );
    assert!(second_page_sdk.next_after.is_none());
    let second_page_rpc = client
        .request(
            "runtime.scheduler",
            json!({"after": second_task_id, "limit": 2}),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&second_page_sdk).unwrap(),
        second_page_rpc,
        "SDK and IPC must agree after the cursor task became terminal"
    );
    let final_live = client
        .request("runtime.scheduler", json!({"limit": 10}))
        .await
        .unwrap();
    assert_eq!(
        final_live["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|task| task["id"].as_str() == Some(second_task_id.as_str()))
            .count(),
        0,
        "a terminal task stays out of the live page"
    );

    let receipt = json!({
        "scenario": "scheduler_ipc_pagination",
        "fixture_provider": {
            "kind": "synthetic",
            "note": "no provider call is made; queued tasks are enumerated without a model",
        },
        "configuration": {
            "max_concurrent_runs": 1,
            "max_accepted_tasks": 4,
            "fairness_after_dispatches": 2,
            "max_run_time_ms": 20_000,
        },
        "priority_inputs": {
            "page-1": "normal",
            "page-2": "normal",
            "page-3": "normal",
        },
        "expected_order": expected_order,
        "limits": {
            "accepted_page_limit": 1000,
            "sdk_zero_error": sdk_zero_error,
            "sdk_1001_error": sdk_1001_error,
            "rpc_zero_error": {
                "code": rpc_zero_error.code,
                "outcome_unknown": rpc_zero_error.outcome_unknown,
            },
            "rpc_1001_error": {
                "code": rpc_1001_error.code,
                "outcome_unknown": rpc_1001_error.outcome_unknown,
            },
            "sdk_max_page": sdk_max,
            "rpc_max_page": rpc_max,
        },
        "invalid_cursor": {
            "request": {"after": "missing-task-id", "limit": 10},
            "sdk_error": sdk_bad_cursor,
            "rpc_error": {
                "code": bad_cursor.code,
                "message": bad_cursor.message,
                "outcome_unknown": bad_cursor.outcome_unknown,
            },
            "before": before_bad_cursor,
            "after": after_bad_cursor,
        },
        "pages": {
            "walk_limit_1": walk_evidence,
            "enumerated_order": enumerated,
            "first_page_sdk": first_page_sdk,
            "first_page_rpc": first_page_rpc,
            "second_page_sdk": second_page_sdk,
            "second_page_rpc": second_page_rpc,
        },
        "terminal_cursor": {
            "cursor_task_id": second_task_id,
            "task_before": cursor_task,
            "cancel_request": {
                "session_id": second_session,
                "task_id": second_task_id,
                "worker_id": cursor_task["worker_id"],
                "attempt_id": cursor_task["attempt_id"],
                "fence": cursor_task["fence"],
            },
            "cancel_result": cancelled,
            "task_after": terminal_cursor,
            "next_page_after_terminal_cursor": second_page_sdk,
        },
        "final_live_page": final_live,
    });
    write_evidence(&evidence_root(), "scheduler_ipc_pagination.json", &receipt);

    client.close().await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[test]
fn headless_scheduler_json_maps_to_runtime_config_and_rejects_invalid_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("headless.json");

    std::fs::write(
        &path,
        r#"{
            "scheduler": {
                "max_concurrent_runs": 2,
                "max_accepted_tasks": 5,
                "fairness_after_dispatches": 3,
                "max_queue_time_ms": 1500
            }
        }"#,
    )
    .unwrap();
    let config = HeadlessConfig::load(&path)
        .unwrap()
        .into_runtime_config()
        .unwrap();
    assert_eq!(config.max_concurrent_runs, 2);
    assert_eq!(config.max_queued_runs, 5);
    assert_eq!(config.fairness_after_dispatches, 3);
    assert_eq!(config.max_queue_time, Duration::from_millis(1500));

    for (name, json) in [
        (
            "capacity below concurrency",
            r#"{"scheduler":{"max_concurrent_runs":4,"max_accepted_tasks":2}}"#,
        ),
        (
            "zero concurrency",
            r#"{"scheduler":{"max_concurrent_runs":0}}"#,
        ),
        (
            "zero fairness window",
            r#"{"scheduler":{"fairness_after_dispatches":0}}"#,
        ),
        (
            "zero queue time",
            r#"{"scheduler":{"max_queue_time_ms":0}}"#,
        ),
    ] {
        std::fs::write(&path, json).unwrap();
        let loaded = HeadlessConfig::load(&path)
            .unwrap_or_else(|error| panic!("{name} must be valid JSON: {error}"));
        assert!(
            loaded.into_runtime_config().is_err(),
            "{name} must be rejected by runtime budgets"
        );
    }

    std::fs::write(
        &path,
        r#"{"scheduler":{"max_accepted_tasks":3,"unknown_field":1}}"#,
    )
    .unwrap();
    assert!(
        HeadlessConfig::load(&path).is_err(),
        "unknown scheduler fields must not be silently ignored"
    );
}
