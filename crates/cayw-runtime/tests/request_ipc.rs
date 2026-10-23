//! Trusted local control uses authenticated IPC; model calls remain requests only.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_host::{ProcessIo, Profile, ProfileKind};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_requests::{OperationScope, RequestRecord, RequestState};
use cayw_runtime::{
    RequestPolicy, Runtime, RuntimeConfig, ScopedPermission, TaskIntent, TaskRecord,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};

const TOKEN: &str = "issue8-public-local-control-fixture";
const SCRIPT: &str = "local r=cayw.call('capability.request',{purpose='run the exact requested project test',gap='test profile requires host permission',operation_id='project-test',capability='test.run',arguments={profile='project'}}); cayw.emit(r); yield_control(); return cayw.call('test.run',{profile='project'})";

struct Script {
    pause: Arc<Semaphore>,
    calls: AtomicUsize,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            SCRIPT,
            &format!("request-ipc-{}", request.messages.len()),
        );
        let pause = (self.calls.fetch_add(1, Ordering::AcqRel) == 1).then(|| self.pause.clone());
        Box::pin(async_stream::stream! {
            if let Some(pause) = pause { pause.acquire_owned().await.unwrap().forget(); }
            yield AssistantEvent::Done { message };
        })
    }
}
fn setup() -> (tempfile::TempDir, Runtime, Arc<Semaphore>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("release"), "allowed to exit").unwrap();
    let mut config = RuntimeConfig {
        max_run_time: Duration::from_secs(20),
        ..Default::default()
    };
    config.host.profiles.insert(
        "project".into(),
        Profile {
            kind: ProfileKind::Test,
            program: env!("CARGO_BIN_EXE_cayw-runtime-process-fixture").into(),
            args: vec!["leaf".into(), root.to_string_lossy().into_owned()],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 4096,
            allow_filter: false,
            io: ProcessIo::Pipes,
        },
    );
    config.permissions = vec![ScopedPermission::exact(
        "test.run",
        json!({"profile":"project"}),
    )];
    config.request_policy = Some(RequestPolicy {
        permission_policy_id: "issue8-ipc-fixture".into(),
        operations: vec![OperationScope::exact(
            "project-test",
            "test.run",
            json!({"profile":"project"}),
            "project-policy",
        )],
    });
    let pause = Arc::new(Semaphore::new(0));
    let rt = Runtime::open(
        temp.path().join("session.db"),
        root,
        Arc::new(Script {
            pause: pause.clone(),
            calls: AtomicUsize::new(0),
        }),
        Model::new("fixture", "request-ipc"),
        config,
    )
    .unwrap();
    (temp, rt, pause)
}
async fn server(
    rt: Runtime,
) -> (
    std::net::SocketAddr,
    CancellationToken,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let worker = tokio::spawn(serve(
        listener,
        Arc::new(rt),
        ServerConfig::new(TOKEN),
        stop.clone(),
    ));
    (address, stop, worker)
}
async fn awaiting(rt: &Runtime, task: &str) -> RequestRecord {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(events) = rt.request_events(task, 0, 64) {
                for event in events.into_iter().rev() {
                    if let Some(id) = event.request_id {
                        let record = rt.request_status(task, &id).unwrap();
                        if record.state == RequestState::AwaitingEvaluation {
                            return record;
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
async fn enqueue(client: &Client, id: &str) -> TaskRecord {
    serde_json::from_value(client.request("task.enqueue_with_intent", json!({
        "session_id":id, "text":"run this exact project test",
        "intent":TaskIntent::ExplicitProjectTestOrScript { user_instruction:"run this exact project test".into() }
    })).await.unwrap()).unwrap()
}
async fn finish(
    client: Client,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
) {
    client.close().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_control_lists_exact_operations_and_records_explicit_host_intent() {
    let (_temp, rt, _pause) = setup();
    let id = rt.create(None).unwrap().id;
    let (address, stop, server) = server(rt.clone()).await;
    let client = Client::connect(address, TOKEN).await.unwrap();
    let operations = client
        .request("request.operations", json!({}))
        .await
        .unwrap();
    assert_eq!(operations[0]["capability"], "test.run");
    assert_eq!(operations[0]["arguments"], json!({"profile":"project"}));
    let forged = client
        .request(
            "task.enqueue",
            json!({"session_id":id,"text":"test","intent":{"kind":"general"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(forged.code, "invalid_params");
    let task = enqueue(&client, &id).await;
    assert!(matches!(
        task.intent,
        TaskIntent::ExplicitProjectTestOrScript { .. }
    ));
    assert_eq!(rt.task(&task.id).unwrap().intent, task.intent);
    assert!(
        rt.capabilities()
            .unwrap()
            .iter()
            .all(|definition| !definition.name.starts_with("request."))
    );
    assert!(
        rt.request_operations()
            .unwrap()
            .iter()
            .all(|operation| !operation.capability.starts_with("request."))
    );
    rt.cancel(&task.cancel_target()).unwrap();
    finish(client, stop, server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_decision_uses_fixed_actor_and_original_task_before_real_dispatch() {
    let (temp, rt, pause) = setup();
    let id = rt.create(None).unwrap().id;
    let (address, stop, server) = server(rt.clone()).await;
    let client = Client::connect(address, TOKEN).await.unwrap();
    let task = enqueue(&client, &id).await;
    let run_client = client.clone();
    let task_id = task.id.clone();
    let run = tokio::spawn(async move {
        run_client
            .request("task.resume", json!({"task_id":task_id}))
            .await
    });
    let record = awaiting(&rt, &task.id).await;
    assert_eq!(record.valid_request_count, 1);
    assert!(!temp.path().join("workspace/leaf.pid").exists());
    for (field, value) in [
        ("actor", json!("model")),
        ("role", json!("host")),
        ("approved", json!(true)),
    ] {
        let mut forged = json!({"task_id":task.id,"request_id":record.id,"decision":{"kind":"allow","lifetime_ms":10000},"reason":"host decision"});
        forged[field] = value;
        assert_eq!(
            client
                .request("request.decide", forged)
                .await
                .unwrap_err()
                .code,
            "invalid_params"
        );
        assert_eq!(
            rt.request_status(&task.id, &record.id).unwrap().state,
            RequestState::AwaitingEvaluation
        );
    }
    for (decision, reason) in [
        (json!({"kind":"allow","lifetime_ms":0}), "invalid lifetime"),
        (
            json!({"kind":"allow","lifetime_ms":u64::MAX}),
            "invalid lifetime",
        ),
        (json!({"kind":"deny"}), ""),
        (json!({"kind":"unavailable"}), "invalid\u{0000}reason"),
    ] {
        let invalid =
            json!({"task_id":task.id,"request_id":record.id,"decision":decision,"reason":reason});
        assert_eq!(
            client
                .request("request.decide", invalid)
                .await
                .unwrap_err()
                .code,
            "invalid_params"
        );
        assert_eq!(
            rt.request_status(&task.id, &record.id).unwrap().state,
            RequestState::AwaitingEvaluation
        );
    }
    let params = json!({"task_id":task.id,"request_id":record.id,"decision":{"kind":"allow","lifetime_ms":10000},"reason":"trusted exact project test"});
    let granted = client
        .request("request.decide", params.clone())
        .await
        .unwrap();
    assert_eq!(granted["state"], "granted");
    let duplicate = client.request("request.decide", params).await.unwrap_err();
    assert_eq!(duplicate.code, "request_stale");
    let state = client
        .request(
            "request.status",
            json!({"task_id":task.id,"request_id":record.id}),
        )
        .await
        .unwrap();
    assert_eq!(state, granted);
    let events = client
        .request(
            "request.events",
            json!({"task_id":task.id,"after":0,"limit":64}),
        )
        .await
        .unwrap();
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["actor"] == "rpc-user")
    );
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["actor"] != "model")
    );
    let default_page = client
        .request("request.events", json!({"task_id":task.id}))
        .await
        .unwrap();
    assert_eq!(default_page, events);
    pause.add_permits(1);
    let view = run.await.unwrap().unwrap();
    assert_eq!(view["state"], "idle");
    assert!(temp.path().join("workspace/leaf.pid").is_file());
    let calls: Vec<_> = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.task_id == task.id));
    assert!(calls.iter().any(|call| call.name == "test.run"
        && matches!(call.outcome, Some(CapabilityOutcome::Completed { .. }))));
    assert!(matches!(rt.get(&id).unwrap().state, SessionState::Idle));
    let ended = client
        .request("request.task_state", json!({"task_id":task.id}))
        .await
        .unwrap();
    assert_eq!(ended["state"], "ended");
    let late = client.request("request.decide", json!({"task_id":task.id,"request_id":record.id,"decision":{"kind":"allow","lifetime_ms":10000},"reason":"late"})).await.unwrap_err();
    assert_eq!(late.code, "stale_approval");
    finish(client, stop, server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_revoke_before_dispatch_cancels_only_original_task() {
    let (temp, rt, pause) = setup();
    let id = rt.create(None).unwrap().id;
    let (address, stop, server) = server(rt.clone()).await;
    let client = Client::connect(address, TOKEN).await.unwrap();
    let task = enqueue(&client, &id).await;
    let worker_rt = rt.clone();
    let task_id = task.id.clone();
    let run =
        tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
    let record = awaiting(&rt, &task.id).await;
    client.request("request.decide", json!({"task_id":task.id,"request_id":record.id,"decision":{"kind":"allow","lifetime_ms":10000},"reason":"original grant"})).await.unwrap();
    client
        .request(
            "request.revoke",
            json!({"task_id":task.id,"request_id":record.id,"reason":"withdraw before dispatch"}),
        )
        .await
        .unwrap();
    pause.add_permits(1);
    let _ = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
    assert!(!temp.path().join("workspace/leaf.pid").exists());
    assert_eq!(
        rt.request_status(&task.id, &record.id).unwrap().state,
        RequestState::Revoked
    );
    assert_eq!(
        rt.task(&task.id).unwrap().state,
        cayw_runtime::TaskState::Cancelled
    );
    assert_eq!(rt.health().unwrap().running, 0);
    finish(client, stop, server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_query_decision_and_revocation_never_reach_runtime() {
    let (_temp, rt, _pause) = setup();
    let (address, stop, server) = server(rt.clone()).await;
    assert_eq!(
        Client::connect(address, "incorrect-public-fixture-token")
            .await
            .unwrap_err()
            .code,
        "unauthorized"
    );
    for (index, method) in [
        "request.operations",
        "request.status",
        "request.decide",
        "request.revoke",
    ]
    .into_iter()
    .enumerate()
    {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let frame = json!({"version":1,"id":format!("unauth-{index}"),"method":method,"params":{}});
        stream
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            BufReader::new(stream).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["error"]["code"], "handshake_required");
    }
    assert!(rt.list().unwrap().is_empty());
    stop.cancel();
    server.await.unwrap().unwrap();
}
