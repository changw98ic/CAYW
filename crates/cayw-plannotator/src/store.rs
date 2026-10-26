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
///
/// Feedback IDs are globally unique (`fb-{uuid}`) so per-Session lazy restore
/// cannot collide with newly allocated IDs after process restart. Legacy
/// monotonic `fb-N` records remain readable; restore is idempotent on
/// `(session_id, feedback_id)` so an unrelated session's same legacy id does
/// not swallow another session's record.
#[derive(Debug, Default, Clone)]
pub struct FeedbackStore {
    items: Vec<StructuredFeedback>,
}

impl FeedbackStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate and allocate a feedback record **without** committing it to the
    /// in-memory store.
    ///
    /// Runtime persist must succeed before [`Self::commit`]; otherwise a failed
    /// disk write must not leave injectable state for the next Agent turn.
    pub fn prepare(draft: FeedbackDraft) -> Result<StructuredFeedback, PlannotatorError> {
        validate_draft(&draft)?;
        Ok(StructuredFeedback {
            feedback_id: allocate_feedback_id(),
            session_id: draft.session_id,
            target: draft.target,
            decision: draft.decision,
            comments: draft.comments,
            summary: draft.summary,
            created_at_ms: draft.created_at_ms,
            visible_from_attempt: draft.current_attempt.saturating_add(1),
        })
    }

    /// Commit a prepared (or restored) record into the in-memory store.
    ///
    /// Idempotent on `(session_id, feedback_id)` — same contract as [`Self::restore`].
    pub fn commit(&mut self, feedback: StructuredFeedback) {
        self.restore(feedback);
    }

    /// Remove one `(session_id, feedback_id)` record (rollback after a failed
    /// durable write when memory was updated first). No-op if absent.
    pub fn rollback(&mut self, session_id: &str, feedback_id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|item| {
            !(item.session_id == session_id && item.feedback_id == feedback_id)
        });
        self.items.len() != before
    }

    /// Record one annotation submission (memory-only convenience).
    ///
    /// `visible_from_attempt` is `current_attempt + 1` so the *subsequent*
    /// Agent attempt sees the structured feedback (SLICE I smoke).
    ///
    /// Prefer [`Self::prepare`] + durable write + [`Self::commit`] when a
    /// Session ledger is involved so disk failure cannot leave injectable state.
    pub fn submit(&mut self, draft: FeedbackDraft) -> Result<StructuredFeedback, PlannotatorError> {
        let feedback = Self::prepare(draft)?;
        self.commit(feedback.clone());
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
    /// Idempotent on `(session_id, feedback_id)` — a colliding id from another
    /// session does not suppress restore (legacy `fb-N` restart hazard).
    pub fn restore(&mut self, feedback: StructuredFeedback) {
        if self.items.iter().any(|item| {
            item.feedback_id == feedback.feedback_id && item.session_id == feedback.session_id
        }) {
            return;
        }
        self.items.push(feedback);
    }
}

fn allocate_feedback_id() -> String {
    format!("fb-{}", uuid::Uuid::new_v4())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileVersionBinding, TargetKind};

    fn draft(session: &str, target_id: &str) -> FeedbackDraft {
        FeedbackDraft {
            session_id: session.into(),
            target: AnnotationTarget {
                target_id: target_id.into(),
                kind: TargetKind::Plan,
                file: FileVersionBinding {
                    path: format!("plan:{target_id}"),
                    version: 1,
                    content_sha256: "sha".into(),
                },
                label: None,
            },
            decision: FeedbackDecision::Approve,
            comments: vec![],
            summary: None,
            created_at_ms: 1,
            current_attempt: 0,
        }
    }

    #[test]
    fn submit_allocates_unique_uuid_ids() {
        let mut store = FeedbackStore::new();
        let a = store.submit(draft("s1", "t1")).unwrap();
        let b = store.submit(draft("s1", "t2")).unwrap();
        assert!(a.feedback_id.starts_with("fb-"));
        assert!(b.feedback_id.starts_with("fb-"));
        assert_ne!(a.feedback_id, b.feedback_id);
        // UUID form: fb- + 36-char hyphenated uuid
        assert_eq!(a.feedback_id.len(), 3 + 36);
    }

    #[test]
    fn restore_does_not_swallow_legacy_id_from_other_session() {
        // Simulates the pre-fix hazard: global fb-N + lazy per-session restore
        // where session B claimed fb-1 after restart before session A hydrated.
        let mut store = FeedbackStore::new();
        let mut legacy_b = StructuredFeedback {
            feedback_id: "fb-1".into(),
            session_id: "sess-b".into(),
            target: AnnotationTarget {
                target_id: "tb".into(),
                kind: TargetKind::Plan,
                file: FileVersionBinding {
                    path: "plan:b".into(),
                    version: 1,
                    content_sha256: "sha-b".into(),
                },
                label: None,
            },
            decision: FeedbackDecision::Approve,
            comments: vec![],
            summary: Some("new after restart".into()),
            created_at_ms: 2,
            visible_from_attempt: 1,
        };
        store.restore(legacy_b.clone());

        let legacy_a = StructuredFeedback {
            feedback_id: "fb-1".into(),
            session_id: "sess-a".into(),
            target: AnnotationTarget {
                target_id: "ta".into(),
                kind: TargetKind::Plan,
                file: FileVersionBinding {
                    path: "plan:a".into(),
                    version: 1,
                    content_sha256: "sha-a".into(),
                },
                label: None,
            },
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![AnnotationComment {
                comment_id: "c1".into(),
                body: "old feedback".into(),
                anchor: None,
                created_at_ms: 1,
            }],
            summary: Some("old before restart".into()),
            created_at_ms: 1,
            visible_from_attempt: 2,
        };
        store.restore(legacy_a.clone());

        assert_eq!(store.list_for_session("sess-a").len(), 1);
        assert_eq!(store.list_for_session("sess-b").len(), 1);
        assert_eq!(
            store.list_for_session("sess-a")[0].summary.as_deref(),
            Some("old before restart")
        );
        assert_eq!(
            store.list_for_session("sess-b")[0].summary.as_deref(),
            Some("new after restart")
        );

        // Idempotent for the same session+id.
        legacy_b.summary = Some("mutated".into());
        store.restore(legacy_b);
        assert_eq!(
            store.list_for_session("sess-b")[0].summary.as_deref(),
            Some("new after restart")
        );
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn prepare_does_not_commit_until_explicit_commit() {
        let mut store = FeedbackStore::new();
        let prepared = FeedbackStore::prepare(draft("s1", "t1")).unwrap();
        assert!(store.is_empty());
        assert!(prepared.feedback_id.starts_with("fb-"));
        assert_eq!(prepared.feedback_id.len(), 3 + 36);
        store.commit(prepared.clone());
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(&prepared.feedback_id).unwrap().session_id, "s1");
    }

    #[test]
    fn rollback_removes_only_matching_session_id() {
        let mut store = FeedbackStore::new();
        let a = store.submit(draft("s1", "t1")).unwrap();
        let b = store.submit(draft("s2", "t2")).unwrap();
        // Same id string cannot happen with uuid alloc, but rollback is keyed.
        assert!(store.rollback("s1", &a.feedback_id));
        assert!(!store.rollback("s1", &a.feedback_id));
        assert_eq!(store.len(), 1);
        assert!(store.get(&b.feedback_id).is_some());
        assert!(store.list_for_session("s1").is_empty());
    }
}
