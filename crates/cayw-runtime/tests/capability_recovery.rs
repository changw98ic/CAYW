use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_codemode::CapabilityError;
use cayw_core::{CapabilityOutcome, CapabilityReconciliation, CoreEvent, SessionState};
use cayw_host::ExternalHost;
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, ScopedPermission};
use futures::stream;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Script {
    script: &'static str,
    calls: AtomicUsize,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let answer =
            cayw_runtime::fixture::script_response(&request, self.script, &format!("call-{n}"));
        Box::pin(stream::iter(vec![AssistantEvent::Done { message: answer }]))
    }
}
struct UnknownService(AtomicUsize);
#[async_trait]
impl ExternalHost for UnknownService {
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(CapabilityError::unknown(
            "synthetic external response lost after side effect boundary",
        ))
    }
}
fn open(dir: &tempfile::TempDir, provider: Arc<Script>, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        dir.path().join("sessions.db"),
        dir.path().join("workspace"),
        provider,
        Model::new("fixture", "audit"),
        config,
    )
    .unwrap()
}
fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    dir
}

#[tokio::test]
async fn real_host_unknown_terminal_pairs_in_core_and_blocks_models_after_reopen() {
    let dir = setup();
    let service = Arc::new(UnknownService(AtomicUsize::new(0)));
    let mut config = RuntimeConfig::default();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"test"}),
    )];
    let provider = Arc::new(Script {
        script: "cayw.call('search.query',{query='test'}); return cayw.call('search.query',{query='test'})",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let result = rt
        .prompt(&id, "unknown external effect", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.state, SessionState::NeedsReconciliation);
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let events = rt.events(&id, 0, 1000).unwrap();
    let start = events
        .iter()
        .find_map(|record| match &record.event {
            CoreEvent::CapabilityStarted { call } => Some(call),
            _ => None,
        })
        .unwrap();
    let end = events
        .iter()
        .find_map(|record| match &record.event {
            CoreEvent::CapabilitySettled { call }
                if matches!(
                    call.outcome,
                    Some(cayw_core::CapabilityOutcome::OutcomeUnknown { .. })
                ) =>
            {
                Some(call)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(start.key, end.key);
    assert_eq!(start.parent_call_id, end.parent_call_id);
    assert_eq!(start.task_id, end.task_id);
    assert_eq!(start.lease, end.lease);
    assert_eq!(start.parent_call_id, "call-0");
    assert_eq!(start.key.sequence, 0);
    assert_eq!(result.unresolved_capabilities.len(), 1);
    assert_eq!(result.unresolved_capabilities[0], *end);
    assert!(result.unresolved_calls.is_empty());
    assert!(result.messages.iter().any(|message| {
        matches!(message, Message::ToolResult(result)
            if result.is_error && !result.outcome_unknown
                && result.structured_content.as_ref().is_some_and(|page| page["outcome_unknown"] == true))
    }));
    drop(rt);
    let rt = open(&dir, provider.clone(), config);
    assert!(matches!(
        rt.prompt(&id, "try again", CancellationToken::new()).await,
        Err(RuntimeError::NeedsReconciliation)
    ));
    rt.recover(&id).unwrap();
    assert!(matches!(
        rt.prompt(&id, "still blocked", CancellationToken::new())
            .await,
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let reconciled = rt.reconcile_capability(&id, CapabilityReconciliation {
        key: end.key.clone(),
        actor: "fixture-host".into(),
        evidence: "Synthetic service counter observed one completed test invocation; no real provider used".into(),
        outcome: CapabilityOutcome::Completed { result: json!({"count":1}) },
        external_provenance: end.external_provenance.clone(),
    }).unwrap();
    let view = serde_json::to_value(reconciled).unwrap();
    assert_eq!(view["frozen"], false);
    assert_eq!(view["binding"], serde_json::Value::Null);
    assert_eq!(rt.get(&id).unwrap().state, SessionState::Idle);
    rt.enqueue(&id, "explicit task after verified capability")
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cells_commit_failure_after_core_reconciliation_is_frozen_and_retryable() {
    let dir = setup();
    let service = Arc::new(UnknownService(AtomicUsize::new(0)));
    let mut config = RuntimeConfig::default();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"test"}),
    )];
    let provider = Arc::new(Script {
        script: "return cayw.call('search.query',{query='test'})",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "lose one response", CancellationToken::new())
        .await
        .unwrap();
    let call = view.unresolved_capabilities[0].clone();
    rt.recover(&id).unwrap();
    let input = CapabilityReconciliation {
        key: call.key.clone(),
        actor: "fixture-host".into(),
        evidence: "Observed the synthetic service counter at exactly one invocation".into(),
        outcome: CapabilityOutcome::Completed {
            result: json!({"count":1}),
        },
        external_provenance: call.external_provenance,
    };
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_cells_repair BEFORE UPDATE ON cells_v1
        WHEN json_array_length(json_extract(NEW.body,'$.host_settlements')) >
             json_array_length(json_extract(OLD.body,'$.host_settlements'))
        BEGIN SELECT RAISE(ABORT,'synthetic Cells repair failure'); END;",
    )
    .unwrap();
    let error = rt.reconcile_capability(&id, input.clone()).unwrap_err();
    assert!(
        error.to_string().contains("synthetic Cells repair failure"),
        "{error}"
    );
    assert_eq!(rt.store().snapshot(&id).unwrap().state, SessionState::Idle);
    assert!(matches!(
        rt.enqueue(&id, "blocked during partial commit"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
    db.execute_batch("DROP TRIGGER reject_cells_repair")
        .unwrap();
    drop(rt);
    let rt = open(&dir, provider.clone(), config);
    assert!(matches!(
        rt.enqueue(&id, "still blocked after reopen"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    let recovered = rt.reconcile_capability(&id, input.clone()).unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    let events = rt.events(&id, 0, 1000).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, CoreEvent::CapabilityReconciled { .. }))
            .count(),
        1
    );
    let body: String = db
        .query_row(
            "SELECT body FROM cells_v1 WHERE id=?1",
            [&call.key.execution_id],
            |row| row.get(0),
        )
        .unwrap();
    let first: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(first["calls"][0]["status"], "succeeded");
    assert_eq!(first["calls"][0]["result"], json!({"count":1}));
    assert!(first["backend_error"].is_null());
    assert!(!first["host_settlements"].as_array().unwrap().is_empty());
    rt.reconcile_capability(&id, input.clone()).unwrap();
    let retried: String = db
        .query_row(
            "SELECT body FROM cells_v1 WHERE id=?1",
            [&call.key.execution_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        body, retried,
        "an exact retry must not change Cells revision or its audit"
    );
    let mut changed = input;
    changed.outcome = CapabilityOutcome::Completed {
        result: json!({"count":2}),
    };
    assert!(rt.reconcile_capability(&id, changed).is_err());
    assert_eq!(rt.get(&id).unwrap(), recovered);
    rt.enqueue(&id, "explicit next task").unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stopped_cells_owner_rejects_reconciliation_before_core_is_changed() {
    let dir = setup();
    let service = Arc::new(UnknownService(AtomicUsize::new(0)));
    let mut config = RuntimeConfig::default();
    config.host.external = Some(service.clone());
    config.permissions = vec![ScopedPermission::exact(
        "search.query",
        json!({"query":"test"}),
    )];
    let provider = Arc::new(Script {
        script: "return cayw.call('search.query',{query='test'})",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "lose response", CancellationToken::new())
        .await
        .unwrap();
    let call = view.unresolved_capabilities[0].clone();
    let journal =
        Arc::new(cayw_cells::SqliteJournal::open(dir.path().join("sessions.db")).unwrap());
    let identity = cayw_cells::CellIdentity {
        session_id: id.clone(),
        task_id: call.task_id.clone(),
        attempt_id: call.lease.attempt_id.clone(),
        permission_context: format!("{}:{}", call.lease.worker_id, call.lease.fence),
    };
    let owned = journal
        .stopped_scope(
            cayw_cells::ExecutorStopped {
                identity,
                evidence: "Independent host owns the stopped scope for this test".into(),
            },
            config.cells,
        )
        .unwrap();
    let before = rt.get(&id).unwrap();
    let input = CapabilityReconciliation {
        key: call.key,
        actor: "fixture-host".into(),
        evidence: "Synthetic service observed exactly once".into(),
        outcome: CapabilityOutcome::Completed {
            result: json!({"count":1}),
        },
        external_provenance: call.external_provenance,
    };
    assert!(rt.reconcile_capability(&id, input.clone()).is_err());
    assert_eq!(rt.get(&id).unwrap(), before);
    assert!(rt.recover(&id).is_err());
    assert_eq!(rt.get(&id).unwrap(), before);
    drop(owned);
    assert_eq!(
        rt.reconcile_capability(&id, input).unwrap().state,
        SessionState::Idle
    );
    rt.enqueue(&id, "explicit next task").unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn backend_terminal_failure_repairs_from_core_without_replaying_the_file_write() {
    let dir = setup();
    let config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"once.txt","content":"one effect"}),
        )],
        ..Default::default()
    };
    let provider = Arc::new(Script {
        script: "return cayw.call('fs.write',{path='once.txt',content='one effect'})",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_execution_finish BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record,'$.event.type')='execution_finished'
        BEGIN SELECT RAISE(ABORT,'synthetic backend lifecycle failure'); END;",
    )
    .unwrap();
    assert!(
        rt.prompt(&id, "write once", CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/once.txt")).unwrap(),
        "one effect"
    );
    assert_eq!(rt.health().unwrap().running, 1);
    let body: String = db
        .query_row("SELECT body FROM cells_v1 WHERE session=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    let before: Value = serde_json::from_str(&body).unwrap();
    assert!(
        before["backend_error"]
            .as_str()
            .unwrap()
            .contains("synthetic backend lifecycle failure")
    );
    let call = before["calls"][0].clone();
    db.execute_batch(
        "DROP TRIGGER reject_execution_finish;
        CREATE TRIGGER reject_recovery_cells BEFORE UPDATE ON cells_v1
        WHEN json_array_length(json_extract(NEW.body,'$.host_settlements')) >
             json_array_length(json_extract(OLD.body,'$.host_settlements'))
        BEGIN SELECT RAISE(ABORT,'synthetic stopped Cells write failure'); END;",
    )
    .unwrap();
    assert!(
        rt.recover(&id)
            .unwrap_err()
            .to_string()
            .contains("synthetic stopped Cells write failure")
    );
    assert!(rt.get(&id).unwrap().attempt.is_none());
    assert_eq!(
        rt.health().unwrap().running,
        1,
        "failed Cells sync must retain its slot"
    );
    assert!(rt.enqueue(&id, "must remain frozen").is_err());
    db.execute_batch("DROP TRIGGER reject_recovery_cells")
        .unwrap();
    drop(rt);
    let rt = open(&dir, provider.clone(), config);
    let recovered = rt.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(recovered.unresolved_calls.len(), 1);
    assert!(recovered.unresolved_capabilities.is_empty());
    assert_eq!(rt.health().unwrap().running, 0);
    assert!(
        rt.enqueue(&id, "parent reply still needs host evidence")
            .is_err()
    );
    let repaired: String = db
        .query_row("SELECT body FROM cells_v1 WHERE session=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    let after: Value = serde_json::from_str(&repaired).unwrap();
    assert!(after["backend_error"].is_null());
    assert_eq!(
        after["calls"][0], call,
        "known original result must remain intact"
    );
    assert_eq!(after["host_settlements"].as_array().unwrap().len(), 1);
    assert_eq!(
        after["host_settlements"][0]["settlement"]["execution_terminal"]["expected_backend_error"],
        before["backend_error"]
    );
    rt.recover(&id).unwrap();
    let retried: String = db
        .query_row("SELECT body FROM cells_v1 WHERE session=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        repaired, retried,
        "recovery retry must not add duplicate audit records"
    );
    let input = cayw_core::Reconciliation {
        call_id: recovered.unresolved_calls[0].call.id.clone(), actor: "fixture-host".into(),
        evidence: "Observed written once.txt and matched the durable child result; only the launch reply was lost".into(),
        outcome: cayw_core::ReconciledOutcome::Completed {
            result: cayw_agent::ToolResult::error("VM stopped after the known child result; inspect its original execution"),
        },
    };
    assert_eq!(
        rt.reconcile(&id, input.clone()).unwrap().state,
        SessionState::Idle
    );
    let reconciled = rt.get(&id).unwrap();
    rt.reconcile(&id, input).unwrap();
    assert_eq!(rt.get(&id).unwrap(), reconciled);
    let final_record: String = db
        .query_row("SELECT body FROM cells_v1 WHERE session=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(repaired, final_record);
    rt.enqueue(&id, "explicit next task").unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/once.txt")).unwrap(),
        "one effect"
    );
}

#[tokio::test]
async fn recovering_several_completed_tasks_uses_one_session_gate() {
    let dir = setup();
    let provider = Arc::new(Script {
        script: "return 'known'",
        calls: AtomicUsize::new(0),
    });
    let rt = open(&dir, provider.clone(), RuntimeConfig::default());
    let id = rt.create(None).unwrap().id;
    for index in 0..3 {
        assert_eq!(
            rt.prompt(&id, &format!("task {index}"), CancellationToken::new())
                .await
                .unwrap()
                .state,
            SessionState::Idle
        );
    }
    assert_eq!(rt.tasks(&id).unwrap().len(), 3);
    assert_eq!(rt.recover(&id).unwrap().state, SessionState::Idle);
    rt.enqueue(&id, "explicit next task").unwrap();
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.health().unwrap().queued, 1);
}

#[tokio::test]
async fn settlement_persistence_failure_retains_written_file_and_unknown_parent() {
    let dir = setup();
    let provider = Arc::new(Script {
        script: "return cayw.call('fs.write',{path='committed.txt',content='written'})",
        calls: AtomicUsize::new(0),
    });
    let config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"committed.txt","content":"written"}),
        )],
        ..Default::default()
    };
    let rt = open(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER runtime_test_reject_settlement BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record,'$.event.type')='capability_settled'
        BEGIN SELECT RAISE(ABORT,'synthetic settlement storage failure'); END;",
    )
    .unwrap();
    let result = rt
        .prompt(
            &id,
            "write with failed settlement journal",
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/committed.txt")).unwrap(),
        "written"
    );
    let snapshot = rt.get(&id).unwrap();
    assert_eq!(
        snapshot.state,
        SessionState::NeedsReconciliation,
        "{result:?}"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(
        snapshot
            .unresolved_calls
            .iter()
            .any(|call| call.call.id == "call-0")
    );
    db.execute_batch("DROP TRIGGER runtime_test_reject_settlement")
        .unwrap();
    drop(rt);
    let rt = open(&dir, provider.clone(), config);
    assert!(matches!(
        rt.prompt(&id, "do not replay", CancellationToken::new())
            .await,
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn start_persistence_failure_stops_before_effect_and_conservatively_requires_inspection() {
    let dir = setup();
    let provider = Arc::new(Script {
        script: "return cayw.call('fs.write',{path='absent.txt',content='no'})",
        calls: AtomicUsize::new(0),
    });
    let config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"absent.txt","content":"no"}),
        )],
        ..Default::default()
    };
    let rt = open(&dir, provider.clone(), config);
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER runtime_test_reject_start BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record,'$.event.type')='capability_started'
        BEGIN SELECT RAISE(ABORT,'synthetic start storage failure'); END;",
    )
    .unwrap();
    let _ = rt
        .prompt(&id, "fail durable start", CancellationToken::new())
        .await;
    assert!(!dir.path().join("workspace/absent.txt").exists());
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}
