//! Trusted request limits must agree between live SDK adjustments and RPC decisions.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::SessionState;
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_requests::{OperationScope, RequestRecord, RequestState};
use cayw_runtime::{RequestPolicy, Runtime, RuntimeConfig, ScopedPermission, TaskRecord};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore};

const TOKEN: &str = "local-request-limit-adjustment-fixture";
const SCRIPT: &str = "local request=cayw.call('capability.request',{purpose='write the exact result',gap='the write needs host evaluation',operation_id='write-result',capability='fs.write',arguments={path='result.txt',content='approved'}}); cayw.emit(request); yield_control(); return cayw.call('fs.write',{path='result.txt',content='approved'})";

struct Script {
    pause: Arc<Semaphore>,
    calls: AtomicUsize,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            SCRIPT,
            &format!("request-limit-{}", request.messages.len()),
        );
        let pause = (self.calls.fetch_add(1, Ordering::AcqRel) == 1).then(|| self.pause.clone());
        Box::pin(async_stream::stream! {
            if let Some(pause) = pause { pause.acquire_owned().await.unwrap().forget(); }
            yield AssistantEvent::Done { message };
        })
    }
}

async fn awaiting(runtime: &Runtime, task: &str) -> RequestRecord {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(events) = runtime.request_events(task, 0, 64) {
                for event in events.into_iter().rev() {
                    if let Some(id) = event.request_id {
                        let request = runtime.request_status(task, &id).unwrap();
                        if matches!(
                            request.state,
                            RequestState::Requested | RequestState::AwaitingEvaluation
                        ) {
                            return request;
                        }
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

async fn decision_after_adjustment(initial: u64, adjusted: u64, requested: u64, allowed: bool) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut config = RuntimeConfig {
        max_run_time: Duration::from_secs(20),
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"result.txt","content":"approved"}),
        )],
        request_policy: Some(RequestPolicy {
            permission_policy_id: "request-limit-rpc".into(),
            operations: vec![OperationScope::exact(
                "write-result",
                "fs.write",
                json!({"path":"result.txt","content":"approved"}),
                "write-policy",
            )],
        }),
        ..Default::default()
    };
    config.request_limits.max_grant_lifetime_ms = initial;
    let pause = Arc::new(Semaphore::new(0));
    let runtime = Runtime::open(
        temp.path().join("session.db"),
        &workspace,
        Arc::new(Script {
            pause: pause.clone(),
            calls: AtomicUsize::new(0),
        }),
        Model::new("fixture", "request-limit-rpc"),
        config,
    )
    .unwrap();
    let session = runtime.create(None).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(runtime.clone()),
        ServerConfig::new(TOKEN),
        stop.clone(),
    ));
    let client = Client::connect(address, TOKEN).await.unwrap();
    let task: TaskRecord = serde_json::from_value(
        client
            .request(
                "task.enqueue",
                json!({"session_id":session.id,"text":"write the requested result"}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let resume_client = client.clone();
    let task_id = task.id.clone();
    let run = tokio::spawn(async move {
        resume_client
            .request("task.resume", json!({"task_id":task_id}))
            .await
    });
    let request = awaiting(&runtime, &task.id).await;
    assert_eq!(request.valid_request_count, 1);
    let mut limits = runtime.request_limits();
    limits.max_grant_lifetime_ms = adjusted;
    runtime
        .request_limits_adjust(
            limits,
            "trusted-operator",
            "adjust the maximum grant lifetime",
        )
        .unwrap();
    let result = client
        .request(
            "request.decide",
            json!({
                "task_id":task.id,"request_id":request.id,
                "decision":{"kind":"allow","lifetime_ms":requested},"reason":"trusted exact write"
            }),
        )
        .await;
    let assertion = if allowed {
        result
            .as_ref()
            .map(|value| value["state"] == "granted")
            .unwrap_or(false)
    } else {
        result
            .as_ref()
            .err()
            .is_some_and(|error| error.code == "invalid_params")
            && runtime.request_status(&task.id, &request.id).unwrap().state == request.state
    };
    // Release the fixed provider even on failure, so cleanup never depends on a
    // panic dropping an IPC request whose worker still owns its original task.
    if assertion && !allowed {
        client
            .request(
                "request.decide",
                json!({
                    "task_id":task.id,"request_id":request.id,
                    "decision":{"kind":"allow","lifetime_ms":adjusted.min(requested)},
                    "reason":"within the adjusted maximum"
                }),
            )
            .await
            .unwrap();
    } else if !assertion {
        runtime
            .cancel(&runtime.task(&task.id).unwrap().cancel_target())
            .unwrap();
    }
    pause.add_permits(1);
    let outcome = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
    client.close().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        assertion,
        "RPC decision did not follow the adjusted limits: {result:?}"
    );
    let outcome = outcome.unwrap();
    assert_eq!(outcome["state"], "idle");
    assert_eq!(runtime.get(&session.id).unwrap().state, SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(workspace.join("result.txt")).unwrap(),
        "approved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raised_grant_lifetime_is_used_by_the_live_rpc_decision() {
    decision_after_adjustment(1000, 2000, 1500, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lowered_grant_lifetime_is_rejected_before_claiming_evaluation() {
    decision_after_adjustment(2000, 1000, 1500, false).await;
}
