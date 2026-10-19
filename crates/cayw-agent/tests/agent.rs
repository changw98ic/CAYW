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

#[derive(Default)]
struct Scripted {
    responses: Mutex<VecDeque<Vec<AssistantEvent>>>,
    requests: Mutex<Vec<ModelRequest>>,
}
impl Scripted {
    fn from(messages: Vec<AssistantMessage>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(
                messages
                    .into_iter()
                    .map(|message| vec![AssistantEvent::Done { message }])
                    .collect(),
            ),
            ..Default::default()
        })
    }
}
impl Provider for Scripted {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.requests.lock().unwrap().push(request);
        stream::iter(
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected model request"),
        )
        .boxed()
    }
}
fn model() -> Model {
    Model::new("fixture", "test")
}
fn response(text: &str) -> AssistantMessage {
    let mut message = AssistantMessage::new(&model());
    message.content.push(Content::text(text));
    message.stop_reason = StopReason::Stop;
    message
}
fn tool_response() -> AssistantMessage {
    let mut message = response("");
    message.content = vec![Content::ToolCall(ToolCall::new(
        "call-1",
        "add",
        json!({"a":2,"b":3}),
    ))];
    message.stop_reason = StopReason::ToolUse;
    message
}
struct Add {
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl AgentTool for Add {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "add".into(),
            description: "add two integers".into(),
            parameters: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false}),
        }
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        context.progress.emit(ToolResult::text("adding")).await;
        Ok(ToolResult::text(
            (context.arguments["a"].as_i64().unwrap() + context.arguments["b"].as_i64().unwrap())
                .to_string(),
        ))
    }
}
fn with_add(provider: Arc<dyn Provider>) -> (Agent, Arc<AtomicUsize>) {
    let agent = Agent::new(provider, model());
    let calls = Arc::new(AtomicUsize::new(0));
    agent
        .set_tools(vec![Arc::new(Add {
            calls: calls.clone(),
        })])
        .unwrap();
    (agent, calls)
}
fn event_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_end",
    }
}

#[tokio::test]
async fn model_tool_model_loop_preserves_events_and_history() {
    let provider = Scripted::from(vec![tool_response(), response("5")]);
    let (agent, calls) = with_add(provider.clone());
    let events = Arc::new(Mutex::new(Vec::new()));
    let output = events.clone();
    agent.subscribe(move |event, _| {
        let output = output.clone();
        async move {
            output.lock().unwrap().push(event);
            Ok(())
        }
    });
    let result = agent.prompt(Message::user("add 2 and 3")).await.unwrap();
    assert_eq!(result.last_message.content, vec![Content::text("5")]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(current_system(&requests[0].messages).tools.len(), 1);
    assert!(
        matches!(requests[1].messages.last(), Some(Message::ToolResult(result)) if result.tool_call_id == "call-1" && content_text(&result.content) == "5")
    );
    assert_eq!(agent.snapshot().messages, result.messages);
    assert!(!agent.snapshot().is_running);
    let events = events.lock().unwrap();
    let names = events.iter().map(event_name).collect::<Vec<_>>();
    assert_eq!(names.first(), Some(&"agent_start"));
    assert_eq!(names.last(), Some(&"agent_end"));
    assert_eq!(
        names.iter().filter(|&&name| name == "turn_start").count(),
        2
    );
    let end_assistant = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::MessageEnd {
                    message: Message::Assistant(_)
                }
            )
        })
        .unwrap();
    let start_tool = names.iter().position(|&name| name == "tool_start").unwrap();
    let progress = names
        .iter()
        .position(|&name| name == "tool_update")
        .unwrap();
    let end_tool = names.iter().position(|&name| name == "tool_end").unwrap();
    assert!(end_assistant < start_tool && start_tool < progress && progress < end_tool);
}

#[tokio::test]
async fn invalid_unknown_and_truncated_tools_never_execute() {
    for kind in ["invalid", "unknown", "length", "incomplete"] {
        let mut message = tool_response();
        if let Content::ToolCall(call) = &mut message.content[0] {
            match kind {
                "invalid" => call.arguments = json!({"a":"2","b":3}),
                "unknown" => call.name = "missing".into(),
                "incomplete" => call.arguments_error = Some("incomplete JSON".into()),
                _ => {}
            }
        }
        if kind == "length" {
            message.stop_reason = StopReason::Length;
        }
        let provider = Scripted::from(vec![message, response("handled")]);
        let (agent, calls) = with_add(provider);
        let result = agent.prompt(Message::user("test")).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{kind}");
        assert!(
            result
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResult(result) if result.is_error)),
            "{kind}"
        );
    }
}

#[tokio::test]
async fn failed_or_incomplete_streams_end_without_executing_tools() {
    for terminal in [StopReason::Error, StopReason::Aborted, StopReason::Pending] {
        let mut message = tool_response();
        message.stop_reason = terminal;
        let provider = Scripted::from(vec![message]);
        let (agent, calls) = with_add(provider.clone());
        let result = agent.prompt(Message::user("test")).await.unwrap();
        assert!(result.last_message.stop_reason.is_failure());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }
    let provider = Arc::new(Scripted {
        responses: Mutex::new([vec![]].into()),
        ..Default::default()
    });
    let agent = Agent::new(provider, model());
    let result = agent.prompt(Message::user("test")).await.unwrap();
    assert_eq!(result.last_message.stop_reason, StopReason::Error);
    assert!(
        result
            .last_message
            .error_message
            .unwrap()
            .contains("terminal")
    );
}

#[tokio::test]
async fn empty_continue_does_not_consume_queues_and_assistant_tail_requires_input() {
    let provider = Scripted::from(vec![response("first"), response("second")]);
    let agent = Agent::new(provider.clone(), model());
    agent.control().steer(Message::user("queued"));
    assert_eq!(
        agent.continue_run().await.unwrap_err(),
        AgentError::EmptyHistory
    );
    assert_eq!(agent.control().queued_messages().0.len(), 1);
    agent.control().clear_queues();
    agent.prompt(Message::user("hello")).await.unwrap();
    assert_eq!(
        agent.continue_run().await.unwrap_err(),
        AgentError::NeedsInput
    );
    agent.control().follow_up(Message::user("next"));
    agent.continue_run().await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn steering_waits_for_tools_and_follow_up_waits_for_natural_stop() {
    let provider = Scripted::from(vec![
        tool_response(),
        response("intermediate"),
        response("finished"),
    ]);
    let (agent, calls) = with_add(provider.clone());
    let control = agent.control();
    control.steer(Message::user("steer"));
    control.follow_up(Message::user("follow"));
    agent.prompt(Message::user("start")).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let second = &requests[1].messages;
    assert_eq!(second.last().unwrap().text(), "steer");
    let result_index = second
        .iter()
        .position(|message| matches!(message, Message::ToolResult(_)))
        .unwrap();
    assert!(result_index < second.len() - 1);
    assert_eq!(requests[2].messages.last().unwrap().text(), "follow");
}

struct Finish {
    decision: TurnDecision,
    invocations: AtomicUsize,
}
#[async_trait]
impl AgentHooks for Finish {
    async fn finish_turn(
        &self,
        _: TurnContext,
        _: CancellationToken,
    ) -> Result<TurnDecision, String> {
        Ok(if self.invocations.fetch_add(1, Ordering::SeqCst) == 0 {
            self.decision
        } else {
            TurnDecision::Auto
        })
    }
}
#[tokio::test]
async fn finish_end_preserves_queues_and_continue_adds_only_one_request() {
    let provider = Scripted::from(vec![response("first")]);
    let agent = Agent::new(provider.clone(), model());
    agent
        .set_hooks(Arc::new(Finish {
            decision: TurnDecision::End,
            invocations: AtomicUsize::new(0),
        }))
        .unwrap();
    agent.control().steer(Message::user("keep"));
    agent.prompt(Message::user("start")).await.unwrap();
    assert_eq!(agent.control().queued_messages().0.len(), 1);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    for with_tool in [false, true] {
        let provider = Scripted::from(vec![
            if with_tool {
                tool_response()
            } else {
                response("first")
            },
            response("done"),
        ]);
        let (agent, _) = with_add(provider.clone());
        agent
            .set_hooks(Arc::new(Finish {
                decision: TurnDecision::Continue,
                invocations: AtomicUsize::new(0),
            }))
            .unwrap();
        agent.prompt(Message::user("start")).await.unwrap();
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn listeners_are_awaited_in_order_including_agent_end() {
    let agent = Agent::new(Scripted::from(vec![response("done")]), model());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let order = Arc::new(Mutex::new(vec![]));
    let (e, r, o) = (entered.clone(), release.clone(), order.clone());
    agent.subscribe(move |event, _| {
        let (e, r, o) = (e.clone(), r.clone(), o.clone());
        async move {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                o.lock().unwrap().push(1);
                e.notify_one();
                r.notified().await;
            }
            Ok(())
        }
    });
    let o = order.clone();
    agent.subscribe(move |event, _| {
        let o = o.clone();
        async move {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                o.lock().unwrap().push(2);
            }
            Ok(())
        }
    });
    let worker = agent.clone();
    let task = tokio::spawn(async move { worker.prompt(Message::user("start")).await });
    entered.notified().await;
    assert!(agent.snapshot().is_running);
    assert_eq!(agent.reset().unwrap_err(), AgentError::Busy);
    assert_eq!(*order.lock().unwrap(), vec![1]);
    let control = agent.control();
    let idle = tokio::spawn(async move { control.wait_for_idle().await });
    tokio::task::yield_now().await;
    assert!(!idle.is_finished());
    release.notify_one();
    task.await.unwrap().unwrap();
    idle.await.unwrap();
    assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    assert!(!agent.snapshot().is_running);
}

struct Stall {
    started: Arc<Notify>,
    token: Arc<Mutex<Option<CancellationToken>>>,
}
impl Provider for Stall {
    fn stream(&self, _: ModelRequest, token: CancellationToken) -> AssistantStream {
        *self.token.lock().unwrap() = Some(token);
        self.started.notify_one();
        stream::pending().boxed()
    }
}
#[tokio::test]
async fn abort_and_dropping_prompt_future_release_running_state() {
    for drop_future in [false, true] {
        let started = Arc::new(Notify::new());
        let token = Arc::new(Mutex::new(None));
        let agent = Agent::new(
            Arc::new(Stall {
                started: started.clone(),
                token: token.clone(),
            }),
            model(),
        );
        let worker = agent.clone();
        let task = tokio::spawn(async move { worker.prompt(Message::user("start")).await });
        started.notified().await;
        assert_eq!(
            agent.prompt(Message::user("busy")).await.unwrap_err(),
            AgentError::Busy
        );
        if drop_future {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            agent.control().abort();
            assert_eq!(
                task.await.unwrap().unwrap().last_message.stop_reason,
                StopReason::Aborted
            );
        }
        agent.control().wait_for_idle().await;
        assert!(!agent.snapshot().is_running);
        assert!(token.lock().unwrap().as_ref().unwrap().is_cancelled());
    }
}

#[tokio::test]
async fn duplicate_tool_ids_are_rejected_before_execution() {
    let mut message = tool_response();
    message.content.push(message.content[0].clone());
    let (agent, calls) = with_add(Scripted::from(vec![message]));
    let result = agent.prompt(Message::user("start")).await.unwrap();
    assert_eq!(result.last_message.stop_reason, StopReason::Error);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tool_calls_with_stop_or_reused_ids_are_never_executed() {
    let mut stopped = tool_response();
    stopped.stop_reason = StopReason::Stop;
    let (agent, calls) = with_add(Scripted::from(vec![stopped]));
    assert_eq!(
        agent
            .prompt(Message::user("start"))
            .await
            .unwrap()
            .last_message
            .stop_reason,
        StopReason::Error
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let provider = Scripted::from(vec![tool_response(), tool_response()]);
    let (agent, calls) = with_add(provider);
    assert_eq!(
        agent
            .prompt(Message::user("start"))
            .await
            .unwrap()
            .last_message
            .stop_reason,
        StopReason::Error
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn listener_failures_release_agent_and_reach_other_listeners() {
    let agent = Agent::new(Scripted::from(vec![response("unused")]), model());
    agent.subscribe(|_, _| async { Err("storage unavailable".into()) });
    let ends = Arc::new(AtomicUsize::new(0));
    let copy = ends.clone();
    agent.subscribe(move |event, _| {
        let copy = copy.clone();
        async move {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                copy.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    });
    assert_eq!(
        agent.prompt(Message::user("start")).await.unwrap_err(),
        AgentError::Listener("storage unavailable".into())
    );
    assert_eq!(ends.load(Ordering::SeqCst), 1);
    assert!(!agent.snapshot().is_running);
}

#[tokio::test]
async fn per_run_turn_budget_stops_unbounded_continuation() {
    struct KeepGoing;
    #[async_trait]
    impl AgentHooks for KeepGoing {
        async fn finish_turn(
            &self,
            _: TurnContext,
            _: CancellationToken,
        ) -> Result<TurnDecision, String> {
            Ok(TurnDecision::Continue)
        }
    }
    let provider = Scripted::from(vec![response("one"), response("two")]);
    let agent = Agent::new(provider.clone(), model());
    agent.set_max_turns(2).unwrap();
    agent.set_hooks(Arc::new(KeepGoing)).unwrap();
    let result = agent.prompt(Message::user("start")).await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(result.last_message.stop_reason, StopReason::Error);
    assert!(
        result
            .last_message
            .error_message
            .unwrap()
            .contains("max_turns")
    );
}

#[tokio::test]
async fn host_must_reconcile_unknown_results_before_continuing() {
    let agent = Agent::new(Scripted::from(vec![]), model());
    agent
        .replace_messages(vec![Message::ToolResult(ToolResultMessage {
            tool_call_id: "unknown".into(),
            tool_name: "write".into(),
            content: vec![],
            details: None,
            structured_content: None,
            usage: None,
            is_error: true,
            outcome_unknown: true,
            timestamp: 0,
            duration_ms: None,
        })])
        .unwrap();
    assert!(agent.snapshot().needs_reconciliation);
    assert_eq!(
        agent.prompt(Message::user("retry")).await.unwrap_err(),
        AgentError::UnresolvedToolEffects
    );
}
