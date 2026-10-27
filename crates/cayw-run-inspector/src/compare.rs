//! Cross-session timeline alignment and comparison.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::paths::{RetryPath, build_retry_paths};
use crate::tracks::{DurationTrack, build_duration_track_with_epoch, shared_wall_epoch};
use crate::types::*;

/// One session's contribution to an aligned turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnLane {
    pub session_id: String,
    pub step_ids: Vec<String>,
    /// Sum of step elapsed_ms in this turn (idle between turns excluded).
    pub elapsed_ms: u64,
    /// Count of Failed / Empty / BlindRetry steps in this turn.
    pub branch_count: u64,
}

/// Turn present in at least one compared session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AlignedTurn {
    pub turn: u64,
    pub lanes: Vec<TurnLane>,
}

/// Session A/B (2..N) comparison view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompare {
    /// True when every snapshot shares the same non-empty task fingerprint.
    pub same_task: bool,
    pub session_ids: Vec<String>,
    /// Wall-aligned duration tracks (shared `t0_ms` when steps exist).
    pub tracks: Vec<DurationTrack>,
    /// Turn-aligned lanes (only meaningful when `same_task`).
    pub aligned_turns: Vec<AlignedTurn>,
    pub retry_paths_by_session: BTreeMap<String, Vec<RetryPath>>,
}

fn is_branch(outcome: StepOutcome) -> bool {
    matches!(
        outcome,
        StepOutcome::Failed | StepOutcome::Empty | StepOutcome::BlindRetry
    )
}

fn build_turn_lane(session_id: &str, steps: &[&RunStep]) -> TurnLane {
    let mut ordered: Vec<&RunStep> = steps.to_vec();
    ordered.sort_by_key(|step| (step.ordinal, step.started_at_ms));
    TurnLane {
        session_id: session_id.to_owned(),
        step_ids: ordered.iter().map(|step| step.step_id.clone()).collect(),
        elapsed_ms: ordered.iter().map(|step| step.elapsed_ms).sum(),
        branch_count: ordered
            .iter()
            .filter(|step| is_branch(step.outcome))
            .count() as u64,
    }
}

/// Compare two or more session run snapshots on a shared wall axis.
///
/// Turn alignment is emitted when all snapshots share the same non-empty
/// `task_fingerprint`. Differing tasks still get wall-aligned tracks and
/// per-session retry paths, but `aligned_turns` stays empty.
pub fn compare_sessions(
    snapshots: &[SessionRunSnapshot],
) -> Result<SessionCompare, InspectorError> {
    if snapshots.len() < 2 {
        return Err(InspectorError::Invalid(
            "compare_sessions requires at least two session snapshots".into(),
        ));
    }

    let fingerprints: BTreeSet<Option<&str>> = snapshots
        .iter()
        .map(|snapshot| snapshot.task_fingerprint.as_deref())
        .collect();
    let same_task = fingerprints.len() == 1
        && fingerprints
            .iter()
            .next()
            .and_then(|value| *value)
            .is_some_and(|value| !value.is_empty());

    let epoch = shared_wall_epoch(snapshots);
    let tracks: Vec<DurationTrack> = snapshots
        .iter()
        .map(|snapshot| build_duration_track_with_epoch(snapshot, epoch))
        .collect();

    let mut retry_paths_by_session = BTreeMap::new();
    for snapshot in snapshots {
        retry_paths_by_session.insert(snapshot.session_id.clone(), build_retry_paths(snapshot));
    }

    let aligned_turns = if same_task {
        let mut turns: BTreeSet<u64> = BTreeSet::new();
        for snapshot in snapshots {
            for step in &snapshot.steps {
                turns.insert(step.turn);
            }
        }
        turns
            .into_iter()
            .map(|turn| {
                let lanes = snapshots
                    .iter()
                    .filter_map(|snapshot| {
                        let steps: Vec<&RunStep> = snapshot
                            .steps
                            .iter()
                            .filter(|step| step.turn == turn)
                            .collect();
                        if steps.is_empty() {
                            None
                        } else {
                            Some(build_turn_lane(&snapshot.session_id, &steps))
                        }
                    })
                    .collect();
                AlignedTurn { turn, lanes }
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(SessionCompare {
        same_task,
        session_ids: snapshots
            .iter()
            .map(|snapshot| snapshot.session_id.clone())
            .collect(),
        tracks,
        aligned_turns,
        retry_paths_by_session,
    })
}
