//! A trusted startup configuration changes durable request limits without losing facts.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_requests::{RequestConfig, RequestError};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, TaskState};
use std::{path::Path, sync::Arc};

struct Offline;
impl Provider for Offline {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "return 1",
            &format!("startup-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn open(root: &Path, limits: RequestConfig) -> Result<Runtime, RuntimeError> {
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(Offline),
        Model::new("fixture", "request-policy-startup"),
        RuntimeConfig {
            request_limits: limits,
            ..Default::default()
        },
    )
}
fn setup() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    temp
}
fn limits(max_tasks: u32) -> RequestConfig {
    RequestConfig {
        max_tasks,
        ..Default::default()
    }
}

#[tokio::test]
async fn configured_startup_adjustment_retains_history_and_completed_tasks() {
    let temp = setup();
    let original = limits(2);
    let runtime = open(temp.path(), original.clone()).unwrap();
    let session = runtime.create(None).unwrap();
    runtime
        .prompt(&session.id, "first fixed task", CancellationToken::new())
        .await
        .unwrap();
    let task = runtime.tasks(&session.id).unwrap().remove(0);
    assert_eq!(task.state, TaskState::Completed);
    let before = runtime.events(&session.id, 0, 1000).unwrap();
    drop(runtime);

    let adjusted = limits(3);
    let runtime = open(temp.path(), adjusted.clone()).unwrap();
    assert_eq!(runtime.request_limits(), adjusted);
    assert_eq!(runtime.task(&task.id).unwrap(), task);
    assert_eq!(runtime.events(&session.id, 0, 1000).unwrap(), before);
    let history = runtime.request_config_history().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].actor, "runtime-startup");
    assert_eq!(history[0].previous, original);
    assert_eq!(history[0].next, adjusted);
    runtime
        .prompt(&session.id, "second fixed task", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(runtime.tasks(&session.id).unwrap().len(), 2);
    drop(runtime);
    let runtime = open(temp.path(), adjusted).unwrap();
    assert_eq!(runtime.request_config_history().unwrap().len(), 1);
}

#[tokio::test]
async fn another_open_runtime_keeps_the_current_request_policy_usable() {
    let temp = setup();
    let runtime = open(temp.path(), limits(2)).unwrap();
    let matching = open(temp.path(), limits(2)).unwrap();
    drop(matching);
    assert!(open(temp.path(), limits(3)).is_err());
    assert_eq!(runtime.request_limits(), limits(2));
    assert!(runtime.request_config_history().unwrap().is_empty());
    let session = runtime.create(None).unwrap();
    runtime
        .prompt(
            &session.id,
            "original owner still works",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.tasks(&session.id).unwrap()[0].state,
        TaskState::Completed
    );
}

#[tokio::test]
async fn live_adjustment_cannot_invalidate_another_open_runtime() {
    let temp = setup();
    let runtime = open(temp.path(), limits(2)).unwrap();
    let peer = open(temp.path(), limits(2)).unwrap();
    let result = runtime.request_limits_adjust(
        limits(3),
        "trusted-operator",
        "raise operational allowance with a peer still open",
    );
    assert!(result.is_err(), "live adjustment replaced a peer's policy");
    assert_eq!(runtime.request_limits(), limits(2));
    assert_eq!(peer.request_limits(), limits(2));
    assert!(runtime.request_config_history().unwrap().is_empty());
    assert!(peer.request_config_history().unwrap().is_empty());
    let session = peer.create(None).unwrap();
    peer.prompt(&session.id, "peer still works", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        peer.tasks(&session.id).unwrap()[0].state,
        TaskState::Completed
    );
    drop(peer);
    runtime
        .request_limits_adjust(
            limits(3),
            "trusted-operator",
            "raise operational allowance after the peer closes",
        )
        .unwrap();
    assert_eq!(runtime.request_limits(), limits(3));
    assert_eq!(runtime.request_config_history().unwrap().len(), 1);
}

#[tokio::test]
async fn offline_adjustment_cannot_invalidate_an_open_runtime() {
    let temp = setup();
    let runtime = open(temp.path(), limits(2)).unwrap();
    let actor = cayw_requests::HostActor::new("trusted-operator").unwrap();
    let result = Runtime::adjust_request_limits(
        temp.path().join("sessions.sqlite"),
        limits(3),
        &actor,
        "raise operational allowance",
    );
    assert!(
        result.is_err(),
        "offline adjustment replaced a live request policy"
    );
    assert_eq!(runtime.request_limits(), limits(2));
    assert!(runtime.request_config_history().unwrap().is_empty());
    let session = runtime.create(None).unwrap();
    runtime
        .prompt(
            &session.id,
            "original owner still works",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    drop(runtime);
    assert_eq!(
        Runtime::adjust_request_limits(
            temp.path().join("sessions.sqlite"),
            limits(3),
            &actor,
            "raise after the original owner stops",
        )
        .unwrap(),
        limits(3)
    );
    let reopened = open(temp.path(), limits(3)).unwrap();
    assert_eq!(reopened.request_config_history().unwrap().len(), 1);
}

#[tokio::test]
async fn changed_startup_limits_cannot_reopen_a_stopped_request_authority() {
    let temp = setup();
    let runtime = open(temp.path(), limits(2)).unwrap();
    let session = runtime.create(None).unwrap();
    runtime
        .prompt(
            &session.id,
            "retain this completed task",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let task = runtime.tasks(&session.id).unwrap().remove(0);
    let database = temp.path().join("sessions.sqlite.requests.sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.execute_batch("CREATE TABLE force_commit_failure (id INTEGER REFERENCES request_meta(singleton) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_clock AFTER UPDATE OF last_now_ms ON request_meta BEGIN INSERT INTO force_commit_failure VALUES(2); END;").unwrap();
    assert!(runtime.request_task_state(&task.id).is_err());
    connection.execute_batch("DROP TRIGGER fail_clock").unwrap();
    drop(connection);
    drop(runtime);
    let marker = temp
        .path()
        .join("sessions.sqlite.requests.sqlite.authority");
    let stopped = std::fs::read(&marker).unwrap();
    assert!(matches!(
        open(temp.path(), limits(3)),
        Err(RuntimeError::Request(RequestError::AuthorityStopped))
    ));
    assert_eq!(std::fs::read(marker).unwrap(), stopped);
}
