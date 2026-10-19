use cayw_agent::AgentEvent;
use cayw_ai::{AssistantMessage, CancellationToken, Content, Message, Model, StopReason, ToolCall};
use cayw_core::{ReconciledOutcome, Reconciliation, SessionState};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, fixture::OfflineProvider};
use serde_json::json;
use std::sync::Arc;

// Treating preflight as unexecuted after a worker disappears, silently resetting
// a lease, or copying unknown history into a fork would break this recovery test.
#[tokio::test]
async fn interrupted_execution_is_fenced_and_never_replayed_without_evidence() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("workspace")).unwrap();
    let open = || {
        Runtime::open(
            directory.path().join("sessions.db"),
            directory.path().join("workspace"),
            Arc::new(OfflineProvider::default()),
            Model::new("fixture", "recovery"),
            RuntimeConfig::default(),
        )
        .unwrap()
    };
    let rt = open();
    let session = rt.create(None).unwrap();
    let lease = rt
        .store()
        .begin_attempt(&session.id, "lost-worker")
        .unwrap();
    rt.store()
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    rt.store()
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let mut answer = AssistantMessage::new(&Model::new("fixture", "recovery"));
    let call = ToolCall::new(
        "lost-call",
        "execute_code",
        json!({"script":"return cayw.call('fs.write',{path='lost.txt',content='maybe'})"}),
    );
    answer.content.push(Content::ToolCall(call.clone()));
    answer.stop_reason = StopReason::ToolUse;
    rt.store()
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: Message::Assistant(answer.clone()),
            },
        )
        .unwrap();
    rt.store()
        .append_agent_event(
            &lease,
            &AgentEvent::MessageEnd {
                message: Message::Assistant(answer),
            },
        )
        .unwrap();
    rt.store()
        .append_agent_event(&lease, &AgentEvent::ToolExecutionStart { call })
        .unwrap();
    rt.store().mark_tool_started(&lease, "lost-call").unwrap();
    drop(rt);
    let reopened = open();
    assert!(matches!(
        reopened
            .prompt(&session.id, "continue", CancellationToken::new())
            .await,
        Err(RuntimeError::Busy)
    ));
    let recovered = reopened.recover(&session.id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert!(
        reopened
            .store()
            .append_agent_event(&lease, &AgentEvent::AgentEnd { messages: vec![] })
            .is_err()
    );
    assert!(matches!(
        reopened
            .prompt(&session.id, "retry", CancellationToken::new())
            .await,
        Err(RuntimeError::NeedsReconciliation)
    ));
    assert!(reopened.fork(&session.id, "unsafe fork", true).is_err());
    assert!(!directory.path().join("workspace/lost.txt").exists());
    reopened.reconcile(&session.id,Reconciliation { call_id: "lost-call".into(), actor: "test-user".into(), evidence: "This synthetic fixture never dispatched a Host call; verified the workspace has no lost.txt".into(), outcome: ReconciledOutcome::NotExecuted { reason: "The test stopped before the host call".into() } }).unwrap();
    assert_eq!(reopened.get(&session.id).unwrap().state, SessionState::Idle);
    assert!(reopened.fork(&session.id, "unconfirmed", false).is_err());
    let fork = reopened.fork(&session.id, "confirmed", true).unwrap();
    assert_ne!(fork.id, session.id);
    assert_eq!(fork.messages, reopened.get(&session.id).unwrap().messages);
}
