use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, HostConfig, PermissionGate, Profile, ProfileKind};
use cayw_requests::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct TestClock(AtomicU64);
impl TestClock {
    fn at(ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(ms)))
    }
    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now_ms(&self) -> Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}
fn actor() -> HostActor {
    HostActor::new("trusted-test-evaluator").unwrap()
}
fn identity() -> TaskIdentity {
    TaskIdentity::new("session-a", "task-a", "attempt-a").unwrap()
}
fn input() -> RequestInput {
    RequestInput {
        purpose: "Run the project's selected test".into(),
        gap: "This task needs the host-selected test profile".into(),
        operation_id: "project-tests".into(),
        capability: "test.run".into(),
        arguments: json!({"profile":"project-test"}),
    }
}
fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "test.run".into(),
        description: "host test".into(),
        parameters: json!({"type":"object","additionalProperties":false,"required":["profile"],"properties":{"profile":{"type":"string","enum":["project-test"]}}}),
    }]
}
fn host_identity(
    root: &std::path::Path,
    config: &HostConfig,
    policy: &str,
) -> Result<HostIdentity> {
    let host = cayw_host::Host::new(
        root,
        Arc::new(OrdinaryGate {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(Audit::default()),
        config.clone(),
    )
    .unwrap();
    HostIdentity::from_host(&host, config, policy)
}
struct Fixture {
    _temp: tempfile::TempDir,
    path: PathBuf,
    root: PathBuf,
    clock: Arc<TestClock>,
    config: RequestConfig,
    catalog: OperationCatalog,
}
impl Fixture {
    fn new() -> Self {
        Self::with_config(RequestConfig::default())
    }
    fn with_config(config: RequestConfig) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let host = host_identity(&root, &HostConfig::default(), "policy-a").unwrap();
        let catalog = OperationCatalog::bind(
            host,
            &definitions(),
            vec![OperationScope::exact(
                "project-tests",
                "test.run",
                json!({"profile":"project-test"}),
                "device-test-profile-v1",
            )],
        )
        .unwrap();
        Self {
            path: temp.path().join("requests.sqlite"),
            _temp: temp,
            root,
            clock: TestClock::at(1000),
            config,
            catalog,
        }
    }
    fn open(&self) -> Arc<RequestStore> {
        Arc::new(
            RequestStore::open_with_clock(&self.path, self.config.clone(), self.clock.clone())
                .unwrap(),
        )
    }
    fn start(&self, store: &RequestStore, intent: TaskIntent) {
        store
            .start_task(&identity(), intent, &self.catalog, &actor())
            .unwrap();
    }
    fn eligible(&self, store: &RequestStore) -> RequestRecord {
        let first = store.request(&identity(), &self.catalog, input()).unwrap();
        if first.state == RequestState::AwaitingEvaluation {
            first
        } else {
            store.request(&identity(), &self.catalog, input()).unwrap()
        }
    }
    fn grant(&self, store: &RequestStore, lifetime: u64) -> RequestRecord {
        let record = self.eligible(store);
        let ticket = store.evaluate(&identity(), &record.id, &actor()).unwrap();
        store
            .approve(
                &ticket,
                &self.catalog,
                lifetime,
                "host verified the exact project profile and task need",
            )
            .unwrap()
    }
}

#[test]
fn threshold_is_configurable_and_still_requires_host_decision() {
    let fixture = Fixture::with_config(RequestConfig {
        repeated_valid_threshold: 3,
        ..RequestConfig::default()
    });
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    for expected in [
        RequestState::Requested,
        RequestState::Requested,
        RequestState::AwaitingEvaluation,
    ] {
        assert_eq!(
            store
                .request(&identity(), &fixture.catalog, input())
                .unwrap()
                .state,
            expected
        );
        assert!(
            store
                .admit(
                    &identity(),
                    &fixture.catalog,
                    "test.run",
                    &input().arguments
                )
                .is_err()
        );
    }
}

#[test]
fn repetition_requests_evaluation_only_and_invalid_requests_never_count() {
    let fixture = Fixture::new();
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    for _ in 0..4 {
        let mut invalid = input();
        invalid.gap.clear();
        let record = store
            .request(&identity(), &fixture.catalog, invalid)
            .unwrap();
        assert_eq!(record.state, RequestState::Invalid);
        assert_eq!(record.valid_request_count, 0);
        assert!(
            record
                .validity_reasons
                .contains(&ValidityReason::ConcreteGapMissing)
        );
        assert!(store.evaluate(&identity(), &record.id, &actor()).is_err());
    }
    let mut wrong = input();
    wrong.arguments = json!({"profile":"unconfigured-shell"});
    assert_eq!(
        store
            .request(&identity(), &fixture.catalog, wrong)
            .unwrap()
            .state,
        RequestState::Invalid
    );
    let first = store
        .request(&identity(), &fixture.catalog, input())
        .unwrap();
    assert_eq!(first.state, RequestState::Requested);
    assert_eq!(first.valid_request_count, 1);
    let second = store
        .request(&identity(), &fixture.catalog, input())
        .unwrap();
    assert_eq!(second.state, RequestState::AwaitingEvaluation);
    assert_eq!(second.valid_request_count, 2);
    assert_eq!(
        store.status(&identity(), &first.id).unwrap().state,
        RequestState::Superseded
    );
    assert!(
        store
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    let ticket = store.evaluate(&identity(), &second.id, &actor()).unwrap();
    assert!(store.evaluate(&identity(), &second.id, &actor()).is_err());
    assert!(
        store
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    let approved = store
        .approve(&ticket, &fixture.catalog, 100, "host verified project need")
        .unwrap();
    assert_eq!(approved.decision.as_ref().unwrap().actor, actor().as_str());
    store
        .admit(
            &identity(),
            &fixture.catalog,
            "test.run",
            &input().arguments,
        )
        .unwrap();
    assert!(
        store
            .approve(&ticket, &fixture.catalog, 100, "duplicate")
            .is_err()
    );
    assert!(store.deny(&ticket, "late denial").is_err());
    assert!(
        store
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &json!({"profile":"project-test","filter":"other"})
            )
            .is_err()
    );
}

#[test]
fn explicit_host_user_intent_and_supported_profile_allow_first_evaluation() {
    let fixture = Fixture::new();
    let store = fixture.open();
    fixture.start(
        &store,
        TaskIntent::ExplicitProjectTestOrScript {
            user_instruction: "Run the repository test profile".into(),
        },
    );
    let first = store
        .request(&identity(), &fixture.catalog, input())
        .unwrap();
    assert_eq!(first.state, RequestState::AwaitingEvaluation);
    assert!(
        store
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    let ticket = store.evaluate(&identity(), &first.id, &actor()).unwrap();
    store
        .approve(
            &ticket,
            &fixture.catalog,
            500,
            "host verified the user's exact test instruction",
        )
        .unwrap();
    let events = store.events(&identity(), 0, 64).unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(
        events[0].payload["intent"]["user_instruction"],
        "Run the repository test profile"
    );
    assert_eq!(
        events[1].payload["validity_reasons"],
        json!(["exact_host_operation"])
    );
    assert_eq!(events[3].payload["decision"]["kind"], "approved");
}

#[test]
fn revoke_and_task_end_survive_reopen_and_independent_connections() {
    let fixture = Fixture::new();
    let first = fixture.open();
    let second = fixture.open();
    fixture.start(&first, TaskIntent::General);
    let approved = fixture.grant(&first, 1000);
    second
        .admit(
            &identity(),
            &fixture.catalog,
            "test.run",
            &input().arguments,
        )
        .unwrap();
    second
        .revoke(
            &identity(),
            &approved.id,
            &actor(),
            "host revoked this task scope",
        )
        .unwrap();
    assert!(
        first
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    assert!(
        first
            .revoke(&identity(), &approved.id, &actor(), "duplicate revoke")
            .is_err()
    );
    drop(first);
    drop(second);
    let reopened = fixture.open();
    assert_eq!(
        reopened.status(&identity(), &approved.id).unwrap().state,
        RequestState::Revoked
    );
    assert!(
        reopened
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    let approved = fixture.grant(&reopened, 1000);
    let pending = reopened
        .request(&identity(), &fixture.catalog, input())
        .unwrap();
    let ticket = reopened
        .evaluate(&identity(), &pending.id, &actor())
        .unwrap();
    reopened
        .end_task(
            &identity(),
            TaskOutcome::Completed,
            &actor(),
            "all task work finished",
        )
        .unwrap();
    assert!(
        reopened
            .approve(&ticket, &fixture.catalog, 1000, "late completion")
            .is_err()
    );
    assert!(
        reopened
            .request(&identity(), &fixture.catalog, input())
            .is_err()
    );
    assert!(
        reopened
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    assert!(
        reopened
            .start_task(&identity(), TaskIntent::General, &fixture.catalog, &actor())
            .is_err()
    );
    drop(reopened);
    let reopened = fixture.open();
    assert!(matches!(
        reopened.task_state(&identity()).unwrap(),
        TaskState::Ended { .. }
    ));
    assert!(
        reopened
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    assert_eq!(
        reopened.status(&identity(), &approved.id).unwrap().state,
        RequestState::Superseded
    );
    assert_eq!(
        reopened.status(&identity(), &pending.id).unwrap().state,
        RequestState::TaskEnded
    );
}

#[test]
fn expiration_and_clock_rollback_cannot_revive_a_grant_or_evaluation() {
    let fixture = Fixture::new();
    let first = fixture.open();
    fixture.start(&first, TaskIntent::General);
    let approved = fixture.grant(&first, 100);
    fixture.clock.set(1100);
    assert_eq!(first.expire(&identity()).unwrap(), 1);
    assert_eq!(
        first.status(&identity(), &approved.id).unwrap().state,
        RequestState::Expired
    );
    fixture.clock.set(900);
    drop(first);
    let reopened = fixture.open();
    assert!(
        reopened
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    let pending = fixture.eligible(&reopened);
    let ticket = reopened
        .evaluate(&identity(), &pending.id, &actor())
        .unwrap();
    fixture
        .clock
        .set(1100 + fixture.config.evaluation_timeout_ms);
    assert!(
        reopened
            .approve(&ticket, &fixture.catalog, 100, "late evaluation")
            .is_err()
    );
    assert_eq!(
        reopened.status(&identity(), &pending.id).unwrap().state,
        RequestState::Expired
    );
    fixture.clock.set(800);
    assert!(
        reopened
            .approve(&ticket, &fixture.catalog, 100, "clock rollback")
            .is_err()
    );
}

#[test]
fn old_attempt_and_superseded_evaluator_results_never_revive() {
    let fixture = Fixture::new();
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    let pending = fixture.eligible(&store);
    let old = store.evaluate(&identity(), &pending.id, &actor()).unwrap();
    let replacement = store
        .request(&identity(), &fixture.catalog, input())
        .unwrap();
    assert!(
        store
            .approve(&old, &fixture.catalog, 500, "old request result")
            .is_err()
    );
    let current = store
        .evaluate(&identity(), &replacement.id, &actor())
        .unwrap();
    let next = store
        .replace_attempt(
            &identity(),
            "attempt-b",
            &fixture.catalog,
            &actor(),
            "worker was replaced",
        )
        .unwrap();
    assert!(
        store
            .approve(&current, &fixture.catalog, 500, "old attempt result")
            .is_err()
    );
    assert!(
        store
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    assert!(
        store
            .admit(&next, &fixture.catalog, "test.run", &input().arguments)
            .is_err()
    );
    assert!(
        store
            .replace_attempt(
                &next,
                "attempt-a",
                &fixture.catalog,
                &actor(),
                "reuse old ID"
            )
            .is_err()
    );
    let other_session = TaskIdentity::new("other-session", "task-a", "attempt-b").unwrap();
    assert!(
        store
            .request(&other_session, &fixture.catalog, input())
            .is_err()
    );
    let other_task = TaskIdentity::new("session-a", "task-b", "attempt-a").unwrap();
    store
        .start_task(&other_task, TaskIntent::General, &fixture.catalog, &actor())
        .unwrap();
    assert!(store.status(&other_task, &replacement.id).is_err());
}

#[test]
fn simultaneous_evaluators_on_separate_connections_only_issue_one_ticket() {
    let fixture = Fixture::new();
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    let pending = fixture.eligible(&store);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let a = fixture.open();
    let b = fixture.open();
    let handles: Vec<_> = [a, b]
        .into_iter()
        .map(|store| {
            let barrier = barrier.clone();
            let id = pending.id.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.evaluate(&identity(), &id, &actor())
            })
        })
        .collect();
    let tickets: Vec<_> = handles
        .into_iter()
        .filter_map(|h| h.join().unwrap().ok())
        .collect();
    assert_eq!(tickets.len(), 1);
    store
        .approve(
            &tickets[0],
            &fixture.catalog,
            100,
            "one host evaluator wins",
        )
        .unwrap();
}

#[test]
fn schemas_and_exact_host_policy_are_bound_and_tools_cannot_self_grant() {
    let fixture = Fixture::new();
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    let tools = RequestTools::new(store.clone(), identity(), fixture.catalog.clone()).unwrap();
    assert_eq!(
        tools
            .definitions()
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["capability.request", "capability.status"]
    );
    let mut forged = serde_json::to_value(input()).unwrap();
    forged["approved"] = json!(true);
    assert!(tools.invoke("capability.request", forged).is_err());
    let mut forged = serde_json::to_value(input()).unwrap();
    forged["task_id"] = json!("other-task");
    assert!(tools.invoke("capability.request", forged).is_err());
    assert!(
        tools
            .invoke("capability.approve", json!({"approved":true}))
            .is_err()
    );
    assert!(
        tools
            .invoke(
                "capability.status",
                json!({"request_id":"absent","actor":"host"})
            )
            .is_err()
    );
    let mut unknown = input();
    unknown.capability = "shell.run".into();
    assert_eq!(
        tools
            .invoke("capability.request", serde_json::to_value(unknown).unwrap())
            .unwrap()["state"],
        "invalid"
    );
    let record = fixture.eligible(&store);
    let ticket = store.evaluate(&identity(), &record.id, &actor()).unwrap();
    let host = host_identity(
        &fixture.root,
        &HostConfig::default(),
        "changed-device-policy",
    )
    .unwrap();
    let changed =
        OperationCatalog::bind(host, &definitions(), fixture.catalog.operations()).unwrap();
    assert!(matches!(
        store.approve(&ticket, &changed, 100, "wrong authority"),
        Err(RequestError::PolicyMismatch)
    ));
    let mut changed_definitions = definitions();
    changed_definitions[0].parameters["properties"]["profile"]["description"] =
        json!("changed schema contract");
    let changed_schema = OperationCatalog::bind(
        host_identity(&fixture.root, &HostConfig::default(), "policy-a").unwrap(),
        &changed_definitions,
        fixture.catalog.operations(),
    )
    .unwrap();
    assert!(
        store
            .approve(&ticket, &changed_schema, 100, "schema changed")
            .is_err()
    );
    store
        .approve(&ticket, &fixture.catalog, 100, "actual unchanged contract")
        .unwrap();
    assert!(
        store
            .admit(&identity(), &changed, "test.run", &input().arguments)
            .is_err()
    );
    assert!(
        OperationCatalog::bind(
            host_identity(&fixture.root, &HostConfig::default(), "policy-a").unwrap(),
            &definitions(),
            vec![OperationScope::exact(
                "shell",
                "shell.run",
                json!({}),
                "device-policy"
            )]
        )
        .is_err()
    );
}

#[test]
fn budgets_preserve_space_for_revoke_task_end_and_immutable_events() {
    let config = RequestConfig {
        max_requests_per_task: 2,
        max_events: 15,
        ..RequestConfig::default()
    };
    let fixture = Fixture::with_config(config);
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    let approved = fixture.grant(&store, 1000);
    assert!(matches!(
        store.request(&identity(), &fixture.catalog, input()),
        Err(RequestError::Budget)
    ));
    store
        .revoke(
            &identity(),
            &approved.id,
            &actor(),
            "budget exhausted but revoke must commit",
        )
        .unwrap();
    store
        .end_task(
            &identity(),
            TaskOutcome::Cancelled,
            &actor(),
            "budget exhausted but end must commit",
        )
        .unwrap();
    let before = store.events(&identity(), 0, 64).unwrap();
    let connection = rusqlite::Connection::open(&fixture.path).unwrap();
    assert!(
        connection
            .execute("DELETE FROM request_events", [])
            .is_err()
    );
    assert!(
        connection
            .execute("UPDATE request_events SET actor='forged'", [])
            .is_err()
    );
    assert!(connection.execute("DELETE FROM request_tasks", []).is_err());
    assert_eq!(
        before.len(),
        store.events(&identity(), 0, 64).unwrap().len()
    );
    assert!(before.iter().any(|e| e.kind == "task_ended"));
}

#[test]
fn event_failure_rolls_back_projection_and_oversized_input_never_persists() {
    let fixture = Fixture::new();
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    let mut oversized = input();
    oversized.gap = "x".repeat(2049);
    assert!(
        store
            .request(&identity(), &fixture.catalog, oversized)
            .is_err()
    );
    let before = store.events(&identity(), 0, 64).unwrap().len();
    let connection = rusqlite::Connection::open(&fixture.path).unwrap();
    connection.execute_batch("CREATE TRIGGER test_fail_event BEFORE INSERT ON request_events BEGIN SELECT RAISE(ABORT,'injected event failure'); END;").unwrap();
    assert!(
        store
            .request(&identity(), &fixture.catalog, input())
            .is_err()
    );
    let after: i64 = connection
        .query_row("SELECT count(*) FROM request_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after as usize, before);
    connection
        .execute_batch("DROP TRIGGER test_fail_event")
        .unwrap();
    assert!(matches!(
        store.request(&identity(), &fixture.catalog, input()),
        Err(RequestError::AuthorityStopped)
    ));
    assert!(RequestStore::open(&fixture.path, fixture.config.clone()).is_err());

    // Recovery is an explicit new authority after host reconciliation. Removing
    // a transient SQLite fault cannot revive the failed authority's grants.
    let replacement = RequestStore::open_with_clock(
        fixture._temp.path().join("replacement.sqlite"),
        fixture.config.clone(),
        fixture.clock.clone(),
    )
    .unwrap();
    fixture.start(&replacement, TaskIntent::General);
    assert_eq!(
        replacement
            .request(&identity(), &fixture.catalog, input())
            .unwrap()
            .valid_request_count,
        1
    );
    let mismatch = RequestConfig {
        repeated_valid_threshold: 3,
        ..fixture.config.clone()
    };
    assert!(matches!(
        RequestStore::open(replacement.path(), mismatch),
        Err(RequestError::PolicyMismatch)
    ));
}

#[test]
fn rejected_request_preserves_observed_expiry_across_clock_rollback() {
    let fixture = Fixture::with_config(RequestConfig {
        max_requests_per_task: 2,
        ..RequestConfig::default()
    });
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    let approved = fixture.grant(&store, 100);
    fixture.clock.set(1100);
    assert!(matches!(
        store.request(&identity(), &fixture.catalog, input()),
        Err(RequestError::Budget)
    ));
    fixture.clock.set(900);
    drop(store);
    let reopened = fixture.open();
    assert!(
        reopened
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
    assert_eq!(
        reopened.status(&identity(), &approved.id).unwrap().state,
        RequestState::Expired
    );
}

#[test]
fn revoked_evaluation_ticket_and_clock_overflow_never_grant() {
    let fixture = Fixture::new();
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    let pending = fixture.eligible(&store);
    let ticket = store.evaluate(&identity(), &pending.id, &actor()).unwrap();
    store
        .revoke(
            &identity(),
            &pending.id,
            &actor(),
            "host withdrew evaluation",
        )
        .unwrap();
    assert!(
        store
            .approve(&ticket, &fixture.catalog, 100, "late approval")
            .is_err()
    );
    fixture.clock.set(i64::MAX as u64);
    assert!(matches!(
        store.request(&identity(), &fixture.catalog, input()),
        Err(RequestError::Clock)
    ));
    assert!(
        store
            .admit(
                &identity(),
                &fixture.catalog,
                "test.run",
                &input().arguments
            )
            .is_err()
    );
}

struct OrdinaryGate {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl PermissionGate for OrdinaryGate {
    async fn authorize(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if cancel.is_cancelled() {
            return Err(CapabilityError::new("cancelled"));
        }
        if (name == "fs.read" && args == &json!({"path":"ordinary.txt"}))
            || (name == "test.run" && args == &json!({"profile":"project-test"}))
            || (name == "capability.request" && args == &serde_json::to_value(input()).unwrap())
        {
            Ok(())
        } else {
            Err(CapabilityError::new("upstream exact permission denied"))
        }
    }
}
#[derive(Default)]
struct Audit {
    started: AtomicUsize,
    settled: Mutex<Vec<std::result::Result<Value, String>>>,
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
        self.settled
            .lock()
            .unwrap()
            .push(result.clone().map_err(|e| e.message));
        Ok(())
    }
    fn dropped(&self, _: &str) -> std::result::Result<(), CapabilityError> {
        Err(CapabilityError::unknown("unexpected audit drop"))
    }
}

// The test executable is the actual application-selected process profile. It
// reads a real project input in cwd and writes a marker. No shell or interpreter.
#[test]
fn project_profile_fixture() {
    if std::env::var_os("CAYW_REQUEST_PROFILE_CHILD").is_none() {
        return;
    }
    assert_eq!(
        std::fs::read_to_string("project-input.txt").unwrap(),
        "project test input\n"
    );
    std::fs::write("profile-ran.txt", "project profile executed\n").unwrap();
}
fn host_config(
    fixture: &Fixture,
    gate: Arc<dyn PermissionGate>,
    audit: Arc<dyn AuditSink>,
) -> RequestHostConfig {
    let profile = Profile {
        kind: ProfileKind::Test,
        program: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".into(),
            "project_profile_fixture".into(),
            "--nocapture".into(),
        ],
        env: BTreeMap::from([("CAYW_REQUEST_PROFILE_CHILD".into(), "1".into())]),
        timeout: Duration::from_secs(10),
        max_output_bytes: 8192,
        allow_filter: false,
    };
    RequestHostConfig {
        root: fixture.root.clone(),
        host: HostConfig {
            profiles: BTreeMap::from([("project-test".into(), profile)]),
            ..HostConfig::default()
        },
        permission_policy_id: "real-profile-policy-v1".into(),
        operations: vec![OperationScope::exact(
            "project-tests",
            "test.run",
            json!({"profile":"project-test"}),
            "actual-device-profile-v1",
        )],
        gate,
        audit,
    }
}

#[tokio::test]
async fn actual_host_and_device_profile_check_request_and_upstream_permission_on_every_call() {
    let fixture = Fixture::new();
    let store = fixture.open();
    std::fs::write(
        fixture.root.join("ordinary.txt"),
        "ordinary granted tool remains usable\n",
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("project-input.txt"),
        "project test input\n",
    )
    .unwrap();
    let upstream = Arc::new(OrdinaryGate {
        calls: AtomicUsize::new(0),
    });
    let audit = Arc::new(Audit::default());
    let config = host_config(&fixture, upstream.clone(), audit.clone());
    let catalog = config.catalog().unwrap();
    store
        .start_task(
            &identity(),
            TaskIntent::ExplicitProjectTestOrScript {
                user_instruction: "Run the repository project tests".into(),
            },
            &catalog,
            &actor(),
        )
        .unwrap();
    let host = RequestHost::new(store.clone(), identity(), config).unwrap();
    let request_value = serde_json::to_value(input()).unwrap();
    let request = host
        .call(
            "capability.request",
            request_value.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(request["state"], "awaiting_evaluation");
    assert!(!fixture.root.join("profile-ran.txt").exists());
    assert!(
        host.call("test.run", input().arguments, CancellationToken::new())
            .await
            .is_err()
    );
    let ticket = store
        .evaluate(&identity(), request["id"].as_str().unwrap(), &actor())
        .unwrap();
    store
        .deny(&ticket, "host denied this task request")
        .unwrap();
    let ordinary = || json!({"path":"ordinary.txt"});
    host.call("fs.read", ordinary(), CancellationToken::new())
        .await
        .unwrap();
    let request = host
        .call(
            "capability.request",
            request_value.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let ticket = store
        .evaluate(&identity(), request["id"].as_str().unwrap(), &actor())
        .unwrap();
    store
        .evaluator_unavailable(&ticket, "host reviewer is offline")
        .unwrap();
    host.call("fs.read", ordinary(), CancellationToken::new())
        .await
        .unwrap();
    let request = host
        .call(
            "capability.request",
            request_value.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let ticket = store
        .evaluate(&identity(), request["id"].as_str().unwrap(), &actor())
        .unwrap();
    fixture
        .clock
        .set(1000 + fixture.config.evaluation_timeout_ms);
    assert!(
        store
            .approve(&ticket, &catalog, 1000, "review arrived too late")
            .is_err()
    );
    host.call("fs.read", ordinary(), CancellationToken::new())
        .await
        .unwrap();
    assert!(!host.outcome_unknown());
    assert!(!fixture.root.join("profile-ran.txt").exists());
    let request = host
        .call(
            "capability.request",
            request_value,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let ticket = store
        .evaluate(&identity(), request["id"].as_str().unwrap(), &actor())
        .unwrap();
    let approved = store
        .approve(
            &ticket,
            &catalog,
            1000,
            "host verified actual executable profile and project need",
        )
        .unwrap();
    let before = upstream.calls.load(Ordering::SeqCst);
    for _ in 0..2 {
        let result = host
            .call("test.run", input().arguments, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["termination"], "exited");
    }
    assert_eq!(upstream.calls.load(Ordering::SeqCst), before + 2);
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("profile-ran.txt")).unwrap(),
        "project profile executed\n"
    );
    assert!(
        host.call(
            "test.run",
            json!({"profile":"other"}),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert!(
        host.call(
            "test.run",
            json!({"profile":"project-test","filter":"other"}),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        host.call("test.run", input().arguments, cancelled)
            .await
            .is_err()
    );
    let connection =
        RequestStore::open_with_clock(&fixture.path, fixture.config.clone(), fixture.clock.clone())
            .unwrap();
    connection
        .revoke(
            &identity(),
            &approved.id,
            &actor(),
            "revoke from independent connection",
        )
        .unwrap();
    let started = audit.started.load(Ordering::SeqCst);
    assert!(
        host.call("test.run", input().arguments, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(started, audit.started.load(Ordering::SeqCst));
    host.call("fs.read", ordinary(), CancellationToken::new())
        .await
        .unwrap();
    assert!(!host.outcome_unknown());
    assert!(host.audit_failure().is_none());
    connection
        .end_task(
            &identity(),
            TaskOutcome::Completed,
            &actor(),
            "profile validation finished",
        )
        .unwrap();
    assert!(
        host.call("fs.read", ordinary(), CancellationToken::new())
            .await
            .is_err()
    );
}

struct RevokeDuringAudit {
    store: Arc<RequestStore>,
    request_id: Mutex<Option<String>>,
    audit: Audit,
}
#[async_trait::async_trait]
impl AuditSink for RevokeDuringAudit {
    async fn started(
        &self,
        name: &str,
        args: &Value,
    ) -> std::result::Result<String, CapabilityError> {
        let id = self.audit.started(name, args).await?;
        if name == "test.run"
            && let Some(request_id) = self.request_id.lock().unwrap().take()
        {
            self.store
                .revoke(
                    &identity(),
                    &request_id,
                    &actor(),
                    "revoked while committing audit start",
                )
                .unwrap();
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
async fn revocation_while_audit_start_waits_settles_denial_and_never_dispatches() {
    let fixture = Fixture::new();
    let store = fixture.open();
    std::fs::write(
        fixture.root.join("ordinary.txt"),
        "ordinary granted tool remains usable",
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("project-input.txt"),
        "project test input\n",
    )
    .unwrap();
    let audit = Arc::new(RevokeDuringAudit {
        store: fixture.open(),
        request_id: Mutex::new(None),
        audit: Audit::default(),
    });
    let upstream = Arc::new(OrdinaryGate {
        calls: AtomicUsize::new(0),
    });
    let config = host_config(&fixture, upstream, audit.clone());
    let catalog = config.catalog().unwrap();
    store
        .start_task(
            &identity(),
            TaskIntent::ExplicitProjectTestOrScript {
                user_instruction: "Run selected repository tests".into(),
            },
            &catalog,
            &actor(),
        )
        .unwrap();
    let record = store.request(&identity(), &catalog, input()).unwrap();
    let ticket = store.evaluate(&identity(), &record.id, &actor()).unwrap();
    store
        .approve(&ticket, &catalog, 1000, "verified host test profile")
        .unwrap();
    *audit.request_id.lock().unwrap() = Some(record.id);
    let host = RequestHost::new(store, identity(), config).unwrap();
    assert!(
        host.call("test.run", input().arguments, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(!fixture.root.join("profile-ran.txt").exists());
    assert_eq!(audit.audit.started.load(Ordering::SeqCst), 1);
    assert!(audit.audit.settled.lock().unwrap()[0].is_err());
    assert!(!host.outcome_unknown());
    host.call(
        "fs.read",
        json!({"path":"ordinary.txt"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
}

struct RevokeDuringWait {
    store: Arc<RequestStore>,
    request_id: String,
}
#[async_trait::async_trait]
impl PermissionGate for RevokeDuringWait {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        self.store
            .revoke(
                &identity(),
                &self.request_id,
                &actor(),
                "revoked during upstream permission wait",
            )
            .unwrap();
        Ok(())
    }
}
#[tokio::test]
async fn revoke_during_upstream_permission_wait_defeats_previously_valid_check() {
    let fixture = Fixture::new();
    let store = fixture.open();
    fixture.start(&store, TaskIntent::General);
    let approved = fixture.grant(&store, 1000);
    let upstream = Arc::new(RevokeDuringWait {
        store: fixture.open(),
        request_id: approved.id,
    });
    let gate = RequestGate::new(store, identity(), fixture.catalog, upstream).unwrap();
    assert!(
        gate.authorize("test.run", &input().arguments, CancellationToken::new())
            .await
            .is_err()
    );
}
