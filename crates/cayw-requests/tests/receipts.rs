use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, HostConfig, PermissionGate};
use cayw_requests::*;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now_ms(&self) -> Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
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
#[derive(Default)]
struct Audit {
    started: AtomicUsize,
    settled: Mutex<Vec<std::result::Result<Value, CapabilityError>>>,
}
#[async_trait::async_trait]
impl AuditSink for Audit {
    async fn started(&self, _: &str, _: &Value) -> std::result::Result<String, CapabilityError> {
        Ok(self.started.fetch_add(1, Ordering::SeqCst).to_string())
    }
    async fn settled(
        &self,
        _: &str,
        result: &std::result::Result<Value, CapabilityError>,
    ) -> std::result::Result<(), CapabilityError> {
        self.settled.lock().unwrap().push(result.clone());
        Ok(())
    }
    fn dropped(&self, _: &str) -> std::result::Result<(), CapabilityError> {
        Err(CapabilityError::unknown("interrupted"))
    }
}
fn host_identity(
    root: &std::path::Path,
    config: &HostConfig,
    policy: &str,
) -> Result<HostIdentity> {
    let host = cayw_host::Host::new(
        root,
        Arc::new(Allow),
        Arc::new(Audit::default()),
        config.clone(),
    )
    .unwrap();
    HostIdentity::from_host(&host, config, policy)
}
fn identity() -> TaskIdentity {
    TaskIdentity::new("session", "task", "attempt").unwrap()
}
fn actor() -> HostActor {
    HostActor::new("trusted-host").unwrap()
}
fn input() -> RequestInput {
    RequestInput {
        purpose: "read the selected project file".into(),
        gap: "this exact file is needed for the task".into(),
        operation_id: "file".into(),
        capability: "fs.read".into(),
        arguments: json!({"path":"ordinary.txt"}),
    }
}
struct Fixture {
    temp: tempfile::TempDir,
    store: Arc<RequestStore>,
    second: Arc<RequestStore>,
    clock: Arc<TestClock>,
    catalog: OperationCatalog,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("ordinary.txt"), "protected real content\n").unwrap();
        let clock = Arc::new(TestClock(AtomicU64::new(1000)));
        let config = RequestConfig {
            repeated_valid_threshold: 1,
            ..RequestConfig::default()
        };
        let store = Arc::new(
            RequestStore::open_with_clock(
                temp.path().join("requests.sqlite"),
                config.clone(),
                clock.clone(),
            )
            .unwrap(),
        );
        let second =
            Arc::new(RequestStore::open_with_clock(store.path(), config, clock.clone()).unwrap());
        let catalog = OperationCatalog::bind(
            host_identity(temp.path(), &HostConfig::default(), "policy").unwrap(),
            &cayw_host::Host::new(
                temp.path(),
                Arc::new(Allow),
                Arc::new(Audit::default()),
                HostConfig::default(),
            )
            .unwrap()
            .capabilities(),
            vec![OperationScope::exact(
                "file",
                "fs.read",
                input().arguments,
                "policy",
            )],
        )
        .unwrap();
        store
            .start_task(&identity(), TaskIntent::General, &catalog, &actor())
            .unwrap();
        Self {
            temp,
            store,
            second,
            clock,
            catalog,
        }
    }
    fn config(
        &self,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
    ) -> RequestHostConfig {
        RequestHostConfig {
            root: self.temp.path().into(),
            host: HostConfig::default(),
            permission_policy_id: "policy".into(),
            operations: self.catalog.operations(),
            gate,
            audit,
        }
    }
    fn grant(&self) -> RequestRecord {
        grant(&self.store, &self.catalog)
    }
}
fn grant(store: &RequestStore, catalog: &OperationCatalog) -> RequestRecord {
    let request = store.request(&identity(), catalog, input()).unwrap();
    let ticket = store.evaluate(&identity(), &request.id, &actor()).unwrap();
    store
        .approve(&ticket, catalog, 100, "host verified exact file")
        .unwrap()
}

// Omitting the I/O mode or either PTY dimension from Host identity would let
// an existing receipt authorize a different process configuration.
#[cfg(target_os = "macos")]
#[test]
fn a_process_io_change_invalidates_the_original_permission_receipt() {
    use cayw_host::{ProcessIo, Profile, ProfileKind};
    let fixture = Fixture::new();
    let config = |io| HostConfig {
        profiles: std::collections::BTreeMap::from([(
            "check".into(),
            Profile {
                kind: ProfileKind::Check,
                program: "/usr/bin/true".into(),
                args: vec![],
                env: Default::default(),
                timeout: std::time::Duration::from_secs(10),
                max_output_bytes: 1024,
                allow_filter: false,
                io,
            },
        )]),
        ..Default::default()
    };
    let catalog = |io| {
        let config = config(io);
        let host = cayw_host::Host::new(
            fixture.temp.path(),
            Arc::new(Allow),
            Arc::new(Audit::default()),
            config.clone(),
        )
        .unwrap();
        OperationCatalog::bind(
            HostIdentity::from_host(&host, &config, "policy").unwrap(),
            &host.capabilities(),
            fixture.catalog.operations(),
        )
        .unwrap()
    };
    let original = catalog(ProcessIo::Pty {
        columns: 80,
        rows: 24,
    });
    fixture
        .store
        .replace_attempt(
            &identity(),
            "io-attempt",
            &original,
            &actor(),
            "bind terminal configuration",
        )
        .unwrap();
    let task = TaskIdentity::new("session", "task", "io-attempt").unwrap();
    let request = fixture.store.request(&task, &original, input()).unwrap();
    let ticket = fixture
        .store
        .evaluate(&task, &request.id, &actor())
        .unwrap();
    fixture
        .store
        .approve(&ticket, &original, 100, "verified process configuration")
        .unwrap();
    let receipt = fixture
        .store
        .admit(&task, &original, "fs.read", &input().arguments)
        .unwrap();
    fixture
        .second
        .check_receipt(
            &receipt,
            &catalog(ProcessIo::Pty {
                columns: 80,
                rows: 24,
            }),
        )
        .unwrap();
    let admitted: Vec<_> = [
        ProcessIo::Pipes,
        ProcessIo::Pty {
            columns: 81,
            rows: 24,
        },
        ProcessIo::Pty {
            columns: 80,
            rows: 25,
        },
    ]
    .into_iter()
    .filter(|io| {
        fixture
            .second
            .check_receipt(&receipt, &catalog(*io))
            .is_ok()
    })
    .collect();
    assert!(
        admitted.is_empty(),
        "old receipt accepted changed process I/O: {admitted:?}"
    );
}

#[test]
fn receipt_is_exact_and_cannot_follow_a_newer_grant_on_another_connection() {
    let fixture = Fixture::new();
    let approved = fixture.grant();
    assert!(fixture.catalog.requires_request("fs.read"));
    assert!(!fixture.catalog.requires_request("fs.list"));
    let receipt = fixture
        .store
        .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
        .unwrap();
    assert_eq!(receipt.identity(), &identity());
    assert_eq!(receipt.request_id(), approved.id);
    assert_eq!(receipt.catalog_fingerprint(), fixture.catalog.fingerprint());
    assert_eq!(receipt.scope(), &fixture.catalog.operations()[0]);
    assert_eq!(receipt.grant_expires_at_ms(), 1100);
    assert_eq!(receipt.decision().kind, DecisionKind::Approved);
    fixture
        .second
        .check_receipt(&receipt, &fixture.catalog)
        .unwrap();
    assert!(
        fixture
            .store
            .admit(&identity(), &fixture.catalog, "fs.list", &json!({}))
            .is_err()
    );
    assert!(
        fixture
            .store
            .admit(
                &identity(),
                &fixture.catalog,
                "fs.read",
                &json!({"path":"other.txt"})
            )
            .is_err()
    );
    let wrong_catalog = OperationCatalog::bind(
        host_identity(fixture.temp.path(), &HostConfig::default(), "other-policy").unwrap(),
        &cayw_host::Host::new(
            fixture.temp.path(),
            Arc::new(Allow),
            Arc::new(Audit::default()),
            HostConfig::default(),
        )
        .unwrap()
        .capabilities(),
        fixture.catalog.operations(),
    )
    .unwrap();
    assert!(
        fixture
            .second
            .check_receipt(&receipt, &wrong_catalog)
            .is_err()
    );
    grant(&fixture.second, &fixture.catalog);
    assert!(
        fixture
            .store
            .check_receipt(&receipt, &fixture.catalog)
            .is_err()
    );
    let current = fixture
        .store
        .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
        .unwrap();
    assert_ne!(current.request_id(), receipt.request_id());
    fixture
        .second
        .revoke(&identity(), current.request_id(), &actor(), "withdrawn")
        .unwrap();
    assert!(
        fixture
            .store
            .check_receipt(&current, &fixture.catalog)
            .is_err()
    );
}

#[test]
fn direct_task_end_invalidates_the_original_receipt_on_another_connection() {
    let fixture = Fixture::new();
    fixture.grant();
    let receipt = fixture
        .store
        .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
        .unwrap();
    fixture
        .second
        .end_task(
            &identity(),
            TaskOutcome::Completed,
            &actor(),
            "work completed",
        )
        .unwrap();
    assert!(
        fixture
            .store
            .check_receipt(&receipt, &fixture.catalog)
            .is_err()
    );
    assert_eq!(
        fixture
            .store
            .status(&identity(), receipt.request_id())
            .unwrap()
            .state,
        RequestState::TaskEnded
    );
}

#[test]
fn receipt_expires_and_attempt_replacement_never_restores_it() {
    let fixture = Fixture::new();
    fixture.grant();
    let receipt = fixture
        .store
        .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
        .unwrap();
    fixture.clock.0.store(1100, Ordering::SeqCst);
    assert!(
        fixture
            .second
            .check_receipt(&receipt, &fixture.catalog)
            .is_err()
    );
    fixture.clock.0.store(900, Ordering::SeqCst);
    assert!(
        fixture
            .store
            .check_receipt(&receipt, &fixture.catalog)
            .is_err()
    );
    fixture.grant();
    let receipt = fixture
        .store
        .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
        .unwrap();
    let next = fixture
        .second
        .replace_attempt(
            &identity(),
            "next-attempt",
            &fixture.catalog,
            &actor(),
            "new worker",
        )
        .unwrap();
    assert!(
        fixture
            .store
            .check_receipt(&receipt, &fixture.catalog)
            .is_err()
    );
    assert!(
        fixture
            .store
            .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
            .is_err()
    );
    assert!(
        fixture
            .store
            .admit(&next, &fixture.catalog, "fs.read", &input().arguments)
            .is_err()
    );
}

#[test]
fn closing_is_idempotent_revokes_admissions_and_can_finish_from_another_connection() {
    let fixture = Fixture::new();
    let approved = fixture.grant();
    let receipt = fixture
        .store
        .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
        .unwrap();
    let closing = fixture
        .second
        .begin_closing(&identity(), &actor(), "drain running work")
        .unwrap();
    assert!(matches!(closing, TaskState::Closing { .. }));
    assert_eq!(
        fixture
            .store
            .begin_closing(&identity(), &actor(), "duplicate close")
            .unwrap(),
        closing
    );
    assert_eq!(
        fixture
            .store
            .events(&identity(), 0, 64)
            .unwrap()
            .iter()
            .filter(|event| event.kind == "task_closing")
            .count(),
        1
    );
    assert!(
        fixture
            .store
            .check_task(&identity(), &fixture.catalog)
            .is_err()
    );
    assert!(
        fixture
            .store
            .check_receipt(&receipt, &fixture.catalog)
            .is_err()
    );
    assert!(
        fixture
            .store
            .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
            .is_err()
    );
    assert!(
        fixture
            .store
            .request(&identity(), &fixture.catalog, input())
            .is_err()
    );
    assert!(
        fixture
            .store
            .replace_attempt(&identity(), "next", &fixture.catalog, &actor(), "restart")
            .is_err()
    );
    assert_eq!(
        fixture
            .store
            .status(&identity(), &approved.id)
            .unwrap()
            .state,
        RequestState::Revoked
    );
    let stale = TaskIdentity::new("session", "task", "old-attempt").unwrap();
    assert!(
        fixture
            .store
            .begin_closing(&stale, &actor(), "stale close")
            .is_err()
    );
    assert!(
        fixture
            .store
            .end_task(&stale, TaskOutcome::Cancelled, &actor(), "stale end")
            .is_err()
    );
    let ended = fixture
        .store
        .end_task(
            &identity(),
            TaskOutcome::Cancelled,
            &actor(),
            "all running work settled",
        )
        .unwrap();
    assert!(matches!(
        ended,
        TaskState::Ended {
            outcome: TaskOutcome::Cancelled,
            ..
        }
    ));
    assert!(
        fixture
            .second
            .begin_closing(&identity(), &actor(), "late close")
            .is_err()
    );
    assert!(
        fixture
            .second
            .end_task(
                &identity(),
                TaskOutcome::Completed,
                &actor(),
                "duplicate end"
            )
            .is_err()
    );
    let reopened = RequestStore::open_with_clock(
        fixture.store.path(),
        fixture.store.config().clone(),
        fixture.clock.clone(),
    )
    .unwrap();
    assert_eq!(reopened.task_state(&identity()).unwrap(), ended);
    assert!(reopened.check_receipt(&receipt, &fixture.catalog).is_err());
}

#[test]
fn event_database_failure_rolls_back_business_state_but_commits_observed_clock() {
    let fixture = Fixture::new();
    let approved = fixture.grant();
    let receipt = fixture
        .store
        .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
        .unwrap();
    let database = rusqlite::Connection::open(fixture.store.path()).unwrap();
    let events: i64 = database
        .query_row("SELECT count(*) FROM request_events", [], |row| row.get(0))
        .unwrap();
    database.execute_batch("CREATE TRIGGER fail_expiry_event BEFORE INSERT ON request_events BEGIN SELECT RAISE(ABORT,'injected event failure'); END;").unwrap();
    fixture.clock.0.store(1100, Ordering::SeqCst);
    assert!(matches!(
        fixture.store.check_receipt(&receipt, &fixture.catalog),
        Err(RequestError::Database(_))
    ));
    let highwater: i64 = database
        .query_row("SELECT last_now_ms FROM request_meta", [], |row| row.get(0))
        .unwrap();
    assert_eq!(highwater, 1100);
    let after: i64 = database
        .query_row("SELECT count(*) FROM request_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, events);
    let stored: Vec<u8> = database
        .query_row(
            "SELECT data FROM capability_requests WHERE id=?1",
            [&approved.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&stored).unwrap()["record"]["state"],
        "granted"
    );
    fixture.clock.0.store(900, Ordering::SeqCst);
    // An unavailable audit cannot be hidden as a normal denial or permission.
    assert!(matches!(
        fixture
            .second
            .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments),
        Err(RequestError::AuthorityStopped)
    ));
    database
        .execute_batch("DROP TRIGGER fail_expiry_event")
        .unwrap();
    assert!(matches!(
        fixture.second.check_receipt(&receipt, &fixture.catalog),
        Err(RequestError::AuthorityStopped)
    ));
    assert!(
        RequestStore::open_with_clock(
            fixture.store.path(),
            fixture.store.config().clone(),
            fixture.clock.clone()
        )
        .is_err(),
        "removing a persistence fault must not recover authority"
    );
}

#[tokio::test]
async fn clock_persistence_failure_never_dispatches_or_issues_a_receipt() {
    let fixture = Fixture::new();
    fixture.grant();
    let audit = Arc::new(Audit::default());
    let host = RequestHost::new(
        fixture.store.clone(),
        identity(),
        fixture.config(Arc::new(Allow), audit.clone()),
    )
    .unwrap();
    let database = rusqlite::Connection::open(fixture.store.path()).unwrap();
    database.execute_batch("CREATE TRIGGER fail_highwater BEFORE UPDATE OF last_now_ms ON request_meta BEGIN SELECT RAISE(ABORT,'injected clock persistence failure'); END;").unwrap();
    fixture.clock.0.store(1100, Ordering::SeqCst);
    assert!(matches!(
        fixture
            .store
            .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments),
        Err(RequestError::Database(_))
    ));
    assert!(
        host.call("fs.read", input().arguments, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(audit.started.load(Ordering::SeqCst), 0);
    database
        .execute_batch("DROP TRIGGER fail_highwater")
        .unwrap();
    assert!(
        fixture
            .store
            .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
            .is_err()
    );
}

struct ClosingAudit {
    audit: Audit,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl AuditSink for ClosingAudit {
    async fn started(
        &self,
        name: &str,
        args: &Value,
    ) -> std::result::Result<String, CapabilityError> {
        self.audit.started(name, args).await
    }
    async fn settled(
        &self,
        id: &str,
        result: &std::result::Result<Value, CapabilityError>,
    ) -> std::result::Result<(), CapabilityError> {
        self.entered.notify_one();
        self.resume.notified().await;
        self.audit.settled(id, result).await
    }
    fn dropped(&self, id: &str) -> std::result::Result<(), CapabilityError> {
        self.audit.dropped(id)
    }
}
#[tokio::test]
async fn closing_preserves_settlement_of_the_same_already_started_host_future() {
    let fixture = Fixture::new();
    fixture.grant();
    let receipt = fixture
        .store
        .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
        .unwrap();
    let audit = Arc::new(ClosingAudit {
        audit: Audit::default(),
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let host = RequestHost::new(
        fixture.store.clone(),
        identity(),
        fixture.config(Arc::new(Allow), audit.clone()),
    )
    .unwrap();
    let call = host.call("fs.read", input().arguments, CancellationToken::new());
    let closing = async {
        audit.entered.notified().await;
        fixture
            .second
            .begin_closing(
                &identity(),
                &actor(),
                "stop admissions and drain settlement",
            )
            .unwrap();
        assert!(
            fixture
                .store
                .check_receipt(&receipt, &fixture.catalog)
                .is_err()
        );
        audit.resume.notify_one();
        assert!(
            host.call("fs.read", input().arguments, CancellationToken::new())
                .await
                .is_err()
        );
        assert!(
            host.call("fs.list", json!({"path":"."}), CancellationToken::new())
                .await
                .is_err()
        );
    };
    let (result, ()) = tokio::join!(call, closing);
    assert_eq!(result.unwrap()["content"], "protected real content\n");
    assert_eq!(audit.audit.started.load(Ordering::SeqCst), 1);
    assert_eq!(audit.audit.settled.lock().unwrap().len(), 1);
    assert!(audit.audit.settled.lock().unwrap()[0].is_ok());
    assert!(!host.outcome_unknown());
    fixture
        .store
        .end_task(
            &identity(),
            TaskOutcome::Cancelled,
            &actor(),
            "settlement is complete",
        )
        .unwrap();
}

#[tokio::test]
async fn per_call_host_scopes_share_the_original_call_budget() {
    let fixture = Fixture::new();
    let audit = Arc::new(Audit::default());
    let mut config = fixture.config(Arc::new(Allow), audit.clone());
    config.host.max_calls = 1;
    let catalog = config.catalog().unwrap();
    let current = fixture
        .store
        .replace_attempt(
            &identity(),
            "budget-attempt",
            &catalog,
            &actor(),
            "configure shared budget",
        )
        .unwrap();
    let host = RequestHost::new(fixture.store.clone(), current, config).unwrap();
    assert!(
        host.call("fs.list", json!({"path":"."}), CancellationToken::new())
            .await
            .is_ok()
    );
    let error = host
        .call("fs.list", json!({"path":"."}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.message.contains("budget exhausted"), "{error:?}");
    assert_eq!(audit.started.load(Ordering::SeqCst), 1);
    assert!(
        host.call(
            "capability.request",
            serde_json::to_value(input()).unwrap(),
            CancellationToken::new()
        )
        .await
        .is_err(),
        "metadata must consume the same Host call budget"
    );
    assert_eq!(audit.started.load(Ordering::SeqCst), 1);
}

struct FailingSettlement(Audit);
#[async_trait::async_trait]
impl AuditSink for FailingSettlement {
    async fn started(
        &self,
        name: &str,
        args: &Value,
    ) -> std::result::Result<String, CapabilityError> {
        self.0.started(name, args).await
    }
    async fn settled(
        &self,
        _: &str,
        _: &std::result::Result<Value, CapabilityError>,
    ) -> std::result::Result<(), CapabilityError> {
        Err(CapabilityError::new("injected persistence failure"))
    }
    fn dropped(&self, id: &str) -> std::result::Result<(), CapabilityError> {
        self.0.dropped(id)
    }
}
#[tokio::test]
async fn per_call_host_scopes_share_unknown_outcome_and_stop_later_calls() {
    let fixture = Fixture::new();
    let audit = Arc::new(FailingSettlement(Audit::default()));
    let host = RequestHost::new(
        fixture.store.clone(),
        identity(),
        fixture.config(Arc::new(Allow), audit.clone()),
    )
    .unwrap();
    assert!(
        host.call("fs.list", json!({"path":"."}), CancellationToken::new())
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert!(host.outcome_unknown());
    assert!(host.audit_failure().is_some());
    assert!(
        host.call("fs.list", json!({"path":"."}), CancellationToken::new())
            .await
            .unwrap_err()
            .outcome_unknown
    );
    // A missing protected grant must not mask the Host's existing unknown fence.
    assert!(
        host.call("fs.read", input().arguments, CancellationToken::new())
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert!(
        host.call(
            "capability.request",
            serde_json::to_value(input()).unwrap(),
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .outcome_unknown,
        "metadata cannot bypass the same Host unknown fence"
    );
    assert_eq!(audit.0.started.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn denied_protected_calls_still_use_the_shared_host_admission_budget() {
    let fixture = Fixture::new();
    let audit = Arc::new(Audit::default());
    let mut config = fixture.config(Arc::new(Allow), audit.clone());
    config.host.max_calls = 1;
    let catalog = config.catalog().unwrap();
    let current = fixture
        .store
        .replace_attempt(
            &identity(),
            "denied-budget-attempt",
            &catalog,
            &actor(),
            "configure shared budget",
        )
        .unwrap();
    let host = RequestHost::new(fixture.store.clone(), current.clone(), config).unwrap();
    assert!(
        host.call("fs.read", input().arguments, CancellationToken::new())
            .await
            .is_err()
    );
    let request = fixture.store.request(&current, &catalog, input()).unwrap();
    let ticket = fixture
        .store
        .evaluate(&current, &request.id, &actor())
        .unwrap();
    fixture
        .store
        .approve(&ticket, &catalog, 100, "verified after initial denial")
        .unwrap();
    let error = host
        .call("fs.read", input().arguments, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.message.contains("budget exhausted"), "{error:?}");
    assert_eq!(audit.started.load(Ordering::SeqCst), 0);
}

struct Deny;
#[async_trait::async_trait]
impl PermissionGate for Deny {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        Err(CapabilityError::new("upstream denied the exact file"))
    }
}
#[tokio::test]
async fn a_live_receipt_never_overrides_upstream_permission_denial() {
    let fixture = Fixture::new();
    fixture.grant();
    let audit = Arc::new(Audit::default());
    let host = RequestHost::new(
        fixture.store.clone(),
        identity(),
        fixture.config(Arc::new(Deny), audit.clone()),
    )
    .unwrap();
    let error = host
        .call("fs.read", input().arguments, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.message, "upstream denied the exact file");
    assert!(
        host.call(
            "capability.request",
            serde_json::to_value(input()).unwrap(),
            CancellationToken::new()
        )
        .await
        .is_err(),
        "metadata needs the same upstream permission"
    );
    assert_eq!(audit.started.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .store
            .admit(&identity(), &fixture.catalog, "fs.read", &input().arguments)
            .is_ok()
    );
}

struct ReplaceDuringPermission {
    store: Arc<RequestStore>,
    catalog: OperationCatalog,
    once: AtomicBool,
}
#[async_trait::async_trait]
impl PermissionGate for ReplaceDuringPermission {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        if !self.once.swap(true, Ordering::SeqCst) {
            grant(&self.store, &self.catalog);
        }
        Ok(())
    }
}
struct ReplaceDuringAudit {
    store: Arc<RequestStore>,
    catalog: OperationCatalog,
    once: AtomicBool,
    audit: Arc<Audit>,
}
#[async_trait::async_trait]
impl AuditSink for ReplaceDuringAudit {
    async fn started(
        &self,
        name: &str,
        args: &Value,
    ) -> std::result::Result<String, CapabilityError> {
        let id = self.audit.started(name, args).await?;
        if !self.once.swap(true, Ordering::SeqCst) {
            grant(&self.store, &self.catalog);
        }
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &std::result::Result<Value, CapabilityError>,
    ) -> std::result::Result<(), CapabilityError> {
        self.audit.settled(id, result).await
    }
    fn dropped(&self, id: &str) -> std::result::Result<(), CapabilityError> {
        self.audit.dropped(id)
    }
}

#[tokio::test]
async fn newer_grant_during_permission_cannot_replace_the_admitted_grant() {
    let fixture = Fixture::new();
    let old = fixture.grant();
    let audit = Arc::new(Audit::default());
    let gate = Arc::new(ReplaceDuringPermission {
        store: fixture.second.clone(),
        catalog: fixture.catalog.clone(),
        once: AtomicBool::new(false),
    });
    let host = RequestHost::new(
        fixture.store.clone(),
        identity(),
        fixture.config(gate, audit.clone()),
    )
    .unwrap();
    assert!(
        host.call("fs.read", input().arguments, CancellationToken::new())
            .await
            .is_err(),
        "the pending call must retain the superseded request ID"
    );
    assert_eq!(
        fixture.store.status(&identity(), &old.id).unwrap().state,
        RequestState::Superseded
    );
    assert_eq!(audit.started.load(Ordering::SeqCst), 0);
    assert_eq!(
        host.call("fs.read", input().arguments, CancellationToken::new())
            .await
            .unwrap()["content"],
        "protected real content\n"
    );
}

#[tokio::test]
async fn newer_grant_during_audit_cannot_replace_the_admitted_grant() {
    let fixture = Fixture::new();
    fixture.grant();
    let audit = Arc::new(Audit::default());
    let replacing = Arc::new(ReplaceDuringAudit {
        store: fixture.second.clone(),
        catalog: fixture.catalog.clone(),
        once: AtomicBool::new(false),
        audit: audit.clone(),
    });
    let host = RequestHost::new(
        fixture.store.clone(),
        identity(),
        fixture.config(Arc::new(Allow), replacing),
    )
    .unwrap();
    assert!(
        host.call("fs.read", input().arguments, CancellationToken::new())
            .await
            .is_err(),
        "audit waiting must not switch to a replacement grant"
    );
    assert_eq!(audit.started.load(Ordering::SeqCst), 1);
    assert!(audit.settled.lock().unwrap()[0].is_err());
    assert!(!host.outcome_unknown());
    assert_eq!(
        host.call("fs.read", input().arguments, CancellationToken::new())
            .await
            .unwrap()["content"],
        "protected real content\n"
    );
}
