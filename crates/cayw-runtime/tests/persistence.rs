use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_core::{CallState, CapabilityOutcome, SessionState};
use cayw_ipc::{ClientError, RpcError, RpcHandler};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, ScopedPermission, TaskState};
use futures::stream;
use rusqlite::Connection;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct WriteProvider(AtomicUsize);
impl Provider for WriteProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        let answer = cayw_runtime::fixture::script_response(
            &request,
            "return cayw.call('fs.write', {path='written.txt', content='durable effect'})",
            &format!("write-{}", request.messages.len()),
        );
        Box::pin(stream::iter([AssistantEvent::Done { message: answer }]))
    }
}

fn open(temp: &tempfile::TempDir, provider: Arc<WriteProvider>) -> Runtime {
    Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        provider,
        Model::new("fixture", "persistent-failure"),
        RuntimeConfig {
            max_concurrent_runs: 1,
            max_queued_runs: 1,
            permissions: vec![ScopedPermission::exact(
                "fs.write",
                json!({"path":"written.txt","content":"durable effect"}),
            )],
            ..Default::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn failed_tool_terminal_keeps_capacity_and_reports_the_original_unknown_error() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let provider = Arc::new(WriteProvider(AtomicUsize::new(0)));
    let rt = open(&temp, provider.clone());
    let id = rt.create(None).unwrap().id;
    let other = rt.create(None).unwrap().id;
    let db = Connection::open(temp.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_tool_terminal BEFORE INSERT ON core_events
         WHEN json_extract(NEW.record, '$.event.type')='agent'
           AND json_extract(NEW.record, '$.event.event.type')='tool_execution_end'
         BEGIN SELECT RAISE(ABORT, 'injected original tool terminal failure'); END;",
    )
    .unwrap();

    let error = rt
        .call(
            "session.prompt",
            json!({"session_id":id,"text":"write once"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        std::fs::read(temp.path().join("workspace/written.txt")).unwrap(),
        b"durable effect"
    );
    assert!(
        error
            .message
            .contains("injected original tool terminal failure"),
        "original recording error was lost: {error:?}"
    );
    let error: ClientError = error.into();
    assert!(
        error.outcome_unknown,
        "RPC incorrectly classifies {error:?}"
    );
    assert_eq!(rt.health().unwrap().running, 1);
    let task = rt.tasks(&id).unwrap().remove(0);
    assert_ne!(task.state, TaskState::Cancelled);
    assert_ne!(task.state, TaskState::Failed);
    assert_ne!(task.state, TaskState::Completed);
    let snapshot = rt.get(&id).unwrap();
    assert!(snapshot.attempt.is_some());
    assert_eq!(snapshot.unresolved_calls.len(), 1);
    assert_eq!(snapshot.unresolved_calls[0].state, CallState::Started);
    assert!(rt.enqueue(&id, "retry").is_err());
    assert!(matches!(
        rt.enqueue(&other, "use capacity"),
        Err(RuntimeError::QueueFull)
    ));
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    assert!(!rt.audit(&id, 0, 100).unwrap().iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "run_cancelled" | "run_failed" | "run_finished"
        )
    }));

    drop(rt);
    let rt = open(&temp, provider.clone());
    assert_eq!(rt.health().unwrap().running, 1);
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    db.execute_batch("DROP TRIGGER fail_tool_terminal").unwrap();
    let recovered = rt.recover(&id).unwrap();
    // Losing the model tool reply does not erase the known child terminal or
    // create a second unknown effect for the same VM.
    assert_eq!(recovered.state, SessionState::Idle);
    assert!(recovered.unresolved_calls.is_empty());
    assert!(recovered.unresolved_capabilities.is_empty());
    assert!(rt.events(&id, 0, 1000).unwrap().iter().any(|event| {
        matches!(&event.event, cayw_core::CoreEvent::CapabilitySettled { call }
            if matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })))
    }));
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    rt.enqueue(&id, "explicit next task").unwrap();
}

struct YieldingWriteProvider(AtomicUsize);
impl Provider for YieldingWriteProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        let answer = cayw_runtime::fixture::script_response(
            &request,
            "yield_control(); return cayw.call('fs.write', {path='written.txt', content='durable effect'})",
            &format!("yield-write-{}", request.messages.len()),
        );
        Box::pin(stream::iter([AssistantEvent::Done { message: answer }]))
    }
}

#[tokio::test]
async fn lost_registered_wait_reply_recovers_known_child_without_replay() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let provider = Arc::new(YieldingWriteProvider(AtomicUsize::new(0)));
    let config = RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 1,
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"written.txt","content":"durable effect"}),
        )],
        ..Default::default()
    };
    let rt = Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        provider.clone(),
        Model::new("fixture", "wait-recovery"),
        config.clone(),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let db = Connection::open(temp.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_wait_terminal BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record, '$.event.type')='agent'
          AND json_extract(NEW.record, '$.event.event.type')='tool_execution_end'
          AND json_extract(NEW.record, '$.event.event.call.arguments.action')='wait'
        BEGIN SELECT RAISE(ABORT,'injected wait reply failure'); END;",
    )
    .unwrap();
    let error = rt
        .prompt(&id, "write once after yielding", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("injected wait reply failure"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(temp.path().join("workspace/written.txt")).unwrap(),
        b"durable effect"
    );
    let before = rt.get(&id).unwrap();
    assert_eq!(before.unresolved_calls.len(), 1);
    let wait_id = before.unresolved_calls[0].call.id.clone();
    assert!(
        rt.events(&id, 0, 1000)
            .unwrap()
            .iter()
            .any(|event| matches!(&event.event,
        cayw_core::CoreEvent::ExecutionWaitRegistered { binding } if binding.call_id == wait_id))
    );
    assert_eq!(rt.health().unwrap().running, 1);
    assert_eq!(provider.0.load(Ordering::SeqCst), 2);
    drop(rt);
    db.execute_batch("DROP TRIGGER fail_wait_terminal").unwrap();
    let rt = Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        provider.clone(),
        Model::new("fixture", "wait-recovery"),
        config,
    )
    .unwrap();
    let recovered = rt.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    assert!(recovered.unresolved_calls.is_empty());
    assert!(recovered.unresolved_capabilities.is_empty());
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(provider.0.load(Ordering::SeqCst), 2);
    assert_eq!(rt.recover(&id).unwrap().messages, recovered.messages);
    let calls = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            cayw_core::CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert!(matches!(
        calls[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    rt.enqueue(&id, "explicit next task").unwrap();
}

#[tokio::test]
async fn failed_attempt_finish_keeps_the_known_tool_result_and_holds_capacity() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let provider = Arc::new(WriteProvider(AtomicUsize::new(0)));
    let rt = open(&temp, provider);
    let id = rt.create(None).unwrap().id;
    let db = Connection::open(temp.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_attempt_finish BEFORE INSERT ON core_events
         WHEN json_extract(NEW.record, '$.event.type')='attempt_finished'
         BEGIN SELECT RAISE(ABORT, 'injected attempt finish failure'); END;",
    )
    .unwrap();
    let error = rt
        .prompt(&id, "write", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected attempt finish failure")
    );
    assert_eq!(rt.health().unwrap().running, 1);
    let snapshot = rt.get(&id).unwrap();
    assert!(snapshot.attempt.is_some());
    assert!(snapshot.unresolved_calls.is_empty());
    assert!(snapshot.messages.iter().any(|message| {
        matches!(message, Message::ToolResult(result) if !result.is_error && !result.outcome_unknown)
    }));
    let rpc: RpcError = error.into();
    let client: ClientError = rpc.into();
    assert!(
        !client.outcome_unknown,
        "known effect was incorrectly made unknown"
    );
    db.execute_batch("DROP TRIGGER fail_attempt_finish")
        .unwrap();
    let recovered = rt.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    assert!(recovered.unresolved_calls.is_empty());
    assert_eq!(rt.health().unwrap().running, 0);
}

#[tokio::test]
async fn failed_runtime_terminal_audit_cannot_release_the_task_slot() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let provider = Arc::new(WriteProvider(AtomicUsize::new(0)));
    let rt = open(&temp, provider);
    let id = rt.create(None).unwrap().id;
    let other = rt.create(None).unwrap().id;
    let db = Connection::open(temp.path().join("sessions.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_runtime_terminal BEFORE INSERT ON runtime_audit
         WHEN NEW.kind='run_finished'
         BEGIN SELECT RAISE(ABORT, 'injected runtime terminal failure'); END;",
    )
    .unwrap();
    let error = rt
        .prompt(&id, "write", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected runtime terminal failure")
    );
    assert_eq!(rt.health().unwrap().running, 1);
    assert!(rt.get(&id).unwrap().attempt.is_none());
    assert!(matches!(
        rt.enqueue(&other, "use capacity"),
        Err(RuntimeError::QueueFull)
    ));
    assert!(rt.enqueue(&id, "retry").is_err());
    db.execute_batch("DROP TRIGGER fail_runtime_terminal")
        .unwrap();
    rt.recover(&id).unwrap();
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(
        std::fs::read(temp.path().join("workspace/written.txt")).unwrap(),
        b"durable effect"
    );
}
