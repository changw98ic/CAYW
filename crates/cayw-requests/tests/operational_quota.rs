//! C19/C20 regressions: task quotas are operational (released at task end),
//! and the repeat threshold is only a hint. A trusted host may evaluate the
//! first valid general request. These tests use the real RequestStore and a
//! real, already-open Host identity; no model or network is involved.
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use cayw_host::{AuditSink, HostConfig, PermissionGate};
use cayw_requests::*;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

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
struct Audit;
#[async_trait::async_trait]
impl AuditSink for Audit {
    async fn started(&self, _: &str, _: &Value) -> std::result::Result<String, CapabilityError> {
        Ok("audit".into())
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

fn actor() -> HostActor {
    HostActor::new("trusted-test-host").unwrap()
}
fn identity(task: &str) -> TaskIdentity {
    TaskIdentity::new("session-a", task, "attempt-a").unwrap()
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

struct Fixture {
    _temp: tempfile::TempDir,
    path: PathBuf,
    config: RequestConfig,
    catalog: OperationCatalog,
}
impl Fixture {
    fn with_config(config: RequestConfig) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let host_config = HostConfig::default();
        let host =
            cayw_host::Host::new(&root, Arc::new(Allow), Arc::new(Audit), host_config.clone())
                .unwrap();
        let host_identity = HostIdentity::from_host(&host, &host_config, "policy-a").unwrap();
        let catalog = OperationCatalog::bind(
            host_identity,
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
            config,
            catalog,
        }
    }
    fn new() -> Self {
        Self::with_config(RequestConfig::default())
    }
    fn open(&self) -> RequestStore {
        RequestStore::open(&self.path, self.config.clone()).unwrap()
    }
    fn start(&self, store: &RequestStore, task: &str) {
        store
            .start_task(
                &identity(task),
                TaskIntent::General,
                &self.catalog,
                &actor(),
            )
            .unwrap();
    }
}

/// C20: the threshold is a hint. The trusted host can evaluate the first
/// structurally valid general request without forcing a repeated model request.
#[test]
fn first_general_valid_request_is_evaluable_below_repeat_threshold() {
    let fixture = Fixture::new();
    let store = fixture.open();
    fixture.start(&store, "task-a");
    let first = store
        .request(&identity("task-a"), &fixture.catalog, input())
        .unwrap();
    // The persisted state remains the below-threshold hint.
    assert_eq!(first.state, RequestState::Requested);
    assert_eq!(first.valid_request_count, 1);
    let ticket = store
        .evaluate(&identity("task-a"), &first.id, &actor())
        .expect("a trusted host must be able to evaluate the first valid request");
    let approved = store
        .approve(
            &ticket,
            &fixture.catalog,
            100,
            "host verified the exact need",
        )
        .unwrap();
    assert_eq!(approved.state, RequestState::Granted);
    store
        .admit(
            &identity("task-a"),
            &fixture.catalog,
            "test.run",
            &input().arguments,
        )
        .unwrap();
    // Evaluation never grants on its own: the same catalog and host decision
    // are still required, and invalid requests remain ineligible.
    let mut invalid = input();
    invalid.gap.clear();
    let invalid = store
        .request(&identity("task-a"), &fixture.catalog, invalid)
        .unwrap();
    assert_eq!(invalid.state, RequestState::Invalid);
    assert!(
        store
            .evaluate(&identity("task-a"), &invalid.id, &actor())
            .is_err()
    );
}

/// C19: finished tasks release their task count and conservative state
/// reservation. A chain of ordinary zero-request tasks must not exhaust an
/// operational allowance, while their tombstones stay immutable.
#[test]
fn finished_zero_request_tasks_release_task_and_state_reservations() {
    let fixture = Fixture::with_config(RequestConfig {
        max_tasks: 64,
        // One start reserves about 1.6 MiB; two simultaneous starts do not fit.
        max_state_bytes: 2 * 1024 * 1024,
        ..RequestConfig::default()
    });
    let store = fixture.open();
    for index in 0..5 {
        let task = format!("task-{index}");
        fixture.start(&store, &task);
        store
            .end_task(
                &identity(&task),
                TaskOutcome::Completed,
                &actor(),
                "ordinary chat finished without any capability request",
            )
            .unwrap();
        assert!(matches!(
            store.task_state(&identity(&task)).unwrap(),
            TaskState::Ended {
                outcome: TaskOutcome::Completed,
                ..
            }
        ));
    }
    // Tombstones remain and the same task id cannot restart.
    assert!(
        store
            .start_task(
                &identity("task-0"),
                TaskIntent::General,
                &fixture.catalog,
                &actor()
            )
            .is_err()
    );
}

/// C19: operational limits are adjustable in place, persist with immutable
/// history, and the previous value cannot silently return through a stale
/// handle or a reopen with old configuration.
#[test]
fn trusted_adjustment_is_durable_and_stale_handles_fail_closed() {
    let fixture = Fixture::new();
    let store = fixture.open();
    // Open a second handle under the old configuration before adjustment.
    let stale = fixture.open();
    fixture.start(&store, "task-a");
    let reduced = RequestConfig {
        max_tasks: 64,
        ..fixture.config.clone()
    };
    store
        .adjust_config(
            reduced.clone(),
            &actor(),
            "operator lowered the active task allowance",
        )
        .unwrap();
    assert_eq!(store.config(), reduced);
    let history = store.config_history().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].previous, fixture.config);
    assert_eq!(history[0].next, reduced);
    assert_eq!(history[0].actor, "trusted-test-host");
    drop(store);

    // A reopened handle must use the persisted value. The old configuration is
    // rejected instead of silently reverting the adjustment.
    assert_eq!(
        RequestStore::persisted_config(&fixture.path).unwrap(),
        reduced
    );
    assert!(matches!(
        RequestStore::open(&fixture.path, fixture.config.clone()),
        Err(RequestError::PolicyMismatch)
    ));
    // The supported restart path adopts persisted limits and applies the new
    // trusted value with another immutable history row.
    let next = RequestConfig {
        max_tasks: 32,
        ..reduced.clone()
    };
    let store = RequestStore::open_with_trusted_adjustment(
        &fixture.path,
        next.clone(),
        &actor(),
        "operator changed the allowance again",
    )
    .unwrap();
    assert_eq!(store.config(), next);
    assert_eq!(store.config_history().unwrap().len(), 2);
    // The task started under the original configuration is still inspectable.
    assert_eq!(
        store.task_state(&identity("task-a")).unwrap(),
        TaskState::Active
    );

    // A second live handle opened with the old value fails closed on its next
    // operation rather than admitting work under stale limits.
    assert!(matches!(
        stale.task_state(&identity("task-a")),
        Err(RequestError::PolicyMismatch)
    ));
}

/// C19: adjustment cannot quietly invalidate live reservations.
#[test]
fn adjustment_rejects_limits_below_active_usage() {
    let fixture = Fixture::with_config(RequestConfig {
        max_tasks: 2,
        ..RequestConfig::default()
    });
    let store = fixture.open();
    fixture.start(&store, "task-a");
    fixture.start(&store, "task-b");
    let lowered = RequestConfig {
        max_tasks: 1,
        ..fixture.config.clone()
    };
    assert!(matches!(
        store.adjust_config(lowered, &actor(), "too small for the active tasks"),
        Err(RequestError::InvalidInput(_))
    ));
    assert_eq!(store.config(), fixture.config);
    assert!(store.config_history().unwrap().is_empty());
}

/// C19: the supported restart path creates a new store with the supplied
/// configuration and adjusts an existing store from its persisted value.
#[test]
fn trusted_open_creates_or_adjusts_an_existing_store() {
    let fixture = Fixture::new();
    let store = RequestStore::open_with_trusted_adjustment(
        &fixture.path,
        fixture.config.clone(),
        &actor(),
        "initial configuration",
    )
    .unwrap();
    assert_eq!(store.config(), fixture.config);
    assert!(store.config_history().unwrap().is_empty());
    drop(store);
    let next = RequestConfig {
        max_tasks: 16,
        ..fixture.config.clone()
    };
    let store = RequestStore::open_with_trusted_adjustment(
        &fixture.path,
        next.clone(),
        &actor(),
        "restart with a reviewed allowance",
    )
    .unwrap();
    assert_eq!(store.config(), next);
    assert_eq!(store.config_history().unwrap().len(), 1);
}

/// C19: databases from the earlier lifetime-counter semantics migrate on open.
/// The stale counters are rebuilt from unfinished tasks, while every original
/// task, request, and event audit row remains.
#[test]
fn reopen_rebuilds_operational_counters_without_deleting_audit() {
    let fixture = Fixture::with_config(RequestConfig {
        max_tasks: 1,
        ..RequestConfig::default()
    });
    let store = fixture.open();
    fixture.start(&store, "task-a");
    store
        .end_task(
            &identity("task-a"),
            TaskOutcome::Completed,
            &actor(),
            "finished before the migration",
        )
        .unwrap();
    drop(store);
    let database = rusqlite::Connection::open(&fixture.path).unwrap();
    database
        .execute(
            "UPDATE request_meta SET task_count=999,reserved_bytes=999999999 WHERE singleton=1",
            [],
        )
        .unwrap();
    let events: i64 = database
        .query_row("SELECT COUNT(*) FROM request_events", [], |row| row.get(0))
        .unwrap();
    drop(database);
    // Open recomputes the operational counters from the retained projection.
    let reopened = fixture.open();
    reopened
        .start_task(
            &identity("task-b"),
            TaskIntent::General,
            &fixture.catalog,
            &actor(),
        )
        .expect("migrated counters must not keep a finished task's allowance");
    assert!(matches!(
        reopened.task_state(&identity("task-a")).unwrap(),
        TaskState::Ended {
            outcome: TaskOutcome::Completed,
            ..
        }
    ));
    let database = rusqlite::Connection::open(&fixture.path).unwrap();
    let events_after: i64 = database
        .query_row("SELECT COUNT(*) FROM request_events", [], |row| row.get(0))
        .unwrap();
    assert!(events_after > events);
}

#[test]
fn max_tasks_bounds_active_tasks_and_releases_on_end() {
    let fixture = Fixture::with_config(RequestConfig {
        max_tasks: 1,
        max_state_bytes: 16 * 1024 * 1024,
        ..RequestConfig::default()
    });
    let store = fixture.open();
    fixture.start(&store, "task-a");
    assert!(matches!(
        store.start_task(
            &identity("task-b"),
            TaskIntent::General,
            &fixture.catalog,
            &actor()
        ),
        Err(RequestError::Budget)
    ));
    store
        .end_task(
            &identity("task-a"),
            TaskOutcome::Completed,
            &actor(),
            "first task finished",
        )
        .unwrap();
    store
        .start_task(
            &identity("task-b"),
            TaskIntent::General,
            &fixture.catalog,
            &actor(),
        )
        .unwrap();
}
