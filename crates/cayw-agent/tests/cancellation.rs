use cayw_agent::{ai::*, *};
use futures::{StreamExt, poll, stream};
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct ProviderFixture(AssistantMessage);
impl Provider for ProviderFixture {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        stream::iter([AssistantEvent::Done {
            message: self.0.clone(),
        }])
        .boxed()
    }
}
#[derive(Clone, Copy)]
enum Reply {
    Success,
    Failure,
    Unknown,
    Pending,
    Panic,
}
struct CleanupTool {
    started: Notify,
    cancelling: Notify,
    release: Notify,
    calls: AtomicUsize,
    cancelled: AtomicBool,
    settled: AtomicBool,
    dropped: AtomicBool,
    reply: Reply,
    abort_on_start: Mutex<Option<AgentControl>>,
    grace: Duration,
}
impl CleanupTool {
    fn new(reply: Reply) -> Self {
        Self {
            started: Notify::new(),
            cancelling: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            reply,
            abort_on_start: Mutex::new(None),
            grace: Duration::from_secs(1),
        }
    }
}
struct DropCheck<'a>(&'a CleanupTool);
impl Drop for DropCheck<'_> {
    fn drop(&mut self) {
        if !self.0.settled.load(Ordering::SeqCst) {
            self.0.dropped.store(true, Ordering::SeqCst);
        }
    }
}
#[async_trait]
impl AgentTool for CleanupTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "cleanup".into(),
            description: String::new(),
            parameters: json!({"type":"object"}),
        }
    }
    fn shutdown_grace(&self) -> Duration {
        self.grace
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        let _drop = DropCheck(self);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        assert!(
            context
                .progress
                .emit(ToolResult::text("accepted before cancel"))
                .await
        );
        if let Some(control) = self.abort_on_start.lock().unwrap().as_ref() {
            control.abort();
        }
        context.cancel.cancelled().await;
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancelling.notify_one();
        if matches!(self.reply, Reply::Pending) {
            return std::future::pending().await;
        }
        self.release.notified().await;
        assert!(
            context
                .progress
                .emit(ToolResult::text("accepted during cleanup"))
                .await
        );
        if matches!(self.reply, Reply::Panic) {
            panic!("cleanup panic");
        }
        self.settled.store(true, Ordering::SeqCst);
        match self.reply {
            Reply::Success => Ok(ToolResult {
                structured_content: Some(json!({"committed":true})),
                ..ToolResult::text("committed")
            }),
            Reply::Failure => Err("known cleanup failure".into()),
            Reply::Unknown => Ok(ToolResult {
                outcome_unknown: true,
                ..ToolResult::error("unconfirmed remote effect")
            }),
            Reply::Pending | Reply::Panic => unreachable!(),
        }
    }
}
fn agent(tool: Arc<dyn AgentTool>) -> Agent {
    let model = Model::new("fixture", "offline");
    let mut message = AssistantMessage::new(&model);
    message.stop_reason = StopReason::ToolUse;
    message.content = ["first", "later"]
        .into_iter()
        .map(|id| Content::ToolCall(ToolCall::new(id, "cleanup", json!({}))))
        .collect();
    let agent = Agent::new(Arc::new(ProviderFixture(message)), model);
    agent.set_tools(vec![tool]).unwrap();
    agent
}
fn results(run: &RunResult) -> Vec<&ToolResultMessage> {
    run.messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

// Break caught: abort releases the running slot and emits an unknown result
// before a cooperative tool can complete cleanup and report its true outcome.
#[tokio::test]
async fn cooperative_tool_cleanup_finishes_before_result_and_idle() {
    for mode in [ExecutionMode::Sequential, ExecutionMode::Parallel] {
        for reply in [Reply::Success, Reply::Failure, Reply::Unknown, Reply::Panic] {
            tokio::time::timeout(Duration::from_secs(10), async {
                let tool = Arc::new(CleanupTool::new(reply));
                let agent = agent(tool.clone());
                agent.set_execution_mode(mode).unwrap();
                // Cancel synchronously from the first execute poll. Even in
                // parallel mode the next prepared tool must remain unstarted.
                *tool.abort_on_start.lock().unwrap() = Some(agent.control());
                let recorded = Arc::new(Mutex::new(Vec::new()));
                let events = recorded.clone();
                agent.subscribe(move |event, _| {
                    let events = events.clone();
                    async move {
                        events.lock().unwrap().push(event);
                        Ok(())
                    }
                });
                let mut run = Box::pin(agent.prompt(Message::user("clean up on cancel")));
                tokio::select! {
                    biased;
                    result = &mut run => panic!("returned before cleanup observed cancel: {result:?}"),
                    () = tool.cancelling.notified() => {},
                }
                assert!(agent.snapshot().is_running);
                assert!(!tool.settled.load(Ordering::SeqCst));
                assert_eq!(agent.reset(), Err(AgentError::Busy));
                assert!(poll!(&mut run).is_pending());
                tool.release.notify_one();
                let run = run.await.unwrap();
                assert_eq!(run.last_message.stop_reason, StopReason::Aborted);
                let results = results(&run);
                assert_eq!(results.len(), 2);
                assert_eq!(results[0].tool_call_id, "first");
                assert_eq!(results[1].tool_call_id, "later");
                assert!(!results[1].outcome_unknown);
                assert!(results[1].is_error);
                assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
                let unknown = matches!(reply, Reply::Unknown | Reply::Panic);
                assert_eq!(results[0].outcome_unknown, unknown);
                assert_eq!(agent.snapshot().needs_reconciliation, unknown);
                assert!(!agent.snapshot().is_running);
                if matches!(reply, Reply::Success) {
                    assert!(!results[0].is_error);
                    assert_eq!(results[0].structured_content, Some(json!({"committed":true})));
                }
                if !matches!(reply, Reply::Panic) {
                    assert!(tool.settled.load(Ordering::SeqCst));
                    assert!(!tool.dropped.load(Ordering::SeqCst));
                }
                let events = recorded.lock().unwrap();
                let first_end = events.iter().position(|event| matches!(event, AgentEvent::ToolExecutionEnd { call, .. } if call.id == "first")).unwrap();
                let progress = events.iter().enumerate().filter_map(|(i, event)| matches!(event, AgentEvent::ToolExecutionUpdate { call, .. } if call.id == "first").then_some(i)).collect::<Vec<_>>();
                assert_eq!(progress.len(), 2);
                assert!(progress.iter().all(|index| *index < first_end));
            }).await.expect("cooperative cleanup must finish");
        }
    }
}

#[tokio::test]
async fn permanently_pending_tool_is_polled_for_cleanup_then_returns_unknown() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut fixture = CleanupTool::new(Reply::Pending);
        fixture.grace = Duration::from_millis(25);
        let tool = Arc::new(fixture);
        let agent = agent(tool.clone());
        agent.set_execution_mode(ExecutionMode::Sequential).unwrap();
        let (run, ()) = tokio::join!(agent.prompt(Message::user("cancel")), async {
            tool.started.notified().await;
            agent.control().abort();
        });
        let run = run.unwrap();
        assert!(tool.cancelled.load(Ordering::SeqCst));
        assert!(tool.dropped.load(Ordering::SeqCst));
        assert!(!tool.settled.load(Ordering::SeqCst));
        let results = results(&run);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_error && results[0].outcome_unknown);
        assert!(!results[1].outcome_unknown);
        assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
        assert!(agent.snapshot().needs_reconciliation);
    })
    .await
    .expect("uncooperative tool must not wait forever");
}

#[tokio::test]
async fn cancellation_before_tool_first_poll_starts_zero_effects() {
    let tool = Arc::new(CleanupTool::new(Reply::Success));
    let agent = agent(tool.clone());
    let control = agent.control();
    agent.subscribe(move |event, _| {
        let control = control.clone();
        async move {
            if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                control.abort();
            }
            Ok(())
        }
    });
    let run = agent
        .prompt(Message::user("cancel before dispatch"))
        .await
        .unwrap();
    assert_eq!(run.last_message.stop_reason, StopReason::Aborted);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(results(&run).iter().all(|result| !result.outcome_unknown));
    assert!(!agent.snapshot().needs_reconciliation);
}

#[tokio::test]
async fn cancellation_race_preserves_a_ready_committed_tool_result() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let tool = Arc::new(CleanupTool::new(Reply::Success));
        let agent = agent(tool.clone());
        agent.set_execution_mode(ExecutionMode::Sequential).unwrap();
        let mut run = Box::pin(agent.prompt(Message::user("cancel and finish")));
        tokio::select! {
            result = &mut run => panic!("returned before tool dispatch: {result:?}"),
            () = tool.started.notified() => {},
        }
        tool.release.notify_one();
        agent.control().abort();
        let run = run.await.unwrap();
        let results = results(&run);
        assert!(!results[0].is_error && !results[0].outcome_unknown);
        assert_eq!(
            results[0].structured_content,
            Some(json!({"committed":true}))
        );
        assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
        assert!(!agent.snapshot().needs_reconciliation);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dropping_agent_during_cleanup_fences_unsettled_effects_without_detaching() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let tool = Arc::new(CleanupTool::new(Reply::Success));
        let agent = agent(tool.clone());
        agent.set_execution_mode(ExecutionMode::Sequential).unwrap();
        let mut run = Box::pin(agent.prompt(Message::user("drop during cleanup")));
        tokio::select! {
            result = &mut run => panic!("returned before tool dispatch: {result:?}"),
            () = tool.started.notified() => {},
        }
        agent.control().abort();
        tokio::select! {
            biased;
            result = &mut run => panic!("returned before cleanup observed cancel: {result:?}"),
            () = tool.cancelling.notified() => {},
        }
        drop(run);
        assert!(tool.cancelled.load(Ordering::SeqCst));
        assert!(tool.dropped.load(Ordering::SeqCst));
        assert!(!tool.settled.load(Ordering::SeqCst));
        assert!(!agent.snapshot().is_running);
        assert!(agent.snapshot().needs_reconciliation);
        assert_eq!(agent.reset(), Err(AgentError::UnresolvedToolEffects));
        tool.release.notify_one();
        tokio::task::yield_now().await;
        assert!(!tool.settled.load(Ordering::SeqCst));
    })
    .await
    .unwrap();
}

#[test]
fn invalid_tool_shutdown_grace_is_rejected_before_registration() {
    for grace in [Duration::ZERO, Duration::MAX] {
        let mut invalid = CleanupTool::new(Reply::Success);
        invalid.grace = grace;
        let model = Model::new("fixture", "offline");
        let agent = Agent::new(
            Arc::new(ProviderFixture(AssistantMessage::new(&model))),
            model,
        );
        assert!(matches!(
            agent.set_tools(vec![Arc::new(invalid)]),
            Err(AgentError::InvalidConfiguration(_))
        ));
    }
}
