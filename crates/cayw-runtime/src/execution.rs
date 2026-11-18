use crate::{
    ActiveRun, Inner, Runtime, RuntimeError, SessionView, TaskRecord, TaskState, lock,
    permissions::Gate,
    queue::{TaskLease, TaskTerminal},
};
use async_trait::async_trait;
use cayw_agent::{
    Agent, AgentContext, AgentControl, AgentEvent, AgentHooks, AgentTool, ToolContext, ToolResult,
};
use cayw_ai::{CancellationToken, Message, ToolDefinition};
use cayw_cells::{CellManager, CellTool};
use cayw_codemode::{CapabilityError, CapabilityStop};
use cayw_core::{ApprovalDecision, AttemptLease};
use futures::FutureExt;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) struct RunContext {
    pub inner: Arc<Inner>,
    pub selection: crate::ConfiguredProvider,
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
    pub(crate) cells_manager: Mutex<Option<std::sync::Weak<CellManager>>>,
    persistence_error: Mutex<Option<String>>,
    pub(crate) model_recording_error: Mutex<Option<String>>,
    /// A user task on a main-session Runtime that already has a workspace router.
    /// A dispatched worker and a Runtime without a router stay false.
    pub(crate) routed_user_turn: bool,
    /// This turn has called `work.list`. Create and dispatch before that are refused.
    pub(crate) work_listed: AtomicBool,
    /// WorkItem ids created in this turn. Dispatching any other id reuses existing work.
    created_work_ids: Mutex<BTreeSet<String>>,
    work_created: AtomicBool,
    work_dispatched: AtomicBool,
    /// A successful dispatch used a WorkItem this turn did not create.
    work_reused: AtomicBool,
    /// This turn read one WorkItem with work.get or work.tasks. That is enough
    /// when the user is asking for status; it does not dispatch.
    work_status_read: AtomicBool,
    route_reminded: AtomicBool,
    route_notice_sent: AtomicBool,
}

#[cfg(test)]
impl RunContext {
    pub(crate) fn model_fixture(inner: Arc<Inner>, lease: AttemptLease, task: TaskLease) -> Self {
        let selection = lock(&inner.selection).clone();
        Self {
            selection,
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
            cells_manager: Mutex::new(None),
            persistence_error: Mutex::new(None),
            model_recording_error: Mutex::new(None),
            routed_user_turn: false,
            work_listed: AtomicBool::new(false),
            created_work_ids: Mutex::new(BTreeSet::new()),
            work_created: AtomicBool::new(false),
            work_dispatched: AtomicBool::new(false),
            work_reused: AtomicBool::new(false),
            work_status_read: AtomicBool::new(false),
            route_reminded: AtomicBool::new(false),
            route_notice_sent: AtomicBool::new(false),
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
    pub(crate) fn note_work_listed(&self) {
        self.work_listed.store(true, Ordering::Release);
    }

    pub(crate) fn note_work_created(&self, work_id: &str) {
        lock(&self.created_work_ids).insert(work_id.to_string());
        self.work_created.store(true, Ordering::Release);
    }

    pub(crate) fn note_work_dispatched(&self, work_id: &str) {
        if !lock(&self.created_work_ids).contains(work_id) {
            self.work_reused.store(true, Ordering::Release);
        }
        self.work_dispatched.store(true, Ordering::Release);
    }

    pub(crate) fn note_work_status(&self) {
        self.work_status_read.store(true, Ordering::Release);
    }

    /// A routed user turn is unfinished until existing work was listed and one
    /// WorkItem was dispatched, or until this turn read one WorkItem's status.
    /// A new `work.create` is required only when this turn did not reuse one.
    /// Reading status does not dispatch.
    pub(crate) fn route_pending(&self) -> bool {
        self.routed_user_turn
            && !self.work_status_read.load(Ordering::Acquire)
            && !(self.work_listed.load(Ordering::Acquire)
                && self.work_dispatched.load(Ordering::Acquire)
                && (self.work_created.load(Ordering::Acquire)
                    || self.work_reused.load(Ordering::Acquire)))
    }

    pub(crate) fn check(&self, cancel: &CancellationToken) -> Result<(), RuntimeError> {
        if self.cancel.is_cancelled() || cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if self
            .scope
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(RuntimeError::Budget("time"));
        }
        self.scope.check()?;
        if self
            .inner
            .config
            .max_turns
            .is_some_and(|max_turns| self.turns.load(Ordering::Acquire) > max_turns)
        {
            return Err(RuntimeError::Budget("turn"));
        }
        if let Some(error) = lock(&self.persistence_error).clone() {
            return Err(RuntimeError::Worker(format!(
                "durable execution recording failed: {error}"
            )));
        }
        if self.unknown.load(Ordering::Acquire) {
            return Err(RuntimeError::Cancelled);
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
    /// A pre-effect write or check keeps its ordinary error unless the durable
    /// store itself is doubtful. Validation, limits and serialization cannot
    /// claim that an external effect may have happened.
    pub(crate) fn record_error(&self, error: RuntimeError) -> CapabilityError {
        if durable_failure(&error) {
            self.persistence_failed(error)
        } else {
            capability_error(error)
        }
    }

    /// A known store rejection before an uncertain write stays a normal tool
    /// error. Only a database, poisoned, or corrupt store can hide whether the
    /// write landed.
    fn recorded_store_error(&self, error: cayw_core::CoreError) -> String {
        if store_effect_uncertain(&error) {
            self.persistence_failed(error).to_string()
        } else {
            error.to_string()
        }
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
            .map_err(|error| self.record_error(error.into()))?;
        self.inner.changed.notify_waiters();
        Ok(())
    }
}
/// True only when a write may have committed without a durable receipt. Every
/// other error is a known refusal that must not freeze the session.
pub(crate) fn store_effect_uncertain(error: &cayw_core::CoreError) -> bool {
    matches!(
        error,
        cayw_core::CoreError::Database(_)
            | cayw_core::CoreError::StorePoisoned
            | cayw_core::CoreError::CorruptStore(_)
    )
}

pub(crate) fn durable_failure(error: &RuntimeError) -> bool {
    match error {
        RuntimeError::Journal(_) => true,
        RuntimeError::Core(error) => store_effect_uncertain(error),
        _ => false,
    }
}
/// Core rejects these before any SQLite write commits, so the refusal is a
/// known limit rather than an uncertain side effect.
fn is_pre_write_refusal(error: &cayw_core::CoreError) -> bool {
    matches!(
        error,
        cayw_core::CoreError::InvalidInput(message)
            if message == "Metadata byte limit exceeded"
                || message == "Metadata nesting limit exceeded"
                || message == "Event byte limit exceeded"
    )
}
pub(crate) fn capability_error(error: RuntimeError) -> CapabilityError {
    match error {
        RuntimeError::Cancelled => {
            CapabilityError::stopped(CapabilityStop::Cancelled, error.to_string())
        }
        RuntimeError::Budget(_) => {
            CapabilityError::stopped(CapabilityStop::Budget, error.to_string())
        }
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
        self.check(name, args, cancel)
            .await
            .map_err(|error| self.scope.context.record_error(error))?;
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
            .map_err(|error| self.context.recorded_store_error(error))?;
        self.context
            .inner
            .store
            .mark_tool_started(&self.context.lease, &input.call_id)
            .map_err(|error| self.context.recorded_store_error(error))?;
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
                    .map_err(|error| self.context.recorded_store_error(error))?
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
                    .map_err(|error| self.context.recorded_store_error(error))?;
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
        if !result.terminate
            && self.context.inner.config.approval_mode == crate::ApprovalMode::ModelAuto
        {
            let pending: Vec<_> = self
                .context
                .inner
                .store
                .snapshot(&self.context.lease.session_id)
                .map_err(|error| self.context.recorded_store_error(error))?
                .pending_approvals
                .into_iter()
                .filter(|approval| approval.input.task_id == self.context.task.id)
                .collect();
            if !pending.is_empty() {
                result.content.push(cayw_ai::Content::text(format!(
                    "The original cell is waiting for your approval decision. Before waiting again, use a new execute_code cell to call cayw.call('approval.pending', {{}}), review the exact scope, then call approval.decide with that approval_id and a decision of allow_once, allow_task or deny. Then wait for the original cell; do not replay it. Pending approvals: {}",
                    serde_json::to_string(&pending).map_err(|error| error.to_string())?
                )));
            }
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
        if self.inner.config.main_session {
            self.inner.due_checks.notify_waiters();
        }
    }
}

/// Removes the live agent control when the model turn ends, including on panic.
struct SessionControlGuard {
    inner: Arc<Inner>,
    session: String,
}

impl SessionControlGuard {
    fn attach(inner: &Arc<Inner>, session: String, control: AgentControl) -> Self {
        lock(&inner.session_controls).insert(session.clone(), control);
        Self {
            inner: inner.clone(),
            session,
        }
    }
}

impl Drop for SessionControlGuard {
    fn drop(&mut self) {
        lock(&self.inner.session_controls).remove(&self.session);
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
        input.validate(
            &lock(&self.inner.selection).model,
            self.inner.config.max_prompt_bytes,
        )?;
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
    /// Create a task with its `work_id` and `dispatch_key` in the same durable
    /// transaction. A repeated key with the same input, session and work_id
    /// returns the original task; a different identity is a conflict.
    pub fn enqueue_dispatched(
        &self,
        session: &str,
        input: crate::TaskInput,
        intent: cayw_requests::TaskIntent,
        priority: crate::TaskPriority,
        resources: crate::TaskResources,
        dispatch: crate::TaskDispatch,
    ) -> Result<TaskRecord, RuntimeError> {
        resources.validate(&self.inner.config.resources)?;
        input.validate(
            &lock(&self.inner.selection).model,
            self.inner.config.max_prompt_bytes,
        )?;
        dispatch.validate()?;
        // A retry after a lost response must return the original task even if
        // that task already started and the session is now busy. The durable
        // queue repeats this check inside the create transaction for races.
        if let Some(existing) = self
            .inner
            .queue
            .find_by_dispatch_key(&dispatch.dispatch_key)?
        {
            if existing.input != input
                || existing.session_id != session
                || existing.work_id.as_deref() != Some(dispatch.work_id.as_str())
            {
                return Err(RuntimeError::InvalidInput(
                    "dispatch_key was already used for different input, session, or work_id".into(),
                ));
            }
            return Ok(existing);
        }
        let _coordinate = self.inner.coordination.enter()?;
        self.check_ready(session)?;
        let task = self.inner.queue.enqueue_dispatched_with_resources(
            session,
            &input,
            &intent,
            priority,
            &resources,
            self.inner.config.max_queued_runs,
            Some(&dispatch),
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

    /// Claim and start a queued task on the current Tokio runtime without tying
    /// its lifetime to a caller's RPC stream. Shutdown of the owning Runtime
    /// cancels the background worker cleanly.
    pub fn start_queued(&self, id: &str) -> Result<TaskRecord, RuntimeError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            RuntimeError::Worker(
                "background task execution requires an active Tokio runtime".into(),
            )
        })?;
        let task = self.inner.queue.get(id)?;
        if task.state != TaskState::Queued {
            return Ok(task);
        }
        let child = self.inner.maintenance_cancel.child_token();
        let flags = RunFlags::default();
        let (lease, ownership) = {
            let _coordinate = self.inner.coordination.enter()?;
            self.check_ready(&task.session_id)?;
            let current = self.inner.queue.get(id)?;
            if current.state != TaskState::Queued {
                return Ok(current);
            }
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
        let task_id = task.id.clone();
        let _worker = spawn_worker_task(
            &handle,
            self.inner.clone(),
            task,
            lease,
            ownership,
            child,
            flags,
            None,
        );
        self.inner.changed.notify_waiters();
        self.inner.queue.get(&task_id)
    }

    /// Deliver one failed program check to a session in this workspace.
    /// `sessions` is newest first. A running session is steered and no second
    /// task is created. Otherwise the newest session gets one new task, unless
    /// it already has a live task: that stays `Busy` and the caller keeps the
    /// acceptance record. This does not run the check.
    pub fn deliver_session_notice(
        &self,
        notice: &crate::WorkSessionNotice,
    ) -> Result<crate::WorkSessionNoticeReply, RuntimeError> {
        if notice.sessions.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "a session notice needs a session".into(),
            ));
        }
        {
            let controls = lock(&self.inner.session_controls);
            for session_id in &notice.sessions {
                if let Some(control) = controls.get(session_id) {
                    control.steer(Message::runtime(&notice.text));
                    return Ok(crate::WorkSessionNoticeReply {
                        delivered: true,
                        steered: true,
                        session_id: Some(session_id.clone()),
                        task_id: None,
                        reason: None,
                    });
                }
            }
        }
        let session_id = &notice.sessions[0];
        match self.enqueue_dispatched(
            session_id,
            crate::TaskInput {
                text: notice.text.clone(),
                images: Vec::new(),
            },
            cayw_requests::TaskIntent::General,
            crate::TaskPriority::Normal,
            crate::TaskResources::default(),
            crate::TaskDispatch {
                work_id: notice.work_id.clone(),
                dispatch_key: notice.dispatch_key.clone(),
                pause_version: notice.pause_version,
                stop_version: notice.stop_version,
            },
        ) {
            Ok(task) => {
                let task = if task.state == TaskState::Queued {
                    self.start_queued(&task.id)?
                } else {
                    task
                };
                Ok(crate::WorkSessionNoticeReply {
                    delivered: true,
                    steered: false,
                    session_id: Some(session_id.clone()),
                    task_id: Some(task.id),
                    reason: None,
                })
            }
            Err(RuntimeError::Busy) => Ok(crate::WorkSessionNoticeReply {
                delivered: false,
                steered: false,
                session_id: Some(session_id.clone()),
                task_id: None,
                reason: Some(
                    "this session already has a live task, so the notice was not delivered".into(),
                ),
            }),
            Err(error) => Err(error),
        }
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
    let handle = tokio::runtime::Handle::current();
    let worker = spawn_worker_task(
        &handle,
        inner,
        task,
        lease,
        ownership,
        child.clone(),
        flags,
        scope,
    );
    let _cancel_on_drop = PromptGuard(child);
    worker
        .await
        .map_err(|error| RuntimeError::RecoveryRequired {
            cause: Box::new(RuntimeError::WorkerLost(error)),
            cleanup: None,
            outcome_unknown: true,
        })?
}

#[allow(clippy::too_many_arguments)]
fn spawn_worker_task(
    handle: &tokio::runtime::Handle,
    inner: Arc<Inner>,
    task: TaskRecord,
    lease: TaskLease,
    ownership: std::fs::File,
    worker_cancel: CancellationToken,
    flags: RunFlags,
    scope: Option<Arc<crate::children::RunScope>>,
) -> tokio::task::JoinHandle<Result<SessionView, RuntimeError>> {
    let flush = FlushPending {
        inner: inner.clone(),
        session: task.session_id.clone(),
    };
    let guard = ActiveGuard {
        inner: inner.clone(),
        session: task.session_id.clone(),
        task: lease.clone(),
        _ownership: ownership,
    };
    handle.spawn(async move {
        // Flush is declared first so it runs after the session leaves `active`.
        let _flush = flush;
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
            Err(payload) => {
                worker_cancel.cancel();
                let reason = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("non-string panic payload");
                // Ownership may already have changed in another connection.
                // Only explicit host recovery can invalidate an unfinished lease.
                Err(RuntimeError::RecoveryRequired {
                    cause: Box::new(RuntimeError::WorkerPanicked(
                        reason.escape_debug().to_string(),
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
            if let Err(cleanup) = inner
                .queue
                .block(&lease, &error.to_string())
                .map_err(RuntimeError::from)
            {
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
        let unresolved = flags.unknown.load(Ordering::Acquire);
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
                |terminal| {
                    crate::requests::finish(&inner, &original, crate::requests::outcome(terminal))
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
            if let Err(cleanup) = inner
                .queue
                .block(&lease, &error.to_string())
                .map_err(RuntimeError::from)
            {
                return Err(RuntimeError::RecoveryRequired {
                    cause: Box::new(error),
                    cleanup: Some(Box::new(cleanup)),
                    outcome_unknown: unresolved,
                });
            }
            return Err(error);
        }
        report_dispatched_status(&inner, &task, terminal, settlement_error(&outcome, &flags)).await;
        outcome
    })
}

fn settlement_error(
    outcome: &Result<SessionView, RuntimeError>,
    flags: &RunFlags,
) -> Option<String> {
    outcome
        .as_ref()
        .err()
        .map(ToString::to_string)
        .or_else(|| lock(&flags.reason).clone())
}

fn terminal_state(terminal: TaskTerminal) -> &'static str {
    match terminal {
        TaskTerminal::Completed => "completed",
        TaskTerminal::Failed => "failed",
        TaskTerminal::Cancelled => "cancelled",
        TaskTerminal::OutcomeUnknown => "outcome_unknown",
    }
}

/// Tell the main session that a dispatched task reached a terminal state.
/// A failure here stays on the main session's pending list. It does not change
/// the task row that was just recorded.
async fn report_dispatched_status(
    inner: &Arc<Inner>,
    task: &TaskRecord,
    terminal: TaskTerminal,
    error: Option<String>,
) {
    let Some(work_id) = task.work_id.clone() else {
        return;
    };
    let Some(owner) = lock(&inner.work_owner).clone() else {
        return;
    };
    let _ = owner
        .report_status(crate::WorkStatusReport {
            work_id,
            task_id: task.id.clone(),
            session_id: task.session_id.clone(),
            state: terminal_state(terminal).to_string(),
            error,
        })
        .await;
}

/// After the live session is released, deliver status reports that arrived
/// while it was busy. No Tokio runtime means the reports stay pending.
struct FlushPending {
    inner: Arc<Inner>,
    session: String,
}

impl Drop for FlushPending {
    fn drop(&mut self) {
        if !self.inner.config.main_session {
            return;
        }
        let pending = take_status_reports_for(&self.inner, &self.session);
        if pending.is_empty() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            lock(&self.inner.pending_status_reports).extend(pending);
            return;
        };
        let inner = self.inner.clone();
        handle.spawn(async move {
            tokio::task::yield_now().await;
            let runtime = Runtime { inner };
            for pending in pending {
                let _ = runtime.relay_task_status(pending.report);
            }
        });
    }
}

/// Remove only the reports that belong to `session_id`. Reports for other main
/// sessions stay where they are.
fn take_status_reports_for(
    inner: &Inner,
    session_id: &str,
) -> Vec<crate::work_router::PendingStatusReport> {
    let mut pending = lock(&inner.pending_status_reports);
    let mut kept = Vec::new();
    let mut mine = Vec::new();
    for report in pending.drain(..) {
        if report.owner_session_id == session_id {
            mine.push(report);
        } else {
            kept.push(report);
        }
    }
    pending.extend(kept);
    mine
}

pub(crate) async fn resume_child(
    runtime: &Runtime,
    id: &str,
    scope: Arc<crate::children::RunScope>,
    cancel: CancellationToken,
) -> Result<SessionView, RuntimeError> {
    let task = runtime.inner.queue.get(id)?;
    if task.state != TaskState::Queued || task.attempt_id.is_some() {
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
    let deadline = inner
        .config
        .max_queue_time
        .map(|time| tokio::time::Instant::now() + time);
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
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => return Err(RuntimeError::Budget("queue time")),
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
    admit(&inner, worker, &cancel).await?;
    if scope.is_some() {
        inner.fleet.mark_running(&task.id);
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
            return Err(error.into());
        }
        lease
    };
    let routed_user_turn = scope.is_none()
        && inner.config.main_session
        && lock(&inner.work_router).is_some()
        && task.work_id.is_none()
        && task.dispatch_key.is_none();
    let context = Arc::new(RunContext {
        selection: lock(&inner.selection).clone(),
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
        cells_manager: Mutex::new(None),
        persistence_error: Mutex::new(None),
        model_recording_error: Mutex::new(None),
        routed_user_turn,
        work_listed: AtomicBool::new(false),
        created_work_ids: Mutex::new(BTreeSet::new()),
        work_created: AtomicBool::new(false),
        work_dispatched: AtomicBool::new(false),
        work_reused: AtomicBool::new(false),
        work_status_read: AtomicBool::new(false),
        route_reminded: AtomicBool::new(false),
        route_notice_sent: AtomicBool::new(false),
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
        (None, Ok(())) => {
            if !context.unknown.load(Ordering::Acquire) && !context.cancel.is_cancelled() {
                let weak = Arc::downgrade(&inner);
                crate::context_maintenance::schedule_post_turn(weak.clone(), &task.session_id);
                crate::memory_writer::schedule_post_turn(weak, &task.session_id);
            }
            Runtime { inner }.get(&task.session_id)
        }
    }
}

const ROUTE_REQUIRED: &str = "This main-session turn cannot finish yet. Call work.list and read the existing WorkItems. If one is the same task, call work.dispatch for that work_id and do not create another. If none is the same task, call work.create and then work.dispatch for the new work_id. Or call work.get or work.tasks for one WorkItem and tell the user that status; that does not dispatch. A text answer is not accepted until work.list and a dispatch have both succeeded, or until one WorkItem status has been read.";
const RELAY: &str = "The main session only passes messages. The child session does the work and the review. Do not read or write files, run checks, or start a subagent. After you dispatch, tell the user and stop. When a task status update arrives, tell the user that status and stop. Read a WorkItem status only when the user is talking to you, or when this turn is the status update.";
const UNKNOWN_STOP: &str = "Tool outcome is unknown; the interrupted turn was stopped. Reconcile the original effect before replaying it.";

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
            if self.context.route_pending() {
                // One reminder, then a known failure. max_turns stays None on
                // the product path, so a repeated Continue would not stop.
                if !self.context.route_reminded.swap(true, Ordering::AcqRel) {
                    return Ok(cayw_agent::TurnDecision::Continue);
                }
                let error = RuntimeError::Worker(
                    "the main session must check existing WorkItems and dispatch the same task, or create a new WorkItem and dispatch it, or read one WorkItem status, before this turn can finish"
                        .into(),
                );
                let reason = error.to_string();
                *lock(&self.rejection) = Some(error);
                return Err(reason);
            }
        }
        Ok(cayw_agent::TurnDecision::Auto)
    }

    async fn prepare_next_turn(
        &self,
        _turn: cayw_agent::TurnContext,
        _cancel: CancellationToken,
    ) -> Result<cayw_agent::NextTurnUpdate, String> {
        if self.context.route_pending()
            && self.context.route_reminded.load(Ordering::Acquire)
            && !self.context.route_notice_sent.swap(true, Ordering::AcqRel)
        {
            return Ok(cayw_agent::NextTurnUpdate {
                messages: vec![Message::runtime(ROUTE_REQUIRED)],
                ..Default::default()
            });
        }
        Ok(cayw_agent::NextTurnUpdate::default())
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
    *lock(&context.cells_manager) = Some(Arc::downgrade(&manager));
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
        context.selection.model.clone(),
    );
    // FleetView intervention and a failed acceptance check both need the live
    // Agent control. The guard removes it when this turn ends.
    inner
        .fleet
        .attach_control(&context.lease.session_id, agent.control());
    let _session_control =
        SessionControlGuard::attach(inner, context.lease.session_id.clone(), agent.control());
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
    let max_tokens = context
        .scope
        .token_limit()
        .map(|limit| {
            limit.min(
                context
                    .selection
                    .model
                    .max_output_tokens
                    .unwrap_or(u64::MAX),
            )
        })
        .or(context.selection.model.max_output_tokens);
    agent.set_stream_options(cayw_ai::StreamOptions {
        max_tokens,
        ..Default::default()
    })?;
    let listener_context = context.clone();
    let started_tools = Arc::new(Mutex::new(std::collections::BTreeSet::<String>::new()));
    agent.subscribe(move |event, agent_cancel| {
        let ctx = listener_context.clone();
        let started_tools = started_tools.clone();
        async move {
            // Persist terminal bookkeeping before applying budgets/cancellation.
            let mut persisted = ctx.inner.store.append_agent_event(&ctx.lease, &event);
            if let Err(error) = &persisted
                && matches!(event, AgentEvent::AgentEnd { .. })
                && is_pre_write_refusal(error)
            {
                // AgentEnd's message list is informational: Core never compares
                // it with persisted history, and every message was already
                // persisted by its own terminal. Core can still reject the
                // duplicate list when a model message, or the accumulated run,
                // exceeds its metadata or event bound. Retrying without the
                // list keeps the lifecycle terminal durable so the attempt can
                // finish instead of remaining Running.
                persisted = ctx.inner.store.append_agent_event(
                    &ctx.lease,
                    &AgentEvent::AgentEnd {
                        messages: Vec::new(),
                    },
                );
            }
            if let Err(error) = persisted {
                // A pre-execution event can be refused by Core's metadata or
                // event bound (for example a model streamed a message or
                // tool-call argument larger than Core accepts). Nothing touched
                // SQLite, so that is a known failure, not an unknown side
                // effect. Settling events for an already-started tool and
                // genuine store failures still require reconciliation.
                let settles = matches!(
                    event,
                    AgentEvent::ToolExecutionUpdate { .. }
                        | AgentEvent::ToolExecutionEnd { .. }
                        | AgentEvent::TurnEnd { .. }
                        | AgentEvent::AgentEnd { .. }
                );
                let started_tool = match &event {
                    AgentEvent::ToolExecutionUpdate { call, .. }
                    | AgentEvent::ToolExecutionEnd { call, .. } => {
                        lock(&started_tools).contains(&call.id)
                    }
                    _ => false,
                };
                // Core checks these bounds before it touches SQLite. A settling
                // event can carry an oversized tool call whose start event was
                // already refused; the tool never executed, so the refusal is
                // still known rather than an uncertain effect.
                let metadata_refusal = is_pre_write_refusal(&error);
                let durable = matches!(
                    &error,
                    cayw_core::CoreError::Database(_)
                        | cayw_core::CoreError::StorePoisoned
                        | cayw_core::CoreError::CorruptStore(_)
                );
                if durable || started_tool || (settles && !metadata_refusal) {
                    ctx.persistence_failed(&error);
                } else {
                    lock(&ctx.failed_reason).get_or_insert_with(|| error.to_string());
                    ctx.failed.store(true, Ordering::Release);
                    ctx.cancel.cancel();
                }
                agent_cancel.cancel();
                return Err(error.to_string());
            }
            match &event {
                AgentEvent::ToolExecutionStart { call } => {
                    lock(&started_tools).insert(call.id.clone());
                }
                AgentEvent::ToolExecutionEnd { call, .. } => {
                    lock(&started_tools).remove(&call.id);
                    crate::context_maintenance::schedule_mid_turn(
                        Arc::downgrade(&ctx.inner),
                        &ctx.lease.session_id,
                    );
                }
                AgentEvent::TurnEnd { .. } => {
                    crate::context_maintenance::schedule_mid_turn(
                        Arc::downgrade(&ctx.inner),
                        &ctx.lease.session_id,
                    );
                }
                _ => {}
            }
            ctx.inner.changed.notify_waiters();
            if ctx.unknown.load(Ordering::Acquire)
                && matches!(
                    event,
                    AgentEvent::ToolExecutionEnd { .. } | AgentEvent::TurnEnd { .. }
                )
            {
                lock(&ctx.inner.session_controls)
                    .get(&ctx.lease.session_id)
                    .expect("running Agent control is registered")
                    .stop_with_error(UNKNOWN_STOP);
            }
            if matches!(event, AgentEvent::TurnStart) {
                ctx.turns.fetch_add(1, Ordering::AcqRel);
            }
            if let Some(max_event_bytes) = ctx.inner.config.max_event_bytes
                && serde_json::to_vec(&event).map_err(|e| e.to_string())?.len() > max_event_bytes
            {
                let reason = format!(
                    "agent event exceeds the configured max_event_bytes ({max_event_bytes})"
                );
                lock(&ctx.failed_reason).get_or_insert_with(|| reason);
                ctx.failed.store(true, Ordering::Release);
                ctx.cancel.cancel();
                agent_cancel.cancel();
            } else if ctx
                .inner
                .config
                .max_turns
                .is_some_and(|max_turns| ctx.turns.load(Ordering::Acquire) > max_turns)
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
    const INSTRUCTION: &str = "Use execute_code with action=exec, script, yield_time_ms and max_output_tokens for bounded Luau cells. A timed reply can leave the VM running: use action=wait with its cell_id, cursor, yield_time_ms and max_output_tokens until status is terminal and has_more is false. wait resumes yield_control unless continue_execution=false. Use tool_search(query,limit) or ALL_TOOLS to discover names and toolDefinition(name) for needed schemas. Invoke capabilities with cayw.call(name, arguments). Helpers store/load retain versioned session values; text, image, audio and notify produce typed output. return a JSON value or cayw.emit(value). Discovery is not permission. Every exact invocation needs host authorization. Never replay uncertain effects. No ambient shell, network or filesystem APIs are available.";
    let run = async {
        let approval_instruction = match inner.config.approval_mode {
            crate::ApprovalMode::ModelAuto => {
                "You decide operation approvals yourself through CodeMode. If a cell waits for permission, call approval.pending, review the exact operation, then call approval.decide with its approval_id and your decision. Continue waiting for the original cell, never replay it. For capability.request use capability.decide with the separate request_id. Approval is permission to execute, not successful execution or user acceptance."
            }
            crate::ApprovalMode::Explicit => {
                "Operation approvals are supplied by the caller. Do not claim to approve your own request."
            }
        };
        let mut sections = std::collections::BTreeMap::from([
            ("cayw-runtime-codemode".into(), Some(INSTRUCTION.into())),
            (
                "cayw-runtime-approval".into(),
                Some(approval_instruction.into()),
            ),
        ]);
        if context.routed_user_turn {
            sections.insert(
                "cayw-runtime-work-route".into(),
                Some(ROUTE_REQUIRED.into()),
            );
        }
        if inner.config.main_session && lock(&inner.work_router).is_some() {
            sections.insert("cayw-runtime-relay".into(), Some(RELAY.into()));
        }
        let instruction = Message::System(cayw_ai::SystemMessage {
            sections,
            ..Default::default()
        });
        // F08: fold Applied plannotator feedback into the next Agent attempt.
        let mut prompt = vec![instruction];
        prompt.extend(crate::plannotator::feedback_messages_for_attempt(
            inner,
            &context.lease.session_id,
        )?);
        if inner.config.main_session {
            for pending in take_status_reports_for(inner, &context.lease.session_id) {
                prompt.push(Message::runtime(crate::work_router::status_notice_text(
                    &pending.report,
                )));
            }
        }
        let host_notice = inner.queue.get(&context.task.id)?.dispatch_key.is_some();
        let turn = input.message();
        prompt.push(if host_notice {
            turn.with_author(cayw_ai::MessageAuthor::Runtime)
        } else {
            turn
        });
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
        context.scope.deadline().map(tokio::time::Instant::from_std),
        || async { context.check(&context.cancel) },
        |reason| {
            context.cells_stopping.store(true, Ordering::Release);
            if context.unknown.load(Ordering::Acquire) {
                agent.control().stop_with_error(UNKNOWN_STOP);
            } else if let Some(reason) = reason {
                agent.control().stop_with_error(reason);
            } else {
                agent.control().abort();
            }
        },
    )
    .await;
    inner.fleet.clear_control(&context.lease.session_id);
    result
}

async fn supervise_run<F, C, CF, A>(
    run: F,
    cancel: &CancellationToken,
    deadline: Option<tokio::time::Instant>,
    mut check: C,
    mut abort: A,
) -> Result<(), RuntimeError>
where
    F: std::future::Future<Output = Result<(), RuntimeError>>,
    C: FnMut() -> CF,
    CF: std::future::Future<Output = Result<(), RuntimeError>>,
    A: FnMut(Option<String>),
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
                    Ok(())
                        if expired
                            || deadline
                                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline) =>
                    {
                        Err(RuntimeError::Budget("time"))
                    }
                    other => other,
                };
            }
            _ = cancel.cancelled() => { abort(None); return run.await; },
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                abort(Some(RuntimeError::Budget("time").to_string()));
                return match run.await {
                    Ok(()) => Err(RuntimeError::Budget("time")),
                    Err(error) => Err(error),
                };
            }
            _ = fence.tick() => {
                if let Err(error) = check().await {
                    if matches!(error, RuntimeError::Budget("time")) {
                        expired = true;
                        abort(Some(error.to_string()));
                    } else if matches!(error, RuntimeError::Cancelled) {
                        cancel.cancel();
                    } else {
                        // A known stop must keep its cause after the Agent has
                        // joined. The user cancellation token is a separate fact.
                        abort(Some(error.to_string()));
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
    if inner.config.main_session && lock(&inner.work_router).is_some() {
        catalog.retain(|tool| crate::work_tools::main_session_relays(&tool.name));
    }
    if !inner.config.main_session && lock(&inner.work_owner).is_some() {
        catalog.push(crate::work_tools::child_review_definition());
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn sqlite(code: rusqlite::ErrorCode) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: 0,
            },
            None,
        )
    }

    #[test]
    fn durable_failure_only_marks_doubtful_writes() {
        for error in [
            RuntimeError::InvalidInput("bad name".into()),
            RuntimeError::StaleApproval,
            RuntimeError::Core(cayw_core::CoreError::StaleLease),
            RuntimeError::Core(cayw_core::CoreError::CounterExhausted),
            RuntimeError::Core(cayw_core::CoreError::InvalidInput("too large".into())),
            RuntimeError::Json(serde_json::from_str::<Value>("{").unwrap_err()),
            RuntimeError::Worker("known".into()),
            RuntimeError::Busy,
            RuntimeError::Budget("capability count"),
            RuntimeError::Cancelled,
        ] {
            assert!(!durable_failure(&error), "{error:?}");
        }
        for error in [
            RuntimeError::Journal(sqlite(rusqlite::ErrorCode::DatabaseBusy)),
            RuntimeError::Core(cayw_core::CoreError::Database(sqlite(
                rusqlite::ErrorCode::DatabaseBusy,
            ))),
            RuntimeError::Core(cayw_core::CoreError::StorePoisoned),
            RuntimeError::Core(cayw_core::CoreError::CorruptStore("fixture".into())),
        ] {
            assert!(durable_failure(&error), "{error:?}");
            let RuntimeError::Core(core) = &error else {
                continue;
            };
            assert!(store_effect_uncertain(core), "{error:?}");
        }
        assert!(!store_effect_uncertain(
            &cayw_core::CoreError::InvalidEvent("Wait binding".into(),)
        ));
        assert!(!store_effect_uncertain(&cayw_core::CoreError::Json(
            serde_json::from_str::<u64>("").unwrap_err(),
        )));
    }

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
            Some(started + Duration::from_secs(10)),
            || async {
                checks.set(checks.get() + 1);
                tokio::time::advance(Duration::from_millis(100)).await;
                Ok(())
            },
            |_| panic!("a valid response must not be cancelled"),
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
            Some(tokio::time::Instant::now() + Duration::from_secs(10)),
            || async {
                checks.set(checks.get() + 1);
                tokio::time::advance(Duration::from_millis(100)).await;
                cancel.cancel();
                Ok(())
            },
            |_| finished.cancel(),
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
                Some(tokio::time::Instant::now() + Duration::from_secs(10)),
                || std::future::ready(Err(failure.take().unwrap())),
                |reason| {
                    assert_eq!(reason.as_deref(), Some(expected.as_str()));
                    stopped.cancel();
                },
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
            Some(tokio::time::Instant::now() + Duration::from_secs(10)),
            || async { Err(RuntimeError::Cancelled) },
            |_| finished.cancel(),
        )
        .await
        .unwrap();
        assert!(cancel.is_cancelled());
        assert!(finished.is_cancelled());
    }
}
