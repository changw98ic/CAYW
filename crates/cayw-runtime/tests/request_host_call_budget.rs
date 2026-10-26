//! Request tool and granted project execution share one Host call budget.
//!
//! Linux-friendly mirror of the macOS request_processes budget test: with
//! max_calls=2, capability.request + the first test.run consume the budget so
//! a second test.run is NotExecuted. Guards the same invariant the F05 wiring
//! must not break.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_host::{ProcessIo, Profile, ProfileKind};
use cayw_requests::{OperationScope, RequestState};
use cayw_runtime::{
    RequestPolicy, Runtime, RuntimeConfig, RuntimeError, ScopedPermission, SessionView, TaskIntent,
    TaskState,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Semaphore, task::JoinHandle};

struct Script {
    script: String,
    calls: AtomicUsize,
    resume: Arc<Semaphore>,
}
impl Script {
    fn post_yield_turn_started(&self) -> bool {
        self.calls.load(Ordering::Acquire) >= 2
    }
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            &self.script,
            &format!("budget-share-{}", request.messages.len()),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_request_and_execution_share_the_existing_host_call_budget() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("project-input.txt"), "project test input\n").unwrap();
    let settings = RuntimeConfig {
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
                    args: vec!["project".into(), "complete".into()],
                    env: BTreeMap::from([("CAYW_REQUEST_PROJECT".into(), "exact-project".into())]),
                    timeout: Duration::from_secs(20),
                    max_output_bytes: 16 * 1024,
                    allow_filter: false,
                    io: ProcessIo::Pipes,
                },
            )]),
            max_calls: 2,
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
    };
    let resume = Arc::new(Semaphore::new(0));
    let tail = "local first=cayw.call('test.run',{profile='project-test'}); return cayw.call('test.run',{profile='project-test'})";
    let script = format!(
        "local request=cayw.call('capability.request',{{purpose='run the project test selected by the user',gap='the project process requires a task grant',operation_id='project-test',capability='test.run',arguments={{profile='project-test'}}}}); cayw.emit(request); yield_control(); {tail}"
    );
    let provider = Arc::new(Script {
        script,
        calls: AtomicUsize::new(0),
        resume: resume.clone(),
    });
    let rt = Runtime::open(
        temp.path().join("session.db"),
        &workspace,
        provider.clone(),
        Model::new("fixture", "task-project-process"),
        settings,
    )
    .unwrap();
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
    let worker: JoinHandle<Result<SessionView, RuntimeError>> = {
        let rt = rt.clone();
        let id = task.id.clone();
        tokio::spawn(async move { rt.resume(&id, CancellationToken::new()).await })
    };
    let record = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match rt.request_events(&task.id, 0, 64) {
                Ok(events) => {
                    for event in events.into_iter().rev() {
                        if let Some(id) = event.request_id {
                            let request = rt.request_status(&task.id, &id).unwrap();
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
    .unwrap();
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
    tokio::time::timeout(Duration::from_secs(15), async {
        while !provider.post_yield_turn_started() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("post-yield model turn did not start after approval");
    resume.add_permits(1);
    let terminal = tokio::time::timeout(Duration::from_secs(30), worker)
        .await
        .expect("worker timeout")
        .unwrap();
    assert!(
        matches!(
            &terminal,
            Err(RuntimeError::Worker(reason))
                if reason == "host call or total byte budget exhausted"
        ),
        "request tool and first test.run must share the existing Host call budget; got {terminal:?}"
    );
    let effects = fs::read_to_string(workspace.join("project-effects.log")).unwrap_or_default();
    assert_eq!(effects, "project test effect\n");
    assert_eq!(rt.get(&session).unwrap().state, SessionState::Idle);
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Failed);
    let settled: Vec<_> = rt
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "test.run" => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(settled.len(), 2);
    assert!(matches!(
        settled[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(
        matches!(&settled[1].outcome, Some(CapabilityOutcome::NotExecuted { reason }) if reason.contains("host call or total byte budget exhausted"))
    );
}
