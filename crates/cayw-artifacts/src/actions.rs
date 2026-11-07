//! Host action protocol for artifact cards / forms / charts (F19).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::dispatch::{
    FollowUpDispatch, HostActionDispatch, HostDispatchError, SharedHostDispatch, ToolDispatch,
};
use crate::identity::validate_id;
use crate::store::ArtifactStore;
use crate::types::{
    ActionIntent, ActionOutcome, ActionRecord, ActionRequest, ActionStatus, ArtifactError,
    ArtifactReconciledOutcome, ArtifactReconciliation,
};

/// Host policy for validating artifact action/tool reconciliation receipts.
pub trait ArtifactReconciliationVerifier: Send + Sync {
    fn verify(&self, request: &ArtifactReconciliation) -> Result<(), String>;
}

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
    #[serde(skip)]
    in_flight: BTreeMap<String, usize>,
}

pub(crate) type PreparedActionSnapshot = (
    BTreeMap<String, ActionRecord>,
    BTreeMap<String, BTreeMap<String, String>>,
);

struct InFlightAction<'a> {
    inner: &'a Mutex<ActionInner>,
    action_id: String,
}

impl Drop for InFlightAction<'_> {
    fn drop(&mut self) {
        let mut guard = self.inner.lock().expect("action lock");
        if let Some(active) = guard.in_flight.get_mut(&self.action_id) {
            *active -= 1;
            if *active == 0 {
                guard.in_flight.remove(&self.action_id);
            }
        }
    }
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

        if guard
            .actions
            .values()
            .any(|r| r.session_id == req.session_id && r.status == ActionStatus::OutcomeUnknown)
        {
            return Err(ArtifactError::NeedsReconciliation(format!(
                "session {} has an artifact action with outcome_unknown and requires reconciliation",
                req.session_id
            )));
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
        *guard.in_flight.entry(req.action_id.clone()).or_default() += 1;
        drop(guard);
        let _in_flight = InFlightAction {
            inner: &self.inner,
            action_id: req.action_id.clone(),
        };

        let applied = apply_intent(&req, self.dispatch.as_ref(), &self.inner)?;
        if let IntentApply::OutcomeUnknown { reason } = &applied {
            let record = outcome_unknown_record(&req, recorded_at_ms, reason.clone());
            self.inner
                .lock()
                .expect("action lock")
                .actions
                .insert(req.action_id.clone(), record.clone());
            return Ok(ActionOutcome { record });
        }
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
        // A concurrent cancel or restore may have already settled this action.
        if let Some(existing) = guard.actions.get(&req.action_id)
            && existing.status != ActionStatus::Accepted
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
            IntentApply::OutcomeUnknown { reason } => {
                outcome_unknown_record(&req, recorded_at_ms, reason)
            }
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
        self.cancel_owned(action_id, None)
    }

    /// Check Session ownership and pin the original record before calling the bridge.
    pub fn cancel_for_session(
        &self,
        session_id: &str,
        action_id: &str,
    ) -> Result<ActionOutcome, ArtifactError> {
        validate_id("session_id", session_id).map_err(ArtifactError::Invalid)?;
        self.cancel_owned(action_id, Some(session_id))
    }

    fn cancel_owned(
        &self,
        action_id: &str,
        session_id: Option<&str>,
    ) -> Result<ActionOutcome, ArtifactError> {
        validate_id("action_id", action_id).map_err(ArtifactError::Invalid)?;
        let snapshot = {
            let mut guard = self.inner.lock().expect("action lock");
            let snapshot = guard
                .actions
                .get(action_id)
                .cloned()
                .ok_or_else(|| ArtifactError::NotFound(action_id.to_owned()))?;
            if let Some(session_id) = session_id
                && snapshot.session_id != session_id
            {
                return Err(ArtifactError::Invalid(format!(
                    "action {action_id} does not belong to session {session_id}"
                )));
            }
            *guard.in_flight.entry(action_id.to_owned()).or_default() += 1;
            snapshot
        };
        let _in_flight = InFlightAction {
            inner: &self.inner,
            action_id: action_id.to_owned(),
        };
        if snapshot.status == ActionStatus::Cancelled {
            return Ok(ActionOutcome { record: snapshot });
        }
        if snapshot.status == ActionStatus::OutcomeUnknown {
            return Err(ArtifactError::NeedsReconciliation(format!(
                "action {action_id} has outcome_unknown and cannot be cancelled without reconciliation"
            )));
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

        let signalled = self.dispatch.cancel_action(action_id).map_err(|reason| {
            if reason.outcome_unknown {
                ArtifactError::NeedsReconciliation(format!("cancel_action: {reason}"))
            } else {
                ArtifactError::Invalid(format!("cancel_action: {reason}"))
            }
        })?;

        let mut guard = self.inner.lock().expect("action lock");
        let existing = guard
            .actions
            .get_mut(action_id)
            .ok_or_else(|| ArtifactError::NotFound(action_id.to_owned()))?;
        if existing.status == ActionStatus::OutcomeUnknown {
            return Err(ArtifactError::NeedsReconciliation(format!(
                "action {action_id} has outcome_unknown and cannot be cancelled without reconciliation"
            )));
        }
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

    pub fn has_outcome_unknown(&self, session_id: &str) -> bool {
        let guard = self.inner.lock().expect("action lock");
        guard
            .actions
            .values()
            .any(|r| r.session_id == session_id && r.status == ActionStatus::OutcomeUnknown)
    }

    /// Reconcile an action with verified host evidence.
    pub fn reconcile(
        &self,
        reconciliation: &ArtifactReconciliation,
    ) -> Result<ActionRecord, ArtifactError> {
        validate_id("action_id", &reconciliation.action_id).map_err(ArtifactError::Invalid)?;
        validate_id("session_id", &reconciliation.session_id).map_err(ArtifactError::Invalid)?;
        validate_id("artifact_id", &reconciliation.expected_artifact_id)
            .map_err(ArtifactError::Invalid)?;

        let mut guard = self.inner.lock().expect("action lock");
        let action = guard
            .actions
            .get_mut(&reconciliation.action_id)
            .ok_or_else(|| ArtifactError::NotFound(reconciliation.action_id.clone()))?;

        if action.session_id != reconciliation.session_id {
            return Err(ArtifactError::Invalid(format!(
                "action {} belongs to session {}, not {}",
                reconciliation.action_id, action.session_id, reconciliation.session_id
            )));
        }
        if action.artifact_id != reconciliation.expected_artifact_id {
            return Err(ArtifactError::Invalid(format!(
                "action {} targets artifact {}, not {}",
                reconciliation.action_id, action.artifact_id, reconciliation.expected_artifact_id
            )));
        }
        if reconciliation.call_id.is_none()
            && action.status != ActionStatus::OutcomeUnknown
            && action.status != ActionStatus::Accepted
        {
            let matches_existing = match (&reconciliation.outcome, action.status) {
                (ArtifactReconciledOutcome::Applied { result_json }, ActionStatus::Applied) => {
                    action.outcome_json.as_ref() == result_json.as_ref()
                }
                (ArtifactReconciledOutcome::NotExecuted { .. }, ActionStatus::Cancelled) => true,
                (ArtifactReconciledOutcome::Failed { .. }, ActionStatus::Rejected) => true,
                _ => false,
            };
            if matches_existing {
                return Ok(action.clone());
            }
            return Err(ArtifactError::Invalid(format!(
                "action {} is in status {:?}, which does not require reconciliation",
                reconciliation.action_id, action.status
            )));
        }

        match &reconciliation.outcome {
            ArtifactReconciledOutcome::Applied { result_json } => {
                action.status = ActionStatus::Applied;
                action.outcome_json = result_json.clone();
                action.rejection_reason = None;
            }
            ArtifactReconciledOutcome::NotExecuted { reason } => {
                action.status = ActionStatus::Cancelled;
                action.rejection_reason = Some(format!("reconciled_not_executed: {reason}"));
            }
            ArtifactReconciledOutcome::Failed { error } => {
                action.status = ActionStatus::Rejected;
                action.rejection_reason = Some(format!("reconciled_failed: {error}"));
            }
        }
        Ok(action.clone())
    }

    /// Refresh an unchanged, inactive unknown record from its durable decision.
    pub fn refresh_reconciled(&self, expected: &ActionRecord, record: ActionRecord) -> bool {
        if expected.status != ActionStatus::OutcomeUnknown
            || !matches!(
                record.status,
                ActionStatus::Applied | ActionStatus::Rejected | ActionStatus::Cancelled
            )
            || record.action_id != expected.action_id
            || record.artifact_id != expected.artifact_id
            || record.session_id != expected.session_id
            || record.kind != expected.kind
            || record.intent != expected.intent
            || record.requested_at_ms != expected.requested_at_ms
        {
            return false;
        }
        let mut guard = self.inner.lock().expect("action lock");
        if guard.in_flight.contains_key(&expected.action_id)
            || guard.actions.get(&expected.action_id) != Some(expected)
        {
            return false;
        }
        guard.actions.insert(record.action_id.clone(), record);
        true
    }

    /// Insert or replace a reconciled action record (e.g. recovered from durable audit).
    pub fn upsert_reconciled(&self, record: ActionRecord) {
        let mut guard = self.inner.lock().expect("action lock");
        guard.actions.insert(record.action_id.clone(), record);
    }

    pub fn export_snapshot(&self) -> ActionSnapshot {
        let guard = self.inner.lock().expect("action lock");
        ActionSnapshot {
            actions: guard.actions.values().cloned().collect(),
            state: guard.state.clone(),
        }
    }

    pub(crate) fn prepare_snapshot(
        snapshot: ActionSnapshot,
    ) -> Result<PreparedActionSnapshot, ArtifactError> {
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
        Ok((actions, snapshot.state))
    }

    pub(crate) fn commit_prepared(
        &self,
        mut actions: BTreeMap<String, ActionRecord>,
        state: BTreeMap<String, BTreeMap<String, String>>,
    ) {
        let mut guard = self.inner.lock().expect("action lock");
        // A snapshot cannot replace a record while its original dispatch or a
        // cancellation call is still running across the host bridge.
        for id in guard.in_flight.keys() {
            if let Some(existing) = guard.actions.get(id) {
                actions.insert(id.clone(), existing.clone());
            }
        }
        guard.actions = actions;
        guard.state = state;
    }

    pub fn restore_snapshot(&self, snapshot: ActionSnapshot) -> Result<(), ArtifactError> {
        let (actions, state) = Self::prepare_snapshot(snapshot)?;
        self.commit_prepared(actions, state);
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

fn outcome_unknown_record(
    req: &ActionRequest,
    recorded_at_ms: u64,
    reason: String,
) -> ActionRecord {
    ActionRecord {
        action_id: req.action_id.clone(),
        artifact_id: req.artifact_id.clone(),
        session_id: req.session_id.clone(),
        kind: req.kind,
        intent: req.intent.clone(),
        status: ActionStatus::OutcomeUnknown,
        requested_at_ms: req.requested_at_ms,
        recorded_at_ms,
        rejection_reason: Some(reason),
        outcome_json: None,
    }
}

fn classify_bridge_err(prefix: &str, error: HostDispatchError) -> IntentApply {
    if error.outcome_unknown {
        IntentApply::OutcomeUnknown {
            reason: format!("{prefix}: {}", error.message),
        }
    } else {
        let lower = error.message.to_ascii_lowercase();
        if lower.contains("cancel") {
            IntentApply::Cancelled {
                reason: format!("{prefix}: {}", error.message),
            }
        } else {
            IntentApply::Rejected {
                reason: format!("{prefix}: {}", error.message),
            }
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
    OutcomeUnknown { reason: String },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArtifactKind, HostDispatchResult, PutArtifactRequest};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::{thread, time::Duration};

    struct HeldCancel {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    struct HeldDispatch {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        cancelled: AtomicBool,
        cancellation: Option<HeldCancel>,
    }

    impl HostActionDispatch for HeldDispatch {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn follow_up_task(
            &self,
            _: FollowUpDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            unreachable!()
        }

        fn invoke_tool(
            &self,
            _: ToolDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            self.started
                .send(())
                .map_err(|error| HostDispatchError::rejected(error.to_string()))?;
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .map_err(|error| HostDispatchError::rejected(error.to_string()))?;
            Ok(HostDispatchResult {
                outcome_json: "{}".into(),
            })
        }

        fn cancel_action(&self, _: &str) -> Result<bool, HostDispatchError> {
            self.cancelled.store(true, Ordering::SeqCst);
            if let Some(held) = &self.cancellation {
                held.started
                    .send(())
                    .map_err(|error| HostDispatchError::rejected(error.to_string()))?;
                held.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(|error| HostDispatchError::rejected(error.to_string()))?;
            }
            Ok(true)
        }
    }

    struct DispatchGuard {
        release: mpsc::Sender<()>,
        handle: Option<thread::JoinHandle<Result<ActionOutcome, ArtifactError>>>,
    }

    impl DispatchGuard {
        fn finish(mut self) -> ActionOutcome {
            self.release.send(()).unwrap();
            self.handle.take().unwrap().join().unwrap().unwrap()
        }
    }

    impl Drop for DispatchGuard {
        fn drop(&mut self) {
            let _ = self.release.send(());
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[test]
    fn restore_preserves_original_dispatch_through_cancellation_until_bridge_returns() {
        let store = Arc::new(ArtifactStore::new());
        store
            .put(PutArtifactRequest {
                artifact_id: "card".into(),
                session_id: "session".into(),
                kind: ArtifactKind::HtmlCard,
                title: "Card".into(),
                body: "<p>Card</p>".into(),
                at_ms: 1,
                result_handle: None,
            })
            .unwrap();
        let (started, ready) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let bridge = Arc::new(HeldDispatch {
            started,
            release: Mutex::new(held),
            cancelled: AtomicBool::new(false),
            cancellation: None,
        });
        let actions = Arc::new(ActionProtocol::new(store, bridge.clone()));
        let running = actions.clone();
        let handle = thread::spawn(move || {
            running.dispatch(ActionRequest {
                action_id: "action".into(),
                artifact_id: "card".into(),
                session_id: "session".into(),
                kind: crate::ActionKind::FormSubmit,
                intent: ActionIntent::InvokeTool {
                    tool: "search.query".into(),
                    args_json: "{}".into(),
                },
                expected_content_version: Some(1),
                requested_at_ms: 2,
            })
        });
        let dispatch = DispatchGuard {
            release,
            handle: Some(handle),
        };
        ready.recv_timeout(Duration::from_secs(10)).unwrap();
        let original = actions.get_action("action").unwrap();
        assert_eq!(original.status, ActionStatus::Accepted);
        let mut conflict = actions.export_snapshot();
        assert_eq!(conflict.actions, vec![original.clone()]);
        conflict.actions[0].session_id = "older-session".into();
        conflict.actions[0].status = ActionStatus::Applied;

        actions.restore_snapshot(conflict.clone()).unwrap();
        assert_eq!(actions.get_action("action").unwrap(), original);
        let cancelled = actions.cancel("action").unwrap().record;
        assert!(bridge.cancelled.load(Ordering::SeqCst));
        assert_eq!(cancelled.status, ActionStatus::Cancelled);

        actions.restore_snapshot(conflict.clone()).unwrap();
        assert_eq!(actions.get_action("action").unwrap(), cancelled);
        assert_eq!(dispatch.finish().record, cancelled);

        // Once the original dispatch has returned, normal replacement applies.
        actions.restore_snapshot(conflict.clone()).unwrap();
        assert_eq!(actions.export_snapshot(), conflict);
    }

    #[test]
    fn restore_preserves_record_until_cancellation_bridge_returns() {
        let store = Arc::new(ArtifactStore::new());
        store
            .put(PutArtifactRequest {
                artifact_id: "card".into(),
                session_id: "session".into(),
                kind: ArtifactKind::HtmlCard,
                title: "Card".into(),
                body: "<p>Card</p>".into(),
                at_ms: 1,
                result_handle: None,
            })
            .unwrap();
        let (started, ready) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let (cancel_started, cancel_ready) = mpsc::channel();
        let (cancel_release, cancel_held) = mpsc::channel();
        let bridge = Arc::new(HeldDispatch {
            started,
            release: Mutex::new(held),
            cancelled: AtomicBool::new(false),
            cancellation: Some(HeldCancel {
                started: cancel_started,
                release: Mutex::new(cancel_held),
            }),
        });
        let actions = Arc::new(ActionProtocol::new(store, bridge.clone()));
        let running = actions.clone();
        let handle = thread::spawn(move || {
            running.dispatch(ActionRequest {
                action_id: "action".into(),
                artifact_id: "card".into(),
                session_id: "session".into(),
                kind: crate::ActionKind::FormSubmit,
                intent: ActionIntent::InvokeTool {
                    tool: "search.query".into(),
                    args_json: "{}".into(),
                },
                expected_content_version: Some(1),
                requested_at_ms: 2,
            })
        });
        let dispatch = DispatchGuard {
            release,
            handle: Some(handle),
        };
        ready.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            actions
                .cancel_for_session("other-session", "action")
                .is_err()
        );
        assert!(!bridge.cancelled.load(Ordering::SeqCst));

        let cancelling = actions.clone();
        let handle = thread::spawn(move || cancelling.cancel_for_session("session", "action"));
        let cancellation = DispatchGuard {
            release: cancel_release,
            handle: Some(handle),
        };
        cancel_ready.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(dispatch.finish().record.status, ActionStatus::Applied);

        // Only the cancellation call remains active across the bridge now.
        let original = actions.get_action("action").unwrap();
        let mut conflict = actions.export_snapshot();
        conflict.actions[0].session_id = "replacement-session".into();
        actions.restore_snapshot(conflict.clone()).unwrap();
        assert_eq!(actions.get_action("action").unwrap(), original);
        let cancelled = cancellation.finish().record;
        assert_eq!(cancelled.session_id, "session");
        assert_eq!(cancelled.status, ActionStatus::Cancelled);

        actions.restore_snapshot(conflict.clone()).unwrap();
        assert_eq!(actions.export_snapshot(), conflict);
    }
}
