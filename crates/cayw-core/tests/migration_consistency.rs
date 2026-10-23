use cayw_agent::AgentEvent;
use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
use cayw_core::*;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("cayw-core-consistency-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        Self(directory)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.sqlite"))
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

fn raw_rows(path: &Path, id: &str) -> (Value, Vec<Value>) {
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
        .map(|e| {
            let mut record: Value = serde_json::from_str(e).unwrap();
            record["protocol_version"] = json!(2);
            record
        })
        .collect();
    (projection, events)
}

struct MigrationVerifier;
impl ReconciliationVerifier for MigrationVerifier {
    fn verify(&self, request: &ReconciliationRequest) -> Result<(), String> {
        match request {
            ReconciliationRequest::Capability { reconciliation }
                if reconciliation.actor == "host"
                    && reconciliation.evidence == "checked original operation"
                    && reconciliation.outcome
                        == (CapabilityOutcome::Completed {
                            result: json!({"saved":true}),
                        }) =>
            {
                Ok(())
            }
            _ => Err("fixture receipt rejected".into()),
        }
    }
}

fn fixture(path: &Path) -> (SessionStore, AttemptLease, CapabilityCallKey) {
    let store = SessionStore::open(path)
        .unwrap()
        .with_reconciliation_verifier(std::sync::Arc::new(MigrationVerifier));
    let session = store.create_session("migration facts").unwrap();
    let lease = store.begin_attempt(&session.id, "original-worker").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let call = ToolCall::new(
        "launch",
        "execute_code",
        json!({"action":"exec", "script":"return 1"}),
    );
    let mut assistant = AssistantMessage::new(&Model::new("offline", "fixture"));
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![Content::ToolCall(call.clone())];
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
            &AgentEvent::ToolExecutionStart { call: call.clone() },
        )
        .unwrap();
    store.mark_tool_started(&lease, &call.id).unwrap();
    store
        .begin_execution(
            &lease,
            ExecutionInput {
                execution_id: "vm".into(),
                parent_call_id: call.id,
                task_id: "task".into(),
                source_hash: "source".into(),
            },
        )
        .unwrap();
    let key = CapabilityCallKey {
        execution_id: "vm".into(),
        sequence: 0,
    };
    store
        .plan_capability(
            &lease,
            CapabilityInput {
                key: key.clone(),
                name: "fs.write".into(),
                arguments: json!({"path":"once"}),
            },
        )
        .unwrap();
    store
        .start_capability(
            &lease,
            &key,
            serde_json::to_value(CapabilityAuthorization {
                actor: "host".into(),
                target: key.clone(),
                task_id: "task".into(),
                capability: "fs.write".into(),
                scope: "once".into(),
                decision: ApprovalDecision::Approve,
                approval_id: None,
                external_provenance: Some(json!({"operation_id":"original"})),
            })
            .unwrap(),
        )
        .unwrap();
    (store, lease, key)
}

fn assert_rejected_unchanged(path: &Path, version: u32) {
    let before = std::fs::read(path).unwrap();
    assert!(
        SessionStore::open(path).is_err(),
        "inconsistent legacy input must reject"
    );
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
    assert!(
        db.execute("UPDATE core_events SET record='{}'", [])
            .is_err()
    );
    assert!(db.execute("DELETE FROM core_events", []).is_err());
    let insert_guards: u32 = db.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND name='core_events_no_replace'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(
        insert_guards, 0,
        "failed migration must not install new DDL"
    );
}

#[test]
fn v2_projection_cannot_invent_or_change_execution_child_terminals_or_authority() {
    let db = Database::new();
    let baseline = db.path("baseline");
    let (store, lease, key) = fixture(&baseline);
    let (active_projection, active_events) = raw_rows(&baseline, &lease.session_id);
    store
        .settle_capability(
            &lease,
            &key,
            CapabilityOutcome::Completed {
                result: json!({"saved":"original"}),
            },
        )
        .unwrap();
    store
        .finish_execution(
            &lease,
            "vm",
            ExecutionOutcome::Completed {
                result: json!("original"),
            },
        )
        .unwrap();
    let (settled_projection, settled_events) = raw_rows(&baseline, &lease.session_id);
    for change in [
        "child-terminal",
        "execution-terminal",
        "authorization",
        "provenance",
        "missing-child-log",
        "missing-execution-log",
        "duplicate-child",
        "duplicate-execution",
        "changed-result",
        "changed-execution-result",
        "event-authorization",
        "event-provenance",
        "invented-recovery",
        "repeated-start",
        "repeated-terminal",
        "changed-origin-in-log",
        "orphan-child",
        "duplicate-recovery-child",
    ] {
        let (mut projection, mut events) =
            if change.starts_with("changed-") || change == "repeated-terminal" {
                (settled_projection.clone(), settled_events.clone())
            } else {
                (active_projection.clone(), active_events.clone())
            };
        match change {
            "child-terminal" => {
                projection["capabilities"][0]["stage"] = json!("settled");
                projection["capabilities"][0]["outcome"] =
                    json!({"type":"completed","result":{"saved":true}});
            }
            "execution-terminal" => {
                projection["executions"][0]["state"] = json!("completed");
                projection["executions"][0]["terminal"] =
                    json!({"type":"completed","result":"invented"});
            }
            "authorization" => {
                projection["capabilities"][0]["authorization"]["actor"] = json!("different-host")
            }
            "provenance" => {
                projection["capabilities"][0]["external_provenance"] =
                    json!({"operation_id":"different"})
            }
            "missing-child-log" => {
                projection["capabilities"] = json!([]);
            }
            "missing-execution-log" => {
                projection["executions"] = json!([]);
            }
            "duplicate-child" => {
                let extra = projection["capabilities"][0].clone();
                projection["capabilities"]
                    .as_array_mut()
                    .unwrap()
                    .push(extra);
            }
            "duplicate-execution" => {
                let extra = projection["executions"][0].clone();
                projection["executions"].as_array_mut().unwrap().push(extra);
            }
            "changed-result" => {
                projection["capabilities"][0]["outcome"]["result"] = json!({"saved":"different"})
            }
            "changed-execution-result" => {
                projection["executions"][0]["terminal"]["result"] = json!("different")
            }
            "event-authorization" => {
                projection["capabilities"][0]["authorization"]["actor"] = json!("different-host");
                let mut call = projection["capabilities"][0].clone();
                call["stage"] = json!("settled");
                call["outcome"] = json!({"type":"completed","result":null});
                projection["capabilities"][0] = call.clone();
                let mut record = events.last().unwrap().clone();
                record["cursor"] = json!(events.len() + 1);
                record["event"] = json!({"type":"capability_settled","call":call});
                events.push(record);
                projection["cursor"] = json!(events.len());
            }
            "event-provenance" => {
                projection["capabilities"][0]["external_provenance"] =
                    json!({"operation_id":"different"});
                projection["capabilities"][0]["authorization"]["external_provenance"] =
                    json!({"operation_id":"different"});
                let mut call = projection["capabilities"][0].clone();
                call["stage"] = json!("settled");
                call["outcome"] = json!({"type":"completed","result":null});
                projection["capabilities"][0] = call.clone();
                let mut record = events.last().unwrap().clone();
                record["cursor"] = json!(events.len() + 1);
                record["event"] = json!({"type":"capability_settled","call":call});
                events.push(record);
                projection["cursor"] = json!(events.len());
            }
            "repeated-start" | "repeated-terminal" => {
                let kind = if change == "repeated-start" {
                    "capability_started"
                } else {
                    "execution_finished"
                };
                let mut record = events
                    .iter()
                    .find(|r| r["event"]["type"] == kind)
                    .unwrap()
                    .clone();
                record["cursor"] = json!(events.len() + 1);
                events.push(record);
                projection["cursor"] = json!(events.len());
            }
            "changed-origin-in-log" => {
                projection["executions"][0]["source_hash"] = json!("replaced-origin");
                events
                    .iter_mut()
                    .find(|r| r["event"]["type"] == "execution_finished")
                    .unwrap()["event"]["execution"] = projection["executions"][0].clone();
            }
            "orphan-child" => {
                projection["capabilities"][0]["key"]["execution_id"] = json!("missing-execution");
                for event in events.iter_mut().filter(|r| {
                    r["event"]["type"] == "capability_planned"
                        || r["event"]["type"] == "capability_started"
                }) {
                    event["event"]["call"]["key"]["execution_id"] = json!("missing-execution");
                }
            }
            "duplicate-recovery-child" => {
                projection["capabilities"][0]["stage"] = json!("settled");
                projection["capabilities"][0]["outcome"] =
                    json!({"type":"outcome_unknown","error":"worker lost"});
                projection["executions"][0]["state"] = json!("interrupted");
                projection["executions"][0]["terminal"] =
                    json!({"type":"interrupted","reason":"worker lost"});
                projection["attempt"] = Value::Null;
                let mut record = events.last().unwrap().clone();
                record["cursor"] = json!(events.len() + 1);
                record["event"] = json!({"type":"attempt_recovered","lease":lease,"unknown_call_ids":[],"not_executed_call_ids":[],"executions":[projection["executions"][0].clone()],"capabilities":[projection["capabilities"][0].clone(),projection["capabilities"][0].clone()]});
                events.push(record);
                projection["cursor"] = json!(events.len());
            }
            _ => {
                projection["capabilities"][0]["stage"] = json!("settled");
                projection["capabilities"][0]["outcome"] =
                    json!({"type":"not_executed","reason":"invented"});
                let mut record = events.last().unwrap().clone();
                record["cursor"] = json!(events.len() + 1);
                record["event"] = json!({"type":"attempt_recovered","lease":lease,"unknown_call_ids":[],"not_executed_call_ids":[],"executions":[],"capabilities":[projection["capabilities"][0].clone()]});
                events.push(record);
                projection["cursor"] = json!(events.len());
            }
        }
        let path = db.path(change);
        legacy(&path, 2, &projection, &events);
        assert_rejected_unchanged(&path, 2);
    }
}

#[test]
fn v2_logged_known_unknown_recovered_and_reconciled_results_migrate_unchanged() {
    for terminal in ["completed", "unknown", "recovered", "reconciled"] {
        let db = Database::new();
        let baseline = db.path("baseline");
        let (store, lease, key) = fixture(&baseline);
        if terminal == "completed" {
            store
                .settle_capability(
                    &lease,
                    &key,
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
                        result: json!("done"),
                    },
                )
                .unwrap();
        } else if terminal == "unknown" {
            store
                .settle_capability(
                    &lease,
                    &key,
                    CapabilityOutcome::OutcomeUnknown {
                        error: "lost acknowledgement".into(),
                    },
                )
                .unwrap();
            store
                .finish_execution(
                    &lease,
                    "vm",
                    ExecutionOutcome::Interrupted {
                        reason: "VM lost".into(),
                    },
                )
                .unwrap();
        } else {
            store.recover(&lease.session_id).unwrap();
            if terminal == "reconciled" {
                store
                    .reconcile_capability(
                        &lease.session_id,
                        CapabilityReconciliation {
                            expected: Box::new(ReconciliationOrigin {
                                lease: lease.clone(),
                                task_id: "task".into(),
                                execution_id: "vm".into(),
                                parent_call_id: "launch".into(),
                                source_hash: "source".into(),
                            }),
                            key,
                            actor: "host".into(),
                            evidence: "checked original operation".into(),
                            outcome: CapabilityOutcome::Completed {
                                result: json!({"saved":true}),
                            },
                            external_provenance: Some(json!({"operation_id":"original"})),
                        },
                    )
                    .unwrap();
            }
        }
        let expected = store.snapshot(&lease.session_id).unwrap();
        let (projection, events) = raw_rows(&baseline, &lease.session_id);
        let path = db.path("legacy");
        legacy(&path, 2, &projection, &events);
        let migrated = SessionStore::open(&path).unwrap();
        assert_eq!(migrated.snapshot(&lease.session_id).unwrap(), expected);
        let actual = migrated.events(&lease.session_id, 0, 1000).unwrap();
        for (record, old) in actual.iter().zip(&events) {
            let mut expected = old.clone();
            expected["protocol_version"] = json!(PROTOCOL_VERSION);
            assert_eq!(serde_json::to_value(record).unwrap(), expected);
        }
        assert_eq!(actual.len(), events.len());
    }
}

#[test]
fn weak_v3_reconciliation_events_without_expected_are_rejected_without_changing_storage() {
    struct ExactToolReceipt(ReconciliationRequest);
    impl ReconciliationVerifier for ExactToolReceipt {
        fn verify(&self, request: &ReconciliationRequest) -> Result<(), String> {
            if request == &self.0 {
                Ok(())
            } else {
                Err("request has no exact fixture receipt".into())
            }
        }
    }
    for kind in ["reconciled", "capability_reconciled"] {
        let db = Database::new();
        let baseline = db.path("complete-v3");
        let (store, lease, key) = fixture(&baseline);
        store
            .append_agent_event(
                &lease,
                &AgentEvent::ToolExecutionEnd {
                    call: ToolCall::new(
                        "launch",
                        "execute_code",
                        json!({"action":"exec", "script":"return 1"}),
                    ),
                    result: cayw_agent::ToolResult {
                        outcome_unknown: true,
                        ..cayw_agent::ToolResult::error("original tool reply was lost")
                    },
                    duration_ms: None,
                },
            )
            .unwrap();
        store.recover(&lease.session_id).unwrap();
        store
            .reconcile_capability(
                &lease.session_id,
                CapabilityReconciliation {
                    key,
                    expected: Box::new(ReconciliationOrigin {
                        lease: lease.clone(),
                        task_id: "task".into(),
                        execution_id: "vm".into(),
                        parent_call_id: "launch".into(),
                        source_hash: "source".into(),
                    }),
                    actor: "host".into(),
                    evidence: "checked original operation".into(),
                    outcome: CapabilityOutcome::Completed {
                        result: json!({"saved":true}),
                    },
                    external_provenance: Some(json!({"operation_id":"original"})),
                },
            )
            .unwrap();
        let reconciliation = Reconciliation {
            call_id: "launch".into(),
            expected: store
                .tool_reconciliation_origin(&lease.session_id, "launch")
                .unwrap(),
            actor: "host".into(),
            evidence: "checked original tool".into(),
            outcome: ReconciledOutcome::Completed {
                result: cayw_agent::ToolResult::text("fixture operation completed"),
            },
        };
        store
            .clone()
            .with_reconciliation_verifier(std::sync::Arc::new(ExactToolReceipt(
                ReconciliationRequest::Tool {
                    reconciliation: reconciliation.clone(),
                },
            )))
            .reconcile(&lease.session_id, reconciliation)
            .unwrap();
        let (mut projection, mut events) = raw_rows(&baseline, &lease.session_id);
        projection["execution_waits"] = json!([]);
        let mut removed = 0;
        for event in &mut events {
            event["protocol_version"] = json!(PROTOCOL_VERSION);
            if event["event"]["type"] == kind {
                assert!(
                    event["event"]["reconciliation"]
                        .as_object_mut()
                        .unwrap()
                        .remove("expected")
                        .is_some()
                );
                removed += 1;
            }
        }
        assert_eq!(removed, 1);
        let path = db.path(kind);
        legacy(&path, SCHEMA_VERSION, &projection, &events);
        let raw_before = {
            let db = Connection::open(&path).unwrap();
            let projection: String = db
                .query_row("SELECT projection FROM core_sessions", [], |row| row.get(0))
                .unwrap();
            let records: Vec<String> = db
                .prepare("SELECT record FROM core_events ORDER BY cursor")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            (projection, records)
        };
        assert_rejected_unchanged(&path, SCHEMA_VERSION);
        let db = Connection::open(&path).unwrap();
        let projection: String = db
            .query_row("SELECT projection FROM core_sessions", [], |row| row.get(0))
            .unwrap();
        let records: Vec<String> = db
            .prepare("SELECT record FROM core_events ORDER BY cursor")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!((projection, records), raw_before);
    }
}

#[test]
fn v2_approval_facts_replay_without_allowing_projection_or_receipt_changes() {
    let db = Database::new();
    let baseline = db.path("approval-baseline");
    let (store, lease, _) = fixture(&baseline);
    let key = CapabilityCallKey {
        execution_id: "vm".into(),
        sequence: 1,
    };
    store
        .plan_capability(
            &lease,
            CapabilityInput {
                key: key.clone(),
                name: "fs.write".into(),
                arguments: json!({"path":"approved"}),
            },
        )
        .unwrap();
    let approval = store
        .request_approval(
            &lease,
            ApprovalInput {
                task_id: "task".into(),
                target: ApprovalTarget::CapabilityCall { key: key.clone() },
                capability: "fs.write".into(),
                scope: "once".into(),
                reason: "write approved file".into(),
            },
        )
        .unwrap();
    store
        .decide_approval(
            &lease.session_id,
            &approval.id,
            ApprovalDecision::Approve,
            "reviewer",
        )
        .unwrap();
    store
        .start_capability(
            &lease,
            &key,
            serde_json::to_value(CapabilityAuthorization {
                actor: "host".into(),
                target: key.clone(),
                task_id: "task".into(),
                capability: "fs.write".into(),
                scope: "once".into(),
                decision: ApprovalDecision::Approve,
                approval_id: Some(approval.id.clone()),
                external_provenance: Some(json!({"operation_id":"approved"})),
            })
            .unwrap(),
        )
        .unwrap();
    let expected = store.snapshot(&lease.session_id).unwrap();
    let (projection, events) = raw_rows(&baseline, &lease.session_id);
    let control = db.path("approval-control");
    legacy(&control, 2, &projection, &events);
    assert_eq!(
        SessionStore::open(&control)
            .unwrap()
            .snapshot(&lease.session_id)
            .unwrap(),
        expected
    );
    for change in ["missing", "duplicate", "decision", "target", "receipt"] {
        let mut projection = projection.clone();
        let mut events = events.clone();
        match change {
            "missing" => projection["approvals"] = json!([]),
            "duplicate" => {
                let extra = projection["approvals"][0].clone();
                projection["approvals"].as_array_mut().unwrap().push(extra);
            }
            "decision" => {
                projection["approvals"][0]["decision"]["actor"] = json!("invented-reviewer")
            }
            "target" => {
                projection["approvals"][0]["input"]["target"]["key"]["sequence"] = json!(0);
                let decision = events
                    .iter_mut()
                    .find(|r| r["event"]["type"] == "approval_decided")
                    .unwrap();
                decision["event"]["approval"] = projection["approvals"][0].clone();
            }
            _ => {
                projection["capabilities"][1]["authorization"]["approval_id"] =
                    json!("absent-receipt");
                let started = events
                    .iter_mut()
                    .rev()
                    .find(|r| r["event"]["type"] == "capability_started")
                    .unwrap();
                started["event"]["call"] = projection["capabilities"][1].clone();
            }
        }
        let path = db.path(change);
        legacy(&path, 2, &projection, &events);
        assert_rejected_unchanged(&path, 2);
    }
}

#[test]
fn migration_validates_value_limits_in_events_and_projections_and_retains_original_guards() {
    for version in [1, 2] {
        for placement in ["event", "projection"] {
            for shape in ["depth", "nodes", "bytes"] {
                let db = Database::new();
                let path = db.path("limits");
                let value = match shape {
                    "depth" => {
                        (0..MAX_METADATA_DEPTH + 1).fold(Value::Null, |v, _| Value::Array(vec![v]))
                    }
                    "nodes" => Value::Array(vec![Value::Null; MAX_METADATA_NODES]),
                    _ => Value::String("x".repeat(MAX_METADATA_BYTES + 1)),
                };
                let mut projection = json!({"id":"legacy","label":"legacy","cursor":2,"fence":0,"attempt":null,"lifecycle":null,"history":[],"calls":[],"approvals":[]});
                if version == 2 {
                    projection["executions"] = json!([]);
                    projection["capabilities"] = json!([]);
                }
                let mut events = vec![
                    json!({"protocol_version":version,"session_id":"legacy","cursor":1,"timestamp":1,"attempt":null,"event":{"type":"session_created","label":"legacy"}}),
                    json!({"protocol_version":version,"session_id":"legacy","cursor":2,"timestamp":2,"attempt":null,"event":{"type":"host","kind":"data","data":null}}),
                ];
                if placement == "event" {
                    events[1]["event"]["data"] = value;
                } else {
                    projection["history"] = json!([{"type":"message","message":{"role":"custom","kind":"data","data":value}}]);
                }
                legacy(&path, version, &projection, &events);
                assert_rejected_unchanged(&path, version);
            }
        }
    }
}

#[test]
fn successful_migrations_install_insert_guard_without_blocking_valid_append() {
    for version in [1, 2] {
        let db = Database::new();
        let path = db.path("valid");
        let mut projection = json!({"id":"legacy","label":"legacy","cursor":1,"fence":0,"attempt":null,"lifecycle":null,"history":[],"calls":[],"approvals":[]});
        if version == 2 {
            projection["executions"] = json!([]);
            projection["capabilities"] = json!([]);
        }
        let events = vec![
            json!({"protocol_version":version,"session_id":"legacy","cursor":1,"timestamp":1,"attempt":null,"event":{"type":"session_created","label":"legacy"}}),
        ];
        legacy(&path, version, &projection, &events);
        let store = SessionStore::open(&path).unwrap();
        let before = store.events("legacy", 0, 1000).unwrap();
        let connection = Connection::open(&path).unwrap();
        assert!(connection.execute("INSERT OR REPLACE INTO core_events(session_id,cursor,record) VALUES('legacy',1,'{}')", []).is_err());
        assert!(
            connection
                .execute("UPDATE core_events SET record='{}'", [])
                .is_err()
        );
        assert!(connection.execute("DELETE FROM core_events", []).is_err());
        assert_eq!(store.events("legacy", 0, 1000).unwrap(), before);
        let lease = store.begin_attempt("legacy", "worker").unwrap();
        store.append_host_event(&lease, "valid", json!({})).unwrap();
        assert_eq!(
            store.events("legacy", 0, 1000).unwrap().len(),
            before.len() + 2
        );
    }
}
