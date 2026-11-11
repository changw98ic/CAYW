//! Model-facing WorkItem entry points.
//!
//! These capabilities exist only in the main-session workspace. They use the
//! existing CodeMode/Host permission and audit path, the existing Task queue and
//! the existing WorkItem store. No shell, second queue, lease or usage ledger is
//! introduced. A model completion report can never write a passing acceptance
//! record, and notification capabilities only record intent and receipts.

use crate::{
    NotificationStatus, Runtime, RuntimeConfig, RuntimeError, TaskPriority, WorkControl,
    WorkDispatchRequest, WorkItem, WorkLifecycle, WorkTargetOutcome, WorkTaskQuery, WorkTaskRef,
    lock,
    work_router::{WorkRouter, WorkRouterError},
};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityStop};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const CREATE: &str = "work.create";
const LIST: &str = "work.list";
const GET: &str = "work.get";
const WORKSPACES: &str = "work.workspaces";
const SESSIONS: &str = "work.sessions";
const CREATE_SESSION: &str = "work.session.create";
const DISPATCH: &str = "work.dispatch";
const TASKS: &str = "work.tasks";
const PAUSE: &str = "work.pause";
const RESUME: &str = "work.resume";
const STOP: &str = "work.stop";
const ACCEPT_PROGRAM: &str = "work.accept.program";
const ACCEPT_USER: &str = "work.accept.user";
const REPORT: &str = "work.report";
const LATE_RESULT: &str = "work.late_result";
const NOTIFY_INTENT: &str = "work.notify.intent";
const NOTIFY_NEXT: &str = "work.notify.next";
const NOTIFY_GET: &str = "work.notify.get";
const NOTIFY_LIST: &str = "work.notify.list";
const NOTIFY_RECEIPT: &str = "work.notify.receipt";
const NOTIFY_SEND: &str = "notify.send";
const WORK_NOTIFY_SEND: &str = "work.notify.send";
const PROACTIVE_TRIGGER: &str = "proactive.trigger";
const MEMORY_STATUS: &str = "memory.status";
const MEMORY_ENTRIES: &str = "memory.entries";
const MEMORY_QUERY: &str = "memory.query";
const MEMORY_APPLY: &str = "memory.apply";
const MEMORY_FORGET: &str = "memory.forget";
const MEMORY_DREAM: &str = "memory.dream";

pub(crate) fn is_tool(name: &str) -> bool {
    matches!(
        name,
        CREATE
            | LIST
            | GET
            | WORKSPACES
            | SESSIONS
            | CREATE_SESSION
            | DISPATCH
            | TASKS
            | PAUSE
            | RESUME
            | STOP
            | ACCEPT_PROGRAM
            | ACCEPT_USER
            | REPORT
            | LATE_RESULT
            | NOTIFY_INTENT
            | NOTIFY_NEXT
            | NOTIFY_GET
            | NOTIFY_LIST
            | NOTIFY_RECEIPT
            | NOTIFY_SEND
            | WORK_NOTIFY_SEND
            | PROACTIVE_TRIGGER
            | MEMORY_STATUS
            | MEMORY_ENTRIES
            | MEMORY_QUERY
            | MEMORY_APPLY
            | MEMORY_FORGET
            | MEMORY_DREAM
    )
}

fn id(max: usize) -> Value {
    json!({"type":"string","minLength":1,"maxLength":max})
}

fn text(max: usize) -> Value {
    json!({"type":"string","minLength":1,"maxLength":max})
}

fn string_list(max_items: usize, max_bytes: usize) -> Value {
    json!({"type":"array","maxItems":max_items,"items":{"type":"string","minLength":1,"maxLength":max_bytes}})
}

fn file_check() -> Value {
    json!({"oneOf":[
        {"type":"object","additionalProperties":false,"required":["kind","path"],"properties":{
            "kind":{"const":"path_exists"},"path":text(4096)}},
        {"type":"object","additionalProperties":false,"required":["kind","path","text"],"properties":{
            "kind":{"const":"file_contains"},"path":text(4096),"text":{"type":"string","maxLength":65536}}},
        {"type":"object","additionalProperties":false,"required":["kind","path","text"],"properties":{
            "kind":{"const":"file_equals"},"path":text(4096),"text":{"type":"string","maxLength":65536}}}
    ]})
}

fn acceptance_criteria() -> Value {
    json!({
        "type":"array","minItems":1,"maxItems":64,
        "items":{"type":"object","additionalProperties":false,
            "required":["criterion_id","description"],
            "properties":{
                "criterion_id":id(256),
                "description":text(4096),
                "check":file_check()
            }
        }
    })
}

fn session_refs() -> Value {
    json!({"type":"array","maxItems":64,"items":{
        "type":"object","additionalProperties":false,"required":["session_id"],
        "properties":{"session_id":id(256),"workspace":{"type":"string","minLength":1,"maxLength":4096}}
    }})
}

fn artifact_refs() -> Value {
    string_list(64, 1024)
}

fn notification_status() -> Value {
    json!({"enum":["accepted","sent","delivered","read","failed"]})
}

/// Definitions are advertised only to the main-session workspace. The local
/// Client's workspace router is part of this Runtime's host configuration, not
/// a model argument.
pub(crate) fn definitions(config: &RuntimeConfig) -> Vec<ToolDefinition> {
    if !config.main_session {
        return vec![];
    }
    let mut prompt_text = json!({"type":"string","minLength":1});
    if let Some(max) = config.max_prompt_bytes {
        prompt_text["maxLength"] = json!(max);
    }
    vec![
        ToolDefinition {
            name: CREATE.into(),
            description: "Create one durable WorkItem owned by this main session. Acceptance criteria with a file check are observed by the program; a model report can never mark them passed. Use a criterion without `check` when only the user can judge the result.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["title","goal","acceptance_criteria"],"properties":{
                "work_id":id(256),
                "title":text(1024),
                "goal":text(65536),
                "acceptance_criteria":acceptance_criteria(),
                "user_source_refs":string_list(64,1024),
                "action_scope":string_list(64,256),
                "approval_refs":string_list(64,256),
                "session_refs":session_refs(),
                "artifact_refs":artifact_refs(),
                "waiting_conditions":string_list(64,1024),
                "pending_decisions":string_list(64,1024),
                "next_check_at":{"type":"integer","minimum":0}
            }}),
        },
        ToolDefinition {
            name: LIST.into(),
            description: "List WorkItems owned by this main session, including their lifecycle, acceptance records, task references and late results.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{}}),
        },
        ToolDefinition {
            name: GET.into(),
            description: "Read one WorkItem owned by this main session.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id"],"properties":{"work_id":id(256)}}),
        },
        ToolDefinition {
            name: WORKSPACES.into(),
            description: "List registered workspaces known to the unique Client. Use the returned `id` (or registered path) as `workspace` when dispatching. A Headless/SDK Runtime without the local Client refuses this call.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{}}),
        },
        ToolDefinition {
            name: SESSIONS.into(),
            description: "List the Sessions in one registered workspace through the unique Client. Use a returned `id` as `session_id` when dispatching; a Task must belong to a real Session in its target workspace.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["workspace"],"properties":{"workspace":text(4096)}}),
        },
        ToolDefinition {
            name: CREATE_SESSION.into(),
            description: "Create one ordinary Session in a registered workspace through the unique Client. This does not run any work; dispatch a Task to the returned Session explicitly.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["workspace"],"properties":{
                "workspace":text(4096),
                "label":{"type":"string","minLength":1,"maxLength":1024}
            }}),
        },
        ToolDefinition {
            name: DISPATCH.into(),
            description: "Dispatch one Task to an already registered workspace through the unique Client. The target stores only work_id and dispatch_key. Reusing the same dispatch_key and input returns the original Task; different input under the same key is a conflict. Paused or stopped WorkItems refuse new dispatches. The target workspace checks its pause/stop version before every later capability call.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id","workspace","session_id","text","dispatch_key"],"properties":{
                "work_id":id(256),
                "workspace":text(4096),
                "session_id":id(256),
                "label":text(1024),
                "text":prompt_text,
                "dispatch_key":text(512),
                "priority":{"enum":["low","normal","high"]},
                "declared_memory_bytes":{"type":"integer","minimum":0}
            }}),
        },
        ToolDefinition {
            name: TASKS.into(),
            description: "Read the live status of every Task dispatched for one WorkItem. Tasks are read from their owning workspace through the unique Client. A terminal Task is a fact, not acceptance and not a new dispatch.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id"],"properties":{"work_id":id(256)}}),
        },
        ToolDefinition {
            name: PAUSE.into(),
            description: "Pause one open WorkItem and push its new pause version to every already-dispatched Task through the unique Client. Later capability calls in the target workspace are refused. Repeating pause after a routing failure is safe and applies the same version again.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id"],"properties":{"work_id":id(256)}}),
        },
        ToolDefinition {
            name: RESUME.into(),
            description: "Resume one paused WorkItem so new dispatches are allowed. This does not restart a Task that the pause already stopped.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id"],"properties":{"work_id":id(256)}}),
        },
        ToolDefinition {
            name: STOP.into(),
            description: "Stop one WorkItem permanently for this version and push its stop version to every already-dispatched Task through the unique Client. Late results stay recorded and never redispatch.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id"],"properties":{"work_id":id(256)}}),
        },
        ToolDefinition {
            name: ACCEPT_PROGRAM.into(),
            description: "Run the fixed file check of one acceptance criterion in the target workspace or the main workspace and record the observed result. Caller text is never used as the result. Model self-reports cannot replace this.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id","criterion_id"],"properties":{
                "work_id":id(256),"criterion_id":id(256),
                "workspace":{"type":"string","minLength":1,"maxLength":4096}
            }}),
        },
        ToolDefinition {
            name: ACCEPT_USER.into(),
            description: "Record the user's own answer to a criterion that has no program check. `answer_ref` must identify a real user-input log record in this main session; model or tool text is refused.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id","criterion_id","passed","answer_ref"],"properties":{
                "work_id":id(256),"criterion_id":id(256),
                "passed":{"type":"boolean"},"answer_ref":text(1024)
            }}),
        },
        ToolDefinition {
            name: REPORT.into(),
            description: "Report a model completion hint. This always returns a refusal: it never writes a passing acceptance record. Use work.accept.program or work.accept.user for real evidence.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id","text"],"properties":{
                "work_id":id(256),"text":{"type":"string","minLength":1,"maxLength":65536}
            }}),
        },
        ToolDefinition {
            name: LATE_RESULT.into(),
            description: "Record a result that arrived after pause/stop. It is durable evidence only; it does not reopen the WorkItem and never redispatch. Include the pause/stop versions captured when the Task was dispatched.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id","task_id","result_ref","pause_version_at_dispatch","stop_version_at_dispatch"],"properties":{
                "work_id":id(256),"task_id":id(256),"result_ref":text(1024),
                "pause_version_at_dispatch":{"type":"integer","minimum":0},
                "stop_version_at_dispatch":{"type":"integer","minimum":0}
            }}),
        },
        ToolDefinition {
            name: NOTIFY_INTENT.into(),
            description: "Save one notification intent before any delivery. The target must exactly match the user-configured notification list. This records intent only; no network channel is opened and nothing is sent.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id","target"],"properties":{
                "work_id":id(256),"target":text(256),
                "content_ref":{"type":"string","minLength":1,"maxLength":1024},
                "artifact_refs":artifact_refs()
            }}),
        },
        ToolDefinition {
            name: NOTIFY_NEXT.into(),
            description: "Return the next allowed notification step from durable facts. Pending asks to send; an unknown result asks to query its receipt and never resends; a real service status is complete.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["notification_id"],"properties":{"notification_id":id(256)}}),
        },
        ToolDefinition {
            name: NOTIFY_GET.into(),
            description: "Read one notification intent and its recorded receipt.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["notification_id"],"properties":{"notification_id":id(256)}}),
        },
        ToolDefinition {
            name: NOTIFY_LIST.into(),
            description: "List every notification intent recorded for one WorkItem. Work completion and delivery remain separate facts.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["work_id"],"properties":{"work_id":id(256)}}),
        },
        ToolDefinition {
            name: NOTIFY_RECEIPT.into(),
            description: "Record a real service receipt for one notification. `unknown` and `pending` are not service results and are refused. A later query reads this receipt instead of resending.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["notification_id","status","receipt_ref"],"properties":{
                "notification_id":id(256),"status":notification_status(),"receipt_ref":text(1024)
            }}),
        },
        ToolDefinition {
            name: NOTIFY_SEND.into(),
            description: "Deliver a notification to a configured user target, recording the intent first and persisting the delivery receipt.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "notification_id":id(256),
                "work_id":id(256),
                "work_version":{"type":"integer","minimum":1},
                "target":text(256),
                "text":{"type":"string","maxLength":16384},
                "content_ref":{"type":"string","minLength":1,"maxLength":1024},
                "artifact_refs":artifact_refs()
            }}),
        },
        ToolDefinition {
            name: WORK_NOTIFY_SEND.into(),
            description: "Deliver a WorkItem notification to a configured user target, recording the intent first and persisting the delivery receipt.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "notification_id":id(256),
                "work_id":id(256),
                "work_version":{"type":"integer","minimum":1},
                "target":text(256),
                "text":{"type":"string","maxLength":16384},
                "content_ref":{"type":"string","minLength":1,"maxLength":1024},
                "artifact_refs":artifact_refs()
            }}),
        },
        ToolDefinition {
            name: PROACTIVE_TRIGGER.into(),
            description: "Trigger a proactive rule for one WorkItem. In auto mode this idempotently creates and dispatches the corresponding Task.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["rule_id","condition_version","work_id"],"properties":{
                "mode":{"type":"string","enum":["off","observe","auto"]},
                "rule_id":id(256),
                "condition_version":{"type":"integer","minimum":1},
                "work_id":id(256),
                "usage_tokens":{"type":"integer","minimum":0}
            }}),
        },
        ToolDefinition {
            name: MEMORY_STATUS.into(),
            description: "Read the main-session memory directory status, active count, user_set count, and forgotten ranges.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{}}),
        },
        ToolDefinition {
            name: MEMORY_ENTRIES.into(),
            description: "List visible active memory and preference entries in the main session.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{}}),
        },
        ToolDefinition {
            name: MEMORY_QUERY.into(),
            description: "Read back conversation, work, and recorded memory entries inside a session cursor range.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "session_id":id(256),
                "start_cursor":{"type":"integer","minimum":0},
                "end_cursor":{"type":"integer","minimum":1}
            }}),
        },
        ToolDefinition {
            name: MEMORY_APPLY.into(),
            description: "Apply verified memory or preference writes in the main session. Every cited source_log_id must be a user-input turn.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "session_id":id(256),
                "candidates":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["kind"],"properties":{
                    "operation":{"type":"string","enum":["upsert","disable"]},
                    "entry_id":id(128),
                    "kind":text(128),
                    "body":{"type":"string","maxLength":65536},
                    "source_type":{"type":"string","enum":["stated","inferred","user_set"]},
                    "source_log_ids":string_list(64,256)
                }}},
                "operation":{"type":"string","enum":["upsert","disable"]},
                "entry_id":id(128),
                "kind":text(128),
                "body":{"type":"string","maxLength":65536},
                "source_type":{"type":"string","enum":["stated","inferred","user_set"]},
                "source_log_ids":string_list(64,256)
            }}),
        },
        ToolDefinition {
            name: MEMORY_FORGET.into(),
            description: "Record a forgotten cursor range (or entry_id) in the main session so future context and memory reads omit it without deleting durable logs.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["reason"],"properties":{
                "session_id":id(256),
                "start_cursor":{"type":"integer","minimum":1},
                "end_cursor":{"type":"integer","minimum":1},
                "entry_id":id(128),
                "reason":text(4096)
            }}),
        },
        ToolDefinition {
            name: MEMORY_DREAM.into(),
            description: "Organize summary references in the main session without rewriting summaries or logs.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "session_id":id(256),
                "summaries":{"type":"array","items":{"type":"object"}},
                "model_text":{"type":"string","maxLength":65536}
            }}),
        },
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    #[serde(default)]
    work_id: Option<String>,
    title: String,
    goal: String,
    acceptance_criteria: Vec<crate::AcceptanceCriterion>,
    #[serde(default)]
    user_source_refs: Vec<String>,
    #[serde(default)]
    action_scope: Vec<String>,
    #[serde(default)]
    approval_refs: Vec<String>,
    #[serde(default)]
    session_refs: Vec<crate::WorkSessionRef>,
    #[serde(default)]
    artifact_refs: Vec<String>,
    #[serde(default)]
    waiting_conditions: Vec<String>,
    #[serde(default)]
    pending_decisions: Vec<String>,
    #[serde(default)]
    next_check_at: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkId {
    work_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceInput {
    workspace: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionCreateInput {
    workspace: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchInput {
    work_id: String,
    workspace: String,
    session_id: String,
    #[serde(default)]
    label: Option<String>,
    text: String,
    dispatch_key: String,
    #[serde(default)]
    priority: TaskPriority,
    #[serde(default)]
    declared_memory_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgramAccept {
    work_id: String,
    criterion_id: String,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserAccept {
    work_id: String,
    criterion_id: String,
    passed: bool,
    answer_ref: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportInput {
    work_id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LateResultInput {
    work_id: String,
    task_id: String,
    result_ref: String,
    pause_version_at_dispatch: u64,
    stop_version_at_dispatch: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyIntent {
    work_id: String,
    target: String,
    #[serde(default)]
    content_ref: Option<String>,
    #[serde(default)]
    artifact_refs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationId {
    notification_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptInput {
    notification_id: String,
    status: NotificationStatus,
    receipt_ref: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifySendInput {
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
struct ProactiveTriggerInput {
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
struct MemoryQueryInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    start_cursor: u64,
    #[serde(default)]
    end_cursor: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryApplyInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    candidates: Vec<crate::MemoryCandidate>,
    #[serde(default)]
    operation: Option<crate::MemoryOperation>,
    #[serde(default)]
    entry_id: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    source_type: Option<crate::SourceType>,
    #[serde(default)]
    source_log_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryForgetInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    start_cursor: Option<u64>,
    #[serde(default)]
    end_cursor: Option<u64>,
    #[serde(default)]
    entry_id: Option<String>,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryDreamInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    summaries: Option<Vec<cayw_context::SummaryRecord>>,
    #[serde(default)]
    model_text: Option<String>,
}

pub(crate) async fn invoke(
    scope: &Arc<crate::cells::CallScope>,
    base: &cayw_host::Host,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let definitions = definitions(&scope.context.inner.config);
    let definition = definitions
        .into_iter()
        .find(|definition| definition.name == scope.name)
        .ok_or_else(|| known("work capability is not available in this Runtime"))?;
    let host = crate::bridge::scoped_host(base, scope)?;
    let service = Service { scope };
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let run = host.call_service(&definition, arguments, child.clone(), &service);
    crate::requests::watch(scope, child, run).await
}

struct Service<'a> {
    scope: &'a Arc<crate::cells::CallScope>,
}

impl Service<'_> {
    fn owner(&self) -> &str {
        &self.scope.context.lease.session_id
    }

    fn runtime(&self) -> Runtime {
        Runtime {
            inner: self.scope.context.inner.clone(),
        }
    }

    fn router(&self) -> Result<Arc<dyn WorkRouter>, CapabilityError> {
        lock(&self.scope.context.inner.work_router)
            .clone()
            .ok_or_else(|| {
                known(
                    "workspace routing is unavailable; run the main session through the unique local Client",
                )
            })
    }

    fn item(&self, work_id: &str) -> Result<WorkItem, CapabilityError> {
        let item = self
            .scope
            .context
            .inner
            .work_items
            .get(work_id)
            .map_err(known)?
            .ok_or_else(|| known("WorkItem not found"))?;
        if item.owner_main_session_id != self.owner() {
            return Err(known("WorkItem belongs to a different main session"));
        }
        Ok(item)
    }
}

fn router_error(error: WorkRouterError) -> CapabilityError {
    if error.outcome_unknown {
        CapabilityError::unknown(error.message)
    } else {
        known(error.message)
    }
}

fn cancellation_unknown(message: impl Into<String>) -> CapabilityError {
    let mut error = CapabilityError::stopped(CapabilityStop::Cancelled, message);
    error.outcome_unknown = true;
    error
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub(crate) fn external_workspaces(item: &WorkItem) -> Vec<String> {
    target_workspaces(item)
        .into_iter()
        .filter(|workspace| workspace != "local")
        .collect()
}

fn target_workspaces(item: &WorkItem) -> Vec<String> {
    let mut targets = Vec::new();
    for workspace in item
        .task_refs
        .iter()
        .filter_map(|task| task.workspace.as_deref())
        .chain(
            item.session_refs
                .iter()
                .filter_map(|session| session.workspace.as_deref()),
        )
    {
        if !workspace.trim().is_empty() && !targets.iter().any(|value| value == workspace) {
            targets.push(workspace.to_string());
        }
    }
    targets
}

#[async_trait]
impl cayw_host::ExternalHost for Service<'_> {
    async fn call(
        &self,
        capability: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if capability != self.scope.name {
            return Err(known("work service differs from the original capability"));
        }
        self.scope.check_authority(&cancel)?;
        match capability {
            CREATE => {
                let input: CreateInput = serde_json::from_value(arguments).map_err(known)?;
                for session in &input.session_refs {
                    if session.session_id.trim().is_empty()
                        || session
                            .workspace
                            .as_deref()
                            .is_some_and(|value| value.trim().is_empty())
                    {
                        return Err(known("work session references must not be empty"));
                    }
                }
                let item = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .create(crate::WorkItemSpec {
                        work_id: input.work_id,
                        owner_main_session_id: self.owner().to_string(),
                        title: input.title,
                        goal: input.goal,
                        acceptance_criteria: input.acceptance_criteria,
                        user_source_refs: input.user_source_refs,
                        action_scope: input.action_scope,
                        approval_refs: input.approval_refs,
                        session_refs: input.session_refs,
                        task_refs: Vec::new(),
                        artifact_refs: input.artifact_refs,
                        waiting_conditions: input.waiting_conditions,
                        pending_decisions: input.pending_decisions,
                        next_check_at: input.next_check_at,
                    })
                    .map_err(known)?;
                encode(item)
            }
            LIST => {
                let items = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .list()
                    .map_err(known)?
                    .into_iter()
                    .filter(|item| item.owner_main_session_id == self.owner())
                    .collect::<Vec<_>>();
                encode(items)
            }
            GET => {
                let input: WorkId = serde_json::from_value(arguments).map_err(known)?;
                encode(self.item(&input.work_id)?)
            }
            WORKSPACES => {
                let workspaces = self.router()?.workspaces().await.map_err(router_error)?;
                Ok(Value::Array(workspaces))
            }
            SESSIONS => {
                let input: WorkspaceInput = serde_json::from_value(arguments).map_err(known)?;
                let sessions = self
                    .router()?
                    .sessions(&input.workspace)
                    .await
                    .map_err(router_error)?;
                Ok(Value::Array(sessions))
            }
            CREATE_SESSION => {
                let input: SessionCreateInput = serde_json::from_value(arguments).map_err(known)?;
                let label = input.label.as_deref().unwrap_or("Work");
                self.router()?
                    .create_session(&input.workspace, label)
                    .await
                    .map_err(router_error)
            }
            DISPATCH => {
                let input: DispatchInput = serde_json::from_value(arguments).map_err(known)?;
                let item = self.item(&input.work_id)?;
                if item.lifecycle != WorkLifecycle::Open {
                    return Err(known(
                        "only an open WorkItem can dispatch; paused or stopped work is refused",
                    ));
                }
                let request = WorkDispatchRequest {
                    work_id: item.work_id.clone(),
                    workspace: input.workspace,
                    session_id: Some(input.session_id),
                    label: input.label,
                    text: input.text,
                    dispatch_key: input.dispatch_key.clone(),
                    pause_version: item.pause_version,
                    stop_version: item.stop_version,
                    priority: input.priority,
                    declared_memory_bytes: input.declared_memory_bytes,
                };
                let reply = match self.router()?.dispatch(request).await {
                    Ok(reply) => reply,
                    Err(error) if cancel.is_cancelled() => {
                        return Err(cancellation_unknown(format!(
                            "dispatch {} may have been recorded; query work.tasks before retrying: {}",
                            input.dispatch_key, error
                        )));
                    }
                    Err(error) => return Err(router_error(error)),
                };
                let updated = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .attach_task_ref(
                        &item.work_id,
                        WorkTaskRef {
                            task_id: reply.task_id.clone(),
                            session_id: Some(reply.session_id.clone()),
                            workspace: Some(reply.workspace.clone()),
                        },
                    )
                    .map_err(known)?;
                Ok(json!({"work":updated,"dispatch":reply}))
            }
            TASKS => {
                let input: WorkId = serde_json::from_value(arguments).map_err(known)?;
                let item = self.item(&input.work_id)?;
                let router = if item.task_refs.is_empty() {
                    None
                } else {
                    Some(self.router()?)
                };
                let mut tasks = Vec::new();
                for task in &item.task_refs {
                    let workspace = task.workspace.clone().ok_or_else(|| {
                        known("a task reference has no workspace and cannot be read")
                    })?;
                    let found = router
                        .as_ref()
                        .expect("task references require a router")
                        .task(WorkTaskQuery {
                            workspace,
                            task_id: Some(task.task_id.clone()),
                            dispatch_key: None,
                        })
                        .await
                        .map_err(router_error)?;
                    tasks.push(json!({"task_ref":task,"task":found}));
                }
                Ok(json!({
                    "work":item,
                    "tasks":tasks,
                    "late_results":item.late_results,
                }))
            }
            PAUSE | STOP => {
                let input: WorkId = serde_json::from_value(arguments).map_err(known)?;
                let item = self.item(&input.work_id)?;
                let action = if capability == PAUSE {
                    WorkControl::Pause
                } else {
                    WorkControl::Stop
                };
                let updated = match (action, item.lifecycle) {
                    (WorkControl::Pause, WorkLifecycle::Open) => {
                        self.scope.context.inner.work_items.pause(&item.work_id)
                    }
                    (WorkControl::Pause, WorkLifecycle::Paused) => Ok(item.clone()),
                    (WorkControl::Stop, WorkLifecycle::Open | WorkLifecycle::Paused) => {
                        self.scope.context.inner.work_items.stop(&item.work_id)
                    }
                    (WorkControl::Stop, WorkLifecycle::Cancelled) => Ok(item.clone()),
                    (WorkControl::Pause, _) => Err(RuntimeError::InvalidInput(
                        "only an open or already-paused WorkItem can be paused".into(),
                    )),
                    (WorkControl::Stop, WorkLifecycle::Completed) => Err(
                        RuntimeError::InvalidInput("a completed WorkItem cannot be stopped".into()),
                    ),
                }
                .map_err(known)?;
                let version = match action {
                    WorkControl::Pause => updated.pause_version,
                    WorkControl::Stop => updated.stop_version,
                };
                let local = match action {
                    WorkControl::Pause => self
                        .scope
                        .context
                        .inner
                        .queue
                        .pause_work(&updated.work_id, version),
                    WorkControl::Stop => self
                        .scope
                        .context
                        .inner
                        .queue
                        .stop_work(&updated.work_id, version),
                }
                .map_err(known)?;
                let targets = target_workspaces(&updated);
                let router = lock(&self.scope.context.inner.work_router).clone();
                let outcomes = if targets.is_empty() {
                    vec![WorkTargetOutcome {
                        workspace: "local".into(),
                        updated: Some(local),
                        error: None,
                    }]
                } else if let Some(router) = router {
                    router
                        .control(action, &updated.work_id, version, &targets)
                        .await
                        .map_err(|error| {
                            if cancel.is_cancelled() {
                                cancellation_unknown(format!(
                                    "work control may have reached some target workspaces: {error}"
                                ))
                            } else {
                                router_error(error)
                            }
                        })?
                } else {
                    return Err(known(
                        "the WorkItem was updated, but workspace routing is unavailable; dispatched Tasks were not updated and the same call may be retried after the Client is available",
                    ));
                };
                let complete = outcomes.iter().all(|outcome| outcome.error.is_none());
                let item = self.item(&updated.work_id)?;
                Ok(json!({
                    "work":item,
                    "local_updated":local,
                    "targets":outcomes,
                    "complete":complete,
                }))
            }
            RESUME => {
                let input: WorkId = serde_json::from_value(arguments).map_err(known)?;
                let item = self.item(&input.work_id)?;
                if item.lifecycle != WorkLifecycle::Paused {
                    return Err(known("only a paused WorkItem can be resumed"));
                }
                let resumed = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .resume(&item.work_id)
                    .map_err(known)?;
                encode(resumed)
            }
            ACCEPT_PROGRAM => {
                let input: ProgramAccept = serde_json::from_value(arguments).map_err(known)?;
                let item = self.item(&input.work_id)?;
                let criterion = item
                    .acceptance_criteria
                    .iter()
                    .find(|criterion| criterion.criterion_id == input.criterion_id)
                    .ok_or_else(|| known("unknown acceptance criterion"))?;
                let check = criterion.check.clone().ok_or_else(|| {
                    known("this criterion has no program check; ask the user with work.accept.user")
                })?;
                let targets = target_workspaces(&item);
                let workspace = input
                    .workspace
                    .or_else(|| (targets.len() == 1).then(|| targets[0].clone()));
                let (passed, evidence, observed_in) = match workspace {
                    Some(workspace) => {
                        if !targets.iter().any(|target| target == &workspace) {
                            return Err(known(
                                "acceptance workspace is not associated with this WorkItem",
                            ));
                        }
                        let passed = self
                            .router()?
                            .evaluate_check(&workspace, &check)
                            .await
                            .map_err(router_error)?;
                        (
                            passed,
                            format!("program check {check:?} observed in workspace {workspace}"),
                            workspace,
                        )
                    }
                    None if targets.is_empty() => {
                        let passed = check
                            .evaluate(self.scope.context.inner.root.as_path())
                            .map_err(known)?;
                        (
                            passed,
                            format!("program check {check:?} observed in the main workspace"),
                            "local".into(),
                        )
                    }
                    None => {
                        return Err(known(
                            "multiple workspaces are associated with this WorkItem; pass workspace explicitly",
                        ));
                    }
                };
                let updated = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .record_program_acceptance_at(
                        &item.work_id,
                        &input.criterion_id,
                        passed,
                        vec![evidence],
                        "runtime-program",
                        now_ms(),
                    )
                    .map_err(known)?;
                Ok(json!({
                    "work":updated,
                    "criterion_id":input.criterion_id,
                    "passed":passed,
                    "workspace":observed_in,
                }))
            }
            ACCEPT_USER => {
                let input: UserAccept = serde_json::from_value(arguments).map_err(known)?;
                self.item(&input.work_id)?;
                let updated = self
                    .runtime()
                    .record_work_user_acceptance(
                        &input.work_id,
                        &input.criterion_id,
                        input.passed,
                        &input.answer_ref,
                    )
                    .map_err(known)?;
                encode(updated)
            }
            REPORT => {
                let input: ReportInput = serde_json::from_value(arguments).map_err(known)?;
                self.item(&input.work_id)?;
                self.runtime()
                    .record_work_model_report(&input.work_id, &input.text)
                    .map_err(known)?;
                Ok(json!({"recorded":false}))
            }
            LATE_RESULT => {
                let input: LateResultInput = serde_json::from_value(arguments).map_err(known)?;
                self.item(&input.work_id)?;
                let late = self
                    .runtime()
                    .record_work_late_result(
                        &input.work_id,
                        &input.task_id,
                        &input.result_ref,
                        input.pause_version_at_dispatch,
                        input.stop_version_at_dispatch,
                    )
                    .map_err(known)?;
                let item = self.item(&input.work_id)?;
                Ok(json!({"work":item,"late_result":late}))
            }
            NOTIFY_INTENT => {
                let input: NotifyIntent = serde_json::from_value(arguments).map_err(known)?;
                let item = self.item(&input.work_id)?;
                let notification = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .prepare_notification(
                        &item.work_id,
                        item.work_version,
                        &input.target,
                        input.content_ref.as_deref(),
                        &input.artifact_refs,
                        &self.scope.context.inner.config.notification_targets,
                    )
                    .map_err(known)?;
                let decision = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .notification_decision(&notification.notification_id)
                    .map_err(known)?;
                Ok(json!({"notification":notification,"decision":decision}))
            }
            NOTIFY_NEXT => {
                let input: NotificationId = serde_json::from_value(arguments).map_err(known)?;
                let notification = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .notification(&input.notification_id)
                    .map_err(known)?
                    .ok_or_else(|| known("notification not found"))?;
                self.item(&notification.work_id)?;
                let decision = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .notification_decision(&input.notification_id)
                    .map_err(known)?;
                Ok(json!({"notification":notification,"decision":decision}))
            }
            NOTIFY_GET => {
                let input: NotificationId = serde_json::from_value(arguments).map_err(known)?;
                let notification = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .notification(&input.notification_id)
                    .map_err(known)?
                    .ok_or_else(|| known("notification not found"))?;
                self.item(&notification.work_id)?;
                encode(notification)
            }
            NOTIFY_LIST => {
                let input: WorkId = serde_json::from_value(arguments).map_err(known)?;
                self.item(&input.work_id)?;
                encode(
                    self.scope
                        .context
                        .inner
                        .work_items
                        .notifications_for_work(&input.work_id)
                        .map_err(known)?,
                )
            }
            NOTIFY_RECEIPT => {
                let input: ReceiptInput = serde_json::from_value(arguments).map_err(known)?;
                let notification = self
                    .scope
                    .context
                    .inner
                    .work_items
                    .notification(&input.notification_id)
                    .map_err(known)?
                    .ok_or_else(|| known("notification not found"))?;
                self.item(&notification.work_id)?;
                encode(
                    self.scope
                        .context
                        .inner
                        .work_items
                        .record_notification_receipt(
                            &input.notification_id,
                            input.status,
                            &input.receipt_ref,
                        )
                        .map_err(known)?,
                )
            }
            NOTIFY_SEND | WORK_NOTIFY_SEND => {
                let input: NotifySendInput = serde_json::from_value(arguments).map_err(known)?;
                if let Some(work_id) = input.work_id.as_deref() {
                    self.item(work_id)?;
                }
                let notification = self
                    .runtime()
                    .send_notification(
                        input.notification_id.as_deref(),
                        input.work_id.as_deref(),
                        input.work_version,
                        input.target.as_deref(),
                        input.text.as_deref(),
                        input.content_ref.as_deref(),
                        &input.artifact_refs,
                    )
                    .map_err(known)?;
                encode(notification)
            }
            PROACTIVE_TRIGGER => {
                let input: ProactiveTriggerInput =
                    serde_json::from_value(arguments).map_err(known)?;
                self.item(&input.work_id)?;
                let runtime = self.runtime();
                let outcome = match input.mode {
                    Some(mode) => runtime.trigger_proactive(
                        mode,
                        &input.rule_id,
                        input.condition_version,
                        &input.work_id,
                        input.usage_tokens,
                    ),
                    None => runtime.trigger_configured_proactive(
                        &input.rule_id,
                        input.condition_version,
                        &input.work_id,
                        input.usage_tokens,
                    ),
                }
                .map_err(known)?;
                let _ = runtime.run_proactive_dispatch(&outcome).await;
                encode(outcome)
            }
            MEMORY_STATUS => {
                let status = self.runtime().memory_status().map_err(known)?;
                encode(status)
            }
            MEMORY_ENTRIES => {
                let entries = self.runtime().memory_entries().map_err(known)?;
                encode(entries)
            }
            MEMORY_QUERY => {
                let input: MemoryQueryInput = serde_json::from_value(arguments).map_err(known)?;
                let session_id = input.session_id.unwrap_or_else(|| self.owner().to_owned());
                let view = self
                    .runtime()
                    .query_memory(crate::MemoryQuery {
                        session_id,
                        start_cursor: input.start_cursor,
                        end_cursor: input.end_cursor,
                    })
                    .map_err(known)?;
                encode(view)
            }
            MEMORY_APPLY => {
                let input: MemoryApplyInput = serde_json::from_value(arguments).map_err(known)?;
                let runtime = self.runtime();
                let session_id = input.session_id.unwrap_or_else(|| self.owner().to_owned());
                let mut candidates = input.candidates;
                if candidates.is_empty() {
                    let kind = input
                        .kind
                        .ok_or_else(|| known("memory.apply requires candidates or kind"))?;
                    candidates.push(crate::MemoryCandidate {
                        operation: input.operation.unwrap_or(crate::MemoryOperation::Upsert),
                        entry_id: input.entry_id,
                        kind,
                        body: input.body.unwrap_or_default(),
                        source_type: input.source_type.unwrap_or(crate::SourceType::Stated),
                        source_log_ids: input.source_log_ids,
                    });
                }
                let evidence = runtime.memory_user_evidence(&session_id).map_err(known)?;
                let latest_user_log_id = evidence
                    .sources()
                    .max_by_key(|source| source.cursor())
                    .map(|source| source.log_id().to_owned());
                if let Some(latest) = latest_user_log_id {
                    for candidate in &mut candidates {
                        if candidate.source_log_ids.is_empty() {
                            candidate.source_log_ids.push(latest.clone());
                        }
                    }
                }
                let report = runtime
                    .apply_memory_candidates(&session_id, &candidates)
                    .map_err(known)?;
                encode(report)
            }
            MEMORY_FORGET => {
                let input: MemoryForgetInput = serde_json::from_value(arguments).map_err(known)?;
                let runtime = self.runtime();
                let session_id = input.session_id.unwrap_or_else(|| self.owner().to_owned());
                if let (Some(start_cursor), Some(end_cursor)) =
                    (input.start_cursor, input.end_cursor)
                {
                    let range =
                        cayw_context::SourceRange::new(&session_id, start_cursor, end_cursor)
                            .map_err(known)?;
                    runtime
                        .forget_memory_range(range, &input.reason)
                        .map_err(known)?;
                } else if let Some(entry_id) = input.entry_id.as_deref() {
                    let entries = runtime.memory_entries().map_err(known)?;
                    let entry = entries
                        .into_iter()
                        .find(|entry| entry.entry_id == entry_id)
                        .ok_or_else(|| known("memory entry not found"))?;
                    for log_id in &entry.source_log_ids {
                        let (src_session, cursor_str) = log_id
                            .rsplit_once(':')
                            .ok_or_else(|| known("invalid source log id"))?;
                        let cursor = cursor_str.parse::<u64>().map_err(known)?;
                        let range = cayw_context::SourceRange::new(src_session, cursor, cursor)
                            .map_err(known)?;
                        runtime
                            .forget_memory_range(range, &input.reason)
                            .map_err(known)?;
                    }
                } else {
                    return Err(known(
                        "memory.forget requires start_cursor and end_cursor, or entry_id",
                    ));
                }
                Ok(json!({"forgotten": true}))
            }
            MEMORY_DREAM => {
                let input: MemoryDreamInput = serde_json::from_value(arguments).map_err(known)?;
                let runtime = self.runtime();
                let session_id = input.session_id.unwrap_or_else(|| self.owner().to_owned());
                let summaries = match input.summaries {
                    Some(summaries) => summaries,
                    None => runtime.context_summaries(&session_id).map_err(known)?,
                };
                let plan = runtime
                    .dream_memory(&summaries, input.model_text.as_deref())
                    .map_err(known)?;
                encode(plan)
            }
            _ => Err(known("not a work capability")),
        }
    }
}

fn encode(value: impl serde::Serialize) -> Result<Value, CapabilityError> {
    serde_json::to_value(value).map_err(known)
}

fn known(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_schema_only_limits_prompt_text_when_configured() {
        for max_prompt_bytes in [None, Some(1024), Some(512 * 1024)] {
            let config = RuntimeConfig {
                main_session: true,
                max_prompt_bytes,
                ..Default::default()
            };
            let dispatch = definitions(&config)
                .into_iter()
                .find(|definition| definition.name == DISPATCH)
                .unwrap();
            let schema = &dispatch.parameters["properties"]["text"];
            assert_eq!(schema["type"], "string");
            assert_eq!(schema["minLength"], 1);
            match max_prompt_bytes {
                Some(max) => assert_eq!(schema["maxLength"], json!(max)),
                None => assert!(schema.get("maxLength").is_none()),
            }
        }
    }
}
