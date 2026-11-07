//! Apply structured feedback into a subsequent Agent attempt with version checks.

use std::collections::BTreeMap;

use crate::FeedbackStore;
use crate::types::{
    AgentFeedbackPayload, FileVersionBinding, PlannotatorError, StructuredFeedback,
};

/// Live content identity observed by the host at apply time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveFileVersion {
    pub version: u64,
    pub content_sha256: String,
}

/// Context for folding store feedback into one Agent attempt.
#[derive(Debug, Clone)]
pub struct ApplyContext {
    pub session_id: String,
    pub attempt: u64,
    /// Path → live `(version, sha)` currently open for the Agent.
    pub live_files: BTreeMap<String, LiveFileVersion>,
}

/// Per-feedback apply result. Version mismatch never silently rewrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied(AgentFeedbackPayload),
    RejectedVersionMismatch {
        feedback_id: String,
        bound: FileVersionBinding,
        live: LiveFileVersion,
    },
    /// Bound path has no live entry — treat as mismatch (cannot verify).
    RejectedMissingLive {
        feedback_id: String,
        bound: FileVersionBinding,
    },
}

/// Materialise every feedback visible to `ctx.attempt` for `ctx.session_id`.
///
/// Matching versions produce [`ApplyOutcome::Applied`]; mismatched or missing
/// live versions produce explicit rejects — **never** a silent apply of stale
/// annotations onto new content.
pub fn apply_feedback_for_attempt(
    store: &FeedbackStore,
    ctx: &ApplyContext,
) -> Result<Vec<ApplyOutcome>, PlannotatorError> {
    if ctx.session_id.trim().is_empty() {
        return Err(PlannotatorError::Invalid(
            "session_id must be non-empty".into(),
        ));
    }
    if ctx.attempt == 0 {
        return Err(PlannotatorError::Invalid("attempt must be >= 1".into()));
    }

    let mut outcomes = Vec::new();
    for feedback in store.for_attempt(&ctx.session_id, ctx.attempt) {
        outcomes.push(apply_one(feedback, ctx));
    }
    Ok(outcomes)
}

fn apply_one(feedback: &StructuredFeedback, ctx: &ApplyContext) -> ApplyOutcome {
    let bound = &feedback.target.file;
    match ctx.live_files.get(&bound.path) {
        None => ApplyOutcome::RejectedMissingLive {
            feedback_id: feedback.feedback_id.clone(),
            bound: bound.clone(),
        },
        Some(live)
            if live.version == bound.version && live.content_sha256 == bound.content_sha256 =>
        {
            ApplyOutcome::Applied(to_payload(feedback))
        }
        Some(live) => ApplyOutcome::RejectedVersionMismatch {
            feedback_id: feedback.feedback_id.clone(),
            bound: bound.clone(),
            live: live.clone(),
        },
    }
}

fn to_payload(feedback: &StructuredFeedback) -> AgentFeedbackPayload {
    AgentFeedbackPayload {
        feedback_id: feedback.feedback_id.clone(),
        session_id: feedback.session_id.clone(),
        target_id: feedback.target.target_id.clone(),
        target_kind: feedback.target.kind,
        path: feedback.target.file.path.clone(),
        file_version: feedback.target.file.version,
        content_sha256: feedback.target.file.content_sha256.clone(),
        decision: feedback.decision,
        comments: feedback.comments.clone(),
        summary: feedback.summary.clone(),
    }
}

/// Convenience: collect only successfully applied payloads (rejects stay explicit).
pub fn applied_payloads(outcomes: &[ApplyOutcome]) -> Vec<&AgentFeedbackPayload> {
    outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            ApplyOutcome::Applied(payload) => Some(payload),
            _ => None,
        })
        .collect()
}
