//! Per-session run timeline (retry paths + duration track).

use serde::Serialize;

use crate::paths::{RetryPath, build_retry_paths};
use crate::tracks::{DurationTrack, build_duration_track};
use crate::types::*;

/// First-cut run inspector view for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunTimeline {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_fingerprint: Option<String>,
    pub step_count: u64,
    pub failure_count: u64,
    pub retry_paths: Vec<RetryPath>,
    pub track: DurationTrack,
}

/// Fold one session into a run timeline (paths + duration track).
pub fn build_run_timeline(snapshot: &SessionRunSnapshot) -> RunTimeline {
    let failure_count = snapshot
        .steps
        .iter()
        .filter(|step| step.outcome == StepOutcome::Failed)
        .count() as u64;
    RunTimeline {
        session_id: snapshot.session_id.clone(),
        label: snapshot.label.clone(),
        task_fingerprint: snapshot.task_fingerprint.clone(),
        step_count: snapshot.steps.len() as u64,
        failure_count,
        retry_paths: build_retry_paths(snapshot),
        track: build_duration_track(snapshot),
    }
}
