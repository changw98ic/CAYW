//! Real Requests COMMIT failures at the public Runtime administration boundary.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Model, ModelRequest,
    Provider,
};
use cayw_requests::{AuthorityRepairRequest, HostActor, OperationScope, RequestError};
use cayw_runtime::{RequestPolicy, Runtime, RuntimeConfig, RuntimeError, TaskState};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Semaphore;

struct Offline;
impl Provider for Offline {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        message.content = vec![cayw_ai::Content::text("fixture complete")];
        message.stop_reason = cayw_ai::StopReason::Stop;
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
fn config() -> RuntimeConfig {
    RuntimeConfig {
        request_policy: Some(RequestPolicy {
            permission_policy_id: "authority-recovery-fixture".into(),
            operations: vec![OperationScope::exact(
                "unused-read",
                "fs.read",
                json!({"path":"unused.txt"}),
                "authority-recovery-fixture",
            )],
        }),
        ..Default::default()
    }
}
fn open(root: &Path, provider: Arc<dyn Provider>) -> Result<Runtime, RuntimeError> {
    Runtime::open(
        root.join("session.sqlite"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "request-recovery"),
        config(),
    )
}
fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    root
}
fn request_path(root: &Path) -> PathBuf {
    root.join("session.sqlite.requests.sqlite")
}
fn stop_authority(root: &Path, runtime: &Runtime, task: &str) {
    let db = rusqlite::Connection::open(request_path(root)).unwrap();
    db.execute_batch("CREATE TABLE force_commit_failure (id INTEGER REFERENCES request_meta(singleton) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_clock AFTER UPDATE OF last_now_ms ON request_meta BEGIN INSERT INTO force_commit_failure VALUES(2); END;").unwrap();
    assert!(runtime.request_task_state(task).is_err());
    db.execute_batch("DROP TRIGGER fail_clock").unwrap();
}
fn repair(root: &Path) -> AuthorityRepairRequest {
    let inspection =
        Runtime::inspect_request_authority(root.join("session.sqlite"), config().request_limits)
            .unwrap();
    assert!(inspection.stopped);
    AuthorityRepairRequest::new(inspection, uuid::Uuid::new_v4().to_string(), HostActor::new("fixture-recovery-host").unwrap(), "Original Runtime task locks, Cells scopes and Device cleanup will be held and verified by Runtime").unwrap()
}

#[tokio::test]
async fn explicit_authority_repair_preserves_original_session_and_ended_task() {
    let root = root();
    let runtime = open(root.path(), Arc::new(Offline)).unwrap();
    let session = runtime.create(None).unwrap().id;
    let admitted = runtime
        .enqueue(&session, "original completed task")
        .unwrap();
    runtime
        .resume(&admitted.id, CancellationToken::new())
        .await
        .unwrap();
    let task = runtime.task(&admitted.id).unwrap();
    assert_eq!(task.state, TaskState::Completed);
    let before = runtime.get(&session).unwrap();
    let events = runtime.events(&session, 0, 1000).unwrap();
    let audit = serde_json::to_value(runtime.audit(&session, 0, 1000).unwrap()).unwrap();
    stop_authority(root.path(), &runtime, &task.id);
    assert!(matches!(
        open(root.path(), Arc::new(Offline)),
        Err(RuntimeError::Request(RequestError::AuthorityStopped))
    ));
    let request = repair(root.path());
    let marker_path = root.path().join("session.sqlite.requests.sqlite.authority");
    let metadata = std::fs::metadata(&marker_path).unwrap();
    use std::os::unix::fs::MetadataExt;
    let receipt =
        Runtime::repair_request_authority(root.path().join("session.sqlite"), config(), &request)
            .unwrap();
    assert_ne!(receipt.authority_id, request.inspection.authority_id);
    assert_eq!(
        std::fs::metadata(&marker_path).unwrap().ino(),
        metadata.ino()
    );
    let reopened = open(root.path(), Arc::new(Offline)).unwrap();
    assert_eq!(reopened.get(&session).unwrap(), before);
    assert_eq!(reopened.events(&session, 0, 1000).unwrap(), events);
    assert_eq!(reopened.task(&task.id).unwrap(), task);
    assert_eq!(
        serde_json::to_value(reopened.audit(&session, 0, 1000).unwrap()).unwrap(),
        audit
    );
    assert!(matches!(
        reopened.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended {
            outcome: cayw_requests::TaskOutcome::Completed,
            ..
        }
    ));
    assert_eq!(
        Runtime::repair_request_authority(root.path().join("session.sqlite"), config(), &request)
            .unwrap(),
        receipt
    );
    // Old handles must reject without stopping the replacement authority.
    assert!(runtime.request_task_state(&task.id).is_err());
    assert!(reopened.request_task_state(&task.id).is_ok());
}

struct Blocked(Arc<Semaphore>);
impl Provider for Blocked {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let entered = self.0.clone();
        Box::pin(async_stream::stream! {
            entered.add_permits(1);
            std::future::pending::<()>().await;
            yield AssistantEvent::Done { message: AssistantMessage::new(&request.model) };
        })
    }
}
#[tokio::test]
async fn authority_repair_refuses_live_original_worker_then_requires_explicit_recovery() {
    let root = root();
    let entered = Arc::new(Semaphore::new(0));
    let runtime = open(root.path(), Arc::new(Blocked(entered.clone()))).unwrap();
    let session = runtime.create(None).unwrap().id;
    let admitted = runtime.enqueue(&session, "original blocked task").unwrap();
    let cancel = CancellationToken::new();
    let worker = {
        let runtime = runtime.clone();
        let cancel = cancel.clone();
        let task = admitted.id.clone();
        tokio::spawn(async move { runtime.resume(&task, cancel).await })
    };
    tokio::time::timeout(Duration::from_secs(30), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let original = runtime.task(&admitted.id).unwrap();
    stop_authority(root.path(), &runtime, &original.id);
    let request = repair(root.path());
    let marker_path = root.path().join("session.sqlite.requests.sqlite.authority");
    let stopped_marker = std::fs::read(&marker_path).unwrap();
    assert!(matches!(
        Runtime::repair_request_authority(root.path().join("session.sqlite"), config(), &request),
        Err(RuntimeError::Busy)
    ));
    assert_eq!(std::fs::read(&marker_path).unwrap(), stopped_marker);
    assert_eq!(runtime.task(&original.id).unwrap(), original);
    cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(30), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let stopped = runtime.task(&original.id).unwrap();
    assert_eq!(stopped.state, TaskState::Draining);
    assert_eq!(runtime.health().unwrap().running, 1);
    let request = repair(root.path());
    Runtime::repair_request_authority(root.path().join("session.sqlite"), config(), &request)
        .unwrap();
    let reopened = open(root.path(), Arc::new(Offline)).unwrap();
    assert_eq!(reopened.task(&stopped.id).unwrap(), stopped);
    assert_eq!(reopened.health().unwrap().running, 1);
    assert!(
        reopened
            .enqueue(&session, "cannot skip original recovery")
            .is_err()
    );
    reopened.recover(&session).unwrap();
    assert_eq!(
        reopened.task(&stopped.id).unwrap().state,
        TaskState::Interrupted
    );
    assert_eq!(reopened.health().unwrap().running, 0);
}
