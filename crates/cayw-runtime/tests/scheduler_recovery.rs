//! Independent acceptance for the Issue 11 scheduler combination.
//!
//! The fixed offline Provider is synthetic; the queue, Core store, Cells
//! journal, Host permissions and file writes are the real product. Every
//! scenario states its K (concurrency), C (accepted capacity) and A (fairness
//! window) and uses a durable SQLite trigger instead of a timing race.
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_requests::TaskIntent;
use cayw_runtime::{
    Runtime, RuntimeConfig, RuntimeError, ScopedPermission, TaskInput, TaskPriority, TaskRecord,
    TaskState,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

// K = 2, C = 3, A = 8. A recovered dormant entry must never consume a slot.
fn settings() -> RuntimeConfig {
    RuntimeConfig {
        max_concurrent_runs: 2,
        max_queued_runs: 3,
        fairness_after_dispatches: 8,
        max_run_time: Duration::from_secs(60),
        // Long enough for a real child process to claim a queued Task and wait
        // without being judged by a product deadline. The test kills it.
        max_queue_time: Duration::from_secs(120),
        permissions: vec![
            ScopedPermission::exact("fs.write", json!({"path":"a.txt","content":"approved-a"})),
            ScopedPermission::exact("fs.write", json!({"path":"b.txt","content":"approved-b"})),
            ScopedPermission::exact("fs.write", json!({"path":"c.txt","content":"approved-c"})),
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"safe.txt","content":"approved-safe"}),
            ),
        ],
        ..Default::default()
    }
}

fn open(dir: &tempfile::TempDir, provider: Arc<dyn Provider>) -> Runtime {
    Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        provider,
        Model::new("fixture", "scheduler-recovery"),
        settings(),
    )
    .unwrap()
}

fn script_for(text: &str) -> &'static str {
    match text {
        "write a" => "return cayw.call('fs.write',{path='a.txt',content='approved-a'})",
        "hold b" => "return cayw.call('fs.write',{path='b.txt',content='approved-b'})",
        "write c" => "return cayw.call('fs.write',{path='c.txt',content='approved-c'})",
        "write safe" => "return cayw.call('fs.write',{path='safe.txt',content='approved-safe'})",
        _ => "return 1",
    }
}

/// Synthetic model: the first request for a gated text blocks until the test
/// releases it. Every later request (the tool-result turn) is not gated.
struct FixtureProvider {
    calls: Arc<Mutex<Vec<String>>>,
    gates: Mutex<HashMap<String, Arc<Semaphore>>>,
    pending: Mutex<HashSet<String>>,
}
impl FixtureProvider {
    fn new(gated: &[&str]) -> Arc<Self> {
        let gates = gated
            .iter()
            .map(|text| (text.to_string(), Arc::new(Semaphore::new(0))))
            .collect::<HashMap<_, _>>();
        let pending = gated.iter().map(|text| text.to_string()).collect();
        Arc::new(Self {
            calls: Arc::default(),
            gates: Mutex::new(gates),
            pending: Mutex::new(pending),
        })
    }
    fn release(&self, text: &str) {
        let gate = self
            .gates
            .lock()
            .unwrap()
            .get(text)
            .cloned()
            .unwrap_or_else(|| panic!("test text {text} has no gate"));
        gate.add_permits(1);
    }
    fn calls_for(&self, text: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == text)
            .count()
    }
    async fn entered(&self, text: &str) {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if self.calls_for(text) > 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the model never received {text}"))
    }
}
impl Provider for FixtureProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let text = request
            .messages
            .iter()
            .rev()
            .find(|message| matches!(message, Message::User { .. }))
            .unwrap()
            .text();
        self.calls.lock().unwrap().push(text.clone());
        let message = cayw_runtime::fixture::script_response(
            &request,
            script_for(&text),
            &format!("slot-{}-{}", text.replace(' ', "-"), request.messages.len()),
        );
        let gate = if self.pending.lock().unwrap().remove(&text) {
            self.gates.lock().unwrap().get(&text).cloned()
        } else {
            None
        };
        Box::pin(async_stream::stream! {
            if let Some(gate) = gate {
                gate.acquire_owned().await.unwrap().forget();
            }
            yield AssistantEvent::Done { message };
        })
    }
}

/// A child process must never reach the model: it is killed while admission
/// waits behind two real slots.
struct NeverCalled;
impl Provider for NeverCalled {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        unreachable!("a claimed queued Task that was never admitted must not call the model")
    }
}

fn receipt(name: &str, value: Value) {
    let Some(dir) = std::env::var_os("CAYW_SCHEDULER_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .unwrap();
}

/// Install a durable trigger that fails only this Task's terminal queue write.
/// It stays installed for the whole scenario, so no timing window matters and
/// unrelated Tasks keep settling normally.
fn fail_this_task_terminal(dir: &Path, task: &str) {
    let db = rusqlite::Connection::open(dir.join("session.db")).unwrap();
    db.execute_batch(&format!(
        "CREATE TRIGGER fail_scheduler_queue_terminal BEFORE UPDATE ON runtime_tasks
         WHEN NEW.id = '{task}' AND NEW.state IN ('completed','failed','cancelled','outcome_unknown')
         BEGIN SELECT RAISE(ABORT, 'injected queue terminal failure'); END;"
    ))
    .unwrap();
}

fn child_paths() -> (PathBuf, PathBuf, String) {
    (
        PathBuf::from(std::env::var("CAYW_SCHEDULER_RECOVERY_CHILD_DB").unwrap()),
        PathBuf::from(std::env::var("CAYW_SCHEDULER_RECOVERY_CHILD_WORKSPACE").unwrap()),
        std::env::var("CAYW_SCHEDULER_RECOVERY_CHILD_TASK").unwrap(),
    )
}

async fn claim_queued_child() -> ! {
    let (db, workspace, task) = child_paths();
    let runtime = Runtime::open(
        db,
        workspace,
        Arc::new(NeverCalled),
        Model::new("fixture", "scheduler-recovery"),
        settings(),
    )
    .unwrap_or_else(|error| {
        eprintln!("claim child could not open its original runtime: {error}");
        std::process::exit(30);
    });
    eprintln!("claim child waits for admission of task {task}");
    let _ = runtime.resume(&task, CancellationToken::new()).await;
    // The parent must kill this process while admission waits.
    std::process::exit(31);
}

fn spawn_claim_child(dir: &tempfile::TempDir, test_name: &str, task: &str) -> Child {
    let log = std::fs::File::create(dir.path().join("claim-child.log")).unwrap();
    Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("CAYW_SCHEDULER_RECOVERY_CHILD", "1")
        .env(
            "CAYW_SCHEDULER_RECOVERY_CHILD_DB",
            dir.path().join("session.db"),
        )
        .env(
            "CAYW_SCHEDULER_RECOVERY_CHILD_WORKSPACE",
            dir.path().join("workspace"),
        )
        .env("CAYW_SCHEDULER_RECOVERY_CHILD_TASK", task)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap()
}

async fn child_claimed(runtime: &Runtime, id: &str, child: &mut Child) -> TaskRecord {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("claim child exited before claiming the queued Task: {status}");
        }
        let task = runtime.task(id).unwrap();
        if task.worker_id.is_some() {
            return task;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "claim child did not commit its worker claim in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn workspace_file(dir: &tempfile::TempDir, name: &str) -> String {
    std::fs::read_to_string(dir.path().join("workspace").join(name)).unwrap()
}

/// K=2, C=3, A=8.
///
/// A real authorized `fs.write` completes, then its durable queue terminal is
/// denied by a persistent trigger. The Task must enter Draining with its slot
/// retained. A second Session's real, exactly authorized `fs.write` must still
/// use the other safe slot and succeed; a third Task must stay queued and must
/// never make active executions exceed K. The original effect is never replayed:
/// recovery fences the old Attempt and leaves the file and the model history
/// unchanged.
#[tokio::test]
async fn draining_slot_keeps_capacity_while_an_authorized_write_uses_the_other_slot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let provider = FixtureProvider::new(&["write a", "hold b"]);
    let rt = open(&dir, provider.clone());
    let peer = open(&dir, provider.clone());

    let session_a = rt.create(None).unwrap().id;
    let a = rt.enqueue(&session_a, "write a").unwrap();
    fail_this_task_terminal(dir.path(), &a.id);

    let run_rt = rt.clone();
    let run_id = a.id.clone();
    let run_a = tokio::spawn(async move { run_rt.resume(&run_id, CancellationToken::new()).await });
    provider.entered("write a").await;
    wait_until("A holds its admitted slot", || {
        rt.task(&a.id)
            .is_ok_and(|task| task.state == TaskState::Running && task.holds_slot)
    })
    .await;
    let old_lease = rt
        .get(&session_a)
        .unwrap()
        .attempt
        .expect("A has a real Core attempt");
    assert!(
        matches!(peer.recover(&session_a), Err(RuntimeError::Busy)),
        "explicit recovery must wait until A's original owner has stopped"
    );
    receipt(
        "r01-group1-running",
        json!({"K":2,"C":3,"A":8,"task": rt.task(&a.id).unwrap(), "session": rt.get(&session_a).unwrap(), "health": rt.health().unwrap()}),
    );

    provider.release("write a");
    let error = tokio::time::timeout(Duration::from_secs(30), run_a)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(
            &error,
            RuntimeError::RecoveryRequired {
                outcome_unknown: false,
                ..
            }
        ),
        "a denied queue terminal is a known-effect recovery failure: {error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("injected queue terminal failure"),
        "the injected denial must be the reported cause: {error}"
    );

    let draining = rt.task(&a.id).unwrap();
    assert_eq!(draining.state, TaskState::Draining);
    assert!(
        draining.holds_slot,
        "an unfinished queue terminal must retain the original slot"
    );
    assert_eq!(draining.input, a.input);
    assert!(draining.attempt_id.is_some());
    assert_eq!(rt.health().unwrap().running, 1);
    assert_eq!(workspace_file(&dir, "a.txt"), "approved-a");
    assert!(
        rt.audit(&session_a, 0, 1000)
            .unwrap()
            .iter()
            .all(|event| !event.kind.starts_with("run_")),
        "the rolled-back terminal must leave no run_* audit"
    );
    assert!(
        matches!(
            rt.resume(&a.id, CancellationToken::new()).await,
            Err(RuntimeError::InvalidInput(_))
        ),
        "a Draining Task must not resume"
    );
    assert_eq!(
        provider.calls_for("write a"),
        2,
        "the model saw exactly the original exec and final turn"
    );
    receipt(
        "r01-group1-draining",
        json!({"task": draining, "health": rt.health().unwrap(), "audit": rt.audit(&session_a,0,1000).unwrap(), "file": workspace_file(&dir,"a.txt")}),
    );

    // The second Session uses the other safe slot with a real, exactly
    // authorized write. It is paused in the model so the third Task can be
    // measured against the concurrency limit.
    let session_b = rt.create(None).unwrap().id;
    let b = rt.enqueue(&session_b, "hold b").unwrap();
    let b_rt = rt.clone();
    let b_id = b.id.clone();
    let run_b = tokio::spawn(async move { b_rt.resume(&b_id, CancellationToken::new()).await });
    provider.entered("hold b").await;
    wait_until("B holds the second safe slot", || {
        rt.task(&b.id)
            .is_ok_and(|task| task.state == TaskState::Running && task.holds_slot)
    })
    .await;
    assert_eq!(rt.health().unwrap().running, 2);

    let session_c = rt.create(None).unwrap().id;
    let c = rt.enqueue(&session_c, "write c").unwrap();
    let c_rt = rt.clone();
    let c_id = c.id.clone();
    let run_c = tokio::spawn(async move { c_rt.resume(&c_id, CancellationToken::new()).await });
    wait_until("C is claimed but waits for a slot", || {
        rt.task(&c.id)
            .is_ok_and(|task| task.state == TaskState::Queued && task.worker_id.is_some())
    })
    .await;
    let waiting_c = rt.task(&c.id).unwrap();
    assert!(waiting_c.attempt_id.is_none());
    assert_eq!(
        rt.health().unwrap().running,
        2,
        "active executions must never exceed K"
    );
    assert_eq!(rt.health().unwrap().queued, 1);
    assert_eq!(
        provider.calls_for("write c"),
        0,
        "a Task beyond K must not reach the model"
    );
    // C=3: A (draining), B (running), C (queued) already fill accepted capacity.
    let session_d = rt.create(None).unwrap().id;
    assert!(matches!(
        rt.enqueue(&session_d, "beyond capacity"),
        Err(RuntimeError::QueueFull)
    ));
    receipt(
        "r01-group1-k-limit",
        json!({"task_a": rt.task(&a.id).unwrap(), "task_b": rt.task(&b.id).unwrap(), "task_c": waiting_c, "health": rt.health().unwrap()}),
    );

    provider.release("hold b");
    tokio::time::timeout(Duration::from_secs(20), run_b)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(rt.task(&b.id).unwrap().state, TaskState::Completed);
    assert_eq!(workspace_file(&dir, "b.txt"), "approved-b");
    tokio::time::timeout(Duration::from_secs(20), run_c)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(rt.task(&c.id).unwrap().state, TaskState::Completed);
    assert_eq!(workspace_file(&dir, "c.txt"), "approved-c");
    assert_eq!(
        rt.health().unwrap().running,
        1,
        "only Draining A holds a slot"
    );

    // The original result is retained, not replayed. Recovery fences the old
    // Attempt; the old lease cannot append anything and resume stays rejected.
    let calls_before = provider.calls_for("write a");
    let events_before = rt.events(&session_a, 0, 1000).unwrap();
    let recovered_view = peer.recover(&session_a).unwrap();
    assert!(recovered_view.attempt.is_none());
    assert_eq!(
        rt.events(&session_a, 0, 1000).unwrap(),
        events_before,
        "recovery must not rewrite or replay the original Core history"
    );
    assert_eq!(
        events_before
            .iter()
            .filter(|event| matches!(event.event, cayw_core::CoreEvent::CapabilitySettled { .. }))
            .count(),
        1,
        "the real write must have exactly one durable settlement"
    );
    let recovered = rt.task(&a.id).unwrap();
    assert_eq!(recovered.state, TaskState::Interrupted);
    assert!(!recovered.holds_slot);
    assert!(recovered.cancel_requested);
    assert_eq!(
        recovered.error.as_deref(),
        Some("explicit worker recovery; inspect Core outcomes")
    );
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(workspace_file(&dir, "a.txt"), "approved-a");
    assert_eq!(
        provider.calls_for("write a"),
        calls_before,
        "recovery must not run the original model work again"
    );
    assert!(
        rt.store()
            .append_host_event(&old_lease, "late-worker-result", json!({}))
            .is_err(),
        "the old Attempt must not append a late result"
    );
    assert!(
        matches!(
            rt.resume(&a.id, CancellationToken::new()).await,
            Err(RuntimeError::InvalidInput(_))
        ),
        "an Interrupted Task cannot be implicitly replayed"
    );
    assert!(
        rt.audit(&session_a, 0, 1000)
            .unwrap()
            .iter()
            .all(|event| !event.kind.starts_with("run_")),
        "recovery must not invent a terminal the queue never committed"
    );
    let terminal = rt.task(&a.id).unwrap();
    peer.recover(&session_a).unwrap();
    assert_eq!(
        rt.task(&a.id).unwrap(),
        terminal,
        "second recovery is idempotent"
    );
    receipt(
        "r01-group1-recovered",
        json!({"task": rt.task(&a.id).unwrap(), "view": rt.get(&session_a).unwrap(), "audit": rt.audit(&session_a,0,1000).unwrap(), "file": workspace_file(&dir,"a.txt"), "model_calls": provider.calls_for("write a")}),
    );
}

fn child_test_cancelled() -> bool {
    std::env::var_os("CAYW_SCHEDULER_RECOVERY_CHILD").is_some()
}

/// K=2, C=3.
///
/// A real child worker claims a queued Task and blocks behind two occupied
/// slots. The worker is SIGKILLed before it can observe anything, and the Task
/// is then cancelled with the exact pre-crash worker identity. Public recovery
/// must close exactly one `run_cancelled` terminal, release accepted capacity,
/// and stay idempotent. The cancelled entry must not permanently occupy C.
#[tokio::test]
async fn killed_claimed_cancelled_queue_recovers_to_one_terminal_and_releases_capacity() {
    if child_test_cancelled() {
        claim_queued_child().await;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let provider = FixtureProvider::new(&["hold slot one", "hold slot two"]);
    let rt = open(&dir, provider.clone());

    let mut occupants = Vec::new();
    for text in ["hold slot one", "hold slot two"] {
        let session = rt.create(None).unwrap().id;
        let task = rt.enqueue(&session, text).unwrap();
        let run_rt = rt.clone();
        let id = task.id.clone();
        let run = tokio::spawn(async move { run_rt.resume(&id, CancellationToken::new()).await });
        provider.entered(text).await;
        wait_until("an occupant holds its slot", || {
            rt.task(&task.id)
                .is_ok_and(|record| record.state == TaskState::Running && record.holds_slot)
        })
        .await;
        occupants.push((text, task, run));
    }
    assert_eq!(rt.health().unwrap().running, 2);

    let session = rt.create(None).unwrap().id;
    let victim = rt.enqueue(&session, "claimed then killed").unwrap();
    let mut child = spawn_claim_child(
        &dir,
        "killed_claimed_cancelled_queue_recovers_to_one_terminal_and_releases_capacity",
        &victim.id,
    );
    let claimed = child_claimed(&rt, &victim.id, &mut child).await;
    assert_eq!(claimed.state, TaskState::Queued);
    assert!(claimed.attempt_id.is_none());
    assert!(!claimed.cancel_requested);
    assert!(
        matches!(rt.recover(&session), Err(RuntimeError::Busy)),
        "recovery must not proceed while the original claimed worker is alive"
    );

    // The real worker is SIGKILLed while it waits, so it cannot race or observe
    // the cancellation. The durable cancellation is then recorded exactly as a
    // peer would record it before the dead worker could settle: queued, worker
    // Some, attempt None, cancel_requested=1. Recovery is the only path that
    // can close it.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(rt.cancel(&claimed.cancel_target()).unwrap());
    let cancelled = rt.task(&victim.id).unwrap();
    assert_eq!(cancelled.state, TaskState::Queued);
    assert!(cancelled.cancel_requested);
    assert_eq!(cancelled.worker_id, claimed.worker_id);
    assert_eq!(cancelled.attempt_id, None);
    assert!(!cancelled.holds_slot);
    assert_eq!(rt.task(&victim.id).unwrap(), cancelled);
    assert!(
        matches!(
            rt.resume(&victim.id, CancellationToken::new()).await,
            Err(RuntimeError::InvalidInput(_))
        ),
        "a cancelled claimed Task must not be resumed twice"
    );

    // Accepted capacity is still fully occupied before recovery.
    let blocked = rt.create(None).unwrap().id;
    assert!(matches!(
        rt.enqueue(&blocked, "capacity is still held"),
        Err(RuntimeError::QueueFull)
    ));
    receipt(
        "r01-group2-before-recovery",
        json!({"task": cancelled, "health": rt.health().unwrap(), "audit": rt.audit(&session,0,1000).unwrap()}),
    );

    rt.recover(&session).unwrap();
    let terminal = rt.task(&victim.id).unwrap();
    assert_eq!(terminal.state, TaskState::Cancelled);
    assert!(terminal.cancel_requested);
    assert_eq!(terminal.attempt_id, None);
    assert!(!terminal.holds_slot);
    assert_eq!(terminal.fence, claimed.fence + 1);
    assert_eq!(terminal.input, victim.input);
    assert_eq!(
        rt.health().unwrap().running,
        2,
        "occupants keep their slots"
    );
    assert_eq!(rt.health().unwrap().queued, 0);
    let cancelled_audits = rt
        .audit(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.kind == "run_cancelled" && event.task_id.as_deref() == Some(&victim.id)
        })
        .collect::<Vec<_>>();
    assert_eq!(cancelled_audits.len(), 1);
    assert_eq!(
        cancelled_audits[0].data["worker_id"].as_str(),
        claimed.worker_id.as_deref()
    );
    // The pre-recovery worker target no longer selects the recovered Task.
    assert!(matches!(
        rt.cancel(&claimed.cancel_target()),
        Err(RuntimeError::StaleCancellation)
    ));
    // The recovered row is terminal, so its own target cannot cancel it again.
    let terminal_target = rt.task(&victim.id).unwrap().cancel_target();
    assert!(!rt.cancel(&terminal_target).unwrap());
    rt.recover(&session).unwrap();
    assert_eq!(rt.task(&victim.id).unwrap(), terminal);
    assert_eq!(
        rt.audit(&session, 0, 1000)
            .unwrap()
            .iter()
            .filter(|event| event.kind == "run_cancelled")
            .count(),
        1,
        "recovery must be idempotent"
    );

    // Capacity is genuinely released, not merely reported as free.
    let released = rt.create(None).unwrap().id;
    let next = rt.enqueue(&released, "capacity released").unwrap();
    assert_eq!(next.state, TaskState::Queued);
    assert_eq!(rt.health().unwrap().queued, 1);
    assert!(
        rt.cancel(&rt.task(&next.id).unwrap().cancel_target())
            .unwrap()
    );
    assert_eq!(rt.task(&next.id).unwrap().state, TaskState::Cancelled);
    assert_eq!(rt.health().unwrap().queued, 0);
    receipt(
        "r01-group2-recovered",
        json!({"task": rt.task(&victim.id).unwrap(), "audit": rt.audit(&session,0,1000).unwrap(), "health": rt.health().unwrap()}),
    );

    for (text, task, run) in occupants {
        provider.release(text);
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Completed);
    }
    assert_eq!(rt.health().unwrap().running, 0);
}

/// K=2, C=3.
///
/// A killed worker's claimed queued Task was never executed. After recovery it
/// must become dormant (worker unbound, fence advanced) while keeping its
/// input, High priority, ready order and ready round. Because it is no longer a
/// ready candidate, it must not block an unrelated safe ready Task. It must
/// still be explicitly resumable on the same Task.
#[tokio::test]
async fn recovered_high_priority_dormant_task_does_not_block_safe_ready_work() {
    if child_test_cancelled() {
        claim_queued_child().await;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let provider = FixtureProvider::new(&["hold one slot", "hold two slots"]);
    let rt = open(&dir, provider.clone());

    // Both execution slots are occupied, so the killed worker can be claimed
    // and then blocked in admission without ever reaching the model.
    let mut occupants = Vec::new();
    for text in ["hold one slot", "hold two slots"] {
        let session = rt.create(None).unwrap().id;
        let task = rt.enqueue(&session, text).unwrap();
        let run_rt = rt.clone();
        let id = task.id.clone();
        let run = tokio::spawn(async move { run_rt.resume(&id, CancellationToken::new()).await });
        provider.entered(text).await;
        wait_until("an occupant holds its slot", || {
            rt.task(&task.id)
                .is_ok_and(|record| record.state == TaskState::Running && record.holds_slot)
        })
        .await;
        occupants.push((text, task, run));
    }
    assert_eq!(rt.health().unwrap().running, 2);

    let session = rt.create(None).unwrap().id;
    let victim = rt
        .enqueue_scheduled(
            &session,
            TaskInput {
                text: "dormant high priority".into(),
                images: vec![],
            },
            TaskIntent::General,
            TaskPriority::High,
        )
        .unwrap();
    assert_eq!(victim.priority, TaskPriority::High);
    let mut child = spawn_claim_child(
        &dir,
        "recovered_high_priority_dormant_task_does_not_block_safe_ready_work",
        &victim.id,
    );
    let claimed = child_claimed(&rt, &victim.id, &mut child).await;
    assert_eq!(claimed.state, TaskState::Queued);
    assert!(claimed.attempt_id.is_none());
    let order = claimed
        .ready_order
        .expect("a claim records its ready order");
    let since = claimed.ready_since_dispatch;
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(rt.task(&victim.id).unwrap(), claimed);

    // Free one safe slot for the unrelated Task. This occupant is unrelated and
    // completes normally; the other occupant keeps one slot busy.
    let (first_text, first_occupant, first_run) = occupants.remove(0);
    provider.release(first_text);
    tokio::time::timeout(Duration::from_secs(20), first_run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        rt.task(&first_occupant.id).unwrap().state,
        TaskState::Completed
    );
    assert_eq!(rt.health().unwrap().running, 1);

    rt.recover(&session).unwrap();
    let recovered = rt.task(&victim.id).unwrap();
    assert_eq!(recovered.state, TaskState::Queued);
    assert_eq!(recovered.id, victim.id);
    assert_eq!(recovered.input, victim.input);
    assert_eq!(recovered.priority, TaskPriority::High);
    assert_eq!(recovered.attempt_id, None);
    assert_eq!(recovered.worker_id, None, "recovery unbinds the dead claim");
    assert_eq!(recovered.fence, claimed.fence + 1);
    assert_eq!(recovered.ready_order, Some(order));
    assert_eq!(recovered.ready_since_dispatch, since);
    assert!(!recovered.cancel_requested);
    assert!(matches!(
        rt.cancel(&claimed.cancel_target()),
        Err(RuntimeError::StaleCancellation)
    ));
    // A second recovery leaves the dormant accepted entry unchanged.
    rt.recover(&session).unwrap();
    assert_eq!(rt.task(&victim.id).unwrap(), recovered);

    // An unrelated safe ready Task must run even though the recovered entry has
    // higher priority and an earlier ready order: it owns no ready candidate.
    let safe_session = rt.create(None).unwrap().id;
    let safe = rt.enqueue(&safe_session, "write safe").unwrap();
    let safe_rt = rt.clone();
    let safe_id = safe.id.clone();
    let run_safe =
        tokio::spawn(async move { safe_rt.resume(&safe_id, CancellationToken::new()).await });
    tokio::time::timeout(Duration::from_secs(20), run_safe)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(rt.task(&safe.id).unwrap().state, TaskState::Completed);
    assert_eq!(workspace_file(&dir, "safe.txt"), "approved-safe");
    assert_eq!(rt.task(&victim.id).unwrap(), recovered);
    assert_eq!(provider.calls_for("dormant high priority"), 0);
    assert_eq!(rt.health().unwrap().running, 1, "only the occupant runs");
    assert_eq!(rt.health().unwrap().queued, 1);
    receipt(
        "r01-group3-safe-ran",
        json!({"K":2,"C":3,"A":8,"recovered": rt.task(&victim.id).unwrap(), "safe": rt.task(&safe.id).unwrap(), "health": rt.health().unwrap()}),
    );

    // The accepted work is still the same Task and can be explicitly resumed.
    let resume_rt = rt.clone();
    let victim_id = victim.id.clone();
    let run_victim =
        tokio::spawn(async move { resume_rt.resume(&victim_id, CancellationToken::new()).await });
    tokio::time::timeout(Duration::from_secs(20), run_victim)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let completed = rt.task(&victim.id).unwrap();
    assert_eq!(completed.state, TaskState::Completed);
    assert_eq!(completed.input, victim.input);
    assert_eq!(completed.priority, TaskPriority::High);
    assert_eq!(completed.fence, claimed.fence + 2);
    assert_eq!(provider.calls_for("dormant high priority"), 2);
    receipt(
        "r01-group3-explicit-resume",
        json!({"task": completed, "health": rt.health().unwrap(), "model_calls": provider.calls_for("dormant high priority")}),
    );

    let (last_text, last_occupant, last_run) = occupants.remove(0);
    provider.release(last_text);
    tokio::time::timeout(Duration::from_secs(20), last_run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        rt.task(&last_occupant.id).unwrap().state,
        TaskState::Completed
    );
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.health().unwrap().queued, 0);
}
