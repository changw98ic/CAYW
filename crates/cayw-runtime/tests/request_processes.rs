#![cfg(target_os = "macos")]

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityCallRecord, CapabilityOutcome, CoreEvent, SessionState};
use cayw_host::{ProcessIdentity, ProcessIo, Profile, ProfileKind};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_requests::{OperationScope, RequestRecord, RequestState};
use cayw_runtime::{
    RequestPolicy, Runtime, RuntimeConfig, RuntimeError, ScopedPermission, SessionView, TaskIntent,
    TaskRecord, TaskState,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Semaphore, task::JoinHandle};

struct Project {
    root: PathBuf,
    _temporary: Option<tempfile::TempDir>,
}
impl Project {
    fn new(label: &str) -> Self {
        let temporary = match std::env::var_os("CAYW_REQUEST_PROCESS_EVIDENCE_DIR") {
            Some(parent) => {
                fs::create_dir_all(&parent).unwrap();
                tempfile::Builder::new()
                    .prefix(&format!("{label}-"))
                    .tempdir_in(parent)
                    .unwrap()
            }
            None => tempfile::tempdir().unwrap(),
        };
        let (root, temporary) = if std::env::var_os("CAYW_REQUEST_PROCESS_EVIDENCE_DIR").is_some() {
            (temporary.keep(), None)
        } else {
            (temporary.path().to_owned(), Some(temporary))
        };
        fs::create_dir(root.join("workspace")).unwrap();
        fs::write(
            root.join("workspace/project-input.txt"),
            "project test input\n",
        )
        .unwrap();
        Self {
            root,
            _temporary: temporary,
        }
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn observation(&self, label: &str, value: Value) {
        let path = self.root.join(format!("{label}.json"));
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        eprintln!(
            "REQUEST_PROCESS_OBSERVATION {}",
            json!({"stage":label,"path":path,"observation":value})
        );
    }
    fn effects(&self) -> String {
        fs::read_to_string(self.workspace().join("project-effects.log")).unwrap_or_default()
    }
}

struct Script {
    script: String,
    calls: AtomicUsize,
    resume: Arc<Semaphore>,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            &self.script,
            &format!("project-request-{}", request.messages.len()),
        );
        let pause = (self.calls.fetch_add(1, Ordering::AcqRel) == 1).then(|| self.resume.clone());
        Box::pin(async_stream::stream! {
            if let Some(pause) = pause {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    permit = pause.acquire_owned() => permit.unwrap().forget(),
                }
            }
            yield AssistantEvent::Done { message };
        })
    }
}

fn config(mode: &str) -> RuntimeConfig {
    RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 1,
        max_run_time: Duration::from_secs(45),
        codemode: cayw_codemode::Limits {
            timeout: Duration::from_secs(30),
            ..Default::default()
        },
        host: cayw_host::HostConfig {
            call_timeout: Duration::from_secs(30),
            process_timeout: Duration::from_secs(20),
            profiles: BTreeMap::from([(
                "project-test".into(),
                Profile {
                    kind: ProfileKind::Test,
                    program: env!("CARGO_BIN_EXE_cayw-runtime-process-fixture").into(),
                    args: vec!["project".into(), mode.into()],
                    env: BTreeMap::from([("CAYW_REQUEST_PROJECT".into(), "exact-project".into())]),
                    timeout: Duration::from_secs(20),
                    max_output_bytes: 16 * 1024,
                    allow_filter: false,
                    io: ProcessIo::Pipes,
                },
            )]),
            ..Default::default()
        },
        permissions: vec![ScopedPermission::exact(
            "test.run",
            json!({"profile":"project-test"}),
        )],
        request_policy: Some(RequestPolicy {
            permission_policy_id: "issue8-project-process".into(),
            operations: vec![OperationScope::exact(
                "project-test",
                "test.run",
                json!({"profile":"project-test"}),
                "exact-local-project",
            )],
        }),
        ..Default::default()
    }
}
fn script(tail: &str) -> String {
    format!(
        "local request=cayw.call('capability.request',{{purpose='run the project test selected by the user',gap='the project process requires a task grant',operation_id='project-test',capability='test.run',arguments={{profile='project-test'}}}}); cayw.emit(request); yield_control(); {tail}"
    )
}
fn open(project: &Project, config: RuntimeConfig, tail: &str, resume: Arc<Semaphore>) -> Runtime {
    Runtime::open(
        project.root.join("session.db"),
        project.workspace(),
        Arc::new(Script {
            script: script(tail),
            calls: AtomicUsize::new(0),
            resume,
        }),
        Model::new("fixture", "task-project-process"),
        config,
    )
    .unwrap()
}
fn start(
    rt: &Runtime,
) -> (
    String,
    TaskRecord,
    JoinHandle<Result<SessionView, RuntimeError>>,
) {
    let session = rt.create(None).unwrap().id;
    let task = rt
        .enqueue_with_intent(
            &session,
            "Run exactly the host-selected project test",
            TaskIntent::ExplicitProjectTestOrScript {
                user_instruction: "Run the project-test profile in this project".into(),
            },
        )
        .unwrap();
    let worker = {
        let rt = rt.clone();
        let id = task.id.clone();
        tokio::spawn(async move { rt.resume(&id, CancellationToken::new()).await })
    };
    (session, task, worker)
}
async fn request(rt: &Runtime, task: &str) -> RequestRecord {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match rt.request_events(task, 0, 64) {
                Ok(events) => {
                    for event in events.into_iter().rev() {
                        if let Some(id) = event.request_id {
                            let request = rt.request_status(task, &id).unwrap();
                            if request.state == RequestState::AwaitingEvaluation {
                                return request;
                            }
                        }
                    }
                }
                Err(RuntimeError::InvalidInput(reason))
                    if reason == "task has no host request authority" => {}
                Err(error) => panic!("cannot query original request: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
async fn approve(rt: &Runtime, project: &Project, task: &TaskRecord) -> RequestRecord {
    let record = request(rt, &task.id).await;
    assert_eq!(record.valid_request_count, 1);
    assert_eq!(record.identity.task_id, task.id);
    assert_eq!(record.identity.session_id, task.session_id);
    assert!(project.effects().is_empty());
    assert!(!project.workspace().join("project.pid").exists());
    assert!(calls(rt, &task.session_id, true).is_empty());
    let ticket = rt
        .request_evaluate(&task.id, &record.id, "project-test-host")
        .unwrap();
    let approved = rt
        .request_approve(
            &task.id,
            &ticket,
            Duration::from_secs(40),
            "The user requested this exact configured project test",
        )
        .unwrap();
    assert_eq!(approved.state, RequestState::Granted);
    assert!(project.effects().is_empty());
    assert!(calls(rt, &task.session_id, true).is_empty());
    project.observation(
        "approved-before-launch",
        json!({"task":rt.task(&task.id).unwrap(),"request":approved,
            "evaluation":{"request_id":ticket.request_id(),"actor":ticket.evaluator()}}),
    );
    approved
}
fn calls(rt: &Runtime, session: &str, started: bool) -> Vec<CapabilityCallRecord> {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilityStarted { call } if started && call.name == "test.run" => {
                Some(call)
            }
            CoreEvent::CapabilitySettled { call } if !started && call.name == "test.run" => {
                Some(call)
            }
            _ => None,
        })
        .collect()
}
fn process_rows(project: &Project) -> Vec<Value> {
    let connection = rusqlite::Connection::open(project.root.join("session.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT task,session,attempt,worker,fence,execution,sequence,
        pid,parent_pid,process_group,started_seconds,started_microseconds,state
        FROM runtime_process_identities ORDER BY pid",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok(json!({
                "task":row.get::<_, String>(0)?,"session":row.get::<_, String>(1)?,
                "attempt":row.get::<_, String>(2)?,"worker":row.get::<_, String>(3)?,
                "fence":row.get::<_, i64>(4)?,"execution":row.get::<_, String>(5)?,
                "sequence":row.get::<_, i64>(6)?,"pid":row.get::<_, i64>(7)?,
                "parent_pid":row.get::<_, i64>(8)?,"process_group":row.get::<_, i64>(9)?,
                "started_seconds":row.get::<_, i64>(10)?,
                "started_microseconds":row.get::<_, i64>(11)?,"state":row.get::<_, String>(12)?,
            }))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}
fn recorded_process(project: &Project, original: &ProcessIdentity, call: &CapabilityCallRecord) {
    let rows = process_rows(project);
    let row = rows
        .iter()
        .find(|row| row["pid"] == original.pid)
        .expect("original PID has no durable Runtime identity");
    assert_eq!(row["task"], call.task_id);
    assert_eq!(row["session"], call.lease.session_id);
    assert_eq!(row["attempt"], call.lease.attempt_id);
    assert_eq!(row["worker"], call.lease.worker_id);
    assert_eq!(row["fence"], call.lease.fence);
    assert_eq!(row["execution"], call.key.execution_id);
    assert_eq!(row["sequence"], call.key.sequence);
    assert_eq!(row["parent_pid"], original.parent_pid);
    assert_eq!(row["process_group"], original.process_group);
    assert_eq!(row["started_seconds"], original.started_seconds);
    assert_eq!(row["started_microseconds"], original.started_microseconds);
    assert_eq!(row["state"], "stopped");
}
fn snapshot(
    project: &Project,
    rt: &Runtime,
    task: &TaskRecord,
    request: &RequestRecord,
    label: &str,
) {
    project.observation(
        label,
        json!({
            "task":rt.task(&task.id).unwrap(),
            "view":rt.get(&task.session_id).unwrap(),
            "request":rt.request_status(&task.id,&request.id).unwrap(),
            "request_task":rt.request_task_state(&task.id).unwrap(),
            "request_events":rt.request_events(&task.id,0,64).unwrap(),
            "capability_calls":calls(rt,&task.session_id,false),
            "process_identities":process_rows(project),
            "audit":rt.audit(&task.session_id,0,1000).unwrap(),
            "effects":project.effects(),
            "project_result":fs::read_to_string(project.workspace().join("project-result.txt")).ok(),
            "process_observation":fs::read(project.workspace().join("project-observed.json")).ok().map(|bytes| serde_json::from_slice::<Value>(&bytes).unwrap()),
            "project_finished":project.workspace().join("project-finished").exists(),
            "running":rt.health().unwrap().running,
        }),
    );
}
async fn finished(
    worker: JoinHandle<Result<SessionView, RuntimeError>>,
) -> Result<SessionView, RuntimeError> {
    tokio::time::timeout(Duration::from_secs(30), worker)
        .await
        .expect("original process worker did not finish")
        .unwrap()
}
async fn time_limited(
    project: &Project,
    original: &ProcessIdentity,
    mut worker: JoinHandle<Result<SessionView, RuntimeError>>,
) -> SessionView {
    // The exercised limit is three seconds, while every fallback is at least
    // twenty. Ten seconds allows ordinary scheduling slack without accepting a
    // later fallback as proof that the selected boundary was enforced.
    let result = tokio::time::timeout(Duration::from_secs(10), &mut worker).await;
    let view = match result {
        Ok(result) => result.unwrap().unwrap(),
        Err(_) => {
            fs::write(
                project.workspace().join("release"),
                "finish the original process",
            )
            .unwrap();
            let terminal = finished(worker).await;
            stopped(original).await;
            panic!(
                "selected time limit did not stop the original process before its fallback; terminal={terminal:?}"
            );
        }
    };
    stopped(original).await;
    view
}
async fn running(project: &Project) -> ProcessIdentity {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(bytes) = fs::read(project.workspace().join("project.pid"))
                && let Ok(pid) = serde_json::from_slice::<u32>(&bytes)
            {
                assert_eq!(project.effects(), "project test effect\n");
                let identity = ProcessIdentity::observe(pid).unwrap();
                assert_eq!(identity.process_group, pid);
                return identity;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
async fn stopped(original: &ProcessIdentity) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match ProcessIdentity::observe(original.pid) {
                Err(error) if error.raw_os_error() == Some(3) => return,
                Ok(current)
                    if current.started_seconds != original.started_seconds
                        || current.started_microseconds != original.started_microseconds =>
                {
                    return;
                }
                Ok(_) => {}
                Err(error) => panic!("original process identity cannot be checked: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("original process incarnation remained alive");
}
fn unknown(rt: &Runtime, session: &str) -> CapabilityCallRecord {
    let records = calls(rt, session, false);
    assert_eq!(records.len(), 1);
    let call = records.into_iter().next().unwrap();
    assert!(
        matches!(call.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. })),
        "a started process was not settled as unknown: {call:?}"
    );
    call
}
fn assert_identity(project: &Project, original: &ProcessIdentity, label: &str) {
    project.observation(
        label,
        json!({"pid":original.pid,"parent_pid":original.parent_pid,
            "process_group":original.process_group,"started_seconds":original.started_seconds,
            "started_microseconds":original.started_microseconds,"effects":project.effects()}),
    );
}

// Removing ExplicitProjectTestOrScript admission or allowing an application to
// execute a request before its trusted decision breaks the first-request gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_trusted_project_request_runs_the_exact_real_test_after_approval() {
    let project = Project::new("project-allowed");
    let settings = config("complete");
    let resume = Arc::new(Semaphore::new(0));
    let rt = open(
        &project,
        settings.clone(),
        "return cayw.call('test.run',{profile='project-test'})",
        resume.clone(),
    );
    let (session, task, worker) = start(&rt);
    let record = approve(&rt, &project, &task).await;
    resume.add_permits(1);
    assert_eq!(finished(worker).await.unwrap().state, SessionState::Idle);
    assert_eq!(project.effects(), "project test effect\n");
    assert_eq!(
        fs::read_to_string(project.workspace().join("project-result.txt")).unwrap(),
        "project test passed\n"
    );
    assert!(project.workspace().join("project-finished").exists());
    let records = calls(&rt, &session, false);
    assert_eq!(records.len(), 1);
    assert!(
        matches!(&records[0].outcome, Some(CapabilityOutcome::Completed { result }) if result["success"] == true && result["stdout"].as_str().unwrap().contains("project-test-complete"))
    );
    assert_eq!(records[0].task_id, task.id);
    assert_eq!(records[0].lease.attempt_id, record.identity.attempt_id);
    let receipt = rt
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|event| match event.event {
            CoreEvent::Host { kind, data }
                if kind == "task_request_admitted" && data["name"] == "test.run" =>
            {
                Some(data)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(receipt["receipt"]["request_id"], record.id);
    assert_eq!(
        receipt["key"],
        serde_json::to_value(&records[0].key).unwrap()
    );
    assert_eq!(
        receipt["receipt"]["operation"]["arguments"],
        json!({"profile":"project-test"})
    );
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    snapshot(&project, &rt, &task, &record, "project-completed");
    drop(rt);
    let reopened = open(
        &project,
        settings,
        "return nil",
        Arc::new(Semaphore::new(1)),
    );
    assert!(
        reopened
            .request_evaluate(&task.id, &record.id, "late-host")
            .is_err()
    );
    assert_eq!(project.effects(), "project test effect\n");
    snapshot(&project, &reopened, &task, &record, "completed-reopened");
}

// Replacing the request's original cancellation target or omitting its admission
// check would let this already-approved but not yet started program run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_before_project_launch_never_starts_the_program() {
    let project = Project::new("project-revoke-before");
    let resume = Arc::new(Semaphore::new(0));
    let rt = open(
        &project,
        config("complete"),
        "return cayw.call('test.run',{profile='project-test'})",
        resume.clone(),
    );
    let (_, task, worker) = start(&rt);
    let record = approve(&rt, &project, &task).await;
    let revoked = rt
        .request_revoke(
            &task.id,
            &record.id,
            "project-test-host",
            "withdrawn before execution",
        )
        .unwrap();
    assert_eq!(revoked.state, RequestState::Revoked);
    resume.add_permits(1);
    let _ = finished(worker).await;
    assert!(project.effects().is_empty());
    assert!(!project.workspace().join("project.pid").exists());
    assert!(calls(&rt, &task.session_id, true).is_empty());
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    assert_eq!(rt.health().unwrap().running, 0);
    snapshot(&project, &rt, &task, &record, "revoked-before-launch");
}

// Settling cancellation as NotExecuted, clearing the request, or replaying after
// recovery would contradict this program's real effect and original OS identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_a_running_project_stops_its_original_process_and_preserves_effects() {
    let project = Project::new("project-revoke-running");
    let settings = config("wait");
    let resume = Arc::new(Semaphore::new(0));
    let rt = open(
        &project,
        settings.clone(),
        "return cayw.call('test.run',{profile='project-test'})",
        resume.clone(),
    );
    let (session, task, worker) = start(&rt);
    let record = approve(&rt, &project, &task).await;
    resume.add_permits(1);
    let original = running(&project).await;
    assert_identity(&project, &original, "running-before-revoke");
    assert_eq!(calls(&rt, &session, true).len(), 1);
    assert_eq!(
        rt.request_revoke(
            &task.id,
            &record.id,
            "project-test-host",
            "stop the running project test"
        )
        .unwrap()
        .state,
        RequestState::Revoked
    );
    let view = finished(worker).await.unwrap();
    stopped(&original).await;
    assert_eq!(view.state, SessionState::NeedsReconciliation);
    let call = unknown(&rt, &session);
    assert_eq!(call.task_id, task.id);
    assert_eq!(call.lease.attempt_id, record.identity.attempt_id);
    recorded_process(&project, &original, &call);
    assert_eq!(project.effects(), "project test effect\n");
    assert!(!project.workspace().join("project-finished").exists());
    assert_eq!(
        rt.request_status(&task.id, &record.id).unwrap().state,
        RequestState::Revoked
    );
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    assert_eq!(rt.health().unwrap().running, 0);
    assert!(
        rt.enqueue(&session, "do not replay the cancelled project")
            .is_err()
    );
    snapshot(&project, &rt, &task, &record, "revoked-running-settled");
    drop(rt);
    let reopened = open(
        &project,
        settings,
        "return nil",
        Arc::new(Semaphore::new(1)),
    );
    assert_eq!(
        reopened.recover(&session).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(
        reopened
            .request_evaluate(&task.id, &record.id, "late-host")
            .is_err()
    );
    assert!(reopened.enqueue(&session, "replay is forbidden").is_err());
    assert_eq!(unknown(&reopened, &session), call);
    assert_eq!(project.effects(), "project test effect\n");
    snapshot(
        &project,
        &reopened,
        &task,
        &record,
        "revoked-reopened-no-replay",
    );
}

// Bypassing the authenticated request.revoke dispatch or cancelling a different
// task would leave the real process running or its original effect replayable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_revoke_stops_the_already_started_original_project_process() {
    let project = Project::new("project-revoke-rpc");
    let resume = Arc::new(Semaphore::new(0));
    let rt = open(
        &project,
        config("wait"),
        "return cayw.call('test.run',{profile='project-test'})",
        resume.clone(),
    );
    let (session, task, worker) = start(&rt);
    let record = approve(&rt, &project, &task).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt.clone()),
        ServerConfig::new("issue8-project-revoke-public-fixture"),
        shutdown.clone(),
    ));
    let client = Client::connect(address, "issue8-project-revoke-public-fixture")
        .await
        .unwrap();
    resume.add_permits(1);
    let original = running(&project).await;
    assert_identity(&project, &original, "running-before-authenticated-revoke");
    let started = calls(&rt, &session, true);
    assert_eq!(started.len(), 1);
    let revoked = client
        .request(
            "request.revoke",
            json!({"task_id":task.id,"request_id":record.id,"reason":"withdraw while running"}),
        )
        .await
        .unwrap();
    let revoked: RequestRecord = serde_json::from_value(revoked).unwrap();
    assert_eq!(revoked.id, record.id);
    assert_eq!(revoked.identity, record.identity);
    assert_eq!(revoked.state, RequestState::Revoked);
    assert_eq!(revoked.decision.as_ref().unwrap().actor, "rpc-user");
    assert_eq!(
        finished(worker).await.unwrap().state,
        SessionState::NeedsReconciliation
    );
    stopped(&original).await;
    let call = unknown(&rt, &session);
    assert_eq!(call.key, started[0].key);
    assert_eq!(call.task_id, task.id);
    assert_eq!(call.lease, started[0].lease);
    recorded_process(&project, &original, &call);
    assert_eq!(project.effects(), "project test effect\n");
    assert!(!project.workspace().join("project-finished").exists());
    let status = client
        .request(
            "request.status",
            json!({"task_id":task.id,"request_id":record.id}),
        )
        .await
        .unwrap();
    assert_eq!(status["state"], "revoked");
    assert_eq!(rt.health().unwrap().running, 0);
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    let retry = client
        .request(
            "task.enqueue",
            json!({"session_id":session,"text":"do not replay the revoked process"}),
        )
        .await
        .unwrap_err();
    assert_eq!(retry.code, "needs_reconciliation_outcome_unknown");
    assert!(retry.outcome_unknown);
    let recovered = client
        .request("session.recover", json!({"session_id":session}))
        .await
        .unwrap();
    assert_eq!(recovered["state"], "needs_reconciliation");
    assert_eq!(unknown(&rt, &session), call);
    assert_eq!(project.effects(), "project test effect\n");
    snapshot(
        &project,
        &rt,
        &task,
        &record,
        "authenticated-revoke-settled-no-replay",
    );
    client.close().await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

// Releasing Queue capacity when the final processes_stopped audit write fails,
// or releasing it on drain alone, would fail the held-slot assertions below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_project_keeps_its_slot_until_cleanup_is_recorded_and_recovery_runs() {
    let project = Project::new("project-cleanup-record-failed");
    let resume = Arc::new(Semaphore::new(0));
    let rt = open(
        &project,
        config("wait"),
        "return cayw.call('test.run',{profile='project-test'})",
        resume.clone(),
    );
    let (session, task, worker) = start(&rt);
    let other = rt.create(None).unwrap().id;
    let record = approve(&rt, &project, &task).await;
    resume.add_permits(1);
    let original = running(&project).await;
    assert_identity(&project, &original, "running-before-cleanup-fault");
    let db = rusqlite::Connection::open(project.root.join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_request_process_terminal BEFORE INSERT ON runtime_audit WHEN NEW.kind='processes_stopped' BEGIN SELECT RAISE(ABORT, 'injected requested process cleanup record failure'); END;").unwrap();
    rt.request_revoke(
        &task.id,
        &record.id,
        "project-test-host",
        "revoke with unavailable cleanup record",
    )
    .unwrap();
    let error = finished(worker).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected requested process cleanup record failure"),
        "{error}"
    );
    assert!(matches!(
        error,
        RuntimeError::RecoveryRequired {
            outcome_unknown: true,
            ..
        }
    ));
    stopped(&original).await;
    assert_eq!(project.effects(), "project test effect\n");
    assert!(!project.workspace().join("project-finished").exists());
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Draining);
    let call = unknown(&rt, &session);
    recorded_process(&project, &original, &call);
    assert_eq!(
        rt.request_status(&task.id, &record.id).unwrap().state,
        RequestState::Revoked
    );
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Closing { .. }
    ));
    assert_eq!(rt.health().unwrap().running, 1);
    assert!(matches!(
        rt.enqueue(&other, "cannot reuse the original slot"),
        Err(RuntimeError::QueueFull)
    ));
    let before = rt.get(&session).unwrap();
    assert!(matches!(
        rt.recover(&session),
        Err(RuntimeError::RecoveryRequired { .. })
    ));
    assert_eq!(rt.get(&session).unwrap(), before);
    assert!(rt.drain_task_processes(&task.id).await.is_err());
    assert!(
        !rt.audit(&session, 0, 1000)
            .unwrap()
            .iter()
            .any(|event| event.kind == "processes_stopped")
    );
    snapshot(
        &project,
        &rt,
        &task,
        &record,
        "cleanup-record-refused-slot-held",
    );
    db.execute_batch("DROP TRIGGER fail_request_process_terminal")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match rt.drain_task_processes(&task.id).await {
                Err(RuntimeError::Busy) => tokio::task::yield_now().await,
                result => break result.unwrap(),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        rt.health().unwrap().running,
        1,
        "cleanup alone must not release the original execution slot"
    );
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Draining);
    let cleanup = rt
        .audit(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect::<Vec<_>>();
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0].task_id.as_ref(), Some(&task.id));
    assert_eq!(cleanup[0].data["state"], "drained");
    snapshot(
        &project,
        &rt,
        &task,
        &record,
        "cleanup-recorded-before-recovery",
    );
    let recovered = rt.recover(&session).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(rt.health().unwrap().running, 0);
    unknown(&rt, &session);
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    assert_eq!(project.effects(), "project test effect\n");
    assert!(
        rt.enqueue(&session, "do not replay the original effect")
            .is_err()
    );
    snapshot(
        &project,
        &rt,
        &task,
        &record,
        "explicit-recovery-after-cleanup",
    );
}

// Removing one selected time clamp cannot pass via a later fallback: the case
// must settle before that fallback, with an actual started PID and no completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requested_project_keeps_profile_host_process_and_call_time_limits() {
    for boundary in ["profile", "process", "call"] {
        let project = Project::new(&format!("project-time-{boundary}"));
        let mut settings = config("wait");
        match boundary {
            "profile" => {
                settings
                    .host
                    .profiles
                    .get_mut("project-test")
                    .unwrap()
                    .timeout = Duration::from_secs(3)
            }
            "process" => settings.host.process_timeout = Duration::from_secs(3),
            "call" => settings.host.call_timeout = Duration::from_secs(3),
            _ => unreachable!(),
        }
        let resume = Arc::new(Semaphore::new(0));
        let rt = open(
            &project,
            settings,
            "return cayw.call('test.run',{profile='project-test'})",
            resume.clone(),
        );
        let (session, task, worker) = start(&rt);
        let record = approve(&rt, &project, &task).await;
        resume.add_permits(1);
        let original = running(&project).await;
        assert_identity(&project, &original, "running-before-time-limit");
        let view = time_limited(&project, &original, worker).await;
        assert_eq!(view.state, SessionState::NeedsReconciliation);
        let call = unknown(&rt, &session);
        recorded_process(&project, &original, &call);
        assert!(
            matches!(&call.outcome, Some(CapabilityOutcome::OutcomeUnknown { error }) if error.contains("TimedOut")),
            "{call:?}"
        );
        assert_eq!(project.effects(), "project test effect\n");
        assert!(!project.workspace().join("project-finished").exists());
        assert_eq!(rt.health().unwrap().running, 0);
        snapshot(&project, &rt, &task, &record, boundary);
    }
}

// Ignoring either output clamp would allow the released fixture to emit its
// complete 8 KiB output and publish project-finished instead of OutputLimit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requested_project_keeps_profile_and_host_combined_output_limits() {
    for boundary in ["profile", "host"] {
        let project = Project::new(&format!("project-output-{boundary}"));
        let mut settings = config("flood");
        match boundary {
            "profile" => {
                settings
                    .host
                    .profiles
                    .get_mut("project-test")
                    .unwrap()
                    .max_output_bytes = 1024
            }
            "host" => settings.host.max_process_output_bytes = 1024,
            _ => unreachable!(),
        }
        let resume = Arc::new(Semaphore::new(0));
        let rt = open(
            &project,
            settings,
            "return cayw.call('test.run',{profile='project-test'})",
            resume.clone(),
        );
        let (session, task, worker) = start(&rt);
        let record = approve(&rt, &project, &task).await;
        resume.add_permits(1);
        let original = running(&project).await;
        assert_identity(&project, &original, "running-before-output-release");
        fs::write(project.workspace().join("output-release"), "emit now").unwrap();
        let view = finished(worker).await.unwrap();
        stopped(&original).await;
        assert_eq!(view.state, SessionState::NeedsReconciliation);
        let call = unknown(&rt, &session);
        recorded_process(&project, &original, &call);
        assert!(
            matches!(&call.outcome, Some(CapabilityOutcome::OutcomeUnknown { error }) if error.contains("OutputLimit") && error.contains("stdout_bytes=1024") && error.contains("stderr_bytes=0")),
            "{call:?}"
        );
        assert_eq!(project.effects(), "project test effect\n");
        assert!(!project.workspace().join("project-finished").exists());
        assert_eq!(rt.health().unwrap().running, 0);
        snapshot(&project, &rt, &task, &record, boundary);
    }
}

// A fresh Host budget for the post-yield call, or ignoring the request tool's
// own charge, would execute the project twice through the same approved scope.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_request_and_execution_share_the_existing_host_call_budget() {
    let project = Project::new("project-call-budget");
    let mut settings = config("complete");
    settings.host.max_calls = 2;
    let resume = Arc::new(Semaphore::new(0));
    let rt = open(
        &project,
        settings,
        "local first=cayw.call('test.run',{profile='project-test'}); return cayw.call('test.run',{profile='project-test'})",
        resume.clone(),
    );
    let (session, task, worker) = start(&rt);
    let record = approve(&rt, &project, &task).await;
    resume.add_permits(1);
    assert!(matches!(
        finished(worker).await,
        Err(RuntimeError::Worker(ref reason)) if reason == "host call or total byte budget exhausted"
    ));
    let view = rt.get(&session).unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_calls.is_empty());
    assert!(view.unresolved_capabilities.is_empty());
    let terminal_task = rt.task(&task.id).unwrap();
    assert_eq!(terminal_task.state, TaskState::Failed);
    assert!(!terminal_task.cancel_requested);
    let terminals = rt
        .audit(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.task_id.as_deref() == Some(task.id.as_str()) && event.kind.starts_with("run_")
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].kind, "run_failed");
    assert_eq!(project.effects(), "project test effect\n");
    assert_eq!(calls(&rt, &session, true).len(), 1);
    let settled = calls(&rt, &session, false);
    assert_eq!(settled.len(), 2);
    assert!(matches!(
        settled[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(
        matches!(&settled[1].outcome, Some(CapabilityOutcome::NotExecuted { reason }) if reason.contains("host call or total byte budget exhausted"))
    );
    assert_eq!(rt.health().unwrap().running, 0);
    snapshot(&project, &rt, &task, &record, "shared-host-call-budget");
}
