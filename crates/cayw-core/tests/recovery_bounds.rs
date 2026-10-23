use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{AssistantMessage, Content, Message, StopReason, ToolCall, Usage};
use cayw_core::{
    AttemptLease, CapabilityCallKey, CapabilityInput, CapabilityOutcome, CapabilityReconciliation,
    CoreError, CoreRequest, ExecutionOutcome, ReconciledOutcome, Reconciliation, SessionStore,
};
use serde_json::{Value, json};
use std::path::PathBuf;

fn database() -> PathBuf {
    std::env::temp_dir().join(format!("cayw-core-bounds-{}.db", uuid::Uuid::new_v4()))
}

fn nested(depth: usize) -> Value {
    let mut value = Value::Null;
    for _ in 0..depth {
        value = Value::Array(vec![value]);
    }
    value
}

fn assistant(call: ToolCall) -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::ToolCall(call)],
        provider: "offline".into(),
        model: "fixture".into(),
        response_id: None,
        response_model: None,
        usage: Usage::default(),
        usage_status: cayw_ai::UsageStatus::Unknown,
        usage_fields: Default::default(),
        provider_metadata: Default::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 1,
        duration_ms: None,
    }
}

fn started(store: &SessionStore, call: &ToolCall) -> AttemptLease {
    let session = store.create_session("bounds").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let message = Message::Assistant(assistant(call.clone()));
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
    store
        .bind_tool_origin(&lease, &call.id, "original-task")
        .unwrap();
    store.mark_tool_started(&lease, &call.id).unwrap();
    lease
}

#[test]
fn deep_reconciliation_rejects_without_committing_or_poisoning_readers() {
    let path = database();
    let store = SessionStore::open(&path).unwrap();
    let call = ToolCall::new("write", "fs.write", json!({"path":"once.txt"}));
    let lease = started(&store, &call);
    let recovered = store.recover(&lease.session_id).unwrap();
    let result = ToolResult {
        structured_content: Some(nested(130)),
        ..ToolResult::text("verified")
    };
    let response = store.reconcile(
        &lease.session_id,
        Reconciliation {
            expected: store
                .tool_reconciliation_origin(&lease.session_id, &call.id)
                .unwrap(),
            call_id: call.id,
            actor: "trusted-host".into(),
            evidence: "Host inspected the original effect".into(),
            outcome: ReconciledOutcome::Completed { result },
        },
    );
    assert!(response.is_err(), "deep results must be rejected");
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), recovered);
    assert_eq!(
        store
            .events(&lease.session_id, 0, 1000)
            .unwrap()
            .last()
            .unwrap()
            .cursor,
        recovered.cursor
    );
    assert_eq!(
        SessionStore::open(&path)
            .unwrap()
            .snapshot(&lease.session_id)
            .unwrap(),
        recovered
    );
}

#[test]
fn direct_agent_results_reject_deep_details_and_structured_content_before_mutation() {
    for details in [false, true] {
        let path = database();
        let store = SessionStore::open(&path).unwrap();
        let call = ToolCall::new("call", "read", json!({}));
        let lease = started(&store, &call);
        let before = store.snapshot(&lease.session_id).unwrap();
        let mut result = ToolResult::text("result");
        if details {
            result.details = Some(nested(130));
        } else {
            result.structured_content = Some(nested(130));
        }
        for terminal in [false, true] {
            let event = if terminal {
                AgentEvent::ToolExecutionEnd {
                    call: call.clone(),
                    result: result.clone(),
                    duration_ms: Some(1),
                }
            } else {
                AgentEvent::ToolExecutionUpdate {
                    call: call.clone(),
                    result: result.clone(),
                }
            };
            assert!(store.append_agent_event(&lease, &event).is_err());
            assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
        }
    }
}

#[test]
fn direct_message_and_agent_end_cannot_persist_deep_unprojected_values() {
    for end in [false, true] {
        let store = SessionStore::open(database()).unwrap();
        let session = store.create_session("direct events").unwrap();
        let lease = store.begin_attempt(&session.id, "worker").unwrap();
        store
            .append_agent_event(&lease, &AgentEvent::AgentStart)
            .unwrap();
        if !end {
            store
                .append_agent_event(&lease, &AgentEvent::TurnStart)
                .unwrap();
        }
        let before = store.snapshot(&session.id).unwrap();
        let message = Message::Custom {
            kind: "payload".into(),
            data: nested(130),
        };
        let event = if end {
            AgentEvent::AgentEnd {
                messages: vec![message],
            }
        } else {
            AgentEvent::MessageStart { message }
        };
        assert!(store.append_agent_event(&lease, &event).is_err());
        assert_eq!(store.snapshot(&session.id).unwrap(), before);
        store.events(&session.id, 0, 1000).unwrap();
    }
}

struct BoundsVerifier;
impl cayw_core::ReconciliationVerifier for BoundsVerifier {
    fn verify(&self, request: &cayw_core::ReconciliationRequest) -> Result<(), String> {
        match request {
            cayw_core::ReconciliationRequest::Tool { reconciliation }
                if reconciliation.actor == "host"
                    && reconciliation.evidence == "verified original effect" =>
            {
                Ok(())
            }
            _ => Err("fixture host rejected receipt".into()),
        }
    }
}

#[test]
fn bounded_reconciliation_round_trips_through_all_canonical_readers() {
    let path = database();
    let store = SessionStore::open(&path)
        .unwrap()
        .with_reconciliation_verifier(std::sync::Arc::new(BoundsVerifier));
    let call = ToolCall::new("call", "write", json!({}));
    let lease = started(&store, &call);
    store.recover(&lease.session_id).unwrap();
    let result = ToolResult {
        details: Some(nested(32)),
        structured_content: Some(nested(32)),
        ..ToolResult::text("Host verified")
    };
    let reconciled = store
        .reconcile(
            &lease.session_id,
            Reconciliation {
                expected: store
                    .tool_reconciliation_origin(&lease.session_id, &call.id)
                    .unwrap(),
                call_id: call.id,
                actor: "host".into(),
                evidence: "verified original effect".into(),
                outcome: ReconciledOutcome::Completed { result },
            },
        )
        .unwrap();
    assert!(reconciled.unresolved_calls.is_empty());
    let events = store.events(&lease.session_id, 0, 1000).unwrap();
    for event in events {
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: cayw_core::EventRecord = serde_json::from_str(&encoded).unwrap();
        assert_eq!(event, decoded);
    }
    assert_eq!(store.list_sessions().unwrap(), vec![reconciled.clone()]);
    assert_eq!(
        SessionStore::open(&path)
            .unwrap()
            .snapshot(&lease.session_id)
            .unwrap(),
        reconciled
    );
    store.recover(&lease.session_id).unwrap();
}

#[test]
fn owned_deep_input_rejection_survives_on_a_small_stack() {
    // The parent remains alive if a regression aborts a child's stack during Drop.
    const CHILD: &str = "CAYW_CORE_DEEP_DROP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("owned_deep_input_rejection_survives_on_a_small_stack")
            .arg("--nocapture")
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "owned rejected JSON must be destroyed without stack recursion"
        );
        return;
    }
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let store = SessionStore::open(database()).unwrap();
            let call = ToolCall::new("call", "write", json!({}));
            let lease = started(&store, &call);
            store.recover(&lease.session_id).unwrap();
            let response = store.reconcile(
                &lease.session_id,
                Reconciliation {
                    expected: store
                        .tool_reconciliation_origin(&lease.session_id, &call.id)
                        .unwrap(),
                    call_id: call.id,
                    actor: "host".into(),
                    evidence: "verified".into(),
                    outcome: ReconciledOutcome::Completed {
                        result: ToolResult {
                            details: Some(nested(20_000)),
                            structured_content: Some(nested(20_000)),
                            ..ToolResult::text("verified")
                        },
                    },
                },
            );
            assert!(response.is_err());
            let response = store.dispatch(CoreRequest::AppendAgentEvent {
                lease,
                event: Box::new(AgentEvent::AgentEnd {
                    messages: vec![Message::Custom {
                        kind: "deep".into(),
                        data: nested(20_000),
                    }],
                }),
            });
            assert!(matches!(
                response,
                Err(CoreError::InvalidInput(_))
                    | Err(CoreError::InvalidEvent(_))
                    | Err(CoreError::StaleLease)
            ));
            let session = store.create_session("owned JSON boundaries").unwrap();
            let lease = store.begin_attempt(&session.id, "worker").unwrap();
            assert!(
                store
                    .append_host_event(&lease, "event", nested(20_000))
                    .is_err()
            );
            assert!(
                store
                    .append_control_event(
                        &session.id,
                        session.cursor,
                        Some(&lease),
                        "host",
                        "decision",
                        nested(20_000)
                    )
                    .is_err()
            );
            let key = CapabilityCallKey {
                execution_id: "vm".into(),
                sequence: 0,
            };
            assert!(
                store
                    .plan_capability(
                        &lease,
                        CapabilityInput {
                            key: key.clone(),
                            name: "write".into(),
                            arguments: nested(20_000)
                        }
                    )
                    .is_err()
            );
            assert!(
                store
                    .start_capability(&lease, &key, nested(20_000))
                    .is_err()
            );
            assert!(
                store
                    .settle_capability(
                        &lease,
                        &key,
                        CapabilityOutcome::Completed {
                            result: nested(20_000)
                        }
                    )
                    .is_err()
            );
            assert!(
                store
                    .finish_execution(
                        &lease,
                        "vm",
                        ExecutionOutcome::Completed {
                            result: nested(20_000)
                        }
                    )
                    .is_err()
            );
            assert!(
                store
                    .reconcile_capability(
                        &session.id,
                        CapabilityReconciliation {
                            expected: Box::new(cayw_core::ReconciliationOrigin {
                                lease: lease.clone(),
                                task_id: "task".into(),
                                execution_id: "vm".into(),
                                parent_call_id: "parent".into(),
                                source_hash: "source".into()
                            }),
                            key,
                            actor: "host".into(),
                            evidence: "verified".into(),
                            outcome: CapabilityOutcome::Completed {
                                result: nested(20_000)
                            },
                            external_provenance: Some(nested(20_000)),
                        }
                    )
                    .is_err()
            );
            let before = store.snapshot(&session.id).unwrap();
            assert!(
                store
                    .append_agent_event(&lease, &AgentEvent::AgentStart)
                    .is_ok()
            );
            assert_eq!(
                store.snapshot(&session.id).unwrap().cursor,
                before.cursor + 1
            );
        })
        .unwrap()
        .join()
        .unwrap();
}
