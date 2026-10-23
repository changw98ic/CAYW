use async_trait::async_trait;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_codemode::CapabilityError;
use cayw_host::ExternalHost;
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, TaskState};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, r: ModelRequest, _: CancellationToken) -> AssistantStream {
        let m = cayw_runtime::fixture::script_response(
            &r,
            self.0,
            &format!("tool-{}", r.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message: m,
        }]))
    }
}
struct Service {
    calls: AtomicUsize,
    entered: Notify,
    block: bool,
    first_release: Option<Semaphore>,
}
#[async_trait]
impl ExternalHost for Service {
    async fn call(
        &self,
        _: &str,
        _: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.block {
            cancel.cancelled().await;
            return Err(CapabilityError::unknown(
                "synthetic service cancelled after dispatch",
            ));
        }
        if n == 0
            && let Some(release) = &self.first_release
        {
            tokio::select! {
                _ = cancel.cancelled() => return Err(CapabilityError::unknown(
                    "synthetic service cancelled before its coordinated reply",
                )),
                permit = release.acquire() => permit.expect("fixture release remains open").forget(),
            }
        }
        Ok(json!({"count":n+1}))
    }
}
const TWICE: &str = "local first=cayw.call('search.query',{query='same'}); local second=cayw.call('search.query',{query='same'}); return second";
fn setup(
    script: &'static str,
    service: Arc<Service>,
    lifetime: Duration,
) -> (tempfile::TempDir, Runtime, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let mut config = RuntimeConfig {
        grant_lifetime: lifetime,
        ..Default::default()
    };
    config.host.external = Some(service);
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(script)),
        Model::new("fixture", "approvals"),
        config,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    (dir, rt, id)
}
fn service() -> Arc<Service> {
    Arc::new(Service {
        calls: AtomicUsize::new(0),
        entered: Notify::new(),
        block: false,
        first_release: None,
    })
}
fn start(
    rt: &Runtime,
    id: &str,
) -> tokio::task::JoinHandle<Result<cayw_runtime::SessionView, cayw_runtime::RuntimeError>> {
    let rt = rt.clone();
    let id = id.to_string();
    tokio::spawn(async move {
        rt.prompt(&id, "call exact service", CancellationToken::new())
            .await
    })
}
async fn approval(rt: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(a) = rt.get(id).unwrap().pending_approvals.first() {
                return a.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn peer(dir: &tempfile::TempDir, lifetime: Duration) -> Runtime {
    Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(Script(TWICE)),
        Model::new("fixture", "approvals"),
        RuntimeConfig {
            grant_lifetime: lifetime,
            ..Default::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn independent_sdk_connection_decides_only_the_live_original_attempt() {
    // Break caught: an independent SDK connection has no local active map, so
    // it cannot approve the real owner's pending request despite matching facts.
    let svc = service();
    let lifetime = Duration::from_secs(10);
    let (dir, rt, id) = setup(TWICE, svc.clone(), lifetime);
    let controller = peer(&dir, lifetime);
    let task = start(&rt, &id);
    let first = approval(&controller, &id).await;
    let original = controller.tasks(&id).unwrap()[0].clone();
    assert_eq!(original.state, TaskState::Running);
    assert_eq!(svc.calls.load(Ordering::SeqCst), 0);
    controller
        .approve(&id, &first.id, ApprovalChoice::AllowOnce, "sdk-peer")
        .unwrap();
    let second = approval(&controller, &id).await;
    assert_ne!(first.id, second.id);
    assert_eq!(second.attempt_id, first.attempt_id);
    assert_eq!(svc.calls.load(Ordering::SeqCst), 1);
    assert!(
        controller
            .approve(&id, &first.id, ApprovalChoice::AllowTask, "late-peer")
            .is_err()
    );
    controller
        .approve(&id, &second.id, ApprovalChoice::Deny, "sdk-peer")
        .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(svc.calls.load(Ordering::SeqCst), 1);
    let terminal = controller.tasks(&id).unwrap()[0].clone();
    assert_eq!(terminal.id, original.id);
    assert_eq!(terminal.worker_id, original.worker_id);
    assert_eq!(terminal.attempt_id, original.attempt_id);
    assert_eq!(terminal.fence, original.fence);
    assert!(
        controller
            .approve(&id, &second.id, ApprovalChoice::AllowOnce, "late-peer")
            .is_err()
    );
    assert_eq!(controller.tasks(&id).unwrap()[0], terminal);
    assert!(
        controller
            .grants(&id)
            .unwrap()
            .iter()
            .all(|grant| grant.state != "active")
    );
}

#[tokio::test]
async fn sdk_peer_cancellation_closes_pending_approval_before_any_effect() {
    let svc = service();
    let lifetime = Duration::from_secs(10);
    let (dir, rt, id) = setup(TWICE, svc.clone(), lifetime);
    let controller = peer(&dir, lifetime);
    let task = start(&rt, &id);
    let pending = approval(&controller, &id).await;
    let original = controller.task(&pending.input.task_id).unwrap();
    assert!(controller.cancel(&original.cancel_target()).unwrap());
    assert!(
        controller
            .approve(&id, &pending.id, ApprovalChoice::AllowOnce, "late-peer")
            .is_err()
    );
    let _ = task.await.unwrap();
    assert_eq!(
        controller.tasks(&id).unwrap()[0].state,
        TaskState::Cancelled
    );
    assert_eq!(svc.calls.load(Ordering::SeqCst), 0);
    assert!(controller.grants(&id).unwrap().is_empty());
}

#[tokio::test]
async fn sdk_peer_approval_storage_failure_closes_admission_until_recovery() {
    // Break caught: a peer cannot cancel the owner's in-memory token. A failed
    // durable decision must close the persistent fence and retain the slot.
    let svc = service();
    let lifetime = Duration::from_secs(10);
    let (dir, rt, id) = setup(TWICE, svc.clone(), lifetime);
    let controller = peer(&dir, lifetime);
    let task = start(&rt, &id);
    let pending = approval(&controller, &id).await;
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_peer_activation BEFORE UPDATE OF state ON runtime_grants WHEN NEW.state='active' BEGIN SELECT RAISE(ABORT, 'injected grant activation failure'); END;").unwrap();
    assert!(matches!(
        controller.approve(&id, &pending.id, ApprovalChoice::AllowOnce, "sdk-peer"),
        Err(cayw_runtime::RuntimeError::RecoveryRequired {
            outcome_unknown: true,
            ..
        })
    ));
    assert!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(svc.calls.load(Ordering::SeqCst), 0);
    assert_eq!(controller.tasks(&id).unwrap()[0].state, TaskState::Draining);
    assert_eq!(controller.health().unwrap().running, 1);
    assert!(
        controller
            .approve(&id, &pending.id, ApprovalChoice::AllowOnce, "late-peer")
            .is_err()
    );
    assert!(
        controller
            .prompt(&id, "no replay", CancellationToken::new())
            .await
            .is_err()
    );
    db.execute_batch("DROP TRIGGER fail_peer_activation;")
        .unwrap();
    let recovered = controller.recover(&id).unwrap();
    assert_eq!(recovered.state, cayw_core::SessionState::Idle);
    assert_eq!(
        controller.tasks(&id).unwrap()[0].state,
        TaskState::Interrupted
    );
    assert_eq!(controller.health().unwrap().running, 0);
    assert_eq!(svc.calls.load(Ordering::SeqCst), 0);
}

async fn peer_double_write_failure(activation: bool, revoke: bool) {
    // Break caught: the peer cannot notify the owner's memory. Even when both
    // database writes fail, cancellation/revocation must not free admission.
    let svc = service();
    let lifetime = Duration::from_secs(10);
    let (dir, rt, id) = setup(TWICE, svc.clone(), lifetime);
    let controller = peer(&dir, lifetime);
    let task = start(&rt, &id);
    let pending = approval(&controller, &id).await;
    let original = controller.tasks(&id).unwrap()[0].clone();
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    db.execute_batch(if activation {
        "CREATE TRIGGER fail_peer_decision BEFORE UPDATE OF state ON runtime_grants WHEN NEW.state='active' BEGIN SELECT RAISE(ABORT, 'injected activation failure'); END;"
    } else {
        "CREATE TRIGGER fail_peer_decision BEFORE INSERT ON runtime_grants BEGIN SELECT RAISE(ABORT, 'injected grant insertion failure'); END;"
    }).unwrap();
    db.execute_batch("CREATE TRIGGER fail_peer_block BEFORE UPDATE OF state ON runtime_tasks WHEN NEW.state='draining' BEGIN SELECT RAISE(ABORT, 'injected admission closure failure'); END;").unwrap();
    assert!(matches!(
        controller.approve(&id, &pending.id, ApprovalChoice::AllowOnce, "sdk-peer"),
        Err(cayw_runtime::RuntimeError::RecoveryRequired {
            outcome_unknown: true,
            cleanup: Some(_),
            ..
        })
    ));
    let grants = controller.grants(&id).unwrap();
    assert!(
        controller
            .approve(&id, &pending.id, ApprovalChoice::AllowTask, "retry-peer")
            .is_err()
    );
    assert_eq!(controller.grants(&id).unwrap(), grants);
    assert_eq!(svc.calls.load(Ordering::SeqCst), 0);
    if revoke {
        let grant = controller.grants(&id).unwrap().pop().unwrap();
        controller.revoke(&id, &grant.id, "trusted-revoke").unwrap();
    } else {
        assert!(
            controller
                .cancel(&controller.task(&original.id).unwrap().cancel_target())
                .unwrap()
        );
    }
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(cayw_runtime::RuntimeError::RecoveryRequired { .. })
    ));
    let blocked = controller.tasks(&id).unwrap()[0].clone();
    assert_eq!(blocked.id, original.id);
    assert_eq!(blocked.attempt_id, original.attempt_id);
    assert_eq!(blocked.worker_id, original.worker_id);
    assert_eq!(blocked.fence, original.fence);
    assert_eq!(blocked.state, TaskState::Running);
    assert_eq!(controller.health().unwrap().running, 1);
    assert_eq!(svc.calls.load(Ordering::SeqCst), 0);
    assert!(controller.get(&id).unwrap().attempt.is_some());
    assert!(
        controller
            .approve(&id, &pending.id, ApprovalChoice::AllowTask, "late-peer")
            .is_err()
    );
    assert!(
        controller
            .prompt(&id, "no replay", CancellationToken::new())
            .await
            .is_err()
    );
    db.execute_batch("DROP TRIGGER fail_peer_decision; DROP TRIGGER fail_peer_block;")
        .unwrap();
    assert_eq!(
        controller.recover(&id).unwrap().state,
        cayw_core::SessionState::Idle
    );
    assert_eq!(
        controller.tasks(&id).unwrap()[0].state,
        TaskState::Interrupted
    );
    assert_eq!(controller.health().unwrap().running, 0);
    assert_eq!(svc.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn sdk_peer_grant_insertion_and_block_failures_retain_original_admission() {
    peer_double_write_failure(false, false).await;
}

#[tokio::test]
async fn sdk_peer_grant_activation_and_block_failures_retain_original_admission() {
    peer_double_write_failure(true, false).await;
}

#[tokio::test]
async fn sdk_peer_revocation_cannot_erase_failed_decision_before_recovery() {
    peer_double_write_failure(true, true).await;
}

#[tokio::test]
async fn once_prompts_again_and_task_grant_does_not_escape_its_attempt() {
    let svc = service();
    let (_dir, rt, id) = setup(TWICE, svc.clone(), Duration::from_secs(10));
    let task = start(&rt, &id);
    let first = approval(&rt, &id).await;
    rt.approve(&id, &first.id, ApprovalChoice::AllowOnce, "host-user")
        .unwrap();
    let second = approval(&rt, &id).await;
    assert_ne!(first.id, second.id);
    assert_eq!(svc.calls.load(Ordering::SeqCst), 1);
    rt.approve(&id, &second.id, ApprovalChoice::AllowTask, "host-user")
        .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(svc.calls.load(Ordering::SeqCst), 2);
    let grants = rt.grants(&id).unwrap();
    assert!(grants.iter().any(|g| g.mode == "allow_once" && g.consumed));
    assert!(grants.iter().all(|g| g.state == "revoked"));
    let task = start(&rt, &id);
    let third = approval(&rt, &id).await;
    assert_ne!(third.attempt_id, first.attempt_id);
    rt.approve(&id, &third.id, ApprovalChoice::Deny, "host-user")
        .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(svc.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn task_grant_reuses_only_exact_scope_until_task_terminal() {
    let svc = service();
    let (_dir, rt, id) = setup(TWICE, svc.clone(), Duration::from_secs(10));
    let task = start(&rt, &id);
    let a = approval(&rt, &id).await;
    rt.approve(&id, &a.id, ApprovalChoice::AllowTask, "host-user")
        .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(svc.calls.load(Ordering::SeqCst), 2);
    assert_eq!(rt.store().approvals(&id).unwrap().len(), 1);
    assert!(rt.grants(&id).unwrap().iter().all(|g| g.state == "revoked"));
    let svc = service();
    let (_other, rt, id) = setup(
        "cayw.call('search.query',{query='same'});return cayw.call('search.query',{query='different'})",
        svc.clone(),
        Duration::from_secs(10),
    );
    let task = start(&rt, &id);
    let a = approval(&rt, &id).await;
    rt.approve(&id, &a.id, ApprovalChoice::AllowTask, "host-user")
        .unwrap();
    let other = approval(&rt, &id).await;
    assert_ne!(other.input.scope, a.input.scope);
    assert_eq!(svc.calls.load(Ordering::SeqCst), 1);
    rt.approve(&id, &other.id, ApprovalChoice::Deny, "host-user")
        .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(svc.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn expired_grant_requires_a_new_decision_before_second_dispatch() {
    // Ignoring a persisted expiry would reuse this task grant and dispatch the
    // second call, so it could not produce the new pending approval below.
    let svc = Arc::new(Service {
        calls: AtomicUsize::new(0),
        entered: Notify::new(),
        block: false,
        first_release: Some(Semaphore::new(0)),
    });
    let (dir, rt, id) = setup(TWICE, svc.clone(), Duration::from_secs(300));
    let task = start(&rt, &id);
    let first = approval(&rt, &id).await;
    rt.approve(&id, &first.id, ApprovalChoice::AllowTask, "host-user")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), svc.entered.notified())
        .await
        .expect("first call must enter before its grant expires");
    assert_eq!(svc.calls.load(Ordering::SeqCst), 1);
    let original = rt
        .grants(&id)
        .unwrap()
        .into_iter()
        .find(|grant| grant.approval_id == first.id)
        .unwrap();
    assert_eq!(original.state, "active");
    assert_eq!(original.mode, "allow_task");
    assert!(!original.consumed);
    assert!(original.expires_at > cayw_ai::timestamp_ms());

    // Change only the persisted deadline while the admitted first call is held.
    // This exercises the real next-call expiry check without a CI timing window.
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE runtime_grants SET expires=0 WHERE id=?1 AND session=?2 AND approval=?3 AND state='active'",
            rusqlite::params![original.id, id, first.id],
        )
        .unwrap(),
        1
    );
    let expired = rt
        .grants(&id)
        .unwrap()
        .into_iter()
        .find(|grant| grant.id == original.id)
        .unwrap();
    let mut expected = original;
    expected.expires_at = 0;
    assert_eq!(expired, expected);
    assert!(expired.expires_at <= cayw_ai::timestamp_ms());
    svc.first_release.as_ref().unwrap().add_permits(1);

    let second = approval(&rt, &id).await;
    assert_ne!(first.id, second.id);
    assert_eq!(first.attempt_id, second.attempt_id);
    assert_eq!(first.input.capability, second.input.capability);
    assert_eq!(first.input.scope, second.input.scope);
    assert_eq!(svc.calls.load(Ordering::SeqCst), 1);
    rt.approve(&id, &second.id, ApprovalChoice::Deny, "host-user")
        .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(svc.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn revoking_live_grant_cancels_dispatch_and_retains_unknown_effect() {
    let svc = Arc::new(Service {
        calls: AtomicUsize::new(0),
        entered: Notify::new(),
        block: true,
        first_release: None,
    });
    let (_dir, rt, id) = setup(
        "return cayw.call('search.query',{query='same'})",
        svc.clone(),
        Duration::from_secs(10),
    );
    let task = start(&rt, &id);
    let a = approval(&rt, &id).await;
    rt.approve(&id, &a.id, ApprovalChoice::AllowTask, "host-user")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), svc.entered.notified())
        .await
        .unwrap();
    let grant = rt
        .grants(&id)
        .unwrap()
        .into_iter()
        .find(|g| g.state == "active")
        .unwrap();
    rt.revoke(&id, &grant.id, "host-user").unwrap();
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.state, cayw_core::SessionState::NeedsReconciliation);
    assert!(rt.grants(&id).unwrap().iter().all(|g| g.state == "revoked"));
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::OutcomeUnknown);
    assert!(
        rt.approve(&id, &a.id, ApprovalChoice::AllowTask, "late-user")
            .is_err()
    );
}
