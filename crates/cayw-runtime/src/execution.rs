use crate::{
    ActiveRun, Inner, Runtime, RuntimeError, SessionView, TaskRecord, TaskState, lock,
    permissions::Gate,
    queue::{TaskLease, TaskTerminal},
};
use async_trait::async_trait;
use cayw_agent::{Agent, AgentContext, AgentEvent, AgentHooks, AgentTool, ToolContext, ToolResult};
use cayw_ai::{CancellationToken, Message, ToolDefinition};
use cayw_cells::{CellManager, CellTool};
use cayw_codemode::{CapabilityError, CapabilityStop};
use cayw_core::{ApprovalDecision, AttemptLease};
use futures::FutureExt;
use serde_json::Value;
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) struct RunContext {
    pub inner: Arc<Inner>,
    pub lease: AttemptLease,
    pub task: TaskLease,
    pub cancel: CancellationToken,
    pub scope: Arc<crate::children::RunScope>,
    pub(crate) activity: crate::scheduler::Activity,
    turns: AtomicUsize,
    pub unknown: Arc<AtomicBool>,
    /// A model turn or trusted capability stopped the execution with a known
    /// failure. The durable task must not claim Completed.
    pub failed: Arc<AtomicBool>,
    pub failed_reason: Arc<Mutex<Option<String>>>,
    cells_stopping: AtomicBool,
    persistence_error: Mutex<Option<String>>,
    pub(crate) model_recording_error: Mutex<Option<String>>,
}

#[cfg(test)]
impl RunContext {
    pub(crate) fn model_fixture(inner: Arc<Inner>, lease: AttemptLease, task: TaskLease) -> Self {
        Self {
            scope: Arc::new(crate::children::RunScope::root(&inner)),
            inner,
            lease,
            task,
            cancel: CancellationToken::new(),
            activity: Default::default(),
            turns: AtomicUsize::new(0),
            unknown: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            failed_reason: Arc::new(Mutex::new(None)),
            cells_stopping: AtomicBool::new(false),
            persistence_error: Mutex::new(None),
            model_recording_error: Mutex::new(None),
        }
    }
}

#[derive(Clone, Default)]
struct RunFlags {
    unknown: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    reason: Arc<Mutex<Option<String>>>,
}
impl RunContext {
    pub(crate) fn check(&self, cancel: &CancellationToken) -> Result<(), RuntimeError> {
        if self.cancel.is_cancelled() || cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if Instant::now() >= self.scope.deadline() {
            return Err(RuntimeError::Budget("time"));
        }
        self.scope.check()?;
        if self.turns.load(Ordering::Acquire) > self.inner.config.max_turns {
            return Err(RuntimeError::Budget("turn"));
        }
        if self.unknown.load(Ordering::Acquire) {
            return Err(RuntimeError::NeedsReconciliation);
        }
        if self.failed.load(Ordering::Acquire) {
            return Err(RuntimeError::Worker(
                lock(&self.failed_reason)
                    .clone()
                    .unwrap_or_else(|| "execution stopped with a known failure".into()),
            ));
        }
        if lock(&self.model_recording_error).is_some() {
            return Err(RuntimeError::Worker(
                "model accounting recording failed; retain the original reservation".into(),
            ));
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
        self.scope.admit_call(arguments)
    }
    pub(crate) fn charge_bytes(&self, value: &impl serde::Serialize) -> Result<(), RuntimeError> {
        self.scope.charge_bytes(value)
    }
    pub(crate) fn persistence_failed(&self, error: impl std::fmt::Display) -> CapabilityError {
        let error = error.to_string();
        lock(&self.persistence_error).get_or_insert_with(|| error.clone());
        self.unknown.store(true, Ordering::Release);
        self.cancel.cancel();
        CapabilityError::unknown(format!("durable capability recording failed: {error}"))
    }
    /// Model accounting uncertainty cannot assert or clear tool side effects.
    pub(crate) fn model_recording_failed(&self) {
        let reason =
            "durable model request recording failed; its budget reservation remains unsettled";
        lock(&self.model_recording_error).get_or_insert_with(|| reason.into());
        lock(&self.failed_reason).get_or_insert_with(|| reason.into());
        self.failed.store(true, Ordering::Release);
    }
    pub(crate) fn capability_stopped(&self, error: &CapabilityError) {
        if let Some(stop) = error.stop {
            // Runtime may stop cells for a deadline or after the run has its
            // outcome. Their cancellation must not replace that original cause.
            if stop == CapabilityStop::Cancelled && self.cells_stopping.load(Ordering::Acquire) {
                return;
            }
            lock(&self.failed_reason).get_or_insert_with(|| error.message.clone());
            self.failed.store(true, Ordering::Release);
            if stop == CapabilityStop::Cancelled {
                self.cancel.cancel();
            }
        }
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
pub(crate) fn capability_error(error: RuntimeError) -> CapabilityError {
    match error {
        RuntimeError::Cancelled => {
            CapabilityError::stopped(CapabilityStop::Cancelled, error.to_string())
        }
        RuntimeError::Budget(_) => {
            CapabilityError::stopped(CapabilityStop::Budget, error.to_string())
        }
        RuntimeError::NeedsReconciliation => CapabilityError::unknown(error.to_string()),
        _ => CapabilityError::new(error.to_string()),
    }
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
        let mut activity = crate::scheduler::Activity::model(&self.context, &input.cancel)
            .await
            .map_err(|error| error.to_string())?;
        if input.arguments.get("action").and_then(Value::as_str) == Some("wait") {
            activity.finish().map_err(|error| error.to_string())?;
        }
        self.context
            .check(&input.cancel)
            .map_err(|e| e.to_string())?;
        self.context
            .inner
            .store
            .bind_tool_origin(&self.context.lease, &input.call_id, &self.context.task.id)
            .map_err(|error| self.context.persistence_failed(error).to_string())?;
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
                // Preserve the model's original call in the log; only execution
                // receives the tool-owned numeric conversion.
                let original = self
                    .context
                    .inner
                    .store
                    .snapshot(&self.context.lease.session_id)
                    .map_err(|error| self.context.persistence_failed(error).to_string())?
                    .unresolved_calls
                    .into_iter()
                    .find(|call| call.call.id == input.call_id)
                    .ok_or_else(|| "wait requires its original started tool call".to_string())?;
                self.context
                    .inner
                    .store
                    .register_execution_wait(
                        &self.context.lease,
                        &input.call_id,
                        execution_id,
                        &self.context.task.id,
                        &original.call.arguments,
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
                    let storage_unknown = crate::results::owns_storage_uncertainty(
                        &self.context,
                        &snapshot,
                        &parent_call_id,
                        waited_execution.as_deref(),
                    )
                    .unwrap_or(false);
                    storage_unknown
                        || snapshot.unresolved_capabilities.iter().any(|call| {
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
        self.enqueue_input(
            session,
            crate::TaskInput {
                text: text.into(),
                images: vec![],
            },
        )
    }
    pub fn enqueue_input(
        &self,
        session: &str,
        input: crate::TaskInput,
    ) -> Result<TaskRecord, RuntimeError> {
        self.enqueue_input_with_intent(session, input, cayw_requests::TaskIntent::General)
    }
    /// Explicit user intent is supplied by trusted host code, never inferred from
    /// model arguments or the prompt's wording.
    pub fn enqueue_with_intent(
        &self,
        session: &str,
        text: &str,
        intent: cayw_requests::TaskIntent,
    ) -> Result<TaskRecord, RuntimeError> {
        self.enqueue_input_with_intent(
            session,
            crate::TaskInput {
                text: text.into(),
                images: vec![],
            },
            intent,
        )
    }
    pub fn enqueue_input_with_intent(
        &self,
        session: &str,
        input: crate::TaskInput,
        intent: cayw_requests::TaskIntent,
    ) -> Result<TaskRecord, RuntimeError> {
        self.enqueue_scheduled(session, input, intent, crate::TaskPriority::Normal)
    }
    /// Trusted host priority is fixed when the task is accepted. It is never
    /// inferred from model output, tool arguments or the prompt text.
    pub fn enqueue_scheduled(
        &self,
        session: &str,
        input: crate::TaskInput,
        intent: cayw_requests::TaskIntent,
        priority: crate::TaskPriority,
    ) -> Result<TaskRecord, RuntimeError> {
        self.enqueue_with_resources(
            session,
            input,
            intent,
            priority,
            crate::TaskResources::default(),
        )
    }
    /// Host-declared execution resources are fixed before acceptance. A model
    /// cannot change this task's declaration or require unsupported guarantees.
    pub fn enqueue_with_resources(
        &self,
        session: &str,
        input: crate::TaskInput,
        intent: cayw_requests::TaskIntent,
        priority: crate::TaskPriority,
        resources: crate::TaskResources,
    ) -> Result<TaskRecord, RuntimeError> {
        resources.validate(&self.inner.config.resources)?;
        crate::requests::validate_intent(&intent)?;
        input.validate(&self.inner.model, self.inner.config.max_prompt_bytes)?;
        let _coordinate = self.inner.coordination.enter()?;
        self.check_ready(session)?;
        let task = self.inner.queue.enqueue_with_resources(
            session,
            &input,
            &intent,
            priority,
            &resources,
            self.inner.config.max_queued_runs,
        )?;
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
        self.prompt_input(
            session,
            crate::TaskInput {
                text: text.into(),
                images: vec![],
            },
            cancel,
        )
        .await
    }
    pub async fn prompt_input(
        &self,
        session: &str,
        input: crate::TaskInput,
        cancel: CancellationToken,
    ) -> Result<SessionView, RuntimeError> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        let task = self.enqueue_input(session, input)?;
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
        let flags = RunFlags::default();
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
                    unknown: flags.unknown.clone(),
                },
            );
            (lease, ownership)
        };
        launch_worker(
            self.inner.clone(),
            task,
            lease,
            ownership,
            child,
            flags,
            None,
        )
        .await
    }
}

async fn launch_worker(
    inner: Arc<Inner>,
    task: TaskRecord,
    lease: TaskLease,
    ownership: std::fs::File,
    child: CancellationToken,
    flags: RunFlags,
    scope: Option<Arc<crate::children::RunScope>>,
) -> Result<SessionView, RuntimeError> {
    let guard = ActiveGuard {
        inner: inner.clone(),
        session: task.session_id.clone(),
        task: lease.clone(),
        _ownership: ownership,
    };
    let worker_cancel = child.clone();
    let worker = tokio::spawn(async move {
        let _active = guard;
        let outcome = AssertUnwindSafe(run_worker(
            inner.clone(),
            &task,
            &lease,
            worker_cancel.clone(),
            flags.clone(),
            scope,
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
                        "worker panicked; explicitly recover and inspect persisted effects".into(),
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
        let unresolved = flags.unknown.load(Ordering::Acquire)
            || outcome.as_ref().is_ok_and(|snapshot| {
                snapshot.state == cayw_core::SessionState::NeedsReconciliation
            });
        let terminal = if unresolved {
            TaskTerminal::OutcomeUnknown
        } else if worker_cancel.is_cancelled() {
            TaskTerminal::Cancelled
        } else if outcome.is_err() || flags.failed.load(Ordering::Acquire) {
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
                outcome
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .or_else(|| lock(&flags.reason).clone()),
                |terminal| crate::requests::finish(&inner, &original, terminal.request_outcome()),
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

pub(crate) async fn resume_child(
    runtime: &Runtime,
    id: &str,
    scope: Arc<crate::children::RunScope>,
    cancel: CancellationToken,
) -> Result<SessionView, RuntimeError> {
    let task = runtime.inner.queue.get(id)?;
    if task.state != TaskState::Running || task.attempt_id.is_some() {
        return Err(RuntimeError::InvalidInput(
            "child task is not newly admitted".into(),
        ));
    }
    let child = cancel.child_token();
    let flags = RunFlags::default();
    let lease = TaskLease {
        id: task.id.clone(),
        worker: task
            .worker_id
            .clone()
            .ok_or_else(|| RuntimeError::Worker("child task has no worker".into()))?,
        fence: task.fence,
    };
    let ownership = {
        let _coordinate = runtime.inner.coordination.enter()?;
        runtime.check_ready(&task.session_id)?;
        let mut active = lock(&runtime.inner.active);
        if active.contains_key(&task.session_id) {
            return Err(RuntimeError::Busy);
        }
        let ownership = runtime.inner.coordination.task(id)?;
        active.insert(
            task.session_id.clone(),
            ActiveRun {
                task: lease.clone(),
                cancel: child.clone(),
                unknown: flags.unknown.clone(),
            },
        );
        ownership
    };
    Box::pin(launch_worker(
        runtime.inner.clone(),
        task,
        lease,
        ownership,
        child,
        flags,
        Some(scope),
    ))
    .await
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
        let admitted = {
            let _coordinate = inner.coordination.enter()?;
            inner.queue.admit(task, inner.config.max_concurrent_runs)?
        };
        if admitted {
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
    flags: RunFlags,
    scope: Option<Arc<crate::children::RunScope>>,
) -> Result<SessionView, RuntimeError> {
    if scope.is_none() {
        admit(&inner, worker, &cancel).await?;
    }
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
        scope: scope.unwrap_or_else(|| Arc::new(crate::children::RunScope::root(&inner))),
        activity: Default::default(),
        turns: AtomicUsize::new(0),
        unknown: flags.unknown,
        failed: flags.failed,
        failed_reason: flags.reason,
        cells_stopping: AtomicBool::new(false),
        persistence_error: Mutex::new(None),
        model_recording_error: Mutex::new(None),
    });
    let outcome = execute_agent(context.clone(), &task.input).await;
    if let Err(cleanup) = crate::children::drain(&context).await {
        cancel.cancel();
        return Err(RuntimeError::RecoveryRequired {
            cause: Box::new(outcome.err().unwrap_or_else(|| {
                RuntimeError::Worker("child cleanup did not reach a confirmed terminal".into())
            })),
            cleanup: Some(Box::new(cleanup)),
            outcome_unknown: true,
        });
    }
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
        if inner.journal.decision_pending(&task.id)? {
            return Err(RuntimeError::RecoveryRequired {
                cause: Box::new(RuntimeError::Worker(
                    "approval decision did not commit; explicit recovery required".into(),
                )),
                cleanup: None,
                outcome_unknown: true,
            });
        }
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
    let recording_error = lock(&context.persistence_error)
        .clone()
        .or_else(|| lock(&context.model_recording_error).clone());
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

struct RequestFence {
    context: Arc<RunContext>,
    completion: Arc<crate::completion::Completion>,
    rejection: Arc<Mutex<Option<RuntimeError>>>,
}
#[async_trait]
impl AgentHooks for RequestFence {
    async fn prepare_request(
        &self,
        _: AgentContext,
        cancel: CancellationToken,
    ) -> Result<Option<AgentContext>, String> {
        self.context.check(&cancel).map_err(|e| e.to_string())?;
        Ok(None)
    }
    async fn finish_turn(
        &self,
        turn: cayw_agent::TurnContext,
        cancel: CancellationToken,
    ) -> Result<cayw_agent::TurnDecision, String> {
        for result in &turn.tool_results {
            if let Err(error) = self.completion.observe(result) {
                let reason = error.to_string();
                *lock(&self.rejection) = Some(error);
                return Err(reason);
            }
        }
        if turn.assistant.tool_calls().is_empty() && !turn.assistant.stop_reason.is_failure() {
            self.context
                .check(&cancel)
                .map_err(|error| error.to_string())?;
            if let Err(error) = self.completion.check() {
                // The Agent turns this hook error into a failure message. Keep
                // the real reason so the caller still sees a rejected
                // completion instead of an ordinary model failure.
                let reason = error.to_string();
                *lock(&self.rejection) = Some(error);
                return Err(reason);
            }
        }
        Ok(cayw_agent::TurnDecision::Auto)
    }
}
async fn execute_agent(
    context: Arc<RunContext>,
    input: &crate::TaskInput,
) -> Result<(), RuntimeError> {
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
    let outcome = execute_with_cells(context.clone(), input, manager.clone()).await;
    // The model reply is not the lifetime of background code. Stop and join all
    // cells before this task can finish or release its persistent admission slot.
    let closing = crate::requests::closing(inner, &context.task.id).inspect_err(|error| {
        context.persistence_failed(error);
    });
    context.cells_stopping.store(true, Ordering::Release);
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
    input: &crate::TaskInput,
    manager: Arc<CellManager>,
) -> Result<(), RuntimeError> {
    let inner = &context.inner;
    let completion = Arc::new(crate::completion::Completion::new(
        context.clone(),
        manager.clone(),
    ));
    let tool = Arc::new(DurableTool {
        inner: CellTool::new(manager),
        context: context.clone(),
    });
    let agent = Agent::new(
        Arc::new(crate::model_requests::AccountedProvider::new(
            context.clone(),
        )),
        inner.model.clone(),
    );
    // FleetView intervention: steer/stop need the live Agent control.
    inner
        .fleet
        .attach_control(&context.lease.session_id, agent.control());
    let mut messages = Runtime {
        inner: inner.clone(),
    }
    .continuation_messages(&context.lease.session_id)?;
    // Canonicalize only the exact instruction previously emitted by Runtime.
    // This provider view leaves the original durable events and host rules intact.
    for message in &mut messages {
        if let Message::System(system) = message
            && system.content == INSTRUCTION
        {
            system.content.clear();
            system
                .sections
                .insert("cayw-runtime-codemode".into(), Some(INSTRUCTION.into()));
        }
    }
    agent.replace_messages(messages)?;
    agent.set_tools(vec![tool])?;
    let rejection = Arc::new(Mutex::new(None));
    agent.set_hooks(Arc::new(RequestFence {
        context: context.clone(),
        completion: completion.clone(),
        rejection: rejection.clone(),
    }))?;
    agent.set_execution_mode(cayw_agent::ExecutionMode::Sequential)?;
    agent.set_max_turns(inner.config.max_turns)?;
    agent.set_stream_options(cayw_ai::StreamOptions {
        max_tokens: Some(
            context
                .scope
                .token_limit()
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
            if matches!(event, AgentEvent::TurnStart) {
                ctx.turns.fetch_add(1, Ordering::AcqRel);
            }
            if serde_json::to_vec(&event).map_err(|e| e.to_string())?.len()
                > ctx.inner.config.max_event_bytes
                || ctx.scope.tokens().load(Ordering::Acquire) > ctx.scope.token_limit()
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
    let run = async {
        let instruction = Message::System(cayw_ai::SystemMessage {
            sections: [("cayw-runtime-codemode".into(), Some(INSTRUCTION.into()))].into(),
            ..Default::default()
        });
        // F08: fold Applied plannotator feedback into the next Agent attempt.
        let mut prompt = vec![instruction];
        prompt.extend(crate::plannotator::feedback_messages_for_attempt(
            inner,
            &context.lease.session_id,
        )?);
        prompt.push(input.message());
        let result = agent.prompt_messages(prompt).await?;
        // The Agent reports hook rejections as failed model turns. Restore the
        // completion decision the hook actually made.
        if let Some(error) = lock(&rejection).take() {
            return Err(error);
        }
        // A terminating tool can make the Agent finish before the lifecycle
        // check observes its known failure. Keep the same cause in either order.
        if context.failed.load(Ordering::Acquire)
            && !context.cancel.is_cancelled()
            && !context.unknown.load(Ordering::Acquire)
        {
            context.check(&context.cancel)?;
        }
        // A failed model turn is not a completion claim. Cancellation, budget
        // and uncertain effects settle through the worker and durable queue;
        // other model/provider failures stay visible in the session view. Do
        // not replace them with a generic worker error.
        if result.last_message.stop_reason.is_failure() {
            if !context.cancel.is_cancelled() && !context.unknown.load(Ordering::Acquire) {
                *lock(&context.failed_reason) = Some(
                    result
                        .last_message
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "model run did not finish".into()),
                );
                context.failed.store(true, Ordering::Release);
            }
            return Ok(());
        }
        context.check(&context.cancel)?;
        completion.check()
    };
    let result = supervise_run(
        run,
        &context.cancel,
        tokio::time::Instant::from_std(context.scope.deadline()),
        || async { context.check(&context.cancel) },
        || {
            context.cells_stopping.store(true, Ordering::Release);
            agent.control().abort();
        },
    )
    .await;
    inner.fleet.clear_control(&context.lease.session_id);
    result
}

async fn supervise_run<F, C, CF, A>(
    run: F,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
    mut check: C,
    mut abort: A,
) -> Result<(), RuntimeError>
where
    F: std::future::Future<Output = Result<(), RuntimeError>>,
    C: FnMut() -> CF,
    CF: std::future::Future<Output = Result<(), RuntimeError>>,
    A: FnMut(),
{
    tokio::pin!(run);
    let mut fence = tokio::time::interval(Duration::from_millis(25));
    // The absolute deadline is a budget outcome, not a user cancellation. It
    // aborts the model turn without setting the user token so the durable task
    // keeps the budget reason instead of a generic Cancelled terminal.
    let mut expired = false;
    loop {
        tokio::select! { biased;
            result = &mut run => {
                return match result {
                    Ok(()) if expired || tokio::time::Instant::now() >= deadline => {
                        Err(RuntimeError::Budget("time"))
                    }
                    other => other,
                };
            }
            _ = cancel.cancelled() => { abort(); return run.await; },
            _ = tokio::time::sleep_until(deadline) => {
                abort();
                return match run.await {
                    Ok(()) => Err(RuntimeError::Budget("time")),
                    Err(error) => Err(error),
                };
            }
            _ = fence.tick() => {
                if let Err(error) = check().await {
                    if matches!(error, RuntimeError::Budget("time")) {
                        expired = true;
                        abort();
                    } else if matches!(error, RuntimeError::Cancelled) {
                        cancel.cancel();
                    } else {
                        // A known stop must keep its cause after the Agent has
                        // joined. The user cancellation token is a separate fact.
                        abort();
                        return match run.await {
                            Err(cleanup @ RuntimeError::RecoveryRequired { .. }) => Err(cleanup),
                            _ => Err(error),
                        };
                    }
                }
                // A slow persistent check must not queue catch-up work ahead of
                // the model stream. Start the next interval after it completes.
                fence.reset();
            },
        }
    }
}

pub(crate) fn capabilities(inner: &Arc<Inner>) -> Result<Vec<ToolDefinition>, RuntimeError> {
    let host = &inner.host_root;
    crate::requests::validate(inner, host)?;
    let mut catalog = crate::requests::operation_definitions(inner, host);
    catalog.extend(crate::requests::definitions(inner));
    #[cfg(feature = "plugins")]
    for plugin in inner.registry.list() {
        catalog.extend(plugin.manifest.capabilities);
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // Break caught: overdue checks run again before a response that becomes ready
    // during the next normal checking interval. Only virtual time is advanced.
    #[tokio::test(start_paused = true)]
    async fn slow_lifecycle_check_does_not_catch_up_overdue_ticks() {
        let checks = Cell::new(0);
        let started = tokio::time::Instant::now();
        let run = async {
            tokio::time::sleep_until(started + Duration::from_millis(120)).await;
            Ok(())
        };
        supervise_run(
            run,
            &CancellationToken::new(),
            started + Duration::from_secs(10),
            || async {
                checks.set(checks.get() + 1);
                tokio::time::advance(Duration::from_millis(100)).await;
                Ok(())
            },
            || panic!("a valid response must not be cancelled"),
        )
        .await
        .unwrap();
        assert_eq!(
            checks.get(),
            1,
            "overdue lifecycle checks must not run back-to-back"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_after_a_slow_check_still_aborts_the_run() {
        let cancel = CancellationToken::new();
        let finished = CancellationToken::new();
        let run = async {
            finished.cancelled().await;
            Ok(())
        };
        let checks = Cell::new(0);
        supervise_run(
            run,
            &cancel,
            tokio::time::Instant::now() + Duration::from_secs(10),
            || async {
                checks.set(checks.get() + 1);
                tokio::time::advance(Duration::from_millis(100)).await;
                cancel.cancel();
                Ok(())
            },
            || finished.cancel(),
        )
        .await
        .unwrap();
        assert!(finished.is_cancelled());
        assert_eq!(checks.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn known_lifecycle_failure_joins_without_becoming_user_cancellation() {
        for failure in [
            RuntimeError::Budget("turn"),
            RuntimeError::Worker("host call or total byte budget exhausted".into()),
        ] {
            let expected = failure.to_string();
            let mut failure = Some(failure);
            let cancel = CancellationToken::new();
            let stopped = CancellationToken::new();
            let joined = Cell::new(false);
            let error = supervise_run(
                async {
                    stopped.cancelled().await;
                    joined.set(true);
                    Ok(())
                },
                &cancel,
                tokio::time::Instant::now() + Duration::from_secs(10),
                || std::future::ready(Err(failure.take().unwrap())),
                || stopped.cancel(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert!(joined.get());
            assert!(!cancel.is_cancelled());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_lifecycle_check_cancels_and_joins_the_run() {
        let cancel = CancellationToken::new();
        let finished = CancellationToken::new();
        supervise_run(
            async {
                finished.cancelled().await;
                Ok(())
            },
            &cancel,
            tokio::time::Instant::now() + Duration::from_secs(10),
            || async { Err(RuntimeError::Cancelled) },
            || finished.cancel(),
        )
        .await
        .unwrap();
        assert!(cancel.is_cancelled());
        assert!(finished.is_cancelled());
    }
}
