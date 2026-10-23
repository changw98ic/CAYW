use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, UsageStatus,
};
use cayw_cells::{CellPage, CellStatus};
use cayw_core::{
    ApprovalRequest, ApprovalTarget, CapabilityOutcome, CoreEvent, ExecutionState, SessionState,
};
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, RuntimeError};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

// The test controls model replies and human decisions separately. Only the real
// Cells -> Runtime -> Host path performs the file operations.
struct ModelRound {
    request: ModelRequest,
    reply: oneshot::Sender<AssistantMessage>,
}
struct ControlledProvider(mpsc::UnboundedSender<ModelRound>);
impl Provider for ControlledProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let model = request.model.clone();
        let (reply, answer) = oneshot::channel();
        self.0.send(ModelRound { request, reply }).unwrap();
        Box::pin(futures::stream::once(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => AssistantEvent::Error {
                    message: AssistantMessage::new(&model)
                        .failure(StopReason::Aborted, "controlled fixture cancelled"),
                },
                result = answer => AssistantEvent::Done {
                    message: result.expect("test must supply a model reply"),
                },
            }
        }))
    }
}
async fn next_round(receiver: &mut mpsc::UnboundedReceiver<ModelRound>) -> ModelRound {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}
fn reported_reply(model: &Model) -> AssistantMessage {
    let mut reply = AssistantMessage::new(model);
    // Fixed synthetic counters exercise accounting, not tokenization or billing.
    reply.usage.input = 4;
    reply.usage.output = 4;
    reply.usage.update_total();
    reply.usage_status = UsageStatus::Reported;
    reply
}
fn tool(round: ModelRound, id: &str, arguments: Value) {
    let mut reply = reported_reply(&round.request.model);
    reply.content.push(Content::ToolCall(ToolCall::new(
        id,
        "execute_code",
        arguments,
    )));
    reply.stop_reason = StopReason::ToolUse;
    round.reply.send(reply).unwrap();
}
fn page(round: &ModelRound, call_id: &str) -> CellPage {
    let Some(Message::ToolResult(result)) = round.request.messages.last() else {
        panic!("model must receive a completed tool reply");
    };
    assert_eq!(result.tool_call_id, call_id);
    assert!(!result.is_error, "{result:?}");
    assert!(!result.outcome_unknown);
    serde_json::from_value(result.structured_content.clone().unwrap()).unwrap()
}
fn wait(page: &CellPage, yield_time_ms: u64) -> Value {
    json!({
        "action":"wait", "cell_id":page.cell_id, "cursor":page.next_cursor,
        "yield_time_ms":yield_time_ms, "max_output_tokens":4096,
    })
}
async fn approval(rt: &Runtime, id: &str) -> ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(approval) = rt.get(id).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn assert_origin(
    rt: &Runtime,
    id: &str,
    approval: &ApprovalRequest,
    expected: (&str, &str, u64, &str, &Value),
) {
    let (cell_id, parent, sequence, capability, arguments) = expected;
    let ApprovalTarget::CapabilityCall { key } = &approval.input.target else {
        panic!("approval must target an internal capability");
    };
    assert_eq!(key.execution_id, cell_id);
    assert_eq!(key.sequence, sequence);
    assert_eq!(approval.input.capability, capability);
    let snapshot = rt.get(id).unwrap();
    let execution = snapshot
        .executions
        .iter()
        .find(|record| record.execution_id == cell_id)
        .unwrap();
    assert_eq!(execution.parent_call_id, parent);
    assert_eq!(execution.task_id, approval.input.task_id);
    assert_eq!(execution.lease.attempt_id, approval.attempt_id);
    assert_eq!(execution.task_id, rt.tasks(id).unwrap()[0].id);
    let planned = snapshot
        .unresolved_capabilities
        .iter()
        .find(|call| call.key == *key)
        .unwrap();
    assert_eq!(planned.parent_call_id, parent);
    assert_eq!(planned.task_id, execution.task_id);
    assert_eq!(planned.lease, execution.lease);
    assert_eq!(planned.source_hash, execution.source_hash);
    assert_eq!(&planned.arguments, arguments);
    // The launch reply is already in model history; later waits cannot replace it.
    assert!(snapshot.messages.iter().any(|message| {
        matches!(message, Message::ToolResult(result) if result.tool_call_id == parent)
    }));
    assert!(
        snapshot
            .unresolved_calls
            .iter()
            .all(|call| call.call.id != parent)
    );
}

#[tokio::test]
async fn live_task_ownership_prevents_peer_recovery_and_slot_release() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let provider = Arc::new(ControlledProvider(sender));
    let open = || {
        Runtime::open(
            dir.path().join("session.db"),
            dir.path().join("workspace"),
            provider.clone(),
            Model::new("fixture", "live-cell-lock"),
            RuntimeConfig::default(),
        )
        .unwrap()
    };
    let rt = open();
    let peer = open();
    let id = rt.create(None).unwrap().id;
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(
                &id,
                "hold the original task owner",
                CancellationToken::new(),
            )
            .await
        })
    };
    let round = next_round(&mut receiver).await;
    let before = rt.get(&id).unwrap();
    assert!(matches!(peer.recover(&id), Err(RuntimeError::Busy)));
    assert_eq!(rt.get(&id).unwrap(), before);
    assert_eq!(peer.health().unwrap().running, 1);
    tool(
        round,
        "live-lock-exec",
        json!({
            "action":"exec", "script":"yield_control(); return 'finished'",
            "yield_time_ms":30_000, "max_output_tokens":4096,
        }),
    );
    let round = next_round(&mut receiver).await;
    let yielded = page(&round, "live-lock-exec");
    assert_eq!(yielded.status, CellStatus::Yielded);
    let before = rt.get(&id).unwrap();
    assert!(matches!(peer.recover(&id), Err(RuntimeError::Busy)));
    assert_eq!(rt.get(&id).unwrap(), before);
    assert_eq!(peer.health().unwrap().running, 1);
    tool(round, "live-lock-wait", wait(&yielded, 30_000));
    let round = next_round(&mut receiver).await;
    assert_eq!(page(&round, "live-lock-wait").status, CellStatus::Completed);
    let mut reply = reported_reply(&round.request.model);
    reply
        .content
        .push(Content::text("finished with the original owner"));
    reply.stop_reason = StopReason::Stop;
    round.reply.send(reply).unwrap();
    assert_eq!(worker.await.unwrap().unwrap().state, SessionState::Idle);
    assert_eq!(peer.health().unwrap().running, 0);
}

#[tokio::test]
async fn interleaved_cells_keep_original_parents_and_exact_approval_sequences() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut config = RuntimeConfig {
        max_run_time: Duration::from_secs(30),
        ..Default::default()
    };
    config.codemode.timeout = Duration::from_secs(20);
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(ControlledProvider(sender)),
        Model::new("fixture", "interleaved-cell-origins"),
        config,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(
                &id,
                "run two independently approved cells",
                CancellationToken::new(),
            )
            .await
        })
    };
    tool(
        next_round(&mut receiver).await,
        "origin-exec-a",
        json!({
            "action":"exec", "yield_time_ms":30_000, "max_output_tokens":4096,
            "script":"yield_control(); cayw.call('fs.write',{path='a.txt',content='a'}); return cayw.call('fs.read',{path='a.txt'})",
        }),
    );
    let round = next_round(&mut receiver).await;
    let a = page(&round, "origin-exec-a");
    assert_eq!(a.status, CellStatus::Yielded);
    assert!(a.vm_alive);
    tool(
        round,
        "origin-exec-b",
        json!({
            "action":"exec", "yield_time_ms":30_000, "max_output_tokens":4096,
            "script":"yield_control(); cayw.call('fs.write',{path='b.txt',content='b'}); return cayw.call('fs.read',{path='b.txt'})",
        }),
    );
    let round = next_round(&mut receiver).await;
    let b = page(&round, "origin-exec-b");
    assert_eq!(b.status, CellStatus::Yielded);
    assert_ne!(a.cell_id, b.cell_id);
    tool(round, "origin-wait-a", wait(&a, 0));
    let round = next_round(&mut receiver).await;
    let a = page(&round, "origin-wait-a");
    tool(round, "origin-wait-b", wait(&b, 0));
    let round = next_round(&mut receiver).await;
    let b = page(&round, "origin-wait-b");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if rt.get(&id).unwrap().unresolved_capabilities.len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!dir.path().join("workspace/a.txt").exists());
    assert!(!dir.path().join("workspace/b.txt").exists());
    let expected = [
        (
            &a.cell_id,
            "origin-exec-a",
            0,
            "fs.write",
            json!({"path":"a.txt","content":"a"}),
        ),
        (
            &b.cell_id,
            "origin-exec-b",
            0,
            "fs.write",
            json!({"path":"b.txt","content":"b"}),
        ),
        (
            &a.cell_id,
            "origin-exec-a",
            1,
            "fs.read",
            json!({"path":"a.txt"}),
        ),
        (
            &b.cell_id,
            "origin-exec-b",
            1,
            "fs.read",
            json!({"path":"b.txt"}),
        ),
    ];
    for (cell, parent, sequence, capability, arguments) in &expected {
        let request = approval(&rt, &id).await;
        assert_origin(
            &rt,
            &id,
            &request,
            (cell, parent, *sequence, capability, arguments),
        );
        rt.approve(&id, &request.id, ApprovalChoice::AllowOnce, "fixture-host")
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if rt
                .get(&id)
                .unwrap()
                .executions
                .iter()
                .all(|execution| execution.state == ExecutionState::Completed)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tool(round, "origin-finish-a", wait(&a, 30_000));
    let round = next_round(&mut receiver).await;
    let terminal_a = page(&round, "origin-finish-a");
    assert_eq!(terminal_a.cell_id, a.cell_id);
    assert_eq!(terminal_a.status, CellStatus::Completed);
    assert!(!terminal_a.vm_alive && !terminal_a.has_more);
    tool(round, "origin-finish-b", wait(&b, 30_000));
    let round = next_round(&mut receiver).await;
    let terminal_b = page(&round, "origin-finish-b");
    assert_eq!(terminal_b.cell_id, b.cell_id);
    assert_eq!(terminal_b.status, CellStatus::Completed);
    assert!(!terminal_b.vm_alive && !terminal_b.has_more);
    let mut reply = reported_reply(&round.request.model);
    reply.content.push(Content::text("both cells completed"));
    reply.stop_reason = StopReason::Stop;
    round.reply.send(reply).unwrap();
    let result = worker.await.unwrap().unwrap();
    assert_eq!(result.state, SessionState::Idle);
    assert!(result.unresolved_calls.is_empty());
    assert!(result.unresolved_capabilities.is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/a.txt")).unwrap(),
        "a"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/b.txt")).unwrap(),
        "b"
    );
    let events = rt.events(&id, 0, 1000).unwrap();
    let starts: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilityStarted { call } => Some(call),
            _ => None,
        })
        .collect();
    let terminals: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 4);
    assert_eq!(terminals.len(), 4);
    for (cell, parent, sequence, capability, arguments) in expected {
        let start = starts
            .iter()
            .find(|call| call.key.execution_id == *cell && call.key.sequence == sequence)
            .unwrap();
        let terminal = terminals.iter().find(|call| call.key == start.key).unwrap();
        assert_eq!(start.parent_call_id, parent);
        assert_eq!(start.name, capability);
        assert_eq!(start.arguments, arguments);
        assert_eq!(terminal.parent_call_id, parent);
        assert_eq!(terminal.task_id, start.task_id);
        assert_eq!(terminal.lease, start.lease);
        assert!(matches!(
            terminal.outcome,
            Some(CapabilityOutcome::Completed { .. })
        ));
    }
}
