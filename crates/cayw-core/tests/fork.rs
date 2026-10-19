use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
use cayw_core::{
    ApprovalDecision, ApprovalInput, ApprovalTarget, CoreError, CoreEvent, SessionState,
    SessionStore,
};
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("cayw-core-fork-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn open(&self) -> SessionStore {
        SessionStore::open(self.0.join("sessions.sqlite")).unwrap()
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
fn planned(store: &SessionStore) -> (String, cayw_core::AttemptLease, AssistantMessage, ToolCall) {
    let id = store.create_session("source").unwrap().id;
    let lease = store.begin_attempt(&id, "source-worker").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    message(store, &lease, Message::user("save"));
    let call = ToolCall::new("fork-call", "save", json!({}));
    let mut assistant = AssistantMessage::new(&Model::new("fixture", "offline"));
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![Content::ToolCall(call.clone())];
    message(store, &lease, Message::Assistant(assistant.clone()));
    (id, lease, assistant, call)
}

// A fork must copy the actual paired content and preserve an auditable source cursor.
#[test]
fn confirmed_fork_copies_history_to_a_new_identity_and_leaves_source_unchanged() {
    let database = Database::new();
    let store = database.open();
    let (id, lease, assistant, call) = planned(&store);
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionStart { call: call.clone() },
        )
        .unwrap();
    store.mark_tool_started(&lease, &call.id).unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::ToolExecutionEnd {
                call,
                result: ToolResult::text("saved"),
                duration_ms: Some(3),
            },
        )
        .unwrap();
    let results = store
        .snapshot(&id)
        .unwrap()
        .messages
        .into_iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::TurnEnd {
                message: assistant,
                tool_results: results,
            },
        )
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentEnd { messages: vec![] })
        .unwrap();
    store.finish_attempt(&lease).unwrap();
    let source = store.snapshot(&id).unwrap();
    let original_log = store.events(&id, 0, 100).unwrap();
    assert!(store.fork_session(&id, "copy", false).is_err());
    let fork = store.fork_session(&id, "copy", true).unwrap();
    assert_ne!(source.id, fork.id);
    assert_eq!(fork.label, "copy");
    assert_eq!(fork.messages, source.messages);
    assert_eq!(fork.state, SessionState::Idle);
    assert!(fork.attempt.is_none());
    assert!(fork.pending_approvals.is_empty());
    assert!(fork.unresolved_calls.is_empty());
    assert_eq!(store.snapshot(&id).unwrap(), source);
    assert_eq!(store.events(&id, 0, 100).unwrap(), original_log);
    let records = store.events(&fork.id, 0, 100).unwrap();
    assert!(
        matches!(&records[1].event, CoreEvent::SessionForked { source_session_id, source_cursor, messages }
        if source_session_id == &id && *source_cursor == source.cursor && messages == &source.messages)
    );
    let lease = store.begin_attempt(&fork.id, "new-worker").unwrap();
    store
        .append_host_event(&lease, "fork_used", json!({}))
        .unwrap();
    drop(store);
    let store = database.open();
    assert_eq!(store.snapshot(&fork.id).unwrap().messages, source.messages);
    assert_eq!(store.snapshot(&id).unwrap(), source);
}

#[test]
fn active_and_unknown_sessions_cannot_fork_and_recovery_resolves_pending_approvals() {
    let database = Database::new();
    let store = database.open();
    let (id, lease, _assistant, call) = planned(&store);
    let active = store.snapshot(&id).unwrap();
    assert!(matches!(
        store.fork_session(&id, "copy", true),
        Err(CoreError::Busy)
    ));
    assert_eq!(store.snapshot(&id).unwrap(), active);
    let approval = store
        .request_approval(
            &lease,
            ApprovalInput {
                task_id: "task".into(),
                target: ApprovalTarget::ToolCall { call_id: call.id },
                capability: "save".into(),
                scope: "file".into(),
                reason: "ask".into(),
            },
        )
        .unwrap();
    let pending = store.snapshot(&id).unwrap();
    assert_eq!(pending.state, SessionState::AwaitingApproval);
    assert!(matches!(
        store.fork_session(&id, "copy", true),
        Err(CoreError::Busy)
    ));
    assert_eq!(store.snapshot(&id).unwrap(), pending);
    let recovered = store.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    assert!(recovered.pending_approvals.is_empty());
    let decision = store.approval(&id, &approval.id).unwrap().decision.unwrap();
    assert_eq!(decision.decision, ApprovalDecision::Deny);
    assert_eq!(decision.actor, "host-recovery");
    let fork = store.fork_session(&id, "copy", true).unwrap();
    assert_eq!(fork.messages, recovered.messages);
    assert_ne!(fork.id, id);
    assert_eq!(store.snapshot(&id).unwrap(), recovered);
    let (unknown, lease, _, call) = planned(&store);
    store
        .append_agent_event(&lease, &AgentEvent::ToolExecutionStart { call })
        .unwrap();
    store.mark_tool_started(&lease, "fork-call").unwrap();
    store.recover(&unknown).unwrap();
    let before = store.snapshot(&unknown).unwrap();
    assert!(matches!(
        store.fork_session(&unknown, "copy", true),
        Err(CoreError::NeedsReconciliation)
    ));
    assert_eq!(store.snapshot(&unknown).unwrap(), before);
    assert!(store.fork_session(&id, " ", true).is_err());
    assert_eq!(store.list_sessions().unwrap().len(), 3);
}
