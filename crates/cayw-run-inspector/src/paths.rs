//! Failure → recovery path folding.

use serde::Serialize;

use crate::types::*;

/// One failure and the deterministic recovery classification that followed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPath {
    /// Stable id: `{session_id}:{failure_step_id}:{recovery}`.
    pub path_id: String,
    pub session_id: String,
    pub failure_step_id: String,
    pub failure_tool: String,
    pub failure_params_digest: String,
    pub recovery: RecoveryKind,
    /// Steps considered part of this recovery chain (not including the failure).
    pub follow_up_step_ids: Vec<String>,
    /// Wall time from failure start to last follow-up end, when recovered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_elapsed_ms: Option<u64>,
    /// Evidence cursors along the chain (failure first, then follow-ups).
    pub evidence_cursors: Vec<u64>,
}

fn step_key(step: &RunStep) -> (u64, u64) {
    (step.turn, step.ordinal)
}

fn classify(failure: &RunStep, next: &RunStep) -> RecoveryKind {
    if next.tool != failure.tool {
        RecoveryKind::ChangedTool
    } else if next.params_digest != failure.params_digest {
        RecoveryKind::ChangedParams
    } else {
        RecoveryKind::SameParamsRetry
    }
}

fn end_ms(step: &RunStep) -> u64 {
    step.started_at_ms.saturating_add(step.elapsed_ms)
}

/// Build retry / recovery paths from a session run snapshot.
///
/// Only [`StepOutcome::Failed`] steps open a path. Empty / blind-retry steps
/// are not chain heads (they still appear as follow-ups when present).
/// Look-ahead stops at the next success with a different tool+params identity
/// after a recovering attempt, or at the next unrelated failure head.
pub fn build_retry_paths(snapshot: &SessionRunSnapshot) -> Vec<RetryPath> {
    let mut steps: Vec<&RunStep> = snapshot.steps.iter().collect();
    steps.sort_by_key(|step| step_key(step));

    let mut paths = Vec::new();
    let mut index = 0usize;
    while index < steps.len() {
        let failure = steps[index];
        if failure.outcome != StepOutcome::Failed {
            index += 1;
            continue;
        }

        let mut follow_ups: Vec<&RunStep> = Vec::new();
        let mut recovery = RecoveryKind::Unrecovered;
        let mut cursor = index + 1;
        while cursor < steps.len() {
            let candidate = steps[cursor];
            // Stop before another failure that starts its own path, unless it is
            // clearly a same-identity retry continuation.
            if candidate.outcome == StepOutcome::Failed
                && !(candidate.tool == failure.tool
                    && candidate.params_digest == failure.params_digest)
            {
                break;
            }
            let kind = classify(failure, candidate);
            follow_ups.push(candidate);
            recovery = kind;
            if candidate.outcome == StepOutcome::Success {
                break;
            }
            // Same-params retries may continue; stop after a strategy change
            // once we have at least one follow-up classified.
            if matches!(
                kind,
                RecoveryKind::ChangedParams | RecoveryKind::ChangedTool
            ) && candidate.outcome != StepOutcome::Failed
            {
                break;
            }
            cursor += 1;
        }

        let mut evidence_cursors = Vec::new();
        if let Some(cursor) = failure.evidence_cursor {
            evidence_cursors.push(cursor);
        }
        for step in &follow_ups {
            if let Some(cursor) = step.evidence_cursor {
                evidence_cursors.push(cursor);
            }
        }

        let recovery_elapsed_ms = follow_ups
            .last()
            .map(|last| end_ms(last).saturating_sub(failure.started_at_ms));
        let recovery_elapsed_ms = match recovery {
            RecoveryKind::Unrecovered if follow_ups.is_empty() => None,
            _ => recovery_elapsed_ms,
        };

        let path_id = format!(
            "{}:{}:{}",
            snapshot.session_id,
            failure.step_id,
            recovery_slug(recovery)
        );

        paths.push(RetryPath {
            path_id,
            session_id: snapshot.session_id.clone(),
            failure_step_id: failure.step_id.clone(),
            failure_tool: failure.tool.clone(),
            failure_params_digest: failure.params_digest.clone(),
            recovery,
            follow_up_step_ids: follow_ups.iter().map(|step| step.step_id.clone()).collect(),
            recovery_elapsed_ms,
            evidence_cursors,
        });

        index += 1;
    }
    paths
}

fn recovery_slug(kind: RecoveryKind) -> &'static str {
    match kind {
        RecoveryKind::SameParamsRetry => "same_params_retry",
        RecoveryKind::ChangedParams => "changed_params",
        RecoveryKind::ChangedTool => "changed_tool",
        RecoveryKind::Unrecovered => "unrecovered",
    }
}
