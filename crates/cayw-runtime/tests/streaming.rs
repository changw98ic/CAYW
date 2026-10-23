use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_core::SessionState;
use cayw_runtime::{Runtime, RuntimeConfig, TaskState};
use futures::StreamExt;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

struct FragmentedProvider;
impl Provider for FragmentedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let (sender, mut receiver) = mpsc::channel(1);
        tokio::spawn(async move {
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
                if sender
                    .send(AssistantEvent::TextDelta {
                        index: 0,
                        delta: "0123456789abcdef".into(),
                        partial: message.clone(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            message.stop_reason = StopReason::Stop;
            let _ = sender.send(AssistantEvent::Done { message }).await;
        });
        async_stream::stream! {
            while let Some(event) = receiver.recv().await {
                yield event;
            }
        }
        .boxed()
    }
}

// Check that a bounded, fragmented response reaches durable history in full.
// Timer catch-up behavior is covered separately with a virtual clock.
#[tokio::test]
async fn fragmented_response_completes_with_durable_history() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let runtime = Runtime::open(
        directory.path().join("sessions.db"),
        &workspace,
        Arc::new(FragmentedProvider),
        Model::new("fixture", "fragmented"),
        RuntimeConfig {
            max_run_time: Duration::from_secs(30),
            ..Default::default()
        },
    )
    .unwrap();
    let id = runtime.create(None).unwrap().id;
    let result = runtime
        .prompt(
            &id,
            "receive the complete response",
            CancellationToken::new(),
        )
        .await
        .expect("bounded local response must finish before its task deadline");
    assert_eq!(result.state, SessionState::Idle);
    assert_eq!(
        result.messages.last().unwrap().text(),
        "0123456789abcdef".repeat(512)
    );
    assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Completed);
    let persisted = runtime.store().snapshot(&id).unwrap();
    assert_eq!(persisted.messages, result.messages);
    assert!(persisted.unresolved_calls.is_empty());
    assert!(persisted.unresolved_capabilities.is_empty());
}
