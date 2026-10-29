//! Wire cayw-artifacts into Runtime/Host (F09/F19 product path).
//!
//! `RecordingHostDispatch` remains the offline/test double inside
//! `cayw-artifacts`. The default product consumer is [`RuntimeHostDispatch`]:
//! - `FollowUpTask` → [`Runtime::enqueue`] (button → queued task)
//! - `InvokeTool` → authorized [`cayw_host::Host::call`] against
//!   [`RuntimeConfig::permissions`] (exact scope; never a new capability grant)

use crate::{Inner, Runtime, RuntimeError, ScopedPermission, lock, permissions::canonical_scope};
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_artifacts::{
    ActionIntent, ActionKind, ActionOutcome, ActionProtocol, ActionRecord, ActionRequest,
    ActionStatus, ArtifactError, ArtifactPreview, ArtifactReconciledOutcome,
    ArtifactReconciliation, ArtifactRecord, ArtifactStore, ArtifactWorkspace, FollowUpDispatch,
    HostActionDispatch, HostDispatchError, HostDispatchResult, PutArtifactRequest, RecoveryBundle,
    SharedHostDispatch, ToolDispatch, UpdateArtifactRequest, load_recovery, validate_id,
};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, Host, PermissionGate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

/// Process-local artifact workspace owned by one Runtime.
#[derive(Debug)]
pub(crate) struct ArtifactState {
    pub workspace: ArtifactWorkspace,
}

/// In-flight Host/Runtime work spawned from an artifact action.
#[derive(Clone)]
enum ArtifactFlight {
    Tool {
        cancel: CancellationToken,
        _executor: Option<Arc<crate::coordination::BackgroundLock>>,
    },
    FollowUp {
        session_id: String,
        task_id: String,
    },
}

/// Real HostActionDispatch: follow-ups become Runtime tasks; tools hit Host.
pub struct RuntimeHostDispatch {
    inner: Weak<Inner>,
    flights: Mutex<BTreeMap<String, ArtifactFlight>>,
}

impl RuntimeHostDispatch {
    pub(crate) fn new(inner: Weak<Inner>) -> Self {
        Self {
            inner,
            flights: Mutex::new(BTreeMap::new()),
        }
    }

    fn runtime(&self) -> Result<Runtime, String> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| "runtime closed".to_string())?;
        Ok(Runtime { inner })
    }

    fn register_flight(&self, action_id: &str, flight: ArtifactFlight) {
        lock(&self.flights).insert(action_id.to_owned(), flight);
    }

    fn take_flight(&self, action_id: &str) -> Option<ArtifactFlight> {
        lock(&self.flights).remove(action_id)
    }

    pub(crate) fn is_flight_active(&self, action_id: &str) -> bool {
        let mut flights = lock(&self.flights);
        if let Some(flight) = flights.get(action_id) {
            match flight {
                ArtifactFlight::Tool { .. } => true,
                ArtifactFlight::FollowUp {
                    session_id: _,
                    task_id,
                } => {
                    if let Ok(runtime) = self.runtime() {
                        if let Ok(task) = runtime.inner.queue.get(task_id) {
                            match task.state {
                                crate::TaskState::Queued
                                | crate::TaskState::Running
                                | crate::TaskState::WaitingApproval
                                | crate::TaskState::Draining => true,
                                crate::TaskState::Completed
                                | crate::TaskState::Failed
                                | crate::TaskState::Cancelled
                                | crate::TaskState::Interrupted
                                | crate::TaskState::OutcomeUnknown => {
                                    flights.remove(action_id);
                                    false
                                }
                            }
                        } else {
                            flights.remove(action_id);
                            false
                        }
                    } else {
                        flights.remove(action_id);
                        false
                    }
                }
            }
        } else {
            false
        }
    }

    fn session_has_unknown(&self, runtime: &Runtime, session_id: &str) -> Result<bool, String> {
        runtime
            .has_artifact_unknown(session_id)
            .map_err(|e| e.to_string())
    }
}

impl HostActionDispatch for RuntimeHostDispatch {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn follow_up_task(
        &self,
        req: FollowUpDispatch<'_>,
    ) -> Result<HostDispatchResult, HostDispatchError> {
        let runtime = self.runtime().map_err(HostDispatchError::rejected)?;
        if self
            .session_has_unknown(&runtime, req.session_id)
            .map_err(HostDispatchError::rejected)?
        {
            return Err(HostDispatchError::rejected(format!(
                "session {} requires reconciliation before dispatching follow-up tasks",
                req.session_id
            )));
        }
        // Session must exist and be ready; same admission path as trusted host prompts.
        let task = runtime
            .enqueue(req.session_id, req.prompt)
            .map_err(|error| HostDispatchError::rejected(error.to_string()))?;
        let target = task.cancel_target();
        self.register_flight(
            req.action_id,
            ArtifactFlight::FollowUp {
                session_id: req.session_id.to_owned(),
                task_id: task.id.clone(),
            },
        );
        Ok(HostDispatchResult {
            outcome_json: json!({
                "kind": "follow_up_task_dispatched",
                "session_id": req.session_id,
                "artifact_id": req.artifact_id,
                "action_id": req.action_id,
                "prompt": req.prompt,
                "task_id": task.id,
                "task_state": task.state,
                "cancel_target": target,
            })
            .to_string(),
        })
    }

    fn invoke_tool(&self, req: ToolDispatch<'_>) -> Result<HostDispatchResult, HostDispatchError> {
        let runtime = self.runtime().map_err(HostDispatchError::rejected)?;
        let _executor = {
            let _coordinate = runtime
                .inner
                .coordination
                .enter()
                .map_err(|error| HostDispatchError::rejected(error.to_string()))?;
            let executor = runtime
                .artifact_executor(req.session_id)
                .map_err(|error| HostDispatchError::rejected(error.to_string()))?;
            let session = runtime
                .get(req.session_id)
                .map_err(|error| HostDispatchError::rejected(error.to_string()))?;
            if session.attempt.is_some()
                || runtime
                    .inner
                    .queue
                    .list(req.session_id)
                    .map_err(|error| HostDispatchError::rejected(error.to_string()))?
                    .iter()
                    .any(|task| task_is_active(task.state))
            {
                return Err(HostDispatchError::rejected(
                    "session already has active work",
                ));
            }
            if runtime
                .artifact_unknown(req.session_id, true)
                .map_err(|error| HostDispatchError::rejected(error.to_string()))?
            {
                return Err(HostDispatchError::rejected(format!(
                    "session {} requires reconciliation before invoking tools",
                    req.session_id
                )));
            }
            if runtime
                .artifact_audit(req.session_id)
                .map_err(|error| HostDispatchError::rejected(error.to_string()))?
                .actions
                .contains_key(req.action_id)
            {
                return Err(HostDispatchError::rejected(
                    "artifact action was already dispatched",
                ));
            }
            executor
        };
        let args: Value = serde_json::from_str(req.args_json).map_err(|error| {
            HostDispatchError::rejected(format!("invalid invoke_tool args_json: {error}"))
        })?;
        let permissions = runtime.inner.config.permissions.clone();
        let scope = canonical_scope(&args);
        if !permissions
            .iter()
            .any(|p| p.capability == req.tool && p.scope == scope)
        {
            return Err(HostDispatchError::rejected(format!(
                "tool `{}` with scope `{scope}` is not preauthorized in RuntimeConfig.permissions",
                req.tool
            )));
        }
        let host = artifact_tool_host(
            &runtime.inner,
            req.session_id,
            req.action_id,
            req.artifact_id,
            permissions,
        )
        .map_err(HostDispatchError::rejected)?;
        let executor = Arc::new(_executor);
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_worker = Arc::clone(&stopped);
        let tool = req.tool.to_owned();
        let cancel = CancellationToken::new();
        self.register_flight(
            req.action_id,
            ArtifactFlight::Tool {
                cancel: cancel.clone(),
                _executor: Some(Arc::clone(&executor)),
            },
        );
        let inner = runtime.inner.clone();
        let session_id = req.session_id.to_owned();
        let action_id = req.action_id.to_owned();
        let artifact_id = req.artifact_id.to_owned();
        let result = block_on_host(async move {
            let _executor = executor;
            let result = host.call(&tool, args, cancel).await;
            host.close_processes();
            if let Err(error) = host.drain_processes().await {
                let message = format!("artifact tool cleanup is unconfirmed: {error}");
                inner
                    .journal
                    .record(
                        &session_id,
                        None,
                        "artifact.action.outcome_unknown",
                        json!({
                            "action_id": action_id,
                            "artifact_id": artifact_id,
                            "error": message,
                        }),
                    )
                    .map_err(|error| CapabilityError::unknown(error.to_string()))?;
                return Err(CapabilityError::unknown(message));
            }
            stopped_worker.store(true, Ordering::Release);
            result
        });
        if stopped.load(Ordering::Acquire) {
            self.take_flight(req.action_id);
        }
        let result = match result {
            Ok(val) => val,
            Err(err) => {
                if err.outcome_unknown {
                    return Err(HostDispatchError::unknown(err.message));
                } else {
                    return Err(HostDispatchError::rejected(err.message));
                }
            }
        };
        Ok(HostDispatchResult {
            outcome_json: json!({
                "kind": "tool_invoked",
                "session_id": req.session_id,
                "artifact_id": req.artifact_id,
                "action_id": req.action_id,
                "tool": req.tool,
                "args_json": req.args_json,
                "result": result,
            })
            .to_string(),
        })
    }

    fn cancel_action(&self, action_id: &str) -> Result<bool, HostDispatchError> {
        let flight = {
            let flights = lock(&self.flights);
            flights.get(action_id).cloned()
        };
        let Some(flight) = flight else {
            return Ok(false);
        };
        match flight {
            ArtifactFlight::Tool { cancel, .. } => {
                cancel.cancel();
                Ok(true)
            }
            ArtifactFlight::FollowUp {
                session_id,
                task_id,
            } => {
                let runtime = self.runtime().map_err(HostDispatchError::rejected)?;
                let task = runtime
                    .inner
                    .queue
                    .get(&task_id)
                    .map_err(|error| HostDispatchError::rejected(error.to_string()))?;
                if task.session_id != session_id {
                    return Err(HostDispatchError::rejected(format!(
                        "task {task_id} does not belong to session {session_id}"
                    )));
                }
                let target = task.cancel_target();
                let cancelled = runtime.cancel(&target).map_err(|error| {
                    let unknown = match &error {
                        RuntimeError::NeedsReconciliation => true,
                        RuntimeError::RecoveryRequired {
                            outcome_unknown, ..
                        } => *outcome_unknown,
                        _ => false,
                    };
                    if unknown {
                        HostDispatchError::unknown(error.to_string())
                    } else {
                        HostDispatchError::rejected(error.to_string())
                    }
                })?;
                if cancelled
                    || !matches!(
                        task.state,
                        crate::TaskState::Queued
                            | crate::TaskState::Running
                            | crate::TaskState::WaitingApproval
                            | crate::TaskState::Draining
                    )
                {
                    self.take_flight(action_id);
                }
                Ok(cancelled)
            }
        }
    }
}

fn artifact_tool_host(
    inner: &Arc<Inner>,
    session_id: &str,
    action_id: &str,
    artifact_id: &str,
    permissions: Vec<ScopedPermission>,
) -> Result<Host, String> {
    let gate = Arc::new(PreauthorizedGate { permissions });
    let audit = Arc::new(ArtifactAudit {
        inner: Arc::downgrade(inner),
        session_id: session_id.to_string(),
        action_id: action_id.to_string(),
        artifact_id: artifact_id.to_string(),
    });
    inner
        .host_root
        .task_host(gate, audit)
        .map_err(|error| error.to_string())
}

/// Drive an async Host call from the sync [`HostActionDispatch`] bridge.
///
/// Spawns a dedicated OS thread with its own current-thread runtime so this
/// works under both multi-thread and `#[tokio::test]` current-thread executors
/// without `block_in_place`.
fn block_on_host<F, T>(fut: F) -> Result<T, CapabilityError>
where
    F: std::future::Future<Output = Result<T, CapabilityError>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name("cayw-artifact-host".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| CapabilityError::unknown(error.to_string()))?;
            rt.block_on(fut)
        })
        .map_err(|error| CapabilityError::unknown(error.to_string()))?
        .join()
        .map_err(|_| CapabilityError::unknown("artifact host worker panicked"))?
}

struct PreauthorizedGate {
    permissions: Vec<ScopedPermission>,
}

#[async_trait]
impl PermissionGate for PreauthorizedGate {
    async fn authorize(
        &self,
        name: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        if cancel.is_cancelled() {
            return Err(CapabilityError::stopped(
                cayw_codemode::CapabilityStop::Cancelled,
                "artifact tool invoke cancelled",
            ));
        }
        let scope = canonical_scope(arguments);
        if self
            .permissions
            .iter()
            .any(|permission| permission.capability == name && permission.scope == scope)
        {
            Ok(())
        } else {
            Err(CapabilityError::new(format!(
                "artifact invoke denied for `{name}` scope `{scope}`"
            )))
        }
    }
}

pub(crate) struct ArtifactAudit {
    inner: Weak<Inner>,
    session_id: String,
    action_id: String,
    artifact_id: String,
}

#[async_trait]
impl AuditSink for ArtifactAudit {
    async fn started(&self, name: &str, arguments: &Value) -> Result<String, CapabilityError> {
        let call_id = uuid::Uuid::new_v4().to_string();
        if let Some(inner) = self.inner.upgrade() {
            inner
                .journal
                .record(
                    &self.session_id,
                    None,
                    "artifact.tool.started",
                    json!({
                        "action_id": self.action_id,
                        "artifact_id": self.artifact_id,
                        "call_id": call_id,
                        "tool": name,
                        "arguments": arguments,
                    }),
                )
                .map_err(|e| {
                    CapabilityError::unknown(format!("failed to record tool start: {e}"))
                })?;
        } else {
            return Err(CapabilityError::unknown("runtime closed during tool start"));
        }
        Ok(call_id)
    }

    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        if let Some(inner) = self.inner.upgrade() {
            let (is_ok, outcome_unknown, err_msg) = match result {
                Ok(_) => (true, false, None),
                Err(e) => (false, e.outcome_unknown, Some(e.to_string())),
            };
            inner
                .journal
                .record(
                    &self.session_id,
                    None,
                    "artifact.tool.settled",
                    json!({
                        "action_id": self.action_id,
                        "artifact_id": self.artifact_id,
                        "call_id": id,
                        "success": is_ok,
                        "outcome_unknown": outcome_unknown,
                        "error": err_msg,
                    }),
                )
                .map_err(|e| {
                    CapabilityError::unknown(format!("failed to record tool settlement: {e}"))
                })?;
        } else {
            return Err(CapabilityError::unknown(
                "runtime closed during tool settlement",
            ));
        }
        Ok(())
    }

    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .journal
                .record(
                    &self.session_id,
                    None,
                    "artifact.tool.dropped",
                    json!({
                        "action_id": self.action_id,
                        "artifact_id": self.artifact_id,
                        "call_id": id,
                        "outcome_unknown": true,
                    }),
                )
                .map_err(|e| {
                    CapabilityError::unknown(format!("failed to record tool dropped: {e}"))
                })?;
        } else {
            return Err(CapabilityError::unknown(
                "runtime closed during tool dropped",
            ));
        }
        Ok(())
    }
}

pub(crate) fn new_state(inner: Weak<Inner>) -> ArtifactState {
    let dispatch: SharedHostDispatch = Arc::new(RuntimeHostDispatch::new(inner));
    ArtifactState {
        workspace: ArtifactWorkspace::with_dispatch(dispatch),
    }
}

fn map_artifact(error: ArtifactError) -> RuntimeError {
    match error {
        ArtifactError::NeedsReconciliation(_) => RuntimeError::NeedsReconciliation,
        other => RuntimeError::InvalidInput(other.to_string()),
    }
}

fn record_artifact_audit(
    runtime: &Runtime,
    session_id: &str,
    record: &ActionRecord,
) -> Result<(), RuntimeError> {
    let kind = match record.status {
        ActionStatus::Accepted => "artifact.action.accepted",
        ActionStatus::Applied => "artifact.action.applied",
        ActionStatus::Rejected => "artifact.action.rejected",
        ActionStatus::Cancelled => "artifact.action.cancelled",
        ActionStatus::OutcomeUnknown => "artifact.action.outcome_unknown",
    };
    let task_id = record
        .outcome_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| {
            value
                .get("task_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        });
    runtime.inner.journal.record(
        session_id,
        task_id.as_deref(),
        kind,
        json!({
            "action_id": record.action_id,
            "artifact_id": record.artifact_id,
            "status": record.status,
            "kind": record.kind,
            "intent": record.intent,
            "rejection_reason": record.rejection_reason,
            "outcome_json": record.outcome_json,
            "requested_at_ms": record.requested_at_ms,
            "recorded_at_ms": record.recorded_at_ms,
        }),
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactFact {
    action_id: String,
    call_id: Option<String>,
    started_cursor: Option<u64>,
    cursor: u64,
}

struct ReconciledMarker {
    action_id: String,
    artifact_id: String,
    call_id: Option<String>,
    facts: Vec<ArtifactFact>,
    outcome: ArtifactReconciledOutcome,
    cursor: u64,
}

#[derive(Default)]
struct ArtifactActionAudit {
    artifact_id: Option<String>,
    identity_conflict: bool,
    kind: Option<ActionKind>,
    intent: Option<ActionIntent>,
    requested_at_ms: u64,
    call_starts: BTreeMap<String, u64>,
    tool_calls: BTreeMap<String, (String, Value)>,
    unknown_records: Vec<(u64, ActionRecord)>,
}

#[derive(Default)]
struct ArtifactAuditLog {
    actions: BTreeMap<String, ArtifactActionAudit>,
    facts: BTreeMap<u64, ArtifactFact>,
    reconciliations: Vec<ReconciledMarker>,
    malformed: bool,
}

impl ArtifactAuditLog {
    fn observe(&mut self, event: &crate::AuditRecord) {
        if !event.kind.starts_with("artifact.") {
            return;
        }
        let Some(action_id) = event.data.get("action_id").and_then(Value::as_str) else {
            self.malformed = true;
            return;
        };
        let call_id = event
            .data
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if event.kind == "artifact.action.reconciled" {
            if let (Some(artifact_id), Some(facts), Some(outcome)) = (
                event.data.get("artifact_id").and_then(Value::as_str),
                event
                    .data
                    .get("facts")
                    .and_then(|value| serde_json::from_value(value.clone()).ok()),
                event
                    .data
                    .get("outcome")
                    .and_then(|value| serde_json::from_value(value.clone()).ok()),
            ) {
                self.reconciliations.push(ReconciledMarker {
                    action_id: action_id.to_owned(),
                    artifact_id: artifact_id.to_owned(),
                    call_id,
                    facts,
                    outcome,
                    cursor: event.cursor,
                });
            }
            return;
        }
        let action = self.actions.entry(action_id.to_owned()).or_default();
        if let Some(artifact_id) = event.data.get("artifact_id").and_then(Value::as_str) {
            if action
                .artifact_id
                .as_deref()
                .is_some_and(|previous| previous != artifact_id)
            {
                action.identity_conflict = true;
            } else {
                action.artifact_id = Some(artifact_id.to_owned());
            }
        }
        if let Some(kind) = event
            .data
            .get("kind")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
        {
            action.kind = Some(kind);
        }
        if let Some(intent) = event
            .data
            .get("intent")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
        {
            action.intent = Some(intent);
        }
        if let Some(at_ms) = event.data.get("requested_at_ms").and_then(Value::as_u64) {
            action.requested_at_ms = at_ms;
        }
        match event.kind.as_str() {
            "artifact.tool.started" => {
                let Some(call_id) = call_id else {
                    self.malformed = true;
                    return;
                };
                action.call_starts.insert(call_id.clone(), event.cursor);
                if let (Some(tool), Some(arguments)) = (
                    event.data.get("tool").and_then(Value::as_str),
                    event.data.get("arguments"),
                ) {
                    action.intent = Some(ActionIntent::InvokeTool {
                        tool: tool.to_owned(),
                        args_json: arguments.to_string(),
                    });
                    action
                        .tool_calls
                        .insert(call_id.clone(), (tool.to_owned(), arguments.clone()));
                }
                self.facts.insert(
                    event.cursor,
                    ArtifactFact {
                        action_id: action_id.to_owned(),
                        call_id: Some(call_id),
                        started_cursor: Some(event.cursor),
                        cursor: event.cursor,
                    },
                );
            }
            "artifact.tool.settled" | "artifact.tool.dropped" => {
                let Some(call_id) = call_id else {
                    self.malformed = true;
                    return;
                };
                let started_cursor = action.call_starts.get(&call_id).copied();
                if let Some(cursor) = started_cursor {
                    self.facts.remove(&cursor);
                }
                if event.kind == "artifact.tool.dropped"
                    || event.data.get("outcome_unknown").and_then(Value::as_bool) == Some(true)
                {
                    self.facts.insert(
                        event.cursor,
                        ArtifactFact {
                            action_id: action_id.to_owned(),
                            call_id: Some(call_id),
                            started_cursor,
                            cursor: event.cursor,
                        },
                    );
                }
            }
            "artifact.action.outcome_unknown" => {
                let mut record = event.data.clone();
                if let Some(fields) = record.as_object_mut() {
                    fields.insert("session_id".into(), Value::String(event.session_id.clone()));
                }
                if let Ok(record) = serde_json::from_value::<ActionRecord>(record)
                    && record.status == ActionStatus::OutcomeUnknown
                {
                    action.unknown_records.push((event.cursor, record));
                }
                self.facts.insert(
                    event.cursor,
                    ArtifactFact {
                        action_id: action_id.to_owned(),
                        call_id: None,
                        started_cursor: None,
                        cursor: event.cursor,
                    },
                );
            }
            _ => {}
        }
    }

    fn pending(&self) -> Vec<ArtifactFact> {
        self.facts
            .values()
            .filter(|fact| {
                !self.reconciliations.iter().any(|marker| {
                    marker.action_id == fact.action_id
                        && marker.cursor > fact.cursor
                        && self.actions.get(&fact.action_id).is_some_and(|action| {
                            !action.identity_conflict
                                && action.artifact_id.as_deref()
                                    == Some(marker.artifact_id.as_str())
                        })
                        && marker.facts.contains(fact)
                })
            })
            .cloned()
            .collect()
    }

    fn reconciled_record(
        &self,
        expected: &ActionRecord,
        pending: &[ArtifactFact],
    ) -> Option<ActionRecord> {
        let action = self.actions.get(&expected.action_id)?;
        if action.identity_conflict
            || action.artifact_id.as_deref() != Some(expected.artifact_id.as_str())
            || pending
                .iter()
                .any(|fact| fact.action_id == expected.action_id)
        {
            return None;
        }
        let (cursor, original) = action.unknown_records.last()?;
        if original != expected {
            return None;
        }
        let fact = self.facts.get(cursor)?;
        let marker = self.reconciliations.iter().rev().find(|marker| {
            marker.action_id == expected.action_id
                && marker.artifact_id == expected.artifact_id
                && marker.cursor > *cursor
                && marker.facts.contains(fact)
        })?;
        let mut record = expected.clone();
        match &marker.outcome {
            ArtifactReconciledOutcome::Applied { result_json } => {
                record.status = ActionStatus::Applied;
                record.outcome_json = result_json.clone();
                record.rejection_reason = None;
            }
            ArtifactReconciledOutcome::NotExecuted { reason } => {
                record.status = ActionStatus::Cancelled;
                record.outcome_json = None;
                record.rejection_reason = Some(format!("reconciled_not_executed: {reason}"));
            }
            ArtifactReconciledOutcome::Failed { error } => {
                record.status = ActionStatus::Rejected;
                record.outcome_json = None;
                record.rejection_reason = Some(format!("reconciled_failed: {error}"));
            }
        }
        Some(record)
    }
}

fn task_is_active(state: crate::TaskState) -> bool {
    matches!(
        state,
        crate::TaskState::Queued
            | crate::TaskState::Running
            | crate::TaskState::WaitingApproval
            | crate::TaskState::Draining
    )
}

impl Runtime {
    /// Session-owned HTML artifact store (F09). Product path uses
    /// [`RuntimeHostDispatch`], not Recording.
    pub fn artifact_store(&self) -> Arc<ArtifactStore> {
        Arc::clone(&lock(&self.inner.artifacts).workspace.store)
    }

    /// Host action protocol (F19) backed by Runtime/Host dispatch.
    pub fn artifact_actions(&self) -> Arc<ActionProtocol> {
        Arc::clone(&lock(&self.inner.artifacts).workspace.actions)
    }

    /// Shared product dispatch bridge (Runtime enqueue + authorized Host::call).
    pub fn artifact_host_dispatch(&self) -> SharedHostDispatch {
        Arc::clone(lock(&self.inner.artifacts).workspace.host_dispatch())
    }

    /// Checks if a tool flight spawned by an artifact action is actively running.
    pub fn is_tool_flight_active(&self, action_id: &str) -> bool {
        let dispatch = lock(&self.inner.artifacts)
            .workspace
            .host_dispatch()
            .clone();
        if let Some(runtime_dispatch) = dispatch.as_any().downcast_ref::<RuntimeHostDispatch>() {
            runtime_dispatch.is_flight_active(action_id)
        } else {
            false
        }
    }

    fn artifact_executor(
        &self,
        session_id: &str,
    ) -> Result<crate::coordination::BackgroundLock, RuntimeError> {
        validate_id("session_id", session_id)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        self.inner
            .coordination
            .background(&format!("artifact-session-{session_id}"))
    }

    pub(crate) fn ensure_artifact_executor_idle(
        &self,
        session_id: &str,
    ) -> Result<(), RuntimeError> {
        let _executor = self.artifact_executor(session_id)?;
        Ok(())
    }

    fn artifact_audit(&self, session_id: &str) -> Result<ArtifactAuditLog, RuntimeError> {
        let mut log = ArtifactAuditLog::default();
        let mut after = 0;
        loop {
            let page = self.inner.journal.events(session_id, after, 1000)?;
            for event in &page {
                log.observe(event);
            }
            let Some(last) = page.last() else { break };
            after = last.cursor;
            if page.len() < 1000 {
                break;
            }
        }
        Ok(log)
    }

    fn recover_artifact_host_fence(
        &self,
        session_id: &str,
        log: &ArtifactAuditLog,
    ) -> Result<(), RuntimeError> {
        use futures::FutureExt;

        let host = self.inner.host_root.clone();
        let Some(pending) = block_on_host(async move {
            host.pending_mutation(CancellationToken::new())
                .now_or_never()
                .transpose()
        })
        .map_err(|error| RuntimeError::Workspace(error.to_string()))?
        .ok_or(RuntimeError::Busy)?
        else {
            return Ok(());
        };
        let mut originals = log
            .actions
            .iter()
            .filter(|(_, action)| action.call_starts.contains_key(&pending.audit_id));
        let Some((action_id, action)) = originals.next() else {
            // This Session cannot settle another executor's workspace mutation.
            return Ok(());
        };
        if originals.next().is_some() || action.identity_conflict {
            return Err(RuntimeError::NeedsReconciliation);
        }
        let Some((tool, arguments)) = action.tool_calls.get(&pending.audit_id) else {
            return Err(RuntimeError::NeedsReconciliation);
        };
        if tool != &pending.capability
            || cayw_cells::HostResult::value_hash(arguments) != pending.arguments_sha256
        {
            return Err(RuntimeError::NeedsReconciliation);
        }
        let marker = log.reconciliations.iter().rev().find(|marker| {
            &marker.action_id == action_id
                && action.artifact_id.as_ref() == Some(&marker.artifact_id)
                && marker.call_id.as_ref() == Some(&pending.audit_id)
                && marker.facts.iter().any(|fact| {
                    fact.action_id == *action_id
                        && fact.call_id.as_ref() == Some(&pending.audit_id)
                        && fact.started_cursor == action.call_starts.get(&pending.audit_id).copied()
                        && fact.cursor < marker.cursor
                })
        });
        let Some(marker) = marker else {
            return Err(RuntimeError::NeedsReconciliation);
        };
        if log.pending().iter().any(|fact| {
            fact.action_id == *action_id && fact.call_id.as_ref() == Some(&pending.audit_id)
        }) {
            return Err(RuntimeError::NeedsReconciliation);
        }
        let call_id = pending.audit_id.clone();
        let evidence = cayw_host::FenceRecovery {
            pending,
            stopped_owner: format!(
                "artifact Session {session_id} executor is locked and original call {call_id} stopped"
            ),
            cleanup_evidence: format!(
                "trusted artifact reconciliation cursor {} confirms original call {call_id} cleanup",
                marker.cursor
            ),
            terminal_evidence: format!(
                "durable artifact action {action_id} reconciliation cursor {} covers original call {call_id}",
                marker.cursor
            ),
        };
        let host = self.inner.host_root.clone();
        block_on_host(async move {
            host.recover_mutation(evidence, CancellationToken::new())
                .now_or_never()
                .transpose()
        })
        .map_err(|error| RuntimeError::Workspace(error.to_string()))?
        .ok_or(RuntimeError::Busy)
    }

    fn artifact_unknown(
        &self,
        session_id: &str,
        executor_idle: bool,
    ) -> Result<bool, RuntimeError> {
        if self
            .inner
            .cells
            .is_frozen(session_id)
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?
        {
            return Ok(true);
        }
        let log = self.artifact_audit(session_id)?;
        let pending = log.pending();
        if log.malformed
            || pending
                .iter()
                .any(|fact| executor_idle || fact.started_cursor != Some(fact.cursor))
        {
            return Ok(true);
        }
        if executor_idle && !log.reconciliations.is_empty() {
            self.recover_artifact_host_fence(session_id, &log)?;
        }
        let actions = self.artifact_actions();
        for record in actions
            .list_actions_for_session(session_id)
            .map_err(map_artifact)?
        {
            if record.status != ActionStatus::OutcomeUnknown {
                continue;
            }
            if !executor_idle {
                return Ok(true);
            }
            let Some(reconciled) = log.reconciled_record(&record, &pending) else {
                return Ok(true);
            };
            if !actions.refresh_reconciled(&record, reconciled) {
                return Ok(true);
            }
        }
        Ok(actions.has_outcome_unknown(session_id))
    }

    /// Unpaired starts remain live while their original executor owns the lock.
    /// Unknown terminal facts always block new effects.
    pub fn has_artifact_unknown(&self, session_id: &str) -> Result<bool, RuntimeError> {
        match self.artifact_executor(session_id) {
            Ok(_executor) => self.artifact_unknown(session_id, true),
            Err(RuntimeError::Busy) => self.artifact_unknown(session_id, false),
            Err(error) => Err(error),
        }
    }

    /// Reconcile an artifact action or hanging tool call with trusted host evidence.
    ///
    /// Validates non-empty actor and evidence, matches original action identity,
    /// verifies that the action is not currently active, updates in-memory status,
    /// and records a durable `artifact.action.reconciled` audit entry.
    pub fn reconcile_artifact_action(
        &self,
        input: ArtifactReconciliation,
    ) -> Result<ActionRecord, RuntimeError> {
        validate_id("session_id", &input.session_id)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        validate_id("action_id", &input.action_id)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        validate_id("artifact_id", &input.expected_artifact_id)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        if let Some(ref cid) = input.call_id {
            validate_id("call_id", cid).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        }

        if input.actor.trim().is_empty() || input.actor.len() > 256 {
            return Err(RuntimeError::InvalidInput(
                "actor is required and must be 1..256 bytes".into(),
            ));
        }
        let actor_lower = input.actor.to_lowercase();
        if actor_lower == "model"
            || actor_lower == "assistant"
            || actor_lower.starts_with("model:")
            || actor_lower.starts_with("agent:")
        {
            return Err(RuntimeError::InvalidInput(
                "model and agent self-reports cannot reconcile artifact unknown outcomes".into(),
            ));
        }
        if input.evidence.trim().is_empty() || input.evidence.len() > 65536 {
            return Err(RuntimeError::InvalidInput(
                "reconciliation evidence is required and must be 1..65536 bytes".into(),
            ));
        }

        let verifier = self
            .inner
            .config
            .artifact_reconciliation_verifier
            .as_ref()
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "artifact reconciliation requires artifact_reconciliation_verifier to be configured".into(),
                )
            })?;
        verifier.verify(&input).map_err(|err| {
            RuntimeError::InvalidInput(format!(
                "artifact reconciliation verification failed: {err}"
            ))
        })?;

        let _coordinate = self.inner.coordination.enter()?;
        let _executor = self.artifact_executor(&input.session_id)?;
        let snapshot = self.get(&input.session_id)?;
        if snapshot.attempt.is_some() {
            return Err(RuntimeError::Busy);
        }
        let mut task_locks = Vec::new();
        for task in self.inner.queue.list(&input.session_id)? {
            if task_is_active(task.state) {
                return Err(RuntimeError::Busy);
            }
            task_locks.push(self.inner.coordination.task(&task.id)?);
        }
        if self.is_tool_flight_active(&input.action_id) {
            return Err(RuntimeError::Busy);
        }

        let log = self.artifact_audit(&input.session_id)?;
        let action = log.actions.get(&input.action_id).ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "action {} not found in audit journal",
                input.action_id
            ))
        })?;
        if action.identity_conflict
            || action.artifact_id.as_deref() != Some(&input.expected_artifact_id)
        {
            return Err(RuntimeError::InvalidInput(
                "artifact identity does not match original audit facts".into(),
            ));
        }
        if let Some(call_id) = &input.call_id
            && !action.call_starts.contains_key(call_id)
        {
            return Err(RuntimeError::InvalidInput(format!(
                "call_id {call_id} not found in original tool starts"
            )));
        }
        let pending: Vec<_> = log
            .pending()
            .into_iter()
            .filter(|fact| fact.action_id == input.action_id)
            .collect();
        let other_calls = pending
            .iter()
            .any(|fact| fact.call_id.is_some() && fact.call_id != input.call_id);
        if input.call_id.is_none() && other_calls {
            return Err(RuntimeError::InvalidInput(
                "call_id is required for an unknown tool call".into(),
            ));
        }
        let facts: Vec<_> = pending
            .iter()
            .filter(|fact| {
                fact.call_id == input.call_id || (fact.call_id.is_none() && !other_calls)
            })
            .cloned()
            .collect();
        if facts.is_empty() {
            let previous = log.reconciliations.iter().rev().find(|marker| {
                marker.action_id == input.action_id
                    && marker.artifact_id == input.expected_artifact_id
                    && marker.call_id == input.call_id
            }).ok_or_else(|| RuntimeError::InvalidInput(format!(
                "action {} has no recorded unknown effect or hanging call requiring reconciliation", input.action_id
            )))?;
            if previous.outcome != input.outcome {
                return Err(RuntimeError::InvalidInput(format!(
                    "conflicting reconciliation decision already recorded for action {}",
                    input.action_id
                )));
            }
        }
        let (mut status, mut rejection_reason, outcome_json) = match &input.outcome {
            ArtifactReconciledOutcome::Applied { result_json } => {
                (ActionStatus::Applied, None, result_json.clone())
            }
            ArtifactReconciledOutcome::NotExecuted { reason } => (
                ActionStatus::Cancelled,
                Some(format!("reconciled_not_executed: {reason}")),
                None,
            ),
            ArtifactReconciledOutcome::Failed { error } => (
                ActionStatus::Rejected,
                Some(format!("reconciled_failed: {error}")),
                None,
            ),
        };
        if pending.iter().any(|fact| !facts.contains(fact)) {
            status = ActionStatus::OutcomeUnknown;
            rejection_reason =
                Some("other artifact tool calls still require reconciliation".into());
        }
        let actions = self.artifact_actions();
        let existing = match actions.get_action(&input.action_id) {
            Ok(record) => {
                if record.session_id != input.session_id
                    || record.artifact_id != input.expected_artifact_id
                {
                    return Err(RuntimeError::InvalidInput(
                        "in-memory artifact identity conflicts with audit".into(),
                    ));
                }
                Some(record)
            }
            Err(ArtifactError::NotFound(_)) => None,
            Err(error) => return Err(map_artifact(error)),
        };
        let record = ActionRecord {
            action_id: input.action_id.clone(),
            artifact_id: input.expected_artifact_id.clone(),
            session_id: input.session_id.clone(),
            kind: existing
                .as_ref()
                .map(|record| record.kind)
                .or(action.kind)
                .unwrap_or(ActionKind::Custom),
            intent: existing
                .map(|record| record.intent)
                .or_else(|| action.intent.clone())
                .unwrap_or_else(|| ActionIntent::ReadOnly {
                    note: "recovered_action".into(),
                }),
            status,
            requested_at_ms: action.requested_at_ms,
            recorded_at_ms: cayw_ai::timestamp_ms(),
            rejection_reason,
            outcome_json,
        };
        if !facts.is_empty() {
            self.inner.journal.record(
                &input.session_id,
                None,
                "artifact.action.reconciled",
                json!({
                    "action_id": input.action_id,
                    "artifact_id": input.expected_artifact_id,
                    "call_id": input.call_id,
                    "facts": facts,
                    "actor": input.actor,
                    "evidence": input.evidence,
                    "outcome": input.outcome,
                }),
            )?;
        }
        self.recover_artifact_host_fence(
            &input.session_id,
            &self.artifact_audit(&input.session_id)?,
        )?;
        actions.upsert_reconciled(record.clone());
        self.inner.changed.notify_waiters();
        Ok(record)
    }

    pub fn put_artifact(
        &self,
        request: PutArtifactRequest,
    ) -> Result<ArtifactRecord, RuntimeError> {
        self.get(&request.session_id)?;
        self.artifact_store().put(request).map_err(map_artifact)
    }

    pub fn update_artifact(
        &self,
        request: UpdateArtifactRequest,
    ) -> Result<ArtifactRecord, RuntimeError> {
        self.get(&request.session_id)?;
        self.artifact_store().update(request).map_err(map_artifact)
    }

    pub fn preview_artifact(&self, artifact_id: &str) -> Result<ArtifactPreview, RuntimeError> {
        self.artifact_store()
            .preview(artifact_id)
            .map_err(map_artifact)
    }

    pub fn list_artifacts(&self, session_id: &str) -> Result<Vec<ArtifactRecord>, RuntimeError> {
        self.get(session_id)?;
        self.artifact_store()
            .list_by_session(session_id)
            .map_err(map_artifact)
    }

    /// Dispatch a card/form/chart action through the real Runtime/Host consumer.
    ///
    /// Also appends a unified `artifact.action.*` row to the Runtime audit journal
    /// so WebUI/SDK consumers can page the same trail as other Runtime events.
    pub fn dispatch_artifact_action(
        &self,
        request: ActionRequest,
    ) -> Result<ActionOutcome, RuntimeError> {
        {
            let _coordinate = self.inner.coordination.enter()?;
            self.get(&request.session_id)?;
            self.ensure_artifact_executor_idle(&request.session_id)?;
            if self.has_artifact_unknown(&request.session_id)? {
                return Err(RuntimeError::NeedsReconciliation);
            }
        }
        let session_id = request.session_id.clone();
        let actions = self.artifact_actions();
        let outcome = actions.dispatch(request).map_err(map_artifact)?;
        record_artifact_audit(self, &session_id, &outcome.record)?;
        Ok(outcome)
    }

    /// Audit trail of artifact actions for a Session (protocol records).
    pub fn list_artifact_actions(
        &self,
        session_id: &str,
    ) -> Result<Vec<ActionRecord>, RuntimeError> {
        self.get(session_id)?;
        self.artifact_actions()
            .list_actions_for_session(session_id)
            .map_err(map_artifact)
    }

    pub fn get_artifact_action(&self, action_id: &str) -> Result<ActionRecord, RuntimeError> {
        self.artifact_actions()
            .get_action(action_id)
            .map_err(map_artifact)
    }

    /// Cancel in-flight Host tool work or a follow-up Runtime task spawned from
    /// an artifact action. Records `artifact.action.cancelled` in the audit journal.
    pub fn cancel_artifact_action(
        &self,
        session_id: &str,
        action_id: &str,
    ) -> Result<ActionOutcome, RuntimeError> {
        self.get(session_id)?;
        let actions = self.artifact_actions();
        let outcome = actions
            .cancel_for_session(session_id, action_id)
            .map_err(map_artifact)?;
        record_artifact_audit(self, session_id, &outcome.record)?;
        Ok(outcome)
    }

    /// Persist artifacts + actions under `{workspace}/artifacts/recovery.json`.
    pub fn save_artifact_recovery(&self) -> Result<std::path::PathBuf, RuntimeError> {
        lock(&self.inner.artifacts)
            .workspace
            .save_to_disk(&self.inner.root)
            .map_err(map_artifact)
    }

    /// Replace the in-memory workspace from `{workspace}/artifacts/recovery.json`,
    /// keeping the live Runtime/Host dispatch bridge.
    pub fn load_artifact_recovery(&self) -> Result<(), RuntimeError> {
        let bundle = load_recovery(&self.inner.root).map_err(map_artifact)?;
        lock(&self.inner.artifacts)
            .workspace
            .restore_recovery(bundle)
            .map_err(map_artifact)
    }

    pub fn export_artifact_recovery(&self) -> RecoveryBundle {
        lock(&self.inner.artifacts).workspace.export_recovery()
    }

    pub fn restore_artifact_recovery(&self, bundle: RecoveryBundle) -> Result<(), RuntimeError> {
        lock(&self.inner.artifacts)
            .workspace
            .restore_recovery(bundle)
            .map_err(map_artifact)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeConfig;
    use cayw_ai::{
        AssistantEvent, AssistantMessage, AssistantStream, Model, ModelRequest, Provider,
    };
    use cayw_host::AuditSink;
    use tempfile::tempdir;

    #[derive(Default)]
    struct DummyProvider;

    impl Provider for DummyProvider {
        fn stream(&self, _request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
            Box::pin(async_stream::stream! {
                cancel.cancelled().await;
                yield AssistantEvent::Done {
                    message: AssistantMessage::new(&Model::new("fixture", "artifacts")),
                };
            })
        }
    }

    #[tokio::test]
    async fn artifact_audit_dropped_propagates_sqlite_write_failures_and_closed_runtime() {
        let temp = tempdir().unwrap();
        let ws = temp.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        let db_path = temp.path().join("sessions.db");

        let runtime = Runtime::open(
            &db_path,
            &ws,
            Arc::new(DummyProvider),
            Model::new("fixture", "artifacts"),
            RuntimeConfig::default(),
        )
        .unwrap();

        let session = runtime.create(Some("AuditInternalSess".into())).unwrap();
        let audit = ArtifactAudit {
            inner: Arc::downgrade(&runtime.inner),
            session_id: session.id.clone(),
            action_id: "act-internal-1".into(),
            artifact_id: "card-internal-1".into(),
        };

        // 1. Normal dropped succeeds
        audit.dropped("call-1").unwrap();
        let events = runtime.audit(&session.id, 0, 100).unwrap();
        assert!(events.iter().any(|e| {
            e.kind == "artifact.tool.dropped"
                && e.data.get("call_id").and_then(Value::as_str) == Some("call-1")
        }));

        // 2. Real SQLite write failure: dropping the runtime_audit table from a separate connection
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        conn.execute_batch("DROP TABLE runtime_audit;").unwrap();

        let db_err = audit.dropped("call-db-err").unwrap_err();
        assert!(
            db_err.outcome_unknown,
            "persistence error must result in outcome_unknown: true"
        );
        assert!(
            db_err.message.contains("failed to record tool dropped"),
            "error message must describe dropped failure: {}",
            db_err.message
        );

        // 3. Closed runtime (inner dropped)
        drop(runtime);
        let closed_err = audit.dropped("call-closed").unwrap_err();
        assert!(
            closed_err.outcome_unknown,
            "closed runtime must result in outcome_unknown: true"
        );
        assert!(closed_err.message.contains("runtime closed"));
    }
}
