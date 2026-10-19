use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{
    AssistantEvent, AssistantMessage, Content, Message, Model, StopReason, SystemMessage, ToolCall,
    ToolDefinition,
};
use cayw_core::*;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Debug)]
struct ProbeDatabase(PathBuf);
impl std::ops::Deref for ProbeDatabase {
    type Target = PathBuf;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl AsRef<std::path::Path> for ProbeDatabase {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for ProbeDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}
fn database(tag: &str) -> ProbeDatabase {
    let directory =
        std::env::temp_dir().join(format!("cayw-core-independent-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    ProbeDatabase(directory.join(format!("{tag}.sqlite")))
}
fn nested(depth: usize) -> Value {
    (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
}
fn assistant(calls: &[ToolCall]) -> AssistantMessage {
    let mut message = AssistantMessage::new(&Model::new("offline", "independent"));
    message.stop_reason = StopReason::ToolUse;
    message.content = calls.iter().cloned().map(Content::ToolCall).collect();
    message
}
fn start(store: &SessionStore, lease: &AttemptLease, call: &ToolCall) {
    store
        .append_agent_event(
            lease,
            &AgentEvent::ToolExecutionStart { call: call.clone() },
        )
        .unwrap();
    store.mark_tool_started(lease, &call.id).unwrap();
}
fn fixture(path: &PathBuf, extra: &[ToolCall]) -> (SessionStore, AttemptLease) {
    let store = SessionStore::open(path).unwrap();
    let id = store.create_session("independent").unwrap().id;
    let lease = store.begin_attempt(&id, "original-worker").unwrap();
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
    let mut calls = vec![launch.clone()];
    calls.extend_from_slice(extra);
    let message = Message::Assistant(assistant(&calls));
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
    start(&store, &lease, &launch);
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
    (store, lease)
}
fn wait() -> ToolCall {
    ToolCall::new(
        "wait",
        "execute_code",
        json!({
            "action":"wait","cell_id":"vm","cursor":0,"yield_time_ms":0,"max_output_tokens":1024
        }),
    )
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
fn raw_rows(path: &PathBuf, id: &str) -> (Value, Vec<Value>) {
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
    (
        serde_json::from_str(&projection).unwrap(),
        events
            .iter()
            .map(|e| serde_json::from_str(e).unwrap())
            .collect(),
    )
}
fn legacy(path: &PathBuf, version: u32, projection: &Value, events: &[Value]) {
    std::fs::write(
        path.with_extension("input.json"),
        serde_json::to_vec_pretty(
            &json!({"version":version,"projection":projection,"events":events}),
        )
        .unwrap(),
    )
    .unwrap();
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
                let path = database(&format!("migration-v{version}-{shape}-{in_projection}"));
                legacy(&path, version, &projection, &events);
                let before = std::fs::read(&path).unwrap();
                match SessionStore::open(&path) {
                    Ok(store) => {
                        let snapshot = store.snapshot("legacy");
                        let events = store.events("legacy", 0, 1000);
                        let list = store.list_sessions();
                        let recover = store.recover("legacy");
                        let reopened = SessionStore::open(&path).unwrap().snapshot("legacy");
                        let db = Connection::open(&path).unwrap();
                        let committed: u32 = db
                            .pragma_query_value(None, "user_version", |r| r.get(0))
                            .unwrap();
                        let text = format!(
                            "{path:?}: v{version} {shape} projection={in_projection}, committed={committed}, snapshot={:?}, events={:?}, list={:?}, recover={:?}, reopened={:?}",
                            snapshot.as_ref().err(),
                            events.as_ref().err(),
                            list.as_ref().err(),
                            recover.as_ref().err(),
                            reopened.as_ref().err()
                        );
                        println!("{text}");
                        if snapshot.is_err()
                            || events.is_err()
                            || list.is_err()
                            || recover.is_err()
                            || reopened.is_err()
                        {
                            violations.push(text);
                        }
                    }
                    Err(error) => {
                        println!("migration refused {path:?}: {error}");
                        assert_eq!(std::fs::read(&path).unwrap(), before);
                    }
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "migration committed {} unreadable cases",
        violations.len()
    );
}

#[test]
fn v2_migration_must_not_hide_a_child_without_a_logged_terminal() {
    let baseline = database("v2-origin-baseline");
    let (store, lease) = fixture(&baseline, &[]);
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
    let (mut projection, mut events) = raw_rows(&baseline, &lease.session_id);
    projection
        .as_object_mut()
        .unwrap()
        .remove("execution_waits");
    for event in &mut events {
        event["protocol_version"] = json!(2);
    }
    let control = database("v2-matched-control");
    legacy(&control, 2, &projection, &events);
    let control_store = SessionStore::open(&control).unwrap();
    let control_recovered = control_store.recover(&lease.session_id).unwrap();
    assert_eq!(control_recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(control_recovered.unresolved_capabilities.len(), 1);
    projection["capabilities"][0]["stage"] = json!("settled");
    projection["capabilities"][0]["outcome"] = json!({"type":"completed","result":{"saved":true}});
    let path = database("v2-missing-child-terminal");
    legacy(&path, 2, &projection, &events);
    let migrated = SessionStore::open(&path);
    let accepted = migrated.is_ok();
    if let Ok(store) = migrated {
        let recovered = store.recover(&lease.session_id).unwrap();
        let next = store.begin_attempt(&lease.session_id, "replacement-worker");
        println!(
            "{path:?}: migration accepted projection-only terminal; recovered state={:?}, unresolved children={}, replacement admitted={}",
            recovered.state,
            recovered.unresolved_capabilities.len(),
            next.is_ok()
        );
        std::fs::write(
            path.with_extension("recovered.json"),
            serde_json::to_vec_pretty(&recovered).unwrap(),
        )
        .unwrap();
    }
    assert!(
        !accepted,
        "v2 migration accepted a child terminal absent from its append-only log"
    );
}

#[test]
fn v2_migration_must_preserve_authorization_provenance() {
    let baseline = database("v2-provenance-baseline");
    let (store, lease) = fixture(&baseline, &[]);
    store
        .plan_capability(
            &lease,
            CapabilityInput {
                key: key(),
                name: "fs.write".into(),
                arguments: json!({}),
            },
        )
        .unwrap();
    store
        .start_capability(&lease, &key(), authorization())
        .unwrap();
    let (mut projection, mut events) = raw_rows(&baseline, &lease.session_id);
    projection
        .as_object_mut()
        .unwrap()
        .remove("execution_waits");
    for event in &mut events {
        event["protocol_version"] = json!(2);
    }
    let changed = json!({"operation_id":"different-operation"});
    projection["capabilities"][0]["external_provenance"] = changed.clone();
    let path = database("v2-changed-provenance");
    legacy(&path, 2, &projection, &events);
    let migrated = SessionStore::open(&path);
    let accepted = migrated.is_ok();
    if let Ok(store) = migrated {
        store.recover(&lease.session_id).unwrap();
        let result = store.reconcile_capability(
            &lease.session_id,
            CapabilityReconciliation {
                key: key(),
                actor: "host".into(),
                evidence: "checked different operation".into(),
                outcome: CapabilityOutcome::Completed {
                    result: json!("different operation"),
                },
                external_provenance: Some(changed),
            },
        );
        println!(
            "{path:?}: migration accepted altered provenance; reconciliation using different operation admitted={}",
            result.is_ok()
        );
    }
    assert!(
        !accepted,
        "v2 migration accepted provenance that differs from original CapabilityStarted"
    );
}

#[test]
fn wait_uses_full_original_lease_and_preserves_unknown_children() {
    let path = database("wait-authority");
    let waiting = wait();
    let (store, lease) = fixture(&path, std::slice::from_ref(&waiting));
    start(&store, &lease, &waiting);
    let before = store.snapshot(&lease.session_id).unwrap();
    for field in ["session", "attempt", "worker", "fence"] {
        let mut forged = lease.clone();
        match field {
            "session" => forged.session_id = store.create_session("other").unwrap().id,
            "attempt" => forged.attempt_id = "forged-attempt".into(),
            "worker" => forged.worker_id = "forged-worker".into(),
            _ => forged.fence += 1,
        }
        assert!(
            store
                .register_execution_wait(
                    &forged,
                    &waiting.id,
                    "vm",
                    "original-task",
                    &waiting.arguments
                )
                .is_err(),
            "{field}"
        );
        assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    }
    for (execution, task) in [("missing", "original-task"), ("vm", "wrong-task")] {
        assert!(
            store
                .register_execution_wait(&lease, &waiting.id, execution, task, &waiting.arguments)
                .is_err()
        );
    }
    let mut changed = waiting.arguments.clone();
    changed["continue_execution"] = json!(true);
    assert!(
        store
            .register_execution_wait(&lease, &waiting.id, "vm", "original-task", &changed)
            .is_err()
    );
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_projection BEFORE UPDATE ON core_sessions BEGIN SELECT RAISE(ABORT,'independent projection fault'); END;").unwrap();
    assert!(
        store
            .register_execution_wait(
                &lease,
                &waiting.id,
                "vm",
                "original-task",
                &waiting.arguments
            )
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    assert!(
        store
            .events(&lease.session_id, before.cursor, 1000)
            .unwrap()
            .is_empty()
    );
    db.execute_batch("DROP TRIGGER reject_projection").unwrap();
    let registered = store
        .register_execution_wait(
            &lease,
            &waiting.id,
            "vm",
            "original-task",
            &waiting.arguments,
        )
        .unwrap();
    assert_eq!(
        store
            .register_execution_wait(
                &lease,
                &waiting.id,
                "vm",
                "original-task",
                &waiting.arguments
            )
            .unwrap(),
        registered
    );
    assert!(
        store
            .begin_execution(
                &lease,
                ExecutionInput {
                    execution_id: "hidden-launch".into(),
                    parent_call_id: waiting.id.clone(),
                    task_id: "original-task".into(),
                    source_hash: "other".into()
                }
            )
            .is_err()
    );
    store
        .plan_capability(
            &lease,
            CapabilityInput {
                key: key(),
                name: "fs.write".into(),
                arguments: json!({}),
            },
        )
        .unwrap();
    store
        .start_capability(&lease, &key(), authorization())
        .unwrap();
    let recovered = SessionStore::open(&path)
        .unwrap()
        .recover(&lease.session_id)
        .unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert!(recovered.unresolved_calls.is_empty());
    assert_eq!(recovered.unresolved_capabilities.len(), 1);
    assert!(store.begin_attempt(&lease.session_id, "new").is_err());
    assert!(
        db.execute("UPDATE core_events SET record='{}'", [])
            .is_err()
    );
    assert!(db.execute("DELETE FROM core_events", []).is_err());
    println!(
        "{path:?}: four lease fields, exact arguments/task, rollback, retry, launch prevention, and independent child unknown verified"
    );
}

#[test]
fn accepted_json_stays_readable_at_the_actual_typed_readers() {
    for depth in [0, 30, 31, 32, 33] {
        let path = database(&format!("reader-depth-{depth}"));
        let store = SessionStore::open(&path).unwrap();
        let id = store.create_session("reader boundaries").unwrap().id;
        let lease = store.begin_attempt(&id, "worker").unwrap();
        let before = store.snapshot(&id).unwrap();
        let event = store.append_host_event(&lease, "bounded", nested(depth));
        if depth > MAX_METADATA_DEPTH {
            assert!(event.is_err());
            assert_eq!(store.snapshot(&id).unwrap(), before);
            continue;
        }
        let event = event.unwrap();
        let encoded = serde_json::to_string(&event).unwrap();
        let _: EventRecord = serde_json::from_str(&encoded).unwrap();
        let snapshot = store.snapshot(&id).unwrap();
        assert_eq!(store.list_sessions().unwrap(), vec![snapshot.clone()]);
        assert_eq!(
            SessionStore::open(&path).unwrap().snapshot(&id).unwrap(),
            snapshot
        );
        let events = store.events(&id, 0, 1000).unwrap();
        let envelope = ProtocolEnvelope::new("read", CoreResponse::Events { events }).unwrap();
        let encoded = serde_json::to_string(&envelope).unwrap();
        let _: ProtocolEnvelope<CoreResponse> = ProtocolEnvelope::decode(&encoded).unwrap();
    }
}

#[test]
fn owned_dispatch_deep_leaf_matrix_survives_on_small_stack() {
    const CHILD: &str = "CAYW_INDEPENDENT_DEEP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "owned_dispatch_deep_leaf_matrix_survives_on_small_stack",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        println!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "deep input child exit {:?}",
            output.status
        );
        return;
    }
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let path = database("deep-dispatch");
            let store = SessionStore::open(&path).unwrap();
            let session = store.create_session("deep dispatch").unwrap();
            let lease = store.begin_attempt(&session.id, "worker").unwrap();
            let before = store.snapshot(&session.id).unwrap();
            for variant in 0..15 {
                let deep = || nested(25_000);
                let call = || ToolCall::new("invalid", "invalid", deep());
                let result = || ToolResult {
                    details: Some(deep()),
                    structured_content: Some(deep()),
                    ..ToolResult::error("untrusted")
                };
                let event = match variant {
                    0 => AgentEvent::MessageStart {
                        message: Message::System(SystemMessage {
                            tools_added: vec![ToolDefinition {
                                name: "tool".into(),
                                description: String::new(),
                                parameters: deep(),
                            }],
                            ..Default::default()
                        }),
                    },
                    1 => AgentEvent::MessageEnd {
                        message: Message::Custom {
                            kind: "custom".into(),
                            data: deep(),
                        },
                    },
                    2 => AgentEvent::AgentEnd {
                        messages: vec![Message::Custom {
                            kind: "custom".into(),
                            data: deep(),
                        }],
                    },
                    3 => AgentEvent::ToolExecutionStart { call: call() },
                    4 => AgentEvent::ToolExecutionUpdate {
                        call: call(),
                        result: result(),
                    },
                    5 => AgentEvent::ToolExecutionEnd {
                        call: call(),
                        result: result(),
                        duration_ms: None,
                    },
                    6 => {
                        let mut message = AssistantMessage::new(&Model::new("offline", "fixture"));
                        message.content = vec![Content::ToolCall(call())];
                        AgentEvent::MessageUpdate {
                            message: AssistantMessage::new(&Model::new("offline", "fixture")),
                            event: Box::new(AssistantEvent::ToolCallDelta {
                                index: 0,
                                delta: String::new(),
                                partial: message,
                            }),
                        }
                    }
                    _ => AgentEvent::AgentStart,
                };
                let request = match variant {
                    0..=6 => CoreRequest::AppendAgentEvent {
                        lease: lease.clone(),
                        event: Box::new(event),
                    },
                    7 => CoreRequest::AppendHostEvent {
                        lease: lease.clone(),
                        kind: "event".into(),
                        data: deep(),
                    },
                    8 => CoreRequest::AppendControlEvent {
                        session_id: session.id.clone(),
                        expected_cursor: before.cursor,
                        expected_attempt: Some(lease.clone()),
                        actor: "host".into(),
                        kind: "event".into(),
                        data: deep(),
                    },
                    9 => CoreRequest::RegisterExecutionWait {
                        lease: lease.clone(),
                        call_id: "wait".into(),
                        execution_id: "vm".into(),
                        task_id: "task".into(),
                        arguments: deep(),
                    },
                    10 => CoreRequest::PlanCapability {
                        lease: lease.clone(),
                        input: CapabilityInput {
                            key: key(),
                            name: "cap".into(),
                            arguments: deep(),
                        },
                    },
                    11 => CoreRequest::StartCapability {
                        lease: lease.clone(),
                        key: key(),
                        authorization: deep(),
                    },
                    12 => CoreRequest::SettleCapability {
                        lease: lease.clone(),
                        key: key(),
                        outcome: CapabilityOutcome::Completed { result: deep() },
                    },
                    13 => CoreRequest::FinishExecution {
                        lease: lease.clone(),
                        execution_id: "vm".into(),
                        outcome: ExecutionOutcome::Completed { result: deep() },
                    },
                    _ => CoreRequest::ReconcileCapability {
                        session_id: session.id.clone(),
                        reconciliation: CapabilityReconciliation {
                            key: key(),
                            actor: "host".into(),
                            evidence: "checked".into(),
                            outcome: CapabilityOutcome::Completed { result: deep() },
                            external_provenance: Some(deep()),
                        },
                    },
                };
                assert!(
                    matches!(store.dispatch(request), Err(CoreError::InvalidInput(_))),
                    "variant {variant}"
                );
                assert_eq!(store.snapshot(&session.id).unwrap(), before);
                println!("owned deep dispatch variant {variant} survived rejection");
            }
            store
                .append_agent_event(&lease, &AgentEvent::AgentStart)
                .unwrap();
            store.events(&session.id, 0, 1000).unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}
