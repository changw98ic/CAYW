use cayw_agent::{ai::*, *};
use futures::{StreamExt, stream};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Events(Mutex<VecDeque<AssistantEvent>>);
impl Provider for Events {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        let events = std::mem::take(&mut *self.0.lock().unwrap());
        stream::unfold(events, |mut events| async move {
            tokio::task::yield_now().await;
            events.pop_front().map(|event| (event, events))
        })
        .boxed()
    }
}

#[tokio::test]
async fn pending_between_deltas_keeps_complete_text_without_repeated_full_updates() {
    let model = Model::new("fixture", "incremental");
    let mut message = AssistantMessage::new(&model);
    let mut events = VecDeque::from([AssistantEvent::Start {
        message: message.clone(),
    }]);
    message.content.push(Content::text(""));
    events.push_back(AssistantEvent::TextStart {
        index: 0,
        partial: message.clone(),
    });
    let prefix = "x".repeat(1024 * 1024);
    events.push_back(AssistantEvent::TextDelta {
        index: 0,
        delta: prefix.clone(),
    });
    for _ in 0..100 {
        events.push_back(AssistantEvent::TextDelta {
            index: 0,
            delta: "y".into(),
        });
    }
    let complete = format!("{prefix}{}", "y".repeat(100));
    message.content = vec![Content::text(&complete)];
    events.push_back(AssistantEvent::TextEnd {
        index: 0,
        partial: message.clone(),
    });
    message.stop_reason = StopReason::Stop;
    events.push_back(AssistantEvent::Done { message });
    let agent = Agent::new(Arc::new(Events(Mutex::new(events))), model);
    let bytes = Arc::new(AtomicUsize::new(0));
    let deltas = Arc::new(AtomicUsize::new(0));
    let observed_bytes = bytes.clone();
    let observed_deltas = deltas.clone();
    agent.subscribe(move |update, _| {
        if let AgentEvent::MessageUpdate { event } = &update {
            observed_bytes.fetch_add(serde_json::to_vec(&update).unwrap().len(), Ordering::SeqCst);
            if matches!(**event, AssistantEvent::TextDelta { .. }) {
                observed_deltas.fetch_add(1, Ordering::SeqCst);
            }
        }
        async { Ok(()) }
    });
    let result = agent.prompt(Message::user("stream")).await.unwrap();
    assert_eq!(result.last_message.content, vec![Content::text(&complete)]);
    assert_eq!(deltas.load(Ordering::SeqCst), 101);
    assert!(
        bytes.load(Ordering::SeqCst) < complete.len() * 4,
        "updates must carry deltas, not 101 copies of the growing reply"
    );
}

#[tokio::test]
async fn cancellation_preserves_unfinished_tool_argument_prefix() {
    let model = Model::new("fixture", "incremental");
    let mut message = AssistantMessage::new(&model);
    let start = AssistantEvent::Start {
        message: message.clone(),
    };
    message.content.push(Content::ToolCall(ToolCall::new(
        "call",
        "write",
        serde_json::json!({}),
    )));
    let prefix = "{\"path\":\"unfinished";
    let events = VecDeque::from([
        start,
        AssistantEvent::ToolCallStart {
            index: 0,
            partial: message,
        },
        AssistantEvent::ToolCallDelta {
            index: 0,
            delta: prefix.into(),
        },
    ]);
    let agent = Agent::new(Arc::new(Events(Mutex::new(events))), model);
    let control = agent.control();
    agent.subscribe(move |update,_| {
        if matches!(update,AgentEvent::MessageUpdate {event} if matches!(*event,AssistantEvent::ToolCallDelta {..})) {control.abort();}
        async {Ok(())}
    });
    let result = agent.prompt(Message::user("cancel stream")).await.unwrap();
    assert_eq!(result.last_message.stop_reason, StopReason::Aborted);
    let calls = result.last_message.tool_calls();
    assert_eq!(calls[0].arguments, serde_json::json!(prefix));
    assert!(calls[0].arguments_error.is_some());
}
