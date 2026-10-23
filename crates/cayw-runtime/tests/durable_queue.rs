use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, TaskState};
use std::{
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
fn settings() -> RuntimeConfig {
    RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 4,
        max_run_time: Duration::from_secs(10),
        ..Default::default()
    }
}
fn open(dir: &tempfile::TempDir, p: Arc<dyn Provider>) -> Runtime {
    Runtime::open(
        dir.path().join("queue.db"),
        dir.path().join("workspace"),
        p,
        Model::new("fixture", "queue"),
        settings(),
    )
    .unwrap()
}
fn provider() -> Arc<Controlled> {
    Arc::new(Controlled {
        calls: Arc::default(),
        entered: Arc::default(),
        release: Arc::new(Semaphore::new(0)),
    })
}
async fn entered(p: &Controlled) {
    tokio::time::timeout(Duration::from_secs(5), p.entered.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn reopening_never_runs_tasks_and_explicit_resume_preserves_identity_and_fifo() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let p = provider();
    let rt = open(&dir, p.clone());
    let a = rt.create(None).unwrap().id;
    let b = rt.create(None).unwrap().id;
    let first = rt.enqueue(&a, "first persisted input").unwrap();
    let second = rt.enqueue(&b, "second persisted input").unwrap();
    assert!(first.worker_id.is_none());
    assert!(first.attempt_id.is_none());
    drop(rt);
    let rt = open(&dir, p.clone());
    let peer = open(&dir, p.clone());
    assert!(p.calls.lock().unwrap().is_empty());
    assert_eq!(rt.task(&first.id).unwrap(), first);
    assert_eq!(peer.health().unwrap().queued, 2);
    let b_rt = peer.clone();
    let b_task = second.id.clone();
    let later = tokio::spawn(async move { b_rt.resume(&b_task, CancellationToken::new()).await });
    tokio::task::yield_now().await;
    assert!(p.calls.lock().unwrap().is_empty());
    let a_rt = rt.clone();
    let a_task = first.id.clone();
    let earlier = tokio::spawn(async move { a_rt.resume(&a_task, CancellationToken::new()).await });
    entered(&p).await;
    assert_eq!(*p.calls.lock().unwrap(), vec!["first persisted input"]);
    let running = rt.task(&first.id).unwrap();
    assert_ne!(running.id, running.worker_id.clone().unwrap());
    assert_ne!(running.id, running.attempt_id.clone().unwrap());
    assert_ne!(running.worker_id, running.attempt_id);
    p.release.add_permits(1);
    earlier.await.unwrap().unwrap();
    entered(&p).await;
    p.release.add_permits(1);
    later.await.unwrap().unwrap();
    assert_eq!(
        *p.calls.lock().unwrap(),
        vec!["first persisted input", "second persisted input"]
    );
    assert_eq!(rt.task(&first.id).unwrap().state, TaskState::Completed);
    assert_eq!(rt.task(&second.id).unwrap().state, TaskState::Completed);
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

#[tokio::test]
async fn queued_cancellation_and_queue_timeout_release_capacity_without_model_calls() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let p = provider();
    let mut config = settings();
    config.max_queue_time = Duration::from_millis(100);
    let rt = Runtime::open(
        dir.path().join("queue.db"),
        dir.path().join("workspace"),
        p.clone(),
        Model::new("fixture", "queue"),
        config,
    )
    .unwrap();
    let first = rt.create(None).unwrap().id;
    let next = rt.create(None).unwrap().id;
    let blocked = rt.enqueue(&first, "dormant persisted head").unwrap();
    assert!(matches!(
        rt.prompt(&next, "expires behind head", CancellationToken::new())
            .await,
        Err(RuntimeError::Budget("queue time"))
    ));
    assert!(p.calls.lock().unwrap().is_empty());
    assert_eq!(rt.health().unwrap().queued, 1);
    assert!(rt.cancel(&blocked.cancel_target()).unwrap());
    assert_eq!(rt.task(&blocked.id).unwrap().state, TaskState::Cancelled);
    assert_eq!(rt.health().unwrap().queued, 0);
}
