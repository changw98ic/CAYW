use crate::{
    ActiveRun, Inner, Runtime, RuntimeError, SessionView, TaskRecord, TaskState, lock,
    permissions::Gate,
    queue::{TaskLease, TaskTerminal},
};
use async_trait::async_trait;
use cayw_agent::{Agent, AgentContext, AgentEvent, AgentHooks, AgentTool, ToolContext, ToolResult};
use cayw_ai::{CancellationToken, Message, ToolDefinition};
use cayw_cells::{CellManager, CellTool};
use cayw_codemode::CapabilityError;
use cayw_core::{ApprovalDecision, AttemptLease};
use futures::FutureExt;
use serde_json::Value;
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) struct RunContext {
    pub inner: Arc<Inner>,
    pub lease: AttemptLease,
    pub task: TaskLease,
    pub cancel: CancellationToken,
    started: Instant,
    tokens: AtomicU64,
    turns: AtomicUsize,
    calls: AtomicUsize,
    bytes: AtomicUsize,
    pub unknown: Arc<AtomicBool>,
    persistence_error: Mutex<Option<String>>,
}
impl RunContext {
    pub(crate) fn check(&self, cancel: &CancellationToken) -> Result<(), RuntimeError> {
        if self.cancel.is_cancelled() || cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if self.started.elapsed() >= self.inner.config.max_run_time {
            return Err(RuntimeError::Budget("time"));
        }
        if self.tokens.load(Ordering::Acquire) > self.inner.config.max_total_tokens {
            return Err(RuntimeError::Budget("token"));
        }
        if self.turns.load(Ordering::Acquire) > self.inner.config.max_turns {
            return Err(RuntimeError::Budget("turn"));
        }
        if self.calls.load(Ordering::Acquire) > self.inner.config.max_capability_calls {
            return Err(RuntimeError::Budget("capability count"));
        }
        if self.bytes.load(Ordering::Acquire) > self.inner.config.max_capability_bytes {
            return Err(RuntimeError::Budget("capability byte"));
        }
        if self.unknown.load(Ordering::Acquire) {
            return Err(RuntimeError::NeedsReconciliation);
        }
        let view = Runtime {
            inner: self.inner.clone(),
        }
        .get(&self.lease.session_id)?;
        if view.frozen {
            return Err(RuntimeError::Frozen(
                view.binding
                    .and_then(|binding| binding.reason)
                    .unwrap_or_else(|| "plugin unavailable".into()),
            ));
        }
        self.inner.queue.check(&self.task)?;
        self.inner.store.validate_lease(&self.lease)?;
        Ok(())
    }
    pub(crate) fn admit_call(&self, arguments: &Value) -> Result<(), RuntimeError> {
        let count = self.calls.fetch_add(1, Ordering::AcqRel);
        if count >= self.inner.config.max_capability_calls {
            return Err(RuntimeError::Budget("capability count"));
        }
        self.charge_bytes(arguments)
    }
    pub(crate) fn charge_bytes(&self, value: &impl serde::Serialize) -> Result<(), RuntimeError> {
        let size = serde_json::to_vec(value)?.len();
        let previous = self.bytes.fetch_add(size, Ordering::AcqRel);
        if previous.saturating_add(size) > self.inner.config.max_capability_bytes {
            return Err(RuntimeError::Budget("capability byte"));
        }
        Ok(())
    }
    pub(crate) fn persistence_failed(&self, error: impl std::fmt::Display) -> CapabilityError {
        let error = error.to_string();
        lock(&self.persistence_error).get_or_insert_with(|| error.clone());
        self.unknown.store(true, Ordering::Release);
        self.cancel.cancel();
        CapabilityError::unknown(format!("durable capability recording failed: {error}"))
    }
    pub(crate) fn host_event(&self, kind: &str, data: Value) -> Result<(), CapabilityError> {
        self.inner
            .store
            .append_host_event(&self.lease, kind, data)
            .map_err(|e| self.persistence_failed(e))?;
        self.inner.changed.notify_waiters();
        Ok(())
    }
}
fn capability_error(error: RuntimeError) -> CapabilityError {
    CapabilityError::new(error.to_string())
}
#[async_trait]
impl cayw_host::PermissionGate for Gate {
    async fn authorize(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.scope.request.check(&self.scope.context)?;
        self.check(name, args, cancel).await.map_err(|error| {
            if matches!(
                error,
                RuntimeError::Journal(_)
                    | RuntimeError::Json(_)
                    | RuntimeError::Core(
                        cayw_core::CoreError::Database(_)
                            | cayw_core::CoreError::StorePoisoned
                            | cayw_core::CoreError::CorruptStore(_)
                    )
            ) {
                self.scope.context.persistence_failed(error)
            } else {
                capability_error(error)
            }
        })?;
        self.scope.request.check(&self.scope.context)
    }
}

struct DurableTool {
    inner: CellTool,
    context: Arc<RunContext>,
}
#[async_trait]
impl AgentTool for DurableTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }
    fn execution_mode(&self) -> Option<cayw_agent::ExecutionMode> {
        Some(cayw_agent::ExecutionMode::Sequential)
    }
    fn prepare_arguments(&self, args: Value) -> Result<Value, String> {
        self.inner.prepare_arguments(args)
    }
    fn shutdown_grace(&self) -> Duration {
        self.inner.shutdown_grace()
    }
    async fn execute(&self, input: ToolContext) -> Result<ToolResult, String> {
        self.context
            .check(&input.cancel)
            .map_err(|e| e.to_string())?;
        self.context
            .inner
            .store
            .mark_tool_started(&self.context.lease, &input.call_id)
            .map_err(|e| self.context.persistence_failed(e).to_string())?;
        self.context.inner.changed.notify_waiters();
        let waited_execution =
            if input.arguments.get("action").and_then(Value::as_str) == Some("wait") {
                let execution_id = input
                    .arguments
                    .get("cell_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "wait requires its original cell_id".to_string())?;
                let record = self
                    .inner
                    .manager()
                    .record(execution_id)
                    .map_err(|error| error.to_string())?;
                if record.identity != crate::cells::identity(&self.context) {
                    return Err("wait cell belongs to a different task attempt".into());
                }
                self.context
                    .inner
                    .store
                    .register_execution_wait(
                        &self.context.lease,
                        &input.call_id,
                        execution_id,
                        &self.context.task.id,
                        &input.arguments,
                    )
                    .map_err(|error| self.context.persistence_failed(error).to_string())?;
                self.context.inner.changed.notify_waiters();
                Some(execution_id.to_owned())
            } else {
                None
            };
        let parent_call_id = input.call_id.clone();
        let result = self.inner.execute(input).await;
        let mut result = result.unwrap_or_else(ToolResult::error);
        if self.context.unknown.load(Ordering::Acquire) {
            result.is_error = true;
            result.terminate = true;
            // Child effects have independent durable identities. A launch/wait
            // reply must not invent a second unknown effect for the same call.
            let recording_failed = lock(&self.context.persistence_error).is_some();
            let child_unknown = self
                .context
                .inner
                .store
                .snapshot(&self.context.lease.session_id)
                .is_ok_and(|snapshot| {
                    let dispatches = recording_failed
                        .then(|| {
                            crate::bridge::read_log(
                                &self.context.inner.store,
                                &self.context.lease.session_id,
                                &snapshot,
                            )
                            .ok()
                        })
                        .flatten();
                    snapshot.unresolved_capabilities.iter().any(|call| {
                        let unknown = matches!(
                            call.outcome,
                            Some(cayw_core::CapabilityOutcome::OutcomeUnknown { .. })
                        );
                        // Only a verified canonical dispatch can make an
                        // unfinished child the sole owner of its uncertainty.
                        // Consumer presence alone conveys no execution fact.
                        let bridge_child =
                            dispatches.as_ref().is_some_and(|log| log.tracks_call(call));
                        call.lease == self.context.lease
                            && call.task_id == self.context.task.id
                            && (call.parent_call_id == parent_call_id
                                || waited_execution.as_deref()
                                    == Some(call.key.execution_id.as_str()))
                            && ((!recording_failed && unknown)
                                || bridge_child
                                    && (unknown
                                        || call.stage == cayw_core::CapabilityStage::Started))
                    })
                });
            result.outcome_unknown = !child_unknown;
        }
        if result.outcome_unknown {
            self.context.unknown.store(true, Ordering::Release);
        }
        Ok(result)
    }
}

struct PromptGuard(CancellationToken);
impl Drop for PromptGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
struct ActiveGuard {
    inner: Arc<Inner>,
    session: String,
    task: TaskLease,
    _ownership: std::fs::File,
}
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut active = lock(&self.inner.active);
        if active
            .get(&self.session)
            .is_some_and(|run| run.task.id == self.task.id && run.task.worker == self.task.worker)
        {
            active.remove(&self.session);
        }
        drop(active);
        self.inner.changed.notify_waiters();
    }
}

impl Runtime {
    /// Persist input without starting a model. `resume` explicitly claims it.
    pub fn enqueue(&self, session: &str, text: &str) -> Result<TaskRecord, RuntimeError> {
        self.enqueue_with_intent(session, text, cayw_requests::TaskIntent::General)
    }
    /// Explicit user intent is supplied by trusted host code, never inferred from
    /// model arguments or the prompt's wording.
    pub fn enqueue_with_intent(
        &self,
        session: &str,
        text: &str,
        intent: cayw_requests::TaskIntent,
    ) -> Result<TaskRecord, RuntimeError> {
        crate::requests::validate_intent(&intent)?;
        if text.trim().is_empty() || text.len() > self.inner.config.max_prompt_bytes {
            return Err(RuntimeError::InvalidInput(
                "prompt is empty or exceeds byte budget".into(),
            ));
        }
        let _coordinate = self.inner.coordination.enter()?;
        self.check_ready(session)?;
        let task =
            self.inner
                .queue
                .enqueue(session, text, &intent, self.inner.config.max_queued_runs)?;
        self.inner.changed.notify_waiters();
        Ok(task)
    }
    /// Runs to a durable terminal. Dropping the caller cancels this worker; the
    /// owned cleanup continues to hold the persisted admission slot until done.
    pub async fn prompt(
        &self,
        session: &str,
        text: &str,
        cancel: CancellationToken,
    ) -> Result<SessionView, RuntimeError> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        let task = self.enqueue(session, text)?;
        self.resume(&task.id, cancel).await
    }
    pub async fn resume(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<SessionView, RuntimeError> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        let task = self.inner.queue.get(id)?;
        if task.state != TaskState::Queued {
            return Err(RuntimeError::InvalidInput(
                "only queued tasks can resume; running tasks require explicit recovery".into(),
            ));
        }
        let child = cancel.child_token();
        let unknown = Arc::new(AtomicBool::new(false));
        let (lease, ownership) = {
            let _coordinate = self.inner.coordination.enter()?;
            self.check_ready(&task.session_id)?;
            let mut active = lock(&self.inner.active);
            if active.contains_key(&task.session_id) {
                return Err(RuntimeError::Busy);
            }
            let ownership = self.inner.coordination.task(id)?;
            let lease = self.inner.queue.claim(id)?;
            active.insert(
                task.session_id.clone(),
                ActiveRun {
                    task: lease.clone(),
                    cancel: child.clone(),
                    unknown: unknown.clone(),
                },
            );
            (lease, ownership)
        };
        let guard = ActiveGuard {
            inner: self.inner.clone(),
            session: task.session_id.clone(),
            task: lease.clone(),
            _ownership: ownership,
        };
        let inner = self.inner.clone();
        let worker_cancel = child.clone();
        let worker = tokio::spawn(async move {
            let _active = guard;
            let outcome = AssertUnwindSafe(run_worker(
                inner.clone(),
                &task,
                &lease,
                worker_cancel.clone(),
                unknown.clone(),
            ))
            .catch_unwind()
            .await;
            let outcome = match outcome {
                Ok(result) => result,
                Err(_) => {
                    worker_cancel.cancel();
                    // Ownership may already have changed in another connection.
                    // Only explicit host recovery can invalidate an unfinished lease.
                    Err(RuntimeError::RecoveryRequired {
                        cause: Box::new(RuntimeError::Worker(
                            "worker panicked; explicitly recover and inspect persisted effects"
                                .into(),
                        )),
                        cleanup: None,
                        outcome_unknown: true,
                    })
                }
            };
            // Panic also drops VM callers. Stop new admissions before joining
            // the original independent process supervisors.
            let outcome = match crate::requests::closing(&inner, &lease.id) {
                Ok(()) => outcome,
                Err(cleanup) => Err(RuntimeError::RecoveryRequired {
                    cause: Box::new(outcome.err().unwrap_or_else(|| {
                        RuntimeError::Worker("request authority could not enter closing".into())
                    })),
                    cleanup: Some(Box::new(cleanup)),
                    outcome_unknown: true,
                }),
            };
            let outcome = match crate::processes::drain(&inner, &lease).await {
                Ok(()) => outcome,
                Err(cleanup) => Err(RuntimeError::RecoveryRequired {
                    cause: Box::new(outcome.err().unwrap_or_else(|| {
                        RuntimeError::Worker(
                            "process cleanup did not reach a confirmed terminal".into(),
                        )
                    })),
                    cleanup: Some(Box::new(cleanup)),
                    outcome_unknown: true,
                }),
            };
            if let Err(error @ RuntimeError::RecoveryRequired { .. }) = outcome {
                // Keep durable admission until an explicit recovery settles the
                // unfinished bookkeeping. A failed block write also leaves the
                // existing queued/running row occupying its original slot.
                if let Err(cleanup) = inner.queue.block(&lease, &error.to_string()) {
                    return Err(RuntimeError::RecoveryRequired {
                        outcome_unknown: matches!(
                            &error,
                            RuntimeError::RecoveryRequired {
                                outcome_unknown: true,
                                ..
                            }
                        ),
                        cause: Box::new(error),
                        cleanup: Some(Box::new(cleanup)),
                    });
                }
                return Err(error);
            }
            let unresolved = unknown.load(Ordering::Acquire)
                || outcome.as_ref().is_ok_and(|snapshot| {
                    snapshot.state == cayw_core::SessionState::NeedsReconciliation
                });
            let terminal = if unresolved {
                TaskTerminal::OutcomeUnknown
            } else if worker_cancel.is_cancelled() {
                TaskTerminal::Cancelled
            } else if outcome.is_err() {
                TaskTerminal::Failed
            } else {
                TaskTerminal::Completed
            };
            // Keep cancellation outside the whole synchronous settlement. Queue
            // rereads durable cancellation under its write transaction, then ends
            // the exact original Requests authority with that canonical decision.
            let settlement = (|| {
                let _coordinate = inner.coordination.enter()?;
                let original = inner.queue.get(&lease.id)?;
                inner.queue.finish(
                    &lease,
                    terminal,
                    outcome.as_ref().err().map(ToString::to_string),
                    |terminal| {
                        crate::requests::finish(&inner, &original, terminal.request_outcome())
                    },
                )
            })();
            // A fenced owner cannot free another owner's slot or replace a terminal.
            if let Err(error) = settlement {
                let error = RuntimeError::RecoveryRequired {
                    cause: Box::new(error),
                    cleanup: None,
                    outcome_unknown: unresolved,
                };
                if let Err(cleanup) = inner.queue.block(&lease, &error.to_string()) {
                    return Err(RuntimeError::RecoveryRequired {
                        cause: Box::new(error),
                        cleanup: Some(Box::new(cleanup)),
                        outcome_unknown: unresolved,
                    });
                }
                return Err(error);
            }
            outcome
        });
        let _cancel_on_drop = PromptGuard(child);
        worker
            .await
            .map_err(|e| RuntimeError::Worker(e.to_string()))?
    }
}

async fn admit(
    inner: &Arc<Inner>,
    task: &TaskLease,
    cancel: &CancellationToken,
) -> Result<(), RuntimeError> {
    let deadline = tokio::time::Instant::now() + inner.config.max_queue_time;
    loop {
        let notified = inner.changed.notified();
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        inner.queue.check(task)?;
        if inner.queue.admit(task, inner.config.max_concurrent_runs)? {
            inner.changed.notify_waiters();
            return Ok(());
        }
        tokio::select! { biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => return Err(RuntimeError::Budget("queue time")),
            _ = notified => {},
            _ = tokio::time::sleep(Duration::from_millis(20)) => {},
        }
    }
}

async fn run_worker(
    inner: Arc<Inner>,
    task: &TaskRecord,
    worker: &TaskLease,
    cancel: CancellationToken,
    unknown: Arc<AtomicBool>,
) -> Result<SessionView, RuntimeError> {
    admit(&inner, worker, &cancel).await?;
    if cancel.is_cancelled() {
        return Err(RuntimeError::Cancelled);
    }
    let lease = {
        let _coordinate = inner.coordination.enter()?;
        let rt = Runtime {
            inner: inner.clone(),
        };
        rt.check_ready(&task.session_id)?;
        inner.queue.check(worker)?;
        let lease = inner
            .store
            .begin_attempt(&task.session_id, &worker.worker)?;
        if let Err(error) = inner.queue.attach(worker, &lease.attempt_id) {
            let _ = inner.store.finish_attempt(&lease);
            return Err(error);
        }
        lease
    };
    let context = Arc::new(RunContext {
        inner: inner.clone(),
        lease: lease.clone(),
        task: worker.clone(),
        cancel: cancel.clone(),
        started: Instant::now(),
        tokens: AtomicU64::new(0),
        turns: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        bytes: AtomicUsize::new(0),
        unknown,
        persistence_error: Mutex::new(None),
    });
    let outcome = execute_agent(context.clone(), &task.text).await;
    // VM completion does not prove process cleanup. Leave the Core attempt and
    // persistent admission intact if a supervisor cannot establish its terminal.
    if let Err(cleanup) = crate::processes::drain(&inner, worker).await {
        cancel.cancel();
        return Err(RuntimeError::RecoveryRequired {
            cause: Box::new(outcome.err().unwrap_or_else(|| {
                RuntimeError::Worker("process cleanup did not reach a confirmed terminal".into())
            })),
            cleanup: Some(Box::new(cleanup)),
            outcome_unknown: true,
        });
    }
    // No pending approval survives the attempt. Cleanup errors retain the task
    // slot until recovery; no process can mistake this for permission to retry.
    let cleanup = (|| -> Result<(), RuntimeError> {
        let _coordinate = inner.coordination.enter()?;
        inner.store.validate_lease(&lease)?;
        for request in inner.store.snapshot(&task.session_id)?.pending_approvals {
            if request.attempt_id == lease.attempt_id {
                inner.store.decide_approval(
                    &task.session_id,
                    &request.id,
                    ApprovalDecision::Deny,
                    "runtime-task-ended",
                )?;
            }
        }
        inner
            .journal
            .revoke_session(&task.session_id, "task finished")?;
        // Requests stays Closing until process cleanup and canonical queue
        // settlement; peer cancellation may still be accepted after Core ends.
        crate::requests::closing(&inner, &task.id)?;
        // A failed leased write must never recover a possibly replaced owner.
        inner.store.finish_attempt(&lease)?;
        Ok(())
    })();
    inner.changed.notify_waiters();
    let recording_error = lock(&context.persistence_error).clone();
    let primary = recording_error
        .map(|error| RuntimeError::Worker(format!("durable execution recording failed: {error}")))
        .or_else(|| outcome.err());
    match (primary, cleanup) {
        (Some(cause), Err(cleanup)) => Err(RuntimeError::RecoveryRequired {
            cause: Box::new(cause),
            cleanup: Some(Box::new(cleanup)),
            outcome_unknown: context.unknown.load(Ordering::Acquire),
        }),
        (None, Err(cause)) => Err(RuntimeError::RecoveryRequired {
            cause: Box::new(cause),
            cleanup: None,
            outcome_unknown: context.unknown.load(Ordering::Acquire),
        }),
        (Some(cause), Ok(())) if lock(&context.persistence_error).is_some() => {
            Err(RuntimeError::RecoveryRequired {
                cause: Box::new(cause),
                cleanup: None,
                outcome_unknown: context.unknown.load(Ordering::Acquire),
            })
        }
        (Some(error), Ok(())) => Err(error),
        (None, Ok(())) => Runtime { inner }.get(&task.session_id),
    }
}

struct RequestFence(Arc<RunContext>);
#[async_trait]
impl AgentHooks for RequestFence {
    async fn prepare_request(
        &self,
        _: AgentContext,
        cancel: CancellationToken,
    ) -> Result<Option<AgentContext>, String> {
        self.0.check(&cancel).map_err(|e| e.to_string())?;
        Ok(None)
    }
}
async fn execute_agent(context: Arc<RunContext>, text: &str) -> Result<(), RuntimeError> {
    let inner = &context.inner;
    let backend = Arc::new(crate::cells::Backend::new(context.clone())?);
    let manager = CellManager::new(
        crate::cells::identity(&context),
        inner.cells.clone(),
        backend,
        inner.reads.clone(),
        inner.config.cells.clone(),
        inner.config.codemode.clone(),
    )
    .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
    let outcome = execute_with_cells(context.clone(), text, manager.clone()).await;
    // The model reply is not the lifetime of background code. Stop and join all
    // cells before this task can finish or release its persistent admission slot.
    let closing = crate::requests::closing(inner, &context.task.id).inspect_err(|error| {
        context.persistence_failed(error);
    });
    let closed = manager.close().await;
    if closed.outcome_unknown {
        context.unknown.store(true, Ordering::Release);
        context.cancel.cancel();
    }
    closing?;
    outcome
}
async fn execute_with_cells(
    context: Arc<RunContext>,
    text: &str,
    manager: Arc<CellManager>,
) -> Result<(), RuntimeError> {
    let inner = &context.inner;
    let tool = Arc::new(DurableTool {
        inner: CellTool::new(manager),
        context: context.clone(),
    });
    let agent = Agent::new(inner.provider.clone(), inner.model.clone());
    agent.replace_messages(inner.store.snapshot(&context.lease.session_id)?.messages)?;
    agent.set_tools(vec![tool])?;
    agent.set_hooks(Arc::new(RequestFence(context.clone())))?;
    agent.set_execution_mode(cayw_agent::ExecutionMode::Sequential)?;
    agent.set_max_turns(inner.config.max_turns)?;
    agent.set_stream_options(cayw_ai::StreamOptions {
        max_tokens: Some(
            inner
                .config
                .max_total_tokens
                .min(inner.model.max_output_tokens.unwrap_or(u64::MAX)),
        ),
        ..Default::default()
    })?;
    let listener_context = context.clone();
    agent.subscribe(move |event, agent_cancel| {
        let ctx = listener_context.clone();
        async move {
            // Persist terminal bookkeeping before applying budgets/cancellation.
            if let Err(error) = ctx.inner.store.append_agent_event(&ctx.lease, &event) {
                ctx.persistence_failed(&error);
                agent_cancel.cancel();
                return Err(error.to_string());
            }
            ctx.inner.changed.notify_waiters();
            if let AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            } = &event
            {
                let usage = &message.usage;
                let tokens = usage.total_tokens.max(
                    usage
                        .input
                        .saturating_add(usage.output)
                        .saturating_add(usage.cache_read)
                        .saturating_add(usage.cache_write),
                );
                let _ = ctx
                    .tokens
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                        Some(old.saturating_add(tokens))
                    });
            }
            if matches!(event, AgentEvent::TurnStart) {
                ctx.turns.fetch_add(1, Ordering::AcqRel);
            }
            if serde_json::to_vec(&event).map_err(|e| e.to_string())?.len()
                > ctx.inner.config.max_event_bytes
                || ctx.tokens.load(Ordering::Acquire) > ctx.inner.config.max_total_tokens
                || ctx.turns.load(Ordering::Acquire) > ctx.inner.config.max_turns
            {
                ctx.cancel.cancel();
                agent_cancel.cancel();
            }
            if ctx.cancel.is_cancelled() {
                agent_cancel.cancel();
            }
            Ok(())
        }
    });
    const INSTRUCTION: &str = "Use execute_code with action=exec, script, yield_time_ms and max_output_tokens for bounded Luau cells. A timed reply can leave the VM running: use action=wait with its cell_id, next_cursor, yield_time_ms and max_output_tokens until status is terminal and has_more is false. wait resumes yield_control unless continue_execution=false. Use tool_search(query,limit) or ALL_TOOLS to discover names and toolDefinition(name) for needed schemas. Invoke capabilities with cayw.call(name, arguments). Helpers store/load retain versioned session values; text, image, audio and notify produce typed output. return a JSON value or cayw.emit(value). Discovery is not permission. Every exact invocation needs host authorization. Never approve your own request or replay uncertain effects. No ambient shell, network or filesystem APIs are available.";
    let run = agent.prompt_messages(vec![Message::system(INSTRUCTION), Message::user(text)]);
    tokio::pin!(run);
    let deadline = tokio::time::Instant::now() + inner.config.max_run_time;
    let mut fence = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! { biased;
            result = &mut run => return result.map(|_| ()).map_err(Into::into),
            _ = context.cancel.cancelled() => { agent.control().abort(); return run.await.map(|_| ()).map_err(Into::into); },
            _ = tokio::time::sleep_until(deadline) => { context.cancel.cancel(); agent.control().abort(); return run.await.map(|_| ()).map_err(Into::into); },
            _ = fence.tick() => { if context.check(&context.cancel).is_err() { context.cancel.cancel(); } },
        }
    }
}

pub(crate) fn capabilities(inner: &Arc<Inner>) -> Result<Vec<ToolDefinition>, RuntimeError> {
    use cayw_codemode::CapabilityHost;
    let host = &inner.host_root;
    crate::requests::validate(inner, host)?;
    let mut catalog = host.capabilities();
    catalog.extend(crate::requests::definitions(inner));
    #[cfg(feature = "plugins")]
    for plugin in inner.registry.list() {
        catalog.extend(plugin.manifest.capabilities);
    }
    Ok(catalog)
}
