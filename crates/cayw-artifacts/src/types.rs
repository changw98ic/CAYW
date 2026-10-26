//! Serializable HTML artifact and host-action protocol DTOs (F09 / F19).

use serde::{Deserialize, Serialize};

/// First-cut display kinds for conversation artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Inline HTML card in the conversation / workbench.
    HtmlCard,
    /// Chart markup or chart description HTML.
    Chart,
    /// Standalone preview document (independent page body).
    StandalonePreview,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HtmlCard => "html_card",
            Self::Chart => "chart",
            Self::StandalonePreview => "standalone_preview",
        }
    }
}

/// One stored HTML (or chart) artifact owned by a Session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecord {
    pub artifact_id: String,
    pub session_id: String,
    pub kind: ArtifactKind,
    pub title: String,
    /// HTML / chart body. Host / UI decides sandboxing; this crate stores text only.
    pub body: String,
    /// Monotonic content version; actions may pin `expected_content_version`.
    pub content_version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Optional F01-style result handle reference (opaque string; not resolved here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_handle: Option<String>,
}

/// Renderer-facing preview projection (not the storage record).
///
/// UI / WebUI / TUI consume this shape to render HTML; they do not need the
/// full mutable catalog row. `html` is the body to place in a sandboxed surface;
/// `content_type` is a fixed first-cut hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPreview {
    pub artifact_id: String,
    pub session_id: String,
    pub kind: ArtifactKind,
    pub title: String,
    /// HTML / chart markup for the renderer.
    pub html: String,
    /// MIME hint for renderers (`text/html; charset=utf-8` in this first cut).
    pub content_type: String,
    pub content_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_handle: Option<String>,
}

impl ArtifactPreview {
    pub const HTML_CONTENT_TYPE: &'static str = "text/html; charset=utf-8";

    pub fn from_record(record: &ArtifactRecord) -> Self {
        Self {
            artifact_id: record.artifact_id.clone(),
            session_id: record.session_id.clone(),
            kind: record.kind,
            title: record.title.clone(),
            html: record.body.clone(),
            content_type: Self::HTML_CONTENT_TYPE.into(),
            content_version: record.content_version,
            result_handle: record.result_handle.clone(),
        }
    }
}

/// How the user interacted with an artifact surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Button,
    FormSubmit,
    ChartSelect,
    Custom,
}

/// What the host should do after accepting an action (structured intent only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionIntent {
    /// Ask Runtime / Agent to start a follow-up task in the same Session.
    FollowUpTask { prompt: String },
    /// Ask host to invoke an already-authorized tool (no new capability grant).
    InvokeTool {
        tool: String,
        #[serde(default)]
        args_json: String,
    },
    /// Persist a small UI/state key on the artifact (no execution).
    UpdateState {
        key: String,
        #[serde(default)]
        value_json: String,
    },
    /// Explicitly read-only acknowledgement (F09 first-cut default for unknown UI).
    ReadOnly {
        #[serde(default)]
        note: String,
    },
}

/// Incoming host action from a card / form / chart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub action_id: String,
    pub artifact_id: String,
    /// Caller-claimed Session; must match the artifact's owner.
    pub session_id: String,
    pub kind: ActionKind,
    pub intent: ActionIntent,
    /// When set, must equal the artifact's current `content_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_content_version: Option<u64>,
    pub requested_at_ms: u64,
}

/// Lifecycle of a recorded action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Accepted,
    Rejected,
    Applied,
}

/// Durable record of a dispatched action (accepted or rejected).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRecord {
    pub action_id: String,
    pub artifact_id: String,
    pub session_id: String,
    pub kind: ActionKind,
    pub intent: ActionIntent,
    pub status: ActionStatus,
    pub requested_at_ms: u64,
    pub recorded_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
    /// Structured outcome payload for accepted intents (JSON object text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_json: Option<String>,
}

/// Result returned to the UI / host after `dispatch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionOutcome {
    pub record: ActionRecord,
}

/// Put request for a new artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutArtifactRequest {
    pub artifact_id: String,
    pub session_id: String,
    pub kind: ArtifactKind,
    pub title: String,
    pub body: String,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_handle: Option<String>,
}

/// Body/title update for an existing artifact (same Session only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateArtifactRequest {
    pub artifact_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_content_version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactError {
    #[error("invalid artifact input: {0}")]
    Invalid(String),
    #[error("artifact not found: {0}")]
    NotFound(String),
    #[error("artifact already exists: {0}")]
    AlreadyExists(String),
    #[error("session mismatch for artifact {artifact_id}: owner={owner}, claimed={claimed}")]
    SessionMismatch {
        artifact_id: String,
        owner: String,
        claimed: String,
    },
    #[error(
        "content version mismatch for artifact {artifact_id}: expected {expected}, actual {actual}"
    )]
    VersionMismatch {
        artifact_id: String,
        expected: u64,
        actual: u64,
    },
    #[error("action already recorded: {0}")]
    DuplicateAction(String),
    #[error("artifact io: {0}")]
    Io(String),
}
