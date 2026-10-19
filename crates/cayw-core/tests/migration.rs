use cayw_core::{
    ApprovalTarget, CoreError, CoreEvent, PROTOCOL_VERSION, SCHEMA_VERSION, SessionState,
    SessionStore,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("cayw-core-migration-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn path(&self) -> PathBuf {
        self.0.join("session.sqlite")
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Literal old-contract rows, rather than new serialization with fields removed.
fn v1_fixture() -> (Value, Vec<Value>) {
    let lease = json!({"session_id":"v1-session", "attempt_id":"v1-attempt", "worker_id":"v1-worker", "fence":1});
    let call = json!({"id":"v1-call", "name":"execute_code", "arguments":{"script":"return 1"}});
    let request = json!({
        "id":"v1-approval", "session_id":"v1-session", "attempt_id":"v1-attempt",
        "input":{"task_id":"v1-task", "call_id":"v1-call", "capability":"execute_code", "scope":"script", "reason":"original reason"},
        "requested_at":20, "decision":null
    });
    let mut decision = request.clone();
    decision["decision"] = json!({"decision":"approve", "actor":"original-host", "decided_at":21});
    let projection = json!({
        "id":"v1-session", "label":"retained label", "cursor":5, "fence":1,
        "attempt":lease, "lifecycle":{"started":true,"ended":false,"turn_open":false,"open_message":null,"assistant":null},
        "history":[],
        "calls":[{"record":{"call":call,"attempt_id":"v1-attempt","state":"started","execution_started":true,"result":null},"batch_settled":false,"result_acknowledged":false}],
        "approvals":[decision]
    });
    let events = vec![
        json!({"type":"session_created", "label":"retained label"}),
        json!({"type":"attempt_started", "lease":lease}),
        json!({"type":"host", "kind":"fixture_marker", "data":{"receipt":"retained-receipt", "known":true}}),
        json!({"type":"approval_requested", "approval":request}),
        json!({"type":"approval_decided", "approval":decision}),
    ].into_iter().enumerate().map(|(index, event)| json!({
        "protocol_version":1, "session_id":"v1-session", "cursor":index + 1, "timestamp":100 + index,
        "attempt":if index == 0 { Value::Null } else { lease.clone() }, "event":event
    })).collect();
    (projection, events)
}

fn write_v1(database: &Database, projection: &Value, events: &[Value], extra_schema: Option<&str>) {
    let db = Connection::open(database.path()).unwrap();
    db.execute_batch(
        "PRAGMA application_id=1128356183;
         PRAGMA user_version=1;
         CREATE TABLE core_metadata (format TEXT PRIMARY KEY NOT NULL);
         INSERT INTO core_metadata VALUES ('cayw-core/1');
         CREATE TABLE core_sessions (id TEXT PRIMARY KEY NOT NULL, projection TEXT NOT NULL);
         CREATE TABLE core_events (session_id TEXT NOT NULL REFERENCES core_sessions(id), cursor INTEGER NOT NULL CHECK(cursor > 0), record TEXT NOT NULL, PRIMARY KEY(session_id,cursor));
         CREATE TRIGGER core_events_no_update BEFORE UPDATE ON core_events BEGIN SELECT RAISE(ABORT,'core events are append-only'); END;
         CREATE TRIGGER core_events_no_delete BEFORE DELETE ON core_events BEGIN SELECT RAISE(ABORT,'core events are append-only'); END;"
    ).unwrap();
    db.execute(
        "INSERT INTO core_sessions VALUES (?1,?2)",
        params!["v1-session", projection.to_string()],
    )
    .unwrap();
    for event in events {
        db.execute(
            "INSERT INTO core_events VALUES (?1,?2,?3)",
            params![
                "v1-session",
                event["cursor"].as_i64().unwrap(),
                event.to_string()
            ],
        )
        .unwrap();
    }
    if let Some(sql) = extra_schema {
        db.execute_batch(sql).unwrap();
    }
}

fn assert_v1_untouched(database: &Database, bytes: &[u8], projection: &Value, events: &[Value]) {
    assert_eq!(std::fs::read(database.path()).unwrap(), bytes);
    let db = Connection::open(database.path()).unwrap();
    let version: u32 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1);
    let format: String = db
        .query_row("SELECT format FROM core_metadata", [], |r| r.get(0))
        .unwrap();
    assert_eq!(format, "cayw-core/1");
    let retained: String = db
        .query_row("SELECT projection FROM core_sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(retained, projection.to_string());
    let retained: Vec<String> = db
        .prepare("SELECT record FROM core_events ORDER BY cursor")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        retained,
        events.iter().map(Value::to_string).collect::<Vec<_>>()
    );
    let journal: String = db
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    assert!(
        db.execute("UPDATE core_events SET record='{}'", [])
            .is_err()
    );
    assert!(db.execute("DELETE FROM core_events", []).is_err());
}

#[test]
fn v1_exact_approvals_migrate_transactionally_to_one_current_contract() {
    let database = Database::new();
    let (projection, events) = v1_fixture();
    write_v1(&database, &projection, &events, None);
    let store = SessionStore::open(database.path()).unwrap();
    let snapshot = store.snapshot("v1-session").unwrap();
    assert_eq!(snapshot.id, "v1-session");
    assert_eq!(snapshot.label, "retained label");
    assert_eq!(snapshot.cursor, 5);
    assert_eq!(snapshot.state, SessionState::Running);
    assert!(snapshot.executions.is_empty());
    assert!(snapshot.unresolved_capabilities.is_empty());
    assert_eq!(snapshot.attempt.as_ref().unwrap().worker_id, "v1-worker");
    assert_eq!(snapshot.unresolved_calls.len(), 1);
    assert_eq!(
        snapshot.unresolved_calls[0].call.arguments,
        json!({"script":"return 1"})
    );
    let approval = store.approval("v1-session", "v1-approval").unwrap();
    assert_eq!(
        approval.input.target,
        ApprovalTarget::ToolCall {
            call_id: "v1-call".into()
        }
    );
    assert_eq!(approval.input.task_id, "v1-task");
    assert_eq!(approval.requested_at, 20);
    assert_eq!(approval.decision.as_ref().unwrap().actor, "original-host");
    assert_eq!(approval.decision.as_ref().unwrap().decided_at, 21);
    let current = store.events("v1-session", 0, 100).unwrap();
    assert_eq!(current.len(), events.len());
    for (index, record) in current.iter().enumerate() {
        assert_eq!(record.protocol_version, PROTOCOL_VERSION);
        assert_eq!(record.cursor, index as u64 + 1);
        assert_eq!(record.timestamp, 100 + index as u64);
        assert_eq!(
            serde_json::to_value(&record.attempt).unwrap(),
            events[index]["attempt"]
        );
    }
    assert!(
        matches!(&current[2].event, CoreEvent::Host { kind, data } if kind == "fixture_marker" && data == &json!({"receipt":"retained-receipt", "known":true}))
    );
    let db = Connection::open(database.path()).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        db.query_row("SELECT format FROM core_metadata", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "cayw-core/3"
    );
    let raw: String = db
        .query_row("SELECT projection FROM core_sessions", [], |r| r.get(0))
        .unwrap();
    let raw: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(raw["executions"], json!([]));
    assert_eq!(raw["capabilities"], json!([]));
    assert!(raw["approvals"][0]["input"].get("call_id").is_none());
    assert_eq!(
        raw["approvals"][0]["input"]["target"],
        json!({"type":"tool_call","call_id":"v1-call"})
    );
    assert!(
        db.execute("UPDATE core_events SET record='{}'", [])
            .is_err()
    );
    assert!(db.execute("DELETE FROM core_events", []).is_err());
    let before = std::fs::read(database.path()).unwrap();
    let reopened = SessionStore::open(database.path()).unwrap();
    assert_eq!(reopened.snapshot("v1-session").unwrap(), snapshot);
    assert_eq!(reopened.events("v1-session", 0, 100).unwrap(), current);
    assert_eq!(std::fs::read(database.path()).unwrap(), before);
}

// Legacy capability effects cannot be erased by adding empty execution arrays.
#[test]
fn legacy_effect_facts_are_refused_before_any_migration_write() {
    for kind in [
        "execution_started",
        "execution_yielded",
        "execution_finished",
        "capability_planned",
        "capability_started",
        "capability_settled",
        "capability_completed",
        "capability_outcome_unknown",
        "capability_reconciled",
    ] {
        let database = Database::new();
        let (projection, mut events) = v1_fixture();
        events[2]["event"] = json!({"type":"host", "kind":kind, "data":{"execution_id":"legacy-vm", "sequence":0, "known_result":{"saved":true}}});
        write_v1(&database, &projection, &events, None);
        let bytes = std::fs::read(database.path()).unwrap();
        assert!(matches!(
            SessionStore::open(database.path()),
            Err(CoreError::MigrationRequired(_))
        ));
        assert_v1_untouched(&database, &bytes, &projection, &events);
    }
}

#[test]
fn migration_cannot_silently_drop_typed_execution_events_from_a_v1_log() {
    let database = Database::new();
    let (projection, mut events) = v1_fixture();
    events[2]["event"] = json!({
        "type":"execution_started",
        "execution":{
            "execution_id":"legacy-vm", "parent_call_id":"v1-call", "task_id":"v1-task", "source_hash":"sha256:legacy",
            "lease":projection["attempt"], "state":"running", "terminal":null
        }
    });
    write_v1(&database, &projection, &events, None);
    let bytes = std::fs::read(database.path()).unwrap();
    assert!(matches!(
        SessionStore::open(database.path()),
        Err(CoreError::MigrationRequired(_))
    ));
    assert_v1_untouched(&database, &bytes, &projection, &events);
}

#[test]
fn migration_refuses_unbound_approvals_and_unversioned_execution_facts() {
    for change in ["unbound", "execution", "recovery"] {
        let database = Database::new();
        let (mut projection, mut events) = v1_fixture();
        match change {
            "unbound" => {
                projection["approvals"][0]["input"]["call_id"] = Value::Null;
                for event in &mut events[3..] {
                    event["event"]["approval"]["input"]["call_id"] = Value::Null;
                }
            }
            "execution" => {
                projection["executions"] =
                    json!([{"execution_id":"unversioned", "outcome":"unknown"}]);
            }
            _ => {
                events[2]["event"] = json!({"type":"attempt_recovered", "lease":projection["attempt"], "unknown_call_ids":[], "not_executed_call_ids":[], "capabilities":[{"legacy":"retained"}]});
            }
        }
        write_v1(&database, &projection, &events, None);
        let bytes = std::fs::read(database.path()).unwrap();
        assert!(matches!(
            SessionStore::open(database.path()),
            Err(CoreError::MigrationRequired(_))
        ));
        assert_v1_untouched(&database, &bytes, &projection, &events);
    }
}

#[test]
fn migration_rolls_back_rewritten_events_projections_versions_and_append_only_guards() {
    let database = Database::new();
    let (projection, events) = v1_fixture();
    write_v1(
        &database,
        &projection,
        &events,
        Some(
            "CREATE TRIGGER reject_migration BEFORE UPDATE ON core_metadata BEGIN SELECT RAISE(ABORT,'injected migration failure'); END;",
        ),
    );
    let bytes = std::fs::read(database.path()).unwrap();
    assert!(matches!(
        SessionStore::open(database.path()),
        Err(CoreError::Database(_))
    ));
    assert_v1_untouched(&database, &bytes, &projection, &events);
    let db = Connection::open(database.path()).unwrap();
    db.execute_batch("DROP TRIGGER reject_migration").unwrap();
    drop(db);
    let store = SessionStore::open(database.path()).unwrap();
    assert_eq!(store.events("v1-session", 0, 100).unwrap().len(), 5);
}

fn v2_fixture() -> (Value, Vec<Value>) {
    let lease = json!({"session_id":"v2-session","attempt_id":"v2-attempt","worker_id":"v2-worker","fence":1});
    let launch = json!({"id":"launch","name":"execute_code","arguments":{"action":"exec","script":"yield_control()"}});
    let wait = json!({"id":"wait","name":"execute_code","arguments":{"action":"wait","cell_id":"vm","cursor":0,"yield_time_ms":0,"max_output_tokens":1024,"continue_execution":false}});
    let execution = json!({"execution_id":"vm","parent_call_id":"launch","task_id":"task","source_hash":"verified-source","lease":lease,"state":"running","terminal":null});
    let child = json!({
        "key":{"execution_id":"vm","sequence":0},"name":"fs.write","arguments":{"path":"once"},
        "parent_call_id":"launch","task_id":"task","source_hash":"verified-source","lease":lease,
        "stage":"started","authorization":{"actor":"host","target":{"execution_id":"vm","sequence":0},"task_id":"task","capability":"fs.write","scope":"once","decision":"approve","approval_id":null,"external_provenance":null},
        "outcome":null,"external_provenance":null
    });
    let projection = json!({
        "id":"v2-session","label":"v2 retained","cursor":5,"fence":1,"attempt":lease,
        "lifecycle":{"started":true,"ended":false,"turn_open":false,"open_message":null,"assistant":null},
        "history":[{"type":"tool_slot","call_id":"launch"},{"type":"tool_slot","call_id":"wait"}],
        "calls":[
            {"record":{"call":launch,"attempt_id":"v2-attempt","state":"started","execution_started":true,"result":null},"batch_settled":false,"result_acknowledged":false},
            {"record":{"call":wait,"attempt_id":"v2-attempt","state":"started","execution_started":true,"result":null},"batch_settled":false,"result_acknowledged":false}
        ],"approvals":[],"executions":[execution],"capabilities":[child]
    });
    let events = vec![
        json!({"type":"session_created","label":"v2 retained"}),
        json!({"type":"attempt_started","lease":lease}),
        json!({"type":"execution_started","execution":execution}),
        json!({"type":"capability_started","call":child}),
        json!({"type":"tool_started","call_id":"wait"}),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, event)| {
        json!({
            "protocol_version":2,"session_id":"v2-session","cursor":i+1,"timestamp":200+i,
            "attempt":if i==0 {Value::Null} else {lease.clone()},"event":event
        })
    })
    .collect();
    (projection, events)
}

fn write_v2(database: &Database, projection: &Value, events: &[Value], extra_schema: Option<&str>) {
    let db = Connection::open(database.path()).unwrap();
    db.execute_batch(
        "PRAGMA application_id=1128356183; PRAGMA user_version=2;
         CREATE TABLE core_metadata (format TEXT PRIMARY KEY NOT NULL);
         INSERT INTO core_metadata VALUES ('cayw-core/2');
         CREATE TABLE core_sessions (id TEXT PRIMARY KEY NOT NULL, projection TEXT NOT NULL);
         CREATE TABLE core_events (session_id TEXT NOT NULL REFERENCES core_sessions(id),cursor INTEGER NOT NULL CHECK(cursor>0),record TEXT NOT NULL,PRIMARY KEY(session_id,cursor));
         CREATE TRIGGER core_events_no_update BEFORE UPDATE ON core_events BEGIN SELECT RAISE(ABORT,'core events are append-only'); END;
         CREATE TRIGGER core_events_no_delete BEFORE DELETE ON core_events BEGIN SELECT RAISE(ABORT,'core events are append-only'); END;"
    ).unwrap();
    db.execute(
        "INSERT INTO core_sessions VALUES ('v2-session',?1)",
        [projection.to_string()],
    )
    .unwrap();
    for event in events {
        db.execute(
            "INSERT INTO core_events VALUES ('v2-session',?1,?2)",
            params![event["cursor"].as_i64().unwrap(), event.to_string()],
        )
        .unwrap();
    }
    if let Some(sql) = extra_schema {
        db.execute_batch(sql).unwrap();
    }
}

#[test]
fn v2_migration_preserves_independent_effects_without_guessing_wait_bindings() {
    let database = Database::new();
    let (projection, events) = v2_fixture();
    write_v2(&database, &projection, &events, None);
    let store = SessionStore::open(database.path()).unwrap();
    let before = store.snapshot("v2-session").unwrap();
    assert_eq!(before.executions.len(), 1);
    assert_eq!(before.unresolved_capabilities.len(), 1);
    let migrated = store.events("v2-session", 0, 100).unwrap();
    for (actual, old) in migrated.iter().zip(&events) {
        let mut expected = old.clone();
        expected["protocol_version"] = json!(3);
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
    }
    let db = Connection::open(database.path()).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        db.query_row("SELECT format FROM core_metadata", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "cayw-core/3"
    );
    let current: String = db
        .query_row("SELECT projection FROM core_sessions", [], |r| r.get(0))
        .unwrap();
    let mut expected = projection;
    expected["execution_waits"] = json!([]);
    assert_eq!(serde_json::from_str::<Value>(&current).unwrap(), expected);
    assert!(
        db.execute("UPDATE core_events SET record='{}'", [])
            .is_err()
    );
    assert!(db.execute("DELETE FROM core_events", []).is_err());
    let recovered = store.recover("v2-session").unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(recovered.unresolved_calls.len(), 1);
    assert_eq!(recovered.unresolved_calls[0].call.id, "wait");
    assert_eq!(recovered.unresolved_capabilities.len(), 1);
    assert!(matches!(
        recovered.unresolved_capabilities[0].outcome,
        Some(cayw_core::CapabilityOutcome::OutcomeUnknown { .. })
    ));
    assert_eq!(
        SessionStore::open(database.path())
            .unwrap()
            .snapshot("v2-session")
            .unwrap(),
        recovered
    );
}

#[test]
fn v2_failed_migrations_preserve_original_rows_versions_and_guards() {
    for change in [
        "projection-waits",
        "event-waits",
        "cursor",
        "event-version",
        "event-origin",
        "write-failure",
    ] {
        let database = Database::new();
        let (mut projection, mut events) = v2_fixture();
        match change {
            "projection-waits" => projection["execution_waits"] = json!([]),
            "event-waits" => {
                events[4]["event"] = json!({"type":"execution_wait_registered","binding":{}})
            }
            "cursor" => projection["cursor"] = json!(6),
            "event-version" => events[3]["protocol_version"] = json!(3),
            "event-origin" => {
                events[3]["event"]["call"]["lease"]["worker_id"] = json!("other-worker")
            }
            _ => {}
        }
        write_v2(&database, &projection, &events, (change=="write-failure").then_some("CREATE TRIGGER reject_migration BEFORE UPDATE ON core_metadata BEGIN SELECT RAISE(ABORT,'injected migration failure'); END;"));
        let bytes = std::fs::read(database.path()).unwrap();
        assert!(SessionStore::open(database.path()).is_err(), "{change}");
        assert_eq!(std::fs::read(database.path()).unwrap(), bytes, "{change}");
        let db = Connection::open(database.path()).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
                .unwrap(),
            "delete"
        );
        assert_eq!(
            db.query_row("SELECT format FROM core_metadata", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "cayw-core/2"
        );
        assert_eq!(
            db.query_row("SELECT projection FROM core_sessions", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            projection.to_string()
        );
        let retained = db
            .prepare("SELECT record FROM core_events ORDER BY cursor")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            retained,
            events.iter().map(Value::to_string).collect::<Vec<_>>()
        );
        assert!(
            db.execute("UPDATE core_events SET record='{}'", [])
                .is_err()
        );
        assert!(db.execute("DELETE FROM core_events", []).is_err());
        if change == "write-failure" {
            db.execute_batch("DROP TRIGGER reject_migration").unwrap();
            drop(db);
            assert!(SessionStore::open(database.path()).is_ok());
        }
    }
}

#[test]
fn migration_rejects_inconsistent_origins_or_missing_logs_without_changes() {
    for change in ["task", "call", "cursor", "lease"] {
        let database = Database::new();
        let (mut projection, events) = v1_fixture();
        match change {
            "task" => {
                projection["approvals"][0]["input"]["task_id"] = json!("rewritten-task");
            }
            "call" => {
                projection["approvals"][0]["input"]["call_id"] = json!("nonexistent-call");
            }
            "cursor" => {
                projection["cursor"] = json!(6);
            }
            _ => {
                projection["attempt"]["fence"] = json!(2);
            }
        }
        write_v1(&database, &projection, &events, None);
        let bytes = std::fs::read(database.path()).unwrap();
        assert!(matches!(
            SessionStore::open(database.path()),
            Err(CoreError::CorruptStore(_))
        ));
        assert_v1_untouched(&database, &bytes, &projection, &events);
    }
}
