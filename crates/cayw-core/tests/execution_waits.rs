use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{AssistantMessage, Content, Message, StopReason, ToolCall, Usage};
use cayw_core::{
    ApprovalDecision, AttemptLease, CapabilityAuthorization, CapabilityCallKey, CapabilityInput,
    CapabilityOutcome, CoreEvent, ExecutionInput, ExecutionOutcome, ExecutionState, SessionState,
    SessionStore,
};
use serde_json::json;
use std::path::PathBuf;

fn database() -> PathBuf {
    std::env::temp_dir().join(format!("cayw-core-waits-{}.db", uuid::Uuid::new_v4()))
}
fn assistant(calls: &[ToolCall]) -> AssistantMessage {
    AssistantMessage {
        content: calls.iter().cloned().map(Content::ToolCall).collect(),
        provider: "offline".into(),
        model: "fixture".into(),
        response_id: None,
        response_model: None,
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 1,
        duration_ms: None,
    }
}
fn wait(id: &str, continuation: Option<bool>) -> ToolCall {
    let mut arguments = json!({"action":"wait","cell_id":"vm","cursor":0,"yield_time_ms":0,"max_output_tokens":1024});
    if let Some(continuation) = continuation {
        arguments["continue_execution"] = json!(continuation);
    }
    ToolCall::new(id, "execute_code", arguments)
}
fn fixture(path: &PathBuf, calls: &[ToolCall]) -> (SessionStore, AttemptLease) {
    let store = SessionStore::open(path).unwrap();
    let session = store.create_session("wait recovery").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let message = Message::Assistant(assistant(calls));
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
    let launch = &calls[0];
    start(&store, &lease, launch);
    store
        .begin_execution(
            &lease,
            ExecutionInput {
                execution_id: "vm".into(),
                parent_call_id: launch.id.clone(),
                task_id: "task".into(),
                source_hash: "verified-source".into(),
            },
        )
        .unwrap();
    (store, lease)
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
fn launch() -> ToolCall {
    ToolCall::new(
        "launch",
        "execute_code",
        json!({"action":"exec","script":"yield_control()","yield_time_ms":0,"max_output_tokens":1024}),
    )
}

#[test]
fn registered_wait_reply_loss_recovers_from_independent_vm_facts() {
    for continuation in [Some(false), Some(true), None] {
        let path = database();
        let waiting = wait("wait", continuation);
        let (store, lease) = fixture(&path, &[launch(), waiting.clone()]);
        store.yield_execution(&lease, "vm").unwrap();
        start(&store, &lease, &waiting);
        let registered = store
            .register_execution_wait(&lease, &waiting.id, "vm", "task", &waiting.arguments)
            .unwrap();
        assert!(matches!(
            registered.event,
            CoreEvent::ExecutionWaitRegistered { .. }
        ));
        let snapshot = SessionStore::open(&path)
            .unwrap()
            .recover(&lease.session_id)
            .unwrap();
        assert_eq!(snapshot.state, SessionState::Idle);
        assert!(snapshot.unresolved_calls.is_empty());
        assert_eq!(snapshot.executions[0].state, ExecutionState::Interrupted);
        assert_eq!(snapshot.messages.len(), 3);
        let repeated = store.recover(&lease.session_id).unwrap();
        assert_eq!(repeated.messages, snapshot.messages);
        assert_eq!(repeated.executions, snapshot.executions);
        assert!(repeated.unresolved_calls.is_empty());
        let events = store.events(&lease.session_id, 0, 1000).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.event, CoreEvent::ExecutionStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.event, CoreEvent::ExecutionWaitRegistered { .. }))
                .count(),
            1
        );
    }
}

#[test]
fn registered_wait_does_not_hide_unknown_child_or_replay_it() {
    let path = database();
    let waiting = wait("wait", Some(true));
    let (store, lease) = fixture(&path, &[launch(), waiting.clone()]);
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
    start(&store, &lease, &waiting);
    store
        .register_execution_wait(&lease, &waiting.id, "vm", "task", &waiting.arguments)
        .unwrap();
    let snapshot = SessionStore::open(path)
        .unwrap()
        .recover(&lease.session_id)
        .unwrap();
    assert!(snapshot.unresolved_calls.is_empty());
    assert_eq!(snapshot.state, SessionState::NeedsReconciliation);
    assert_eq!(snapshot.unresolved_capabilities.len(), 1);
    assert!(matches!(
        snapshot.unresolved_capabilities[0].outcome,
        Some(CapabilityOutcome::OutcomeUnknown { .. })
    ));
    let repeated = store.recover(&lease.session_id).unwrap();
    assert_eq!(
        repeated.unresolved_capabilities,
        snapshot.unresolved_capabilities
    );
    assert_eq!(repeated.messages, snapshot.messages);
}

#[test]
fn unregistered_wait_and_arbitrary_tool_still_require_reconciliation() {
    let waiting = wait("wait", Some(false));
    let other = ToolCall::new("other", "fs.write", json!({}));
    let (store, lease) = fixture(&database(), &[launch(), waiting.clone(), other.clone()]);
    start(&store, &lease, &waiting);
    start(&store, &lease, &other);
    let snapshot = store.recover(&lease.session_id).unwrap();
    assert_eq!(snapshot.state, SessionState::NeedsReconciliation);
    assert_eq!(
        snapshot
            .unresolved_calls
            .iter()
            .map(|c| c.call.id.as_str())
            .collect::<Vec<_>>(),
        ["wait", "other"]
    );
}

#[test]
fn wait_binding_rejects_wrong_origin_arguments_action_and_unstarted_calls() {
    let waiting = wait("wait", Some(false));
    let wrong_action = ToolCall::new(
        "exec-again",
        "execute_code",
        json!({"action":"exec","script":"return 1"}),
    );
    let wrong_name = ToolCall::new("fake", "read", waiting.arguments.clone());
    let (store, lease) = fixture(
        &database(),
        &[
            launch(),
            waiting.clone(),
            wrong_action.clone(),
            wrong_name.clone(),
        ],
    );
    let before = store.snapshot(&lease.session_id).unwrap();
    assert!(
        store
            .register_execution_wait(&lease, &waiting.id, "vm", "task", &waiting.arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionStart {
                call: waiting.clone(),
            },
        )
        .unwrap();
    let before = store.snapshot(&lease.session_id).unwrap();
    assert!(
        store
            .register_execution_wait(&lease, &waiting.id, "vm", "task", &waiting.arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    store.mark_tool_started(&lease, &waiting.id).unwrap();
    let before = store.snapshot(&lease.session_id).unwrap();
    let mut changed = waiting.arguments.clone();
    changed["cursor"] = json!(1);
    for (execution, task, arguments) in [
        ("vm", "wrong-task", &waiting.arguments),
        ("missing", "task", &waiting.arguments),
        ("vm", "task", &changed),
    ] {
        assert!(
            store
                .register_execution_wait(&lease, &waiting.id, execution, task, arguments)
                .is_err()
        );
        assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    }
    let mut stale = lease.clone();
    stale.fence += 1;
    assert!(
        store
            .register_execution_wait(&stale, &waiting.id, "vm", "task", &waiting.arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    for call in [wrong_action, wrong_name] {
        start(&store, &lease, &call);
        let before = store.snapshot(&lease.session_id).unwrap();
        assert!(
            store
                .register_execution_wait(&lease, &call.id, "vm", "task", &call.arguments)
                .is_err()
        );
        assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    }
    let other_session = store.create_session("other").unwrap();
    let other_lease = store.begin_attempt(&other_session.id, "peer").unwrap();
    let before = store.snapshot(&other_session.id).unwrap();
    assert!(
        store
            .register_execution_wait(&other_lease, &waiting.id, "vm", "task", &waiting.arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&other_session.id).unwrap(), before);
}

#[test]
fn wait_binding_insertion_rolls_back_on_storage_failure() {
    let path = database();
    let waiting = wait("wait", Some(false));
    let (store, lease) = fixture(&path, &[launch(), waiting.clone()]);
    start(&store, &lease, &waiting);
    let before = store.snapshot(&lease.session_id).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_wait BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='execution_wait_registered' BEGIN SELECT RAISE(ABORT, 'injected wait failure'); END;").unwrap();
    assert!(
        store
            .register_execution_wait(&lease, &waiting.id, "vm", "task", &waiting.arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    db.execute_batch("DROP TRIGGER reject_wait;").unwrap();
    let recovered = SessionStore::open(path)
        .unwrap()
        .recover(&lease.session_id)
        .unwrap();
    assert_eq!(recovered.unresolved_calls.len(), 1);
    assert_eq!(recovered.unresolved_calls[0].call.id, waiting.id);
}

#[test]
fn known_wait_terminal_remains_known_and_cannot_be_registered_later() {
    let waiting = wait("wait", Some(true));
    let (store, lease) = fixture(&database(), &[launch(), waiting.clone()]);
    store
        .finish_execution(
            &lease,
            "vm",
            ExecutionOutcome::Completed {
                result: json!("done"),
            },
        )
        .unwrap();
    start(&store, &lease, &waiting);
    store
        .register_execution_wait(&lease, &waiting.id, "vm", "task", &waiting.arguments)
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionEnd {
                call: waiting.clone(),
                result: ToolResult::text("done"),
                duration_ms: Some(1),
            },
        )
        .unwrap();
    let before = store.snapshot(&lease.session_id).unwrap();
    assert!(
        store
            .register_execution_wait(&lease, &waiting.id, "vm", "task", &waiting.arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&lease.session_id).unwrap(), before);
    let recovered = store.recover(&lease.session_id).unwrap();
    assert!(recovered.unresolved_calls.is_empty());
    assert_eq!(
        recovered.executions[0].terminal,
        Some(ExecutionOutcome::Completed {
            result: json!("done")
        })
    );
}

#[test]
fn previous_attempt_execution_cannot_authorize_a_new_wait() {
    let path = database();
    let (store, lease) = fixture(&path, &[launch()]);
    store.recover(&lease.session_id).unwrap();
    let next = store
        .begin_attempt(&lease.session_id, "next-worker")
        .unwrap();
    let waiting = wait("new-wait", Some(true));
    store
        .append_agent_event(&next, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&next, &AgentEvent::TurnStart)
        .unwrap();
    let message = Message::Assistant(assistant(std::slice::from_ref(&waiting)));
    store
        .append_agent_event(
            &next,
            &AgentEvent::MessageStart {
                message: message.clone(),
            },
        )
        .unwrap();
    store
        .append_agent_event(&next, &AgentEvent::MessageEnd { message })
        .unwrap();
    start(&store, &next, &waiting);
    let before = store.snapshot(&next.session_id).unwrap();
    assert!(
        store
            .register_execution_wait(&next, &waiting.id, "vm", "task", &waiting.arguments)
            .is_err()
    );
    assert_eq!(store.snapshot(&next.session_id).unwrap(), before);
    assert!(
        store
            .register_execution_wait(&lease, &waiting.id, "vm", "task", &waiting.arguments)
            .is_err()
    );
}
