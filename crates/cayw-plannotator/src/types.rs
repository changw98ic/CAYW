//! Serializable annotation targets, comments, and structured feedback DTOs.

use serde::{Deserialize, Serialize};

/// What surface the human annotated (first-cut taxonomy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// Markdown checklist / plan document.
    Plan,
    /// Arbitrary Markdown (docs, specs).
    Markdown,
    /// Unified diff / PR patch review.
    Diff,
    /// Local run / preview page (loopback HTML).
    LocalPage,
}

/// File revision binding carried with every target (F04-compatible shape, no hard dep).
///
/// Equal `(path, version, content_sha256)` means the same content identity Agent /
/// editor / Trace would share once Slice C lands.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileVersionBinding {
    pub path: String,
    pub version: u64,
    pub content_sha256: String,
}

/// One reviewable artifact inside a Session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationTarget {
    pub target_id: String,
    pub kind: TargetKind,
    pub file: FileVersionBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Byte-range (or line-derived) anchor inside the target content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationAnchor {
    pub start_offset: u64,
    pub end_offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
}

/// One in-place comment on a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationComment {
    pub comment_id: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<AnnotationAnchor>,
    pub created_at_ms: u64,
}

/// Human decision attached to a review submission (plannotator product shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackDecision {
    /// Proceed without blocking notes.
    Approve,
    /// Block execution; comments must reach the next Agent attempt.
    DenyWithFeedback,
    /// Proceed, but include guidance notes for the Agent.
    ApproveWithNotes,
}

/// Structured feedback bound to Session + target + file version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredFeedback {
    pub feedback_id: String,
    pub session_id: String,
    pub target: AnnotationTarget,
    pub decision: FeedbackDecision,
    pub comments: Vec<AnnotationComment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub created_at_ms: u64,
    /// Inclusive: visible to this attempt and later ones in the same Session.
    pub visible_from_attempt: u64,
}

/// Wire form injected into a subsequent Agent attempt (no prose reformatting).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFeedbackPayload {
    pub feedback_id: String,
    pub session_id: String,
    pub target_id: String,
    pub target_kind: TargetKind,
    pub path: String,
    pub file_version: u64,
    pub content_sha256: String,
    pub decision: FeedbackDecision,
    pub comments: Vec<AnnotationComment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlannotatorError {
    #[error("invalid annotation feedback input: {0}")]
    Invalid(String),
    #[error(
        "file version mismatch for {path}: bound version={bound_version} sha={bound_sha}, live version={live_version} sha={live_sha}"
    )]
    VersionMismatch {
        path: String,
        bound_version: u64,
        bound_sha: String,
        live_version: u64,
        live_sha: String,
    },
    #[error("feedback not found: {0}")]
    NotFound(String),
    #[error("session mismatch: expected {expected}, got {got}")]
    SessionMismatch { expected: String, got: String },
}
