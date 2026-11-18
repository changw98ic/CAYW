use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_ipc::RpcHandler;
use cayw_runtime::{Runtime, RuntimeConfig, TaskRecord, TaskState};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

impl Provider for Recording {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let text = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                cayw_ai::Message::User { content, .. } => Some(cayw_ai::content_text(content)),
                _ => None,
            })
            .unwrap();
        self.0.lock().unwrap().push(text);
        let message = cayw_runtime::fixture::script_response(&request, "", "fixture");
        // An ordinary text terminal keeps the test focused on scheduling.
        let mut message = message;
        message.content = vec![cayw_ai::Content::text("done")];
        message.stop_reason = cayw_ai::StopReason::Stop;
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

#[tokio::test]
async fn submissions_are_durable_fifo_and_can_cancel_one_queued_task() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(Recording::default());
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path(),
        provider.clone(),
        Model::new("fixture", "queue"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let session = rt.create(None).unwrap();
    let first = rt.enqueue(&session.id, "first").unwrap();
    async fn submit(rt: &Runtime, params: Value) -> TaskRecord {
        serde_json::from_value(
            rt.call("task.submit_follow_up", params, CancellationToken::new())
                .await
                .unwrap(),
        )
        .unwrap()
    }
    let mut receipts = Vec::new();
    for i in 0..10 {
        let params = json!({"session_id":session.id,"request_id":uuid::Uuid::new_v4().to_string(),"text":format!("follow {i}")});
        let task = submit(&rt, params.clone()).await;
        assert_eq!(submit(&rt, params).await.id, task.id);
        receipts.push(task);
    }
    assert!(receipts.iter().all(|task| task.state == TaskState::Queued));
    assert!(rt.cancel(&receipts[4].cancel_target()).unwrap());
    drop(rt);
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path(),
        provider.clone(),
        Model::new("fixture", "queue"),
        RuntimeConfig::default(),
    )
    .unwrap();
    rt.resume(&first.id, CancellationToken::new())
        .await
        .unwrap();
    let tail = receipts.last().unwrap().id.clone();
    tokio::time::timeout(Duration::from_secs(15), async {
        while rt.task(&tail).unwrap().state != TaskState::Completed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let expected = std::iter::once("first".into())
        .chain((0..10).filter(|&i| i != 4).map(|i| format!("follow {i}")))
        .collect::<Vec<_>>();
    assert_eq!(*provider.0.lock().unwrap(), expected);
    assert_eq!(
        rt.task(&receipts[4].id).unwrap().state,
        TaskState::Cancelled
    );
}
