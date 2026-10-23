use cayw_agent::AgentEvent;
use cayw_ai::{AssistantMessage, CancellationToken, Content, Message, Model, StopReason, ToolCall};
use cayw_core::{ReconciledOutcome, Reconciliation, SessionState};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct ExplicitContinuation(AtomicUsize);
impl cayw_ai::Provider for ExplicitContinuation {
    fn stream(
        &self,
        request: cayw_ai::ModelRequest,
        _: CancellationToken,
    ) -> cayw_ai::AssistantStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        let mut message = AssistantMessage::new(&request.model);
        message.stop_reason = StopReason::Stop;
        message
            .content
            .push(Content::text("explicit fork task completed"));
        Box::pin(futures::stream::iter([cayw_ai::AssistantEvent::Done {
            message,
        }]))
    }
}

struct FixtureReceipt(PathBuf);
impl cayw_core::ReconciliationVerifier for FixtureReceipt {
    fn verify(&self, request: &cayw_core::ReconciliationRequest) -> Result<(), String> {
        let cayw_core::ReconciliationRequest::Tool { reconciliation } = request else {
            return Err("fixture has no capability receipt".into());
        };
        let receipt: Reconciliation =
            serde_json::from_slice(&std::fs::read(&self.0).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if reconciliation == &receipt && receipt.actor == "test-user" {
            Ok(())
        } else {
            Err("request differs from the original fixture receipt".into())
        }
    }
}

// Treating preflight as unexecuted after a worker disappears, silently resetting
// a lease, or copying unresolved history into a fork would break this recovery test.
#[tokio::test]
async fn interrupted_execution_is_fenced_and_never_replayed_without_evidence() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("workspace")).unwrap();
    let provider = Arc::new(ExplicitContinuation::default());
    let open = || {
        Runtime::open(
            directory.path().join("sessions.db"),
            directory.path().join("workspace"),
            provider.clone(),
            Model::new("fixture", "recovery"),
            RuntimeConfig {
                reconciliation_verifier: Some(Arc::new(FixtureReceipt(
                    directory.path().join("fixture-receipt.json"),
                ))),
                ..Default::default()
            },
        )
        .unwrap()
    };
    let rt = open();
    let session = rt.create(None).unwrap();
    let original = rt
        .enqueue(&session.id, "synthetic pre-dispatch fixture")
        .unwrap();
    let lease = rt
        .store()
        .begin_attempt(&session.id, "lost-worker")
        .unwrap();
    // This synthetic fixture seeds the original queue/Core boundary without
    // invoking a Provider or Host executor. Both records name the same Task.
    let db = rusqlite::Connection::open(directory.path().join("sessions.db")).unwrap();
    assert_eq!(db.execute(
        "UPDATE runtime_tasks SET state='running',worker=?2,attempt=?3,fence=1 WHERE id=?1 AND state='queued' AND fence=0",
        rusqlite::params![original.id, lease.worker_id, lease.attempt_id],
    ).unwrap(), 1);
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
    rt.store()
        .bind_tool_origin(&lease, "lost-call", &original.id)
        .unwrap();
    rt.store().mark_tool_started(&lease, "lost-call").unwrap();
    let expected = rt
        .store()
        .tool_reconciliation_origin(&session.id, "lost-call")
        .unwrap();
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
    let receipt = Reconciliation { call_id: "lost-call".into(), expected, actor: "test-user".into(), evidence: "This synthetic fixture never dispatched a Host call; verified the workspace has no lost.txt".into(), outcome: ReconciledOutcome::NotExecuted { reason: "The test stopped before the host call".into() } };
    // The fixture never invokes Host. Preserve that exact observed decision as
    // the trusted host's receipt before submitting it to the verifier.
    std::fs::write(
        directory.path().join("fixture-receipt.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    let mut forged = receipt.clone();
    forged.expected.task_id = "replacement-task".into();
    assert!(reopened.reconcile(&session.id, forged).is_err());
    reopened.reconcile(&session.id, receipt).unwrap();
    assert_eq!(reopened.get(&session.id).unwrap().state, SessionState::Idle);
    assert!(reopened.fork(&session.id, "unconfirmed", false).is_err());
    let fork = reopened.fork(&session.id, "confirmed", true).unwrap();
    assert_ne!(fork.id, session.id);
    assert_eq!(fork.messages, reopened.get(&session.id).unwrap().messages);
    let source = reopened.get(&session.id).unwrap();
    let source_events = reopened.events(&session.id, 0, 1000).unwrap();
    let fork_events = reopened.events(&fork.id, 0, 1000).unwrap();
    let fork_prefix = fork.messages.clone();
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    let completed = reopened.prompt(&fork.id, "explicit new fork task", CancellationToken::new())
        .await.expect("a confirmed fork with the original committed decision must accept an explicit new prompt");
    assert_eq!(completed.state, SessionState::Idle);
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    assert!(completed.messages.iter().any(|message| matches!(message,
        Message::Assistant(assistant) if assistant.stop_reason == StopReason::Stop
            && assistant.content.iter().any(|part| matches!(part, Content::Text { text, .. } if text == "explicit fork task completed")))));
    assert!(completed.executions.is_empty());
    assert!(completed.messages.starts_with(&fork_prefix));
    assert!(
        matches!(completed.messages.get(fork_prefix.len() - 1), Some(Message::ToolResult(result)) if result.outcome_unknown)
    );
    let after_events = reopened.events(&fork.id, 0, 1000).unwrap();
    assert_eq!(&after_events[..fork_events.len()], fork_events);
    assert!(after_events[fork_events.len()..].iter().any(
        |event| matches!(&event.event, cayw_core::CoreEvent::Agent { event }
        if matches!(event.as_ref(), AgentEvent::AgentEnd { .. }))
    ));
    assert_eq!(reopened.get(&session.id).unwrap().messages, source.messages);
    assert_eq!(
        reopened.events(&session.id, 0, 1000).unwrap(),
        source_events
    );
    assert!(!directory.path().join("workspace/lost.txt").exists());
    drop(reopened);
    let final_runtime = open();
    assert!(
        final_runtime
            .get(&fork.id)
            .unwrap()
            .messages
            .starts_with(&fork_prefix)
    );
    assert_eq!(
        final_runtime.get(&session.id).unwrap().messages,
        source.messages
    );
    assert!(!directory.path().join("workspace/lost.txt").exists());
}
