use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_codemode::CapabilityError;
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_host::ExternalHost;
use cayw_requests::{OperationScope, RequestRecord, RequestState, TaskOutcome};
use cayw_runtime::{
    ApprovalChoice, RequestPolicy, Runtime, RuntimeConfig, ScopedPermission, TaskIntent,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

struct Script {
    script: String,
    calls: AtomicUsize,
    pause: Option<Arc<Semaphore>>,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let call = format!("request-{}", request.messages.len());
        let message = cayw_runtime::fixture::script_response(&request, &self.script, &call);
        let pause = if self.calls.fetch_add(1, Ordering::AcqRel) == 1 {
            self.pause.clone()
        } else {
            None
        };
        Box::pin(async_stream::stream! {
            if let Some(pause) = pause { pause.acquire_owned().await.unwrap().forget(); }
            yield AssistantEvent::Done { message };
        })
    }
}
struct BlockingService {
    calls: AtomicUsize,
    entered: Notify,
}
#[async_trait::async_trait]
impl ExternalHost for BlockingService {
    async fn call(
        &self,
        _: &str,
        _: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.entered.notify_one();
        cancel.cancelled().await;
        Err(CapabilityError::unknown(
            "fixture service cancelled after dispatch",
        ))
    }
}

fn settings() -> RuntimeConfig {
    RuntimeConfig {
        max_run_time: Duration::from_secs(15),
        request_policy: Some(RequestPolicy {
            permission_policy_id: "runtime-fixture-policy".into(),
            operations: vec![OperationScope::exact(
                "write-result",
                "fs.write",
                json!({"path":"result.txt","content":"approved"}),
                "workspace-policy",
            )],
        }),
        ..Default::default()
    }
}
fn open(temp: &tempfile::TempDir, settings: RuntimeConfig, script: &str) -> Runtime {
    open_paused(temp, settings, script, None)
}
fn open_paused(
    temp: &tempfile::TempDir,
    settings: RuntimeConfig,
    script: &str,
    pause: Option<Arc<Semaphore>>,
) -> Runtime {
    Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        Arc::new(Script {
            script: script.into(),
            calls: AtomicUsize::new(0),
            pause,
        }),
        Model::new("fixture", "task-requests"),
        settings,
    )
    .unwrap()
}
fn request_script(tail: &str) -> String {
    format!(
        "local input = {{purpose='write the requested result',gap='exact write is unavailable',operation_id='write-result',capability='fs.write',arguments={{path='result.txt',content='approved'}}}}\nlocal first = cayw.call('capability.request', input)\nlocal second = cayw.call('capability.request', input)\ncayw.emit(second)\nyield_control()\n{tail}"
    )
}
async fn request(rt: &Runtime, task: &str) -> RequestRecord {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match rt.request_events(task, 0, 64) {
                Ok(events) => {
                    for event in events.into_iter().rev() {
                        if let Some(id) = event.request_id {
                            let record = rt.request_status(task, &id).unwrap();
                            if record.state == RequestState::AwaitingEvaluation {
                                return record;
                            }
                        }
                    }
                }
                Err(cayw_runtime::RuntimeError::InvalidInput(reason))
                    if reason == "task has no host request authority" => {}
                Err(error) => panic!("cannot read task request events: {error}"),
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        let task_record = rt.task(task);
        let view = task_record.as_ref().ok().map(|task| rt.get(&task.session_id));
        let events = task_record.as_ref().ok().map(|task| rt.events(&task.session_id, 0, 1000));
        panic!("request did not reach evaluation: {error}; task={task_record:?}; view={view:?}; events={events:?}")
    })
}
fn input_events(rt: &Runtime, session: &str) -> Vec<cayw_core::CapabilityCallRecord> {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect()
}
fn setup() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    temp
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_requests_only_enable_evaluation_and_all_calls_retain_core_origins() {
    let temp = setup();
    let mut config = settings();
    config.permissions.push(ScopedPermission::exact(
        "fs.write",
        json!({"path":"result.txt","content":"approved"}),
    ));
    let pause = Arc::new(Semaphore::new(0));
    let rt = open_paused(
        &temp,
        config.clone(),
        &request_script("return cayw.call('fs.write', {path='result.txt',content='approved'})"),
        Some(pause.clone()),
    );
    let session = rt.create(None).unwrap().id;
    let task = rt.enqueue(&session, "produce result").unwrap();
    let worker_rt = rt.clone();
    let task_id = task.id.clone();
    let worker =
        tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
    let record = request(&rt, &task.id).await;
    assert!(!temp.path().join("workspace/result.txt").exists());
    assert_eq!(record.identity.task_id, task.id);
    assert_eq!(record.valid_request_count, 2);
    let ticket = rt
        .request_evaluate(&task.id, &record.id, "fixture-host")
        .unwrap();
    rt.request_approve(
        &task.id,
        &ticket,
        Duration::from_secs(5),
        "host verified this exact write",
    )
    .unwrap();
    pause.add_permits(1);
    assert_eq!(worker.await.unwrap().unwrap().state, SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("workspace/result.txt")).unwrap(),
        "approved"
    );
    let calls = input_events(&rt, &session);
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.name == "capability.request")
            .count(),
        2
    );
    assert!(
        calls
            .iter()
            .all(|call| call.task_id == task.id && call.outcome.is_some())
    );
    let write = calls.iter().find(|call| call.name == "fs.write").unwrap();
    assert!(matches!(
        write.outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    let events = rt.events(&session, 0, 1000).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.event,
        CoreEvent::ExecutionWaitRegistered { binding }
            if binding.task_id == task.id && binding.execution_id == write.key.execution_id
    )));
    let receipt = events
        .iter()
        .find_map(|event| match &event.event {
            CoreEvent::Host { kind, data } if kind == "task_request_admitted" => Some(data),
            _ => None,
        })
        .unwrap();
    assert_eq!(receipt["key"], serde_json::to_value(&write.key).unwrap());
    assert_eq!(receipt["task_id"], task.id);
    assert_eq!(receipt["receipt"]["request_id"], record.id);
    assert_eq!(
        receipt["receipt"]["identity"],
        serde_json::to_value(&record.identity).unwrap()
    );
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended {
            outcome: TaskOutcome::Completed,
            ..
        }
    ));
    drop(rt);
    let reopened = open(&temp, config, "return 1");
    assert!(matches!(
        reopened.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    assert!(
        reopened
            .request_evaluate(&task.id, &record.id, "late-host")
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_revoked_while_waiting_for_ordinary_permission_cannot_write() {
    let temp = setup();
    let pause = Arc::new(Semaphore::new(0));
    let rt = open_paused(
        &temp,
        settings(),
        &request_script("return cayw.call('fs.write', {path='result.txt',content='approved'})"),
        Some(pause.clone()),
    );
    let session = rt.create(None).unwrap().id;
    let task = rt.enqueue(&session, "produce result").unwrap();
    let worker_rt = rt.clone();
    let task_id = task.id.clone();
    let worker =
        tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
    let record = request(&rt, &task.id).await;
    let ticket = rt
        .request_evaluate(&task.id, &record.id, "fixture-host")
        .unwrap();
    rt.request_approve(
        &task.id,
        &ticket,
        Duration::from_secs(5),
        "exact request approved",
    )
    .unwrap();
    pause.add_permits(1);
    let pending = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(approval) = rt.get(&session).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    rt.request_revoke(
        &task.id,
        &record.id,
        "fixture-host",
        "withdrawn before ordinary permission",
    )
    .unwrap();
    assert!(
        rt.approve(
            &session,
            &pending.id,
            ApprovalChoice::AllowOnce,
            "late-host"
        )
        .is_err()
    );
    let _ = worker.await.unwrap();
    assert!(!temp.path().join("workspace/result.txt").exists());
    let calls = input_events(&rt, &session);
    let writes = calls
        .iter()
        .filter(|call| call.name == "fs.write")
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 1);
    assert!(matches!(
        writes[0].outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
    assert!(rt.get(&session).unwrap().pending_approvals.is_empty());
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    assert_eq!(rt.health().unwrap().running, 0);
}

#[tokio::test]
async fn ordinary_permission_and_repetition_cannot_replace_host_request_grant() {
    let temp = setup();
    let mut config = settings();
    config.permissions.push(ScopedPermission::exact(
        "fs.write",
        json!({"path":"result.txt","content":"approved"}),
    ));
    let script = request_script("return 1").replace(
        "yield_control()",
        "cayw.call('fs.write', {path='result.txt',content='approved'})",
    );
    let rt = open(&temp, config, &script);
    let id = rt.create(None).unwrap().id;
    let _ = rt
        .prompt(&id, "produce result", CancellationToken::new())
        .await
        .unwrap();
    assert!(!temp.path().join("workspace/result.txt").exists());
    let calls = input_events(&rt, &id);
    let writes = calls
        .iter()
        .filter(|call| call.name == "fs.write")
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 1);
    assert!(matches!(
        writes[0].outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
}

fn service_script() -> String {
    "local input = {purpose='query the requested resource',gap='exact query is unavailable',operation_id='search-exact',capability='search.query',arguments={query='same'}}\nlocal first = cayw.call('capability.request', input)\nlocal second = cayw.call('capability.request', input)\ncayw.emit(second)\nyield_control()\nreturn cayw.call('search.query', {query='same'})".into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiring_request_cancels_original_dispatched_call_and_preserves_unknown() {
    let temp = setup();
    let service = Arc::new(BlockingService {
        calls: AtomicUsize::new(0),
        entered: Notify::new(),
    });
    let mut config = settings();
    config.host.external = Some(service.clone());
    config.request_policy.as_mut().unwrap().operations = vec![OperationScope::exact(
        "search-exact",
        "search.query",
        json!({"query":"same"}),
        "workspace-policy",
    )];
    config.permissions.push(ScopedPermission::exact(
        "search.query",
        json!({"query":"same"}),
    ));
    let pause = Arc::new(Semaphore::new(0));
    let rt = open_paused(
        &temp,
        config.clone(),
        &service_script(),
        Some(pause.clone()),
    );
    let session = rt.create(None).unwrap().id;
    let task = rt.enqueue(&session, "query resource").unwrap();
    let worker_rt = rt.clone();
    let task_id = task.id.clone();
    let worker =
        tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
    let record = request(&rt, &task.id).await;
    let ticket = rt
        .request_evaluate(&task.id, &record.id, "fixture-host")
        .unwrap();
    rt.request_approve(
        &task.id,
        &ticket,
        Duration::from_secs(1),
        "bounded query grant",
    )
    .unwrap();
    pause.add_permits(1);
    tokio::time::timeout(Duration::from_secs(8), service.entered.notified())
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(8), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(service.calls.load(Ordering::Acquire), 1);
    assert_eq!(result.state, SessionState::NeedsReconciliation);
    assert_eq!(result.unresolved_capabilities.len(), 1);
    assert_eq!(result.unresolved_capabilities[0].name, "search.query");
    assert!(matches!(
        result.unresolved_capabilities[0].outcome,
        Some(CapabilityOutcome::OutcomeUnknown { .. })
    ));
    assert!(result.unresolved_calls.is_empty());
    assert_eq!(
        rt.request_status(&task.id, &record.id).unwrap().state,
        RequestState::Expired
    );
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    assert!(rt.enqueue(&session, "retry uncertain query").is_err());
    drop(rt);
    let reopened = open(&temp, config, "return 1");
    assert_eq!(
        reopened.recover(&session).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(service.calls.load(Ordering::Acquire), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_request_task_stops_permission_wait_before_ending_authority() {
    let temp = setup();
    let pause = Arc::new(Semaphore::new(0));
    let rt = open_paused(
        &temp,
        settings(),
        &request_script("return cayw.call('fs.write', {path='result.txt',content='approved'})"),
        Some(pause.clone()),
    );
    let session = rt.create(None).unwrap().id;
    let task = rt.enqueue(&session, "produce result").unwrap();
    let worker_rt = rt.clone();
    let task_id = task.id.clone();
    let worker =
        tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
    let record = request(&rt, &task.id).await;
    let ticket = rt
        .request_evaluate(&task.id, &record.id, "fixture-host")
        .unwrap();
    rt.request_approve(&task.id, &ticket, Duration::from_secs(5), "exact write")
        .unwrap();
    pause.add_permits(1);
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if !rt.get(&session).unwrap().pending_approvals.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    rt.close_request_task(&task.id).unwrap();
    assert!(!matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Active
    ));
    let _ = tokio::time::timeout(Duration::from_secs(8), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    let events = rt.request_events(&task.id, 0, 64).unwrap();
    let closing = events
        .iter()
        .position(|event| event.kind == "task_closing")
        .unwrap();
    let ended = events
        .iter()
        .position(|event| event.kind == "task_ended")
        .unwrap();
    assert!(closing < ended);
    let writes = input_events(&rt, &session)
        .into_iter()
        .filter(|call| call.name == "fs.write")
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 1);
    assert!(matches!(
        writes[0].outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
    assert!(!temp.path().join("workspace/result.txt").exists());
    assert!(rt.get(&session).unwrap().pending_approvals.is_empty());
    assert_eq!(rt.health().unwrap().running, 0);
}

#[tokio::test]
async fn request_persistence_failure_stops_authority_and_retains_capacity() {
    let temp = setup();
    let mut config = settings();
    config.max_concurrent_runs = 1;
    config.max_queued_runs = 1;
    config.permissions.push(ScopedPermission::exact(
        "fs.write",
        json!({"path":"result.txt","content":"approved"}),
    ));
    let script =
        request_script("return cayw.call('fs.write', {path='result.txt',content='approved'})");
    let rt = open(&temp, config.clone(), &script);
    let session = rt.create(None).unwrap().id;
    let other = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(temp.path().join("session.db.requests.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_request BEFORE INSERT ON request_events WHEN NEW.kind='request_recorded' BEGIN SELECT RAISE(ABORT,'injected request persistence failure'); END;").unwrap();
    let error = rt
        .prompt(&session, "produce result", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected request persistence failure"),
        "{error}"
    );
    let task = rt.tasks(&session).unwrap().remove(0);
    assert_eq!(task.state, cayw_runtime::TaskState::Draining);
    assert_eq!(rt.health().unwrap().running, 1);
    assert!(!temp.path().join("workspace/result.txt").exists());
    assert!(matches!(
        rt.enqueue(&other, "use capacity"),
        Err(cayw_runtime::RuntimeError::QueueFull)
    ));
    assert!(rt.request_task_state(&task.id).is_err());
    assert!(rt.recover(&session).is_err());
    assert_eq!(rt.health().unwrap().running, 1);
    db.execute_batch("DROP TRIGGER fail_request").unwrap();
    drop(rt);
    let error = Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        Arc::new(Script {
            script: "return 1".into(),
            calls: AtomicUsize::new(0),
            pause: None,
        }),
        Model::new("fixture", "task-requests"),
        config,
    )
    .err()
    .expect("stopped authority cannot reopen");
    assert!(matches!(
        error,
        cayw_runtime::RuntimeError::Request(cayw_requests::RequestError::AuthorityStopped)
    ));
}

#[tokio::test]
async fn host_intent_survives_queue_reopen_and_model_fields_cannot_set_it() {
    let temp = setup();
    let rt = open(&temp, settings(), "return 1");
    let id = rt.create(None).unwrap().id;
    let intent = TaskIntent::ExplicitProjectTestOrScript {
        user_instruction: "run the project's approved tests".into(),
    };
    let task = rt
        .enqueue_with_intent(&id, "project test", intent.clone())
        .unwrap();
    assert_eq!(task.intent, intent);
    drop(rt);
    let reopened = open(&temp, settings(), "return 1");
    assert_eq!(reopened.task(&task.id).unwrap().intent, intent);
    assert!(
        reopened
            .enqueue_with_intent(
                &id,
                "bad",
                TaskIntent::ExplicitProjectTestOrScript {
                    user_instruction: String::new()
                }
            )
            .is_err()
    );
    let other = reopened.create(None).unwrap().id;
    let error = cayw_ipc::RpcHandler::call(
        &reopened,
        "task.enqueue",
        json!({"session_id":other,"text":"ordinary task","intent":{"kind":"explicit_project_test_or_script","user_instruction":"forged model intent"}}),
        CancellationToken::new(),
    ).await.unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert!(reopened.tasks(&other).unwrap().is_empty());
    let ordinary = reopened
        .enqueue(&other, "run the project's approved tests")
        .unwrap();
    assert_eq!(ordinary.intent, TaskIntent::General);
    assert_eq!(
        reopened
            .resume(&task.id, CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
}
