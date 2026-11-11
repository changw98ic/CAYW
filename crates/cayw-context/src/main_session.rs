//! Pure planning for the main session's context.
//!
//! The main session does not run cumulative compression. At the replacement
//! threshold it keeps durable memory, user preferences, the current request and
//! the active work state, and evicts older raw turns. An evicted turn remains in
//! the durable log and in any summary index; this module never deletes or
//! rewrites either.
//!
//! Forgotten source ranges are skipped before they enter provider input. The
//! original log is untouched; callers can still read it by cursor.

use serde::{Deserialize, Serialize};

use crate::ContextError;
use crate::assembly::{InputBudget, SourceUnit, SummaryRecord, UnitKind, estimate_text};

/// One inclusive cursor span in a single session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRange {
    pub session_id: String,
    pub start_cursor: u64,
    pub end_cursor: u64,
}

impl SourceRange {
    pub fn new(
        session_id: impl Into<String>,
        start_cursor: u64,
        end_cursor: u64,
    ) -> Result<Self, ContextError> {
        let range = Self {
            session_id: session_id.into(),
            start_cursor,
            end_cursor,
        };
        range.validate()?;
        Ok(range)
    }

    pub fn validate(&self) -> Result<(), ContextError> {
        if self.session_id.is_empty()
            || self.session_id.contains(':')
            || self.session_id.chars().any(char::is_control)
        {
            return Err(ContextError::Invalid(
                "forget range needs a session id without ':'".into(),
            ));
        }
        if self.start_cursor == 0 || self.end_cursor < self.start_cursor {
            return Err(ContextError::Invalid(
                "forget range needs a positive cursor span".into(),
            ));
        }
        Ok(())
    }

    pub fn covers_cursor(&self, session_id: &str, cursor: u64) -> bool {
        self.session_id == session_id && cursor >= self.start_cursor && cursor <= self.end_cursor
    }

    pub fn covers(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && other.start_cursor >= self.start_cursor
            && other.end_cursor <= self.end_cursor
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.start_cursor <= other.end_cursor
            && other.start_cursor <= self.end_cursor
    }
}

/// A cursor-only span, used while planning one session's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CursorRange {
    pub start_cursor: u64,
    pub end_cursor: u64,
}

impl CursorRange {
    pub fn new(start_cursor: u64, end_cursor: u64) -> Result<Self, ContextError> {
        if start_cursor == 0 || end_cursor < start_cursor {
            return Err(ContextError::Invalid(
                "cursor range needs a positive span".into(),
            ));
        }
        Ok(Self {
            start_cursor,
            end_cursor,
        })
    }

    pub fn covers_turn(&self, unit: &SourceUnit) -> bool {
        unit.start_cursor >= self.start_cursor && unit.end_cursor <= self.end_cursor
    }
}

/// The result of planning one main-session provider input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainSessionPlan {
    pub input_tokens: u64,
    /// Unit ranges that remain in provider input, in order.
    pub retained_ranges: Vec<CursorRange>,
    /// Older raw turns removed from provider input. They stay in the log.
    pub evicted_ranges: Vec<CursorRange>,
    /// Forget ranges that applied to a complete replaceable turn or exchange.
    pub skipped_ranges: Vec<CursorRange>,
    /// Forget ranges that overlap a live or unpaired unit. The unit stays paired.
    pub unresolved_ranges: Vec<CursorRange>,
    /// The main session never compresses or calls a model while assembling.
    pub model_calls: u64,
    pub compression_requests: u64,
    pub sendable: bool,
}

pub struct MainSessionInput<'a> {
    pub units: &'a [SourceUnit],
    /// Cursor spans already forgotten for this session.
    pub forgotten: &'a [CursorRange],
    /// Estimated tokens of durable memory, preferences and active work state.
    pub pinned_tokens: u64,
    pub budget: InputBudget,
}

/// Retain durable memory, preferences, the current request and active work.
/// Evict the oldest replaceable raw turns when the input reaches the
/// replacement threshold. Never split or drop an unpaired tool exchange or an
/// `outcome_unknown` result.
pub fn plan_main_session(input: &MainSessionInput<'_>) -> Result<MainSessionPlan, ContextError> {
    validate_units(input.units)?;
    for range in input.forgotten {
        if range.start_cursor == 0 || range.end_cursor < range.start_cursor {
            return Err(ContextError::Invalid(
                "forget range needs a positive cursor span".into(),
            ));
        }
    }

    let mut keep = vec![true; input.units.len()];
    let mut skipped_ranges = Vec::new();
    let mut unresolved_ranges = Vec::new();
    let mut kept_tokens = input
        .pinned_tokens
        .saturating_add(total_tokens(input.units));

    // Forgotten spans leave provider input before any threshold applies. A span
    // that only partly covers a live or unpaired unit cannot be removed without
    // breaking pairing, so it is reported and the unit stays whole.
    for range in input.forgotten {
        let mut removed_any = false;
        for (index, unit) in input.units.iter().enumerate() {
            if !keep[index] || !overlaps(unit, range) {
                continue;
            }
            if replaceable(unit) {
                keep[index] = false;
                kept_tokens = kept_tokens.saturating_sub(estimate_text(&unit.text).tokens);
                removed_any = true;
            }
        }
        let overlaps_kept = input
            .units
            .iter()
            .enumerate()
            .any(|(index, unit)| keep[index] && overlaps(unit, range));
        if overlaps_kept {
            unresolved_ranges.push(*range);
        }
        if removed_any {
            skipped_ranges.push(*range);
        }
    }

    let mut evicted_ranges = Vec::new();
    if kept_tokens >= input.budget.replace_at {
        for (index, unit) in input.units.iter().enumerate() {
            if kept_tokens <= input.budget.replace_at {
                break;
            }
            if !keep[index] || !replaceable(unit) {
                continue;
            }
            // Keep the tail (current request and active work) even when the
            // pinned part alone already exceeds the threshold.
            if index + 1 == input.units.len() {
                break;
            }
            keep[index] = false;
            kept_tokens = kept_tokens.saturating_sub(estimate_text(&unit.text).tokens);
            evicted_ranges.push(CursorRange {
                start_cursor: unit.start_cursor,
                end_cursor: unit.end_cursor,
            });
        }
    }

    let retained_ranges = input
        .units
        .iter()
        .zip(&keep)
        .filter(|(_, keep)| **keep)
        .map(|(unit, _)| CursorRange {
            start_cursor: unit.start_cursor,
            end_cursor: unit.end_cursor,
        })
        .collect::<Vec<_>>();
    if retained_ranges.is_empty() {
        return Err(ContextError::Invalid(
            "main session input retained no source units".into(),
        ));
    }
    Ok(MainSessionPlan {
        input_tokens: kept_tokens,
        retained_ranges,
        evicted_ranges,
        skipped_ranges,
        unresolved_ranges,
        model_calls: 0,
        compression_requests: 0,
        sendable: kept_tokens <= input.budget.input_limit,
    })
}

fn validate_units(units: &[SourceUnit]) -> Result<(), ContextError> {
    if units.is_empty() {
        return Err(ContextError::Invalid(
            "main session context needs source units".into(),
        ));
    }
    let mut min_next = None;
    for unit in units {
        if unit.start_cursor == 0 || unit.end_cursor < unit.start_cursor {
            return Err(ContextError::Invalid(
                "main session source range is invalid".into(),
            ));
        }
        if let Some(next) = min_next
            && unit.start_cursor < next
        {
            return Err(ContextError::Invalid(
                "main session source cursors must be strictly increasing".into(),
            ));
        }
        if unit.outcome_unknown
            && unit.kind != UnitKind::ToolExchange
            && unit.kind != UnitKind::Active
        {
            return Err(ContextError::Invalid(
                "outcome_unknown only belongs on a tool exchange".into(),
            ));
        }
        min_next = Some(unit.end_cursor.saturating_add(1));
    }
    Ok(())
}

fn replaceable(unit: &SourceUnit) -> bool {
    matches!(unit.kind, UnitKind::CompletedTurn | UnitKind::ToolExchange)
        && !unit.pin_original
        && !unit.outcome_unknown
}

fn overlaps(unit: &SourceUnit, range: &CursorRange) -> bool {
    unit.start_cursor <= range.end_cursor && range.start_cursor <= unit.end_cursor
}

fn total_tokens(units: &[SourceUnit]) -> u64 {
    units
        .iter()
        .map(|unit| estimate_text(&unit.text).tokens)
        .fold(0_u64, u64::saturating_add)
}

/// One reference inside a Dream plan. Dream reorganizes references only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamReference {
    pub summary_id: String,
    pub version: u64,
    /// Deduplicated `source_log_ids`, original order preserved.
    pub source_log_ids: Vec<String>,
    /// Set when duplicate direct references sink into a contained summary.
    pub child_of: Option<String>,
}

/// The result of organizing existing summary references.
///
/// `body` stays `None` unless a model supplied text. Without a model this
/// module never invents a Dream body and never rewrites a summary or a log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamPlan {
    pub references: Vec<DreamReference>,
    /// Summaries fully inside a forgotten range. They stay on disk, not loaded.
    pub skipped: Vec<String>,
    pub superseded: Vec<String>,
    pub body: Option<String>,
    pub model_calls: u64,
}

/// Organize existing summary references for Dream.
///
/// A summary fully covered by a forgotten range is not loaded. Evidence is
/// deduplicated. When one summary's references repeat a contained summary's
/// references, the upper summary points at the contained one instead of
/// repeating the direct reference list. Only an explicit model text becomes a
/// Dream body.
pub fn organize_dream(
    summaries: &[SummaryRecord],
    forgotten: &[SourceRange],
    model_text: Option<&str>,
) -> DreamPlan {
    let mut skipped = Vec::new();
    let mut references = Vec::new();
    for summary in summaries {
        let range = SourceRange {
            session_id: summary.session_id.clone(),
            start_cursor: summary.start_cursor,
            end_cursor: summary.end_cursor,
        };
        if forgotten.iter().any(|forgotten| forgotten.covers(&range)) {
            skipped.push(summary.summary_id.clone());
            continue;
        }
        let mut source_log_ids = Vec::new();
        for id in &summary.evidence {
            if !source_log_ids.contains(id) {
                source_log_ids.push(id.clone());
            }
        }
        references.push(DreamReference {
            summary_id: summary.summary_id.clone(),
            version: summary.version,
            source_log_ids,
            child_of: None,
        });
    }
    // Sink duplicate direct references into a contained summary. This keeps the
    // path to the original events and makes the sink reversible.
    let sink = references
        .iter()
        .map(|reference| {
            (
                reference.summary_id.clone(),
                reference.source_log_ids.clone(),
            )
        })
        .collect::<Vec<_>>();
    for reference in &mut references {
        let Some(summary) = summaries
            .iter()
            .find(|summary| summary.summary_id == reference.summary_id)
        else {
            continue;
        };
        let contained = summaries.iter().find(|candidate| {
            candidate.summary_id != summary.summary_id
                && candidate.session_id == summary.session_id
                && candidate.start_cursor >= summary.start_cursor
                && candidate.end_cursor <= summary.end_cursor
                && (candidate.start_cursor > summary.start_cursor
                    || candidate.end_cursor < summary.end_cursor)
                && sink.iter().any(|(id, evidence)| {
                    *id == candidate.summary_id
                        && !evidence.is_empty()
                        && same_set(evidence, &reference.source_log_ids)
                })
        });
        if let Some(contained) = contained {
            reference.child_of = Some(contained.summary_id.clone());
            // The direct duplicate list now lives in the contained summary.
            reference.source_log_ids =
                vec![format!("{}:{}", contained.session_id, contained.summary_id)];
        }
    }
    DreamPlan {
        references,
        skipped,
        superseded: Vec::new(),
        body: model_text.map(str::to_owned),
        model_calls: 0,
    }
}

fn same_set(left: &[String], right: &[String]) -> bool {
    left.len() == right.len() && left.iter().all(|item| right.contains(item))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assembly::input_budget;

    fn unit(
        start: u64,
        end: u64,
        kind: UnitKind,
        text: &str,
        pin: bool,
        unknown: bool,
    ) -> SourceUnit {
        SourceUnit {
            start_cursor: start,
            end_cursor: end,
            kind,
            text: text.to_owned(),
            outcome_unknown: unknown,
            pin_original: pin,
        }
    }

    #[test]
    fn source_range_covers_and_validates() {
        let range = SourceRange::new("s", 3, 5).unwrap();
        assert!(range.covers_cursor("s", 3));
        assert!(range.covers_cursor("s", 5));
        assert!(!range.covers_cursor("s", 6));
        assert!(!range.covers_cursor("other", 4));
        assert!(SourceRange::new("s", 0, 1).is_err());
        assert!(SourceRange::new("s", 5, 3).is_err());
        assert!(SourceRange::new("a:b", 1, 1).is_err());
    }

    #[test]
    fn below_threshold_keeps_every_turn_and_never_compresses() {
        let units = vec![
            unit(1, 1, UnitKind::Fixed, "rules", false, false),
            unit(2, 3, UnitKind::CompletedTurn, "old", false, false),
            unit(4, 4, UnitKind::Active, "now", true, false),
        ];
        let budget = input_budget(Some(100_000), 1_000, 1_000).unwrap();
        let plan = plan_main_session(&MainSessionInput {
            units: &units,
            forgotten: &[],
            pinned_tokens: 10,
            budget,
        })
        .unwrap();
        assert_eq!(plan.retained_ranges.len(), 3);
        assert!(plan.evicted_ranges.is_empty());
        assert_eq!(plan.model_calls, 0);
        assert_eq!(plan.compression_requests, 0);
    }

    #[test]
    fn threshold_evicts_old_turns_but_keeps_memory_request_and_active_work() {
        let units = vec![
            unit(1, 1, UnitKind::Fixed, "rules", false, false),
            unit(
                2,
                3,
                UnitKind::CompletedTurn,
                &"a".repeat(400),
                false,
                false,
            ),
            unit(
                4,
                5,
                UnitKind::CompletedTurn,
                &"b".repeat(400),
                false,
                false,
            ),
            unit(6, 6, UnitKind::Active, "current request", true, false),
        ];
        // pinned tokens stand in for durable memory, preferences and active work.
        let pinned = 500;
        let budget = input_budget(Some(1_400), 100, 100).unwrap();
        let plan = plan_main_session(&MainSessionInput {
            units: &units,
            forgotten: &[],
            pinned_tokens: pinned,
            budget,
        })
        .unwrap();
        assert!(plan.input_tokens <= budget.replace_at);
        assert!(!plan.retained_ranges.is_empty());
        assert_eq!(
            plan.retained_ranges.last().unwrap(),
            &CursorRange {
                start_cursor: 6,
                end_cursor: 6
            }
        );
        assert!(
            plan.retained_ranges
                .iter()
                .any(|range| range.start_cursor == 1)
        );
        assert!(!plan.evicted_ranges.is_empty());
        assert_eq!(plan.compression_requests, 0);
    }

    #[test]
    fn unknown_and_unpaired_tool_exchanges_are_never_evicted() {
        let units = vec![
            unit(1, 2, UnitKind::ToolExchange, &"u".repeat(2_000), true, true),
            unit(
                3,
                4,
                UnitKind::ToolExchange,
                &"p".repeat(2_000),
                true,
                false,
            ),
            unit(5, 5, UnitKind::Active, "now", true, false),
        ];
        let budget = input_budget(Some(600), 10, 10).unwrap();
        let plan = plan_main_session(&MainSessionInput {
            units: &units,
            forgotten: &[],
            pinned_tokens: 0,
            budget,
        })
        .unwrap();
        assert!(plan.evicted_ranges.is_empty());
        assert_eq!(plan.retained_ranges.len(), 3);
        assert!(!plan.sendable);
    }

    #[test]
    fn forgotten_turns_leave_input_and_tail_stays_paired() {
        let units = vec![
            unit(
                1,
                2,
                UnitKind::CompletedTurn,
                "forgotten early",
                false,
                false,
            ),
            unit(3, 3, UnitKind::ToolExchange, "live call", true, true),
            unit(4, 4, UnitKind::Active, "now", true, false),
        ];
        let budget = input_budget(Some(100_000), 1_000, 1_000).unwrap();
        let plan = plan_main_session(&MainSessionInput {
            units: &units,
            forgotten: &[
                CursorRange::new(1, 2).unwrap(),
                CursorRange::new(3, 3).unwrap(),
            ],
            pinned_tokens: 0,
            budget,
        })
        .unwrap();
        assert_eq!(plan.retained_ranges.len(), 2);
        assert_eq!(plan.skipped_ranges, vec![CursorRange::new(1, 2).unwrap()]);
        assert_eq!(
            plan.unresolved_ranges,
            vec![CursorRange::new(3, 3).unwrap()]
        );
        assert!(
            plan.retained_ranges
                .iter()
                .any(|range| range.start_cursor == 3)
        );
    }

    #[test]
    fn dream_deduplicates_and_sinks_references_without_rewriting() {
        let summaries = vec![
            SummaryRecord {
                summary_id: "upper".into(),
                version: 1,
                session_id: "s".into(),
                kind: crate::assembly::SummaryKind::Stage,
                start_cursor: 1,
                end_cursor: 8,
                text: "upper text".into(),
                evidence: vec!["s:1".into(), "s:2".into(), "s:1".into()],
                local_reuse: false,
            },
            SummaryRecord {
                summary_id: "inner".into(),
                version: 1,
                session_id: "s".into(),
                kind: crate::assembly::SummaryKind::Segment,
                start_cursor: 3,
                end_cursor: 5,
                text: "inner text".into(),
                evidence: vec!["s:2".into(), "s:1".into()],
                local_reuse: false,
            },
        ];
        let before = summaries.clone();
        let dream = organize_dream(&summaries, &[], None);
        assert_eq!(dream.body, None);
        assert_eq!(dream.model_calls, 0);
        assert_eq!(summaries, before);
        let upper = dream
            .references
            .iter()
            .find(|reference| reference.summary_id == "upper")
            .unwrap();
        assert_eq!(upper.source_log_ids, vec!["s:inner".to_string()]);
        assert_eq!(upper.child_of.as_deref(), Some("inner"));
    }

    #[test]
    fn dream_skips_published_summaries_inside_forgotten_ranges() {
        let summaries = vec![SummaryRecord {
            summary_id: "old".into(),
            version: 1,
            session_id: "s".into(),
            kind: crate::assembly::SummaryKind::Segment,
            start_cursor: 1,
            end_cursor: 4,
            text: "published".into(),
            evidence: vec!["s:1".into()],
            local_reuse: false,
        }];
        let dream = organize_dream(
            &summaries,
            &[SourceRange::new("s", 1, 4).unwrap()],
            Some("model text"),
        );
        assert_eq!(dream.skipped, vec!["old".to_string()]);
        assert!(dream.references.is_empty());
        assert_eq!(dream.body.as_deref(), Some("model text"));
    }
}
