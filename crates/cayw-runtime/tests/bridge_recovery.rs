//! Fixed canonical evidence fixtures: no model or source replay during recovery.
use cayw_agent::AgentEvent;
use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
use cayw_bridge::{
    BindingSnapshot, DispatchOutcome, DispatchRecord, InvocationContext, Operation, Provenance,
    RequestKey, SourceIdentity, content_sha256,
};
use cayw_core::{
    ApprovalDecision, AttemptLease, CapabilityAuthorization, CapabilityCallKey, CapabilityInput,
    CapabilityOutcome, CoreEvent, ExecutionInput,
};
use cayw_runtime::{BridgeDispatchOrigin, Runtime, RuntimeConfig};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

fn open(temp: &tempfile::TempDir) -> Runtime {
    Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "bridge-evidence"),
        RuntimeConfig::default(),
    )
    .unwrap()
}
struct Fixture {
    temp: tempfile::TempDir,
    runtime: Runtime,
    id: String,
    lease: AttemptLease,
    origin: BridgeDispatchOrigin,
    record: DispatchRecord,
}
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let runtime = open(&temp);
    let id = runtime.create(None).unwrap().id;
    let lease = runtime
        .store()
        .begin_attempt(&id, "original-worker")
        .unwrap();
    let store = runtime.store();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let mut assistant = AssistantMessage::new(&Model::new("fixture", "bridge-evidence"));
    assistant.stop_reason = StopReason::ToolUse;
    let parent = ToolCall::new(
        "original-parent",
        "execute_code",
        json!({"action":"exec","script":"return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})"}),
    );
    assistant.content.push(Content::ToolCall(parent.clone()));
    for event in [
        AgentEvent::MessageStart {
            message: Message::Assistant(assistant.clone()),
        },
        AgentEvent::MessageEnd {
            message: Message::Assistant(assistant),
        },
        AgentEvent::ToolExecutionStart {
            call: parent.clone(),
        },
    ] {
        store.append_agent_event(&lease, &event).unwrap();
    }
    store.mark_tool_started(&lease, &parent.id).unwrap();
    store
        .begin_execution(
            &lease,
            ExecutionInput {
                execution_id: "original-execution".into(),
                parent_call_id: parent.id.clone(),
                task_id: "original-task".into(),
                source_hash: "original-source-hash".into(),
            },
        )
        .unwrap();
    let key = CapabilityCallKey {
        execution_id: "original-execution".into(),
        sequence: 0,
    };
    let arguments = json!({"server":"local","tool":"echo","arguments":{"text":"fixture"}});
    store
        .plan_capability(
            &lease,
            CapabilityInput {
                key: key.clone(),
                name: "mcp.call".into(),
                arguments: arguments.clone(),
            },
        )
        .unwrap();
    let provenance = Provenance {
        registry_incarnation: "original-registry".into(),
        source: SourceIdentity {
            id: "local".into(),
            version: "1".into(),
            origin: "fixed-local-fixture".into(),
        },
        generation: 3,
        manifest_sha256: content_sha256("original manifest"),
    };
    let bindings = BindingSnapshot {
        registry_incarnation: provenance.registry_incarnation.clone(),
        sources: BTreeMap::from([("local".into(), provenance.clone())]),
    };
    store
        .start_capability(
            &lease,
            &key,
            serde_json::to_value(CapabilityAuthorization {
                actor: "fixed-fixture-host".into(),
                target: key.clone(),
                task_id: "original-task".into(),
                capability: "mcp.call".into(),
                scope: arguments.to_string(),
                decision: ApprovalDecision::Approve,
                approval_id: None,
                external_provenance: Some(serde_json::to_value(&bindings).unwrap()),
            })
            .unwrap(),
        )
        .unwrap();
    let origin = BridgeDispatchOrigin {
        lease: lease.clone(),
        task_id: "original-task".into(),
        key,
        parent_call_id: parent.id,
        source_hash: "original-source-hash".into(),
        bindings,
    };
    let source_arguments = json!({"text":"fixture"});
    let record = DispatchRecord {
        context: InvocationContext {
            session_id: id.clone(),
            task_id: "original-task".into(),
            attempt_id: lease.attempt_id.clone(),
            execution_id: "original-execution".into(),
            sequence: 1,
        },
        capability: "mcp.call".into(),
        key: RequestKey {
            id: 19,
            source: provenance,
            operation: Operation::Invoke,
            target: "echo".into(),
        },
        arguments: source_arguments.clone(),
        arguments_sha256: content_sha256(&source_arguments.to_string()),
    };
    Fixture {
        temp,
        runtime,
        id,
        lease,
        origin,
        record,
    }
}
fn evidence(f: &Fixture, outcome: Option<DispatchOutcome>) -> Value {
    json!({"origin":f.origin,"record":f.record,"outcome":outcome})
}
fn append(f: &Fixture, kind: &str, data: Value) {
    f.runtime
        .store()
        .append_host_event(&f.lease, kind, data)
        .unwrap();
}
fn success(f: &Fixture) -> DispatchOutcome {
    DispatchOutcome::Succeeded {
        value: json!({"request":f.record.key,"source":f.record.key.source,"value":{"echo":"fixture"}}),
    }
}
fn assert_unknown(f: &Fixture) {
    let view = f.runtime.recover(&f.id).unwrap();
    assert_eq!(view.state, cayw_core::SessionState::NeedsReconciliation);
    assert_eq!(view.unresolved_capabilities.len(), 1);
    assert_eq!(
        view.unresolved_capabilities[0].external_provenance,
        Some(serde_json::to_value(&f.origin.bindings).unwrap())
    );
}

#[test]
fn prepare_only_crash_and_explicit_source_unknown_remain_unknown_after_reopen() {
    for explicit_unknown in [false, true] {
        let mut f = fixture();
        append(&f, "runtime_bridge_prepared", evidence(&f, None));
        if explicit_unknown {
            append(
                &f,
                "runtime_bridge_settled",
                evidence(
                    &f,
                    Some(DispatchOutcome::Unknown {
                        message: "source completion unconfirmed".into(),
                    }),
                ),
            );
        }
        f.runtime = open(&f.temp);
        assert_unknown(&f);
        assert_unknown(&f);
    }
}

#[test]
fn first_atomic_not_executed_is_valid_without_prepared_event_and_recovers_known() {
    let f = fixture();
    append(
        &f,
        "runtime_bridge_settled",
        evidence(
            &f,
            Some(DispatchOutcome::NotExecuted {
                message: "prepared future was never polled".into(),
            }),
        ),
    );
    let view = f.runtime.recover(&f.id).unwrap();
    assert_eq!(view.state, cayw_core::SessionState::Idle);
    let calls = f.runtime.events(&f.id, 0, 1000).unwrap();
    assert!(calls.iter().any(|event| matches!(&event.event, CoreEvent::CapabilityReconciled { call, .. } if matches!(call.outcome, Some(CapabilityOutcome::NotExecuted { .. })))));
    assert_eq!(
        f.runtime
            .bridge_dispatches(&f.id, 0, 1000)
            .unwrap()
            .events
            .len(),
        1
    );
}

#[test]
fn unknown_without_preparation_and_forged_title_without_full_origin_are_rejected() {
    for unknown in [true, false] {
        let f = fixture();
        let data = if unknown {
            evidence(
                &f,
                Some(DispatchOutcome::Unknown {
                    message: "uncommitted prepare".into(),
                }),
            )
        } else {
            json!({"success":true})
        };
        append(&f, "runtime_bridge_settled", data);
        assert!(f.runtime.bridge_dispatches(&f.id, 0, 1000).is_err());
        let cursor = f.runtime.get(&f.id).unwrap().cursor;
        assert!(f.runtime.recover(&f.id).is_err());
        assert_eq!(f.runtime.get(&f.id).unwrap().cursor, cursor);
    }
}

#[test]
fn altered_origin_pins_arguments_and_source_identity_cannot_repair_core() {
    for field in [
        "lease",
        "task",
        "parent",
        "source_hash",
        "pins",
        "generation",
        "version",
        "origin",
        "digest",
        "operation",
        "target",
        "arguments",
        "sequence",
    ] {
        let f = fixture();
        let mut data = evidence(&f, None);
        match field {
            "lease" => data["origin"]["lease"]["fence"] = json!(f.lease.fence + 1),
            "task" => data["origin"]["task_id"] = json!("wrong-task"),
            "parent" => data["origin"]["parent_call_id"] = json!("wrong-parent"),
            "source_hash" => data["origin"]["source_hash"] = json!("wrong-source-hash"),
            "pins" => data["origin"]["bindings"]["registry_incarnation"] = json!("replacement"),
            "generation" => data["record"]["key"]["source"]["generation"] = json!(4),
            "version" => data["record"]["key"]["source"]["source"]["version"] = json!("2"),
            "origin" => {
                data["record"]["key"]["source"]["source"]["origin"] = json!("replacement-origin")
            }
            "digest" => data["record"]["arguments_sha256"] = json!(content_sha256("wrong")),
            "operation" => data["record"]["key"]["operation"] = json!("search"),
            "target" => data["record"]["key"]["target"] = json!("replacement-tool"),
            "arguments" => {
                data["record"]["arguments"]["text"] = json!("wrong");
                data["record"]["arguments_sha256"] =
                    json!(content_sha256(&data["record"]["arguments"].to_string()));
            }
            "sequence" => data["record"]["context"]["sequence"] = json!(0),
            _ => unreachable!(),
        }
        append(&f, "runtime_bridge_prepared", data);
        assert!(
            f.runtime.recover(&f.id).is_err(),
            "accepted altered {field}"
        );
    }
}

#[test]
fn explicit_core_known_or_unknown_terminal_is_never_replaced_by_source_success() {
    for unknown in [false, true] {
        let f = fixture();
        append(&f, "runtime_bridge_prepared", evidence(&f, None));
        append(
            &f,
            "runtime_bridge_settled",
            evidence(&f, Some(success(&f))),
        );
        f.runtime
            .store()
            .settle_capability(
                &f.lease,
                &f.origin.key,
                if unknown {
                    CapabilityOutcome::OutcomeUnknown {
                        error: "host rejected result after source terminal".into(),
                    }
                } else {
                    CapabilityOutcome::Failed {
                        error: "explicit host known failure".into(),
                    }
                },
            )
            .unwrap();
        let view = f.runtime.recover(&f.id).unwrap();
        assert_eq!(
            view.state,
            if unknown {
                cayw_core::SessionState::NeedsReconciliation
            } else {
                cayw_core::SessionState::Idle
            }
        );
        assert!(
            !f.runtime
                .events(&f.id, 0, 1000)
                .unwrap()
                .iter()
                .any(|event| matches!(event.event, CoreEvent::CapabilityReconciled { .. }))
        );
    }
}

#[test]
fn failed_repair_can_resume_from_core_recovered_event_without_losing_original_eligibility() {
    let f = fixture();
    append(&f, "runtime_bridge_prepared", evidence(&f, None));
    append(
        &f,
        "runtime_bridge_settled",
        evidence(&f, Some(success(&f))),
    );
    let db = rusqlite::Connection::open(f.temp.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_bridge_repair BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='capability_reconciled' BEGIN SELECT RAISE(ABORT,'injected recovery repair failure'); END;").unwrap();
    assert!(f.runtime.recover(&f.id).is_err());
    assert!(f.runtime.get(&f.id).unwrap().attempt.is_none());
    db.execute_batch("DROP TRIGGER fail_bridge_repair;")
        .unwrap();
    let reopened = open(&f.temp);
    assert_eq!(
        reopened.recover(&f.id).unwrap().state,
        cayw_core::SessionState::Idle
    );
    assert_eq!(
        reopened.recover(&f.id).unwrap().state,
        cayw_core::SessionState::Idle
    );
}

#[test]
fn projection_only_origin_change_is_rejected_before_recovery_mutates_ownership() {
    for path in [
        "$.capabilities[0].source_hash",
        "$.capabilities[0].arguments.text",
        "$.executions[0].source_hash",
    ] {
        let f = fixture();
        append(&f, "runtime_bridge_prepared", evidence(&f, None));
        append(
            &f,
            "runtime_bridge_settled",
            evidence(&f, Some(success(&f))),
        );
        let db = rusqlite::Connection::open(f.temp.path().join("session.db")).unwrap();
        db.execute("UPDATE core_sessions SET projection=json_set(projection,?2,'projection-only-drift') WHERE id=?1", rusqlite::params![f.id,path]).unwrap();
        assert!(f.runtime.bridge_dispatches(&f.id, 0, 1000).is_err());
        assert!(f.runtime.recover(&f.id).is_err());
    }
}

#[test]
fn wrong_success_request_envelope_and_changed_terminal_record_are_rejected() {
    for changed_record in [false, true] {
        let f = fixture();
        append(&f, "runtime_bridge_prepared", evidence(&f, None));
        let mut terminal = evidence(&f, Some(success(&f)));
        if changed_record {
            terminal["record"]["key"]["id"] = json!(20);
            terminal["outcome"]["value"]["request"]["id"] = json!(20);
        } else {
            terminal["outcome"]["value"]["request"]["id"] = json!(20);
        }
        append(&f, "runtime_bridge_settled", terminal);
        assert!(f.runtime.bridge_dispatches(&f.id, 0, 1000).is_err());
        assert!(f.runtime.recover(&f.id).is_err());
    }
}

#[test]
fn stale_lease_cannot_append_after_recovery_and_page_cursor_is_core_cursor() {
    let f = fixture();
    append(&f, "runtime_bridge_prepared", evidence(&f, None));
    append(
        &f,
        "runtime_bridge_settled",
        evidence(&f, Some(success(&f))),
    );
    f.runtime.recover(&f.id).unwrap();
    assert!(
        f.runtime
            .store()
            .append_host_event(&f.lease, "runtime_bridge_prepared", evidence(&f, None))
            .is_err()
    );
    let first = f.runtime.bridge_dispatches(&f.id, 0, 1).unwrap();
    assert!(first.events.is_empty());
    assert_eq!(first.next_cursor, 1);
    let cursor = f.runtime.get(&f.id).unwrap().cursor;
    assert!(f.runtime.bridge_dispatches(&f.id, cursor + 1, 10).is_err());
    assert!(f.runtime.bridge_dispatches(&f.id, 0, 0).is_err());
    assert!(f.runtime.bridge_dispatches(&f.id, 0, 1001).is_err());
}
