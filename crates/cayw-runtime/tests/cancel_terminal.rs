//! Synthetic offline fixtures; Runtime, Host, Core, Requests and TCP are real.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_codemode::CapabilityError;
use cayw_core::{ApprovalRequest, CapabilityOutcome, CoreEvent, SessionState};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_requests::{TaskOutcome, TaskState as RequestTaskState};
use cayw_runtime::{
    ApprovalChoice, RequestPolicy, Runtime, RuntimeConfig, RuntimeError, ScopedPermission,
    TaskState,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore};

const WATCHDOG: Duration = Duration::from_secs(10);
const TOKEN: &str = "synthetic-peer-cancel-token";

fn config() -> RuntimeConfig {
    RuntimeConfig {
        request_policy: Some(RequestPolicy {
            permission_policy_id: "synthetic-cancel-policy".into(),
            operations: vec![cayw_requests::OperationScope::exact(
                "read-fixture",
                "fs.read",
                json!({"path":"unused.txt"}),
                "workspace-policy",
            )],
        }),
        ..Default::default()
    }
}
fn open(root: &Path, provider: Arc<dyn Provider>, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        root.join("session.sqlite"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "cancel-terminal"),
        config,
    )
    .unwrap()
}
fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    root
}
struct CancelBeforeReply(Mutex<Option<(Runtime, String)>>);
impl Provider for CancelBeforeReply {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        // Run in the same poll after request preparation passed its fence. The
        // separate connection accepts cancellation before this ready model reply.
        let (peer, task_id) = self.0.lock().unwrap().take().unwrap();
        let target = peer.task(&task_id).unwrap().cancel_target();
        assert!(peer.cancel(&target).unwrap());
        let mut message = AssistantMessage::new(&request.model);
        message.content = vec![Content::text(
            "synthetic terminal after accepted cancellation",
        )];
        message.stop_reason = StopReason::Stop;
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
#[tokio::test]
async fn peer_sdk_cancel_before_ready_provider_terminal_ends_both_authorities_cancelled() {
    let root = root();
    let provider = Arc::new(CancelBeforeReply(Mutex::new(None)));
    let rt = open(root.path(), provider.clone(), config());
    let peer = open(root.path(), provider.clone(), config());
    let id = rt.create(None).unwrap().id;
    let admitted = rt
        .enqueue(&id, "synthetic cancellation at final reply")
        .unwrap();
    *provider.0.lock().unwrap() = Some((peer.clone(), admitted.id.clone()));
    rt.resume(&admitted.id, CancellationToken::new())
        .await
        .unwrap();
    let task = rt.task(&admitted.id).unwrap();
    assert_eq!(task.state, TaskState::Cancelled);
    assert!(task.cancel_requested);
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        RequestTaskState::Ended {
            outcome: TaskOutcome::Cancelled,
            ..
        }
    ));
    let audit = rt.audit(&id, 0, 1000).unwrap();
    let terminals: Vec<_> = audit
        .iter()
        .filter(|e| e.task_id.as_deref() == Some(&task.id) && e.kind.starts_with("run_"))
        .collect();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].kind, "run_cancelled");
    let requests = rt.request_events(&task.id, 0, 64).unwrap();
    assert_eq!(
        requests.iter().filter(|e| e.kind == "task_ended").count(),
        1
    );
    assert!(!peer.cancel(&task.cancel_target()).unwrap());
    assert_eq!(peer.task(&task.id).unwrap(), task);
}

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            self.0,
            &format!("cancel-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
async fn pending(rt: &Runtime, id: &str) -> ApprovalRequest {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if let Some(approval) = rt.get(id).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
async fn peer_cancel_before_allow(ipc: bool) {
    let root = root();
    let provider = Arc::new(Script(
        "return cayw.call('fs.write',{path='cancel.txt',content='synthetic'})",
    ));
    let rt = open(root.path(), provider.clone(), config());
    let peer = open(root.path(), provider, config());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(peer.clone()),
        ServerConfig::new(TOKEN),
        stop.clone(),
    ));
    let client = Client::connect(addr, TOKEN).await.unwrap();
    let id = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&id, "synthetic pending write").unwrap();
    let run = {
        let rt = rt.clone();
        let task_id = admitted.id.clone();
        tokio::spawn(async move { rt.resume(&task_id, CancellationToken::new()).await })
    };
    let approval = pending(&rt, &id).await;
    let old = rt.get(&id).unwrap().attempt.unwrap();
    let target = rt.task(&admitted.id).unwrap().cancel_target();
    if ipc {
        assert_eq!(
            client
                .request("session.cancel", serde_json::to_value(&target).unwrap())
                .await
                .unwrap()["cancelled"],
            true
        );
    } else {
        assert!(peer.cancel(&target).unwrap());
    }
    assert!(
        rt.approve(
            &id,
            &approval.id,
            ApprovalChoice::AllowTask,
            "synthetic-late"
        )
        .is_err()
    );
    tokio::time::timeout(WATCHDOG, run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!root.path().join("workspace/cancel.txt").exists());
    let first = rt.task(&admitted.id).unwrap();
    assert_eq!(first.state, TaskState::Cancelled);
    assert!(matches!(
        rt.request_task_state(&first.id).unwrap(),
        RequestTaskState::Ended {
            outcome: TaskOutcome::Cancelled,
            ..
        }
    ));
    let events = rt.events(&id, 0, 1000).unwrap();
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let CoreEvent::CapabilitySettled { call } = &e.event {
                Some(call)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].lease, old);
    assert_eq!(calls[0].task_id, first.id);
    assert!(matches!(
        calls[0].outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, CoreEvent::CapabilityStarted { .. }))
    );
    let audit = rt.audit(&id, 0, 1000).unwrap();
    let terminals: Vec<_> = audit
        .iter()
        .filter(|e| e.task_id.as_deref() == Some(&first.id) && e.kind.starts_with("run_"))
        .collect();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].kind, "run_cancelled");
    // A delayed full target for the original terminal Task must not cancel the
    // next Task, either before it is claimed or after it is running.
    let replacement = rt.enqueue(&id, "explicit synthetic replacement").unwrap();
    if ipc {
        assert_eq!(
            client
                .request("session.cancel", serde_json::to_value(&target).unwrap())
                .await
                .unwrap()["cancelled"],
            false
        );
    } else {
        assert!(!peer.cancel(&target).unwrap());
    }
    assert_eq!(rt.task(&replacement.id).unwrap(), replacement);
    let run = {
        let rt = rt.clone();
        let task_id = replacement.id.clone();
        tokio::spawn(async move { rt.resume(&task_id, CancellationToken::new()).await })
    };
    let next = pending(&rt, &id).await;
    let active = rt.task(&replacement.id).unwrap();
    if ipc {
        assert_eq!(
            client
                .request("session.cancel", serde_json::to_value(&target).unwrap())
                .await
                .unwrap()["cancelled"],
            false
        );
    } else {
        assert!(!peer.cancel(&target).unwrap());
    }
    assert_eq!(rt.task(&replacement.id).unwrap(), active);
    assert!(!active.cancel_requested);
    let mut stale = active.cancel_target();
    stale.fence += 1;
    assert!(matches!(
        peer.cancel(&stale),
        Err(RuntimeError::StaleCancellation)
    ));
    let lease = rt.get(&id).unwrap().attempt.unwrap();
    assert_ne!(next.id, approval.id);
    assert_ne!(lease.attempt_id, old.attempt_id);
    assert_ne!(lease.worker_id, old.worker_id);
    assert!(lease.fence > old.fence);
    assert!(
        rt.approve(
            &id,
            &approval.id,
            ApprovalChoice::AllowOnce,
            "synthetic-stale"
        )
        .is_err()
    );
    assert!(!root.path().join("workspace/cancel.txt").exists());
    rt.approve(
        &id,
        &next.id,
        ApprovalChoice::Deny,
        "synthetic-replacement-deny",
    )
    .unwrap();
    tokio::time::timeout(WATCHDOG, run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!root.path().join("workspace/cancel.txt").exists());
    assert_eq!(rt.task(&first.id).unwrap(), first);
    assert_eq!(rt.tasks(&id).unwrap().len(), 2);
    client.close().await;
    stop.cancel();
    server.await.unwrap().unwrap();
}
#[tokio::test]
async fn peer_sdk_cancel_precedes_allow_and_replacement_needs_fresh_permission() {
    peer_cancel_before_allow(false).await;
}
#[tokio::test]
async fn authenticated_peer_ipc_cancel_precedes_allow_and_replacement_needs_fresh_permission() {
    peer_cancel_before_allow(true).await;
}

struct UnknownService {
    entered: Semaphore,
}
#[async_trait::async_trait]
impl cayw_host::ExternalHost for UnknownService {
    async fn call(
        &self,
        _: &str,
        _: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.entered.add_permits(1);
        cancel.cancelled().await;
        Err(CapabilityError::unknown(
            "synthetic effect cannot be established after cancellation",
        ))
    }
}
#[tokio::test]
async fn accepted_peer_cancel_preserves_original_unknown_instead_of_success_or_cancelled() {
    let root = root();
    let service = Arc::new(UnknownService {
        entered: Semaphore::new(0),
    });
    let mut config = config();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"synthetic"}),
    )];
    let provider = Arc::new(Script(
        "return cayw.call('search.query',{query='synthetic'})",
    ));
    let rt = open(root.path(), provider.clone(), config.clone());
    let peer = open(root.path(), provider, config);
    let id = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&id, "synthetic unknown effect").unwrap();
    let run = {
        let rt = rt.clone();
        let task_id = admitted.id.clone();
        tokio::spawn(async move { rt.resume(&task_id, CancellationToken::new()).await })
    };
    tokio::time::timeout(WATCHDOG, service.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let original = rt.get(&id).unwrap().attempt.unwrap();
    let target = rt.task(&admitted.id).unwrap().cancel_target();
    assert!(peer.cancel(&target).unwrap());
    let result = tokio::time::timeout(WATCHDOG, run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.state, SessionState::NeedsReconciliation);
    let task = rt.task(&admitted.id).unwrap();
    assert_eq!(task.state, TaskState::OutcomeUnknown);
    assert!(task.cancel_requested);
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        RequestTaskState::Ended {
            outcome: TaskOutcome::Failed,
            ..
        }
    ));
    assert_eq!(result.unresolved_capabilities.len(), 1);
    assert_eq!(result.unresolved_capabilities[0].lease, original);
    assert!(matches!(
        result.unresolved_capabilities[0].outcome,
        Some(CapabilityOutcome::OutcomeUnknown { .. })
    ));
    assert_eq!(
        rt.audit(&id, 0, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "run_outcome_unknown")
            .count(),
        1
    );
    assert!(matches!(
        peer.enqueue(&id, "must not replay"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert_eq!(
        peer.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(rt.task(&task.id).unwrap(), task);
}

#[tokio::test]
async fn accepted_peer_cancel_does_not_hide_failed_attempt_cleanup_or_release_capacity() {
    let root = root();
    let provider = Arc::new(CancelBeforeReply(Mutex::new(None)));
    let mut settings = config();
    settings.max_concurrent_runs = 1;
    settings.max_queued_runs = 1;
    let rt = open(root.path(), provider.clone(), settings.clone());
    let peer = open(root.path(), provider.clone(), settings);
    let id = rt.create(None).unwrap().id;
    let other = rt.create(None).unwrap().id;
    let admitted = rt
        .enqueue(&id, "synthetic cleanup failure after cancel")
        .unwrap();
    *provider.0.lock().unwrap() = Some((peer, admitted.id.clone()));
    let db = rusqlite::Connection::open(root.path().join("session.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_attempt_finish BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='attempt_finished' BEGIN SELECT RAISE(ABORT,'synthetic cleanup failure'); END;").unwrap();
    let error = rt
        .resume(&admitted.id, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::RecoveryRequired { .. }));
    assert!(error.to_string().contains("synthetic cleanup failure"));
    let task = rt.task(&admitted.id).unwrap();
    assert!(task.cancel_requested);
    assert_eq!(task.state, TaskState::Draining);
    assert_eq!(rt.health().unwrap().running, 1);
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        RequestTaskState::Closing { .. }
    ));
    assert!(matches!(
        rt.enqueue(&other, "capacity remains held"),
        Err(RuntimeError::QueueFull)
    ));
    assert!(
        !rt.audit(&id, 0, 1000)
            .unwrap()
            .iter()
            .any(|e| e.kind.starts_with("run_"))
    );
    db.execute_batch("DROP TRIGGER fail_attempt_finish")
        .unwrap();
    rt.recover(&id).unwrap();
    assert_eq!(rt.health().unwrap().running, 0);
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        RequestTaskState::Ended {
            outcome: TaskOutcome::Failed,
            ..
        }
    ));
}
