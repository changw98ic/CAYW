use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{AssistantMessage, Content, Message, StopReason, ToolCall, Usage};
use cayw_core::{
    ApprovalDecision, AttemptLease, CallState, CapabilityAuthorization, CapabilityCallKey,
    CapabilityInput, CapabilityOutcome, CoreEvent, ExecutionInput, ExecutionOutcome,
    ExecutionState, SessionState, SessionStore,
};
use serde_json::{Value, json};
use std::path::PathBuf;

fn database() -> PathBuf {
    std::env::temp_dir().join(format!("cayw-core-wait-{}.db", uuid::Uuid::new_v4()))
}

fn assistant(calls: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        content: calls.into_iter().map(Content::ToolCall).collect(),
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

fn declare(store: &SessionStore, lease: &AttemptLease, calls: &[ToolCall]) -> AssistantMessage {
    store
        .append_agent_event(lease, &AgentEvent::TurnStart)
        .unwrap();
    let message = assistant(calls.to_vec());
    store
        .append_agent_event(
            lease,
            &AgentEvent::MessageStart {
                message: Message::Assistant(message.clone()),
            },
        )
        .unwrap();
    store
        .append_agent_event(
            lease,
            &AgentEvent::MessageEnd {
                message: Message::Assistant(message.clone()),
            },
        )
        .unwrap();
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
fn wait_arguments(continue_execution: bool) -> Value {
    json!({"action":"wait","cell_id":"vm","cursor":0,"yield_time_ms":0,"max_output_tokens":1024,"continue_execution":continue_execution})
}
fn execution(store: &SessionStore, call: &ToolCall) -> AttemptLease {
    let session = store.create_session("wait recovery").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    let message = declare(store, &lease, std::slice::from_ref(call));
    start(store, &lease, call);
    store
        .begin_execution(
            &lease,
            ExecutionInput {
                execution_id: "vm".into(),
                parent_call_id: call.id.clone(),
                task_id: "task".into(),
                source_hash: "sha256:fixture".into(),
            },
        )
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionEnd {
                call: call.clone(),
                result: ToolResult::text("VM running"),
                duration_ms: Some(1),
            },
        )
        .unwrap();
    let terminal = store
        .snapshot(&session.id)
        .unwrap()
        .messages
        .into_iter()
        .find_map(|m| {
            if let Message::ToolResult(result) = m {
                Some(result)
            } else {
                None
            }
        })
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: Message::ToolResult(terminal.clone()),
            },
        )
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageEnd {
                message: Message::ToolResult(terminal.clone()),
            },
        )
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::TurnEnd {
                message,
                tool_results: vec![terminal],
            },
        )
        .unwrap();
    lease
}
fn launch() -> ToolCall {
    ToolCall::new(
        "launch",
        "execute_code",
        json!({"action":"exec","script":"yield_control()"}),
    )
}
fn wait(id: &str, continue_execution: bool) -> ToolCall {
    ToolCall::new(id, "execute_code", wait_arguments(continue_execution))
}

#[test]
fn registered_observation_and_resume_waits_recover_without_an_extra_unknown_effect() {
    for continue_execution in [false, true] {
        let path = database();
        let store = SessionStore::open(&path).unwrap();
        let lease = execution(&store, &launch());
        let call = wait("wait", continue_execution);
        declare(&store, &lease, std::slice::from_ref(&call));
        start(&store, &lease, &call);
        let binding = store
            .register_execution_wait(&lease, &call.id, "vm", "task", &call.arguments)
            .unwrap();
        assert!(matches!(
            binding.event,
            CoreEvent::ExecutionWaitRegistered { .. }
        ));
        let bound_cursor = store.snapshot(&lease.session_id).unwrap().cursor;
        assert_eq!(
            store
                .register_execution_wait(&lease, &call.id, "vm", "task", &call.arguments)
                .unwrap(),
            binding
        );
        assert_eq!(
            store.snapshot(&lease.session_id).unwrap().cursor,
            bound_cursor
        );
        drop(store);
        let store = SessionStore::open(&path).unwrap();
        let recovered = store.recover(&lease.session_id).unwrap();
        assert!(recovered.unresolved_calls.is_empty());
        assert!(recovered.unresolved_capabilities.is_empty());
        assert_eq!(recovered.state, SessionState::Idle);
        assert_eq!(recovered.executions[0].state, ExecutionState::Interrupted);
        assert!(recovered.messages.iter().any(|m| matches!(m, Message::ToolResult(r) if r.tool_call_id == call.id && !r.outcome_unknown)));
        assert_eq!(store.recover(&lease.session_id).unwrap(), recovered);
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(reopened.snapshot(&lease.session_id).unwrap(), recovered);
        assert!(
            reopened
                .events(&lease.session_id, 0, 1000)
                .unwrap()
                .iter()
                .any(|e| matches!(e.event, CoreEvent::ExecutionWaitRegistered { .. }))
        );
    }
}

#[test]
fn unregistered_wait_and_same_named_unrelated_tools_remain_unknown() {
    let store = SessionStore::open(database()).unwrap();
    let lease = execution(&store, &launch());
    let calls = vec![
        wait("unregistered-wait", false),
        ToolCall::new("arbitrary", "other_tool", wait_arguments(true)),
    ];
    declare(&store, &lease, &calls);
    for call in &calls {
        start(&store, &lease, call);
    }
    let recovered = store.recover(&lease.session_id).unwrap();
    assert_eq!(recovered.unresolved_calls.len(), 2);
    assert!(
        recovered
            .unresolved_calls
            .iter()
            .all(|c| c.state == CallState::OutcomeUnknown)
    );
}

#[test]
fn registered_wait_does_not_settle_an_actual_unknown_child() {
    let store = SessionStore::open(database()).unwrap();
    let lease = execution(&store, &launch());
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
                external_provenance: None,
            })
            .unwrap(),
        )
        .unwrap();
    let call = wait("wait", true);
    declare(&store, &lease, std::slice::from_ref(&call));
    start(&store, &lease, &call);
    store
        .register_execution_wait(&lease, &call.id, "vm", "task", &call.arguments)
        .unwrap();
    let recovered = store.recover(&lease.session_id).unwrap();
    assert!(recovered.unresolved_calls.is_empty());
    assert_eq!(recovered.unresolved_capabilities.len(), 1);
    assert!(matches!(
        recovered.unresolved_capabilities[0].outcome,
        Some(CapabilityOutcome::OutcomeUnknown { .. })
    ));
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(store.recover(&lease.session_id).unwrap(), recovered);
}

#[test]
fn wait_registration_requires_exact_started_call_origin_and_arguments() {
    let store = SessionStore::open(database()).unwrap();
    let lease = execution(&store, &launch());
    let mut calls = vec![wait("planned", false), wait("valid", true)];
    calls.push(ToolCall::new(
        "other-name",
        "fs.write",
        wait_arguments(true),
    ));
    calls.push(ToolCall::new(
        "other-action",
        "execute_code",
        json!({"action":"exec","script":"return 1"}),
    ));
    let mut extra = wait_arguments(false);
    extra["unexpected"] = json!(true);
    calls.push(ToolCall::new("extra-field", "execute_code", extra));
    declare(&store, &lease, &calls);
    let before = store.snapshot(&lease.session_id).unwrap();
    assert!(
        store
            .register_execution_wait(&lease, "planned", "vm", "task", &calls[0].arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    for call in &calls[1..] {
        start(&store, &lease, call);
    }
    let before = store.snapshot(&lease.session_id).unwrap();
    let valid = &calls[1];
    assert!(
        store
            .register_execution_wait(&lease, &valid.id, "vm", "wrong-task", &valid.arguments)
            .is_err()
    );
    assert!(
        store
            .register_execution_wait(&lease, &valid.id, "missing-vm", "task", &valid.arguments)
            .is_err()
    );
    assert!(
        store
            .register_execution_wait(&lease, &valid.id, "vm", "task", &wait_arguments(false))
            .is_err()
    );
    let mut stale = lease.clone();
    stale.fence += 1;
    assert!(
        store
            .register_execution_wait(&stale, &valid.id, "vm", "task", &valid.arguments)
            .is_err()
    );
    for call in &calls[2..] {
        assert!(
            store
                .register_execution_wait(&lease, &call.id, "vm", "task", &call.arguments)
                .is_err()
        );
    }
    let other_session = store.create_session("other session").unwrap();
    let other_lease = store
        .begin_attempt(&other_session.id, "other worker")
        .unwrap();
    assert!(
        store
            .register_execution_wait(&other_lease, &valid.id, "vm", "task", &valid.arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    assert!(
        !store
            .events(&lease.session_id, 0, 1000)
            .unwrap()
            .iter()
            .any(|e| matches!(e.event, CoreEvent::ExecutionWaitRegistered { .. }))
    );
}

#[test]
fn terminal_execution_can_have_a_registered_output_observation() {
    let store = SessionStore::open(database()).unwrap();
    let lease = execution(&store, &launch());
    store
        .finish_execution(
            &lease,
            "vm",
            ExecutionOutcome::Completed { result: json!(1) },
        )
        .unwrap();
    let call = wait("wait", false);
    declare(&store, &lease, std::slice::from_ref(&call));
    start(&store, &lease, &call);
    store
        .register_execution_wait(&lease, &call.id, "vm", "task", &call.arguments)
        .unwrap();
    let recovered = store.recover(&lease.session_id).unwrap();
    assert!(recovered.unresolved_calls.is_empty());
    assert_eq!(recovered.executions[0].state, ExecutionState::Completed);
}

#[test]
fn registered_wait_cannot_be_reused_as_an_execution_launch() {
    let store = SessionStore::open(database()).unwrap();
    let lease = execution(&store, &launch());
    let call = wait("wait", false);
    declare(&store, &lease, std::slice::from_ref(&call));
    start(&store, &lease, &call);
    store
        .register_execution_wait(&lease, &call.id, "vm", "task", &call.arguments)
        .unwrap();
    let before = store.snapshot(&lease.session_id).unwrap();
    assert!(
        store
            .begin_execution(
                &lease,
                ExecutionInput {
                    execution_id: "forged-launch".into(),
                    parent_call_id: call.id,
                    task_id: "task".into(),
                    source_hash: "different-source".into(),
                }
            )
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    store.events(&lease.session_id, 0, 1000).unwrap();
}

#[test]
fn wait_projection_requires_its_original_registration_event() {
    for change in ["missing-event", "changed-worker", "changed-arguments"] {
        let path = database();
        let store = SessionStore::open(&path).unwrap();
        let lease = execution(&store, &launch());
        let call = wait("wait", false);
        declare(&store, &lease, std::slice::from_ref(&call));
        start(&store, &lease, &call);
        let registered = store
            .register_execution_wait(&lease, &call.id, "vm", "task", &call.arguments)
            .unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        let raw: String = db
            .query_row(
                "SELECT record FROM core_events WHERE session_id=?1 AND cursor=?2",
                rusqlite::params![lease.session_id, registered.cursor as i64],
                |r| r.get(0),
            )
            .unwrap();
        let mut raw: Value = serde_json::from_str(&raw).unwrap();
        match change {
            "missing-event" => {
                raw["event"] = json!({"type":"host", "kind":"replacement", "data":null})
            }
            "changed-worker" => {
                raw["event"]["binding"]["lease"]["worker_id"] = json!("other-worker")
            }
            _ => raw["event"]["binding"]["arguments"]["cursor"] = json!(1),
        }
        db.execute_batch("DROP TRIGGER core_events_no_update;")
            .unwrap();
        db.execute(
            "UPDATE core_events SET record=?3 WHERE session_id=?1 AND cursor=?2",
            rusqlite::params![lease.session_id, registered.cursor as i64, raw.to_string()],
        )
        .unwrap();
        db.execute_batch("CREATE TRIGGER core_events_no_update BEFORE UPDATE ON core_events BEGIN SELECT RAISE(ABORT,'core events are append-only'); END;").unwrap();
        assert!(store.snapshot(&lease.session_id).is_err(), "{change}");
        assert!(store.list_sessions().is_err(), "{change}");
        assert!(
            store.events(&lease.session_id, 0, 1000).is_err(),
            "{change}"
        );
        assert!(store.recover(&lease.session_id).is_err(), "{change}");
    }
}

#[test]
fn failed_wait_binding_write_leaves_no_privileged_projection() {
    let path = database();
    let store = SessionStore::open(&path).unwrap();
    let lease = execution(&store, &launch());
    let call = wait("wait", false);
    declare(&store, &lease, std::slice::from_ref(&call));
    start(&store, &lease, &call);
    let before = store.snapshot(&lease.session_id).unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_wait BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='execution_wait_registered' BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    assert!(
        store
            .register_execution_wait(&lease, &call.id, "vm", "task", &call.arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    db.execute_batch("DROP TRIGGER reject_wait").unwrap();
    let recovered = store.recover(&lease.session_id).unwrap();
    assert_eq!(recovered.unresolved_calls.len(), 1);
    assert_eq!(recovered.unresolved_calls[0].call.id, call.id);
}
