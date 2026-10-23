use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, UsageStatus,
};
use cayw_cells::{CellPage, CellStatus};
use cayw_core::{CapabilityOutcome, CoreEvent};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

struct Round {
    request: ModelRequest,
    answer: oneshot::Sender<AssistantMessage>,
}
struct Controlled(mpsc::UnboundedSender<Round>);
impl Provider for Controlled {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let model = request.model.clone();
        let (answer, receiver) = oneshot::channel();
        self.0.send(Round { request, answer }).unwrap();
        Box::pin(futures::stream::once(async move {
            tokio::select! { biased;
                _ = cancel.cancelled() => AssistantEvent::Error { message: AssistantMessage::new(&model).failure(StopReason::Aborted, "fixture stopped") },
                answer = receiver => AssistantEvent::Done { message: answer.unwrap() },
            }
        }))
    }
}
struct WriteSame(String);
impl Provider for WriteSame {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let script = format!(
            "return cayw.call('fs.write',{{path='a.txt',content='same bytes',overwrite=true,expected_sha256='{}'}})",
            self.0
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message: cayw_runtime::fixture::script_response(
                &request,
                &script,
                &format!("write-{}", request.messages.len()),
            ),
        }]))
    }
}
async fn next(receiver: &mut mpsc::UnboundedReceiver<Round>) -> Round {
    tokio::time::timeout(Duration::from_secs(15), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}
fn reported_answer(model: &Model) -> AssistantMessage {
    let mut answer = AssistantMessage::new(model);
    // Fixed synthetic counters exercise accounting, not tokenization or billing.
    answer.usage.input = 4;
    answer.usage.output = 4;
    answer.usage.update_total();
    answer.usage_status = UsageStatus::Reported;
    answer
}
fn tool(round: Round, id: &str, args: Value) {
    let mut answer = reported_answer(&round.request.model);
    answer.stop_reason = StopReason::ToolUse;
    answer
        .content
        .push(Content::ToolCall(ToolCall::new(id, "execute_code", args)));
    round.answer.send(answer).unwrap();
}
fn page(round: &Round) -> CellPage {
    let Some(Message::ToolResult(result)) = round.request.messages.last() else {
        panic!("expected original tool result")
    };
    assert!(!result.is_error, "{result:?}");
    serde_json::from_value(result.structured_content.clone().unwrap()).unwrap()
}

#[tokio::test]
async fn another_runtime_identical_write_changes_generation_for_two_original_cells_and_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "same bytes").unwrap();
    let path = dir.path().join("session.db");
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    let expected_sha256 = cayw_bridge::content_sha256("same bytes");
    config.permissions = vec![
        ScopedPermission::exact("fs.read", json!({"path":"a.txt"})),
        ScopedPermission::exact("fs.stat", json!({"path":"a.txt"})),
        ScopedPermission::exact(
            "fs.write",
            json!({"path":"a.txt","content":"same bytes","overwrite":true,"expected_sha256":expected_sha256}),
        ),
    ];
    let rt = Runtime::open(
        &path,
        &root,
        Arc::new(Controlled(sender)),
        Model::new("fixture", "generations"),
        config.clone(),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(&worker_id, "read across write", CancellationToken::new())
            .await
    });
    tool(
        next(&mut receiver).await,
        "parent-a",
        json!({"action":"exec","script":"cayw.call('fs.read',{path='a.txt'}); yield_control(); return cayw.call('fs.read',{path='a.txt'})","yield_time_ms":30000,"max_output_tokens":4096}),
    );
    let round = next(&mut receiver).await;
    let a = page(&round);
    assert_eq!(a.status, CellStatus::Yielded);
    tool(
        round,
        "parent-b",
        json!({"action":"exec","script":"cayw.call('fs.stat',{path='a.txt'}); cayw.call('fs.read',{path='a.txt'}); yield_control(); return cayw.call('fs.read',{path='a.txt'})","yield_time_ms":30000,"max_output_tokens":4096}),
    );
    let round = next(&mut receiver).await;
    let b = page(&round);
    assert_eq!(b.status, CellStatus::Yielded);
    assert_ne!(a.cell_id, b.cell_id);
    let peer = Runtime::open(
        &path,
        &root,
        Arc::new(WriteSame(expected_sha256)),
        Model::new("fixture", "writer"),
        config,
    )
    .unwrap();
    let writer_id = peer.create(None).unwrap().id;
    peer.prompt(&writer_id, "identical write", CancellationToken::new())
        .await
        .unwrap();
    let writer_events = peer.events(&writer_id, 0, 1000).unwrap();
    let write = writer_events
        .iter()
        .find_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "fs.write" => Some(call),
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(write.outcome, Some(CapabilityOutcome::Completed { .. })),
        "identical write did not complete: {write:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "same bytes"
    );
    tool(
        round,
        "wait-a",
        json!({"action":"wait","cell_id":a.cell_id,"cursor":0,"yield_time_ms":30000,"max_output_tokens":4096,"continue_execution":true}),
    );
    let round = next(&mut receiver).await;
    assert_eq!(page(&round).status, CellStatus::Completed);
    tool(
        round,
        "wait-b",
        json!({"action":"wait","cell_id":b.cell_id,"cursor":0,"yield_time_ms":30000,"max_output_tokens":4096,"continue_execution":true}),
    );
    let round = next(&mut receiver).await;
    assert_eq!(page(&round).status, CellStatus::Completed);
    let mut answer = reported_answer(&round.request.model);
    answer.stop_reason = StopReason::Stop;
    answer
        .content
        .push(Content::text("both original cells complete"));
    round.answer.send(answer).unwrap();
    worker.await.unwrap().unwrap();
    let events = rt.events(&id, 0, 1000).unwrap();
    let reads: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 4);
    assert_eq!(reads[0]["role"], "leader");
    assert_eq!(reads[1]["role"], "ready");
    assert_eq!(reads[2]["role"], "leader");
    assert_eq!(reads[3]["role"], "ready");
    let before = reads[0]["stamp"]["generation"].as_u64().unwrap();
    let after = reads[2]["stamp"]["generation"].as_u64().unwrap();
    assert_eq!(after, before + 1);
    assert_eq!(reads[0]["stamp"], reads[1]["stamp"]);
    assert_eq!(reads[2]["stamp"], reads[3]["stamp"]);
    assert_eq!(reads[0]["result_sha256"], reads[2]["result_sha256"]);
    assert_eq!(reads[0]["origin"]["sequence"], 1);
    assert_eq!(reads[1]["origin"]["sequence"], 2);
    assert_eq!(reads[2]["origin"]["sequence"], 2);
    assert_eq!(reads[3]["origin"]["sequence"], 3);
    assert_eq!(reads[0]["origin"]["execution"]["cell_id"], a.cell_id);
    assert_eq!(reads[1]["origin"]["execution"]["cell_id"], b.cell_id);
    assert_eq!(reads[0]["source"], reads[1]["source"]);
    assert_eq!(reads[2]["source"], reads[3]["source"]);
    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "fs.read" => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 4);
    for call in calls {
        assert!(matches!(
            call.outcome,
            Some(CapabilityOutcome::Completed { .. })
        ));
        assert_eq!(
            call.parent_call_id,
            if call.key.execution_id == a.cell_id {
                "parent-a"
            } else {
                "parent-b"
            }
        );
    }
    rt.recover(&id).unwrap();
}
