//! In-memory Session-scoped store for structured annotation feedback.

use crate::types::{
    AnnotationComment, AnnotationTarget, FeedbackDecision, PlannotatorError, StructuredFeedback,
};

/// Host-supplied draft used to record one human review submission.
#[derive(Debug, Clone)]
pub struct FeedbackDraft {
    pub session_id: String,
    pub target: AnnotationTarget,
    pub decision: FeedbackDecision,
    pub comments: Vec<AnnotationComment>,
    pub summary: Option<String>,
    pub created_at_ms: u64,
    /// Attempt that just finished / is waiting; feedback becomes visible from the next.
    pub current_attempt: u64,
}

/// Process-local store (first cut). Durable Session ledger wiring is later.
#[derive(Debug, Default, Clone)]
pub struct FeedbackStore {
    items: Vec<StructuredFeedback>,
    next_id: u64,
}

impl FeedbackStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one annotation submission.
    ///
    /// `visible_from_attempt` is `current_attempt + 1` so the *subsequent*
    /// Agent attempt sees the structured feedback (SLICE I smoke).
    pub fn submit(&mut self, draft: FeedbackDraft) -> Result<StructuredFeedback, PlannotatorError> {
        validate_draft(&draft)?;
        self.next_id = self.next_id.saturating_add(1);
        let feedback = StructuredFeedback {
            feedback_id: format!("fb-{}", self.next_id),
            session_id: draft.session_id,
            target: draft.target,
            decision: draft.decision,
            comments: draft.comments,
            summary: draft.summary,
            created_at_ms: draft.created_at_ms,
            visible_from_attempt: draft.current_attempt.saturating_add(1),
        };
        self.items.push(feedback.clone());
        Ok(feedback)
    }

    pub fn get(&self, feedback_id: &str) -> Option<&StructuredFeedback> {
        self.items
            .iter()
            .find(|item| item.feedback_id == feedback_id)
    }

    pub fn list_for_session(&self, session_id: &str) -> Vec<&StructuredFeedback> {
        self.items
            .iter()
            .filter(|item| item.session_id == session_id)
            .collect()
    }

    /// Feedback visible to a concrete attempt in a Session.
    pub fn for_attempt(&self, session_id: &str, attempt: u64) -> Vec<&StructuredFeedback> {
        self.items
            .iter()
            .filter(|item| item.session_id == session_id && item.visible_from_attempt <= attempt)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Rehydrate one previously recorded feedback (Core event replay / import).
    ///
    /// Skips draft validation; callers must supply already-accepted records.
    /// Idempotent on `feedback_id`.
    pub fn restore(&mut self, feedback: StructuredFeedback) {
        if self.items.iter().any(|item| item.feedback_id == feedback.feedback_id) {
            return;
        }
        if let Some(n) = feedback
            .feedback_id
            .strip_prefix("fb-")
            .and_then(|s| s.parse::<u64>().ok())
        {
            self.next_id = self.next_id.max(n);
        }
        self.items.push(feedback);
    }
}

fn validate_draft(draft: &FeedbackDraft) -> Result<(), PlannotatorError> {
    if draft.session_id.trim().is_empty() {
        return Err(PlannotatorError::Invalid(
            "session_id must be non-empty".into(),
        ));
    }
    if draft.target.target_id.trim().is_empty() {
        return Err(PlannotatorError::Invalid(
            "target_id must be non-empty".into(),
        ));
    }
    if draft.target.file.path.trim().is_empty() {
        return Err(PlannotatorError::Invalid(
            "file.path must be non-empty".into(),
        ));
    }
    if draft.target.file.content_sha256.trim().is_empty() {
        return Err(PlannotatorError::Invalid(
            "file.content_sha256 must be non-empty".into(),
        ));
    }
    if draft.target.file.version == 0 {
        return Err(PlannotatorError::Invalid(
            "file.version must be >= 1".into(),
        ));
    }
    match draft.decision {
        FeedbackDecision::DenyWithFeedback | FeedbackDecision::ApproveWithNotes => {
            if draft.comments.is_empty()
                && draft
                    .summary
                    .as_ref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
            {
                return Err(PlannotatorError::Invalid(
                    "deny_with_feedback / approve_with_notes require comments or summary".into(),
                ));
            }
        }
        FeedbackDecision::Approve => {}
    }
    for comment in &draft.comments {
        if comment.comment_id.trim().is_empty() {
            return Err(PlannotatorError::Invalid(
                "comment_id must be non-empty".into(),
            ));
        }
        if comment.body.trim().is_empty() {
            return Err(PlannotatorError::Invalid(
                "comment body must be non-empty".into(),
            ));
        }
        if let Some(anchor) = &comment.anchor
            && anchor.end_offset < anchor.start_offset
        {
            return Err(PlannotatorError::Invalid(
                "anchor end_offset must be >= start_offset".into(),
            ));
        }
    }
    Ok(())
}
