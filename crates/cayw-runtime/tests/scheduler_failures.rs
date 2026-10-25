//! Independent Issue 11 verification: approval slot accounting and error
//! classification. Public SDK, synthetic providers and SQLite triggers only.
//! No model, network or product-file change.
//!
//! Reachable shapes:
//!   1. an ordinary cancellation while the task is parked for an approval;
//!   2. an Activity park failure while a cell waits for approval;
//!   3. an Activity park failure when an independently yielded cell finishes
//!      while another cell waits for approval (the `Backend::finished` path);
//!   4. one live, non-waiting cell plus one approval wait keeps the only slot.
//!
//! The finished shape is reached with an independently yielded cell that is
//! resumed and runs to completion. A parallel shape where the other cell
//! executes inside an external Host call while a different cell reaches the
//! approval was not verified: in that arrangement the waiting cell's
//! authorization did not progress, so this file makes no claim about it.

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason, ToolCall, UsageStatus,
};
use cayw_cells::{CellPage, CellStatus};
use cayw_core::SessionState;
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, TaskState};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

struct ModelRound {
    request: ModelRequest,
    reply: oneshot::Sender<AssistantMessage>,
}

struct LiveStream(Arc<AtomicUsize>);
impl Drop for LiveStream {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ControlledProvider {
    rounds: mpsc::UnboundedSender<ModelRound>,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}
impl Provider for ControlledProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let model = request.model.clone();
        let (reply, answer) = oneshot::channel();
        self.rounds
            .send(ModelRound { request, reply })
            .expect("verifier keeps its receiver");
        let live = self.live.clone();
        let peak = self.peak.clone();
        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        Box::pin(futures::stream::once(async move {
            let _live = LiveStream(live);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => AssistantEvent::Error {
                    message: AssistantMessage::new(&model)
                        .failure(StopReason::Aborted, "controlled fixture cancelled"),
                },
                result = answer => match result {
                    Ok(message) => AssistantEvent::Done { message },
                    Err(_) => AssistantEvent::Error {
                        message: AssistantMessage::new(&model)
                            .failure(StopReason::Aborted, "verifier dropped its reply"),
                    },
                },
            }
        }))
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    rt: Runtime,
    session: String,
    rounds: mpsc::UnboundedReceiver<ModelRound>,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    workspace: std::path::PathBuf,
    db: std::path::PathBuf,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (sender, rounds) = mpsc::unbounded_channel();
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let db = dir.path().join("sessions.sqlite");
    let rt = Runtime::open(
        &db,
        &workspace,
        Arc::new(ControlledProvider {
            rounds: sender,
            live: live.clone(),
            peak: peak.clone(),
        }),
        Model::new("fixture", "scheduler-failures"),
        RuntimeConfig {
            max_concurrent_runs: 1,
            max_queued_runs: 4,
            max_queue_time: Duration::from_secs(10),
            max_run_time: Duration::from_secs(30),
            ..Default::default()
        },
    )
    .unwrap();
    let session = rt.create(None).unwrap().id;
    Harness {
        _dir: dir,
        rt,
        session,
        rounds,
        live,
        peak,
        workspace,
        db,
    }
}

async fn next_round(rounds: &mut mpsc::UnboundedReceiver<ModelRound>) -> ModelRound {
    tokio::time::timeout(Duration::from_secs(10), rounds.recv())
        .await
        .expect("model round must arrive")
        .expect("verifier keeps the provider channel open")
}

fn reported_reply(model: &Model) -> AssistantMessage {
    let mut reply = AssistantMessage::new(model);
    reply.usage.input = 4;
    reply.usage.output = 4;
    reply.usage.update_total();
    reply.usage_status = UsageStatus::Reported;
    reply
}

fn tool(round: ModelRound, id: &str, arguments: Value) {
    let mut reply = reported_reply(&round.request.model);
    reply.content.push(Content::ToolCall(ToolCall::new(
        id,
        "execute_code",
        arguments,
    )));
    reply.stop_reason = StopReason::ToolUse;
    round.reply.send(reply).unwrap();
}

fn final_text(round: ModelRound, text: &str) {
    let mut reply = reported_reply(&round.request.model);
    reply.content.push(Content::text(text));
    reply.stop_reason = StopReason::Stop;
    let _ = round.reply.send(reply);
}

fn page(round: &ModelRound, call_id: &str) -> CellPage {
    let Some(cayw_ai::Message::ToolResult(result)) = round.request.messages.last() else {
        panic!("model must receive a completed tool reply");
    };
    assert_eq!(result.tool_call_id, call_id);
    assert!(!result.is_error, "{result:?}");
    assert!(!result.outcome_unknown, "{result:?}");
    serde_json::from_value(result.structured_content.clone().unwrap()).unwrap()
}

fn wait(page: &CellPage, yield_time_ms: u64) -> Value {
    json!({
        "action":"wait", "cell_id":page.cell_id, "cursor":page.next_cursor,
        "yield_time_ms":yield_time_ms, "max_output_tokens":4096,
    })
}

fn exec(script: &str) -> Value {
    json!({
        "action":"exec", "yield_time_ms":30_000, "max_output_tokens":4096,
        "script":script,
    })
}

async fn until<F: Fn(&Runtime, &str) -> bool>(rt: &Runtime, id: &str, what: &str, check: F) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if check(rt, id) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn spawn_prompt(
    rt: &Runtime,
    session: &str,
) -> tokio::task::JoinHandle<Result<cayw_runtime::SessionView, cayw_runtime::RuntimeError>> {
    let rt = rt.clone();
    let id = session.to_string();
    tokio::spawn(async move {
        rt.prompt(&id, "run one approved cell", CancellationToken::new())
            .await
    })
}

fn started_events(rt: &Runtime, session: &str) -> Vec<String> {
    rt.events(session, 0, 1000)
        .unwrap()
        .iter()
        .filter_map(|e| match &e.event {
            cayw_core::CoreEvent::CapabilityStarted { call } => Some(call.name.clone()),
            _ => None,
        })
        .collect()
}

fn unknown_tool_results(rt: &Runtime, session: &str) -> Vec<String> {
    rt.get(session)
        .unwrap()
        .messages
        .iter()
        .filter_map(|m| match m {
            cayw_ai::Message::ToolResult(r) if r.outcome_unknown => Some(r.tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Ordinary cancellation while parked: known Cancelled, no side effect.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn ordinary_cancel_while_parked_stays_cancelled() {
    let mut h = harness();
    let worker = spawn_prompt(&h.rt, &h.session);
    let round = next_round(&mut h.rounds).await;
    tool(
        round,
        "exec-a",
        exec("yield_control(); return cayw.call('fs.write',{path='a.txt',content='a'})"),
    );
    let round = next_round(&mut h.rounds).await;
    let a = page(&round, "exec-a");
    assert_eq!(a.status, CellStatus::Yielded);
    tool(round, "wait-a", wait(&a, 30_000));
    until(&h.rt, &h.session, "pending approval", |rt, id| {
        !rt.get(id).unwrap().pending_approvals.is_empty()
    })
    .await;
    let approval = h.rt.get(&h.session).unwrap().pending_approvals[0].clone();
    let task_id = approval.input.task_id.clone();
    // The only live cell waits before a capability boundary and the model is
    // inactive, so the task parks and returns its slot.
    until(&h.rt, &h.session, "parked task", {
        let id = task_id.clone();
        move |rt, _| {
            let task = rt.task(&id).unwrap();
            task.state == TaskState::WaitingApproval && !task.holds_slot
        }
    })
    .await;
    let task = h.rt.task(&task_id).unwrap();
    assert!(h.rt.cancel(&task.cancel_target()).unwrap());
    assert!(
        h.rt.approve(&h.session, &approval.id, ApprovalChoice::AllowOnce, "late")
            .is_err(),
        "a cancelled approval must not be granted"
    );
    let result = tokio::time::timeout(Duration::from_secs(15), worker)
        .await
        .expect("cancellation must not wedge the worker")
        .unwrap();
    let task = h.rt.task(&task_id).unwrap();
    eprintln!(
        "cancel: state={:?} holds_slot={} result={result:?}",
        task.state, task.holds_slot
    );
    assert_eq!(task.state, TaskState::Cancelled);
    assert!(!task.holds_slot);
    assert!(!h.workspace.join("a.txt").exists());
    assert_eq!(started_events(&h.rt, &h.session), Vec::<String>::new());
    assert!(unknown_tool_results(&h.rt, &h.session).is_empty());
    assert!(
        !matches!(
            result,
            Err(cayw_runtime::RuntimeError::RecoveryRequired { .. })
        ),
        "ordinary cancellation is a known outcome: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. The reachable Activity park failure: the park write that runs when a cell
//    waits for approval fails. It must fail closed: nothing executes, the slot
//    is not released by the failed write, and no unknown effect is claimed.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn park_storage_failure_at_approval_wait_fails_closed() {
    let mut h = harness();
    let db = rusqlite::Connection::open(&h.db).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_wait_park BEFORE UPDATE ON runtime_tasks
         WHEN NEW.state='waiting_approval' AND OLD.state='running'
         BEGIN SELECT RAISE(ABORT,'injected scheduler park failure'); END;",
    )
    .unwrap();
    let worker = spawn_prompt(&h.rt, &h.session);
    let round = next_round(&mut h.rounds).await;
    tool(
        round,
        "exec-a",
        exec("yield_control(); return cayw.call('fs.write',{path='a.txt',content='a'})"),
    );
    let round = next_round(&mut h.rounds).await;
    let a = page(&round, "exec-a");
    assert_eq!(a.status, CellStatus::Yielded);
    tool(round, "wait-a", wait(&a, 0));
    // The resumed VM asks for an approval; its park write fails. Record what
    // the model observed and what remains durable.
    let round = next_round(&mut h.rounds).await;
    let last = round.request.messages.last().cloned();
    let tasks = h.rt.tasks(&h.session).unwrap();
    let task = tasks[0].clone();
    eprintln!(
        "park failure: task state={:?} holds_slot={} dispatches={} error={:?}",
        task.state, task.holds_slot, task.dispatches, task.error
    );
    eprintln!(
        "park failure: pending approvals={}",
        h.rt.get(&h.session).unwrap().pending_approvals.len()
    );
    eprintln!("park failure: last model message={last:?}");
    assert!(
        task.holds_slot,
        "the failed park write must not release the execution slot: {task:?}"
    );
    assert!(!h.workspace.join("a.txt").exists(), "nothing executed");

    final_text(round, "done after the scheduler failure");
    let result = tokio::time::timeout(Duration::from_secs(15), worker)
        .await
        .expect("a failed park must not wedge the worker")
        .unwrap();
    let task = h.rt.task(&task.id).unwrap();
    eprintln!(
        "park failure final: task state={:?} holds_slot={} session={:?} result={result:?}",
        task.state,
        task.holds_slot,
        h.rt.get(&h.session).unwrap().state
    );
    eprintln!(
        "park failure final: started={:?} unknown_tools={:?}",
        started_events(&h.rt, &h.session),
        unknown_tool_results(&h.rt, &h.session)
    );
    assert_eq!(
        started_events(&h.rt, &h.session),
        Vec::<String>::new(),
        "no capability may cross its execution boundary"
    );
    assert!(!h.workspace.join("a.txt").exists(), "nothing executed");
    assert!(
        unknown_tool_results(&h.rt, &h.session).is_empty(),
        "a scheduler bookkeeping failure must not label a tool outcome unknown"
    );
    assert_eq!(h.rt.get(&h.session).unwrap().state, SessionState::Idle);
    // Observed contract: the failed park is retained as a conservative
    // lifecycle obligation. It is not an effect unknown (started is empty), and
    // the task keeps its slot until an explicit recovery.
    assert_eq!(task.state, TaskState::Draining);
    assert!(
        task.holds_slot,
        "recovery must start from the retained slot"
    );
    assert!(
        matches!(
            result,
            Err(cayw_runtime::RuntimeError::RecoveryRequired {
                outcome_unknown: true,
                ..
            })
        ),
        "the conservative lifecycle obligation is reported: {result:?}"
    );
    // Recovery clears the obligation and the slot without replaying anything.
    db.execute_batch("DROP TRIGGER fail_wait_park;").unwrap();
    let recovered = h.rt.recover(&h.session).unwrap();
    let task = h.rt.task(&task.id).unwrap();
    eprintln!(
        "park failure recovery: session={:?} task={:?} holds_slot={} running={} started={:?}",
        recovered.state,
        task.state,
        task.holds_slot,
        h.rt.health().unwrap().running,
        started_events(&h.rt, &h.session)
    );
    assert_eq!(recovered.state, SessionState::Idle);
    assert_eq!(task.state, TaskState::Interrupted);
    assert!(!task.holds_slot, "recovery must release the retained slot");
    assert_eq!(h.rt.health().unwrap().running, 0);
    assert_eq!(started_events(&h.rt, &h.session), Vec::<String>::new());
    assert!(!h.workspace.join("a.txt").exists());
}

// ---------------------------------------------------------------------------
// 2b. The `Backend::finished` park failure: a second, independently owned cell
//     really runs and finishes while another waits for an approval. The failed
//     park must keep the slot and must not start the waiting capability.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn park_storage_failure_after_a_cell_finishes_keeps_the_slot() {
    let mut h = harness();
    let worker = spawn_prompt(&h.rt, &h.session);
    let round = next_round(&mut h.rounds).await;
    tool(
        round,
        "exec-a",
        exec("yield_control(); return cayw.call('fs.write',{path='a.txt',content='a'})"),
    );
    let round = next_round(&mut h.rounds).await;
    let a = page(&round, "exec-a");
    assert_eq!(a.status, CellStatus::Yielded);
    tool(round, "exec-b", exec("yield_control(); return 'b-done'"));
    let round = next_round(&mut h.rounds).await;
    let b = page(&round, "exec-b");
    assert_eq!(b.status, CellStatus::Yielded);
    assert_ne!(a.cell_id, b.cell_id);
    // A waits for its approval while B is still live, so no park happens yet.
    tool(round, "wait-a", wait(&a, 0));
    let round = next_round(&mut h.rounds).await;
    page(&round, "wait-a");
    until(&h.rt, &h.session, "approval A", |rt, id| {
        !rt.get(id).unwrap().pending_approvals.is_empty()
    })
    .await;
    let approval = h.rt.get(&h.session).unwrap().pending_approvals[0].clone();
    let task_id = approval.input.task_id.clone();
    assert!(h.rt.task(&task_id).unwrap().holds_slot);
    let db = rusqlite::Connection::open(&h.db).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_finished_park BEFORE UPDATE ON runtime_tasks
         WHEN NEW.state='waiting_approval' AND OLD.state='running'
         BEGIN SELECT RAISE(ABORT,'injected scheduler park failure'); END;",
    )
    .unwrap();
    // B really runs to completion; `Backend::finished` removes it and tries to
    // park the task for A's approval. That park write fails.
    tool(round, "wait-b", wait(&b, 30_000));
    if let Ok(Some(round)) = tokio::time::timeout(Duration::from_secs(5), h.rounds.recv()).await {
        let last = round.request.messages.last().cloned();
        eprintln!("finished-path: next model message={last:?}");
        final_text(round, "done after the finish park failure");
    }
    let result = tokio::time::timeout(Duration::from_secs(15), worker)
        .await
        .expect("a failed park must not wedge the worker")
        .unwrap();
    let task = h.rt.task(&task_id).unwrap();
    let view = h.rt.get(&h.session).unwrap();
    let b_execution = view
        .executions
        .iter()
        .find(|e| e.execution_id == b.cell_id)
        .expect("B keeps its durable execution record");
    eprintln!(
        "finished-path: task={:?} holds_slot={} session={:?} result={result:?} b={:?}",
        task.state,
        task.holds_slot,
        view.state,
        (b_execution.state, b_execution.terminal.clone())
    );
    eprintln!(
        "finished-path: started={:?} unknown_tools={:?}",
        started_events(&h.rt, &h.session),
        unknown_tool_results(&h.rt, &h.session)
    );
    assert_eq!(
        started_events(&h.rt, &h.session),
        Vec::<String>::new(),
        "no capability may cross its execution boundary"
    );
    assert!(!h.workspace.join("a.txt").exists());
    // Current contract: B really completed and its terminal is durable, but the
    // failed park retains a lifecycle obligation and re-labels B's successful
    // tool result as an unknown outcome.
    assert_eq!(b_execution.state, cayw_core::ExecutionState::Completed);
    assert_eq!(task.state, TaskState::Draining);
    assert!(
        task.holds_slot,
        "the failed park must keep the slot: {task:?}"
    );
    assert_eq!(view.state, SessionState::NeedsReconciliation);
    assert_eq!(
        unknown_tool_results(&h.rt, &h.session),
        vec!["wait-b".to_string()],
        "the completed cell's tool result is currently re-labelled unknown"
    );
    assert!(
        matches!(
            result,
            Err(cayw_runtime::RuntimeError::RecoveryRequired {
                outcome_unknown: true,
                ..
            })
        ),
        "the conservative lifecycle obligation is reported: {result:?}"
    );
    db.execute_batch("DROP TRIGGER fail_finished_park;")
        .unwrap();
    let recovered = h.rt.recover(&h.session).unwrap();
    let task = h.rt.task(&task_id).unwrap();
    eprintln!(
        "finished-path recovery: session={:?} task={:?} holds_slot={} running={} started={:?}",
        recovered.state,
        task.state,
        task.holds_slot,
        h.rt.health().unwrap().running,
        started_events(&h.rt, &h.session)
    );
    // Recovery releases the slot, but the session keeps the reconciliation
    // requirement created by the unknown tool result.
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(task.state, TaskState::Interrupted);
    assert!(!task.holds_slot);
    assert_eq!(h.rt.health().unwrap().running, 0);
    assert_eq!(started_events(&h.rt, &h.session), Vec::<String>::new());
    assert!(!h.workspace.join("a.txt").exists());
}

// ---------------------------------------------------------------------------
// 3. One live, non-waiting cell plus one approval wait keeps the only slot.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn one_live_cell_keeps_the_only_slot_while_another_awaits_approval() {
    let mut h = harness();
    let worker = spawn_prompt(&h.rt, &h.session);

    // Cell A stops before a real approval. Cell B stays live after its second
    // yield (it has not stopped before a capability boundary), so the task must
    // not release its lone execution slot while A waits.
    let round = next_round(&mut h.rounds).await;
    tool(
        round,
        "exec-a",
        exec("yield_control(); return cayw.call('fs.write',{path='a.txt',content='a'})"),
    );
    let round = next_round(&mut h.rounds).await;
    let a = page(&round, "exec-a");
    assert_eq!(a.status, CellStatus::Yielded);
    tool(
        round,
        "exec-b",
        exec("yield_control(); yield_control(); return 'b-alive'"),
    );
    let round = next_round(&mut h.rounds).await;
    let b = page(&round, "exec-b");
    assert_eq!(b.status, CellStatus::Yielded);
    assert_ne!(a.cell_id, b.cell_id);
    // Resume B once; it yields again and stays alive without a pending wait.
    tool(round, "wait-b", wait(&b, 0));
    let round = next_round(&mut h.rounds).await;
    let b = page(&round, "wait-b");
    // `yield_time_ms=0` returns as soon as the VM is scheduled. B runs its
    // second `yield_control()` and stays alive; it never stopped before a
    // capability boundary, so the task must keep its only slot.
    assert!(b.vm_alive, "B must stay a live cell: {b:?}");
    // Resume A with a long wait: the tool stays inside the approval barrier, so
    // no model stream is active while the slot is checked.
    tool(round, "wait-a", wait(&a, 30_000));
    until(&h.rt, &h.session, "approval A", |rt, id| {
        !rt.get(id).unwrap().pending_approvals.is_empty()
    })
    .await;
    let approval = h.rt.get(&h.session).unwrap().pending_approvals[0].clone();
    let task_id = approval.input.task_id.clone();
    let cayw_core::ApprovalTarget::CapabilityCall { key } = &approval.input.target else {
        panic!("approval must target the capability call");
    };
    assert_eq!(key.execution_id, a.cell_id);
    // Barrier state: A waits for approval, B is alive on its original page, the
    // task keeps the only slot, and no model stream is active.
    let task = h.rt.task(&task_id).unwrap();
    assert_eq!(task.state, TaskState::Running);
    assert!(task.holds_slot, "the only slot must stay held: {task:?}");
    assert_eq!(task.dispatches, 1, "no re-dispatch while B stays live");
    assert!(
        b.vm_alive,
        "B's original wait page must still describe a live VM: {b:?}"
    );
    assert_eq!(
        h.rt.get(&h.session).unwrap().pending_approvals.len(),
        1,
        "A's approval must still be pending"
    );
    assert_eq!(
        h.live.load(Ordering::SeqCst),
        0,
        "the approval barrier must not keep a model stream active"
    );
    assert_eq!(
        started_events(&h.rt, &h.session),
        Vec::<String>::new(),
        "A never crossed the approval boundary"
    );

    // Cancel instead of approving: the known Cancelled outcome must not run A.
    let task = h.rt.task(&task_id).unwrap();
    assert!(h.rt.cancel(&task.cancel_target()).unwrap());
    let result = tokio::time::timeout(Duration::from_secs(15), worker)
        .await
        .expect("cancellation must not wedge the worker")
        .unwrap();
    let task = h.rt.task(&task_id).unwrap();
    eprintln!(
        "two-cell cancel: task={:?} holds_slot={} started={:?} result={result:?}",
        task.state,
        task.holds_slot,
        started_events(&h.rt, &h.session)
    );
    assert_eq!(task.state, TaskState::Cancelled);
    assert_eq!(
        started_events(&h.rt, &h.session),
        Vec::<String>::new(),
        "A never crossed the approval boundary"
    );
    assert!(!h.workspace.join("a.txt").exists());
    assert!(unknown_tool_results(&h.rt, &h.session).is_empty());
    assert_eq!(
        h.peak.load(Ordering::SeqCst),
        1,
        "sequential execution must never overlap provider streams"
    );
}
