use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
use cayw_core::{
    ApprovalDecision, ApprovalInput, ApprovalTarget, CoreError, ReconciledOutcome, Reconciliation,
    SessionState, SessionStore,
};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};
use uuid::Uuid;

struct ExactReceipt(cayw_core::ReconciliationRequest);
impl cayw_core::ReconciliationVerifier for ExactReceipt {
    fn verify(&self, request: &cayw_core::ReconciliationRequest) -> Result<(), String> {
        if request == &self.0 {
            Ok(())
        } else {
            Err("this decision is not authorized by the test host".into())
        }
    }
}

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("cayw-core-edges-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn model_message(calls: Vec<ToolCall>) -> AssistantMessage {
    let mut message = AssistantMessage::new(&Model::new("fixture", "offline"));
    message.stop_reason = StopReason::ToolUse;
    message.content = calls.into_iter().map(Content::ToolCall).collect();
    message
}
fn fixture() -> (
    Database,
    SessionStore,
    String,
    cayw_core::AttemptLease,
    ToolCall,
) {
    let database = Database::new();
    let store = SessionStore::open(database.0.join("sessions.sqlite")).unwrap();
    let id = store.create_session("edge").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let call = ToolCall::new("edge-call", "write", json!({}));
    let message = Message::Assistant(model_message(vec![call.clone()]));
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
    (database, store, id, lease, call)
}

#[test]
fn incomplete_attempt_cannot_be_finished_and_recovery_never_grants_a_pending_request() {
    let (_database, store, id, lease, call) = fixture();
    let approval = store
        .request_approval(
            &lease,
            ApprovalInput {
                task_id: "task".into(),
                target: ApprovalTarget::ToolCall { call_id: call.id },
                capability: "write".into(),
                scope: "file".into(),
                reason: "approval".into(),
            },
        )
        .unwrap();
    assert!(store.finish_attempt(&lease).is_err());
    let before = store.snapshot(&id).unwrap();
    let snapshot = store.recover(&id).unwrap();
    assert_eq!(snapshot.state, SessionState::Idle);
    assert!(snapshot.pending_approvals.is_empty());
    assert!(snapshot.unresolved_calls.is_empty());
    let resolved = store.approval(&id, &approval.id).unwrap();
    assert_eq!(resolved.input, approval.input);
    let decision = resolved.decision.as_ref().unwrap();
    assert_eq!(decision.decision, ApprovalDecision::Deny);
    assert_eq!(decision.actor, "host-recovery");
    let events = store.events(&id, before.cursor, 10).unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[0].event,
        cayw_core::CoreEvent::ApprovalDecided { approval } if approval == &resolved
    ));
    assert!(matches!(
        &events[1].event,
        cayw_core::CoreEvent::AttemptRecovered { .. }
    ));
    assert!(
        events
            .iter()
            .all(|event| event.attempt.as_ref() == Some(&lease))
    );
    assert!(matches!(
        store.validate_lease(&lease),
        Err(CoreError::StaleLease)
    ));
    assert!(
        store
            .decide_approval(&id, &approval.id, ApprovalDecision::Approve, "late-host")
            .is_err()
    );
    assert!(store.begin_attempt(&id, "worker").is_ok());
}

#[test]
fn recovery_rolls_back_all_approval_decisions_when_its_terminal_cannot_be_recorded() {
    let (_database, store, id, lease, call) = fixture();
    let input = ApprovalInput {
        task_id: "task".into(),
        target: ApprovalTarget::ToolCall { call_id: call.id },
        capability: "write".into(),
        scope: "first".into(),
        reason: "approval".into(),
    };
    let first = store.request_approval(&lease, input.clone()).unwrap();
    let second = store
        .request_approval(
            &lease,
            ApprovalInput {
                scope: "second".into(),
                ..input
            },
        )
        .unwrap();
    let before = store.snapshot(&id).unwrap();
    let db = rusqlite::Connection::open(_database.0.join("sessions.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_recovery BEFORE INSERT ON core_events \
         WHEN json_extract(NEW.record, '$.event.type') = 'attempt_recovered' \
         BEGIN SELECT RAISE(ABORT, 'injected recovery failure'); END;",
    )
    .unwrap();
    assert!(matches!(store.recover(&id), Err(CoreError::Database(_))));
    assert_eq!(store.snapshot(&id).unwrap(), before);
    assert!(store.events(&id, before.cursor, 10).unwrap().is_empty());
    assert_eq!(store.approval(&id, &first.id).unwrap(), first);
    assert_eq!(store.approval(&id, &second.id).unwrap(), second);
    store.validate_lease(&lease).unwrap();
    db.execute_batch("DROP TRIGGER reject_recovery").unwrap();
    let recovered = store.recover(&id).unwrap();
    assert!(recovered.pending_approvals.is_empty());
    assert_eq!(recovered.cursor, before.cursor + 3);
    assert_eq!(recovered.state, SessionState::Idle);
    let denied = store.approvals(&id).unwrap();
    assert!(denied.iter().all(|approval| {
        approval.decision.as_ref().is_some_and(|decision| {
            decision.decision == ApprovalDecision::Deny && decision.actor == "host-recovery"
        })
    }));
    let count = denied.len();
    store.recover(&id).unwrap();
    assert_eq!(store.approvals(&id).unwrap().len(), count);
    assert_eq!(store.approvals(&id).unwrap(), denied);
}

#[test]
fn explicit_unknown_blocks_new_calls_but_allows_existing_calls_to_settle() {
    let (_database, store, id, lease, call) = fixture();
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
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionEnd {
                call: call.clone(),
                result: ToolResult {
                    outcome_unknown: true,
                    ..ToolResult::error("uncertain")
                },
                duration_ms: Some(1),
            },
        )
        .unwrap();
    assert_eq!(
        store.snapshot(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(store.mark_tool_started(&lease, &call.id).is_err());
    let reconciliation = Reconciliation {
        expected: store.tool_reconciliation_origin(&id, &call.id).unwrap(),
        call_id: call.id,
        actor: "host".into(),
        evidence: "checked".into(),
        outcome: ReconciledOutcome::NotExecuted {
            reason: "no effect".into(),
        },
    };
    let store = store.with_reconciliation_verifier(Arc::new(ExactReceipt(
        cayw_core::ReconciliationRequest::Tool {
            reconciliation: reconciliation.clone(),
        },
    )));
    assert!(matches!(
        store.reconcile(&id, reconciliation),
        Err(CoreError::Busy)
    ));
}

#[test]
fn model_declarations_cannot_reuse_a_persisted_call_identifier() {
    let (_database, store, id, lease, call) = fixture();
    store.recover(&id).unwrap();
    let lease = store.begin_attempt(&id, &lease.worker_id).unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let message = Message::Assistant(model_message(vec![call]));
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: message.clone(),
            },
        )
        .unwrap();
    let before = store.snapshot(&id).unwrap();
    assert!(
        store
            .append_agent_event(&lease, &AgentEvent::MessageEnd { message })
            .is_err()
    );
    assert_eq!(store.snapshot(&id).unwrap(), before);
}
