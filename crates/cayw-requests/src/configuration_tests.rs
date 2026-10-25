use super::*;
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use cayw_host::{AuditSink, HostConfig, PermissionGate};

struct Clock1000;
impl Clock for Clock1000 {
    fn now_ms(&self) -> Result<u64> {
        Ok(1000)
    }
}
type Pause = (
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[derive(Default)]
struct PausingClock(Mutex<Option<Pause>>);
impl Clock for PausingClock {
    fn now_ms(&self) -> Result<u64> {
        let pause = self.0.lock().unwrap().take();
        if let Some((reached, resume)) = pause {
            reached.send(()).unwrap();
            resume.recv().unwrap();
        }
        Ok(1000)
    }
}
struct Allow;
#[async_trait::async_trait]
impl PermissionGate for Allow {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
}
struct Audit;
#[async_trait::async_trait]
impl AuditSink for Audit {
    async fn started(&self, _: &str, _: &Value) -> std::result::Result<String, CapabilityError> {
        Ok("fixture-call".into())
    }
    async fn settled(
        &self,
        _: &str,
        _: &std::result::Result<Value, CapabilityError>,
    ) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &str) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
}

// A normal operation must retain its configuration until its transaction has
// finished. Pause a real public task check after it enters the authority and
// try the competing configuration write directly, then run the real adjustment.
#[test]
fn same_store_adjustment_waits_for_the_active_task_check() {
    use std::sync::{TryLockError, mpsc::sync_channel};

    let temp = tempfile::tempdir().unwrap();
    let catalog = crate::RequestHostConfig {
        root: temp.path().into(),
        host: HostConfig::default(),
        permission_policy_id: "adjustment-policy".into(),
        operations: vec![crate::OperationScope::exact(
            "write",
            "fs.write",
            json!({"path":"result.txt","content":"approved"}),
            "adjustment-policy",
        )],
        gate: Arc::new(Allow),
        audit: Arc::new(Audit),
    }
    .catalog()
    .unwrap();
    let actor = HostActor::new("trusted-host").unwrap();
    let identity = TaskIdentity::new("session", "task", "attempt").unwrap();
    let config = RequestConfig::default();
    let path = temp.path().join("requests.sqlite");
    let clock = Arc::new(PausingClock::default());
    let store = RequestStore::open_with_clock(&path, config.clone(), clock.clone()).unwrap();
    let stale = RequestStore::open_with_clock(&path, config.clone(), Arc::new(Clock1000)).unwrap();
    store
        .start_task(&identity, TaskIntent::General, &catalog, &actor)
        .unwrap();
    let next = RequestConfig {
        max_tasks: config.max_tasks - 1,
        ..config
    };
    let (reached, wait_for_check) = sync_channel(0);
    let (resume, wait_for_resume) = sync_channel(0);
    *clock.0.lock().unwrap() = Some((reached, wait_for_resume));
    std::thread::scope(|scope| {
        let checking = scope.spawn(|| store.check_task(&identity, &catalog));
        wait_for_check
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        let (probed, wait_for_probe) = sync_channel(0);
        let store = &store;
        let next = &next;
        let actor = &actor;
        let adjusting = scope.spawn(move || {
            let blocked = matches!(store.config.try_write(), Err(TryLockError::WouldBlock));
            probed.send(blocked).unwrap();
            store.adjust_config(next.clone(), actor, "trusted adjustment during active work")
        });
        let blocked = wait_for_probe
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        // Always release the operation and join both workers before asserting,
        // including the failing implementation where a write lock was available.
        resume.send(()).unwrap();
        checking.join().unwrap().unwrap();
        adjusting.join().unwrap().unwrap();
        assert!(
            blocked,
            "an active task transaction released its configuration before it completed"
        );
    });
    assert_eq!(store.config(), next);
    assert_eq!(RequestStore::persisted_config(&path).unwrap(), next);
    assert_eq!(store.config_history().unwrap().len(), 1);
    store.check_task(&identity, &catalog).unwrap();
    assert_eq!(store.task_state(&identity).unwrap(), TaskState::Active);
    assert!(matches!(
        stale.check_task(&identity, &catalog),
        Err(RequestError::PolicyMismatch)
    ));
}

// Serializes the two sides of a concurrent adjustment at the durable decision
// boundary. An earlier caller-side lifetime check must not authorize a grant
// after the host has lowered the limit before that decision transaction.
#[test]
fn decision_transaction_rechecks_lifetime_after_a_trusted_adjustment() {
    let temp = tempfile::tempdir().unwrap();
    let arguments = json!({"path":"result.txt","content":"approved"});
    let catalog = crate::RequestHostConfig {
        root: temp.path().into(),
        host: HostConfig::default(),
        permission_policy_id: "decision-policy".into(),
        operations: vec![crate::OperationScope::exact(
            "write",
            "fs.write",
            arguments.clone(),
            "decision-policy",
        )],
        gate: Arc::new(Allow),
        audit: Arc::new(Audit),
    }
    .catalog()
    .unwrap();
    let actor = HostActor::new("trusted-host").unwrap();
    let identity = TaskIdentity::new("session", "task", "attempt").unwrap();
    let config = RequestConfig {
        max_grant_lifetime_ms: 2000,
        ..RequestConfig::default()
    };
    let store = RequestStore::open_with_clock(
        temp.path().join("requests.sqlite"),
        config.clone(),
        Arc::new(Clock1000),
    )
    .unwrap();
    store
        .start_task(&identity, TaskIntent::General, &catalog, &actor)
        .unwrap();
    let request = store
        .request(
            &identity,
            &catalog,
            RequestInput {
                purpose: "save the selected result".into(),
                gap: "the result needs an exact write grant".into(),
                operation_id: "write".into(),
                capability: "fs.write".into(),
                arguments,
            },
        )
        .unwrap();
    let ticket = store.evaluate(&identity, &request.id, &actor).unwrap();
    let lifetime = 1500;
    assert!(lifetime <= store.config().max_grant_lifetime_ms);
    store
        .adjust_config(
            RequestConfig {
                max_grant_lifetime_ms: 1000,
                ..config
            },
            &actor,
            "host lowered the maximum before the decision transaction",
        )
        .unwrap();
    assert!(matches!(
        store.finish_evaluation(
            &ticket,
            Some(&catalog),
            RequestState::Granted,
            DecisionKind::Approved,
            Some(lifetime),
            "earlier preflight accepted this lifetime",
        ),
        Err(RequestError::InvalidInput(_))
    ));
    assert_eq!(
        store.status(&identity, &request.id).unwrap().state,
        RequestState::Evaluating
    );
    let granted = store
        .approve(&ticket, &catalog, 1000, "within the current maximum")
        .unwrap();
    assert_eq!(granted.state, RequestState::Granted);
    assert_eq!(granted.grant_expires_at_ms, Some(2000));
}
