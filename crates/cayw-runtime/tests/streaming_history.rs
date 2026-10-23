use cayw_agent::AgentEvent;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, UsageStatus,
};
use cayw_core::CoreEvent;
use cayw_runtime::{ModelCostStatus, ModelRequestTerminal, Runtime, RuntimeConfig, TaskState};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct FragmentedProvider(AtomicUsize);
impl Provider for FragmentedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async_stream::stream! {
            let mut message = AssistantMessage::new(&request.model);
            message.content.push(Content::text(""));
            for _ in 0..512 {
                if cancel.is_cancelled() {
                    return;
                }
                let Content::Text { text } = &mut message.content[0] else {
                    unreachable!();
                };
                text.push_str("0123456789abcdef");
                yield AssistantEvent::TextDelta {
                    index: 0,
                    delta: "0123456789abcdef".into(),
                    partial: message.clone(),
                };
            }
            message.stop_reason = StopReason::Stop;
            yield AssistantEvent::Done { message };
        })
    }
}

#[tokio::test]
async fn coalesced_usage_retains_all_original_text_events_and_reopened_history() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let path = directory.path().join("sessions.db");
    let provider = Arc::new(FragmentedProvider(AtomicUsize::new(0)));
    let model = Model::new("fixture", "streaming-history");
    let config = RuntimeConfig {
        max_run_time: Duration::from_secs(30),
        ..Default::default()
    };
    let runtime = Runtime::open(
        &path,
        &workspace,
        provider.clone(),
        model.clone(),
        config.clone(),
    )
    .unwrap();
    let id = runtime.create(None).unwrap().id;
    let result = runtime
        .prompt(
            &id,
            "retain each original fragment",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut after = 0;
    let mut deltas = 0;
    let mut observed = 0;
    let mut lifecycle = Vec::new();
    let mut lease = None;
    loop {
        let page = runtime.events(&id, after, 100).unwrap();
        if page.is_empty() {
            break;
        }
        for record in page {
            assert_eq!(record.cursor, after + 1);
            after = record.cursor;
            match record.event {
                CoreEvent::AttemptStarted { lease: original } => lease = Some(original),
                CoreEvent::Agent { event } => {
                    if let AgentEvent::MessageUpdate { message, event } = *event
                        && let AssistantEvent::TextDelta {
                            index,
                            delta,
                            partial,
                        } = *event
                    {
                        deltas += 1;
                        assert_eq!(index, 0);
                        assert_eq!(delta, "0123456789abcdef");
                        assert_eq!(partial, message);
                        assert_eq!(
                            message.content,
                            vec![Content::text("0123456789abcdef".repeat(deltas))]
                        );
                        assert_eq!(message.usage_status, UsageStatus::Unknown);
                    }
                }
                CoreEvent::Host { kind, data } if kind == "model_request" => {
                    let kind = data["record"].as_str().unwrap();
                    lifecycle.push(kind.to_string());
                    if kind == "observed" {
                        observed += 1;
                    }
                }
                _ => {}
            }
        }
    }
    assert_eq!(deltas, 512);
    assert_eq!(observed, 2);
    assert_eq!(
        lifecycle,
        ["reserved", "started", "observed", "observed", "finished"]
    );
    assert_eq!(after, result.cursor);
    assert_eq!(
        result.messages.last().unwrap().text(),
        "0123456789abcdef".repeat(512)
    );
    let task = runtime.tasks(&id).unwrap().remove(0);
    assert_eq!(task.state, TaskState::Completed);
    let ledger = runtime.model_requests(&id).unwrap();
    assert_eq!(ledger.requests.len(), 1);
    let request = &ledger.requests[0];
    assert_eq!(request.identity.task_id, task.id);
    assert_eq!(Some(request.identity.lease.clone()), lease);
    assert_eq!(request.terminal, Some(ModelRequestTerminal::Completed));
    assert_eq!(request.observations.len(), 2);
    assert_eq!(request.observations[0].observation.event, "text_delta");
    assert_eq!(request.observations[1].observation.event, "done");
    assert_eq!(
        request.observations[1].observation.stop_reason,
        StopReason::Stop
    );
    assert!(
        request
            .observations
            .iter()
            .all(|entry| entry.observation.usage_status == UsageStatus::Unknown)
    );
    assert_eq!(request.settled_tokens, None);
    assert_eq!(request.outstanding_reservation_tokens, ledger.token_limit);
    assert_eq!(ledger.reserved_tokens, ledger.token_limit);
    assert_eq!(ledger.settled_tokens, 0);
    assert!(ledger.budget_unknown);
    assert_eq!(ledger.cost_status, ModelCostStatus::Unknown);
    assert!(ledger.cost_amount.is_none() && ledger.cost_currency.is_none());
    assert!(
        matches!(result.messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Stop)
    );
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    drop(runtime);
    let reopened = Runtime::open(&path, &workspace, provider.clone(), model, config).unwrap();
    assert_eq!(reopened.get(&id).unwrap(), result);
    assert_eq!(reopened.model_requests(&id).unwrap(), ledger);
    assert_eq!(reopened.tasks(&id).unwrap(), vec![task]);
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
}
