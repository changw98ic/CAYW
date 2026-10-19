use crate::{
    AgentContext, AgentEvent, AgentHooks, AgentTool, ExecutionMode, ToolContext, ToolHookContext,
    ToolProgress, ToolResult,
};
use cayw_ai::{
    ArgumentValidator, AssistantMessage, CancellationToken, StopReason, ToolCall, ToolDefinition,
};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub(crate) type EventSink = Arc<dyn Fn(AgentEvent) -> BoxFuture<'static, ()> + Send + Sync>;

pub(crate) struct ToolOutcome {
    pub call: ToolCall,
    pub result: ToolResult,
    pub duration_ms: Option<u64>,
}

struct RegisteredTool {
    tool: Arc<dyn AgentTool>,
    definition: ToolDefinition,
    mode: Option<ExecutionMode>,
    shutdown_grace: Duration,
}

struct Invocation {
    cancel: CancellationToken,
    active: Arc<AtomicBool>,
}

impl Drop for Invocation {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.cancel.cancel();
    }
}

struct PreparedCall {
    call: ToolCall,
    tool: Option<Arc<dyn AgentTool>>,
    hook_context: Option<ToolHookContext>,
    result: Option<ToolResult>,
    shutdown_grace: Duration,
    invocation: Invocation,
}

pub(crate) async fn execute_batch(
    assistant: &AssistantMessage,
    context: &AgentContext,
    mode: ExecutionMode,
    hooks: Arc<dyn AgentHooks>,
    cancel: CancellationToken,
    emit: EventSink,
) -> Vec<ToolOutcome> {
    let calls = assistant.tool_calls();
    if calls.is_empty() {
        return Vec::new();
    }

    let mut ids = BTreeSet::new();
    let batch_error = if calls
        .iter()
        .any(|call| call.id.trim().is_empty() || !ids.insert(call.id.clone()))
    {
        Some("Tool call IDs must be nonempty and unique within a batch".to_owned())
    } else if assistant.stop_reason == StopReason::Length {
        Some("Model output was truncated; tool calls were not executed".to_owned())
    } else if assistant.stop_reason.is_failure() || assistant.stop_reason == StopReason::Pending {
        Some(
            "The assistant response did not complete successfully; tool calls were not executed"
                .to_owned(),
        )
    } else {
        None
    };
    let registry = if batch_error.is_none() {
        build_registry(context)
    } else {
        Ok(BTreeMap::new())
    };
    let batch_error = batch_error.or_else(|| registry.as_ref().err().cloned());
    let registry = registry.unwrap_or_default();
    let sequential = mode == ExecutionMode::Sequential
        || calls.iter().any(|call| {
            registry
                .get(&call.name)
                .is_some_and(|tool| tool.mode == Some(ExecutionMode::Sequential))
        });

    if sequential {
        let mut outcomes = Vec::with_capacity(calls.len());
        for call in calls {
            let prepared = prepare_call(
                call,
                assistant,
                context,
                &registry,
                batch_error.as_deref(),
                &hooks,
                &cancel,
                &emit,
            )
            .await;
            outcomes.push(finish_call(prepared, hooks.clone(), emit.clone()).await);
        }
        return outcomes;
    }

    // Preflight is deliberately ordered. No executable future is polled until all
    // arguments and before hooks in this batch have been checked.
    let mut prepared = Vec::with_capacity(calls.len());
    for call in calls {
        prepared.push(
            prepare_call(
                call,
                assistant,
                context,
                &registry,
                batch_error.as_deref(),
                &hooks,
                &cancel,
                &emit,
            )
            .await,
        );
    }
    let count = prepared.len();
    let mut pending = FuturesUnordered::new();
    for (index, prepared) in prepared.into_iter().enumerate() {
        let hooks = hooks.clone();
        let emit = emit.clone();
        pending.push(async move { (index, finish_call(prepared, hooks, emit).await) });
    }
    let mut ordered = (0..count).map(|_| None).collect::<Vec<_>>();
    while let Some((index, outcome)) = pending.next().await {
        ordered[index] = Some(outcome);
    }
    ordered.into_iter().flatten().collect()
}

fn build_registry(context: &AgentContext) -> Result<BTreeMap<String, RegisteredTool>, String> {
    let mut registry = BTreeMap::new();
    for tool in &context.tools {
        let (definition, mode, shutdown_grace) = catch_sync(|| {
            (
                tool.definition(),
                tool.execution_mode(),
                tool.shutdown_grace(),
            )
        })
        .map_err(|error| format!("Tool registration panicked: {error}"))?;
        crate::tool::validate_shutdown_grace(shutdown_grace)?;
        if definition.name.trim().is_empty() || registry.contains_key(&definition.name) {
            return Err("Registered tool names must be nonempty and unique".into());
        }
        registry.insert(
            definition.name.clone(),
            RegisteredTool {
                tool: tool.clone(),
                definition,
                mode,
                shutdown_grace,
            },
        );
    }
    Ok(registry)
}

#[allow(clippy::too_many_arguments)]
async fn prepare_call(
    call: ToolCall,
    assistant: &AssistantMessage,
    context: &AgentContext,
    registry: &BTreeMap<String, RegisteredTool>,
    batch_error: Option<&str>,
    hooks: &Arc<dyn AgentHooks>,
    cancel: &CancellationToken,
    emit: &EventSink,
) -> PreparedCall {
    let mut prepared = PreparedCall {
        call,
        tool: None,
        hook_context: None,
        result: None,
        shutdown_grace: Duration::ZERO,
        invocation: Invocation {
            cancel: cancel.child_token(),
            active: Arc::new(AtomicBool::new(false)),
        },
    };
    emit(AgentEvent::ToolExecutionStart {
        call: prepared.call.clone(),
    })
    .await;
    if let Some(error) = batch_error {
        prepared.result = Some(ToolResult::error(error));
        return prepared;
    }
    if prepared.invocation.cancel.is_cancelled() {
        prepared.result = Some(ToolResult::cancelled(false));
        return prepared;
    }
    if let Some(error) = &prepared.call.arguments_error {
        prepared.result = Some(ToolResult::error(format!(
            "Invalid tool arguments: {error}"
        )));
        return prepared;
    }
    let Some(registered) = registry.get(&prepared.call.name) else {
        prepared.result = Some(ToolResult::error(format!(
            "Unknown tool: {}",
            prepared.call.name
        )));
        return prepared;
    };
    let arguments = catch_sync(|| {
        if !prepared.call.arguments.is_object() {
            return Err("Tool arguments must be a JSON object".to_owned());
        }
        let arguments = registered
            .tool
            .prepare_arguments(prepared.call.arguments.clone())?;
        ArgumentValidator::new(&registered.definition.parameters)
            .and_then(|validator| validator.validate(&arguments))
            .map_err(|error| error.to_string())?;
        Ok(arguments)
    });
    let arguments = match arguments {
        Ok(Ok(arguments)) => arguments,
        Ok(Err(error)) | Err(error) => {
            prepared.result = Some(ToolResult::error(format!(
                "Tool argument preparation failed: {error}"
            )));
            return prepared;
        }
    };
    let hook_context = ToolHookContext {
        assistant: assistant.clone(),
        call: prepared.call.clone(),
        arguments,
        context: context.clone(),
    };
    let before = AssertUnwindSafe(async {
        hooks
            .before_tool_call(hook_context.clone(), prepared.invocation.cancel.clone())
            .await
    })
    .catch_unwind();
    let decision = tokio::select! {
        biased;
        _ = prepared.invocation.cancel.cancelled() => {
            prepared.result = Some(ToolResult::cancelled(false));
            return prepared;
        }
        decision = before => decision,
    };
    match decision {
        Ok(Ok(decision)) => {
            if decision.block {
                prepared.result = Some(ToolResult {
                    terminate: decision.terminate,
                    ..ToolResult::error(
                        decision
                            .reason
                            .unwrap_or_else(|| "Tool call blocked by before_tool_call".into()),
                    )
                });
            } else {
                prepared.tool = Some(registered.tool.clone());
                prepared.shutdown_grace = registered.shutdown_grace;
            }
            prepared.hook_context = Some(hook_context);
        }
        Ok(Err(error)) => {
            prepared.result = Some(ToolResult::error(format!(
                "before_tool_call failed: {error}"
            )))
        }
        Err(error) => {
            prepared.result = Some(ToolResult::error(format!(
                "before_tool_call panicked: {}",
                panic_message(error)
            )))
        }
    }
    prepared
}

async fn finish_call(
    mut prepared: PreparedCall,
    hooks: Arc<dyn AgentHooks>,
    emit: EventSink,
) -> ToolOutcome {
    let (mut result, duration_ms) = if let Some(result) = prepared.result.take() {
        (result, None)
    } else if prepared.invocation.cancel.is_cancelled() {
        (ToolResult::cancelled(false), None)
    } else {
        execute_call(&prepared, &emit).await
    };
    normalize_content(&mut result);
    if let Some(context) = prepared.hook_context.take()
        && !prepared.invocation.cancel.is_cancelled()
    {
        let after = AssertUnwindSafe(async {
            hooks
                .after_tool_call(context, result.clone(), prepared.invocation.cancel.clone())
                .await
        })
        .catch_unwind();
        let patch = tokio::select! {
            biased;
            _ = prepared.invocation.cancel.cancelled() => None,
            patch = after => Some(patch),
        };
        match patch {
            Some(Ok(Ok(patch))) => patch.apply(&mut result),
            Some(Ok(Err(error))) => {
                replace_with_error(&mut result, format!("after_tool_call failed: {error}"))
            }
            Some(Err(error)) => replace_with_error(
                &mut result,
                format!("after_tool_call panicked: {}", panic_message(error)),
            ),
            None => {}
        }
    }
    normalize_content(&mut result);
    if result.outcome_unknown {
        result.is_error = true;
    }
    emit(AgentEvent::ToolExecutionEnd {
        call: prepared.call.clone(),
        result: result.clone(),
        duration_ms,
    })
    .await;
    ToolOutcome {
        call: prepared.call,
        result,
        duration_ms,
    }
}

async fn execute_call(prepared: &PreparedCall, emit: &EventSink) -> (ToolResult, Option<u64>) {
    let Some(tool) = &prepared.tool else {
        return (
            ToolResult::error("Tool was not prepared for execution"),
            None,
        );
    };
    let Some(hook_context) = &prepared.hook_context else {
        return (ToolResult::error("Tool arguments were not prepared"), None);
    };
    let mut started = None;
    let (sender, mut receiver) = mpsc::channel(16);
    let execution_cancel = prepared.invocation.cancel.child_token();
    let execution_guard = execution_cancel.clone().drop_guard();
    let tool_context = ToolContext {
        call_id: prepared.call.id.clone(),
        arguments: hook_context.arguments.clone(),
        cancel: execution_cancel.clone(),
        progress: ToolProgress {
            sender,
            active: prepared.invocation.active.clone(),
        },
    };
    let mut result = {
        // The async wrapper catches creation and poll panics, including cleanup.
        // Its first poll checks cancellation again before calling any tool code.
        let execution = AssertUnwindSafe(async {
            if prepared.invocation.cancel.is_cancelled() {
                return Ok(ToolResult::cancelled(false));
            }
            started = Some(Instant::now());
            prepared.invocation.active.store(true, Ordering::Release);
            tool.execute(tool_context).await
        })
        .catch_unwind();
        tokio::pin!(execution);
        let mut progress_open = true;
        let mut shutdown_deadline = None;
        loop {
            tokio::select! {
                biased;
                _ = prepared.invocation.cancel.cancelled(), if shutdown_deadline.is_none() => {
                    if !prepared.invocation.active.load(Ordering::Acquire) {
                        break ToolResult::cancelled(false);
                    }
                    execution_cancel.cancel();
                    shutdown_deadline = Some(tokio::time::Instant::now() + prepared.shutdown_grace);
                }
                result = &mut execution => break match result {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => ToolResult::error(error),
                    Err(error) => ToolResult {
                        outcome_unknown: true,
                        ..ToolResult::error(format!("Tool execution panicked: {}", panic_message(error)))
                    },
                },
                _ = async { tokio::time::sleep_until(shutdown_deadline.expect("cleanup deadline")).await }, if shutdown_deadline.is_some() => {
                    break ToolResult {
                        outcome_unknown: true,
                        ..ToolResult::error("Tool cleanup grace expired; its effects may have occurred. Reconcile before retrying.")
                    };
                }
                update = receiver.recv(), if progress_open => match update {
                    Some(mut result) => {
                        normalize_content(&mut result);
                        emit(AgentEvent::ToolExecutionUpdate { call: prepared.call.clone(), result }).await;
                    }
                    None => progress_open = false,
                },
            }
        }
    };
    let duration_ms =
        started.map(|started| started.elapsed().as_millis().min(u64::MAX as u128) as u64);
    prepared.invocation.active.store(false, Ordering::Release);
    receiver.close();
    drop(execution_guard);
    // Closing rejects new sends but retains accepted updates. Detached progress
    // handles cannot keep the receiver open after it has been closed.
    while let Some(mut update) = receiver.recv().await {
        normalize_content(&mut update);
        emit(AgentEvent::ToolExecutionUpdate {
            call: prepared.call.clone(),
            result: update,
        })
        .await;
    }
    normalize_content(&mut result);
    (result, duration_ms)
}

fn normalize_content(result: &mut ToolResult) {
    if !result.valid_content() {
        replace_with_error(
            result,
            "Tool results may contain only text and images".into(),
        );
    }
}

fn replace_with_error(result: &mut ToolResult, error: String) {
    result.content = vec![cayw_ai::Content::text(error)];
    result.structured_content = None;
    result.is_error = true;
}

fn catch_sync<T>(function: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(function)).map_err(panic_message)
}

fn panic_message(error: Box<dyn Any + Send>) -> String {
    if let Some(message) = error.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = error.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "Unknown panic payload".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BeforeToolDecision, NoHooks, ToolResultPatch};
    use async_trait::async_trait;
    use cayw_ai::{Content, Model, StreamOptions};
    use serde_json::json;
    use std::sync::{Mutex, atomic::AtomicUsize};
    use tokio::sync::Notify;

    struct TestTool {
        name: &'static str,
        mode: Option<ExecutionMode>,
        executions: Arc<AtomicUsize>,
        started: Option<Arc<Notify>>,
        release: Option<Arc<Notify>>,
        progress: Option<Arc<Mutex<Option<ToolProgress>>>>,
        panic: bool,
    }

    #[async_trait]
    impl AgentTool for TestTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name.into(),
                description: String::new(),
                parameters: json!({"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]}),
            }
        }
        fn execution_mode(&self) -> Option<ExecutionMode> {
            self.mode
        }
        async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            if let Some(started) = &self.started {
                started.notify_one();
            }
            if let Some(progress) = &self.progress {
                *progress.lock().unwrap() = Some(context.progress.clone());
                assert!(context.progress.emit(ToolResult::text("working")).await);
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
            assert!(!self.panic, "execution panic");
            Ok(ToolResult::text(context.arguments["n"].to_string()))
        }
    }

    fn test_tool(name: &'static str) -> TestTool {
        TestTool {
            name,
            mode: None,
            executions: Arc::new(AtomicUsize::new(0)),
            started: None,
            release: None,
            progress: None,
            panic: false,
        }
    }

    fn context(tools: Vec<Arc<dyn AgentTool>>) -> AgentContext {
        AgentContext {
            model: Model::new("fixture", "test"),
            options: StreamOptions::default(),
            messages: vec![],
            tools,
        }
    }

    fn assistant(calls: Vec<ToolCall>) -> AssistantMessage {
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "test"));
        assistant.content = calls.into_iter().map(Content::ToolCall).collect();
        assistant.stop_reason = StopReason::ToolUse;
        assistant
    }

    fn sink(events: Arc<Mutex<Vec<AgentEvent>>>) -> EventSink {
        Arc::new(move |event| {
            let events = events.clone();
            Box::pin(async move {
                events.lock().unwrap().push(event);
            })
        })
    }

    #[tokio::test]
    async fn errors_never_execute_and_still_pair_events() {
        let tool = Arc::new(test_tool("test"));
        let events = Arc::new(Mutex::new(vec![]));
        let assistant = assistant(vec![
            ToolCall::new("1", "test", json!({"n":"bad"})),
            ToolCall::new("2", "absent", json!({})),
        ]);
        let outcomes = execute_batch(
            &assistant,
            &context(vec![tool.clone()]),
            ExecutionMode::Parallel,
            Arc::new(NoHooks),
            CancellationToken::new(),
            sink(events.clone()),
        )
        .await;
        assert_eq!(tool.executions.load(Ordering::SeqCst), 0);
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.result.is_error && outcome.duration_ms.is_none())
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn truncated_or_duplicate_batches_never_execute() {
        let tool = Arc::new(test_tool("test"));
        for (reason, ids) in [
            (StopReason::Length, ["1", "2"]),
            (StopReason::ToolUse, ["same", "same"]),
            (StopReason::ToolUse, ["", "2"]),
        ] {
            let mut assistant = assistant(
                ids.into_iter()
                    .map(|id| ToolCall::new(id, "test", json!({"n":1})))
                    .collect(),
            );
            assistant.stop_reason = reason;
            let outcomes = execute_batch(
                &assistant,
                &context(vec![tool.clone()]),
                ExecutionMode::Parallel,
                Arc::new(NoHooks),
                CancellationToken::new(),
                sink(Arc::new(Mutex::new(vec![]))),
            )
            .await;
            assert!(outcomes.iter().all(|outcome| outcome.result.is_error));
        }
        assert_eq!(tool.executions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn parallel_ends_in_completion_order_and_returns_source_order() {
        let release = Arc::new(Notify::new());
        let mut slow = test_tool("slow");
        slow.release = Some(release.clone());
        let events = Arc::new(Mutex::new(vec![]));
        let recorded = events.clone();
        let emit: EventSink = Arc::new(move |event| {
            let recorded = recorded.clone();
            let release = release.clone();
            Box::pin(async move {
                let fast_ended =
                    matches!(&event, AgentEvent::ToolExecutionEnd { call, .. } if call.id == "2");
                recorded.lock().unwrap().push(event);
                if fast_ended {
                    release.notify_one();
                }
            })
        });
        let outcomes = execute_batch(
            &assistant(vec![
                ToolCall::new("1", "slow", json!({"n":1})),
                ToolCall::new("2", "fast", json!({"n":2})),
            ]),
            &context(vec![Arc::new(slow), Arc::new(test_tool("fast"))]),
            ExecutionMode::Parallel,
            Arc::new(NoHooks),
            CancellationToken::new(),
            emit,
        )
        .await;
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.call.id.as_str())
                .collect::<Vec<_>>(),
            ["1", "2"]
        );
        let ends = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolExecutionEnd { call, .. } => Some(call.id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ends, ["2", "1"]);
    }

    #[tokio::test]
    async fn sequential_override_applies_to_the_whole_batch() {
        let mut first = test_tool("first");
        first.mode = Some(ExecutionMode::Sequential);
        let events = Arc::new(Mutex::new(vec![]));
        execute_batch(
            &assistant(vec![
                ToolCall::new("1", "first", json!({"n":1})),
                ToolCall::new("2", "second", json!({"n":2})),
            ]),
            &context(vec![Arc::new(first), Arc::new(test_tool("second"))]),
            ExecutionMode::Parallel,
            Arc::new(NoHooks),
            CancellationToken::new(),
            sink(events.clone()),
        )
        .await;
        let events = events.lock().unwrap();
        assert!(matches!(&events[0], AgentEvent::ToolExecutionStart { call } if call.id == "1"));
        assert!(matches!(&events[1], AgentEvent::ToolExecutionEnd { call, .. } if call.id == "1"));
        assert!(matches!(&events[2], AgentEvent::ToolExecutionStart { call } if call.id == "2"));
    }

    #[tokio::test]
    async fn cancellation_distinguishes_started_calls_and_closes_progress() {
        let cancel = CancellationToken::new();
        let started = Arc::new(Notify::new());
        let progress = Arc::new(Mutex::new(None));
        let mut tool = test_tool("test");
        tool.started = Some(started.clone());
        tool.release = Some(Arc::new(Notify::new()));
        tool.progress = Some(progress.clone());
        let executions = tool.executions.clone();
        let events = Arc::new(Mutex::new(vec![]));
        let assistant = assistant(vec![ToolCall::new("1", "test", json!({"n":1}))]);
        let context = context(vec![Arc::new(tool)]);
        let run = execute_batch(
            &assistant,
            &context,
            ExecutionMode::Parallel,
            Arc::new(NoHooks),
            cancel.clone(),
            sink(events.clone()),
        );
        let abort = async {
            started.notified().await;
            cancel.cancel();
        };
        let (outcomes, ()) = tokio::join!(run, abort);
        assert!(outcomes[0].result.outcome_unknown);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        let handle = progress.lock().unwrap().take().unwrap();
        assert!(!handle.emit(ToolResult::text("late")).await);
        let outcomes = execute_batch(
            &assistant,
            &context,
            ExecutionMode::Parallel,
            Arc::new(NoHooks),
            cancel,
            sink(events),
        )
        .await;
        assert!(!outcomes[0].result.outcome_unknown);
        assert!(outcomes[0].duration_ms.is_none());
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    struct BlockAndRewrite;
    #[async_trait]
    impl AgentHooks for BlockAndRewrite {
        async fn before_tool_call(
            &self,
            _context: ToolHookContext,
            _cancel: CancellationToken,
        ) -> Result<BeforeToolDecision, String> {
            Ok(BeforeToolDecision {
                block: true,
                reason: Some("denied".into()),
                terminate: true,
            })
        }
        async fn after_tool_call(
            &self,
            _context: ToolHookContext,
            _result: ToolResult,
            _cancel: CancellationToken,
        ) -> Result<ToolResultPatch, String> {
            Ok(ToolResultPatch {
                content: Some(vec![Content::text("rewritten")]),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn blocked_calls_run_after_hook_without_executing() {
        let tool = Arc::new(test_tool("test"));
        let outcomes = execute_batch(
            &assistant(vec![ToolCall::new("1", "test", json!({"n":1}))]),
            &context(vec![tool.clone()]),
            ExecutionMode::Parallel,
            Arc::new(BlockAndRewrite),
            CancellationToken::new(),
            sink(Arc::new(Mutex::new(vec![]))),
        )
        .await;
        assert_eq!(tool.executions.load(Ordering::SeqCst), 0);
        assert!(outcomes[0].result.is_error && outcomes[0].result.terminate);
        assert_eq!(outcomes[0].result.content, vec![Content::text("rewritten")]);
    }

    struct BeforePanic;
    #[async_trait]
    impl AgentHooks for BeforePanic {
        async fn before_tool_call(
            &self,
            _context: ToolHookContext,
            _cancel: CancellationToken,
        ) -> Result<BeforeToolDecision, String> {
            panic!("before panic")
        }
    }

    struct AfterFailure;
    #[async_trait]
    impl AgentHooks for AfterFailure {
        async fn after_tool_call(
            &self,
            _context: ToolHookContext,
            _result: ToolResult,
            _cancel: CancellationToken,
        ) -> Result<ToolResultPatch, String> {
            Err("after failed".into())
        }
    }

    #[tokio::test]
    async fn before_panics_and_after_errors_produce_paired_results() {
        let tool = Arc::new(test_tool("test"));
        let assistant = assistant(vec![ToolCall::new("1", "test", json!({"n":1}))]);
        for hooks in [
            Arc::new(BeforePanic) as Arc<dyn AgentHooks>,
            Arc::new(AfterFailure),
        ] {
            let events = Arc::new(Mutex::new(vec![]));
            let outcomes = execute_batch(
                &assistant,
                &context(vec![tool.clone()]),
                ExecutionMode::Parallel,
                hooks,
                CancellationToken::new(),
                sink(events.clone()),
            )
            .await;
            assert!(outcomes[0].result.is_error);
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. }))
                    .count(),
                1
            );
        }
        assert_eq!(tool.executions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn completed_progress_is_drained_before_end_and_late_updates_are_rejected() {
        let progress = Arc::new(Mutex::new(None));
        let mut tool = test_tool("test");
        tool.progress = Some(progress.clone());
        let events = Arc::new(Mutex::new(vec![]));
        let outcomes = execute_batch(
            &assistant(vec![ToolCall::new("1", "test", json!({"n":1}))]),
            &context(vec![Arc::new(tool)]),
            ExecutionMode::Parallel,
            Arc::new(NoHooks),
            CancellationToken::new(),
            sink(events.clone()),
        )
        .await;
        assert!(!outcomes[0].result.is_error);
        let handle = progress.lock().unwrap().take().unwrap();
        assert!(!handle.emit(ToolResult::text("late")).await);
        let events = events.lock().unwrap();
        assert!(matches!(&events[1], AgentEvent::ToolExecutionUpdate { .. }));
        assert!(matches!(&events[2], AgentEvent::ToolExecutionEnd { .. }));
    }

    #[tokio::test]
    async fn dropped_batch_closes_progress_without_cancelling_its_parent() {
        let parent = CancellationToken::new();
        let started = Arc::new(Notify::new());
        let progress = Arc::new(Mutex::new(None));
        let mut tool = test_tool("test");
        tool.started = Some(started.clone());
        tool.release = Some(Arc::new(Notify::new()));
        tool.progress = Some(progress.clone());
        let assistant = assistant(vec![ToolCall::new("1", "test", json!({"n":1}))]);
        let context = context(vec![Arc::new(tool)]);
        let mut run = Box::pin(execute_batch(
            &assistant,
            &context,
            ExecutionMode::Parallel,
            Arc::new(NoHooks),
            parent.clone(),
            sink(Arc::new(Mutex::new(vec![]))),
        ));
        tokio::select! {
            _ = &mut run => panic!("Tool should still be waiting"),
            _ = started.notified() => {},
        }
        drop(run);
        assert!(!parent.is_cancelled());
        let handle = progress.lock().unwrap().take().unwrap();
        assert!(!handle.emit(ToolResult::text("late")).await);
    }

    #[tokio::test]
    async fn execution_panic_becomes_an_unknown_error_result() {
        let mut tool = test_tool("test");
        tool.panic = true;
        let outcomes = execute_batch(
            &assistant(vec![ToolCall::new("1", "test", json!({"n":1}))]),
            &context(vec![Arc::new(tool)]),
            ExecutionMode::Parallel,
            Arc::new(NoHooks),
            CancellationToken::new(),
            sink(Arc::new(Mutex::new(vec![]))),
        )
        .await;
        assert!(outcomes[0].result.is_error && outcomes[0].result.outcome_unknown);
        assert!(outcomes[0].duration_ms.is_some());
    }
}
