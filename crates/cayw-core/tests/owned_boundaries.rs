use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{
    AssistantEvent, AssistantMessage, Content, Message, Model, SystemMessage, ToolCall,
    ToolDefinition,
};
use cayw_core::*;
use rusqlite::{Connection, params};
use serde_json::Value;

fn nested(depth: usize) -> Value {
    (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
}
fn lease() -> AttemptLease {
    AttemptLease {
        session_id: "s".into(),
        attempt_id: "a".into(),
        worker_id: "w".into(),
        fence: 1,
    }
}
#[test]
fn invalid_envelope_id_drops_owned_deep_request_without_aborting() {
    const CHILD: &str = "CAYW_CORE_ENVELOPE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "invalid_envelope_id_drops_owned_deep_request_without_aborting",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        println!(
            "child status: {:?}\n{}{}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result.status.success(),
            "ProtocolEnvelope::new rejection aborted while dropping owned deep CoreRequest"
        );
        return;
    }
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let request = CoreRequest::AppendHostEvent {
                lease: lease(),
                kind: "event".into(),
                data: nested(25_000),
            };
            assert!(ProtocolEnvelope::new("", request).is_err());
            println!("envelope constructor rejection returned normally");
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn append_only_guards_reject_replacing_an_existing_event_key() {
    let path =
        std::env::temp_dir().join(format!("cayw-core-replace-{}.sqlite", uuid::Uuid::new_v4()));
    let store = SessionStore::open(&path).unwrap();
    let session = store.create_session("original").unwrap();
    let original = store.events(&session.id, 0, 1000).unwrap();
    let db = Connection::open(&path).unwrap();
    let guards: u32 = db.query_row("SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND tbl_name='core_events' AND name IN ('core_events_no_update','core_events_no_delete')", [], |r| r.get(0)).unwrap();
    assert_eq!(guards, 2);
    assert!(
        db.execute("UPDATE core_events SET record='{}'", [])
            .is_err()
    );
    assert!(db.execute("DELETE FROM core_events", []).is_err());
    let mut replacement = original[0].clone();
    replacement.event = CoreEvent::Host {
        kind: "replacement".into(),
        data: Value::Null,
    };
    let result = db.execute(
        "INSERT OR REPLACE INTO core_events(session_id,cursor,record) VALUES(?1,?2,?3)",
        params![session.id, 1, serde_json::to_string(&replacement).unwrap()],
    );
    assert!(
        result.is_err(),
        "append-only trigger pair permitted INSERT OR REPLACE to rewrite cursor 1"
    );
    assert_eq!(store.events(&session.id, 0, 1000).unwrap(), original);
    let worker = store.begin_attempt(&session.id, "worker").unwrap();
    store
        .append_host_event(&worker, "valid_append", Value::Null)
        .unwrap();
    assert_eq!(store.events(&session.id, 0, 1000).unwrap().len(), 3);
    // Simulate an existing v3 database created before the insert guard existed.
    db.execute_batch("DROP TRIGGER core_events_no_replace")
        .unwrap();
    let reopened = SessionStore::open(&path).unwrap();
    let retained = reopened.events(&session.id, 0, 1000).unwrap();
    assert!(
        db.execute(
            "INSERT OR REPLACE INTO core_events(session_id,cursor,record) VALUES(?1,1,'{}')",
            [&session.id]
        )
        .is_err()
    );
    assert_eq!(reopened.events(&session.id, 0, 1000).unwrap(), retained);
    reopened
        .append_host_event(&worker, "after_upgrade", Value::Null)
        .unwrap();
    assert_eq!(reopened.events(&session.id, 0, 1000).unwrap().len(), 4);
    drop(reopened);
    drop(db);
    drop(store);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn owned_dispatch_deep_leaf_matrix_survives_on_small_stack() {
    const CHILD: &str = "CAYW_CORE_DEEP_DISPATCH_CHILD";
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
            let path = std::env::temp_dir().join(format!(
                "cayw-core-deep-dispatch-{}.sqlite",
                uuid::Uuid::new_v4()
            ));
            let store = SessionStore::open(&path).unwrap();
            let session = store.create_session("deep dispatch").unwrap();
            let lease = store.begin_attempt(&session.id, "worker").unwrap();
            let before = store.snapshot(&session.id).unwrap();
            let key = || CapabilityCallKey {
                execution_id: "vm".into(),
                sequence: 0,
            };
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
            }
            store
                .append_agent_event(&lease, &AgentEvent::AgentStart)
                .unwrap();
            store.events(&session.id, 0, 1000).unwrap();
            drop(store);
            std::fs::remove_file(path).unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}
