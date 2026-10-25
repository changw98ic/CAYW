//! Persistent declared-resource admission through the SDK and authenticated IPC.
//! Model output is synthetic; the queue, database and transport are real.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus,
};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{
    HeadlessConfig, ResourcePolicy, Runtime, RuntimeConfig, TaskInput, TaskIntent, TaskPriority,
    TaskResources, TaskState,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

struct Gate {
    entered: Semaphore,
    release: Semaphore,
}
impl Gate {
    fn new() -> Self {
        Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
}
struct ProviderGate(Arc<Gate>);
impl Provider for ProviderGate {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let gate = self.0.clone();
        Box::pin(futures::stream::once(async move {
            gate.entered.add_permits(1);
            tokio::select! {
                _ = cancel.cancelled() => AssistantEvent::Error {
                    message: AssistantMessage::new(&request.model)
                        .failure(StopReason::Aborted, "resource test cancelled"),
                },
                permit = gate.release.acquire() => {
                    permit.unwrap().forget();
                    let mut message = AssistantMessage::new(&request.model);
                    message.content.push(Content::text("completed"));
                    message.stop_reason = StopReason::Stop;
                    message.usage = Usage { input:4, output:4, total_tokens:8, ..Default::default() };
                    message.usage_status = UsageStatus::Reported;
                    AssistantEvent::Done { message }
                }
            }
        }))
    }
}
fn model() -> Model {
    Model::new("fixture", "resource-gate")
}
fn input(text: &str) -> TaskInput {
    TaskInput {
        text: text.into(),
        images: vec![],
    }
}
fn resource(bytes: u64) -> TaskResources {
    TaskResources {
        declared_memory_bytes: bytes,
        ..Default::default()
    }
}
async fn entered(gate: &Gate) {
    tokio::time::timeout(Duration::from_secs(20), gate.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_and_authenticated_ipc_share_durable_admission_and_reject_hard_guarantees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("sessions.sqlite");
    let gate = Arc::new(Gate::new());
    let config = RuntimeConfig {
        resources: ResourcePolicy {
            max_declared_memory_bytes: 100,
        },
        max_concurrent_runs: 2,
        max_queued_runs: 4,
        ..Default::default()
    };
    let owner = Runtime::open(
        &db,
        &root,
        Arc::new(ProviderGate(gate.clone())),
        model(),
        config.clone(),
    )
    .unwrap();
    let peer = Runtime::open(
        &db,
        &root,
        Arc::new(ProviderGate(gate.clone())),
        model(),
        config.clone(),
    )
    .unwrap();
    let a = owner.create(Some("a".into())).unwrap().id;
    let b = owner.create(Some("b".into())).unwrap().id;
    let rejected = owner.create(Some("rejected".into())).unwrap().id;
    let first = owner
        .enqueue_with_resources(
            &a,
            input("a"),
            TaskIntent::General,
            TaskPriority::Normal,
            resource(80),
        )
        .unwrap();
    let runtime = owner.clone();
    let first_id = first.id.clone();
    let work_a =
        tokio::spawn(async move { runtime.resume(&first_id, CancellationToken::new()).await });
    entered(&gate).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(peer.clone()),
        ServerConfig::new("resource-test-token"),
        stop.clone(),
    ));
    let client = Client::connect(addr, "resource-test-token").await.unwrap();
    let view = client
        .request("runtime.resources", json!({}))
        .await
        .unwrap();
    assert_eq!(view["usage"]["reserved_declared_memory_bytes"], 80);
    assert_eq!(view["usage"]["holding_tasks"], 1);
    assert_eq!(
        view,
        serde_json::to_value(peer.resources().unwrap()).unwrap()
    );
    let second = client
        .request(
            "task.enqueue_with_resources",
            json!({
                "session_id":b,"text":"b","intent":{"kind":"general"},"priority":"normal",
                "resources":{"declared_memory_bytes":60}
            }),
        )
        .await
        .unwrap();
    let second_id = second["id"].as_str().unwrap().to_owned();
    let client_b = client.clone();
    let second_for_resume = second_id.clone();
    let work_b = tokio::spawn(async move {
        client_b
            .request("task.resume", json!({"task_id":second_for_resume}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let task = peer.task(&second_id).unwrap();
            if task.worker_id.is_some() && task.ready_order.is_some() {
                assert_eq!(task.state, TaskState::Queued);
                assert!(!task.holds_slot);
                assert_eq!(task.resources.declared_memory_bytes, 60);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        peer.resources()
            .unwrap()
            .usage
            .reserved_declared_memory_bytes,
        80
    );
    assert_eq!(gate.entered.available_permits(), 0);
    for resources in [
        json!({"declared_memory_bytes":101}),
        json!({"hard_memory_limit_bytes":1}),
        json!({"cpu_quota_percent":1}),
        json!({"gpu_units":1}),
        json!({"io_bandwidth_bytes_per_sec":1}),
    ] {
        let error = client
            .request(
                "task.enqueue_with_resources",
                json!({
                    "session_id":rejected,"text":"must not execute","intent":{"kind":"general"},
                    "priority":"normal","resources":resources
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_params");
        assert!(owner.tasks(&rejected).unwrap().is_empty());
    }
    let bad_sdk = TaskResources {
        hard_memory_limit_bytes: Some(1),
        ..Default::default()
    };
    assert!(
        owner
            .enqueue_with_resources(
                &rejected,
                input("must not execute"),
                TaskIntent::General,
                TaskPriority::Normal,
                bad_sdk
            )
            .is_err()
    );
    gate.release.add_permits(1);
    work_a.await.unwrap().unwrap();
    entered(&gate).await;
    assert_eq!(owner.task(&first.id).unwrap().state, TaskState::Completed);
    assert_eq!(
        owner
            .resources()
            .unwrap()
            .usage
            .reserved_declared_memory_bytes,
        60
    );
    gate.release.add_permits(1);
    work_b.await.unwrap().unwrap();
    assert_eq!(peer.task(&second_id).unwrap().state, TaskState::Completed);
    assert_eq!(
        peer.resources()
            .unwrap()
            .usage
            .reserved_declared_memory_bytes,
        0
    );
    let bad = RuntimeConfig {
        resources: ResourcePolicy {
            max_declared_memory_bytes: 101,
        },
        ..config
    };
    assert!(Runtime::open(&db, &root, Arc::new(ProviderGate(gate)), model(), bad).is_err());
    client.close().await;
    stop.cancel();
    server.await.unwrap().unwrap();
}

#[test]
fn headless_json_exposes_one_resource_policy_and_rejects_unsupported_configuration() {
    let config: HeadlessConfig = serde_json::from_value(json!({
        "resources":{"max_declared_memory_bytes":1024}
    }))
    .unwrap();
    assert_eq!(
        config
            .into_runtime_config()
            .unwrap()
            .resources
            .max_declared_memory_bytes,
        1024
    );
    for resource in [
        json!({"max_declared_memory_bytes":0}),
        json!({"hard_memory_limit_bytes":128}),
    ] {
        let parsed = serde_json::from_value::<HeadlessConfig>(json!({"resources":resource}));
        assert!(parsed.is_err() || parsed.unwrap().into_runtime_config().is_err());
    }
}
