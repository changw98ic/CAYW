//! Task request authority shares the original Host and immutable call origins.
use crate::{Inner, Runtime, RuntimeError, TaskRecord, TaskState, execution::RunContext};
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_requests::{
    AuthorizationReceipt, EvaluationTicket, HostActor, HostIdentity, OperationCatalog,
    OperationScope, RequestEvent, RequestRecord, RequestTools, TaskIdentity, TaskIntent,
    TaskOutcome,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

/// Selected existing operations need a task request in addition to ordinary
/// Host permission. Changes to injected services require a new policy identity.
#[derive(Clone, Debug)]
pub struct RequestPolicy {
    pub permission_policy_id: String,
    pub operations: Vec<OperationScope>,
}

pub(crate) struct TaskRequests {
    identity: TaskIdentity,
    catalog: OperationCatalog,
    tools: RequestTools,
}

pub(crate) enum Admission {
    Disabled,
    Upstream(Arc<TaskRequests>),
    Protected {
        task: Arc<TaskRequests>,
        receipt: Box<AuthorizationReceipt>,
    },
    Rejected(CapabilityError),
}
impl Admission {
    pub(crate) fn check(&self, context: &RunContext) -> Result<(), CapabilityError> {
        let result = match self {
            Self::Disabled => return Ok(()),
            Self::Rejected(error) => return Err(error.clone()),
            Self::Upstream(task) => context
                .inner
                .requests
                .check_task(&task.identity, &task.catalog),
            Self::Protected { task, receipt } => {
                context.inner.requests.check_receipt(receipt, &task.catalog)
            }
        };
        result.map_err(|error| request_error(context, error))
    }
    pub(crate) fn evidence(&self) -> Option<Value> {
        let Self::Protected { receipt, .. } = self else {
            return None;
        };
        Some(json!({
            "identity":receipt.identity(), "request_id":receipt.request_id(),
            "catalog_fingerprint":receipt.catalog_fingerprint(), "operation":receipt.scope(),
            "grant_expires_at_ms":receipt.grant_expires_at_ms(), "decision":receipt.decision(),
        }))
    }
}

fn request_error(context: &RunContext, error: cayw_requests::RequestError) -> CapabilityError {
    use cayw_requests::RequestError;
    match error {
        RequestError::InvalidInput(_)
        | RequestError::Stale
        | RequestError::NotFound
        | RequestError::Denied(_)
        | RequestError::Budget
        | RequestError::PolicyMismatch => CapabilityError::new(error.to_string()),
        _ => context.persistence_failed(error),
    }
}

pub(crate) fn is_tool(name: &str) -> bool {
    matches!(name, "capability.request" | "capability.status")
}
pub(crate) fn definitions(inner: &Inner) -> Vec<ToolDefinition> {
    if inner.config.request_policy.is_some() {
        RequestTools::tool_definitions()
    } else {
        vec![]
    }
}
fn catalog(inner: &Inner, host: &cayw_host::Host) -> Result<OperationCatalog, RuntimeError> {
    let policy = inner.config.request_policy.as_ref().ok_or_else(|| {
        RuntimeError::InvalidInput("task requests are not configured by the host".into())
    })?;
    Ok(OperationCatalog::bind(
        HostIdentity::from_host(
            host,
            &crate::bridge::effective_host_config(&inner.config),
            &crate::bridge::permission_policy_id(&inner.config, &policy.permission_policy_id)?,
        )?,
        &host.capabilities(),
        policy.operations.clone(),
    )?)
}
pub(crate) fn validate(inner: &Inner, host: &cayw_host::Host) -> Result<(), RuntimeError> {
    if inner.config.request_policy.is_some() {
        catalog(inner, host)?;
    }
    Ok(())
}
pub(crate) fn start(
    context: &Arc<RunContext>,
    host: &cayw_host::Host,
) -> Result<Option<Arc<TaskRequests>>, RuntimeError> {
    if context.inner.config.request_policy.is_none() {
        return Ok(None);
    }
    let identity = TaskIdentity::new(
        &context.lease.session_id,
        &context.task.id,
        &context.lease.attempt_id,
    )?;
    let catalog = catalog(&context.inner, host)?;
    let tools = {
        context.inner.requests.start_task(
            &identity,
            context.inner.queue.intent(&context.task.id)?,
            &catalog,
            &HostActor::new("runtime-task-start")?,
        )?;
        RequestTools::new(
            context.inner.requests.clone(),
            identity.clone(),
            catalog.clone(),
        )?
    };
    Ok(Some(Arc::new(TaskRequests {
        identity,
        catalog,
        tools,
    })))
}
pub(crate) fn admission(
    context: &RunContext,
    task: Option<&Arc<TaskRequests>>,
    name: &str,
    arguments: &Value,
) -> Admission {
    let Some(task) = task else {
        return Admission::Disabled;
    };
    if task.catalog.requires_request(name) {
        match context
            .inner
            .requests
            .admit(&task.identity, &task.catalog, name, arguments)
        {
            Ok(receipt) => Admission::Protected {
                task: task.clone(),
                receipt: Box::new(receipt),
            },
            Err(error) => Admission::Rejected(request_error(context, error)),
        }
    } else {
        Admission::Upstream(task.clone())
    }
}

struct ToolGate(Arc<crate::cells::CallScope>);
#[async_trait::async_trait]
impl cayw_host::PermissionGate for ToolGate {
    async fn authorize(
        &self,
        name: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        let scope = &self.0;
        if !is_tool(name) || name != scope.name || arguments != &scope.arguments {
            return Err(CapabilityError::new(
                "request tool differs from its fixed call",
            ));
        }
        scope
            .context
            .check(&cancel)
            .map_err(|error| CapabilityError::new(error.to_string()))?;
        scope.request.check(&scope.context)?;
        scope
            .authorize(crate::cells::Authorization {
                proof: scope.proof("runtime-request-tools".into(), None),
                grant: None,
            })
            .map_err(|error| CapabilityError::new(error.to_string()))
    }
}
struct ToolService<'a> {
    task: &'a TaskRequests,
    context: &'a RunContext,
}
#[async_trait::async_trait]
impl cayw_host::ExternalHost for ToolService<'_> {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.context
            .check(&cancel)
            .map_err(|error| CapabilityError::new(error.to_string()))?;
        let value = self
            .task
            .tools
            .invoke(name, arguments)
            .map_err(|error| request_error(self.context, error))?;
        self.context.inner.changed.notify_waiters();
        Ok(value)
    }
}
pub(crate) async fn invoke_tool(
    task: &TaskRequests,
    base: &cayw_host::Host,
    scope: &Arc<crate::cells::CallScope>,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let definition = RequestTools::tool_definitions()
        .into_iter()
        .find(|definition| definition.name == scope.name)
        .ok_or_else(|| CapabilityError::new("request tool is not configured"))?;
    let host = base.scoped(Arc::new(ToolGate(scope.clone())), scope.clone())?;
    host.call_service(
        &definition,
        arguments,
        cancel,
        &ToolService {
            task,
            context: &scope.context,
        },
    )
    .await
}

/// Poll the original call while watching the same receipt. Cancellation never
/// selects a replacement grant or creates a second execution future.
pub(crate) async fn call(
    scope: &Arc<crate::cells::CallScope>,
    host: Arc<dyn CapabilityHost>,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let run = host.call(&scope.name, arguments, child.clone());
    watch(scope, child, run).await
}

/// Retains the exact ordinary grant and Request receipt; this polls the single
/// original future and never selects a replacement grant or restarts work.
pub(crate) async fn watch<T>(
    scope: &Arc<crate::cells::CallScope>,
    child: CancellationToken,
    run: impl std::future::Future<Output = Result<T, CapabilityError>>,
) -> Result<T, CapabilityError> {
    tokio::pin!(run);
    let mut fence = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! { biased;
            result = &mut run => return result,
            _ = fence.tick() => {
                if scope.check_authority(&child).is_err() { child.cancel(); }
            },
        }
    }
}

fn identity(inner: &Inner, task: &TaskRecord) -> Result<Option<TaskIdentity>, RuntimeError> {
    let Some(attempt) = &task.attempt_id else {
        return Ok(None);
    };
    let identity = TaskIdentity::new(&task.session_id, &task.id, attempt)?;
    match inner.requests.task_state(&identity) {
        Ok(_) => Ok(Some(identity)),
        Err(cayw_requests::RequestError::NotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn close_identity(inner: &Inner, identity: &TaskIdentity) -> Result<(), RuntimeError> {
    if inner.requests.task_state(identity)? == cayw_requests::TaskState::Active {
        inner.requests.begin_closing(
            identity,
            &HostActor::new("runtime-task-close")?,
            "Task stopped admitting capabilities; original work must finish cleanup",
        )?;
    }
    Ok(())
}
pub(crate) fn closing(inner: &Inner, task_id: &str) -> Result<(), RuntimeError> {
    let task = inner.queue.get(task_id)?;
    let Some(identity) = identity(inner, &task)? else {
        return Ok(());
    };
    close_identity(inner, &identity)
}
/// Called with lifecycle coordination and the original task record. Queue may
/// hold its terminal transaction, so authority settlement must not reread Queue.
pub(crate) fn finish(
    inner: &Inner,
    task: &TaskRecord,
    outcome: TaskOutcome,
) -> Result<(), RuntimeError> {
    let Some(identity) = identity(inner, task)? else {
        return Ok(());
    };
    match inner.requests.task_state(&identity)? {
        cayw_requests::TaskState::Ended { outcome: prior, .. } if prior == outcome => return Ok(()),
        cayw_requests::TaskState::Ended { .. } => {
            return Err(RuntimeError::Worker(
                "request authority has a different terminal outcome".into(),
            ));
        }
        _ => {}
    }
    close_identity(inner, &identity)?;
    inner.requests.end_task(
        &identity,
        outcome,
        &HostActor::new("runtime-task-end")?,
        "Original task work stopped and its process cleanup was confirmed",
    )?;
    Ok(())
}
pub(crate) fn check_cancel(inner: &Inner, session: &str) -> Result<(), RuntimeError> {
    for task in inner.queue.list(session)? {
        if matches!(
            task.state,
            TaskState::Queued | TaskState::Running | TaskState::Draining
        ) && let Some(identity) = identity(inner, &task)?
            && matches!(
                inner.requests.task_state(&identity)?,
                cayw_requests::TaskState::Ended { .. }
            )
        {
            // Requests is a separate durable authority. It may have committed
            // while Queue's terminal commit failed. Do not acknowledge another
            // terminal choice over that partial settlement; keep recovery owed.
            return Err(RuntimeError::RecoveryRequired {
                cause: Box::new(RuntimeError::Worker("request authority ended before queue settlement; explicit recovery is required".into())),
                cleanup: None,
                outcome_unknown: false,
            });
        }
    }
    Ok(())
}
pub(crate) fn close_session(inner: &Inner, session: &str) -> Result<(), RuntimeError> {
    for task in inner.queue.list(session)? {
        closing(inner, &task.id)?;
    }
    Ok(())
}
pub(crate) fn finish_session(inner: &Inner, session: &str) -> Result<(), RuntimeError> {
    for task in inner.queue.list(session)? {
        let Some(identity) = identity(inner, &task)? else {
            continue;
        };
        if matches!(
            inner.requests.task_state(&identity)?,
            cayw_requests::TaskState::Ended { .. }
        ) {
            continue;
        }
        // Recovery never rewrites an ended authority or labels unknown effects
        // completed/cancelled. Core's unknown facts remain independently blocking.
        let outcome = if matches!(
            task.state,
            TaskState::OutcomeUnknown | TaskState::Draining | TaskState::Failed
        ) || inner.store.snapshot(session)?.state
            == cayw_core::SessionState::NeedsReconciliation
        {
            TaskOutcome::Failed
        } else if task.state == TaskState::Completed {
            TaskOutcome::Completed
        } else {
            TaskOutcome::Cancelled
        };
        finish(inner, &task, outcome)?;
    }
    Ok(())
}

pub(crate) fn validate_intent(intent: &TaskIntent) -> Result<(), RuntimeError> {
    if let TaskIntent::ExplicitProjectTestOrScript { user_instruction } = intent
        && (user_instruction.trim().is_empty()
            || user_instruction.len() > 2048
            || user_instruction
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'))
    {
        return Err(RuntimeError::InvalidInput(
            "explicit task instruction must contain 1..2048 ordinary bytes".into(),
        ));
    }
    Ok(())
}

impl Runtime {
    fn request_identity(&self, task_id: &str) -> Result<TaskIdentity, RuntimeError> {
        let task = self.task(task_id)?;
        identity(&self.inner, &task)?
            .ok_or_else(|| RuntimeError::InvalidInput("task has no host request authority".into()))
    }
    fn active_request(&self, task_id: &str) -> Result<TaskIdentity, RuntimeError> {
        let task = self.task(task_id)?;
        if task.state != TaskState::Running || task.cancel_requested {
            return Err(RuntimeError::StaleApproval);
        }
        let view = self.get(&task.session_id)?;
        if view.frozen || view.state == cayw_core::SessionState::NeedsReconciliation {
            return Err(RuntimeError::StaleApproval);
        }
        let lease = view.attempt.ok_or(RuntimeError::StaleApproval)?;
        if task.worker_id.as_ref() != Some(&lease.worker_id)
            || task.attempt_id.as_ref() != Some(&lease.attempt_id)
        {
            return Err(RuntimeError::StaleApproval);
        }
        self.inner.store.validate_lease(&lease)?;
        self.request_identity(task_id)
    }
    pub fn request_operations(&self) -> Result<Vec<OperationScope>, RuntimeError> {
        Ok(catalog(&self.inner, &self.inner.host_root)?.operations())
    }
    pub fn request_status(
        &self,
        task_id: &str,
        request_id: &str,
    ) -> Result<RequestRecord, RuntimeError> {
        Ok(self
            .inner
            .requests
            .status(&self.request_identity(task_id)?, request_id)?)
    }
    pub fn request_events(
        &self,
        task_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<RequestEvent>, RuntimeError> {
        Ok(self
            .inner
            .requests
            .events(&self.request_identity(task_id)?, after, limit)?)
    }
    pub fn request_task_state(
        &self,
        task_id: &str,
    ) -> Result<cayw_requests::TaskState, RuntimeError> {
        Ok(self
            .inner
            .requests
            .task_state(&self.request_identity(task_id)?)?)
    }
    /// Host SDK only: eligibility is checked by RequestStore; this never grants.
    pub fn request_evaluate(
        &self,
        task_id: &str,
        request_id: &str,
        actor: &str,
    ) -> Result<EvaluationTicket, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let identity = self.active_request(task_id)?;
        Ok(self
            .inner
            .requests
            .evaluate(&identity, request_id, &HostActor::new(actor)?)?)
    }
    pub fn request_approve(
        &self,
        task_id: &str,
        ticket: &EvaluationTicket,
        lifetime: Duration,
        reason: &str,
    ) -> Result<RequestRecord, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let identity = self.active_request(task_id)?;
        self.inner.requests.status(&identity, ticket.request_id())?;
        let catalog = catalog(&self.inner, &self.inner.host_root)?;
        let lifetime = u64::try_from(lifetime.as_millis()).map_err(|_| {
            RuntimeError::InvalidInput("request grant lifetime is too large".into())
        })?;
        let record = self
            .inner
            .requests
            .approve(ticket, &catalog, lifetime, reason)?;
        self.inner.changed.notify_waiters();
        Ok(record)
    }
    pub fn request_deny(
        &self,
        task_id: &str,
        ticket: &EvaluationTicket,
        reason: &str,
    ) -> Result<RequestRecord, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let identity = self.active_request(task_id)?;
        self.inner.requests.status(&identity, ticket.request_id())?;
        let record = self.inner.requests.deny(ticket, reason)?;
        self.inner.changed.notify_waiters();
        Ok(record)
    }
    pub fn request_evaluator_unavailable(
        &self,
        task_id: &str,
        ticket: &EvaluationTicket,
        reason: &str,
    ) -> Result<RequestRecord, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let identity = self.active_request(task_id)?;
        self.inner.requests.status(&identity, ticket.request_id())?;
        let record = self.inner.requests.evaluator_unavailable(ticket, reason)?;
        self.inner.changed.notify_waiters();
        Ok(record)
    }
    pub fn request_revoke(
        &self,
        task_id: &str,
        request_id: &str,
        actor: &str,
        reason: &str,
    ) -> Result<RequestRecord, RuntimeError> {
        let record = {
            let _coordinate = self.inner.coordination.enter()?;
            let identity = self.active_request(task_id)?;
            self.inner
                .requests
                .revoke(&identity, request_id, &HostActor::new(actor)?, reason)?
        };
        self.cancel(&record.identity.session_id)?;
        Ok(record)
    }
    /// Closing is durable before cancellation. Finishing is owned by the worker
    /// after its original cells and process supervisors have stopped.
    pub fn close_request_task(&self, task_id: &str) -> Result<(), RuntimeError> {
        let session = {
            let _coordinate = self.inner.coordination.enter()?;
            let identity = self.active_request(task_id)?;
            closing(&self.inner, task_id)?;
            identity.session_id
        };
        self.cancel(&session)?;
        Ok(())
    }
}
