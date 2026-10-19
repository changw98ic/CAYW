use cayw_agent::{ai::*, *};
use futures::{StreamExt, stream};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

struct CountProvider(AtomicUsize);
impl Provider for CountProvider {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&Model::new("fixture", "test"));
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            message.content = vec![Content::ToolCall(ToolCall::new("one", "effect", json!({})))];
            message.stop_reason = StopReason::ToolUse;
        } else {
            message.content = vec![Content::text("retried after unknown effect")];
            message.stop_reason = StopReason::Stop;
        }
        stream::iter([AssistantEvent::Done { message }]).boxed()
    }
}
struct UnknownEffect;
#[async_trait]
impl AgentTool for UnknownEffect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "effect".into(),
            description: String::new(),
            parameters: json!({"type":"object"}),
        }
    }
    async fn execute(&self, _: ToolContext) -> Result<ToolResult, String> {
        Ok(ToolResult {
            is_error: true,
            outcome_unknown: true,
            ..ToolResult::text("unknown effect")
        })
    }
}
struct BadNext;
#[async_trait]
impl AgentHooks for BadNext {
    async fn prepare_next_turn(
        &self,
        _: TurnContext,
        _: CancellationToken,
    ) -> Result<NextTurnUpdate, String> {
        Err("cannot prepare next request".into())
    }
}
struct WaitingNext(Arc<Notify>);
#[async_trait]
impl AgentHooks for WaitingNext {
    async fn prepare_next_turn(
        &self,
        _: TurnContext,
        _: CancellationToken,
    ) -> Result<NextTurnUpdate, String> {
        self.0.notify_one();
        futures::future::pending().await
    }
}
struct TextProvider;
impl Provider for TextProvider {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&Model::new("fixture", "test"));
        message.content = vec![Content::text("done")];
        message.stop_reason = StopReason::Stop;
        stream::iter([AssistantEvent::Done { message }]).boxed()
    }
}

fn correction_is_retained(agent: &Agent, follow_up: bool) -> bool {
    let (steering, follow) = agent.control().queued_messages();
    let queued = if follow_up { follow } else { steering };
    queued
        .iter()
        .chain(agent.snapshot().messages.iter())
        .any(|message| message.text() == "important correction")
}
fn queue_correction(agent: &Agent, follow_up: bool) {
    if follow_up {
        agent
            .control()
            .follow_up(Message::user("important correction"));
    } else {
        agent.control().steer(Message::user("important correction"));
    }
}

#[tokio::test]
async fn failed_next_turn_must_not_lose_queued_user_input() {
    for follow_up in [false, true] {
        let agent = Agent::new(Arc::new(TextProvider), Model::new("fixture", "test"));
        agent.set_hooks(Arc::new(BadNext)).unwrap();
        queue_correction(&agent, follow_up);
        let result = agent.prompt(Message::user("start")).await.unwrap();
        assert_eq!(result.last_message.stop_reason, StopReason::Error);
        assert!(
            correction_is_retained(&agent, follow_up),
            "queued input disappeared without reaching history"
        );
    }
}

#[tokio::test]
async fn cancelling_or_dropping_next_turn_must_not_lose_queued_user_input() {
    for dropped in [false, true] {
        for follow_up in [false, true] {
            let agent = Agent::new(Arc::new(TextProvider), Model::new("fixture", "test"));
            let entered = Arc::new(Notify::new());
            agent
                .set_hooks(Arc::new(WaitingNext(entered.clone())))
                .unwrap();
            queue_correction(&agent, follow_up);
            let worker = agent.clone();
            let run = tokio::spawn(async move { worker.prompt(Message::user("start")).await });
            entered.notified().await;
            if dropped {
                run.abort();
                assert!(run.await.unwrap_err().is_cancelled());
            } else {
                agent.control().abort();
                assert_eq!(
                    run.await.unwrap().unwrap().last_message.stop_reason,
                    StopReason::Aborted
                );
            }
            assert!(
                correction_is_retained(&agent, follow_up),
                "queued input disappeared during cancellation/drop"
            );
        }
    }
}

struct CountTextProvider(AtomicUsize);
impl Provider for CountTextProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        TextProvider.stream(request, cancel)
    }
}

#[tokio::test]
async fn unknown_effect_in_new_input_must_be_blocked_before_model_request() {
    let provider = Arc::new(CountTextProvider(AtomicUsize::new(0)));
    let agent = Agent::new(provider.clone(), Model::new("fixture", "test"));
    let message = Message::ToolResult(ToolResultMessage {
        tool_call_id: "unresolved".into(),
        tool_name: "effect".into(),
        content: vec![],
        details: None,
        structured_content: None,
        usage: None,
        is_error: true,
        outcome_unknown: true,
        timestamp: 0,
        duration_ms: None,
    });
    let _ = agent.prompt(message).await;
    assert_eq!(
        provider.0.load(Ordering::SeqCst),
        0,
        "new input bypassed unresolved-effects guard"
    );
}

#[tokio::test]
async fn unknown_effect_must_stop_current_run_before_another_request() {
    let provider = Arc::new(CountProvider(AtomicUsize::new(0)));
    let agent = Agent::new(provider.clone(), Model::new("fixture", "test"));
    agent.set_tools(vec![Arc::new(UnknownEffect)]).unwrap();
    let result = agent.prompt(Message::user("start")).await.unwrap();
    assert!(agent.snapshot().needs_reconciliation);
    assert_eq!(result.last_message.stop_reason, StopReason::Error);
    assert_eq!(
        provider.0.load(Ordering::SeqCst),
        1,
        "model received a second request after unknown tool effect"
    );
    assert_eq!(
        agent.reset().unwrap_err(),
        AgentError::UnresolvedToolEffects
    );
}
