use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_requests::OperationScope;
use cayw_runtime::{RequestPolicy, Runtime, RuntimeConfig, RuntimeError, TaskRecord, TaskState};
use serde_json::json;
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

struct Controlled {
    calls: Arc<Mutex<Vec<String>>>,
    entered: Arc<Notify>,
    release: Arc<Semaphore>,
}
impl Provider for Controlled {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        self.calls.lock().unwrap().push(
            request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User { .. }))
                .unwrap()
                .text(),
        );
        self.entered.notify_one();
        let release = self.release.clone();
        Box::pin(async_stream::stream! {
            tokio::select! { _ = cancel.cancelled() => {}, permit = release.acquire() => { permit.unwrap().forget(); } }
            let mut message = AssistantMessage::new(&request.model);
            message.content.push(Content::text("known terminal")); message.stop_reason = StopReason::Stop;
            yield AssistantEvent::Done { message };
        })
    }
}

/// A killed worker must never reach the model before it has been admitted.
struct NeverCalled;
impl Provider for NeverCalled {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        unreachable!("a claimed queued task that was never admitted must not call the model")
    }
}

fn settings() -> RuntimeConfig {
    RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 4,
        max_run_time: Duration::from_secs(10),
        ..Default::default()
    }
}
/// Long enough that a real child process can claim a queued task and be killed
/// while it waits for the single execution slot.
fn patient() -> RuntimeConfig {
    RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 4,
        max_run_time: Duration::from_secs(120),
        max_queue_time: Duration::from_secs(60),
        ..Default::default()
    }
}
fn request_settings() -> RuntimeConfig {
    RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 4,
        max_run_time: Duration::from_secs(10),
        request_policy: Some(RequestPolicy {
            permission_policy_id: "durable-queue-fixture".into(),
            operations: vec![OperationScope::exact(
                "accepted-queued-read",
                "fs.read",
                json!({"path":"unused-durable-queue.txt"}),
                "durable-queue-policy",
            )],
        }),
        ..Default::default()
    }
}
fn open_with(dir: &tempfile::TempDir, p: Arc<dyn Provider>, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        dir.path().join("queue.db"),
        dir.path().join("workspace"),
        p,
        Model::new("fixture", "queue"),
        config,
    )
    .unwrap()
}
fn open(dir: &tempfile::TempDir, p: Arc<dyn Provider>) -> Runtime {
    open_with(dir, p, settings())
}
fn provider() -> Arc<Controlled> {
    Arc::new(Controlled {
        calls: Arc::default(),
        entered: Arc::default(),
        release: Arc::new(Semaphore::new(0)),
    })
}
async fn entered(p: &Controlled) {
    tokio::time::timeout(Duration::from_secs(10), p.entered.notified())
        .await
        .unwrap();
}
/// Wait for `resume` to commit its worker claim without any Core attempt.
async fn claimed(runtime: &Runtime, id: &str) -> TaskRecord {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let task = runtime.task(id).unwrap();
            if task.worker_id.is_some() {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("queued task {id} was never claimed"))
}

/// A queued task that was accepted but never claimed has no Core attempt. An
/// explicit recovery must not revoke that acceptance, fabricate a terminal or
/// change the durable history; only `resume` may run the original input.
#[tokio::test]
async fn recover_preserves_a_never_started_queued_task_and_resume_reuses_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let p = provider();
    let rt = open(&dir, p.clone());
    let session = rt.create(None).unwrap().id;
    let accepted = rt.enqueue(&session, "accepted but never started").unwrap();
    assert_eq!(accepted.state, TaskState::Queued);
    assert!(accepted.worker_id.is_none());
    assert!(accepted.attempt_id.is_none());
    let history = rt.events(&session, 0, 1000).unwrap();

    rt.recover(&session).unwrap();
    assert_eq!(rt.task(&accepted.id).unwrap(), accepted);
    assert_eq!(rt.tasks(&session).unwrap(), vec![accepted.clone()]);
    assert_eq!(rt.health().unwrap().queued, 1);
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.events(&session, 0, 1000).unwrap(), history);
    assert!(p.calls.lock().unwrap().is_empty());

    drop(rt);
    let rt = open(&dir, p.clone());
    // Reopening neither claims nor runs accepted work.
    assert!(p.calls.lock().unwrap().is_empty());
    assert_eq!(rt.task(&accepted.id).unwrap(), accepted);

    let run_rt = rt.clone();
    let run_id = accepted.id.clone();
    let run = tokio::spawn(async move { run_rt.resume(&run_id, CancellationToken::new()).await });
    entered(&p).await;
    assert_eq!(*p.calls.lock().unwrap(), vec!["accepted but never started"]);
    let running = rt.task(&accepted.id).unwrap();
    assert_eq!(running.state, TaskState::Running);
    assert_eq!(running.id, accepted.id);
    assert_eq!(running.input, accepted.input);
    assert_ne!(running.id, running.worker_id.clone().unwrap());
    assert_ne!(running.id, running.attempt_id.clone().unwrap());
    assert_ne!(running.worker_id, running.attempt_id);
    p.release.add_permits(1);
    run.await.unwrap().unwrap();

    let completed = rt.task(&accepted.id).unwrap();
    assert_eq!(completed.state, TaskState::Completed);
    assert_eq!(completed.input.text, "accepted but never started");
    assert_eq!(rt.tasks(&session).unwrap().len(), 1);
    assert_eq!(rt.health().unwrap().queued, 0);
    assert_eq!(rt.health().unwrap().running, 0);
}

fn killed_worker_child_paths() -> (PathBuf, PathBuf, String) {
    (
        PathBuf::from(std::env::var("CAYW_DURABLE_QUEUE_CHILD_DB").unwrap()),
        PathBuf::from(std::env::var("CAYW_DURABLE_QUEUE_CHILD_WORKSPACE").unwrap()),
        std::env::var("CAYW_DURABLE_QUEUE_CHILD_TASK").unwrap(),
    )
}

async fn killed_worker_child() -> ! {
    let (db, workspace, task) = killed_worker_child_paths();
    let runtime = Runtime::open(
        db,
        workspace,
        Arc::new(NeverCalled),
        Model::new("fixture", "queue"),
        patient(),
    )
    .unwrap_or_else(|error| {
        eprintln!("claim child could not open its original runtime: {error}");
        std::process::exit(30);
    });
    eprintln!("claim child waits for admission of task {task}");
    let _ = runtime.resume(&task, CancellationToken::new()).await;
    // The parent must SIGKILL this process while admission waits. A returned
    // resume future means the fixture no longer reproduces a lost worker.
    std::process::exit(31);
}

fn spawn_claim_child(dir: &tempfile::TempDir, task: &str) -> Child {
    let log = std::fs::File::create(dir.path().join("claim-child.log")).unwrap();
    Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("recover_preserves_a_claimed_queued_task_after_a_killed_worker_and_resume_reclaims_it")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("CAYW_DURABLE_QUEUE_CHILD", "1")
        .env("CAYW_DURABLE_QUEUE_CHILD_DB", dir.path().join("queue.db"))
        .env(
            "CAYW_DURABLE_QUEUE_CHILD_WORKSPACE",
            dir.path().join("workspace"),
        )
        .env("CAYW_DURABLE_QUEUE_CHILD_TASK", task)
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
            panic!("claim child exited before claiming the queued task: {status}");
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

/// A real child process claims a queued task (worker, no attempt) and blocks
/// behind the single execution slot. SIGKILL leaves exactly the state an
/// externally killed worker leaves. Recovery must keep the accepted Task, its
/// input and its fairness position, and a later resume must reclaim that same
/// Task rather than create a replacement.
#[tokio::test]
async fn recover_preserves_a_claimed_queued_task_after_a_killed_worker_and_resume_reclaims_it() {
    if std::env::var_os("CAYW_DURABLE_QUEUE_CHILD").is_some() {
        killed_worker_child().await;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let p = provider();
    let rt = open_with(&dir, p.clone(), patient());
    let occupant_session = rt.create(None).unwrap().id;
    let occupant = rt.enqueue(&occupant_session, "occupant").unwrap();
    let occupant_rt = rt.clone();
    let occupant_id = occupant.id.clone();
    let occupant_run = tokio::spawn(async move {
        occupant_rt
            .resume(&occupant_id, CancellationToken::new())
            .await
    });
    entered(&p).await;
    assert_eq!(rt.health().unwrap().running, 1);

    let session = rt.create(None).unwrap().id;
    let victim = rt.enqueue(&session, "claimed but never attempted").unwrap();
    let mut child = spawn_claim_child(&dir, &victim.id);
    let claimed = child_claimed(&rt, &victim.id, &mut child).await;
    assert_eq!(claimed.state, TaskState::Queued);
    assert!(claimed.attempt_id.is_none());
    assert!(!claimed.cancel_requested);
    let order = claimed
        .ready_order
        .expect("a claim records its fairness position");
    let dead_worker = claimed.worker_id.clone().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(rt.task(&victim.id).unwrap(), claimed);
    assert_eq!(*p.calls.lock().unwrap(), vec!["occupant"]);

    drop(rt);
    let rt = open_with(&dir, p.clone(), patient());
    rt.recover(&session).unwrap();
    let recovered = rt.task(&victim.id).unwrap();
    assert_eq!(recovered.state, TaskState::Queued);
    assert_eq!(recovered.id, victim.id);
    assert_eq!(recovered.input, victim.input);
    assert_eq!(recovered.priority, victim.priority);
    assert_eq!(recovered.attempt_id, None);
    // Recovery fences and unbinds a claimed worker that never began an
    // Attempt. Keeping the dead worker attached would leave the dormant entry
    // as the ready head, and it would block every later safe claim forever.
    assert_eq!(recovered.worker_id, None);
    assert_eq!(recovered.fence, claimed.fence + 1);
    assert_eq!(recovered.ready_order, Some(order));
    assert_eq!(recovered.ready_since_dispatch, claimed.ready_since_dispatch);
    assert!(!recovered.cancel_requested);
    assert_eq!(rt.tasks(&session).unwrap(), vec![recovered.clone()]);
    assert_eq!(rt.health().unwrap().queued, 1);
    // The fenced worker identity no longer controls the Task.
    assert!(matches!(
        rt.cancel(&claimed.cancel_target()),
        Err(RuntimeError::StaleCancellation)
    ));
    assert_eq!(*p.calls.lock().unwrap(), vec!["occupant"]);

    // Recovery is idempotent for the already-unbound dormant entry.
    rt.recover(&session).unwrap();
    assert_eq!(rt.task(&victim.id).unwrap(), recovered);

    p.release.add_permits(1);
    occupant_run.await.unwrap().unwrap();
    assert_eq!(rt.task(&occupant.id).unwrap().state, TaskState::Completed);

    let run_rt = rt.clone();
    let run_id = victim.id.clone();
    let run = tokio::spawn(async move { run_rt.resume(&run_id, CancellationToken::new()).await });
    entered(&p).await;
    assert_eq!(
        *p.calls.lock().unwrap(),
        vec!["occupant", "claimed but never attempted"]
    );
    let running = rt.task(&victim.id).unwrap();
    assert_eq!(running.id, victim.id);
    assert_eq!(running.state, TaskState::Running);
    assert_eq!(running.ready_order, Some(order));
    // Recovery increased the fence once more when it unbound the dead worker;
    // the new claim fences its own worker on top of that.
    assert_eq!(running.fence, claimed.fence + 2);
    assert_ne!(running.worker_id, Some(dead_worker));
    assert!(running.worker_id.is_some());
    assert!(running.attempt_id.is_some());
    // The pre-recovery worker target remains unable to control the new owner.
    assert!(matches!(
        rt.cancel(&claimed.cancel_target()),
        Err(RuntimeError::StaleCancellation)
    ));
    p.release.add_permits(1);
    run.await.unwrap().unwrap();

    let completed = rt.task(&victim.id).unwrap();
    assert_eq!(completed.state, TaskState::Completed);
    assert_eq!(completed.input, victim.input);
    assert_eq!(rt.tasks(&session).unwrap().len(), 1);
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.health().unwrap().queued, 0);
}

/// Only a resumed, claimed entry may be dispatched. Reopening claims nothing,
/// dormant entries never run, and concurrently ready claims dispatch FIFO by
/// their durable ready order while preserving each Task's identity.
#[tokio::test]
async fn reopening_never_runs_tasks_and_explicit_resume_preserves_identity_and_fifo() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let p = provider();
    let rt = open(&dir, p.clone());
    let occupant_session = rt.create(None).unwrap().id;
    let a = rt.create(None).unwrap().id;
    let b = rt.create(None).unwrap().id;
    let occupant = rt
        .enqueue(&occupant_session, "occupy the only slot")
        .unwrap();
    let occupant_rt = rt.clone();
    let occupant_id = occupant.id.clone();
    let occupant_run = tokio::spawn(async move {
        occupant_rt
            .resume(&occupant_id, CancellationToken::new())
            .await
    });
    entered(&p).await;
    let first = rt.enqueue(&a, "first persisted input").unwrap();
    let second = rt.enqueue(&b, "second persisted input").unwrap();
    assert!(first.worker_id.is_none());
    assert!(first.attempt_id.is_none());
    assert!(second.worker_id.is_none());
    assert!(second.attempt_id.is_none());
    drop(rt);
    let rt = open(&dir, p.clone());
    let peer = open(&dir, p.clone());
    assert_eq!(*p.calls.lock().unwrap(), vec!["occupy the only slot"]);
    assert_eq!(rt.task(&first.id).unwrap(), first);
    assert_eq!(rt.task(&second.id).unwrap(), second);
    assert_eq!(peer.health().unwrap().queued, 2);

    // Both accepted tasks are claimed while the only slot is still occupied.
    let a_rt = rt.clone();
    let a_task = first.id.clone();
    let earlier = tokio::spawn(async move { a_rt.resume(&a_task, CancellationToken::new()).await });
    let a_claim = claimed(&rt, &first.id).await;
    let b_rt = peer.clone();
    let b_task = second.id.clone();
    let later = tokio::spawn(async move { b_rt.resume(&b_task, CancellationToken::new()).await });
    let b_claim = claimed(&peer, &second.id).await;
    assert!(a_claim.ready_order.unwrap() < b_claim.ready_order.unwrap());
    assert_eq!(*p.calls.lock().unwrap(), vec!["occupy the only slot"]);

    p.release.add_permits(1);
    occupant_run.await.unwrap().unwrap();
    entered(&p).await;
    assert_eq!(
        *p.calls.lock().unwrap(),
        vec!["occupy the only slot", "first persisted input"]
    );
    let running = rt.task(&first.id).unwrap();
    assert_eq!(running.state, TaskState::Running);
    assert_eq!(running.id, first.id);
    assert_ne!(running.id, running.worker_id.clone().unwrap());
    assert_ne!(running.id, running.attempt_id.clone().unwrap());
    assert_ne!(running.worker_id, running.attempt_id);
    p.release.add_permits(1);
    earlier.await.unwrap().unwrap();
    entered(&p).await;
    assert_eq!(
        *p.calls.lock().unwrap(),
        vec![
            "occupy the only slot",
            "first persisted input",
            "second persisted input"
        ]
    );
    p.release.add_permits(1);
    later.await.unwrap().unwrap();
    assert_eq!(rt.task(&first.id).unwrap().state, TaskState::Completed);
    assert_eq!(rt.task(&second.id).unwrap().state, TaskState::Completed);
    assert_eq!(rt.tasks(&a).unwrap().len(), 1);
    assert_eq!(rt.tasks(&b).unwrap().len(), 1);
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.health().unwrap().queued, 0);
}

#[tokio::test]
async fn another_connection_cancels_running_work_and_never_duplicates_a_session() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let p = provider();
    let rt = open(&dir, p.clone());
    let peer = open(&dir, p.clone());
    let session = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&session, "only once").unwrap();
    let run_rt = rt.clone();
    let run_id = admitted.id.clone();
    let run = tokio::spawn(async move { run_rt.resume(&run_id, CancellationToken::new()).await });
    entered(&p).await;
    assert!(matches!(
        peer.enqueue(&session, "duplicate"),
        Err(RuntimeError::Busy)
    ));
    assert_eq!(peer.health().unwrap().running, 1);
    assert!(
        peer.cancel(&peer.task(&admitted.id).unwrap().cancel_target())
            .unwrap()
    );
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(rt.tasks(&session).unwrap()[0].state, TaskState::Cancelled);
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(p.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_recovery_requires_stopped_worker_before_releasing_its_slot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let p = provider();
    let rt = open(&dir, p.clone());
    let peer = open(&dir, p.clone());
    let id = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&id, "old executor").unwrap();
    let run_rt = rt.clone();
    let run_id = admitted.id.clone();
    let run = tokio::spawn(async move { run_rt.resume(&run_id, CancellationToken::new()).await });
    entered(&p).await;
    let old = rt.get(&id).unwrap().attempt.unwrap();
    let task = rt.task(&admitted.id).unwrap();
    assert!(
        peer.resume(&task.id, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(matches!(peer.recover(&id), Err(RuntimeError::Busy)));
    assert_eq!(peer.get(&id).unwrap().attempt, Some(old.clone()));
    assert_eq!(peer.task(&task.id).unwrap().state, TaskState::Running);
    assert_eq!(peer.health().unwrap().running, 1);
    assert!(peer.cancel(&task.cancel_target()).unwrap());
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let recovered = peer.recover(&id).unwrap();
    assert!(recovered.attempt.is_none());
    assert!(
        rt.store()
            .append_host_event(&old, "stale", serde_json::json!({}))
            .is_err()
    );
    assert_eq!(peer.task(&task.id).unwrap().state, TaskState::Cancelled);
    assert_eq!(peer.health().unwrap().running, 0);
}

/// Queue time expires only because a genuinely running Task holds the single
/// slot. A dormant accepted entry is not claimed, never blocks ready work and
/// can be cancelled deterministically while it holds accepted capacity.
#[tokio::test]
async fn queue_timeout_behind_an_active_slot_and_dormant_cancellation_release_capacity() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let p = provider();
    let mut config = settings();
    config.max_queue_time = Duration::from_millis(100);
    let rt = open_with(&dir, p.clone(), config);
    let occupant_session = rt.create(None).unwrap().id;
    let occupant = rt.enqueue(&occupant_session, "occupy slot").unwrap();
    let occupant_rt = rt.clone();
    let occupant_id = occupant.id.clone();
    let occupant_run = tokio::spawn(async move {
        occupant_rt
            .resume(&occupant_id, CancellationToken::new())
            .await
    });
    entered(&p).await;

    let dormant_session = rt.create(None).unwrap().id;
    let dormant = rt
        .enqueue(&dormant_session, "dormant accepted head")
        .unwrap();
    assert!(dormant.worker_id.is_none());
    assert_eq!(rt.health().unwrap().queued, 1);

    let next = rt.create(None).unwrap().id;
    assert!(matches!(
        rt.prompt(
            &next,
            "expires behind an active slot",
            CancellationToken::new()
        )
        .await,
        Err(RuntimeError::Budget("queue time"))
    ));
    assert_eq!(*p.calls.lock().unwrap(), vec!["occupy slot"]);
    assert_eq!(rt.task(&dormant.id).unwrap(), dormant);
    assert_eq!(rt.health().unwrap().queued, 1);
    assert_eq!(rt.health().unwrap().running, 1);

    assert!(rt.cancel(&dormant.cancel_target()).unwrap());
    assert_eq!(rt.task(&dormant.id).unwrap().state, TaskState::Cancelled);
    assert_eq!(rt.health().unwrap().queued, 0);

    p.release.add_permits(1);
    occupant_run.await.unwrap().unwrap();
    assert_eq!(rt.task(&occupant.id).unwrap().state, TaskState::Completed);

    // A new dormant entry still does not block a ready claim.
    let head_session = rt.create(None).unwrap().id;
    let head = rt.enqueue(&head_session, "dormant head").unwrap();
    let free = rt.create(None).unwrap().id;
    let free_rt = rt.clone();
    let run = tokio::spawn(async move {
        free_rt
            .prompt(
                &free,
                "runs while a dormant entry waits",
                CancellationToken::new(),
            )
            .await
    });
    entered(&p).await;
    p.release.add_permits(1);
    run.await.unwrap().unwrap();
    assert_eq!(rt.task(&head.id).unwrap(), head);
    assert_eq!(rt.health().unwrap().queued, 1);
    assert!(rt.cancel(&head.cancel_target()).unwrap());
    assert_eq!(rt.task(&head.id).unwrap().state, TaskState::Cancelled);
    assert_eq!(rt.health().unwrap().queued, 0);
    assert_eq!(rt.health().unwrap().running, 0);
}

/// Recovery must not create or terminate Request authority for an accepted
/// queued task that has no Core attempt. Its authority begins only when the
/// explicit resume actually starts the original work.
#[tokio::test]
async fn recover_does_not_terminate_a_queued_task_in_the_request_authority() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let p = provider();
    let rt = open_with(&dir, p.clone(), request_settings());
    let session = rt.create(None).unwrap().id;
    let accepted = rt
        .enqueue(&session, "accepted queued request task")
        .unwrap();
    assert!(matches!(
        rt.request_task_state(&accepted.id),
        Err(RuntimeError::InvalidInput(_))
    ));

    rt.recover(&session).unwrap();
    assert_eq!(rt.task(&accepted.id).unwrap(), accepted);
    assert!(matches!(
        rt.request_task_state(&accepted.id),
        Err(RuntimeError::InvalidInput(_))
    ));

    let run_rt = rt.clone();
    let run_id = accepted.id.clone();
    let run = tokio::spawn(async move { run_rt.resume(&run_id, CancellationToken::new()).await });
    entered(&p).await;
    p.release.add_permits(1);
    run.await.unwrap().unwrap();
    assert_eq!(rt.task(&accepted.id).unwrap().state, TaskState::Completed);
    assert!(matches!(
        rt.request_task_state(&accepted.id).unwrap(),
        cayw_requests::TaskState::Ended {
            outcome: cayw_requests::TaskOutcome::Completed,
            ..
        }
    ));
    assert_eq!(rt.tasks(&session).unwrap().len(), 1);
}
