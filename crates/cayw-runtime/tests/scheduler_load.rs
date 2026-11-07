//! Load-shaped scheduler checks. One scenario exercises continuous
//! high-priority replenishment, an ordinary low-priority wait, and a real
//! fs.write task parked on approval at the same time. Separate scenarios keep
//! the approval parking and single-task cancellation contracts explicit.
//!
//! The gated provider is synthetic and reports valid synthetic Usage 4/4/8.
//! It does not touch the unknown-outcome ledger. Ordering is decided by the
//! runtime's public task/scheduler state and by gate notifications, never by
//! fixed sleeps; timeouts only guard against a hang.
//!
//! When CAYW_SCHEDULER_EVIDENCE_DIR is set, each test writes one JSON receipt
//! into that directory. Without the variable no file is written. Receipts
//! contain only synthetic inputs, public scheduler/task/audit fields, and the
//! real workspace file readback; they never contain credentials.

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus,
};
use cayw_requests::TaskIntent;
use cayw_runtime::{
    ApprovalChoice, Runtime, RuntimeConfig, RuntimeError, TaskInput, TaskPriority, TaskRecord,
    TaskState,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

fn input(text: impl Into<String>) -> TaskInput {
    TaskInput {
        text: text.into(),
        images: vec![],
    }
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

fn task_receipt(task: &TaskRecord) -> Value {
    serde_json::to_value(task).unwrap()
}

fn scheduler_snapshot(rt: &Runtime, gate: Option<&Gate>, limit: usize) -> Value {
    let view = rt.scheduler(limit).unwrap();
    json!({
        "dispatch_count": view.dispatch_count,
        "order": gate.map(Gate::order).unwrap_or_default(),
        "tasks": view.tasks.iter().map(task_receipt).collect::<Vec<_>>(),
    })
}

fn final_tasks(rt: &Runtime, sessions: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    for session in sessions {
        map.insert(
            session.clone(),
            json!(
                rt.tasks(session)
                    .unwrap()
                    .iter()
                    .map(task_receipt)
                    .collect::<Vec<_>>()
            ),
        );
    }
    Value::Object(map)
}

fn audit_receipt(rt: &Runtime, sessions: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    for session in sessions {
        // These scenarios create far fewer than 1000 audit records per
        // session. Serialize every field of the original audit record.
        let audit = rt.audit(session, 0, 1000).unwrap();
        map.insert(session.clone(), serde_json::to_value(&audit).unwrap());
    }
    Value::Object(map)
}

fn core_events_receipt(rt: &Runtime, sessions: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    for session in sessions {
        let mut after = 0u64;
        let mut records = Vec::new();
        loop {
            let page = rt.events(session, after, 1000).unwrap();
            if page.is_empty() {
                break;
            }
            after = page.last().unwrap().cursor;
            records.extend(page);
        }
        map.insert(session.clone(), serde_json::to_value(&records).unwrap());
    }
    Value::Object(map)
}

fn write_receipt(name: &str, receipt: Value) {
    if let Some(directory) = std::env::var_os("CAYW_SCHEDULER_EVIDENCE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(name),
            serde_json::to_string_pretty(&receipt).unwrap(),
        )
        .unwrap();
    }
}

/// Releases one provider run at a time. The recorded order is the real
/// admission order for gated work.
struct Gate {
    entered: Notify,
    release: Semaphore,
    order: Mutex<Vec<String>>,
}

impl Gate {
    fn new() -> Self {
        Self {
            entered: Notify::new(),
            release: Semaphore::new(0),
            order: Mutex::new(vec![]),
        }
    }

    fn order(&self) -> Vec<String> {
        self.order.lock().unwrap().clone()
    }
}

struct GatedProvider {
    gate: Arc<Gate>,
}

impl Provider for GatedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let text = last_user_text(&request);
        if text == "approval" {
            // Real fs.write request: the Host performs the effect only after
            // the host user approves it.
            let message = cayw_runtime::fixture::script_response(
                &request,
                "return cayw.call('fs.write', {path='approval.txt', content='approval'})",
                &format!("approval-{}", request.messages.len()),
            );
            return Box::pin(futures::stream::iter([AssistantEvent::Done { message }]));
        }
        let gate = self.gate.clone();
        Box::pin(futures::stream::once(async move {
            if cancel.is_cancelled() {
                return AssistantEvent::Error {
                    message: synthetic_failure(&request, "gated fixture cancelled"),
                };
            }
            gate.order.lock().unwrap().push(text.clone());
            gate.entered.notify_one();
            tokio::select! {
                _ = cancel.cancelled() => {
                    return AssistantEvent::Error {
                        message: synthetic_failure(&request, "gated fixture cancelled"),
                    };
                }
                permit = gate.release.acquire() => {
                    if let Ok(permit) = permit {
                        permit.forget();
                    }
                }
            }
            AssistantEvent::Done {
                message: synthetic_message(&request, &format!("done-{text}")),
            }
        }))
    }
}

async fn replenish_high(
    rt: Runtime,
    session: String,
    prefix: String,
    posts: usize,
) -> Result<(), RuntimeError> {
    for index in 0..posts {
        let task = rt.enqueue_scheduled(
            &session,
            input(format!("{prefix}-{index}")),
            TaskIntent::General,
            TaskPriority::High,
        )?;
        rt.resume(&task.id, CancellationToken::new()).await?;
    }
    Ok(())
}

async fn wait_for<T, F>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Option<T>,
{
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(value) = probe() {
                return value;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn wait_drained(rt: &Runtime) {
    wait_for("runtime to drain", || {
        let health = rt.health().unwrap();
        (health.running == 0 && health.queued == 0).then_some(())
    })
    .await;
}

fn approval_is_parked(view: &cayw_runtime::SchedulerView, task_id: &str) -> bool {
    view.tasks.iter().any(|task| {
        task.id == task_id && task.state == TaskState::WaitingApproval && !task.holds_slot
    })
}

#[tokio::test]
async fn continuous_high_arrivals_bounded_for_low_and_limits_visible() {
    const CONCURRENCY: usize = 1;
    const CAPACITY: usize = 6;
    const FAIRNESS: usize = 2;
    const POSTS_PER_HIGH_SESSION: usize = 6;

    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let gate = Arc::new(Gate::new());
    let provider = Arc::new(GatedProvider { gate: gate.clone() });
    let rt = Runtime::open(
        directory.path().join("sessions.sqlite"),
        &workspace,
        provider,
        Model::new("fixture", "scheduler-load"),
        RuntimeConfig {
            max_concurrent_runs: CONCURRENCY,
            max_queued_runs: CAPACITY,
            fairness_after_dispatches: FAIRNESS,
            max_run_time: Duration::from_secs(30),
            ..Default::default()
        },
    )
    .unwrap();

    let approval_session = rt.create(None).unwrap().id;
    let low_session = rt.create(None).unwrap().id;
    let normal_session = rt.create(None).unwrap().id;
    let high_sessions: Vec<String> = (0..3).map(|_| rt.create(None).unwrap().id).collect();

    // Barrier: the approval task must be parked before any fair-window load
    // starts. It stays pending and must have no side effect until discharge.
    let approval_task = rt
        .enqueue_scheduled(
            &approval_session,
            input("approval"),
            TaskIntent::General,
            TaskPriority::Normal,
        )
        .unwrap();
    let approval_rt = rt.clone();
    let approval_id = approval_task.id.clone();
    let approval_handle = tokio::spawn(async move {
        approval_rt
            .resume(&approval_id, CancellationToken::new())
            .await
    });
    let approval = wait_for("pending approval", || {
        rt.get(&approval_session)
            .unwrap()
            .pending_approvals
            .first()
            .cloned()
    })
    .await;
    wait_for("approval task to park", || {
        approval_is_parked(&rt.scheduler(16).unwrap(), &approval.input.task_id).then_some(())
    })
    .await;
    let original_approval = rt.task(&approval.input.task_id).unwrap();
    assert_eq!(original_approval.state, TaskState::WaitingApproval);
    assert!(!workspace.join("approval.txt").exists());
    let mut dispatch_snapshots = vec![json!({
        "phase": "approval_parked",
        "snapshot": scheduler_snapshot(&rt, Some(&gate), 16),
    })];

    let mut high_handles = Vec::new();
    for (index, session) in high_sessions.iter().cloned().enumerate() {
        let rt = rt.clone();
        let prefix = format!("high-{index}");
        high_handles.push(tokio::spawn(replenish_high(
            rt,
            session,
            prefix,
            POSTS_PER_HIGH_SESSION,
        )));
    }
    wait_for("three high-priority tasks to become ready", || {
        let view = rt.scheduler(16).unwrap();
        let ready = view
            .tasks
            .iter()
            .filter(|task| task.priority == TaskPriority::High && task.ready_order.is_some())
            .count();
        (ready == 3).then_some(())
    })
    .await;

    let low_task = rt
        .enqueue_scheduled(
            &low_session,
            input("low-0"),
            TaskIntent::General,
            TaskPriority::Low,
        )
        .unwrap();
    let normal_task = rt
        .enqueue_scheduled(
            &normal_session,
            input("normal-0"),
            TaskIntent::General,
            TaskPriority::Normal,
        )
        .unwrap();
    let low_rt = rt.clone();
    let low_id = low_task.id.clone();
    let mut low_handle =
        tokio::spawn(async move { low_rt.resume(&low_id, CancellationToken::new()).await });
    let normal_rt = rt.clone();
    let normal_id = normal_task.id.clone();
    let normal_handle = tokio::spawn(async move {
        normal_rt
            .resume(&normal_id, CancellationToken::new())
            .await
            .map(|_| ())
    });

    let low_ready_since = wait_for("low task to be claimed and ready", || {
        rt.scheduler(16)
            .unwrap()
            .tasks
            .iter()
            .find(|task| task.id == low_task.id)
            .and_then(|task| task.ready_since_dispatch)
    })
    .await;
    let normal_ready_order = wait_for("normal task to be claimed and ready", || {
        rt.scheduler(16)
            .unwrap()
            .tasks
            .iter()
            .find(|task| task.id == normal_task.id)
            .and_then(|task| task.ready_order)
    })
    .await;

    let overflow_session = rt.create(None).unwrap().id;
    assert!(matches!(
        rt.enqueue_scheduled(
            &overflow_session,
            input("overflow"),
            TaskIntent::General,
            TaskPriority::High
        ),
        Err(RuntimeError::QueueFull)
    ));
    assert!(matches!(
        rt.enqueue_scheduled(
            &low_session,
            input("duplicate"),
            TaskIntent::General,
            TaskPriority::Low
        ),
        Err(RuntimeError::Busy)
    ));

    let view = rt.scheduler(16).unwrap();
    assert_eq!(view.max_concurrent_runs, CONCURRENCY);
    assert_eq!(view.max_bypasses, FAIRNESS + CAPACITY - 1);
    assert_eq!(view.fairness_after_dispatches, FAIRNESS);
    assert!(view.tasks.len() <= CAPACITY);
    assert!(view.tasks.iter().filter(|task| task.holds_slot).count() <= CONCURRENCY);
    assert!(approval_is_parked(&view, &approval.input.task_id));
    assert!(rt.health().unwrap().running <= CONCURRENCY);

    let mut low_dispatch = None;
    let low_result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! {
                result = &mut low_handle => break result,
                _ = gate.entered.notified() => {
                    let view = rt.scheduler(16).unwrap();
                    let holding = view.tasks.iter().filter(|task| task.holds_slot).count();
                    assert!(
                        holding <= CONCURRENCY,
                        "at most {CONCURRENCY} task may hold a slot, saw {holding}"
                    );
                    assert!(approval_is_parked(&view, &approval.input.task_id));
                    assert!(!workspace.join("approval.txt").exists());
                    assert!(rt.health().unwrap().running <= CONCURRENCY);
                    let current = gate.order.lock().unwrap().last().cloned();
                    if current.as_deref() == Some("low-0") {
                        let record = view
                            .tasks
                            .iter()
                            .find(|task| task.id == low_task.id)
                            .cloned()
                            .expect("low task must be live while its provider runs");
                        assert!(record.holds_slot);
                        assert_eq!(record.dispatches, 1);
                        low_dispatch = Some(view.dispatch_count);
                    }
                    dispatch_snapshots.push(json!({
                        "phase": "low_fairness",
                        "snapshot": scheduler_snapshot(&rt, Some(&gate), 16),
                    }));
                    gate.release.add_permits(1);
                }
            }
        }
    })
    .await
    .expect("low task must finish before the hang guard");
    low_result.unwrap().unwrap();
    let low_dispatch = low_dispatch.expect("the low provider run must be observed");
    // The first dispatch counted at the provider is the low task itself, so
    // the number of OTHER dispatches since it became ready is delta - 1.
    let low_other_dispatches = low_dispatch - low_ready_since - 1;

    // Stop submitting (replenishers have finite posts) and drain every
    // executable task while the approval task remains pending.
    let mut remaining = futures::stream::FuturesUnordered::new();
    remaining.push(normal_handle);
    for handle in high_handles {
        remaining.push(handle);
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                result = remaining.next() => {
                    match result {
                        Some(result) => {
                            result.unwrap().unwrap();
                        }
                        None => break,
                    }
                }
                _ = gate.entered.notified() => {
                    let view = rt.scheduler(16).unwrap();
                    assert!(approval_is_parked(&view, &approval.input.task_id));
                    assert!(!workspace.join("approval.txt").exists());
                    dispatch_snapshots.push(json!({
                        "phase": "executable_drain",
                        "snapshot": scheduler_snapshot(&rt, Some(&gate), 16),
                    }));
                    gate.release.add_permits(1);
                }
            }
        }
    })
    .await
    .expect("controlled executable drain must finish");
    wait_drained(&rt).await;
    let drained = rt.scheduler(16).unwrap();
    assert!(approval_is_parked(&drained, &approval.input.task_id));
    assert!(!workspace.join("approval.txt").exists());
    dispatch_snapshots.push(json!({
        "phase": "executable_drained",
        "snapshot": scheduler_snapshot(&rt, Some(&gate), 16),
    }));

    // Only after executable work is drained, discharge the original attempt.
    rt.approve(
        &approval_session,
        &approval.id,
        ApprovalChoice::AllowOnce,
        "host-user",
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(15), approval_handle)
        .await
        .expect("approved work must finish")
        .unwrap()
        .unwrap();
    let terminal = rt.task(&approval_task.id).unwrap();
    assert_eq!(terminal.state, TaskState::Completed);
    assert_eq!(terminal.attempt_id, original_approval.attempt_id);
    assert_eq!(terminal.worker_id, original_approval.worker_id);
    assert_eq!(terminal.fence, original_approval.fence);
    let file_after = std::fs::read_to_string(workspace.join("approval.txt")).unwrap();
    assert_eq!(file_after, "approval");
    wait_drained(&rt).await;
    dispatch_snapshots.push(json!({
        "phase": "after_approval",
        "snapshot": scheduler_snapshot(&rt, Some(&gate), 16),
    }));

    let order = gate.order();
    let low_index = order
        .iter()
        .position(|text| text == "low-0")
        .expect("the low task must be dispatched");
    let highs_before_low = order[..low_index]
        .iter()
        .filter(|text| text.starts_with("high-"))
        .count();
    assert!(
        highs_before_low >= FAIRNESS,
        "high-priority arrivals must overtake the low task through the window: {order:?}"
    );
    assert!(
        low_other_dispatches >= FAIRNESS as u64,
        "low must wait through the fairness window, other={low_other_dispatches}, order={order:?}"
    );
    assert!(
        low_other_dispatches <= (FAIRNESS + CAPACITY - 1) as u64,
        "later high arrivals must not bypass promoted low, other={low_other_dispatches}, order={order:?}"
    );

    let mut sessions = vec![
        approval_session.clone(),
        low_session.clone(),
        normal_session.clone(),
    ];
    sessions.extend(high_sessions.iter().cloned());
    write_receipt(
        "continuous_high.json",
        json!({
            "test": "continuous_high_arrivals_bounded_for_low_and_limits_visible",
            "limits": {
                "max_concurrent_runs": CONCURRENCY,
                "max_queued_runs": CAPACITY,
                "fairness_after_dispatches": FAIRNESS,
                "max_bypasses": FAIRNESS + CAPACITY - 1,
            },
            "sessions": {
                "continuous_high": &high_sessions,
                "ordinary_low": &low_session,
                "ordinary_normal": &normal_session,
                "approval": &approval_session,
            },
            "inputs": {
                "approval": {
                    "task_id": &approval_task.id,
                    "input": serde_json::to_value(&approval_task.input).unwrap(),
                    "priority": "normal",
                    "request_id": &approval.id,
                },
                "low": {
                    "task_id": &low_task.id,
                    "input": serde_json::to_value(&low_task.input).unwrap(),
                    "priority": "low",
                },
                "normal": {
                    "task_id": &normal_task.id,
                    "input": serde_json::to_value(&normal_task.input).unwrap(),
                    "priority": "normal",
                },
                "continuous_high": high_sessions.iter().map(|session| json!({
                    "session": session,
                    "posts": POSTS_PER_HIGH_SESSION,
                    "priority": "high",
                })).collect::<Vec<_>>(),
            },
            "permissions": {
                "runtime_config": [],
                "manual_approval": {
                    "choice": "allow_once",
                    "actor": "host-user",
                    "path": "approval.txt",
                    "content": "approval",
                },
            },
            "accepted": {
                "low": {
                    "task_id": &low_task.id,
                    "ready_since_dispatch": low_ready_since,
                    "dispatch_count": low_dispatch,
                },
                "normal": {
                    "task_id": &normal_task.id,
                    "ready_order": normal_ready_order,
                },
                "approval": {
                    "task_id": &approval_task.id,
                    "ready_since_dispatch": original_approval.ready_since_dispatch,
                    "dispatches": original_approval.dispatches,
                },
            },
            "fairness": {
                "other_dispatches_while_ready": low_other_dispatches,
                "highs_before_low": highs_before_low,
            },
            "dispatch_snapshots": dispatch_snapshots,
            "order": order,
            "final_tasks": final_tasks(&rt, &sessions),
            "audit": audit_receipt(&rt, &sessions),
            "events": core_events_receipt(&rt, &sessions),
            "approval_result": {
                "task_id": &approval_task.id,
                "state": serde_json::to_value(terminal.state).unwrap(),
                "attempt_preserved": terminal.attempt_id == original_approval.attempt_id,
                "worker_preserved": terminal.worker_id == original_approval.worker_id,
                "fence_preserved": terminal.fence == original_approval.fence,
                "file_before": Value::Null,
                "file_after": file_after,
            },
        }),
    );
}

#[tokio::test]
async fn waiting_approval_releases_slot_and_real_file_load_completes() {
    const CONCURRENCY: usize = 1;
    const CAPACITY: usize = 3;
    const FAIRNESS: usize = 2;

    struct ApprovalProvider {
        order: Arc<Mutex<Vec<String>>>,
    }
    impl Provider for ApprovalProvider {
        fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
            if cancel.is_cancelled() {
                return Box::pin(futures::stream::iter([AssistantEvent::Error {
                    message: synthetic_failure(&request, "approval fixture cancelled"),
                }]));
            }
            let text = last_user_text(&request);
            if !matches!(request.messages.last(), Some(Message::ToolResult(_))) {
                self.order.lock().unwrap().push(text.clone());
            }
            let message = if text == "approval" {
                cayw_runtime::fixture::script_response(
                    &request,
                    "return cayw.call('fs.write', {path='approval.txt', content='approval'})",
                    &format!("approval-{}", request.messages.len()),
                )
            } else {
                synthetic_message(&request, &format!("done-{text}"))
            };
            Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let order = Arc::new(Mutex::new(vec![]));
    let rt = Runtime::open(
        directory.path().join("sessions.sqlite"),
        &workspace,
        Arc::new(ApprovalProvider {
            order: order.clone(),
        }),
        Model::new("fixture", "scheduler-approval-load"),
        RuntimeConfig {
            max_concurrent_runs: CONCURRENCY,
            max_queued_runs: CAPACITY,
            fairness_after_dispatches: FAIRNESS,
            max_run_time: Duration::from_secs(20),
            ..Default::default()
        },
    )
    .unwrap();

    let approval_session = rt.create(None).unwrap().id;
    let approval_task = rt
        .enqueue_scheduled(
            &approval_session,
            input("approval"),
            TaskIntent::General,
            TaskPriority::Normal,
        )
        .unwrap();
    let approval_rt = rt.clone();
    let approval_id = approval_task.id.clone();
    let approval_handle = tokio::spawn(async move {
        approval_rt
            .resume(&approval_id, CancellationToken::new())
            .await
    });

    let mut dispatch_snapshots = Vec::new();
    let approval = wait_for("pending approval", || {
        rt.get(&approval_session)
            .unwrap()
            .pending_approvals
            .first()
            .cloned()
    })
    .await;
    // pending_approvals becomes visible just before the task is parked. Wait
    // for both the WaitingApproval state and the released slot, not just the
    // request record.
    wait_for("approval task to park", || {
        approval_is_parked(&rt.scheduler(8).unwrap(), &approval.input.task_id).then_some(())
    })
    .await;
    let original = rt.task(&approval.input.task_id).unwrap();
    assert_eq!(original.state, TaskState::WaitingApproval);
    dispatch_snapshots.push(json!({
        "phase": "approval_parked",
        "snapshot": scheduler_snapshot(&rt, None, 8),
    }));

    let parked_view = rt.scheduler(8).unwrap();
    let parked = parked_view
        .tasks
        .iter()
        .find(|task| task.id == approval.input.task_id)
        .expect("waiting approval stays live");
    assert_eq!(parked.state, TaskState::WaitingApproval);
    assert!(!parked.holds_slot);
    assert!(parked.ready_order.is_none());
    assert!(parked.ready_since_dispatch.is_none());
    assert!(
        parked_view
            .tasks
            .iter()
            .filter(|task| task.holds_slot)
            .count()
            <= CONCURRENCY
    );
    assert_eq!(parked_view.max_bypasses, FAIRNESS + CAPACITY - 1);

    let low_session = rt.create(None).unwrap().id;
    let normal_session = rt.create(None).unwrap().id;
    let low_task = rt
        .enqueue_scheduled(
            &low_session,
            input("low-load"),
            TaskIntent::General,
            TaskPriority::Low,
        )
        .unwrap();
    let normal_task = rt
        .enqueue_scheduled(
            &normal_session,
            input("normal-load"),
            TaskIntent::General,
            TaskPriority::Normal,
        )
        .unwrap();
    let low_rt = rt.clone();
    let low_id = low_task.id.clone();
    let low_handle =
        tokio::spawn(async move { low_rt.resume(&low_id, CancellationToken::new()).await });
    let normal_rt = rt.clone();
    let normal_id = normal_task.id.clone();
    let normal_handle =
        tokio::spawn(async move { normal_rt.resume(&normal_id, CancellationToken::new()).await });

    tokio::time::timeout(Duration::from_secs(15), low_handle)
        .await
        .expect("low work must finish while approval waits")
        .unwrap()
        .unwrap();
    dispatch_snapshots.push(json!({
        "phase": "low_completed",
        "snapshot": scheduler_snapshot(&rt, None, 8),
    }));
    tokio::time::timeout(Duration::from_secs(15), normal_handle)
        .await
        .expect("normal work must finish while approval waits")
        .unwrap()
        .unwrap();
    dispatch_snapshots.push(json!({
        "phase": "normal_completed",
        "snapshot": scheduler_snapshot(&rt, None, 8),
    }));
    assert!(!workspace.join("approval.txt").exists());
    assert_eq!(rt.task(&low_task.id).unwrap().state, TaskState::Completed);
    assert_eq!(
        rt.task(&normal_task.id).unwrap().state,
        TaskState::Completed
    );

    rt.approve(
        &approval_session,
        &approval.id,
        ApprovalChoice::AllowOnce,
        "host-user",
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(15), approval_handle)
        .await
        .expect("approved work must finish")
        .unwrap()
        .unwrap();
    dispatch_snapshots.push(json!({
        "phase": "after_approval",
        "snapshot": scheduler_snapshot(&rt, None, 8),
    }));

    let terminal = rt.task(&approval_task.id).unwrap();
    assert_eq!(terminal.state, TaskState::Completed);
    assert_eq!(terminal.attempt_id, original.attempt_id);
    assert_eq!(terminal.worker_id, original.worker_id);
    assert_eq!(terminal.fence, original.fence);
    let file_after = std::fs::read_to_string(workspace.join("approval.txt")).unwrap();
    assert_eq!(file_after, "approval");
    wait_drained(&rt).await;
    assert!(rt.scheduler(8).unwrap().tasks.is_empty());

    let sessions = vec![
        approval_session.clone(),
        low_session.clone(),
        normal_session.clone(),
    ];
    write_receipt(
        "approval.json",
        json!({
            "test": "waiting_approval_releases_slot_and_real_file_load_completes",
            "limits": {
                "max_concurrent_runs": CONCURRENCY,
                "max_queued_runs": CAPACITY,
                "fairness_after_dispatches": FAIRNESS,
                "max_bypasses": FAIRNESS + CAPACITY - 1,
            },
            "inputs": {
                "approval": {
                    "task_id": &approval_task.id,
                    "input": serde_json::to_value(&approval_task.input).unwrap(),
                    "priority": "normal",
                    "request_id": &approval.id,
                },
                "low": {
                    "task_id": &low_task.id,
                    "input": serde_json::to_value(&low_task.input).unwrap(),
                    "priority": "low",
                },
                "normal": {
                    "task_id": &normal_task.id,
                    "input": serde_json::to_value(&normal_task.input).unwrap(),
                    "priority": "normal",
                },
            },
            "permissions": {
                "runtime_config": [],
                "manual_approval": {
                    "choice": "allow_once",
                    "actor": "host-user",
                    "path": "approval.txt",
                    "content": "approval",
                },
            },
            "accepted": {
                "approval": {
                    "task_id": &approval_task.id,
                    "dispatches": terminal.dispatches,
                    "ready_since_dispatch": original.ready_since_dispatch,
                },
                "low": {"task_id": &low_task.id},
                "normal": {"task_id": &normal_task.id},
            },
            "dispatch_snapshots": dispatch_snapshots,
            "order": order.lock().unwrap().clone(),
            "final_tasks": final_tasks(&rt, &sessions),
            "audit": audit_receipt(&rt, &sessions),
            "events": core_events_receipt(&rt, &sessions),
            "approval_result": {
                "task_id": &approval_task.id,
                "state": serde_json::to_value(terminal.state).unwrap(),
                "attempt_preserved": terminal.attempt_id == original.attempt_id,
                "worker_preserved": terminal.worker_id == original.worker_id,
                "fence_preserved": terminal.fence == original.fence,
                "file_before": Value::Null,
                "file_after": file_after,
            },
        }),
    );
}

#[tokio::test]
async fn cancelling_one_queued_task_leaves_other_ready_work_runnable() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let gate = Arc::new(Gate::new());
    let provider = Arc::new(GatedProvider { gate: gate.clone() });
    let rt = Runtime::open(
        directory.path().join("sessions.sqlite"),
        &workspace,
        provider,
        Model::new("fixture", "scheduler-cancel-load"),
        RuntimeConfig {
            max_concurrent_runs: 1,
            max_queued_runs: 3,
            fairness_after_dispatches: 2,
            max_run_time: Duration::from_secs(20),
            ..Default::default()
        },
    )
    .unwrap();

    let held_session = rt.create(None).unwrap().id;
    let victim_session = rt.create(None).unwrap().id;
    let survivor_session = rt.create(None).unwrap().id;

    let held_task = rt
        .enqueue_scheduled(
            &held_session,
            input("held-0"),
            TaskIntent::General,
            TaskPriority::Normal,
        )
        .unwrap();
    let held_rt = rt.clone();
    let held_id = held_task.id.clone();
    let held_handle =
        tokio::spawn(async move { held_rt.resume(&held_id, CancellationToken::new()).await });
    tokio::time::timeout(Duration::from_secs(15), gate.entered.notified())
        .await
        .expect("the first task must reach its provider");
    assert_eq!(gate.order().last().map(String::as_str), Some("held-0"));
    let mut dispatch_snapshots = vec![json!({
        "phase": "held_running",
        "snapshot": scheduler_snapshot(&rt, Some(&gate), 8),
    })];

    let victim_task = rt
        .enqueue_scheduled(
            &victim_session,
            input("victim-0"),
            TaskIntent::General,
            TaskPriority::Normal,
        )
        .unwrap();
    let survivor_task = rt
        .enqueue_scheduled(
            &survivor_session,
            input("survivor-0"),
            TaskIntent::General,
            TaskPriority::Normal,
        )
        .unwrap();
    let victim_rt = rt.clone();
    let victim_id = victim_task.id.clone();
    let victim_handle =
        tokio::spawn(async move { victim_rt.resume(&victim_id, CancellationToken::new()).await });
    let survivor_rt = rt.clone();
    let survivor_id = survivor_task.id.clone();
    let survivor_handle = tokio::spawn(async move {
        survivor_rt
            .resume(&survivor_id, CancellationToken::new())
            .await
    });

    wait_for("victim and survivor to be ready", || {
        let view = rt.scheduler(8).unwrap();
        let victim = view.tasks.iter().find(|task| task.id == victim_task.id);
        let survivor = view.tasks.iter().find(|task| task.id == survivor_task.id);
        match (victim, survivor) {
            (Some(victim), Some(survivor))
                if victim.ready_order.is_some() && survivor.ready_order.is_some() =>
            {
                Some(())
            }
            _ => None,
        }
    })
    .await;

    let target = rt.task(&victim_task.id).unwrap().cancel_target();
    assert!(rt.cancel(&target).unwrap());
    let victim_result = tokio::time::timeout(Duration::from_secs(15), victim_handle)
        .await
        .expect("cancelled run must end")
        .unwrap();
    assert!(matches!(victim_result, Err(RuntimeError::Cancelled)));
    dispatch_snapshots.push(json!({
        "phase": "victim_cancelled",
        "snapshot": scheduler_snapshot(&rt, Some(&gate), 8),
    }));

    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(15), held_handle)
        .await
        .expect("held work must finish")
        .unwrap()
        .unwrap();

    tokio::time::timeout(Duration::from_secs(15), gate.entered.notified())
        .await
        .expect("survivor must reach its provider");
    assert_eq!(gate.order().last().map(String::as_str), Some("survivor-0"));
    dispatch_snapshots.push(json!({
        "phase": "survivor_running",
        "snapshot": scheduler_snapshot(&rt, Some(&gate), 8),
    }));
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(15), survivor_handle)
        .await
        .expect("survivor must finish")
        .unwrap()
        .unwrap();

    let order = gate.order();
    assert_eq!(order, vec!["held-0", "survivor-0"]);
    assert_eq!(rt.scheduler(8).unwrap().dispatch_count, 2);
    wait_drained(&rt).await;
    dispatch_snapshots.push(json!({
        "phase": "drained",
        "snapshot": scheduler_snapshot(&rt, Some(&gate), 8),
    }));

    let sessions = vec![
        held_session.clone(),
        victim_session.clone(),
        survivor_session.clone(),
    ];
    write_receipt(
        "cancel_one.json",
        json!({
            "test": "cancelling_one_queued_task_leaves_other_ready_work_runnable",
            "limits": {
                "max_concurrent_runs": 1,
                "max_queued_runs": 3,
                "fairness_after_dispatches": 2,
                "max_bypasses": 2 + 3 - 1,
            },
            "inputs": {
                "held": {
                    "task_id": &held_task.id,
                    "input": serde_json::to_value(&held_task.input).unwrap(),
                    "priority": "normal",
                },
                "victim": {
                    "task_id": &victim_task.id,
                    "input": serde_json::to_value(&victim_task.input).unwrap(),
                    "priority": "normal",
                },
                "survivor": {
                    "task_id": &survivor_task.id,
                    "input": serde_json::to_value(&survivor_task.input).unwrap(),
                    "priority": "normal",
                },
            },
            "permissions": {"runtime_config": []},
            "cancel": {
                "task_id": &victim_task.id,
                "result": "cancelled",
            },
            "dispatch_snapshots": dispatch_snapshots,
            "order": order,
            "final_tasks": final_tasks(&rt, &sessions),
            "audit": audit_receipt(&rt, &sessions),
            "events": core_events_receipt(&rt, &sessions),
            "file_readback": Value::Null,
        }),
    );
}
