//! Serializable run snapshots and inspector DTOs for F06 first cut.

use serde::{Deserialize, Serialize};

/// Terminal / diagnostic class for one observed run step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// Tool / answer step completed without failure flags.
    Success,
    /// Explicit failure (error flag or failed terminal).
    Failed,
    /// Retrieval-style empty result (not a hard failure).
    Empty,
    /// Behavioural blind retry (same tool + similar params after a failure).
    BlindRetry,
}

/// What happened after a failed step (deterministic recovery classification).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryKind {
    /// Same tool and same params digest retried.
    SameParamsRetry,
    /// Same tool with a different params digest.
    ChangedParams,
    /// A different tool was used next.
    ChangedTool,
    /// No later step recovered the failure within the look-ahead window.
    Unrecovered,
}

/// One observed execution step (tool call, answer, or recovery attempt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStep {
    pub step_id: String,
    pub session_id: String,
    /// One-based dialogue / attempt turn.
    pub turn: u64,
    /// Zero-based ordinal within the turn (stable ordering key with turn).
    pub ordinal: u64,
    pub tool: String,
    /// Host-supplied digest of normalized params; equal digests ⇒ same params.
    pub params_digest: String,
    pub outcome: StepOutcome,
    /// Wall-clock start (milliseconds since an arbitrary host epoch).
    pub started_at_ms: u64,
    pub elapsed_ms: u64,
    /// Optional cursor into the durable event / evidence log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_cursor: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_summary: Option<String>,
}

/// One session's run projection used as inspector input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRunSnapshot {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Same fingerprint ⇒ same logical task (enables turn-aligned compare).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_fingerprint: Option<String>,
    pub steps: Vec<RunStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InspectorError {
    #[error("invalid run inspector input: {0}")]
    Invalid(String),
}
