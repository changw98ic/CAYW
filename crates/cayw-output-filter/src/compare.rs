//! Same-task filter on/off comparison records.
//!
//! Output reduction and task wall/cost/re-read costs are recorded separately.
//! A smaller filtered blob must **not** be reported as proof of task benefit.

use serde::{Deserialize, Serialize};

use crate::types::{FilterResult, OutputReduction};

/// Observed task-level cost counters for one filter toggle setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRunCost {
    /// Wall-clock milliseconds for the observed task attempt (host-supplied).
    pub wall_ms: u64,
    /// Optional cost in micros; None means unknown — never invent a price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    /// Extra full-output re-reads after a failure / truncation (host-supplied).
    pub failure_reread_count: u32,
}

/// Side-by-side record for filter enabled vs disabled on the same logical task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompareReport {
    pub task_fingerprint: String,
    pub with_filter: Arm,
    pub without_filter: Arm,
    /// Explicit disclaimer for consumers / UI copy.
    pub disclaimer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arm {
    pub reduction: OutputReduction,
    pub cost: TaskRunCost,
    pub original_digest: String,
    pub applied_profile: String,
}

const DISCLAIMER: &str = "output_reduction_is_not_task_benefit: compare wall_ms/cost_micros/failure_reread_count separately";

/// Build a compare report from two filter results of the same command capture family.
///
/// Requires matching `original_digest` so the comparison is for the same raw output.
pub fn compare_filter_toggle(
    task_fingerprint: impl Into<String>,
    with_filter: &FilterResult,
    with_cost: TaskRunCost,
    without_filter: &FilterResult,
    without_cost: TaskRunCost,
) -> Result<CompareReport, crate::types::OutputFilterError> {
    if with_filter.original_digest != without_filter.original_digest {
        return Err(crate::types::OutputFilterError::Invalid(
            "original_digest mismatch: compare requires the same raw capture".into(),
        ));
    }
    if !with_filter.options.enabled {
        return Err(crate::types::OutputFilterError::Invalid(
            "with_filter arm must have options.enabled = true".into(),
        ));
    }
    if without_filter.options.enabled {
        return Err(crate::types::OutputFilterError::Invalid(
            "without_filter arm must have options.enabled = false".into(),
        ));
    }
    Ok(CompareReport {
        task_fingerprint: task_fingerprint.into(),
        with_filter: Arm {
            reduction: with_filter.reduction.clone(),
            cost: with_cost,
            original_digest: with_filter.original_digest.clone(),
            applied_profile: with_filter.applied_profile.clone(),
        },
        without_filter: Arm {
            reduction: without_filter.reduction.clone(),
            cost: without_cost,
            original_digest: without_filter.original_digest.clone(),
            applied_profile: without_filter.applied_profile.clone(),
        },
        disclaimer: DISCLAIMER.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::apply_filter;
    use crate::types::FilterOptions;

    #[test]
    fn compare_same_digest() {
        let raw = "noise\nerror: x\n";
        let on = apply_filter(
            raw,
            FilterOptions {
                enabled: true,
                ..FilterOptions::default()
            },
        )
        .unwrap();
        let off = apply_filter(
            raw,
            FilterOptions {
                enabled: false,
                ..FilterOptions::default()
            },
        )
        .unwrap();
        let report = compare_filter_toggle(
            "task-1",
            &on,
            TaskRunCost {
                wall_ms: 100,
                cost_micros: None,
                failure_reread_count: 1,
            },
            &off,
            TaskRunCost {
                wall_ms: 90,
                cost_micros: Some(12),
                failure_reread_count: 0,
            },
        )
        .unwrap();
        assert!(report.disclaimer.contains("not_task_benefit"));
        assert!(report.with_filter.cost.cost_micros.is_none());
        assert_eq!(report.without_filter.cost.cost_micros, Some(12));
    }
}
