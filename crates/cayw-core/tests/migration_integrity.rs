//! Regression cases from the independent v3 review. Fixtures are local SQLite data.
use cayw_agent::AgentEvent;
use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
use cayw_core::*;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("cayw-core-integrity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
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
fn nested(depth: usize) -> Value {
    (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
}
fn legacy(path: &Path, version: u32, projection: &Value, events: &[Value]) {
    let db = Connection::open(path).unwrap();
    db.execute_batch("PRAGMA application_id=1128356183;
        CREATE TABLE core_metadata(format TEXT PRIMARY KEY NOT NULL);
        CREATE TABLE core_sessions(id TEXT PRIMARY KEY NOT NULL,projection TEXT NOT NULL);
        CREATE TABLE core_events(session_id TEXT NOT NULL REFERENCES core_sessions(id),cursor INTEGER NOT NULL CHECK(cursor>0),record TEXT NOT NULL,PRIMARY KEY(session_id,cursor));
        CREATE TRIGGER core_events_no_update BEFORE UPDATE ON core_events BEGIN SELECT RAISE(ABORT,'core events are append-only'); END;
        CREATE TRIGGER core_events_no_delete BEFORE DELETE ON core_events BEGIN SELECT RAISE(ABORT,'core events are append-only'); END;").unwrap();
    db.pragma_update(None, "user_version", version).unwrap();
    db.execute(
        "INSERT INTO core_metadata VALUES(?1)",
        [format!("cayw-core/{version}")],
    )
    .unwrap();
    db.execute(
        "INSERT INTO core_sessions VALUES(?1,?2)",
        params![projection["id"].as_str().unwrap(), projection.to_string()],
    )
    .unwrap();
    for event in events {
        db.execute(
            "INSERT INTO core_events VALUES(?1,?2,?3)",
            params![
                event["session_id"].as_str().unwrap(),
                event["cursor"].as_i64().unwrap(),
                event.to_string()
            ],
        )
        .unwrap();
    }
}
fn assert_untouched(
    path: &Path,
    before: &[u8],
    version: u32,
    projection: &Value,
    events: &[Value],
) {
    assert_eq!(std::fs::read(path).unwrap(), before);
    let db = Connection::open(path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        version
    );
    assert_eq!(
        db.query_row("SELECT format FROM core_metadata", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        format!("cayw-core/{version}")
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
}
fn minimum(version: u32, data: Value, in_projection: bool) -> (Value, Vec<Value>) {
    let mut projection = json!({
        "id":"legacy", "label":"legacy", "cursor":2, "fence":0,
        "attempt":null,"lifecycle":null,"history":[],"calls":[],"approvals":[]
    });
    if version == 2 {
        projection["executions"] = json!([]);
        projection["capabilities"] = json!([]);
    }
    if in_projection {
        projection["history"] = json!([{"type":"message","message":{"role":"custom","kind":"legacy-data","data":data.clone()}}]);
    }
    let events = vec![
        json!({"protocol_version":version,"session_id":"legacy","cursor":1,"timestamp":1,"attempt":null,"event":{"type":"session_created","label":"legacy"}}),
        json!({"protocol_version":version,"session_id":"legacy","cursor":2,"timestamp":2,"attempt":null,"event":{"type":"host","kind":"legacy-data","data":data}}),
    ];
    (projection, events)
}

#[test]
fn migration_must_not_commit_data_rejected_by_its_readers() {
    let mut violations = vec![];
    for version in [1, 2] {
        for shape in ["depth", "nodes", "bytes"] {
            for in_projection in [false, true] {
                let value = match shape {
                    "depth" => nested(MAX_METADATA_DEPTH + 1),
                    "nodes" => Value::Array(vec![Value::Null; MAX_METADATA_NODES]),
                    _ => Value::String("x".repeat(MAX_METADATA_BYTES + 1)),
                };
                let (projection, events) = minimum(version, value, in_projection);
                let database = Database::new();
                let path = database.path();
                legacy(&path, version, &projection, &events);
                let before = std::fs::read(&path).unwrap();
                if SessionStore::open(&path).is_ok() {
                    violations.push(format!("v{version} {shape} projection={in_projection}"));
                } else {
                    assert_untouched(&path, &before, version, &projection, &events);
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "migration committed {} unreadable cases: {violations:?}",
        violations.len()
    );
}

#[test]
fn bounded_legacy_values_migrate_and_remain_readable() {
    for version in [1, 2] {
        for value in [
            nested(MAX_METADATA_DEPTH),
            Value::Array(vec![Value::Null; MAX_METADATA_NODES - 1]),
            Value::String("x".repeat(MAX_METADATA_BYTES - 2)),
        ] {
            for in_projection in [false, true] {
                let (projection, events) = minimum(version, value.clone(), in_projection);
                let database = Database::new();
                let path = database.path();
                legacy(&path, version, &projection, &events);
                let store = SessionStore::open(&path).unwrap();
                let snapshot = store.snapshot("legacy").unwrap();
                assert_eq!(store.list_sessions().unwrap(), vec![snapshot.clone()]);
                assert_eq!(store.recover("legacy").unwrap(), snapshot);
                assert_eq!(store.events("legacy", 0, 1000).unwrap().len(), 2);
                assert_eq!(
                    SessionStore::open(&path)
                        .unwrap()
                        .snapshot("legacy")
                        .unwrap(),
                    snapshot
                );
            }
        }
    }
}
fn key() -> CapabilityCallKey {
    CapabilityCallKey {
        execution_id: "vm".into(),
        sequence: 0,
    }
}
fn authorization() -> Value {
    serde_json::to_value(CapabilityAuthorization {
        actor: "host".into(),
        target: key(),
        task_id: "original-task".into(),
        capability: "fs.write".into(),
        scope: "once".into(),
        decision: ApprovalDecision::Approve,
        approval_id: None,
        external_provenance: Some(json!({"operation_id":"original-operation"})),
    })
    .unwrap()
}
fn started_fixture(path: &Path) -> (SessionStore, AttemptLease) {
    let store = SessionStore::open(path).unwrap();
    let session = store.create_session("independent").unwrap();
    let lease = store.begin_attempt(&session.id, "original-worker").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let launch = ToolCall::new(
        "launch",
        "execute_code",
        json!({"action":"exec","script":"return 1"}),
    );
    let mut assistant = AssistantMessage::new(&Model::new("offline", "independent"));
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![Content::ToolCall(launch.clone())];
    let message = Message::Assistant(assistant);
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: message.clone(),
            },
        )
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::MessageEnd { message })
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionStart {
                call: launch.clone(),
            },
        )
        .unwrap();
    store.mark_tool_started(&lease, &launch.id).unwrap();
    store
        .begin_execution(
            &lease,
            ExecutionInput {
                execution_id: "vm".into(),
                parent_call_id: "launch".into(),
                task_id: "original-task".into(),
                source_hash: "original-source".into(),
            },
        )
        .unwrap();
    store
        .plan_capability(
            &lease,
            CapabilityInput {
                key: key(),
                name: "fs.write".into(),
                arguments: json!({"path":"once"}),
            },
        )
        .unwrap();
    store
        .start_capability(&lease, &key(), authorization())
        .unwrap();
    (store, lease)
}
fn v2_rows(path: &Path, id: &str) -> (Value, Vec<Value>) {
    let db = Connection::open(path).unwrap();
    let projection: String = db
        .query_row(
            "SELECT projection FROM core_sessions WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    let events: Vec<String> = db
        .prepare("SELECT record FROM core_events WHERE session_id=?1 ORDER BY cursor")
        .unwrap()
        .query_map([id], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut projection: Value = serde_json::from_str(&projection).unwrap();
    projection
        .as_object_mut()
        .unwrap()
        .remove("execution_waits");
    let events = events
        .iter()
        .map(|event| {
            let mut event: Value = serde_json::from_str(event).unwrap();
            event["protocol_version"] = json!(2);
            event
        })
        .collect();
    (projection, events)
}
#[test]
fn v2_migration_must_not_hide_a_child_without_a_logged_terminal() {
    let baseline = Database::new();
    let (_store, lease) = started_fixture(&baseline.path());
    let (mut projection, events) = v2_rows(&baseline.path(), &lease.session_id);
    let control = Database::new();
    legacy(&control.path(), 2, &projection, &events);
    let control_store = SessionStore::open(control.path()).unwrap();
    let control_recovered = control_store.recover(&lease.session_id).unwrap();
    assert_eq!(control_recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(control_recovered.unresolved_capabilities.len(), 1);
    projection["capabilities"][0]["stage"] = json!("settled");
    projection["capabilities"][0]["outcome"] = json!({"type":"completed","result":{"saved":true}});
    let database = Database::new();
    let path = database.path();
    legacy(&path, 2, &projection, &events);
    let before = std::fs::read(&path).unwrap();
    assert!(
        SessionStore::open(&path).is_err(),
        "v2 migration accepted a child terminal absent from its append-only log"
    );
    assert_untouched(&path, &before, 2, &projection, &events);
}
#[test]
fn v2_migration_must_preserve_authorization_provenance() {
    let baseline = Database::new();
    let (_store, lease) = started_fixture(&baseline.path());
    let (projection, events) = v2_rows(&baseline.path(), &lease.session_id);
    for field in ["external_provenance", "authorization"] {
        let mut projection = projection.clone();
        if field == "external_provenance" {
            projection["capabilities"][0][field] = json!({"operation_id":"different-operation"});
        } else {
            projection["capabilities"][0][field]["actor"] = json!("different-host");
        }
        let database = Database::new();
        let path = database.path();
        legacy(&path, 2, &projection, &events);
        let before = std::fs::read(&path).unwrap();
        assert!(
            SessionStore::open(&path).is_err(),
            "v2 migration accepted provenance that differs from original CapabilityStarted ({field})"
        );
        assert_untouched(&path, &before, 2, &projection, &events);
    }
}

#[test]
fn v2_final_execution_and_child_facts_must_match_the_log() {
    let baseline = Database::new();
    let (store, lease) = started_fixture(&baseline.path());
    store
        .settle_capability(
            &lease,
            &key(),
            CapabilityOutcome::Completed {
                result: json!({"saved":true}),
            },
        )
        .unwrap();
    store
        .finish_execution(
            &lease,
            "vm",
            ExecutionOutcome::Completed {
                result: json!("finished"),
            },
        )
        .unwrap();
    let (projection, events) = v2_rows(&baseline.path(), &lease.session_id);
    let control = Database::new();
    legacy(&control.path(), 2, &projection, &events);
    let migrated = SessionStore::open(control.path()).unwrap();
    assert_eq!(
        migrated.recover(&lease.session_id).unwrap().state,
        SessionState::Idle
    );
    for change in [
        "execution-terminal",
        "execution-state",
        "child-outcome",
        "missing-execution",
        "missing-child",
        "duplicate-child",
        "event-authorization",
        "event-provenance",
    ] {
        let mut projection = projection.clone();
        let mut events = events.clone();
        match change {
            "execution-terminal" => {
                projection["executions"][0]["terminal"]["result"] = json!("invented")
            }
            "execution-state" => projection["executions"][0]["state"] = json!("running"),
            "child-outcome" => {
                projection["capabilities"][0]["outcome"]["result"] = json!("invented")
            }
            "missing-execution" => projection["executions"] = json!([]),
            "missing-child" => projection["capabilities"] = json!([]),
            "duplicate-child" => {
                let child = projection["capabilities"][0].clone();
                projection["capabilities"]
                    .as_array_mut()
                    .unwrap()
                    .push(child);
            }
            "event-authorization" | "event-provenance" => {
                let event = events
                    .iter_mut()
                    .find(|event| event["event"]["type"] == "capability_started")
                    .unwrap();
                if change == "event-authorization" {
                    event["event"]["call"]["authorization"]["actor"] = json!("other-host");
                } else {
                    event["event"]["call"]["external_provenance"] =
                        json!({"operation_id":"other-operation"});
                }
            }
            _ => unreachable!(),
        }
        let database = Database::new();
        let path = database.path();
        legacy(&path, 2, &projection, &events);
        let before = std::fs::read(&path).unwrap();
        assert!(SessionStore::open(&path).is_err(), "{change}");
        assert_untouched(&path, &before, 2, &projection, &events);
    }
}
