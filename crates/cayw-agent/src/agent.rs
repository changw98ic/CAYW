use crate::execution::{EventSink, execute_batch};
use crate::*;
use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Message, Model, ModelRequest, Provider,
    StopReason, StreamOptions, ToolResultMessage, timestamp_ms, tool_changes,
};
use futures::{FutureExt, future::BoxFuture};
use std::{
    collections::{BTreeSet, VecDeque},
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::watch;

type Listener = Arc<
    dyn Fn(AgentEvent, CancellationToken) -> BoxFuture<'static, Result<(), String>> + Send + Sync,
>;

struct Runtime {
    context: AgentContext,
    running: bool,
    streaming: Option<AssistantMessage>,
    pending: BTreeSet<String>,
    error: Option<String>,
    listener_error: Option<String>,
    needs_reconciliation: bool,
    cancel: Option<CancellationToken>,
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    execution: ExecutionMode,
    max_turns: usize,
    hooks: Arc<dyn AgentHooks>,
}

struct Shared {
    runtime: Mutex<Runtime>,
    listeners: Mutex<Vec<(usize, Listener)>>,
    next_listener: std::sync::atomic::AtomicUsize,
    idle: watch::Sender<u64>,
    event_lock: tokio::sync::Mutex<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An agent can be shared, but only one prompt or continuation runs at a time.
#[derive(Clone)]
pub struct Agent {
    provider: Arc<dyn Provider>,
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub struct AgentControl {
    shared: Arc<Shared>,
}

impl AgentControl {
    pub fn snapshot(&self) -> AgentSnapshot {
        let state = lock(&self.shared.runtime);
        AgentSnapshot {
            model: state.context.model.clone(),
            messages: state.context.messages.clone(),
            is_running: state.running,
            streaming_message: state.streaming.clone(),
            pending_tool_calls: state.pending.clone(),
            error_message: state.error.clone(),
            needs_reconciliation: state.needs_reconciliation,
        }
    }
    pub fn steer(&self, message: Message) {
        lock(&self.shared.runtime).steering.push_back(message);
    }
    pub fn follow_up(&self, message: Message) {
        lock(&self.shared.runtime).follow_up.push_back(message);
    }
    pub fn queued_messages(&self) -> (Vec<Message>, Vec<Message>) {
        let state = lock(&self.shared.runtime);
        (
            state.steering.iter().cloned().collect(),
            state.follow_up.iter().cloned().collect(),
        )
    }
    pub fn clear_queues(&self) {
        let mut state = lock(&self.shared.runtime);
        state.steering.clear();
        state.follow_up.clear();
    }
    pub fn abort(&self) {
        if let Some(cancel) = &lock(&self.shared.runtime).cancel {
            cancel.cancel();
        }
    }
    pub async fn wait_for_idle(&self) {
        let mut idle = self.shared.idle.subscribe();
        loop {
            if !lock(&self.shared.runtime).running {
                return;
            }
            if idle.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum QueueKind {
    Steering,
    FollowUp,
}

struct PendingEntry {
    message: Message,
    origin: Option<QueueKind>,
}

struct PendingMessages {
    shared: Arc<Shared>,
    entries: VecDeque<PendingEntry>,
}

impl PendingMessages {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            entries: VecDeque::new(),
        }
    }
    fn extend(&mut self, messages: Vec<Message>, origin: Option<QueueKind>) {
        self.entries.extend(
            messages
                .into_iter()
                .map(|message| PendingEntry { message, origin }),
        );
    }
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Drop for PendingMessages {
    fn drop(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let mut state = lock(&self.shared.runtime);
        for entry in self.entries.drain(..).rev() {
            match entry.origin {
                Some(QueueKind::Steering) => state.steering.push_front(entry.message),
                Some(QueueKind::FollowUp) => state.follow_up.push_front(entry.message),
                None => {}
            }
        }
    }
}

struct RunGuard {
    shared: Arc<Shared>,
    cancel: CancellationToken,
    settled: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let mut state = lock(&self.shared.runtime);
        if !self.settled {
            state.needs_reconciliation |=
                !state.pending.is_empty() || has_unresolved_tools(&state.context.messages);
            state.error = Some(
                "Run future dropped before settlement; inspect history before continuing".into(),
            );
        }
        state.running = false;
        state.cancel = None;
        state.streaming = None;
        state.pending.clear();
        drop(state);
        self.shared
            .idle
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>, model: Model) -> Self {
        let (idle, _) = watch::channel(0);
        Self {
            provider,
            shared: Arc::new(Shared {
                runtime: Mutex::new(Runtime {
                    context: AgentContext {
                        model,
                        options: StreamOptions::default(),
                        messages: vec![],
                        tools: vec![],
                    },
                    running: false,
                    streaming: None,
                    pending: BTreeSet::new(),
                    error: None,
                    listener_error: None,
                    needs_reconciliation: false,
                    cancel: None,
                    steering: VecDeque::new(),
                    follow_up: VecDeque::new(),
                    steering_mode: QueueMode::OneAtATime,
                    follow_up_mode: QueueMode::OneAtATime,
                    execution: ExecutionMode::Parallel,
                    max_turns: 64,
                    hooks: Arc::new(NoHooks),
                }),
                listeners: Mutex::new(vec![]),
                next_listener: std::sync::atomic::AtomicUsize::new(1),
                idle,
                event_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }
    pub fn control(&self) -> AgentControl {
        AgentControl {
            shared: self.shared.clone(),
        }
    }
    pub fn snapshot(&self) -> AgentSnapshot {
        self.control().snapshot()
    }
    pub fn subscribe<F, Fut>(&self, callback: F) -> usize
    where
        F: Fn(AgentEvent, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let id = self
            .shared
            .next_listener
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        lock(&self.shared.listeners).push((
            id,
            Arc::new(move |event, cancel| callback(event, cancel).boxed()),
        ));
        id
    }
    pub fn unsubscribe(&self, id: usize) {
        lock(&self.shared.listeners).retain(|(current, _)| *current != id);
    }
    fn configure(
        &self,
        change: impl FnOnce(&mut Runtime) -> Result<(), AgentError>,
    ) -> Result<(), AgentError> {
        let mut state = lock(&self.shared.runtime);
        if state.running {
            return Err(AgentError::Busy);
        }
        change(&mut state)
    }
    pub fn set_model(&self, model: Model) -> Result<(), AgentError> {
        self.configure(|state| {
            state.context.model = model;
            Ok(())
        })
    }
    pub fn set_stream_options(&self, options: StreamOptions) -> Result<(), AgentError> {
        self.configure(|state| {
            state.context.options = options;
            Ok(())
        })
    }
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) -> Result<(), AgentError> {
        validate_tools(&tools)?;
        self.configure(|state| {
            state.context.tools = tools;
            Ok(())
        })
    }
    /// The host supplies reconciled history; unknown effects are never replayed automatically.
    pub fn replace_messages(&self, messages: Vec<Message>) -> Result<(), AgentError> {
        self.configure(|state| {
            state.needs_reconciliation = has_unresolved_tools(&messages);
            state.context.messages = messages;
            Ok(())
        })
    }
    pub fn set_hooks(&self, hooks: Arc<dyn AgentHooks>) -> Result<(), AgentError> {
        self.configure(|state| {
            state.hooks = hooks;
            Ok(())
        })
    }
    pub fn set_execution_mode(&self, mode: ExecutionMode) -> Result<(), AgentError> {
        self.configure(|state| {
            state.execution = mode;
            Ok(())
        })
    }
    pub fn set_queue_modes(
        &self,
        steering: QueueMode,
        follow_up: QueueMode,
    ) -> Result<(), AgentError> {
        self.configure(|state| {
            state.steering_mode = steering;
            state.follow_up_mode = follow_up;
            Ok(())
        })
    }
    pub fn set_max_turns(&self, max_turns: usize) -> Result<(), AgentError> {
        if max_turns == 0 {
            return Err(AgentError::InvalidConfiguration(
                "max_turns must be positive".into(),
            ));
        }
        self.configure(|state| {
            state.max_turns = max_turns;
            Ok(())
        })
    }
    pub fn reset(&self) -> Result<(), AgentError> {
        self.configure(|state| {
            if state.needs_reconciliation {
                return Err(AgentError::UnresolvedToolEffects);
            }
            let system = cayw_ai::current_system(&state.context.messages);
            state.context.messages = vec![Message::System(cayw_ai::SystemMessage {
                content: system.prompt,
                tools_added: system.tools,
                ..Default::default()
            })];
            state.steering.clear();
            state.follow_up.clear();
            state.error = None;
            state.needs_reconciliation = false;
            Ok(())
        })
    }
    pub async fn prompt(&self, message: Message) -> Result<RunResult, AgentError> {
        self.prompt_messages(vec![message]).await
    }
    pub async fn prompt_messages(&self, messages: Vec<Message>) -> Result<RunResult, AgentError> {
        if messages.is_empty() {
            return Err(AgentError::EmptyPrompt);
        }
        self.run(Some(messages)).await
    }
    pub async fn continue_run(&self) -> Result<RunResult, AgentError> {
        self.run(None).await
    }

    async fn run(&self, input: Option<Vec<Message>>) -> Result<RunResult, AgentError> {
        let (context, pending, hooks, execution, max_turns, mut guard) = {
            let mut state = lock(&self.shared.runtime);
            if state.running {
                return Err(AgentError::Busy);
            }
            if state.needs_reconciliation {
                return Err(AgentError::UnresolvedToolEffects);
            }
            let mut pending = PendingMessages::new(self.shared.clone());
            if let Some(messages) = input {
                pending.extend(messages, None);
            } else {
                let tail = state.context.messages.iter().rev().find(|message| {
                    !matches!(message, Message::System(_) | Message::Custom { .. })
                });
                if tail.is_none() {
                    return Err(AgentError::EmptyHistory);
                }
                let after_assistant = matches!(tail, Some(Message::Assistant(_)));
                if after_assistant && state.steering.is_empty() && state.follow_up.is_empty() {
                    return Err(AgentError::NeedsInput);
                }
                let mode = state.steering_mode;
                pending.extend(
                    drain_queue(&mut state.steering, mode),
                    Some(QueueKind::Steering),
                );
                if after_assistant && pending.is_empty() {
                    let mode = state.follow_up_mode;
                    pending.extend(
                        drain_queue(&mut state.follow_up, mode),
                        Some(QueueKind::FollowUp),
                    );
                }
            }
            let cancel = CancellationToken::new();
            state.running = true;
            state.error = None;
            state.listener_error = None;
            state.cancel = Some(cancel.clone());
            (
                state.context.clone(),
                pending,
                state.hooks.clone(),
                state.execution,
                state.max_turns,
                RunGuard {
                    shared: self.shared.clone(),
                    cancel,
                    settled: false,
                },
            )
        };
        let cancel = guard.cancel.clone();
        self.emit(AgentEvent::AgentStart, &cancel).await;
        let outcome = AssertUnwindSafe(self.run_loop(
            context,
            pending,
            hooks,
            execution,
            max_turns,
            cancel.clone(),
        ))
        .catch_unwind()
        .await;
        let result = match outcome {
            Ok(result) => result,
            Err(_) => {
                let context = lock(&self.shared.runtime).context.clone();
                let mut messages = vec![];
                let last_message = self
                    .error_turn(
                        &context,
                        StopReason::Error,
                        "Agent runtime panicked",
                        &cancel,
                        &mut messages,
                    )
                    .await;
                RunResult {
                    messages,
                    last_message,
                }
            }
        };
        self.emit(
            AgentEvent::AgentEnd {
                messages: result.messages.clone(),
            },
            &cancel,
        )
        .await;
        {
            let mut state = lock(&self.shared.runtime);
            state.needs_reconciliation |= has_unresolved_tools(&state.context.messages);
        }
        guard.settled = true;
        let listener_error = lock(&self.shared.runtime).listener_error.clone();
        drop(guard);
        if let Some(error) = listener_error {
            Err(AgentError::Listener(error))
        } else {
            Ok(result)
        }
    }

    async fn emit(&self, event: AgentEvent, cancel: &CancellationToken) {
        let _event_guard = self.shared.event_lock.lock().await;
        {
            let mut state = lock(&self.shared.runtime);
            match &event {
                AgentEvent::MessageStart {
                    message: Message::Assistant(message),
                }
                | AgentEvent::MessageUpdate { message, .. } => {
                    state.streaming = Some(message.clone())
                }
                AgentEvent::MessageEnd {
                    message: Message::Assistant(message),
                } => {
                    state.streaming = None;
                    if message.stop_reason.is_failure() {
                        state.error = message.error_message.clone();
                    }
                }
                AgentEvent::ToolExecutionStart { call } => {
                    state.pending.insert(call.id.clone());
                }
                AgentEvent::ToolExecutionEnd { call, result, .. } => {
                    state.pending.remove(&call.id);
                    state.needs_reconciliation |= result.outcome_unknown;
                }
                _ => {}
            }
        }
        let listeners = lock(&self.shared.listeners).clone();
        for (_, listener) in listeners {
            let outcome = AssertUnwindSafe(async { listener(event.clone(), cancel.clone()).await })
                .catch_unwind()
                .await;
            let error = match outcome {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some("Event listener panicked".into()),
            };
            if let Some(error) = error {
                let mut state = lock(&self.shared.runtime);
                state.listener_error.get_or_insert(error.clone());
                state.error = Some(error);
                cancel.cancel();
            }
        }
    }
    fn sync_context(&self, context: &AgentContext) {
        lock(&self.shared.runtime).context = context.clone();
    }
    async fn append(
        &self,
        context: &mut AgentContext,
        message: Message,
        new_messages: &mut Vec<Message>,
        cancel: &CancellationToken,
    ) {
        self.emit(
            AgentEvent::MessageStart {
                message: message.clone(),
            },
            cancel,
        )
        .await;
        context.messages.push(message.clone());
        new_messages.push(message.clone());
        self.sync_context(context);
        self.emit(AgentEvent::MessageEnd { message }, cancel).await;
    }
    async fn announce_tools(
        &self,
        context: &mut AgentContext,
        new_messages: &mut Vec<Message>,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        validate_tools(&context.tools).map_err(|error| error.to_string())?;
        let definitions = context
            .tools
            .iter()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        if let Some(message) = tool_changes(&context.messages, &definitions) {
            self.append(context, message, new_messages, cancel).await;
        }
        Ok(())
    }

    async fn run_loop(
        &self,
        mut context: AgentContext,
        mut pending: PendingMessages,
        hooks: Arc<dyn AgentHooks>,
        execution: ExecutionMode,
        max_turns: usize,
        cancel: CancellationToken,
    ) -> RunResult {
        let mut new_messages = Vec::new();
        let mut turn_number = 0;
        loop {
            turn_number += 1;
            self.emit(AgentEvent::TurnStart, &cancel).await;
            while let Some(entry) = pending.entries.front() {
                let message = entry.message.clone();
                self.emit(
                    AgentEvent::MessageStart {
                        message: message.clone(),
                    },
                    &cancel,
                )
                .await;
                context.messages.push(message.clone());
                new_messages.push(message.clone());
                self.sync_context(&context);
                pending.entries.pop_front();
                self.emit(AgentEvent::MessageEnd { message }, &cancel).await;
            }
            let unresolved_input = has_unresolved_tools(&context.messages);
            if unresolved_input {
                lock(&self.shared.runtime).needs_reconciliation = true;
            }
            let prepared = if unresolved_input {
                Err(
                    "Input contains unresolved tool effects; reconcile history before continuing"
                        .into(),
                )
            } else if turn_number > max_turns {
                Err(format!("Agent reached max_turns ({max_turns})"))
            } else if cancel.is_cancelled() {
                Err("Agent run cancelled".into())
            } else {
                self.announce_tools(&mut context, &mut new_messages, &cancel)
                    .await
                    .map(|_| ())
            };
            let prepared = if prepared.is_ok() {
                async {
                    if let Some(replacement) = callback(
                        &cancel,
                        hooks.prepare_request(context.clone(), cancel.clone()),
                    )
                    .await?
                    {
                        validate_tools(&replacement.tools).map_err(|error| error.to_string())?;
                        if has_unresolved_tools(&replacement.messages) {
                            return Err("prepare_request supplied unresolved tool effects".into());
                        }
                        context = replacement;
                        self.sync_context(&context);
                        self.announce_tools(&mut context, &mut new_messages, &cancel)
                            .await?;
                    }
                    Ok::<(), String>(())
                }
                .await
            } else {
                prepared
            };
            let assistant = match prepared {
                Ok(()) => self.stream_assistant(&context, &cancel).await,
                Err(error) => {
                    let reason = if cancel.is_cancelled() {
                        StopReason::Aborted
                    } else {
                        StopReason::Error
                    };
                    let message = AssistantMessage::new(&context.model).failure(reason, error);
                    self.emit(
                        AgentEvent::MessageStart {
                            message: Message::Assistant(message.clone()),
                        },
                        &cancel,
                    )
                    .await;
                    message
                }
            };
            context.messages.push(Message::Assistant(assistant.clone()));
            new_messages.push(Message::Assistant(assistant.clone()));
            self.sync_context(&context);
            self.emit(
                AgentEvent::MessageEnd {
                    message: Message::Assistant(assistant.clone()),
                },
                &cancel,
            )
            .await;
            let mut tool_results = vec![];
            let mut all_terminate = false;
            if !assistant.stop_reason.is_failure() && !assistant.tool_calls().is_empty() {
                let agent = self.clone();
                let signal = cancel.clone();
                let sink: EventSink = Arc::new(move |event| {
                    let agent = agent.clone();
                    let signal = signal.clone();
                    async move { agent.emit(event, &signal).await }.boxed()
                });
                let outcomes = execute_batch(
                    &assistant,
                    &context,
                    execution,
                    hooks.clone(),
                    cancel.clone(),
                    sink,
                )
                .await;
                all_terminate =
                    !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.result.terminate);
                for outcome in outcomes {
                    let result = ToolResultMessage {
                        tool_call_id: outcome.call.id,
                        tool_name: outcome.call.name,
                        content: outcome.result.content,
                        details: outcome.result.details,
                        structured_content: outcome.result.structured_content,
                        usage: outcome.result.usage,
                        is_error: outcome.result.is_error,
                        outcome_unknown: outcome.result.outcome_unknown,
                        timestamp: timestamp_ms(),
                        duration_ms: outcome.duration_ms,
                    };
                    self.append(
                        &mut context,
                        Message::ToolResult(result.clone()),
                        &mut new_messages,
                        &cancel,
                    )
                    .await;
                    tool_results.push(result);
                }
            }
            let turn = TurnContext {
                assistant: assistant.clone(),
                tool_results: tool_results.clone(),
                context: context.clone(),
                new_messages: new_messages.clone(),
            };
            let unresolved = lock(&self.shared.runtime).needs_reconciliation;
            let decision =
                if assistant.stop_reason.is_failure() || cancel.is_cancelled() || unresolved {
                    Ok(TurnDecision::Auto)
                } else {
                    callback(&cancel, hooks.finish_turn(turn.clone(), cancel.clone())).await
                };
            self.emit(
                AgentEvent::TurnEnd {
                    message: assistant.clone(),
                    tool_results,
                },
                &cancel,
            )
            .await;
            if assistant.stop_reason.is_failure() {
                return RunResult {
                    messages: new_messages,
                    last_message: assistant,
                };
            }
            if cancel.is_cancelled() || decision.is_err() || unresolved {
                let reason = if cancel.is_cancelled() {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                let error = decision.err().unwrap_or_else(|| {
                    if unresolved {
                        "Tool effects are unresolved; reconcile history before continuing".into()
                    } else {
                        "Agent run cancelled".into()
                    }
                });
                let message = self
                    .error_turn(&context, reason, &error, &cancel, &mut new_messages)
                    .await;
                return RunResult {
                    messages: new_messages,
                    last_message: message,
                };
            }
            let decision = decision.unwrap_or_default();
            if decision == TurnDecision::End {
                return RunResult {
                    messages: new_messages,
                    last_message: assistant,
                };
            }
            let tool_continue = !assistant.tool_calls().is_empty() && !all_terminate;
            let took_steering;
            {
                let mut state = lock(&self.shared.runtime);
                let mode = state.steering_mode;
                pending.extend(
                    drain_queue(&mut state.steering, mode),
                    Some(QueueKind::Steering),
                );
                took_steering = !pending.is_empty();
                if !tool_continue && pending.is_empty() {
                    let mode = state.follow_up_mode;
                    pending.extend(
                        drain_queue(&mut state.follow_up, mode),
                        Some(QueueKind::FollowUp),
                    );
                }
            }
            if !tool_continue && pending.is_empty() && decision != TurnDecision::Continue {
                return RunResult {
                    messages: new_messages,
                    last_message: assistant,
                };
            }
            match callback(&cancel, hooks.prepare_next_turn(turn, cancel.clone())).await {
                Ok(update) => {
                    if let Some(replacement) = update.context {
                        if let Err(error) = validate_tools(&replacement.tools) {
                            let message = self
                                .error_turn(
                                    &context,
                                    StopReason::Error,
                                    &error.to_string(),
                                    &cancel,
                                    &mut new_messages,
                                )
                                .await;
                            return RunResult {
                                messages: new_messages,
                                last_message: message,
                            };
                        }
                        if has_unresolved_tools(&replacement.messages) {
                            let message = self
                                .error_turn(
                                    &context,
                                    StopReason::Error,
                                    "prepare_next_turn supplied unresolved tool effects",
                                    &cancel,
                                    &mut new_messages,
                                )
                                .await;
                            return RunResult {
                                messages: new_messages,
                                last_message: message,
                            };
                        }
                        context = replacement;
                        self.sync_context(&context);
                    }
                    pending.extend(update.messages, None);
                }
                Err(error) => {
                    let reason = if cancel.is_cancelled() {
                        StopReason::Aborted
                    } else {
                        StopReason::Error
                    };
                    let message = self
                        .error_turn(&context, reason, &error, &cancel, &mut new_messages)
                        .await;
                    return RunResult {
                        messages: new_messages,
                        last_message: message,
                    };
                }
            }
            if !took_steering {
                let mut state = lock(&self.shared.runtime);
                let mode = state.steering_mode;
                let steering = drain_queue(&mut state.steering, mode);
                pending.extend(steering, Some(QueueKind::Steering));
            }
        }
    }

    async fn error_turn(
        &self,
        context: &AgentContext,
        reason: StopReason,
        error: &str,
        cancel: &CancellationToken,
        new_messages: &mut Vec<Message>,
    ) -> AssistantMessage {
        self.emit(AgentEvent::TurnStart, cancel).await;
        let mut context = context.clone();
        let message = AssistantMessage::new(&context.model).failure(reason, error);
        self.append(
            &mut context,
            Message::Assistant(message.clone()),
            new_messages,
            cancel,
        )
        .await;
        self.emit(
            AgentEvent::TurnEnd {
                message: message.clone(),
                tool_results: vec![],
            },
            cancel,
        )
        .await;
        message
    }

    async fn stream_assistant(
        &self,
        context: &AgentContext,
        cancel: &CancellationToken,
    ) -> AssistantMessage {
        let mut partial = AssistantMessage::new(&context.model);
        let started = std::time::Instant::now();
        self.emit(
            AgentEvent::MessageStart {
                message: Message::Assistant(partial.clone()),
            },
            cancel,
        )
        .await;
        let child = cancel.child_token();
        let _cancel_on_drop = child.clone().drop_guard();
        let request = ModelRequest {
            model: context.model.clone(),
            messages: context.messages.clone(),
            options: context.options.clone(),
        };
        let stream =
            std::panic::catch_unwind(AssertUnwindSafe(|| self.provider.stream(request, child)));
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(_) => {
                return partial
                    .failure(StopReason::Error, "Provider panicked while creating stream");
            }
        };
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => return with_duration(partial.failure(StopReason::Aborted, "Model request cancelled"), started),
                next = AssertUnwindSafe(futures::StreamExt::next(&mut stream)).catch_unwind() => next,
            };
            let event = match next {
                Ok(Some(event)) => event,
                Ok(None) => {
                    return with_duration(
                        partial.failure(
                            StopReason::Error,
                            "Provider stream ended without a terminal event",
                        ),
                        started,
                    );
                }
                Err(_) => {
                    return with_duration(
                        partial.failure(StopReason::Error, "Provider stream panicked"),
                        started,
                    );
                }
            };
            partial = event.message().clone();
            if event.is_terminal() {
                if matches!(event, AssistantEvent::Error { .. })
                    && !partial.stop_reason.is_failure()
                {
                    partial = partial.failure(
                        StopReason::Error,
                        "Provider emitted error without failure reason",
                    );
                } else if partial.stop_reason == StopReason::Pending {
                    partial = partial.failure(
                        StopReason::Error,
                        "Provider emitted unfinished terminal message",
                    );
                }
                let calls = partial.tool_calls();
                let mut ids = BTreeSet::new();
                if !partial.stop_reason.is_failure()
                    && calls.iter().any(|call| {
                        call.id.is_empty() || call.name.is_empty() || !ids.insert(call.id.clone())
                    })
                {
                    partial = partial.failure(
                        StopReason::Error,
                        "Provider emitted missing or duplicate tool call identifiers",
                    );
                }
                if partial.stop_reason == StopReason::ToolUse && calls.is_empty() {
                    partial = partial.failure(
                        StopReason::Error,
                        "Provider requested tool execution without tool calls",
                    );
                }
                if partial.stop_reason == StopReason::Stop && !calls.is_empty() {
                    partial = partial.failure(
                        StopReason::Error,
                        "Provider returned tool calls with a stop reason that forbids execution",
                    );
                }
                let previous_ids = context
                    .messages
                    .iter()
                    .filter_map(|message| match message {
                        Message::Assistant(assistant) if !assistant.stop_reason.is_failure() => {
                            Some(assistant.tool_calls())
                        }
                        _ => None,
                    })
                    .flatten()
                    .map(|call| call.id)
                    .collect::<BTreeSet<_>>();
                if !partial.stop_reason.is_failure()
                    && calls.iter().any(|call| previous_ids.contains(&call.id))
                {
                    partial = partial.failure(
                        StopReason::Error,
                        "Provider reused a tool call identifier from history",
                    );
                }
                return with_duration(partial, started);
            }
            self.emit(
                AgentEvent::MessageUpdate {
                    message: partial.clone(),
                    event: Box::new(event),
                },
                cancel,
            )
            .await;
        }
    }
}

fn with_duration(mut message: AssistantMessage, started: std::time::Instant) -> AssistantMessage {
    if message.duration_ms.is_none() {
        message.duration_ms = Some(started.elapsed().as_millis() as u64);
    }
    message
}

async fn callback<T>(
    cancel: &CancellationToken,
    future: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("Agent callback cancelled".into()),
        result = AssertUnwindSafe(future).catch_unwind() => result.unwrap_or_else(|_| Err("Agent callback panicked".into())),
    }
}

fn drain_queue(queue: &mut VecDeque<Message>, mode: QueueMode) -> Vec<Message> {
    match mode {
        QueueMode::All => queue.drain(..).collect(),
        QueueMode::OneAtATime => queue.pop_front().into_iter().collect(),
    }
}

fn validate_tools(tools: &[Arc<dyn AgentTool>]) -> Result<(), AgentError> {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut names = BTreeSet::new();
        for tool in tools {
            let definition = tool.definition();
            if definition.name.is_empty() || !names.insert(definition.name.clone()) {
                return Err(AgentError::InvalidConfiguration(
                    "Tool names must be nonempty and unique".into(),
                ));
            }
            crate::tool::validate_shutdown_grace(tool.shutdown_grace())
                .map_err(AgentError::InvalidConfiguration)?;
            cayw_ai::ArgumentValidator::new(&definition.parameters)
                .map_err(|error| AgentError::InvalidConfiguration(error.to_string()))?;
        }
        Ok(())
    }))
    .unwrap_or_else(|_| {
        Err(AgentError::InvalidConfiguration(
            "Tool definition panicked".into(),
        ))
    })
}

fn has_unresolved_tools(messages: &[Message]) -> bool {
    let mut pending = BTreeSet::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) if !assistant.stop_reason.is_failure() => {
                for call in assistant.tool_calls() {
                    pending.insert(call.id);
                }
            }
            Message::ToolResult(result) => {
                if result.outcome_unknown {
                    return true;
                }
                pending.remove(&result.tool_call_id);
            }
            _ => {}
        }
    }
    !pending.is_empty()
}
