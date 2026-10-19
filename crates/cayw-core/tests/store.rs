use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall, ToolResultMessage};
use cayw_core::{
    ApprovalDecision, ApprovalInput, ApprovalTarget, CallState, CapabilityCallKey, CapabilityInput,
    CoreError, CoreEvent, ExecutionInput, MAX_EVENT_PAGE, ReconciledOutcome, Reconciliation,
    SessionState, SessionStore,
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, Barrier},
};
use uuid::Uuid;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("cayw-core-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn path(&self) -> PathBuf {
        self.0.join("session.sqlite")
    }
    fn open(&self) -> SessionStore {
        SessionStore::open(self.path()).unwrap()
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn assistant(calls: Vec<ToolCall>) -> AssistantMessage {
    let mut message = AssistantMessage::new(&Model::new("fixture", "offline"));
    message.stop_reason = if calls.is_empty() {
        StopReason::Stop
    } else {
        StopReason::ToolUse
    };
    message.content = calls.into_iter().map(Content::ToolCall).collect();
    message
}
fn call(id: &str) -> ToolCall {
    ToolCall::new(id, "write_file", json!({"path":"a.txt"}))
}
fn start(store: &SessionStore, lease: &cayw_core::AttemptLease) {
    store
        .append_agent_event(lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(lease, &AgentEvent::TurnStart)
        .unwrap();
}
fn message(store: &SessionStore, lease: &cayw_core::AttemptLease, message: Message) {
    store
        .append_agent_event(
            lease,
            &AgentEvent::MessageStart {
                message: message.clone(),
            },
        )
        .unwrap();
    store
        .append_agent_event(lease, &AgentEvent::MessageEnd { message })
        .unwrap();
}
fn end_call(store: &SessionStore, lease: &cayw_core::AttemptLease, call: &ToolCall, text: &str) {
    store
        .append_agent_event(
            lease,
            &AgentEvent::ToolExecutionEnd {
                call: call.clone(),
                result: ToolResult::text(text),
                duration_ms: Some(4),
            },
        )
        .unwrap();
}
fn tool_results(store: &SessionStore, id: &str) -> Vec<ToolResultMessage> {
    store
        .snapshot(id)
        .unwrap()
        .messages
        .into_iter()
        .filter_map(|m| match m {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

// Losing a transaction or treating a model declaration as an execution breaks this test.
#[test]
fn reopen_preserves_identity_messages_planned_calls_and_event_cursor() {
    fn assert_traits<T: Clone + Send + Sync>() {}
    assert_traits::<SessionStore>();
    let database = Database::new();
    let store = database.open();
    let session = store.create_session("durable").unwrap();
    let lease = store.begin_attempt(&session.id, "worker-a").unwrap();
    start(&store, &lease);
    message(&store, &lease, Message::user("save"));
    message(
        &store,
        &lease,
        Message::Assistant(assistant(vec![call("c1")])),
    );
    let before = store.snapshot(&session.id).unwrap();
    assert_eq!(before.messages.len(), 2);
    assert_eq!(before.unresolved_calls[0].state, CallState::Planned);
    assert!(!before.unresolved_calls[0].execution_started);
    drop(store);
    let reopened = database.open();
    assert_eq!(reopened.snapshot(&session.id).unwrap(), before);
    assert_eq!(reopened.list_sessions().unwrap().len(), 1);
    let records = reopened.events(&session.id, 0, MAX_EVENT_PAGE).unwrap();
    assert_eq!(records.len() as u64, before.cursor);
    assert_eq!(
        records.iter().map(|r| r.cursor).collect::<Vec<_>>(),
        (1..=before.cursor).collect::<Vec<_>>()
    );
    assert_eq!(
        reopened
            .events(&session.id, before.cursor, 1)
            .unwrap()
            .len(),
        0
    );
    assert!(matches!(
        reopened.events(&session.id, before.cursor + 1, 1),
        Err(CoreError::FutureCursor { .. })
    ));
    assert!(matches!(
        reopened.events(&session.id, 0, MAX_EVENT_PAGE + 1),
        Err(CoreError::InvalidLimit { .. })
    ));
    assert!(matches!(
        reopened.events(&session.id, 0, 0),
        Err(CoreError::InvalidLimit { .. })
    ));
}

// A lease is bound to the session, worker, attempt and monotonically increasing fence.
#[test]
fn recover_fences_old_leases_across_independent_connections() {
    let database = Database::new();
    let first = database.open();
    let second = database.open();
    let session = first.create_session("fence").unwrap();
    let lease = first.begin_attempt(&session.id, "worker-a").unwrap();
    assert!(matches!(
        second.begin_attempt(&session.id, "worker-b"),
        Err(CoreError::Busy)
    ));
    second.recover(&session.id).unwrap();
    let next = second.begin_attempt(&session.id, "worker-b").unwrap();
    assert!(next.fence > lease.fence);
    assert_ne!(next.attempt_id, lease.attempt_id);
    assert!(matches!(
        first.append_agent_event(&lease, &AgentEvent::AgentStart),
        Err(CoreError::StaleLease)
    ));
    assert!(matches!(
        first.finish_attempt(&lease),
        Err(CoreError::StaleLease)
    ));
    let mut forged = next.clone();
    forged.worker_id = "worker-a".into();
    assert!(matches!(
        first.append_agent_event(&forged, &AgentEvent::AgentStart),
        Err(CoreError::StaleLease)
    ));
    second.finish_attempt(&next).unwrap();
    assert!(matches!(
        second.finish_attempt(&next),
        Err(CoreError::StaleLease)
    ));
}

#[test]
fn competing_connections_cannot_both_acquire_an_attempt() {
    let database = Database::new();
    let first = database.open();
    let second = database.open();
    let id = first.create_session("race").unwrap().id;
    let barrier = Arc::new(Barrier::new(2));
    let mut threads = vec![];
    for (store, worker) in [(first, "a"), (second, "b")] {
        let barrier = barrier.clone();
        let id = id.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            store.begin_attempt(&id, worker)
        }));
    }
    let outcomes = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| matches!(r, Err(CoreError::Busy)))
            .count(),
        1
    );
}

// Wrong calls and duplicate terminal events must roll back both log and projection.
#[test]
fn tool_results_pair_atomically_in_source_order_and_duplicates_are_rejected() {
    let database = Database::new();
    let store = database.open();
    let session = store.create_session("pairing").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();
    start(&store, &lease);
    let a = call("a");
    let b = call("b");
    let output = assistant(vec![a.clone(), b.clone()]);
    message(&store, &lease, Message::Assistant(output.clone()));
    for c in [&a, &b] {
        store
            .append_agent_event(&lease, &AgentEvent::ToolExecutionStart { call: c.clone() })
            .unwrap();
        store.mark_tool_started(&lease, &c.id).unwrap();
    }
    let before = store.snapshot(&session.id).unwrap();
    let mut wrong = a.clone();
    wrong.arguments = json!({"path":"another.txt"});
    assert!(
        store
            .append_agent_event(
                &lease,
                &AgentEvent::ToolExecutionEnd {
                    call: wrong,
                    result: ToolResult::text("wrong"),
                    duration_ms: None
                }
            )
            .is_err()
    );
    assert_eq!(store.snapshot(&session.id).unwrap(), before);
    end_call(&store, &lease, &b, "second");
    let first_end = store.snapshot(&session.id).unwrap();
    assert!(
        matches!(first_end.messages.last(), Some(Message::ToolResult(r)) if r.tool_call_id == "b")
    );
    end_call(&store, &lease, &a, "first");
    let results = tool_results(&store, &session.id);
    assert_eq!(
        results
            .iter()
            .map(|r| r.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    let before = store.snapshot(&session.id).unwrap();
    assert!(
        store
            .append_agent_event(
                &lease,
                &AgentEvent::ToolExecutionEnd {
                    call: a,
                    result: ToolResult::text("duplicate"),
                    duration_ms: Some(4)
                }
            )
            .is_err()
    );
    assert_eq!(store.snapshot(&session.id).unwrap(), before);
    store
        .append_agent_event(
            &lease,
            &AgentEvent::TurnEnd {
                message: output.clone(),
                tool_results: results.clone(),
            },
        )
        .unwrap();
    assert!(
        store
            .append_agent_event(
                &lease,
                &AgentEvent::TurnEnd {
                    message: output,
                    tool_results: results
                }
            )
            .is_err()
    );
    store
        .append_agent_event(&lease, &AgentEvent::AgentEnd { messages: vec![] })
        .unwrap();
    assert!(
        store
            .append_agent_event(&lease, &AgentEvent::AgentEnd { messages: vec![] })
            .is_err()
    );
    store.finish_attempt(&lease).unwrap();
    assert_eq!(
        store.snapshot(&session.id).unwrap().state,
        SessionState::Idle
    );
    assert!(
        store
            .snapshot(&session.id)
            .unwrap()
            .unresolved_calls
            .is_empty()
    );
}

#[test]
fn preflight_is_not_the_execution_boundary_and_cannot_claim_success() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("boundary").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    start(&store, &lease);
    let c = call("c");
    message(
        &store,
        &lease,
        Message::Assistant(assistant(vec![c.clone()])),
    );
    store
        .append_agent_event(&lease, &AgentEvent::ToolExecutionStart { call: c.clone() })
        .unwrap();
    let snapshot = store.snapshot(&id).unwrap();
    assert_eq!(snapshot.unresolved_calls[0].state, CallState::Prepared);
    assert!(!snapshot.unresolved_calls[0].execution_started);
    assert!(
        store
            .append_agent_event(
                &lease,
                &AgentEvent::ToolExecutionEnd {
                    call: c.clone(),
                    result: ToolResult::text("success"),
                    duration_ms: None
                }
            )
            .is_err()
    );
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionEnd {
                call: c,
                result: ToolResult::error("blocked before execution"),
                duration_ms: None,
            },
        )
        .unwrap();
    assert!(!tool_results(&store, &id)[0].outcome_unknown);
}

#[test]
fn pending_approval_survives_reopen_and_never_authorizes_execution() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("approval").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    start(&store, &lease);
    let c = call("approval-call");
    message(
        &store,
        &lease,
        Message::Assistant(assistant(vec![c.clone()])),
    );
    store
        .append_agent_event(&lease, &AgentEvent::ToolExecutionStart { call: c.clone() })
        .unwrap();
    let approval = store
        .request_approval(
            &lease,
            ApprovalInput {
                task_id: "task-1".into(),
                target: ApprovalTarget::ToolCall {
                    call_id: c.id.clone(),
                },
                capability: "files.write".into(),
                scope: "a.txt".into(),
                reason: "host permission required".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        store.mark_tool_started(&lease, &c.id),
        Err(CoreError::PendingApproval)
    ));
    drop(store);
    let store = database.open();
    assert_eq!(store.snapshot(&id).unwrap().pending_approvals.len(), 1);
    assert!(matches!(
        store.mark_tool_started(&lease, &c.id),
        Err(CoreError::PendingApproval)
    ));
    store
        .decide_approval(&id, &approval.id, ApprovalDecision::Deny, "host")
        .unwrap();
    assert!(matches!(
        store.mark_tool_started(&lease, &c.id),
        Err(CoreError::ApprovalDenied)
    ));
    assert!(
        store
            .decide_approval(&id, &approval.id, ApprovalDecision::Approve, "host")
            .is_err()
    );
    assert!(store.snapshot(&id).unwrap().pending_approvals.is_empty());
}

#[test]
fn approval_approval_is_explicit_and_recorded_in_the_log() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("approved").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    start(&store, &lease);
    let c = call("approved-call");
    message(
        &store,
        &lease,
        Message::Assistant(assistant(vec![c.clone()])),
    );
    store
        .append_agent_event(&lease, &AgentEvent::ToolExecutionStart { call: c.clone() })
        .unwrap();
    let approval = store
        .request_approval(
            &lease,
            ApprovalInput {
                task_id: "task".into(),
                target: ApprovalTarget::ToolCall {
                    call_id: c.id.clone(),
                },
                capability: "files.write".into(),
                scope: "a.txt".into(),
                reason: "ask".into(),
            },
        )
        .unwrap();
    store
        .decide_approval(&id, &approval.id, ApprovalDecision::Approve, "host")
        .unwrap();
    store.mark_tool_started(&lease, &c.id).unwrap();
    assert!(store.events(&id, 0, 100).unwrap().iter().any(|record| matches!(&record.event, CoreEvent::ApprovalDecided { approval: decided } if decided.id == approval.id && decided.decision.as_ref().unwrap().decision == ApprovalDecision::Approve)));
}

#[test]
fn executing_code_keeps_each_capability_approval_linked_to_its_parent() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("nested approvals").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    start(&store, &lease);
    let parent = ToolCall::new("code-parent", "execute_code", json!({"script":"return 1"}));
    message(
        &store,
        &lease,
        Message::Assistant(assistant(vec![parent.clone()])),
    );
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionStart {
                call: parent.clone(),
            },
        )
        .unwrap();
    store.mark_tool_started(&lease, &parent.id).unwrap();
    store
        .begin_execution(
            &lease,
            ExecutionInput {
                execution_id: "cell".into(),
                parent_call_id: parent.id.clone(),
                task_id: "task".into(),
                source_hash: "hash".into(),
            },
        )
        .unwrap();
    end_call(&store, &lease, &parent, "cell is still running");
    let key = CapabilityCallKey {
        execution_id: "cell".into(),
        sequence: 0,
    };
    store
        .plan_capability(
            &lease,
            CapabilityInput {
                key: key.clone(),
                name: "fs.write".into(),
                arguments: json!({"path":"first.txt"}),
            },
        )
        .unwrap();
    let input = ApprovalInput {
        task_id: "task".into(),
        target: ApprovalTarget::CapabilityCall { key },
        capability: "fs.write".into(),
        scope: "first.txt".into(),
        reason: "child operation".into(),
    };
    let first = store.request_approval(&lease, input.clone()).unwrap();
    assert_eq!(first.input.target, input.target);
    assert_eq!(
        store.snapshot(&id).unwrap().state,
        SessionState::AwaitingApproval
    );
    drop(store);
    let store = database.open();
    assert_eq!(store.approval(&id, &first.id).unwrap(), first);
    store
        .decide_approval(&id, &first.id, ApprovalDecision::Deny, "host")
        .unwrap();
    let next_key = CapabilityCallKey {
        execution_id: "cell".into(),
        sequence: 1,
    };
    store
        .plan_capability(
            &lease,
            CapabilityInput {
                key: next_key.clone(),
                name: "fs.write".into(),
                arguments: json!({"path":"first.txt"}),
            },
        )
        .unwrap();
    assert!(matches!(
        store.request_approval(
            &lease,
            ApprovalInput {
                target: ApprovalTarget::CapabilityCall {
                    key: next_key.clone()
                },
                ..input.clone()
            }
        ),
        Err(CoreError::ApprovalDenied)
    ));
    let other_scope = store
        .request_approval(
            &lease,
            ApprovalInput {
                target: ApprovalTarget::CapabilityCall { key: next_key },
                scope: "second.txt".into(),
                ..input
            },
        )
        .unwrap();
    assert_ne!(first.id, other_scope.id);
    assert!(store.mark_tool_started(&lease, &parent.id).is_err());
    assert_eq!(tool_results(&store, &id)[0].tool_call_id, parent.id);
}

#[test]
fn ordinary_tools_cannot_request_retroactive_approval_after_start() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("approval boundary").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    start(&store, &lease);
    let tool = call("ordinary");
    message(
        &store,
        &lease,
        Message::Assistant(assistant(vec![tool.clone()])),
    );
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionStart { call: tool.clone() },
        )
        .unwrap();
    store.mark_tool_started(&lease, &tool.id).unwrap();
    let before = store.snapshot(&id).unwrap();
    assert!(
        store
            .request_approval(
                &lease,
                ApprovalInput {
                    task_id: "task".into(),
                    target: ApprovalTarget::ToolCall { call_id: tool.id },
                    capability: "fs.write".into(),
                    scope: "a.txt".into(),
                    reason: "too late".into(),
                }
            )
            .is_err()
    );
    assert_eq!(store.snapshot(&id).unwrap(), before);
}

// Durable terminals remain authoritative even if the worker never emits TurnEnd.
#[test]
fn interrupted_batch_preserves_durable_results_and_reconciles_only_missing_terminals() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("recover").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    start(&store, &lease);
    let a = call("completed");
    let b = call("started");
    let c = call("planned");
    message(
        &store,
        &lease,
        Message::Assistant(assistant(vec![a.clone(), b.clone(), c.clone()])),
    );
    for call in [&a, &b] {
        store
            .append_agent_event(
                &lease,
                &AgentEvent::ToolExecutionStart { call: call.clone() },
            )
            .unwrap();
        store.mark_tool_started(&lease, &call.id).unwrap();
    }
    end_call(&store, &lease, &a, "reported before batch commit");
    let durable_result = tool_results(&store, &id)[0].clone();
    let old_log = store.events(&id, 0, 100).unwrap();
    drop(store);
    let store = database.open();
    let recovered = store.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert!(recovered.attempt.is_none());
    assert_eq!(recovered.unresolved_calls.len(), 1);
    assert_eq!(recovered.unresolved_calls[0].call.id, b.id);
    assert_eq!(
        recovered.unresolved_calls[0].state,
        CallState::OutcomeUnknown
    );
    assert!(recovered.unresolved_calls[0].execution_started);
    assert_eq!(tool_results(&store, &id)[0], durable_result);
    assert!(tool_results(&store, &id)[2].is_error);
    assert!(!tool_results(&store, &id)[2].outcome_unknown);
    assert!(matches!(
        store.begin_attempt(&id, "replacement"),
        Err(CoreError::NeedsReconciliation)
    ));
    store.recover(&id).unwrap();
    assert_eq!(
        store.snapshot(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    store
        .reconcile(
            &id,
            Reconciliation {
                call_id: b.id,
                actor: "host".into(),
                evidence: "inspected resulting file".into(),
                outcome: ReconciledOutcome::Completed {
                    result: ToolResult::text("verified saved"),
                },
            },
        )
        .unwrap();
    let current = store.snapshot(&id).unwrap();
    assert_eq!(current.state, SessionState::Idle);
    assert!(current.unresolved_calls.is_empty());
    let results = tool_results(&store, &id);
    assert_eq!(
        results
            .iter()
            .map(|r| r.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        ["completed", "started", "planned"]
    );
    assert!(results.iter().all(|result| !result.outcome_unknown));
    assert_eq!(store.events(&id, 0, old_log.len()).unwrap(), old_log);
    assert_eq!(
        store
            .events(&id, old_log.last().unwrap().cursor, 10)
            .unwrap()
            .iter()
            .filter(|r| matches!(r.event, CoreEvent::Reconciled { .. }))
            .count(),
        1
    );
    store.begin_attempt(&id, "replacement").unwrap();
}

#[test]
fn unknown_outcome_cannot_be_cleared_by_finishing_or_reopening() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("unknown").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    start(&store, &lease);
    let c = call("unknown-call");
    let output = assistant(vec![c.clone()]);
    message(&store, &lease, Message::Assistant(output.clone()));
    store
        .append_agent_event(&lease, &AgentEvent::ToolExecutionStart { call: c.clone() })
        .unwrap();
    store.mark_tool_started(&lease, &c.id).unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionEnd {
                call: c,
                result: ToolResult {
                    outcome_unknown: true,
                    ..ToolResult::error("connection lost")
                },
                duration_ms: Some(1),
            },
        )
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::TurnEnd {
                message: output,
                tool_results: tool_results(&store, &id),
            },
        )
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentEnd { messages: vec![] })
        .unwrap();
    store.finish_attempt(&lease).unwrap();
    drop(store);
    let store = database.open();
    assert_eq!(
        store.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(matches!(
        store.begin_attempt(&id, "next"),
        Err(CoreError::NeedsReconciliation)
    ));
    assert!(
        store
            .reconcile(
                &id,
                Reconciliation {
                    call_id: "unknown-call".into(),
                    actor: "host".into(),
                    evidence: "".into(),
                    outcome: ReconciledOutcome::NotExecuted {
                        reason: "guess".into()
                    }
                }
            )
            .is_err()
    );
}

#[test]
fn rejects_newer_schema_and_unrelated_databases() {
    let database = Database::new();
    {
        let db = rusqlite::Connection::open(database.path()).unwrap();
        db.pragma_update(None, "user_version", 99).unwrap();
    }
    assert!(matches!(
        SessionStore::open(database.path()),
        Err(CoreError::SchemaVersion { found: 99, .. })
    ));
    let other = Database::new();
    {
        let db = rusqlite::Connection::open(other.path()).unwrap();
        db.execute_batch("CREATE TABLE somebody_else (value TEXT)")
            .unwrap();
    }
    assert!(matches!(
        SessionStore::open(other.path()),
        Err(CoreError::ForeignDatabase)
    ));
}

#[test]
fn duplicate_messages_and_unpaired_results_leave_no_partial_log_entries() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("invalid").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    start(&store, &lease);
    let output = Message::Assistant(assistant(vec![call("same"), call("same")]));
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: output.clone(),
            },
        )
        .unwrap();
    let before = store.snapshot(&id).unwrap();
    assert!(
        store
            .append_agent_event(&lease, &AgentEvent::MessageEnd { message: output })
            .is_err()
    );
    assert_eq!(store.snapshot(&id).unwrap(), before);
    store.recover(&id).unwrap();
    let lease = store.begin_attempt(&id, "worker").unwrap();
    start(&store, &lease);
    let output = Message::Assistant(assistant(vec![]));
    message(&store, &lease, output.clone());
    let before = store.snapshot(&id).unwrap();
    assert!(
        store
            .append_agent_event(&lease, &AgentEvent::MessageEnd { message: output })
            .is_err()
    );
    assert_eq!(store.snapshot(&id).unwrap(), before);
}
