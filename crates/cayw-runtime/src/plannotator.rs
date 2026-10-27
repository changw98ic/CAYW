//! Wire cayw-plannotator into Runtime/Agent: submit → Core session log, apply →
//! SystemMessage section on the next Agent attempt (F08 audit remediation).

use crate::{Inner, Runtime, RuntimeError, collaboration, lock};
use cayw_ai::{Message, SystemMessage};
use cayw_core::CoreEvent;
use cayw_plannotator::{
    ApplyContext, ApplyOutcome, FeedbackDecision, FeedbackDraft, FeedbackStore, FileVersionBinding,
    LiveFileVersion, StructuredFeedback, TargetKind, applied_feedback_section,
    apply_feedback_for_attempt,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const FEEDBACK_RECORDED: &str = "plannotator_feedback_recorded";

#[derive(Debug, Default)]
pub(crate) struct PlannotatorState {
    store: FeedbackStore,
    live_files: BTreeMap<String, BTreeMap<String, LiveFileVersion>>,
    hydrated: BTreeSet<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanFeedbackInput {
    pub session_id: String,
    pub expected_task_id: String,
    pub expected_version: u64,
    pub expected_content_sha256: String,
    pub decision: FeedbackDecision,
    #[serde(default)]
    pub comments: Vec<cayw_plannotator::AnnotationComment>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default = "default_actor")]
    pub actor: String,
}

fn default_actor() -> String {
    "host".into()
}

fn plan_path(task_id: &str) -> String {
    format!("plan:{task_id}")
}

pub(crate) fn plan_content_sha(steps: &[crate::PlanStep]) -> Result<String, RuntimeError> {
    let encoded = serde_json::to_vec(steps).map_err(RuntimeError::Json)?;
    Ok(hex_sha256(&encoded))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn attempt_ordinal(inner: &Inner, session: &str) -> Result<u64, RuntimeError> {
    let events = collaboration::read_log(inner, session)?;
    let count = events
        .iter()
        .filter(|record| matches!(record.event, CoreEvent::AttemptStarted { .. }))
        .count() as u64;
    Ok(count)
}

fn hydrate_session(inner: &Inner, session: &str) -> Result<(), RuntimeError> {
    {
        let state = lock(&inner.plannotator);
        if state.hydrated.contains(session) {
            return Ok(());
        }
    }
    let events = collaboration::read_log(inner, session)?;
    let mut state = lock(&inner.plannotator);
    if state.hydrated.contains(session) {
        return Ok(());
    }
    for record in events {
        let CoreEvent::Host { kind, data } = record.event else {
            continue;
        };
        if kind != FEEDBACK_RECORDED {
            continue;
        }
        let payload = data.get("decision").cloned().unwrap_or(data);
        let feedback: StructuredFeedback = serde_json::from_value(payload).map_err(|error| {
            RuntimeError::InvalidInput(format!("invalid plannotator feedback event: {error}"))
        })?;
        if feedback.session_id != session {
            return Err(RuntimeError::InvalidInput(
                "plannotator feedback event session mismatch".into(),
            ));
        }
        state.store.restore(feedback);
    }
    state.hydrated.insert(session.into());
    Ok(())
}

fn persist_feedback(
    inner: &Inner,
    session: &str,
    actor: &str,
    feedback: &StructuredFeedback,
) -> Result<(), RuntimeError> {
    let snapshot = inner.store.snapshot(session)?;
    let data = serde_json::to_value(feedback).map_err(RuntimeError::Json)?;
    inner
        .store
        .append_control_event(
            session,
            snapshot.cursor,
            snapshot.attempt.as_ref(),
            actor,
            FEEDBACK_RECORDED,
            data,
        )
        .map_err(RuntimeError::Core)?;
    inner.changed.notify_waiters();
    Ok(())
}

impl Runtime {
    /// Record structured annotation feedback for a subsequent Agent attempt.
    ///
    /// Durability order: **disk (Core session log) first, then memory**. A
    /// failed persist must not leave an injectable record for the next Agent
    /// turn — prepare validates/allocates, append to Core, only then commit
    /// into the process-local store used by apply/inject.
    pub fn submit_plannotator_feedback(
        &self,
        draft: FeedbackDraft,
        actor: &str,
    ) -> Result<StructuredFeedback, RuntimeError> {
        if actor.trim().is_empty() || actor.len() > 128 {
            return Err(RuntimeError::InvalidInput(
                "actor must contain 1..128 bytes".into(),
            ));
        }
        self.get(&draft.session_id)?;
        hydrate_session(&self.inner, &draft.session_id)?;
        let feedback = FeedbackStore::prepare(draft)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        // Durable write before memory commit so a failed API cannot leave
        // injectable state for for_attempt / feedback_messages_for_attempt.
        persist_feedback(&self.inner, &feedback.session_id, actor, &feedback)?;
        {
            let mut state = lock(&self.inner.plannotator);
            state.store.commit(feedback.clone());
        }
        Ok(feedback)
    }

    /// Submit feedback for the exact [`Runtime::plan`] snapshot the client reviewed.
    ///
    /// The shared coordination lock serializes plan updates with validation and
    /// persistence. Target identity is derived from the authoritative plan only.
    pub fn submit_plan_feedback(
        &self,
        input: PlanFeedbackInput,
    ) -> Result<StructuredFeedback, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let plan = self
            .plan(&input.session_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("session has no plan to annotate".into()))?;
        let path = plan_path(&plan.task_id);
        let sha = plan_content_sha(&plan.steps)?;
        if input.expected_task_id != plan.task_id
            || input.expected_version != plan.version
            || input.expected_content_sha256 != sha
        {
            return Err(RuntimeError::InvalidInput(
                "plan feedback version conflict: reload and review the current plan before submitting".into(),
            ));
        }
        let current_attempt = attempt_ordinal(&self.inner, &input.session_id)?;
        let feedback = self.submit_plannotator_feedback(
            FeedbackDraft {
                session_id: input.session_id,
                target: cayw_plannotator::AnnotationTarget {
                    target_id: format!("plan-{}", plan.task_id),
                    kind: TargetKind::Plan,
                    file: FileVersionBinding {
                        path: path.clone(),
                        version: plan.version,
                        content_sha256: sha.clone(),
                    },
                    label: Some(format!("plan v{}", plan.version)),
                },
                decision: input.decision,
                comments: input.comments,
                summary: input.summary,
                created_at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                current_attempt,
            },
            &input.actor,
        )?;
        let mut state = lock(&self.inner.plannotator);
        state
            .live_files
            .entry(feedback.session_id.clone())
            .or_default()
            .insert(
                path,
                LiveFileVersion {
                    version: plan.version,
                    content_sha256: sha,
                },
            );
        Ok(feedback)
    }

    /// Host-supplied live file versions for apply-time mismatch checks.
    pub fn set_plannotator_live_files(
        &self,
        session: &str,
        files: BTreeMap<String, LiveFileVersion>,
    ) -> Result<(), RuntimeError> {
        self.get(session)?;
        let mut state = lock(&self.inner.plannotator);
        state.live_files.insert(session.into(), files);
        Ok(())
    }

    /// Feedback recorded for a session (hydrates from Core events when needed).
    pub fn plannotator_feedback(
        &self,
        session: &str,
    ) -> Result<Vec<StructuredFeedback>, RuntimeError> {
        self.get(session)?;
        hydrate_session(&self.inner, session)?;
        let state = lock(&self.inner.plannotator);
        Ok(state
            .store
            .list_for_session(session)
            .into_iter()
            .cloned()
            .collect())
    }
}

/// Apply stored feedback for the active attempt and return a System message to
/// prepend into Agent `prompt_messages` (empty Vec when nothing applies).
pub(crate) fn feedback_messages_for_attempt(
    inner: &Inner,
    session_id: &str,
) -> Result<Vec<Message>, RuntimeError> {
    hydrate_session(inner, session_id)?;
    let attempt = attempt_ordinal(inner, session_id)?;
    if attempt == 0 {
        return Ok(vec![]);
    }
    let (outcomes, live_empty) = {
        let state = lock(&inner.plannotator);
        let live = state
            .live_files
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        let live_empty = live.is_empty();
        let outcomes = apply_feedback_for_attempt(
            &state.store,
            &ApplyContext {
                session_id: session_id.into(),
                attempt,
                live_files: live,
            },
        )
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        (outcomes, live_empty)
    };
    // No live map and nothing visible → skip. Visible feedback without live
    // entries yields explicit RejectedMissingLive (still no silent apply).
    if live_empty
        && outcomes
            .iter()
            .all(|outcome| !matches!(outcome, ApplyOutcome::Applied(_)))
    {
        let state = lock(&inner.plannotator);
        if state.store.for_attempt(session_id, attempt).is_empty() {
            return Ok(vec![]);
        }
    }
    match applied_feedback_section(&outcomes)
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?
    {
        None => Ok(vec![]),
        Some((key, body)) => Ok(vec![Message::System(SystemMessage {
            sections: [(key, Some(body))].into(),
            ..Default::default()
        })]),
    }
}

/// Sync live plan identity from Core before apply when a plan exists.
pub(crate) fn refresh_plan_live_files(inner: &Inner, session_id: &str) -> Result<(), RuntimeError> {
    let events = collaboration::read_log(inner, session_id)?;
    let Some(stored) = latest_plan_any(&events)? else {
        return Ok(());
    };
    let sha = plan_content_sha(&stored.steps)?;
    let path = plan_path(&stored.task_id);
    let mut state = lock(&inner.plannotator);
    state
        .live_files
        .entry(session_id.into())
        .or_default()
        .insert(
            path,
            LiveFileVersion {
                version: stored.version,
                content_sha256: sha,
            },
        );
    Ok(())
}

struct PlanSnap {
    task_id: String,
    version: u64,
    steps: Vec<crate::PlanStep>,
}

fn latest_plan_any(events: &[cayw_core::EventRecord]) -> Result<Option<PlanSnap>, RuntimeError> {
    // Prefer Runtime::plan semantics via collaboration host events.
    let mut latest = None;
    for record in events {
        let CoreEvent::Host { kind, data } = &record.event else {
            continue;
        };
        if kind != "collaboration_plan_updated" {
            continue;
        }
        let task_id = data
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::InvalidInput("plan event missing task_id".into()))?
            .to_string();
        let version = data
            .get("version")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| RuntimeError::InvalidInput("plan event missing version".into()))?;
        let steps: Vec<crate::PlanStep> = serde_json::from_value(
            data.get("steps")
                .cloned()
                .ok_or_else(|| RuntimeError::InvalidInput("plan event missing steps".into()))?,
        )
        .map_err(|error| RuntimeError::InvalidInput(format!("plan steps: {error}")))?;
        latest = Some(PlanSnap {
            task_id,
            version,
            steps,
        });
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_plannotator::{
        AnnotationComment, AnnotationTarget, FEEDBACK_SECTION_KEY, FeedbackDecision,
    };

    #[test]
    fn section_key_is_stable() {
        assert_eq!(FEEDBACK_SECTION_KEY, "cayw-plannotator-feedback");
    }

    #[test]
    fn plan_path_stable() {
        assert_eq!(plan_path("task-1"), "plan:task-1");
    }

    #[test]
    fn draft_round_trip_shapes() {
        let draft = FeedbackDraft {
            session_id: "s".into(),
            target: AnnotationTarget {
                target_id: "t".into(),
                kind: TargetKind::Plan,
                file: FileVersionBinding {
                    path: "plan:t".into(),
                    version: 1,
                    content_sha256: "abc".into(),
                },
                label: None,
            },
            decision: FeedbackDecision::ApproveWithNotes,
            comments: vec![AnnotationComment {
                comment_id: "c1".into(),
                body: "note".into(),
                anchor: None,
                created_at_ms: 1,
            }],
            summary: Some("ok".into()),
            created_at_ms: 1,
            current_attempt: 1,
        };
        let mut store = FeedbackStore::new();
        let fb = store.submit(draft).unwrap();
        assert_eq!(fb.visible_from_attempt, 2);
    }
}
