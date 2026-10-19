use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_requests::{OperationScope, RequestRecord, RequestState};
use cayw_runtime::{RequestPolicy, Runtime, RuntimeConfig, ScopedPermission};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

struct Script {
    pause: Arc<Semaphore>,
    calls: AtomicUsize,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "local input={purpose='read requested source',gap='exact read unavailable',operation_id='read-a',capability='fs.read',arguments={path='a.txt'}}; cayw.call('capability.request',input); cayw.call('capability.request',input); yield_control(); return cayw.call('fs.read',{path='a.txt'})",
            &format!("request-read-{}", request.messages.len()),
        );
        let pause = (self.calls.fetch_add(1, Ordering::SeqCst) == 1).then(|| self.pause.clone());
        Box::pin(
            async_stream::stream! { if let Some(pause) = pause { pause.acquire_owned().await.unwrap().forget(); } yield AssistantEvent::Done { message }; },
        )
    }
}
async fn request(rt: &Runtime, task: &str) -> RequestRecord {
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
#[tokio::test]
async fn request_protected_frozen_read_retains_original_receipt_after_request_tools() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "request approved bytes").unwrap();
    let pause = Arc::new(Semaphore::new(0));
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    config.permissions = vec![ScopedPermission::exact("fs.read", json!({"path":"a.txt"}))];
    config.request_policy = Some(RequestPolicy {
        permission_policy_id: "shared-read-fixture".into(),
        operations: vec![OperationScope::exact(
            "read-a",
            "fs.read",
            json!({"path":"a.txt"}),
            "workspace-policy",
        )],
    });
    let rt = Runtime::open(
        dir.path().join("session.db"),
        &root,
        Arc::new(Script {
            pause: pause.clone(),
            calls: AtomicUsize::new(0),
        }),
        Model::new("fixture", "read-request"),
        config,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let task = rt.enqueue(&id, "read requested file").unwrap();
    let worker_rt = rt.clone();
    let task_id = task.id.clone();
    let worker =
        tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
    let original = request(&rt, &task.id).await;
    let ticket = rt
        .request_evaluate(&task.id, &original.id, "fixture-host")
        .unwrap();
    rt.request_approve(
        &task.id,
        &ticket,
        Duration::from_secs(30),
        "verified requested file",
    )
    .unwrap();
    pause.add_permits(1);
    let result = worker.await.unwrap().unwrap();
    assert_eq!(result.state, SessionState::Idle);
    let events = rt.events(&id, 0, 1000).unwrap();
    let read = events
        .iter()
        .find_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "fs.read" => Some(call),
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(&read.outcome, Some(CapabilityOutcome::Completed { result }) if result["content"] == "request approved bytes"),
        "request tools disabled their own authorized read: {read:?}"
    );
    let admitted = events
        .iter()
        .find_map(|event| match &event.event {
            CoreEvent::Host { kind, data } if kind == "task_request_admitted" => Some(data),
            _ => None,
        })
        .unwrap();
    assert_eq!(admitted["receipt"]["request_id"], original.id);
    let shared = events
        .iter()
        .find_map(|event| match &event.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data),
            _ => None,
        })
        .unwrap();
    assert_eq!(shared["role"], "uncached");
    assert_eq!(shared["request"], admitted["receipt"]);
    assert_eq!(shared["key"], serde_json::to_value(&read.key).unwrap());
    assert_eq!(shared["origin"]["sequence"], 3);
    rt.recover(&id).unwrap();
}

#[tokio::test]
async fn request_approval_and_catalog_keep_original_held_root_after_path_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let moved = dir.path().join("moved");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "original held bytes").unwrap();
    let pause = Arc::new(Semaphore::new(0));
    let provider = Arc::new(Script {
        pause: pause.clone(),
        calls: AtomicUsize::new(0),
    });
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    config.permissions = vec![ScopedPermission::exact("fs.read", json!({"path":"a.txt"}))];
    config.request_policy = Some(RequestPolicy {
        permission_policy_id: "held-read-request".into(),
        operations: vec![OperationScope::exact(
            "read-a",
            "fs.read",
            json!({"path":"a.txt"}),
            "workspace-policy",
        )],
    });
    let rt = Runtime::open(
        dir.path().join("session.db"),
        &root,
        provider.clone(),
        Model::new("fixture", "held-request"),
        config,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let original_catalog = serde_json::to_value(rt.request_operations().unwrap()).unwrap();
    let task = rt.enqueue(&id, "read original held workspace").unwrap();
    let worker_rt = rt.clone();
    let task_id = task.id.clone();
    let worker =
        tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
    let original = request(&rt, &task.id).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let ticket = rt
        .request_evaluate(&task.id, &original.id, "fixture-host")
        .unwrap();
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "replacement bytes").unwrap();
    rt.request_approve(
        &task.id,
        &ticket,
        Duration::from_secs(30),
        "original held root approved",
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(rt.request_operations().unwrap()).unwrap(),
        original_catalog
    );
    rt.capabilities().unwrap();
    pause.add_permits(1);
    worker.await.unwrap().unwrap();
    let read = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "fs.read" => Some(call),
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(read.outcome,Some(CapabilityOutcome::Completed {result}) if result["content"]=="original held bytes")
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "replacement bytes"
    );
    rt.recover(&id).unwrap();
}

#[tokio::test]
async fn original_request_expiry_or_revocation_during_fence_wait_never_delivers_bytes() {
    for expire in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "protected original bytes").unwrap();
        let pause = Arc::new(Semaphore::new(0));
        let provider = Arc::new(Script {
            pause: pause.clone(),
            calls: AtomicUsize::new(0),
        });
        let mut config = RuntimeConfig::default();
        config.host.frozen_reads = true;
        config.permissions = vec![ScopedPermission::exact("fs.read", json!({"path":"a.txt"}))];
        config.request_policy = Some(RequestPolicy {
            permission_policy_id: "shared-read-wait-fixture".into(),
            operations: vec![OperationScope::exact(
                "read-a",
                "fs.read",
                json!({"path":"a.txt"}),
                "workspace-policy",
            )],
        });
        let rt = Runtime::open(
            dir.path().join("session.db"),
            &root,
            provider.clone(),
            Model::new("fixture", "request-fence"),
            config,
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        let task = rt.enqueue(&id, "read protected file").unwrap();
        let worker_rt = rt.clone();
        let task_id = task.id.clone();
        let worker =
            tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
        let original = request(&rt, &task.id).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while provider.calls.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let ticket = rt
            .request_evaluate(&task.id, &original.id, "fixture-host")
            .unwrap();
        rt.request_approve(
            &task.id,
            &ticket,
            if expire {
                Duration::from_secs(2)
            } else {
                Duration::from_secs(30)
            },
            "original exact read",
        )
        .unwrap();
        let fence = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".cayw-host-read-fence"))
            .unwrap();
        fence.lock().unwrap();
        pause.add_permits(1);
        let started = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(call) = rt
                    .events(&id, 0, 1000)
                    .unwrap()
                    .into_iter()
                    .find_map(|event| match event.event {
                        CoreEvent::CapabilityStarted { call } if call.name == "fs.read" => {
                            Some(call)
                        }
                        _ => None,
                    })
                {
                    break call;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if !expire {
            rt.request_revoke(
                &task.id,
                &original.id,
                "fixture-host",
                "original read withdrawn while waiting",
            )
            .unwrap();
        }
        let _result = tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap();
        let events = rt.events(&id, 0, 1000).unwrap();
        let ended: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.event {
                CoreEvent::CapabilitySettled { call } if call.name == "fs.read" => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].key, started.key);
        assert_eq!(ended[0].authorization, started.authorization);
        assert!(
            matches!(ended[0].outcome, Some(CapabilityOutcome::Failed { .. })),
            "{ended:?}"
        );
        assert!(!events.iter().any(|event| matches!(&event.event, CoreEvent::Host {kind,..} if kind == "runtime_shared_read")));
        let admitted = events
            .iter()
            .find_map(|event| match &event.event {
                CoreEvent::Host { kind, data } if kind == "task_request_admitted" => Some(data),
                _ => None,
            })
            .unwrap();
        assert_eq!(admitted["key"], serde_json::to_value(&started.key).unwrap());
        assert_eq!(admitted["receipt"]["request_id"], original.id);
        assert_eq!(
            rt.request_status(&task.id, &original.id).unwrap().state,
            if expire {
                RequestState::Expired
            } else {
                RequestState::Revoked
            }
        );
        drop(fence);
        rt.recover(&id).unwrap();
    }
}
