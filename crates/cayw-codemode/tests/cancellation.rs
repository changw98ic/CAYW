use async_trait::async_trait;
use cayw_agent::{Agent, AgentTool};
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, ToolDefinition,
};
use cayw_codemode::{
    CallStatus, CapabilityError, CapabilityHost, CodeMode, CodeModeTool, ErrorKind, Limits,
};
use futures::{StreamExt, poll, stream};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum Reply {
    Success,
    Failure,
    Unknown,
    Pending,
}

struct Host {
    started: Notify,
    cancelling: Notify,
    release: Notify,
    calls: AtomicUsize,
    observed_cancel: AtomicBool,
    settled: AtomicBool,
    dropped: AtomicBool,
    reply: Reply,
}
impl Host {
    fn new(reply: Reply) -> Self {
        Self {
            started: Notify::new(),
            cancelling: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
            observed_cancel: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            reply,
        }
    }
}
struct DropCheck<'a>(&'a Host);
impl Drop for DropCheck<'_> {
    fn drop(&mut self) {
        if !self.0.settled.load(Ordering::SeqCst) {
            self.0.dropped.store(true, Ordering::SeqCst);
        }
    }
}
#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "work".into(),
            description: String::new(),
            parameters: json!({"type":"object"}),
        }]
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let _drop = DropCheck(self);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        cancel.cancelled().await;
        self.observed_cancel.store(true, Ordering::SeqCst);
        self.cancelling.notify_one();
        if matches!(self.reply, Reply::Pending) {
            return std::future::pending().await;
        }
        self.release.notified().await;
        self.settled.store(true, Ordering::SeqCst);
        match self.reply {
            Reply::Success => Ok(json!({"committed":true})),
            Reply::Failure => Err(CapabilityError::new("known stopped operation")),
            Reply::Unknown => Err(CapabilityError::unknown("interrupted remote outcome")),
            Reply::Pending => unreachable!(),
        }
    }
}

// Break caught: cancelling the VM or reaching the Host wait limit drops a started
// Host before its asynchronous cleanup and durable settlement can return, losing
// its actual known/unknown result.
#[tokio::test]
async fn cancellation_or_host_wait_settles_the_same_host_before_return_and_starts_no_next_call() {
    for timed_out in [false, true] {
        for (reply, status, unknown) in [
            (Reply::Success, CallStatus::Succeeded, false),
            (Reply::Failure, CallStatus::Failed, false),
            (Reply::Unknown, CallStatus::OutcomeUnknown, true),
        ] {
            tokio::time::timeout(Duration::from_secs(10), async {
                let host = Arc::new(Host::new(reply));
                let mode = CodeMode::new(
                    host.clone(),
                    Limits {
                        // A VM timeout only bounds computation, so the started Host
                        // needs an explicit short host_wait to reach its own limit.
                        timeout: if timed_out { Duration::from_millis(50) } else { Duration::from_secs(5) },
                        host_wait: if timed_out { Duration::from_millis(300) } else { Duration::from_secs(60) },
                        shutdown_grace: Duration::from_secs(1),
                        ..Limits::default()
                    },
                ).unwrap();
                let cancel = CancellationToken::new();
                let mut execution = Box::pin(mode.execute(
                    "cayw.emit('accepted'); pcall(function() xpcall(function() cayw.call('work', {}) end, function() return 'handled' end) end); cayw.call('work', {})",
                    cancel.clone(),
                ));
                tokio::select! {
                    result = &mut execution => panic!("returned before Host started: {result:?}"),
                    () = host.started.notified() => {},
                }
                if !timed_out { cancel.cancel(); }
                tokio::select! {
                    biased;
                    result = &mut execution => panic!("returned before Host observed cancellation: {result:?}"),
                    () = host.cancelling.notified() => {},
                }
                assert!(poll!(&mut execution).is_pending(), "cleanup has not been released");
                host.release.notify_one();
                let error = execution.await.unwrap_err();
                assert_eq!(error.kind, if timed_out { ErrorKind::Timeout } else { ErrorKind::Cancelled });
                assert!(host.settled.load(Ordering::SeqCst));
                assert!(!host.dropped.load(Ordering::SeqCst));
                assert_eq!(host.calls.load(Ordering::SeqCst), 1);
                assert_eq!(error.report.calls.len(), 1);
                assert_eq!(error.report.calls[0].status, status);
                assert_eq!(error.report.outcome_unknown, unknown);
                assert_eq!(error.report.outputs, [json!("accepted")]);
                match reply {
                    Reply::Success => assert_eq!(error.report.calls[0].result, Some(json!({"committed":true}))),
                    Reply::Failure => assert_eq!(error.report.calls[0].error.as_deref(), Some("known stopped operation")),
                    Reply::Unknown => assert_eq!(error.report.calls[0].error.as_deref(), Some("interrupted remote outcome")),
                    Reply::Pending => unreachable!(),
                }
            }).await.expect("cooperative Host settlement must finish");
        }
    }
}

// Break caught: returning immediately on cancellation or the Host wait limit never
// even polls cleanup; waiting forever instead never returns the explicit unknown result.
#[tokio::test]
async fn permanently_pending_host_is_bounded_by_host_wait_then_explicit_unknown() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for timed_out in [false, true] {
            let host = Arc::new(Host::new(Reply::Pending));
            let mode = CodeMode::new(
                host.clone(),
                Limits {
                    // A VM timeout only bounds computation, so the started Host
                    // needs an explicit short host_wait to reach its own limit.
                    timeout: if timed_out {
                        Duration::from_millis(50)
                    } else {
                        Duration::from_secs(5)
                    },
                    host_wait: if timed_out {
                        Duration::from_millis(300)
                    } else {
                        Duration::from_secs(60)
                    },
                    shutdown_grace: Duration::from_millis(25),
                    ..Limits::default()
                },
            )
            .unwrap();
            let cancel = CancellationToken::new();
            let mut execution = Box::pin(mode.execute("cayw.call('work', {})", cancel.clone()));
            tokio::select! {
                result = &mut execution => panic!("returned before dispatch: {result:?}"),
                () = host.started.notified() => {},
            }
            if !timed_out {
                cancel.cancel();
            }
            let error = execution.await.unwrap_err();
            assert_eq!(
                error.kind,
                if timed_out {
                    ErrorKind::Timeout
                } else {
                    ErrorKind::Cancelled
                }
            );
            assert!(host.observed_cancel.load(Ordering::SeqCst));
            assert!(!host.settled.load(Ordering::SeqCst));
            assert!(host.dropped.load(Ordering::SeqCst));
            assert!(error.report.outcome_unknown);
            assert_eq!(error.report.calls[0].status, CallStatus::OutcomeUnknown);
        }
    })
    .await
    .expect("uncooperative Host must be bounded");
}

#[tokio::test]
async fn cancellation_before_first_poll_has_zero_host_effects() {
    let host = Arc::new(Host::new(Reply::Success));
    let mode = CodeMode::new(host.clone(), Limits::default()).unwrap();
    let cancel = CancellationToken::new();
    let execution = mode.execute("cayw.call('work', {})", cancel.clone());
    cancel.cancel();
    let error = execution.await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(error.report.calls.is_empty());
    assert!(!error.report.outcome_unknown);
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
}

// Break caught: cancellation and an already-started successful Host become ready
// together; selecting cancellation discards a committed result as unknown.
#[tokio::test]
async fn cancellation_race_keeps_a_ready_known_host_result() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let host = Arc::new(Host::new(Reply::Success));
        let mode = CodeMode::new(host.clone(), Limits::default()).unwrap();
        let cancel = CancellationToken::new();
        let mut execution = Box::pin(mode.execute(
            "cayw.call('work', {}); cayw.call('work', {})",
            cancel.clone(),
        ));
        tokio::select! {
            result = &mut execution => panic!("returned before dispatch: {result:?}"),
            () = host.started.notified() => {},
        }
        host.release.notify_one();
        cancel.cancel();
        let error = execution.await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
        assert_eq!(
            error.report.calls[0].result,
            Some(json!({"committed":true}))
        );
        assert!(!error.report.outcome_unknown);
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert!(host.settled.load(Ordering::SeqCst));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dropping_outer_execution_during_cleanup_cancels_without_awaiting_settlement() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let host = Arc::new(Host::new(Reply::Success));
        let mode = CodeMode::new(host.clone(), Limits::default()).unwrap();
        let parent = CancellationToken::new();
        let mut execution = Box::pin(mode.execute("cayw.call('work', {})", parent.clone()));
        tokio::select! {
            result = &mut execution => panic!("returned before dispatch: {result:?}"),
            () = host.started.notified() => {},
        }
        parent.cancel();
        tokio::select! {
            biased;
            result = &mut execution => panic!("returned before cleanup: {result:?}"),
            () = host.cancelling.notified() => {},
        }
        drop(execution);
        assert!(host.observed_cancel.load(Ordering::SeqCst));
        assert!(host.dropped.load(Ordering::SeqCst));
        assert!(!host.settled.load(Ordering::SeqCst));
        // There is no detached coroutine that could finish when released later.
        host.release.notify_one();
        tokio::task::yield_now().await;
        assert!(!host.settled.load(Ordering::SeqCst));
    })
    .await
    .unwrap();
}

// Break caught: zero/overflowing cleanup budgets remove the bound, and a nested
// CodeMode grace longer than its Agent budget lets Agent drop it before settlement.
#[test]
fn shutdown_grace_is_validated_and_agent_grace_covers_inner_cleanup() {
    for shutdown_grace in [Duration::ZERO, Duration::MAX] {
        let result = CodeMode::new(
            Arc::new(Host::new(Reply::Success)),
            Limits {
                shutdown_grace,
                ..Limits::default()
            },
        );
        assert!(matches!(result, Err(error) if error.kind == ErrorKind::InvalidLimits));
    }
    let inner_grace = Duration::from_secs(8);
    let mode = CodeMode::new(
        Arc::new(Host::new(Reply::Success)),
        Limits {
            shutdown_grace: inner_grace,
            ..Limits::default()
        },
    )
    .unwrap();
    let tool = CodeModeTool::new(mode);
    assert!(tool.shutdown_grace() > inner_grace);
}

struct ProviderFixture(AssistantMessage);
impl Provider for ProviderFixture {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        stream::iter([AssistantEvent::Done {
            message: self.0.clone(),
        }])
        .boxed()
    }
}

// Break caught: Agent's cancellation select drops CodeMode before Host cleanup
// and discards its structured audit even though CodeMode can settle cooperatively.
#[tokio::test]
async fn agent_abort_keeps_codemode_running_until_host_settlement() {
    for (reply, status, unknown) in [
        (Reply::Success, "succeeded", false),
        (Reply::Failure, "failed", false),
        (Reply::Unknown, "outcome_unknown", true),
    ] {
        tokio::time::timeout(Duration::from_secs(10), async {
            let host = Arc::new(Host::new(reply));
            let mode = CodeMode::new(
                host.clone(),
                Limits {
                    shutdown_grace: Duration::from_secs(1),
                    ..Limits::default()
                },
            )
            .unwrap();
            let model = Model::new("fixture", "offline");
            let mut response = AssistantMessage::new(&model);
            response.stop_reason = StopReason::ToolUse;
            response.content = ["code-first", "code-later"]
                .into_iter()
                .map(|id| {
                    Content::ToolCall(ToolCall::new(
                        id,
                        "execute_code",
                        json!({"script":"cayw.call('work', {}); cayw.call('work', {})"}),
                    ))
                })
                .collect();
            let agent = Agent::new(Arc::new(ProviderFixture(response)), model);
            agent
                .set_tools(vec![Arc::new(CodeModeTool::new(mode))])
                .unwrap();
            let mut run = Box::pin(agent.prompt(Message::user("cancel after dispatch")));
            tokio::select! {
                result = &mut run => panic!("returned before dispatch: {result:?}"),
                () = host.started.notified() => {},
            }
            agent.control().abort();
            tokio::select! {
                biased;
                result = &mut run => panic!("returned before Host cancellation: {result:?}"),
                () = host.cancelling.notified() => {},
            }
            assert!(agent.snapshot().is_running);
            assert!(agent.snapshot().pending_tool_calls.contains("code-first"));
            assert!(poll!(&mut run).is_pending());
            host.release.notify_one();
            let run = run.await.unwrap();
            assert_eq!(run.last_message.stop_reason, StopReason::Aborted);
            let results = run
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::ToolResult(result) => Some(result),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(results.len(), 2);
            let result = results[0];
            assert_eq!(result.tool_call_id, "code-first");
            assert_eq!(result.outcome_unknown, unknown);
            let audit = result
                .structured_content
                .as_ref()
                .expect("CodeMode report survives abort");
            assert_eq!(audit["error"]["kind"], "cancelled");
            assert_eq!(audit["report"]["outcome_unknown"], unknown);
            assert_eq!(audit["report"]["calls"][0]["status"], status);
            assert_eq!(audit["report"]["calls"].as_array().unwrap().len(), 1);
            assert!(!results[1].outcome_unknown);
            assert_eq!(agent.snapshot().needs_reconciliation, unknown);
            assert!(!agent.snapshot().is_running);
            assert!(host.settled.load(Ordering::SeqCst));
            assert!(!host.dropped.load(Ordering::SeqCst));
            assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        })
        .await
        .expect("Agent must preserve CodeMode settlement");
    }
}
