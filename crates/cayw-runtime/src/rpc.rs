use crate::{
    ApprovalChoice, Runtime, RuntimeError,
    input::{ImageInput, TaskInput},
};
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_core::{CapabilityReconciliation, Reconciliation};
use cayw_ipc::{RpcError, RpcHandler};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::time::Duration;

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))
}
fn encode(value: impl serde::Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|error| RpcError::new("serialization", error.to_string()))
}
const MAX_CONTEXT_FIELD_BYTES: usize = 128;

fn safe_context_field(value: &str) -> Option<&str> {
    (!value.is_empty()
        && value.len() <= MAX_CONTEXT_FIELD_BYTES
        && !value.chars().any(|c| c.is_control() || c.is_whitespace()))
    .then_some(value)
}

/// Only the method and supplied session/task IDs are retained. Prompt text and
/// arbitrary parameters never enter the error message.
fn operation_context(method: &str, params: &Value) -> String {
    let mut context = String::from("method=");
    context.push_str(safe_context_field(method).unwrap_or("<invalid>"));
    for key in ["session_id", "task_id", "child_task_id"] {
        if let Some(value) = params.get(key).and_then(Value::as_str)
            && let Some(value) = safe_context_field(value)
        {
            context.push(' ');
            context.push_str(key);
            context.push('=');
            context.push_str(value);
        }
    }
    context
}

fn with_operation_context(error: RpcError, context: &str) -> RpcError {
    RpcError::new(error.code, format!("{context}: {}", error.message))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    label: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    session_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentInspect {
    session_id: String,
    task_id: Option<String>,
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prompt {
    session_id: String,
    text: String,
    #[serde(default)]
    images: Vec<ImageInput>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Events {
    session_id: String,
    #[serde(default)]
    after: u64,
    #[serde(default = "page_limit")]
    limit: usize,
    #[serde(default)]
    wait_ms: u64,
}
fn page_limit() -> usize {
    100
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunInspector {
    session_id: String,
    /// Exclusive Core cursor to start from (same meaning as `session.events`).
    #[serde(default)]
    after: u64,
    /// Max Core events to fold. Omit to scan the full remaining log (paged
    /// internally past the first 1000). Explicit `null` is also "all remaining".
    #[serde(default)]
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    session_id: String,
    approval_id: String,
    decision: ApprovalChoice,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalQuestionRequest {
    session_id: String,
    approval_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalReply {
    session_id: String,
    approval_id: String,
    reply: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RulePropose {
    session_id: String,
    capability: String,
    arguments: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleConfirm {
    session_id: String,
    pending_id: String,
    reply: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revoke {
    session_id: String,
    grant_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryApply {
    session_id: String,
    candidates: Vec<crate::main_memory::MemoryCandidate>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryForget {
    session_id: String,
    start_cursor: u64,
    end_cursor: u64,
    reason: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryDream {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    summaries: Vec<cayw_context::SummaryRecord>,
    #[serde(default)]
    model_text: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkCreate {
    spec: crate::WorkItemSpec,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkId {
    work_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkAcceptProgram {
    work_id: String,
    criterion_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkAcceptUser {
    work_id: String,
    criterion_id: String,
    passed: bool,
    answer_ref: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkReport {
    work_id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkLateResult {
    work_id: String,
    task_id: String,
    result_ref: String,
    pause_version_at_dispatch: u64,
    stop_version_at_dispatch: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputIngest {
    source: String,
    source_event_id: String,
    log_id: String,
    session_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputTransition {
    log_id: String,
    state: crate::InputState,
    #[serde(default)]
    work_id: Option<String>,
    #[serde(default)]
    result_ref: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputQuery {
    log_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProactiveTrigger {
    #[serde(default)]
    mode: Option<crate::ProactiveMode>,
    rule_id: String,
    condition_version: u64,
    work_id: String,
    #[serde(default)]
    usage_tokens: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyPrepare {
    work_id: String,
    work_version: u64,
    target: String,
    #[serde(default)]
    content_ref: Option<String>,
    #[serde(default)]
    artifact_refs: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifySend {
    #[serde(default)]
    notification_id: Option<String>,
    #[serde(default)]
    work_id: Option<String>,
    #[serde(default)]
    work_version: Option<u64>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    content_ref: Option<String>,
    #[serde(default)]
    artifact_refs: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyReceipt {
    notification_id: String,
    status: crate::NotificationStatus,
    receipt_ref: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyId {
    notification_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchTask {
    session_id: String,
    text: String,
    #[serde(default)]
    images: Vec<ImageInput>,
    #[serde(default)]
    intent: Option<crate::TaskIntent>,
    #[serde(default)]
    priority: Option<crate::TaskPriority>,
    #[serde(default)]
    resources: Option<crate::TaskResources>,
    dispatch: crate::TaskDispatch,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchKey {
    dispatch_key: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkVersion {
    work_id: String,
    version: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerQuestion {
    session_id: String,
    request_id: String,
    answer: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconcile {
    session_id: String,
    reconciliation: Reconciliation,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconcileCapability {
    session_id: String,
    reconciliation: CapabilityReconciliation,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fork {
    session_id: String,
    label: String,
    confirm: bool,
    mode: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    task_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnqueueWithIntent {
    session_id: String,
    text: String,
    #[serde(default)]
    images: Vec<ImageInput>,
    intent: crate::TaskIntent,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestStatus {
    task_id: String,
    request_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scheduled {
    session_id: String,
    text: String,
    #[serde(default)]
    images: Vec<ImageInput>,
    intent: crate::TaskIntent,
    priority: crate::TaskPriority,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactQuery {
    artifact_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactActionQuery {
    action_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArtifactActionQuery {
    session_id: String,
    action_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceTask {
    session_id: String,
    text: String,
    #[serde(default)]
    images: Vec<ImageInput>,
    intent: crate::TaskIntent,
    priority: crate::TaskPriority,
    resources: crate::TaskResources,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scheduler {
    #[serde(default)]
    after: Option<String>,
    #[serde(default = "page_limit")]
    limit: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fleet {
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default = "page_limit")]
    limit: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetStop {
    child_task_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetSteer {
    child_task_id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundTaskId {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundList {
    session_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundLogs {
    id: String,
    #[serde(default)]
    offset: u64,
    #[serde(default = "page_limit")]
    limit: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundRecover {
    id: String,
    status: crate::BackgroundStatus,
    #[serde(default)]
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEvents {
    task_id: String,
    #[serde(default)]
    after: u64,
    #[serde(default = "request_page_limit")]
    limit: usize,
}
fn request_page_limit() -> usize {
    64
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RequestDecision {
    Allow { lifetime_ms: u64 },
    Deny,
    Unavailable,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideRequest {
    task_id: String,
    request_id: String,
    decision: RequestDecision,
    reason: String,
}
impl DecideRequest {
    fn validate(&self, limits: &cayw_requests::RequestConfig) -> Result<(), RpcError> {
        if self.reason.trim().is_empty()
            || self.reason.len() > 2048
            || self
                .reason
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err(RpcError::new(
                "invalid_params",
                "decision reason must contain 1..2048 ordinary bytes",
            ));
        }
        if let RequestDecision::Allow { lifetime_ms } = self.decision
            && (lifetime_ms == 0 || lifetime_ms > limits.max_grant_lifetime_ms)
        {
            return Err(RpcError::new(
                "invalid_params",
                "grant lifetime exceeds host maximum",
            ));
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeRequest {
    task_id: String,
    request_id: String,
    reason: String,
}

impl Runtime {
    async fn dispatch_rpc(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled.into());
        }
        match method {
            "runtime.health" => {
                let _: Empty = parse(params)?;
                encode(self.health()?)
            }
            "runtime.resources" => {
                let _: Empty = parse(params)?;
                encode(self.resources()?)
            }
            "recovery.status" => {
                let _: Empty = parse(params)?;
                encode(self.recovery_status())
            }
            "recovery.reconciliation" => {
                let _: Empty = parse(params)?;
                encode(self.recovery_reconciliation())
            }
            "recovery.disable" => {
                let _: Empty = parse(params)?;
                encode(self.disable_git_recovery()?)
            }
            "recovery.points" => {
                let _: Empty = parse(params)?;
                encode(self.recovery_points()?)
            }
            "recovery.create" => {
                let _: Empty = parse(params)?;
                Ok(self.create_recovery_point()?)
            }
            "recovery.restore" => {
                let request: crate::recovery_points::RestoreRequest = parse(params)?;
                encode(self.restore_recovery_point(request)?)
            }
            "memory.status" => {
                let _: Empty = parse(params)?;
                encode(self.memory_status()?)
            }
            "memory.entries" => {
                let _: Empty = parse(params)?;
                encode(self.memory_entries()?)
            }
            "memory.query" => {
                let p: crate::MemoryQuery = parse(params)?;
                encode(self.query_memory(p)?)
            }
            "memory.apply" => {
                let p: MemoryApply = parse(params)?;
                encode(self.apply_memory_candidates(&p.session_id, &p.candidates)?)
            }
            "memory.forget" => {
                let p: MemoryForget = parse(params)?;
                let range =
                    cayw_context::SourceRange::new(p.session_id, p.start_cursor, p.end_cursor)
                        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
                self.forget_memory_range(range, &p.reason)?;
                encode(serde_json::json!({"status":"recorded"}))
            }
            "memory.dream" => {
                let p: MemoryDream = parse(params)?;
                let summaries = if !p.summaries.is_empty() {
                    p.summaries
                } else if let Some(session_id) = p.session_id.as_deref() {
                    self.context_summaries(session_id)?
                } else {
                    Vec::new()
                };
                encode(self.dream_memory(&summaries, p.model_text.as_deref())?)
            }
            "work.create" => {
                let p: WorkCreate = parse(params)?;
                encode(self.create_work_item(p.spec)?)
            }
            "work.get" => {
                let p: WorkId = parse(params)?;
                encode(self.work_item(&p.work_id)?)
            }
            "work.list" => {
                let _: Empty = parse(params)?;
                encode(self.list_work_items()?)
            }
            "work.pause" => {
                let p: WorkId = parse(params)?;
                let (item, targets) = self.pause_work_item_routed(&p.work_id).await?;
                let mut value = encode(item)?;
                if let Some(object) = value.as_object_mut() {
                    object.insert("targets".into(), encode(targets)?);
                }
                Ok(value)
            }
            "work.resume" => {
                let p: WorkId = parse(params)?;
                encode(self.resume_work_item(&p.work_id)?)
            }
            "work.stop" => {
                let p: WorkId = parse(params)?;
                let (item, targets) = self.stop_work_item_routed(&p.work_id).await?;
                let mut value = encode(item)?;
                if let Some(object) = value.as_object_mut() {
                    object.insert("targets".into(), encode(targets)?);
                }
                Ok(value)
            }
            "work.accept.program" => {
                let p: WorkAcceptProgram = parse(params)?;
                encode(self.evaluate_work_acceptance(&p.work_id, &p.criterion_id)?)
            }
            "work.accept.user" => {
                let p: WorkAcceptUser = parse(params)?;
                encode(self.record_work_user_acceptance(
                    &p.work_id,
                    &p.criterion_id,
                    p.passed,
                    &p.answer_ref,
                )?)
            }
            "work.report" => {
                let p: WorkReport = parse(params)?;
                self.record_work_model_report(&p.work_id, &p.text)?;
                Ok(json!({"recorded":false}))
            }
            "work.late_result" => {
                let p: WorkLateResult = parse(params)?;
                encode(self.record_work_late_result(
                    &p.work_id,
                    &p.task_id,
                    &p.result_ref,
                    p.pause_version_at_dispatch,
                    p.stop_version_at_dispatch,
                )?)
            }
            "input.ingest" => {
                let p: InputIngest = parse(params)?;
                encode(self.ingest_input(
                    &p.source,
                    &p.source_event_id,
                    &p.log_id,
                    &p.session_id,
                )?)
            }
            "input.transition" => {
                let p: InputTransition = parse(params)?;
                encode(self.transition_input(
                    &p.log_id,
                    p.state,
                    p.work_id.as_deref(),
                    p.result_ref.as_deref(),
                )?)
            }
            "input.get" => {
                let p: InputQuery = parse(params)?;
                encode(self.input_record(&p.log_id)?)
            }
            "proactive.trigger" => {
                let p: ProactiveTrigger = parse(params)?;
                let outcome = match p.mode {
                    Some(mode) => self.trigger_proactive(
                        mode,
                        &p.rule_id,
                        p.condition_version,
                        &p.work_id,
                        p.usage_tokens,
                    )?,
                    None => self.trigger_configured_proactive(
                        &p.rule_id,
                        p.condition_version,
                        &p.work_id,
                        p.usage_tokens,
                    )?,
                };
                let task = self.run_proactive_dispatch(&outcome).await?;
                encode(outcome.with_task_id(task.map(|item| item.id)))
            }
            "notify.send" | "work.notify.send" => {
                let p: NotifySend = parse(params)?;
                encode(self.send_notification(
                    p.notification_id.as_deref(),
                    p.work_id.as_deref(),
                    p.work_version,
                    p.target.as_deref(),
                    p.text.as_deref(),
                    p.content_ref.as_deref(),
                    &p.artifact_refs,
                )?)
            }
            "notify.prepare" => {
                let p: NotifyPrepare = parse(params)?;
                encode(self.prepare_notification(
                    &p.work_id,
                    p.work_version,
                    &p.target,
                    p.content_ref.as_deref(),
                    &p.artifact_refs,
                )?)
            }
            "notify.receipt" => {
                let p: NotifyReceipt = parse(params)?;
                encode(self.record_notification_receipt(
                    &p.notification_id,
                    p.status,
                    &p.receipt_ref,
                )?)
            }
            "notify.next" => {
                let p: NotifyId = parse(params)?;
                encode(self.notification_decision(&p.notification_id)?)
            }
            "notify.get" => {
                let p: NotifyId = parse(params)?;
                encode(self.notification(&p.notification_id)?)
            }
            "work.apply_pause" => {
                let p: WorkVersion = parse(params)?;
                Ok(json!({"updated":self.apply_work_pause(&p.work_id, p.version)?}))
            }
            "work.apply_stop" => {
                let p: WorkVersion = parse(params)?;
                Ok(json!({"updated":self.apply_work_stop(&p.work_id, p.version)?}))
            }
            "task.enqueue_with_resources" => {
                let p: ResourceTask = parse(params)?;
                encode(self.enqueue_with_resources(
                    &p.session_id,
                    TaskInput {
                        text: p.text,
                        images: p.images,
                    },
                    p.intent,
                    p.priority,
                    p.resources,
                )?)
            }
            "runtime.scheduler" => {
                let p: Scheduler = parse(params)?;
                encode(self.scheduler_page(p.after.as_deref(), p.limit)?)
            }
            "runtime.fleet" => {
                let p: Fleet = parse(params)?;
                encode(self.fleet(p.parent_session_id.as_deref(), p.after.as_deref(), p.limit)?)
            }
            "runtime.fleet.stop" => {
                let p: FleetStop = parse(params)?;
                encode(self.fleet_stop(&p.child_task_id)?)
            }
            "runtime.fleet.steer" => {
                let p: FleetSteer = parse(params)?;
                encode(self.fleet_steer(&p.child_task_id, &p.text)?)
            }
            "runtime.background.list" => {
                let p: BackgroundList = parse(params)?;
                encode(self.background_list_session(&p.session_id)?)
            }
            "runtime.background.inspect" => {
                let p: BackgroundTaskId = parse(params)?;
                encode(self.background_inspect(&p.id)?)
            }
            "runtime.background.logs" => {
                let p: BackgroundLogs = parse(params)?;
                encode(self.background_logs(&p.id, p.offset, p.limit)?)
            }
            "runtime.background.kill" => {
                let p: BackgroundTaskId = parse(params)?;
                encode(self.background_kill(&p.id)?)
            }
            "runtime.background.result" => {
                let p: BackgroundTaskId = parse(params)?;
                encode(self.background_result(&p.id)?)
            }
            "runtime.background.recover" => {
                let p: BackgroundRecover = parse(params)?;
                encode(self.background_recover(&p.id, p.status, p.error.as_deref())?)
            }
            "task.enqueue_scheduled" => {
                let p: Scheduled = parse(params)?;
                encode(self.enqueue_scheduled(
                    &p.session_id,
                    TaskInput {
                        text: p.text,
                        images: p.images,
                    },
                    p.intent,
                    p.priority,
                )?)
            }
            "content.inspect" => {
                let p: ContentInspect = parse(params)?;
                encode(self.content_inspect(&p.session_id, p.task_id.as_deref(), &p.name)?)
            }
            "session.create" => {
                let p: Create = parse(params)?;
                encode(self.create(p.label)?)
            }
            "session.list" => {
                let _: Empty = parse(params)?;
                encode(self.list()?)
            }
            "session.get" => {
                let p: Session = parse(params)?;
                encode(self.get(&p.session_id)?)
            }
            "session.events" => {
                let p: Events = parse(params)?;
                encode(
                    self.wait_events(
                        &p.session_id,
                        p.after,
                        p.limit,
                        Duration::from_millis(p.wait_ms),
                        cancel,
                    )
                    .await?,
                )
            }
            "session.run_inspector" => {
                let p: RunInspector = parse(params)?;
                encode(self.run_inspector_page(&p.session_id, p.after, p.limit)?)
            }
            "session.audit" => {
                let p: Events = parse(params)?;
                encode(self.audit(&p.session_id, p.after, p.limit)?)
            }
            "session.context_overview" => {
                let p: Session = parse(params)?;
                encode(self.session_context_overview(&p.session_id)?)
            }
            "session.grants" => {
                let p: Session = parse(params)?;
                encode(self.grants(&p.session_id)?)
            }
            "session.prompt" => {
                let p: Prompt = parse(params)?;
                let input = TaskInput {
                    text: p.text,
                    images: p.images,
                };
                encode(self.prompt_input(&p.session_id, input, cancel).await?)
            }
            "session.cancel" => {
                let p: crate::CancelTarget = parse(params)?;
                Ok(json!({"cancelled":self.cancel(&p)?}))
            }
            "session.recover" => {
                let p: Session = parse(params)?;
                encode(self.recover(&p.session_id)?)
            }
            "session.maintenance.status" => {
                let p: Session = parse(params)?;
                encode(self.context_maintenance_status(&p.session_id)?)
            }
            "session.maintenance.reconcile" => {
                let p: crate::MaintenanceReconciliation = parse(params)?;
                encode(self.reconcile_maintenance(p)?)
            }
            "session.reconcile" => {
                let mut p: Reconcile = parse(params)?;
                p.reconciliation.actor = "rpc-user".into();
                encode(self.reconcile(&p.session_id, p.reconciliation)?)
            }
            "capability.reconcile" => {
                let mut p: ReconcileCapability = parse(params)?;
                p.reconciliation.actor = "rpc-user".into();
                encode(self.reconcile_capability(&p.session_id, p.reconciliation)?)
            }
            "session.fork" => {
                let p: Fork = parse(params)?;
                if p.mode != "chat" {
                    return Err(RpcError::new("invalid_params", "fork mode must be chat"));
                }
                encode(self.fork(&p.session_id, &p.label, p.confirm)?)
            }
            "approval.decide" => {
                let p: Approval = parse(params)?;
                encode(self.approve(&p.session_id, &p.approval_id, p.decision, "rpc-user")?)
            }
            "approval.question" => {
                let p: ApprovalQuestionRequest = parse(params)?;
                encode(self.approval_question(&p.session_id, &p.approval_id)?)
            }
            "approval.reply" => {
                let p: ApprovalReply = parse(params)?;
                encode(self.answer_approval(&p.session_id, &p.approval_id, &p.reply, "rpc-user")?)
            }
            "approval.rule.propose" => {
                let p: RulePropose = parse(params)?;
                encode(self.propose_approval_rule(
                    &p.session_id,
                    crate::ApprovalRule {
                        capability: p.capability,
                        arguments: p.arguments,
                    },
                )?)
            }
            "approval.rule.confirm" => {
                let p: RuleConfirm = parse(params)?;
                encode(self.confirm_approval_rule(&p.session_id, &p.pending_id, &p.reply)?)
            }
            "approval.revoke" => {
                let p: Revoke = parse(params)?;
                self.revoke(&p.session_id, &p.grant_id, "rpc-user")?;
                Ok(json!({"revoked":true}))
            }
            "capabilities.list" => {
                let _: Empty = parse(params)?;
                encode(self.capabilities()?)
            }
            "session.questions" => {
                let p: Session = parse(params)?;
                encode(self.questions(&p.session_id)?)
            }
            "session.pending_questions" => {
                let p: Session = parse(params)?;
                encode(self.pending_questions(&p.session_id)?)
            }
            "session.plan" => {
                let p: Session = parse(params)?;
                encode(self.plan(&p.session_id)?)
            }
            "plannotator.submit" => {
                let mut p: crate::PlanFeedbackInput = parse(params)?;
                if p.actor.trim().is_empty() {
                    p.actor = "rpc-user".into();
                }
                encode(self.submit_plan_feedback(p)?)
            }
            "plannotator.list" => {
                let p: Session = parse(params)?;
                encode(self.plannotator_feedback(&p.session_id)?)
            }
            "question.answer" => {
                let p: AnswerQuestion = parse(params)?;
                encode(self.answer_question(&p.session_id, &p.request_id, &p.answer, "rpc-user")?)
            }
            "session.tasks" => {
                let p: Session = parse(params)?;
                encode(self.tasks(&p.session_id)?)
            }
            "task.get" => {
                let p: Task = parse(params)?;
                encode(self.task(&p.task_id)?)
            }
            "task.enqueue" => {
                let p: Prompt = parse(params)?;
                let input = TaskInput {
                    text: p.text,
                    images: p.images,
                };
                encode(self.enqueue_input(&p.session_id, input)?)
            }
            "task.enqueue_dispatched" => {
                let p: DispatchTask = parse(params)?;
                encode(self.enqueue_dispatched(
                    &p.session_id,
                    TaskInput {
                        text: p.text,
                        images: p.images,
                    },
                    p.intent.unwrap_or(crate::TaskIntent::General),
                    p.priority.unwrap_or(crate::TaskPriority::Normal),
                    p.resources.unwrap_or_default(),
                    p.dispatch,
                )?)
            }
            "task.by_dispatch_key" => {
                let p: DispatchKey = parse(params)?;
                encode(self.task_by_dispatch_key(&p.dispatch_key)?)
            }
            "task.enqueue_with_intent" => {
                let p: EnqueueWithIntent = parse(params)?;
                encode(self.enqueue_input_with_intent(
                    &p.session_id,
                    TaskInput {
                        text: p.text,
                        images: p.images,
                    },
                    p.intent,
                )?)
            }
            "task.resume" => {
                let p: Task = parse(params)?;
                encode(self.resume(&p.task_id, cancel).await?)
            }
            "request.operations" => {
                let _: Empty = parse(params)?;
                encode(self.request_operations()?)
            }
            "request.status" => {
                let p: RequestStatus = parse(params)?;
                encode(self.request_status(&p.task_id, &p.request_id)?)
            }
            "request.events" => {
                let p: RequestEvents = parse(params)?;
                encode(self.request_events(&p.task_id, p.after, p.limit)?)
            }
            "request.task_state" => {
                let p: Task = parse(params)?;
                encode(self.request_task_state(&p.task_id)?)
            }
            "request.decide" => {
                let p: DecideRequest = parse(params)?;
                // Validate before claiming evaluation: rejected JSON must not
                // leave a request waiting for an inaccessible private ticket.
                p.validate(&self.request_limits())?;
                let ticket = self.request_evaluate(&p.task_id, &p.request_id, "rpc-user")?;
                // These remain separate durable transitions. Cancellation,
                // expiry or a failed write can reject the original decision;
                // never issue a replacement ticket or retry it automatically.
                let record = match p.decision {
                    RequestDecision::Allow { lifetime_ms } => self.request_approve(
                        &p.task_id,
                        &ticket,
                        Duration::from_millis(lifetime_ms),
                        &p.reason,
                    )?,
                    RequestDecision::Deny => self.request_deny(&p.task_id, &ticket, &p.reason)?,
                    RequestDecision::Unavailable => {
                        self.request_evaluator_unavailable(&p.task_id, &ticket, &p.reason)?
                    }
                };
                encode(record)
            }
            "request.revoke" => {
                let p: RevokeRequest = parse(params)?;
                encode(self.request_revoke(&p.task_id, &p.request_id, "rpc-user", &p.reason)?)
            }
            "request.close_task" => {
                let p: Task = parse(params)?;
                self.close_request_task(&p.task_id)?;
                Ok(json!({"closed":true}))
            }
            "artifacts.put" => {
                let p: crate::PutArtifactRequest = parse(params)?;
                encode(self.put_artifact(p)?)
            }
            "artifacts.update" => {
                let p: crate::UpdateArtifactRequest = parse(params)?;
                encode(self.update_artifact(p)?)
            }
            "artifacts.preview" => {
                let p: ArtifactQuery = parse(params)?;
                encode(self.preview_artifact(&p.artifact_id)?)
            }
            "artifacts.list" => {
                let p: Session = parse(params)?;
                encode(self.list_artifacts(&p.session_id)?)
            }
            "artifacts.action" => {
                let p: crate::ActionRequest = parse(params)?;
                encode(self.dispatch_artifact_action(p)?)
            }
            "artifacts.action_list" => {
                let p: Session = parse(params)?;
                encode(self.list_artifact_actions(&p.session_id)?)
            }
            "artifacts.action_get" => {
                let p: ArtifactActionQuery = parse(params)?;
                encode(self.get_artifact_action(&p.action_id)?)
            }
            "artifacts.action_cancel" => {
                let p: CancelArtifactActionQuery = parse(params)?;
                encode(self.cancel_artifact_action(&p.session_id, &p.action_id)?)
            }
            "artifacts.recovery_save" => {
                let _: Empty = parse(params)?;
                encode(self.save_artifact_recovery()?)
            }
            "artifacts.recovery_load" => {
                let _: Empty = parse(params)?;
                self.load_artifact_recovery()?;
                Ok(json!({"loaded":true}))
            }
            _ => Err(RpcError::new(
                "method_not_found",
                format!("unknown runtime method {method}"),
            )),
        }
    }
}

/// The IPC transport owns framing, authentication, request ids and disconnect
/// cancellation. Local SDK calls and this adapter execute the same Runtime API.
#[async_trait]
impl RpcHandler for Runtime {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        let context = operation_context(method, &params);
        self.dispatch_rpc(method, params, cancel)
            .await
            .map_err(|error| with_operation_context(error, &context))
    }
    /// Query methods only observe durable state. An oversized response cannot
    /// mean that an external effect happened, so the transport reports a known
    /// `response_too_large` instead of `outcome_unknown`.
    fn read_only(&self, method: &str) -> bool {
        matches!(
            method,
            "runtime.health"
                | "runtime.resources"
                | "runtime.scheduler"
                | "runtime.fleet"
                | "runtime.background.list"
                | "runtime.background.inspect"
                | "runtime.background.logs"
                | "runtime.background.result"
                | "content.inspect"
                | "session.list"
                | "session.get"
                | "session.events"
                | "session.run_inspector"
                | "session.audit"
                | "session.context_overview"
                | "session.maintenance.status"
                | "session.grants"
                | "capabilities.list"
                | "session.tasks"
                | "task.get"
                | "approval.question"
                | "memory.status"
                | "memory.entries"
                | "memory.query"
                | "work.get"
                | "work.list"
                | "input.get"
                | "notify.next"
                | "notify.get"
                | "recovery.status"
                | "recovery.reconciliation"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_worker_error_keeps_unknown_outcome_on_rpc_transport() {
        let error = RpcError::from(RuntimeError::WorkerPanicked("boom".into()));
        assert_eq!(error.code, "worker_panicked_outcome_unknown");
        let error = cayw_ipc::ClientError::from(error);
        assert!(error.outcome_unknown);
        let known = cayw_ipc::ClientError::from(RpcError::from(RuntimeError::Busy));
        assert_eq!(known.code, "busy");
        assert!(!known.outcome_unknown);
    }
}
