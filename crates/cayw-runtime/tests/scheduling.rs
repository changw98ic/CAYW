use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

struct OrderedProvider {
    entered: Arc<Notify>,
    release: Arc<Semaphore>,
    order: Arc<Mutex<Vec<String>>>,
}
impl Provider for OrderedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let text = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { .. } => Some(message.text()),
                _ => None,
            })
            .unwrap();
        self.order.lock().unwrap().push(text);
        self.entered.notify_one();
        let release = self.release.clone();
        Box::pin(async_stream::stream! {
            tokio::select! { _ = cancel.cancelled() => {}, permit = release.acquire_owned() => { permit.unwrap().forget(); } }
            let mut message = AssistantMessage::new(&request.model);
            message.stop_reason = StopReason::Stop; message.content.push(Content::text("finished"));
            yield AssistantEvent::Done { message };
        })
    }
}

// Removing the queue's global ticket order, failing to remove cancelled waiters,
// or freeing permits before cleanup breaks this observable session order.
#[tokio::test]
async fn sessions_start_in_fifo_order_and_cancelled_queued_session_never_starts() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let order = Arc::new(Mutex::new(vec![]));
    let provider = Arc::new(OrderedProvider {
        entered: entered.clone(),
        release: release.clone(),
        order: order.clone(),
    });
    let settings = RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 3,
        max_run_time: Duration::from_secs(10),
        ..Default::default()
    };
    let rt = Runtime::open(
        directory.path().join("sessions.db"),
        workspace,
        provider,
        Model::new("fixture", "scheduling"),
        settings,
    )
    .unwrap();
    let ids: Vec<_> = (0..4).map(|_| rt.create(None).unwrap().id).collect();
    let a_rt = rt.clone();
    let a_id = ids[0].clone();
    let a = tokio::spawn(async move { a_rt.prompt(&a_id, "a", CancellationToken::new()).await });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let b_rt = rt.clone();
    let admitted_b = rt.enqueue(&ids[1], "b").unwrap();
    let b_id = admitted_b.id.clone();
    let b = tokio::spawn(async move { b_rt.resume(&b_id, CancellationToken::new()).await });
    wait_queued(&rt, 1).await;
    let c_rt = rt.clone();
    let c_id = ids[2].clone();
    let c = tokio::spawn(async move { c_rt.prompt(&c_id, "c", CancellationToken::new()).await });
    wait_queued(&rt, 2).await;
    assert!(matches!(
        rt.prompt(&ids[3], "d", CancellationToken::new()).await,
        Err(RuntimeError::QueueFull)
    ));
    assert!(matches!(
        rt.prompt(&ids[0], "duplicate", CancellationToken::new())
            .await,
        Err(RuntimeError::Busy)
    ));
    rt.cancel(&rt.task(&admitted_b.id).unwrap().cancel_target())
        .unwrap();
    assert!(matches!(b.await.unwrap(), Err(RuntimeError::Cancelled)));
    release.add_permits(1);
    a.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    release.add_permits(1);
    c.await.unwrap().unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["a", "c"]);
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.health().unwrap().queued, 0);
}
async fn wait_queued(rt: &Runtime, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while rt.health().unwrap().queued != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
