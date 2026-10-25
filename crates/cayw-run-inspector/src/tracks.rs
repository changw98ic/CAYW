//! Duration track folding (per-session and wall-aligned).

use serde::Serialize;

use crate::types::*;

/// One step placed on a duration track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DurationTrackPoint {
    pub step_id: String,
    pub turn: u64,
    pub tool: String,
    pub outcome: StepOutcome,
    /// Offset from the track epoch (`t0_ms`).
    pub offset_ms: u64,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_cursor: Option<u64>,
}

/// Duration track for one session, optionally aligned to a shared wall epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DurationTrack {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Epoch used for `offset_ms` (session-local min start, or shared wall t0).
    pub t0_ms: u64,
    pub wall_ms: u64,
    pub points: Vec<DurationTrackPoint>,
}

/// Build a duration track relative to the session's first step start.
pub fn build_duration_track(snapshot: &SessionRunSnapshot) -> DurationTrack {
    build_duration_track_with_epoch(snapshot, None)
}

/// Build a duration track; when `shared_t0` is set, offsets share that epoch.
pub fn build_duration_track_with_epoch(
    snapshot: &SessionRunSnapshot,
    shared_t0: Option<u64>,
) -> DurationTrack {
    let mut steps = snapshot.steps.clone();
    steps.sort_by_key(|step| (step.turn, step.ordinal, step.started_at_ms));

    let session_t0 = steps
        .iter()
        .map(|step| step.started_at_ms)
        .min()
        .unwrap_or(0);
    let t0_ms = shared_t0.unwrap_or(session_t0);

    let mut wall_end = session_t0;
    let mut points = Vec::with_capacity(steps.len());
    for step in &steps {
        let end = step.started_at_ms.saturating_add(step.elapsed_ms);
        wall_end = wall_end.max(end);
        points.push(DurationTrackPoint {
            step_id: step.step_id.clone(),
            turn: step.turn,
            tool: step.tool.clone(),
            outcome: step.outcome,
            offset_ms: step.started_at_ms.saturating_sub(t0_ms),
            elapsed_ms: step.elapsed_ms,
            evidence_cursor: step.evidence_cursor,
        });
    }

    DurationTrack {
        session_id: snapshot.session_id.clone(),
        label: snapshot.label.clone(),
        t0_ms,
        wall_ms: wall_end.saturating_sub(t0_ms),
        points,
    }
}

/// Shared wall-clock epoch across sessions (min started_at_ms), if any steps exist.
pub fn shared_wall_epoch(snapshots: &[SessionRunSnapshot]) -> Option<u64> {
    snapshots
        .iter()
        .flat_map(|snapshot| snapshot.steps.iter().map(|step| step.started_at_ms))
        .min()
}
