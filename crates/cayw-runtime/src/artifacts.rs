//! Wire cayw-artifacts into Runtime/Host (F09/F19 product path).
//!
//! `RecordingHostDispatch` remains the offline/test double inside
//! `cayw-artifacts`. The default product consumer is [`RuntimeHostDispatch`]:
//! - `FollowUpTask` → [`Runtime::enqueue`] (button → queued task)
//! - `InvokeTool` → authorized [`cayw_host::Host::call`] against
//!   [`RuntimeConfig::permissions`] (exact scope; never a new capability grant)

use crate::{
    CancelTarget, Inner, Runtime, RuntimeError, ScopedPermission, lock,
    permissions::canonical_scope,
};
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_artifacts::{
    ActionOutcome, ActionProtocol, ActionRecord, ActionRequest, ActionStatus, ArtifactError,
    ArtifactPreview, ArtifactRecord, ArtifactStore, ArtifactWorkspace, FollowUpDispatch,
    HostActionDispatch, HostDispatchResult, PutArtifactRequest, RecoveryBundle, SharedHostDispatch,
    ToolDispatch, UpdateArtifactRequest, load_recovery,
};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, Host, PermissionGate};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};

/// Process-local artifact workspace owned by one Runtime.
#[derive(Debug)]
pub(crate) struct ArtifactState {
    pub workspace: ArtifactWorkspace,
}

/// In-flight Host/Runtime work spawned from an artifact action.
enum ArtifactFlight {
    Tool { cancel: CancellationToken },
    FollowUp { target: CancelTarget },
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
}

impl HostActionDispatch for RuntimeHostDispatch {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn follow_up_task(&self, req: FollowUpDispatch<'_>) -> Result<HostDispatchResult, String> {
        let runtime = self.runtime()?;
        // Session must exist and be ready; same admission path as trusted host prompts.
        let task = runtime
            .enqueue(req.session_id, req.prompt)
            .map_err(|error| error.to_string())?;
        let target = task.cancel_target();
        self.register_flight(
            req.action_id,
            ArtifactFlight::FollowUp {
                target: target.clone(),
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

    fn invoke_tool(&self, req: ToolDispatch<'_>) -> Result<HostDispatchResult, String> {
        let runtime = self.runtime()?;
        // Ensure the owning session still exists before spending Host budget.
        runtime
            .get(req.session_id)
            .map_err(|error| error.to_string())?;
        let args: Value = serde_json::from_str(req.args_json)
            .map_err(|error| format!("invalid invoke_tool args_json: {error}"))?;
        let permissions = runtime.inner.config.permissions.clone();
        let scope = canonical_scope(&args);
        if !permissions
            .iter()
            .any(|p| p.capability == req.tool && p.scope == scope)
        {
            return Err(format!(
                "tool `{}` with scope `{scope}` is not preauthorized in RuntimeConfig.permissions",
                req.tool
            ));
        }
        let host = artifact_tool_host(&runtime.inner, permissions)?;
        let tool = req.tool.to_owned();
        let cancel = CancellationToken::new();
        self.register_flight(
            req.action_id,
            ArtifactFlight::Tool {
                cancel: cancel.clone(),
            },
        );
        let result = block_on_host(async move { host.call(&tool, args, cancel).await });
        self.take_flight(req.action_id);
        let result = result?;
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

    fn cancel_action(&self, action_id: &str) -> Result<bool, String> {
        let Some(flight) = self.take_flight(action_id) else {
            return Ok(false);
        };
        match flight {
            ArtifactFlight::Tool { cancel } => {
                cancel.cancel();
                Ok(true)
            }
            ArtifactFlight::FollowUp { target } => {
                let runtime = self.runtime()?;
                runtime.cancel(&target).map_err(|error| error.to_string())
            }
        }
    }
}

fn artifact_tool_host(inner: &Inner, permissions: Vec<ScopedPermission>) -> Result<Host, String> {
    let gate = Arc::new(PreauthorizedGate { permissions });
    let audit = Arc::new(ArtifactAudit::default());
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
fn block_on_host<F>(fut: F) -> Result<Value, String>
where
    F: std::future::Future<Output = Result<Value, CapabilityError>> + Send + 'static,
{
    std::thread::Builder::new()
        .name("cayw-artifact-host".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?
                .block_on(fut)
                .map_err(|error| error.to_string())
        })
        .map_err(|error| error.to_string())?
        .join()
        .map_err(|_| "artifact host worker panicked".to_string())?
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

#[derive(Default)]
struct ArtifactAudit {
    next: AtomicU64,
}

#[async_trait]
impl AuditSink for ArtifactAudit {
    async fn started(&self, _name: &str, _arguments: &Value) -> Result<String, CapabilityError> {
        Ok(self.next.fetch_add(1, Ordering::Relaxed).to_string())
    }

    async fn settled(
        &self,
        _id: &str,
        _result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }

    fn dropped(&self, _id: &str) -> Result<(), CapabilityError> {
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
    RuntimeError::InvalidInput(error.to_string())
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
        self.get(&request.session_id)?;
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
