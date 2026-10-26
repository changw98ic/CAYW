//! Host action protocol for artifact cards / forms / charts (F19).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::dispatch::{FollowUpDispatch, HostActionDispatch, SharedHostDispatch, ToolDispatch};
use crate::identity::validate_id;
use crate::store::ArtifactStore;
use crate::types::{
    ActionIntent, ActionOutcome, ActionRecord, ActionRequest, ActionStatus, ArtifactError,
};

/// Dispatches and persists artifact actions with Session ownership checks.
///
/// `FollowUpTask` / `InvokeTool` call the injected [`HostActionDispatch`] bridge
/// so they actually hand off to Runtime/Host (or a recording stand-in). They no
/// longer stop at pending JSON.
pub struct ActionProtocol {
    artifacts: Arc<ArtifactStore>,
    dispatch: SharedHostDispatch,
    inner: Mutex<ActionInner>,
}

impl std::fmt::Debug for ActionProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionProtocol")
            .field("artifacts", &self.artifacts)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionInner {
    actions: BTreeMap<String, ActionRecord>,
    /// Per-artifact UI state map written by `UpdateState` intents.
    state: BTreeMap<String, BTreeMap<String, String>>,
}

/// Portable action + UI-state snapshot for recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSnapshot {
    pub actions: Vec<ActionRecord>,
    pub state: BTreeMap<String, BTreeMap<String, String>>,
}

impl ActionProtocol {
    pub fn new(artifacts: Arc<ArtifactStore>, dispatch: SharedHostDispatch) -> Self {
        Self {
            artifacts,
            dispatch,
            inner: Mutex::new(ActionInner::default()),
        }
    }

    pub fn artifacts(&self) -> &Arc<ArtifactStore> {
        &self.artifacts
    }

    pub fn host_dispatch(&self) -> &SharedHostDispatch {
        &self.dispatch
    }

    /// Accept or reject an action. Never grants new Host capabilities.
    ///
    /// Records `Accepted` before calling the host bridge so cancel/audit can
    /// observe in-flight work. `FollowUpTask` / `InvokeTool` then settle to
    /// `Applied`, `Rejected`, or `Cancelled`.
    pub fn dispatch(&self, req: ActionRequest) -> Result<ActionOutcome, ArtifactError> {
        validate_id("action_id", &req.action_id).map_err(ArtifactError::Invalid)?;
        validate_id("artifact_id", &req.artifact_id).map_err(ArtifactError::Invalid)?;
        validate_id("session_id", &req.session_id).map_err(ArtifactError::Invalid)?;
        validate_intent(&req.intent)?;

        let artifact = self.artifacts.get(&req.artifact_id)?;
        let recorded_at_ms = req.requested_at_ms;

        let mut guard = self.inner.lock().expect("action lock");
        if guard.actions.contains_key(&req.action_id) {
            return Err(ArtifactError::DuplicateAction(req.action_id));
        }

        if artifact.session_id != req.session_id {
            let record = rejected_record(
                &req,
                recorded_at_ms,
                format!(
                    "cross_session: owner={}, claimed={}",
                    artifact.session_id, req.session_id
                ),
            );
            guard
                .actions
                .insert(record.action_id.clone(), record.clone());
            return Ok(ActionOutcome { record });
        }

        if let Some(expected) = req.expected_content_version
            && expected != artifact.content_version
        {
            let record = rejected_record(
                &req,
                recorded_at_ms,
                format!(
                    "version_mismatch: expected={expected}, actual={}",
                    artifact.content_version
                ),
            );
            guard
                .actions
                .insert(record.action_id.clone(), record.clone());
            return Ok(ActionOutcome { record });
        }

        // Mark Accepted before the bridge so cancel can target in-flight work.
        let accepted = ActionRecord {
            action_id: req.action_id.clone(),
            artifact_id: req.artifact_id.clone(),
            session_id: req.session_id.clone(),
            kind: req.kind,
            intent: req.intent.clone(),
            status: ActionStatus::Accepted,
            requested_at_ms: req.requested_at_ms,
            recorded_at_ms,
            rejection_reason: None,
            outcome_json: None,
        };
        guard.actions.insert(accepted.action_id.clone(), accepted);
        drop(guard);

        let applied = apply_intent(&req, self.dispatch.as_ref(), &self.inner)?;
        // If cancel won the race before/during the bridge, best-effort tear down
        // any flight the bridge registered after the cancel mark.
        {
            let cancelled_already = {
                let guard = self.inner.lock().expect("action lock");
                guard
                    .actions
                    .get(&req.action_id)
                    .is_some_and(|r| r.status == ActionStatus::Cancelled)
            };
            if cancelled_already {
                let _ = self.dispatch.cancel_action(&req.action_id);
                let guard = self.inner.lock().expect("action lock");
                if let Some(existing) = guard.actions.get(&req.action_id) {
                    return Ok(ActionOutcome {
                        record: existing.clone(),
                    });
                }
            }
        }
        let mut guard = self.inner.lock().expect("action lock");
        // A concurrent cancel may have already settled this action.
        if let Some(existing) = guard.actions.get(&req.action_id)
            && existing.status == ActionStatus::Cancelled
        {
            return Ok(ActionOutcome {
                record: existing.clone(),
            });
        }
        let record = match applied {
            IntentApply::Applied { outcome_json } => ActionRecord {
                action_id: req.action_id.clone(),
                artifact_id: req.artifact_id.clone(),
                session_id: req.session_id.clone(),
                kind: req.kind,
                intent: req.intent,
                status: ActionStatus::Applied,
                requested_at_ms: req.requested_at_ms,
                recorded_at_ms,
                rejection_reason: None,
                outcome_json: Some(outcome_json),
            },
            IntentApply::Rejected { reason } => rejected_record(&req, recorded_at_ms, reason),
            IntentApply::Cancelled { reason } => cancelled_record(&req, recorded_at_ms, reason),
        };
        guard
            .actions
            .insert(record.action_id.clone(), record.clone());
        Ok(ActionOutcome { record })
    }

    /// Cancel an in-flight (or follow-up-queued) artifact action.
    ///
    /// Signals the host bridge (`HostActionDispatch::cancel_action`) and marks
    /// the action `Cancelled` when a live flight was interrupted. Idempotent
    /// for already-cancelled records.
    pub fn cancel(&self, action_id: &str) -> Result<ActionOutcome, ArtifactError> {
        validate_id("action_id", action_id).map_err(ArtifactError::Invalid)?;
        let snapshot = {
            let guard = self.inner.lock().expect("action lock");
            guard
                .actions
                .get(action_id)
                .cloned()
                .ok_or_else(|| ArtifactError::NotFound(action_id.to_owned()))?
        };
        if snapshot.status == ActionStatus::Cancelled {
            return Ok(ActionOutcome { record: snapshot });
        }
        if matches!(snapshot.status, ActionStatus::Rejected) {
            return Err(ArtifactError::Invalid(format!(
                "action {action_id} is already rejected and cannot be cancelled"
            )));
        }
        // Applied follow-ups remain cancellable (queued/running Runtime task).
        // Applied UpdateState / ReadOnly / completed tools are not.
        if snapshot.status == ActionStatus::Applied
            && !matches!(snapshot.intent, ActionIntent::FollowUpTask { .. })
        {
            return Err(ArtifactError::Invalid(format!(
                "action {action_id} already settled and cannot be cancelled"
            )));
        }

        let signalled = self
            .dispatch
            .cancel_action(action_id)
            .map_err(|reason| ArtifactError::Invalid(format!("cancel_action: {reason}")))?;

        let mut guard = self.inner.lock().expect("action lock");
        let existing = guard
            .actions
            .get_mut(action_id)
            .ok_or_else(|| ArtifactError::NotFound(action_id.to_owned()))?;
        if existing.status == ActionStatus::Cancelled {
            return Ok(ActionOutcome {
                record: existing.clone(),
            });
        }
        if !signalled && existing.status == ActionStatus::Applied {
            // Follow-up may have already finished; treat as not cancelled.
            return Err(ArtifactError::Invalid(format!(
                "action {action_id} has no cancellable in-flight host/runtime work"
            )));
        }
        existing.status = ActionStatus::Cancelled;
        existing.rejection_reason = Some(if signalled {
            "cancelled".into()
        } else {
            "cancelled_before_bridge_settled".into()
        });
        Ok(ActionOutcome {
            record: existing.clone(),
        })
    }

    pub fn get_action(&self, action_id: &str) -> Result<ActionRecord, ArtifactError> {
        validate_id("action_id", action_id).map_err(ArtifactError::Invalid)?;
        let guard = self.inner.lock().expect("action lock");
        guard
            .actions
            .get(action_id)
            .cloned()
            .ok_or_else(|| ArtifactError::NotFound(action_id.to_owned()))
    }

    pub fn list_actions_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<ActionRecord>, ArtifactError> {
        validate_id("session_id", session_id).map_err(ArtifactError::Invalid)?;
        let guard = self.inner.lock().expect("action lock");
        let mut out: Vec<_> = guard
            .actions
            .values()
            .filter(|r| r.session_id == session_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.requested_at_ms
                .cmp(&b.requested_at_ms)
                .then_with(|| a.action_id.cmp(&b.action_id))
        });
        Ok(out)
    }

    pub fn artifact_state(
        &self,
        artifact_id: &str,
    ) -> Result<BTreeMap<String, String>, ArtifactError> {
        validate_id("artifact_id", artifact_id).map_err(ArtifactError::Invalid)?;
        let guard = self.inner.lock().expect("action lock");
        Ok(guard.state.get(artifact_id).cloned().unwrap_or_default())
    }

    pub fn export_snapshot(&self) -> ActionSnapshot {
        let guard = self.inner.lock().expect("action lock");
        ActionSnapshot {
            actions: guard.actions.values().cloned().collect(),
            state: guard.state.clone(),
        }
    }

    pub fn restore_snapshot(&self, snapshot: ActionSnapshot) -> Result<(), ArtifactError> {
        let mut actions = BTreeMap::new();
        for record in snapshot.actions {
            validate_id("action_id", &record.action_id).map_err(ArtifactError::Invalid)?;
            validate_id("artifact_id", &record.artifact_id).map_err(ArtifactError::Invalid)?;
            validate_id("session_id", &record.session_id).map_err(ArtifactError::Invalid)?;
            if actions.insert(record.action_id.clone(), record).is_some() {
                return Err(ArtifactError::Invalid(
                    "snapshot contains duplicate action_id".into(),
                ));
            }
        }
        for (artifact_id, map) in &snapshot.state {
            validate_id("artifact_id", artifact_id).map_err(ArtifactError::Invalid)?;
            for key in map.keys() {
                validate_id("state_key", key).map_err(ArtifactError::Invalid)?;
            }
        }
        let mut guard = self.inner.lock().expect("action lock");
        guard.actions = actions;
        guard.state = snapshot.state;
        Ok(())
    }
}

fn rejected_record(req: &ActionRequest, recorded_at_ms: u64, reason: String) -> ActionRecord {
    ActionRecord {
        action_id: req.action_id.clone(),
        artifact_id: req.artifact_id.clone(),
        session_id: req.session_id.clone(),
        kind: req.kind,
        intent: req.intent.clone(),
        status: ActionStatus::Rejected,
        requested_at_ms: req.requested_at_ms,
        recorded_at_ms,
        rejection_reason: Some(reason),
        outcome_json: None,
    }
}

fn cancelled_record(req: &ActionRequest, recorded_at_ms: u64, reason: String) -> ActionRecord {
    ActionRecord {
        action_id: req.action_id.clone(),
        artifact_id: req.artifact_id.clone(),
        session_id: req.session_id.clone(),
        kind: req.kind,
        intent: req.intent.clone(),
        status: ActionStatus::Cancelled,
        requested_at_ms: req.requested_at_ms,
        recorded_at_ms,
        rejection_reason: Some(reason),
        outcome_json: None,
    }
}

fn classify_bridge_err(prefix: &str, reason: String) -> IntentApply {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("cancel") {
        IntentApply::Cancelled {
            reason: format!("{prefix}: {reason}"),
        }
    } else {
        IntentApply::Rejected {
            reason: format!("{prefix}: {reason}"),
        }
    }
}

fn validate_intent(intent: &ActionIntent) -> Result<(), ArtifactError> {
    match intent {
        ActionIntent::FollowUpTask { prompt } => {
            if prompt.trim().is_empty() {
                return Err(ArtifactError::Invalid(
                    "follow_up_task prompt is empty".into(),
                ));
            }
            if prompt.len() > 16 * 1024 {
                return Err(ArtifactError::Invalid(
                    "follow_up_task prompt exceeds 16 KiB".into(),
                ));
            }
        }
        ActionIntent::InvokeTool { tool, args_json } => {
            validate_id("tool", tool).map_err(ArtifactError::Invalid)?;
            if args_json.len() > 64 * 1024 {
                return Err(ArtifactError::Invalid(
                    "invoke_tool args_json exceeds 64 KiB".into(),
                ));
            }
        }
        ActionIntent::UpdateState { key, value_json } => {
            validate_id("state_key", key).map_err(ArtifactError::Invalid)?;
            if value_json.len() > 64 * 1024 {
                return Err(ArtifactError::Invalid(
                    "update_state value_json exceeds 64 KiB".into(),
                ));
            }
        }
        ActionIntent::ReadOnly { note } => {
            if note.len() > 4 * 1024 {
                return Err(ArtifactError::Invalid(
                    "read_only note exceeds 4 KiB".into(),
                ));
            }
        }
    }
    Ok(())
}

enum IntentApply {
    Applied { outcome_json: String },
    Rejected { reason: String },
    Cancelled { reason: String },
}

fn apply_intent(
    req: &ActionRequest,
    dispatch: &dyn HostActionDispatch,
    inner: &Mutex<ActionInner>,
) -> Result<IntentApply, ArtifactError> {
    match &req.intent {
        ActionIntent::FollowUpTask { prompt } => {
            match dispatch.follow_up_task(FollowUpDispatch {
                action_id: &req.action_id,
                artifact_id: &req.artifact_id,
                session_id: &req.session_id,
                prompt,
            }) {
                Ok(result) => Ok(IntentApply::Applied {
                    outcome_json: result.outcome_json,
                }),
                Err(reason) => Ok(classify_bridge_err("host_dispatch_follow_up", reason)),
            }
        }
        ActionIntent::InvokeTool { tool, args_json } => {
            match dispatch.invoke_tool(ToolDispatch {
                action_id: &req.action_id,
                artifact_id: &req.artifact_id,
                session_id: &req.session_id,
                tool,
                args_json,
            }) {
                Ok(result) => Ok(IntentApply::Applied {
                    outcome_json: result.outcome_json,
                }),
                Err(reason) => Ok(classify_bridge_err("host_dispatch_invoke_tool", reason)),
            }
        }
        ActionIntent::UpdateState { key, value_json } => {
            let mut guard = inner.lock().expect("action lock");
            let entry = guard.state.entry(req.artifact_id.clone()).or_default();
            entry.insert(key.clone(), value_json.clone());
            let payload = json!({
                "kind": "state_updated",
                "artifact_id": req.artifact_id,
                "key": key,
            });
            Ok(IntentApply::Applied {
                outcome_json: payload.to_string(),
            })
        }
        ActionIntent::ReadOnly { note } => {
            let payload = json!({
                "kind": "read_only",
                "note": note,
            });
            Ok(IntentApply::Applied {
                outcome_json: payload.to_string(),
            })
        }
    }
}
