use cayw_agent::{ai::*, *};
use futures::{StreamExt, stream};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Notify;

fn model() -> Model {
    Model::new("fixture", "test")
}
fn text(text: &str) -> AssistantMessage {
    let mut message = AssistantMessage::new(&model());
    message.content = vec![Content::text(text)];
    message.stop_reason = StopReason::Stop;
    message
}
fn call() -> AssistantMessage {
    let mut message = text("");
    message.content = vec![Content::ToolCall(ToolCall::new(
        "one",
        "increment",
        json!({"n":1}),
    ))];
    message.stop_reason = StopReason::ToolUse;
    message
}
struct Script {
    output: Mutex<VecDeque<AssistantMessage>>,
    requests: Mutex<Vec<ModelRequest>>,
}
impl Script {
    fn new(output: Vec<AssistantMessage>) -> Arc<Self> {
        Arc::new(Self {
            output: Mutex::new(output.into()),
            requests: Mutex::new(vec![]),
        })
    }
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.requests.lock().unwrap().push(request);
        stream::iter([AssistantEvent::Done {
            message: self
                .output
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request"),
        }])
        .boxed()
    }
}
struct Increment {
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl AgentTool for Increment {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "increment".into(),
            description: "increase n".into(),
            parameters: json!({"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]}),
        }
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            details: Some(json!({"original":true})),
            structured_content: Some(json!({"n":2})),
            ..ToolResult::text((context.arguments["n"].as_i64().unwrap() + 1).to_string())
        })
    }
}

struct Hooks {
    control: AgentControl,
    order: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait]
impl AgentHooks for Hooks {
    async fn prepare_request(
        &self,
        mut context: AgentContext,
        _: CancellationToken,
    ) -> Result<Option<AgentContext>, String> {
        self.order.lock().unwrap().push("request");
        assert!(
            context
                .messages
                .iter()
                .any(|message| matches!(message, Message::User { .. }))
        );
        context.model.id = "replacement".into();
        context.options.thinking_level = ThinkingLevel::Low;
        Ok(Some(context))
    }
    async fn before_tool_call(
        &self,
        context: ToolHookContext,
        _: CancellationToken,
    ) -> Result<BeforeToolDecision, String> {
        self.order.lock().unwrap().push("before");
        assert!(matches!(
            self.control.snapshot().messages.last(),
            Some(Message::Assistant(_))
        ));
        assert_eq!(context.arguments, json!({"n":1}));
        Ok(BeforeToolDecision::default())
    }
    async fn after_tool_call(
        &self,
        _: ToolHookContext,
        result: ToolResult,
        _: CancellationToken,
    ) -> Result<ToolResultPatch, String> {
        self.order.lock().unwrap().push("after");
        assert!(result.structured_content.is_some());
        Ok(ToolResultPatch {
            content: Some(vec![Content::text("redacted")]),
            ..Default::default()
        })
    }
    async fn finish_turn(
        &self,
        turn: TurnContext,
        _: CancellationToken,
    ) -> Result<TurnDecision, String> {
        self.order.lock().unwrap().push("finish");
        if !turn.tool_results.is_empty() {
            assert!(matches!(
                self.control.snapshot().messages.last(),
                Some(Message::ToolResult(_))
            ));
        }
        Ok(TurnDecision::Auto)
    }
    async fn prepare_next_turn(
        &self,
        _: TurnContext,
        _: CancellationToken,
    ) -> Result<NextTurnUpdate, String> {
        self.order.lock().unwrap().push("next");
        Ok(NextTurnUpdate {
            messages: vec![Message::user("from next hook")],
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn hooks_observe_committed_state_and_replace_request_and_tool_result() {
    let provider = Script::new(vec![call(), text("done")]);
    let agent = Agent::new(provider.clone(), model());
    let calls = Arc::new(AtomicUsize::new(0));
    agent
        .set_tools(vec![Arc::new(Increment {
            calls: calls.clone(),
        })])
        .unwrap();
    let order = Arc::new(Mutex::new(vec![]));
    agent
        .set_hooks(Arc::new(Hooks {
            control: agent.control(),
            order: order.clone(),
        }))
        .unwrap();
    let result = agent.prompt(Message::user("start")).await.unwrap();
    assert_eq!(
        *order.lock().unwrap(),
        vec![
            "request", "before", "after", "finish", "next", "request", "finish"
        ]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.model.id == "replacement"
                && request.options.thinking_level == ThinkingLevel::Low)
    );
    assert_eq!(
        requests[1].messages.last().unwrap().text(),
        "from next hook"
    );
    let result = result
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert_eq!(content_text(&result.content), "redacted");
    assert_eq!(result.details, Some(json!({"original":true})));
    assert_eq!(result.structured_content, None);
}

struct Block;
#[async_trait]
impl AgentHooks for Block {
    async fn before_tool_call(
        &self,
        _: ToolHookContext,
        _: CancellationToken,
    ) -> Result<BeforeToolDecision, String> {
        Ok(BeforeToolDecision {
            block: true,
            reason: Some("permission denied".into()),
            terminate: true,
        })
    }
}
#[tokio::test]
async fn blocked_terminating_batch_does_not_request_another_model_turn() {
    let provider = Script::new(vec![call()]);
    let agent = Agent::new(provider.clone(), model());
    let calls = Arc::new(AtomicUsize::new(0));
    agent
        .set_tools(vec![Arc::new(Increment {
            calls: calls.clone(),
        })])
        .unwrap();
    agent.set_hooks(Arc::new(Block)).unwrap();
    let result = agent.prompt(Message::user("start")).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(result.messages.iter().any(|message| matches!(message, Message::ToolResult(result) if result.is_error && content_text(&result.content) == "permission denied")));
}

struct WaitingTool {
    entered: Arc<Notify>,
    token: Arc<Mutex<Option<CancellationToken>>>,
}
#[async_trait]
impl AgentTool for WaitingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "increment".into(),
            description: "wait".into(),
            parameters: json!({"type":"object"}),
        }
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        *self.token.lock().unwrap() = Some(context.cancel);
        self.entered.notify_one();
        futures::future::pending().await
    }
}
#[tokio::test]
async fn cancelled_or_dropped_running_tool_requires_reconciliation() {
    for dropped in [false, true] {
        let entered = Arc::new(Notify::new());
        let token = Arc::new(Mutex::new(None));
        let provider = Script::new(vec![call()]);
        let agent = Agent::new(provider.clone(), model());
        agent
            .set_tools(vec![Arc::new(WaitingTool {
                entered: entered.clone(),
                token: token.clone(),
            })])
            .unwrap();
        let worker = agent.clone();
        let task = tokio::spawn(async move { worker.prompt(Message::user("start")).await });
        entered.notified().await;
        if dropped {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            agent.control().abort();
            let result = task.await.unwrap().unwrap();
            assert_eq!(result.last_message.stop_reason, StopReason::Aborted);
            assert!(result.messages.iter().any(
                |message| matches!(message, Message::ToolResult(result) if result.outcome_unknown)
            ));
        }
        assert!(token.lock().unwrap().as_ref().unwrap().is_cancelled());
        assert!(!agent.snapshot().is_running);
        assert!(agent.snapshot().needs_reconciliation);
        assert_eq!(
            agent.continue_run().await.unwrap_err(),
            AgentError::UnresolvedToolEffects
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }
}

struct LateSteering {
    control: AgentControl,
}
#[async_trait]
impl AgentHooks for LateSteering {
    async fn prepare_next_turn(
        &self,
        _: TurnContext,
        _: CancellationToken,
    ) -> Result<NextTurnUpdate, String> {
        self.control.steer(Message::user("late steering"));
        Ok(NextTurnUpdate::default())
    }
}
#[tokio::test]
async fn steering_added_during_prepare_next_turn_is_polled_again() {
    let provider = Script::new(vec![call(), text("done")]);
    let agent = Agent::new(provider.clone(), model());
    agent
        .set_tools(vec![Arc::new(Increment {
            calls: Arc::new(AtomicUsize::new(0)),
        })])
        .unwrap();
    agent
        .set_hooks(Arc::new(LateSteering {
            control: agent.control(),
        }))
        .unwrap();
    agent.prompt(Message::user("start")).await.unwrap();
    assert_eq!(
        provider.requests.lock().unwrap()[1]
            .messages
            .last()
            .unwrap()
            .text(),
        "late steering"
    );
}

#[tokio::test]
async fn queue_modes_control_drain_size() {
    for mode in [QueueMode::All, QueueMode::OneAtATime] {
        let count = if mode == QueueMode::All { 2 } else { 3 };
        let provider = Script::new((0..count).map(|_| text("done")).collect());
        let agent = Agent::new(provider.clone(), model());
        agent.set_queue_modes(mode, mode).unwrap();
        agent.control().follow_up(Message::user("first"));
        agent.control().follow_up(Message::user("second"));
        agent.prompt(Message::user("start")).await.unwrap();
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), count);
        let users = requests[1]
            .messages
            .iter()
            .filter(|message| matches!(message, Message::User { .. }))
            .count();
        assert_eq!(users, if mode == QueueMode::All { 3 } else { 2 });
    }
}

struct BadPrepare {
    panic: bool,
}
#[async_trait]
impl AgentHooks for BadPrepare {
    async fn prepare_request(
        &self,
        _: AgentContext,
        _: CancellationToken,
    ) -> Result<Option<AgentContext>, String> {
        if self.panic {
            panic!("hook panic");
        }
        Err("hook failed".into())
    }
}
#[tokio::test]
async fn hook_failures_and_panics_become_terminal_assistant_errors() {
    for panic in [false, true] {
        let provider = Script::new(vec![]);
        let agent = Agent::new(provider.clone(), model());
        agent.set_hooks(Arc::new(BadPrepare { panic })).unwrap();
        let result = agent.prompt(Message::user("start")).await.unwrap();
        assert_eq!(result.last_message.stop_reason, StopReason::Error);
        assert!(!agent.snapshot().is_running);
        assert!(provider.requests.lock().unwrap().is_empty());
    }
}
