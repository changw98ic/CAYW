//! Local context assembly. This module does not call a model, delete logs,
//! or rewrite `outcome_unknown`.
//!
//! Token estimates are UTF-8 byte lengths (`utf8-bytes/1`). That is an upper
//! bound for scheduling only. Provider-reported usage stays on the model ledger.

use serde::{Deserialize, Serialize};

use crate::ContextError;

/// Used when the selected model plan has no `context_window`.
pub const CONSERVATIVE_CONTEXT_WINDOW: u64 = 8_192;
/// Protocol slack subtracted from the window together with the output reserve.
pub const DEFAULT_PROTOCOL_MARGIN: u64 = 1_024;
/// Output reserve used only when the model plan omits `max_output_tokens`.
pub const DEFAULT_OUTPUT_RESERVE: u64 = 1_024;
/// Completed turns at or below this estimate reuse their own text. No model call.
pub const SHORT_TURN_TOKENS: u64 = 256;
pub const ESTIMATOR_ID: &str = "utf8-bytes/1";

/// `log_id` text for one existing Core event. The cursor is the session cursor.
pub fn log_id(session_id: &str, cursor: u64) -> Result<String, ContextError> {
    if session_id.is_empty() || session_id.contains(':') || session_id.chars().any(char::is_control)
    {
        return Err(ContextError::Invalid(
            "log_id session_id must be nonempty and must not contain ':'".into(),
        ));
    }
    if cursor == 0 {
        return Err(ContextError::Invalid(
            "log_id cursor must be the existing nonzero session cursor".into(),
        ));
    }
    Ok(format!("{session_id}:{cursor}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenEstimate {
    pub tokens: u64,
    pub estimator: &'static str,
}

pub fn estimate_text(text: &str) -> TokenEstimate {
    TokenEstimate {
        tokens: u64::try_from(text.len()).unwrap_or(u64::MAX),
        estimator: ESTIMATOR_ID,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputBudget {
    pub window: u64,
    pub output_reserve: u64,
    pub margin: u64,
    pub input_limit: u64,
    pub replace_at: u64,
    pub compress_at: u64,
    pub window_was_default: bool,
}

/// `B = W - O - R`, and `T` is three quarters of `B` so ordinary appends still fit.
/// Compression starts at half of `T`, not half of the window.
pub fn input_budget(
    window: Option<u64>,
    output_reserve: u64,
    margin: u64,
) -> Result<InputBudget, ContextError> {
    if output_reserve == 0 {
        return Err(ContextError::Invalid(
            "output reserve must be positive".into(),
        ));
    }
    let window_was_default = window.is_none();
    let window = window.unwrap_or(CONSERVATIVE_CONTEXT_WINDOW);
    if !(1..=10_000_000).contains(&window) {
        return Err(ContextError::Invalid(
            "context window must be 1..=10000000".into(),
        ));
    }
    let input_limit = window.saturating_sub(output_reserve).saturating_sub(margin);
    if input_limit == 0 {
        return Err(ContextError::Invalid(
            "input budget is empty after output reserve and margin".into(),
        ));
    }
    let replace_at = (input_limit / 4 * 3).max(1);
    Ok(InputBudget {
        window,
        output_reserve,
        margin,
        input_limit,
        replace_at,
        compress_at: (replace_at / 2).max(1),
        window_was_default,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    /// System instructions and capability text. Never summarized or compressed.
    Fixed,
    CompletedTurn,
    ToolExchange,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUnit {
    pub start_cursor: u64,
    pub end_cursor: u64,
    pub kind: UnitKind,
    pub text: String,
    pub outcome_unknown: bool,
    /// Keep this unit's original text in the provider input.
    pub pin_original: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryKind {
    Segment,
    UserTurn,
    Stage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryRecord {
    pub summary_id: String,
    pub version: u64,
    pub session_id: String,
    pub kind: SummaryKind,
    pub start_cursor: u64,
    pub end_cursor: u64,
    pub text: String,
    pub evidence: Vec<String>,
    /// True when the text is the original turn, not a model summary.
    pub local_reuse: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionRecord {
    pub compression_id: String,
    pub version: u64,
    pub session_id: String,
    pub covered_through: u64,
    pub text: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSummary {
    pub start_cursor: u64,
    pub end_cursor: u64,
    pub input_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionJob {
    pub covered_through: u64,
    pub start_cursor: u64,
    pub input_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Fixed {
        start_cursor: u64,
        end_cursor: u64,
        text: String,
    },
    Original {
        start_cursor: u64,
        end_cursor: u64,
        text: String,
        outcome_unknown: bool,
        /// A saved tool body may be shortened. The live tail is never excerpted.
        excerpt_allowed: bool,
    },
    Compression {
        id: String,
        version: u64,
        start_cursor: u64,
        end_cursor: u64,
        text: String,
    },
    Summary {
        id: String,
        version: u64,
        start_cursor: u64,
        end_cursor: u64,
        text: String,
    },
}

impl Piece {
    pub fn range(&self) -> (u64, u64, &str, bool) {
        match self {
            Self::Fixed {
                start_cursor,
                end_cursor,
                text,
            }
            | Self::Compression {
                start_cursor,
                end_cursor,
                text,
                ..
            }
            | Self::Summary {
                start_cursor,
                end_cursor,
                text,
                ..
            } => (*start_cursor, *end_cursor, text, false),
            Self::Original {
                start_cursor,
                end_cursor,
                text,
                outcome_unknown,
                ..
            } => (*start_cursor, *end_cursor, text, *outcome_unknown),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembly {
    pub epoch: u64,
    pub pieces: Vec<Piece>,
    pub input_tokens: u64,
    pub sendable: bool,
    pub replaces_history: bool,
    pub model_calls: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub input_tokens: u64,
    pub new_summaries: Vec<SummaryRecord>,
    pub pending_summaries: Vec<PendingSummary>,
    pub compression_job: Option<CompressionJob>,
    pub assembly: Option<Assembly>,
    pub model_calls: u64,
}

pub struct ContextInput<'a> {
    pub session_id: &'a str,
    pub units: &'a [SourceUnit],
    pub summaries: &'a [SummaryRecord],
    pub compressions: &'a [CompressionRecord],
    pub budget: InputBudget,
    pub epoch: u64,
}

pub fn plan(input: &ContextInput<'_>) -> Result<Plan, ContextError> {
    validate_units(input.units)?;
    let input_tokens = total_tokens(input.units);
    let mut new_summaries = Vec::new();
    let mut pending_summaries = Vec::new();
    for unit in input.units {
        if unit.outcome_unknown
            || unit.pin_original
            || !matches!(unit.kind, UnitKind::CompletedTurn | UnitKind::ToolExchange)
        {
            continue;
        }
        if input.summaries.iter().any(|summary| {
            summary.start_cursor == unit.start_cursor && summary.end_cursor == unit.end_cursor
        }) {
            continue;
        }
        let tokens = estimate_text(&unit.text).tokens;
        if tokens <= SHORT_TURN_TOKENS {
            new_summaries.push(local_summary(input.session_id, unit)?);
        } else {
            pending_summaries.push(PendingSummary {
                start_cursor: unit.start_cursor,
                end_cursor: unit.end_cursor,
                input_bytes: estimate_text(&unit.text).tokens,
            });
        }
    }
    let cutoff = compressible_cutoff(input.units);
    let compression_job = if input_tokens >= input.budget.compress_at {
        cutoff.and_then(|cutoff| compression_job(input, cutoff))
    } else {
        None
    };
    let assembly = if input_tokens >= input.budget.replace_at {
        Some(assemble(input, &new_summaries)?)
    } else {
        None
    };
    Ok(Plan {
        input_tokens,
        new_summaries,
        pending_summaries,
        compression_job,
        assembly,
        model_calls: 0,
    })
}

/// A newer record is kept. An older or same version for the same range is stale
/// and must not overwrite the published one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionDecision {
    Accepted,
    Stale,
}

pub fn accept_summary(
    existing: &[SummaryRecord],
    incoming: &SummaryRecord,
) -> Result<VersionDecision, ContextError> {
    if incoming.version == 0
        || incoming.summary_id.is_empty()
        || incoming.start_cursor == 0
        || incoming.end_cursor < incoming.start_cursor
    {
        return Err(ContextError::Invalid(
            "summary version and cursor range must be positive".into(),
        ));
    }
    let stale = existing.iter().any(|item| {
        item.session_id == incoming.session_id
            && item.start_cursor == incoming.start_cursor
            && item.end_cursor == incoming.end_cursor
            && item.version >= incoming.version
    });
    Ok(if stale {
        VersionDecision::Stale
    } else {
        VersionDecision::Accepted
    })
}

pub fn accept_compression(
    existing: &[CompressionRecord],
    incoming: &CompressionRecord,
) -> Result<VersionDecision, ContextError> {
    if incoming.version == 0 || incoming.covered_through == 0 || incoming.text.is_empty() {
        return Err(ContextError::Invalid(
            "compression version, coverage, and text must be present".into(),
        ));
    }
    let stale = existing.iter().any(|item| {
        item.session_id == incoming.session_id
            && (item.version > incoming.version
                || (item.version == incoming.version
                    && item.covered_through >= incoming.covered_through))
    });
    Ok(if stale {
        VersionDecision::Stale
    } else {
        VersionDecision::Accepted
    })
}

/// Frontier moves only through a contiguous completed prefix. A later range
/// that finishes first does not skip the hole in front of it.
pub fn advance_frontier(current: u64, ready: &[(u64, u64)]) -> Result<u64, ContextError> {
    for (start, end) in ready {
        if *start == 0 || *end < *start {
            return Err(ContextError::Invalid(
                "frontier range is not a positive cursor span".into(),
            ));
        }
    }
    let mut covered = current;
    loop {
        let next = ready
            .iter()
            .find(|(start, _)| *start == covered.saturating_add(1));
        match next {
            Some((_, end)) => covered = *end,
            None => break,
        }
    }
    Ok(covered)
}

fn local_summary(session_id: &str, unit: &SourceUnit) -> Result<SummaryRecord, ContextError> {
    let mut evidence = vec![log_id(session_id, unit.start_cursor)?];
    if unit.end_cursor != unit.start_cursor {
        evidence.push(log_id(session_id, unit.end_cursor)?);
    }
    Ok(SummaryRecord {
        summary_id: format!(
            "local-{session_id}-{}-{}",
            unit.start_cursor, unit.end_cursor
        ),
        version: 1,
        session_id: session_id.to_owned(),
        kind: if unit.kind == UnitKind::ToolExchange {
            SummaryKind::Segment
        } else {
            SummaryKind::UserTurn
        },
        start_cursor: unit.start_cursor,
        end_cursor: unit.end_cursor,
        text: unit.text.clone(),
        evidence,
        local_reuse: true,
    })
}

fn compression_job(input: &ContextInput<'_>, cutoff: u64) -> Option<CompressionJob> {
    let prefix: Vec<&SourceUnit> = input
        .units
        .iter()
        .filter(|unit| replaceable(unit) && unit.end_cursor <= cutoff)
        .collect();
    if prefix.is_empty() {
        return None;
    }
    let previous = select_compression(input.compressions, cutoff);
    let mut chosen: Vec<&SourceUnit> = Vec::new();
    let mut bytes = previous
        .map(|record| estimate_text(&record.text).tokens)
        .unwrap_or(0);
    for unit in &prefix {
        if previous.is_some_and(|record| unit.end_cursor <= record.covered_through) {
            continue;
        }
        let next = bytes.saturating_add(estimate_text(&unit.text).tokens);
        if next > input.budget.input_limit {
            break;
        }
        bytes = next;
        chosen.push(unit);
    }
    let end = chosen.last()?.end_cursor;
    if previous.is_some_and(|record| record.covered_through == end) {
        return None;
    }
    Some(CompressionJob {
        covered_through: end,
        start_cursor: chosen.first()?.start_cursor,
        input_bytes: bytes,
    })
}

fn assemble(input: &ContextInput<'_>, extra: &[SummaryRecord]) -> Result<Assembly, ContextError> {
    let mut summaries: Vec<&SummaryRecord> = input.summaries.iter().collect();
    summaries.extend(extra.iter());
    let cutoff = compressible_cutoff(input.units);
    let compression = cutoff.and_then(|cutoff| select_compression(input.compressions, cutoff));
    let mut pieces = Vec::new();
    let mut index = 0;
    while index < input.units.len() && input.units[index].kind == UnitKind::Fixed {
        pieces.push(fixed_piece(&input.units[index]));
        index += 1;
    }
    let rest = &input.units[index..];
    let prefix_len = rest
        .iter()
        .position(|unit| !replaceable(unit) && unit.kind != UnitKind::Fixed)
        .unwrap_or(rest.len());
    let prefix = &rest[..prefix_len];
    let tail = &rest[prefix_len..];
    if let Some(record) = compression {
        if let Some(end_index) = prefix
            .iter()
            .rposition(|unit| replaceable(unit) && unit.end_cursor <= record.covered_through)
        {
            let first_index = prefix.iter().position(replaceable).unwrap_or(0);
            for unit in &prefix[..first_index] {
                pieces.push(fixed_piece(unit));
            }
            let start = prefix[first_index].start_cursor;
            let end = prefix[end_index].end_cursor;
            if start <= record.covered_through {
                pieces.push(Piece::Compression {
                    id: record.compression_id.clone(),
                    version: record.version,
                    start_cursor: start,
                    end_cursor: end,
                    text: record.text.clone(),
                });
                emit_after(
                    &mut pieces,
                    &prefix[end_index + 1..],
                    &summaries,
                    record.covered_through,
                );
            }
        } else {
            emit_after(&mut pieces, prefix, &summaries, 0);
        }
    } else {
        emit_after(&mut pieces, prefix, &summaries, 0);
    }
    for unit in tail {
        pieces.push(if unit.kind == UnitKind::Fixed {
            fixed_piece(unit)
        } else {
            original_piece(unit, false)
        });
    }
    check_cover(input.units, &pieces)?;
    let mut input_tokens = pieces
        .iter()
        .map(|piece| piece_tokens(input.units, piece))
        .fold(0_u64, u64::saturating_add);
    let mut sendable = input_tokens <= input.budget.input_limit;
    if !sendable {
        excerpt_large_originals(&mut pieces);
        input_tokens = pieces
            .iter()
            .map(|piece| piece_tokens(input.units, piece))
            .fold(0_u64, u64::saturating_add);
        sendable = input_tokens <= input.budget.input_limit;
    }
    let replaces_history = pieces.iter().any(|piece| match piece {
        Piece::Compression { .. } => true,
        Piece::Summary {
            text,
            start_cursor,
            end_cursor,
            ..
        }
        | Piece::Original {
            text,
            start_cursor,
            end_cursor,
            ..
        } => estimate_text(text).tokens < original_tokens(input.units, *start_cursor, *end_cursor),
        Piece::Fixed { .. } => false,
    });
    Ok(Assembly {
        epoch: input.epoch.saturating_add(1),
        pieces,
        input_tokens,
        sendable,
        replaces_history,
        model_calls: 0,
    })
}

fn emit_after(
    pieces: &mut Vec<Piece>,
    units: &[SourceUnit],
    summaries: &[&SummaryRecord],
    covered_through: u64,
) {
    let mut index = 0;
    while index < units.len() {
        let unit = &units[index];
        if unit.kind == UnitKind::Fixed {
            pieces.push(fixed_piece(unit));
            index += 1;
            continue;
        }
        if let Some(summary) = covering_summary(summaries, units, index, covered_through) {
            let end_index = units
                .iter()
                .position(|candidate| candidate.end_cursor == summary.end_cursor)
                .unwrap_or(index);
            pieces.push(Piece::Summary {
                id: summary.summary_id.clone(),
                version: summary.version,
                start_cursor: summary.start_cursor,
                end_cursor: summary.end_cursor,
                text: summary.text.clone(),
            });
            index = end_index + 1;
            continue;
        }
        pieces.push(original_piece(unit, unit.kind == UnitKind::ToolExchange));
        index += 1;
    }
}

fn covering_summary<'a>(
    summaries: &[&'a SummaryRecord],
    units: &[SourceUnit],
    index: usize,
    covered_through: u64,
) -> Option<&'a SummaryRecord> {
    let start = units[index].start_cursor;
    summaries
        .iter()
        .copied()
        .filter(|summary| {
            summary.start_cursor == start
                && summary.start_cursor > covered_through
                && summary.end_cursor >= start
                && units
                    .iter()
                    .any(|unit| unit.end_cursor == summary.end_cursor)
                && units
                    .iter()
                    .filter(|unit| {
                        unit.start_cursor >= start && unit.end_cursor <= summary.end_cursor
                    })
                    .all(replaceable)
                && !summary.local_reuse
                && estimate_text(&summary.text).tokens
                    < original_tokens(units, summary.start_cursor, summary.end_cursor)
                && !crosses(summary.start_cursor, summary.end_cursor, covered_through)
        })
        .max_by_key(|summary| summary.version)
}

fn crosses(start: u64, end: u64, covered_through: u64) -> bool {
    covered_through > 0 && start <= covered_through && end > covered_through
}

fn select_compression(records: &[CompressionRecord], cutoff: u64) -> Option<&CompressionRecord> {
    records
        .iter()
        .filter(|record| {
            !record.text.is_empty()
                && record.covered_through > 0
                && record.covered_through <= cutoff
                && record.version > 0
        })
        .max_by_key(|record| (record.version, record.covered_through))
}

fn replaceable(unit: &SourceUnit) -> bool {
    matches!(unit.kind, UnitKind::CompletedTurn | UnitKind::ToolExchange)
        && !unit.pin_original
        && !unit.outcome_unknown
}

fn compressible_cutoff(units: &[SourceUnit]) -> Option<u64> {
    let mut cutoff = None;
    for unit in units {
        if unit.kind == UnitKind::Fixed {
            continue;
        }
        if !replaceable(unit) {
            break;
        }
        cutoff = Some(unit.end_cursor);
    }
    cutoff
}

fn piece_tokens(units: &[SourceUnit], piece: &Piece) -> u64 {
    let base = estimate_text(piece.range().2).tokens;
    match piece {
        Piece::Compression {
            start_cursor,
            end_cursor,
            ..
        } => {
            let fixed = units
                .iter()
                .filter(|unit| {
                    unit.kind == UnitKind::Fixed
                        && unit.start_cursor >= *start_cursor
                        && unit.end_cursor <= *end_cursor
                })
                .map(|unit| estimate_text(&unit.text).tokens)
                .fold(0_u64, u64::saturating_add);
            base.saturating_add(fixed)
        }
        _ => base,
    }
}

fn fixed_piece(unit: &SourceUnit) -> Piece {
    Piece::Fixed {
        start_cursor: unit.start_cursor,
        end_cursor: unit.end_cursor,
        text: unit.text.clone(),
    }
}

fn original_piece(unit: &SourceUnit, excerpt_allowed: bool) -> Piece {
    Piece::Original {
        start_cursor: unit.start_cursor,
        end_cursor: unit.end_cursor,
        text: unit.text.clone(),
        outcome_unknown: unit.outcome_unknown,
        excerpt_allowed,
    }
}

fn original_tokens(units: &[SourceUnit], start: u64, end: u64) -> u64 {
    units
        .iter()
        .filter(|unit| unit.start_cursor >= start && unit.end_cursor <= end)
        .map(|unit| estimate_text(&unit.text).tokens)
        .fold(0_u64, u64::saturating_add)
}

fn excerpt_large_originals(pieces: &mut [Piece]) {
    for piece in pieces {
        let Piece::Original {
            text,
            outcome_unknown,
            excerpt_allowed,
            start_cursor,
            end_cursor,
        } = piece
        else {
            continue;
        };
        if !*excerpt_allowed || *outcome_unknown || text.len() <= 8 * 1024 {
            continue;
        }
        let excerpt: String = text.chars().take(512).collect();
        *text = format!(
            "{excerpt}\n[bounded excerpt of cursors {start_cursor}-{end_cursor}; original history was not deleted]"
        );
    }
}

fn check_cover(units: &[SourceUnit], pieces: &[Piece]) -> Result<(), ContextError> {
    if units.is_empty() {
        return Err(ContextError::Invalid(
            "context assembly needs source units".into(),
        ));
    }
    let mut unit_index = 0usize;
    for piece in pieces {
        let (start, end, _, unknown) = piece.range();
        let Some(first_unit) = units.get(unit_index) else {
            return Err(ContextError::Invalid(
                "assembly skipped or repeated a source range".into(),
            ));
        };
        if start != first_unit.start_cursor || end < start {
            return Err(ContextError::Invalid(
                "assembly skipped or repeated a source range".into(),
            ));
        }
        let Some(offset) = units[unit_index..]
            .iter()
            .position(|unit| unit.end_cursor == end)
        else {
            return Err(ContextError::Invalid(
                "assembly skipped or repeated a source range".into(),
            ));
        };
        let end_unit_index = unit_index + offset;
        if unknown {
            let original = units.iter().any(|unit| {
                unit.start_cursor == start && unit.end_cursor == end && unit.outcome_unknown
            });
            if !original {
                return Err(ContextError::Invalid(
                    "assembly changed an outcome_unknown range".into(),
                ));
            }
        }
        if matches!(piece, Piece::Compression { .. } | Piece::Summary { .. }) {
            let split_exchange = units.iter().any(|unit| {
                unit.kind == UnitKind::ToolExchange
                    && ((unit.start_cursor < start && unit.end_cursor >= start)
                        || (unit.start_cursor <= end && unit.end_cursor > end))
            });
            if split_exchange {
                return Err(ContextError::Invalid(
                    "assembly split a tool exchange".into(),
                ));
            }
        }
        unit_index = end_unit_index + 1;
    }
    if unit_index != units.len() {
        return Err(ContextError::Invalid(
            "assembly did not cover the source tail".into(),
        ));
    }
    Ok(())
}

fn total_tokens(units: &[SourceUnit]) -> u64 {
    units
        .iter()
        .map(|unit| estimate_text(&unit.text).tokens)
        .fold(0_u64, u64::saturating_add)
}

fn validate_units(units: &[SourceUnit]) -> Result<(), ContextError> {
    if units.is_empty() {
        return Err(ContextError::Invalid("context source is empty".into()));
    }
    let mut min_next = None;
    for unit in units {
        if unit.start_cursor == 0 || unit.end_cursor < unit.start_cursor {
            return Err(ContextError::Invalid(
                "source cursor range is invalid".into(),
            ));
        }
        if let Some(next) = min_next
            && unit.start_cursor < next
        {
            return Err(ContextError::Invalid(
                "source cursors must be strictly increasing".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn budget_for(window: u64) -> InputBudget {
        input_budget(Some(window), 1_000, 1_000).unwrap()
    }

    #[test]
    fn log_id_uses_session_and_cursor() {
        assert_eq!(log_id("session", 4).unwrap(), "session:4");
        assert!(log_id("", 1).is_err());
        assert!(log_id("a:b", 1).is_err());
        assert!(log_id("session", 0).is_err());
    }

    #[test]
    fn missing_window_uses_the_conservative_default() {
        let budget = input_budget(None, DEFAULT_OUTPUT_RESERVE, DEFAULT_PROTOCOL_MARGIN).unwrap();
        assert!(budget.window_was_default);
        assert_eq!(budget.window, CONSERVATIVE_CONTEXT_WINDOW);
        assert!(budget.replace_at <= budget.input_limit);
        assert_eq!(budget.compress_at, budget.replace_at / 2);
    }

    #[test]
    fn completed_turn_is_summarized_before_the_compression_point() {
        let units = vec![
            unit(1, 2, UnitKind::CompletedTurn, "short turn", false, false),
            unit(3, 3, UnitKind::Active, "current", true, false),
        ];
        let budget = budget_for(100_000);
        let plan = plan(&ContextInput {
            session_id: "s",
            units: &units,
            summaries: &[],
            compressions: &[],
            budget,
            epoch: 1,
        })
        .unwrap();
        assert!(plan.input_tokens < budget.compress_at);
        assert!(plan.compression_job.is_none());
        assert!(plan.assembly.is_none());
        assert_eq!(plan.model_calls, 0);
        assert_eq!(plan.new_summaries.len(), 1);
        assert!(plan.new_summaries[0].local_reuse);
        assert_eq!(plan.new_summaries[0].text, "short turn");
        assert_eq!(plan.new_summaries[0].evidence, vec!["s:1", "s:2"]);
        assert_eq!(units[0].text, "short turn");
    }

    #[test]
    fn half_threshold_queues_compression_without_assembling() {
        let early = "e".repeat(8_000);
        let units = vec![
            unit(1, 2, UnitKind::CompletedTurn, &early, false, false),
            unit(3, 3, UnitKind::Active, "tail", true, false),
        ];
        let budget = input_budget(Some(20_000), 1_000, 1_000).unwrap();
        let planned = plan(&ContextInput {
            session_id: "s",
            units: &units,
            summaries: &[],
            compressions: &[],
            budget,
            epoch: 2,
        })
        .unwrap();
        assert!(planned.input_tokens >= budget.compress_at);
        assert!(planned.input_tokens < budget.replace_at);
        assert!(planned.assembly.is_none());
        assert_eq!(planned.compression_job.unwrap().covered_through, 2);
        assert_eq!(planned.model_calls, 0);
    }

    #[test]
    fn replacement_uses_compression_summary_and_uncovered_original_without_a_model_call() {
        let units = vec![
            unit(1, 1, UnitKind::Fixed, "system rules", false, false),
            unit(2, 4, UnitKind::CompletedTurn, "EARLY", false, false),
            unit(5, 7, UnitKind::CompletedTurn, "MIDDLE", false, false),
            unit(8, 9, UnitKind::CompletedTurn, "LATE", false, false),
            unit(10, 10, UnitKind::Active, "CURRENT", true, false),
        ];
        let summaries = vec![SummaryRecord {
            summary_id: "sum-middle".into(),
            version: 1,
            session_id: "s".into(),
            kind: SummaryKind::UserTurn,
            start_cursor: 5,
            end_cursor: 7,
            text: "mid".into(),
            evidence: vec!["s:5".into()],
            local_reuse: false,
        }];
        let compressions = vec![CompressionRecord {
            compression_id: "c1".into(),
            version: 1,
            session_id: "s".into(),
            covered_through: 4,
            text: "compressed early".into(),
            evidence: vec!["s:2".into()],
        }];
        let budget = input_budget(Some(40), 1, 1).unwrap();
        let planned = plan(&ContextInput {
            session_id: "s",
            units: &units,
            summaries: &summaries,
            compressions: &compressions,
            budget,
            epoch: 4,
        })
        .unwrap();
        let assembly = planned.assembly.expect("threshold reached");
        assert_eq!(assembly.model_calls, 0);
        assert_eq!(planned.model_calls, 0);
        let texts: Vec<&str> = assembly
            .pieces
            .iter()
            .map(|piece| piece.range().2)
            .collect();
        assert_eq!(
            texts,
            vec!["system rules", "compressed early", "mid", "LATE", "CURRENT"]
        );
        assert!(!texts.contains(&"EARLY"));
        assert_eq!(texts.iter().filter(|text| **text == "LATE").count(), 1);
        assert_eq!(units[1].text, "EARLY");
    }

    #[test]
    fn summary_crossing_the_cutoff_is_not_pasted_whole() {
        let units = vec![
            unit(1, 3, UnitKind::CompletedTurn, "early", false, false),
            unit(4, 6, UnitKind::CompletedTurn, "after", false, false),
            unit(7, 7, UnitKind::Active, "tail", true, false),
        ];
        let summaries = vec![SummaryRecord {
            summary_id: "cross".into(),
            version: 1,
            session_id: "s".into(),
            kind: SummaryKind::Stage,
            start_cursor: 1,
            end_cursor: 6,
            text: "whole stage".into(),
            evidence: vec!["s:1".into()],
            local_reuse: false,
        }];
        let compressions = vec![CompressionRecord {
            compression_id: "c".into(),
            version: 2,
            session_id: "s".into(),
            covered_through: 3,
            text: "compressed".into(),
            evidence: vec![],
        }];
        let budget = input_budget(Some(16), 1, 1).unwrap();
        let assembly = plan(&ContextInput {
            session_id: "s",
            units: &units,
            summaries: &summaries,
            compressions: &compressions,
            budget,
            epoch: 1,
        })
        .unwrap()
        .assembly
        .unwrap();
        let texts: Vec<&str> = assembly
            .pieces
            .iter()
            .map(|piece| piece.range().2)
            .collect();
        assert_eq!(texts, vec!["compressed", "after", "tail"]);
    }

    #[test]
    fn unknown_tool_exchange_stays_intact() {
        let units = vec![
            unit(1, 2, UnitKind::CompletedTurn, "done", false, false),
            unit(
                3,
                5,
                UnitKind::ToolExchange,
                "call and unknown result",
                true,
                true,
            ),
        ];
        let budget = input_budget(Some(30), 1, 1).unwrap();
        let assembly = plan(&ContextInput {
            session_id: "s",
            units: &units,
            summaries: &[],
            compressions: &[CompressionRecord {
                compression_id: "c".into(),
                version: 1,
                session_id: "s".into(),
                covered_through: 5,
                text: "should not swallow unknown".into(),
                evidence: vec![],
            }],
            budget,
            epoch: 1,
        })
        .unwrap()
        .assembly
        .unwrap();
        match assembly.pieces.last().unwrap() {
            Piece::Original {
                text,
                outcome_unknown,
                start_cursor,
                end_cursor,
                ..
            } => {
                assert_eq!(text, "call and unknown result");
                assert!(*outcome_unknown);
                assert_eq!((*start_cursor, *end_cursor), (3, 5));
            }
            other => panic!("unknown exchange was rewritten: {other:?}"),
        }
    }

    #[test]
    fn later_range_does_not_skip_the_frontier() {
        assert_eq!(advance_frontier(0, &[(2, 4), (1, 1)]).unwrap(), 4);
        assert_eq!(advance_frontier(0, &[(2, 4)]).unwrap(), 0);
        assert_eq!(advance_frontier(1, &[(4, 9)]).unwrap(), 1);
        assert_eq!(advance_frontier(1, &[(4, 9), (2, 3)]).unwrap(), 9);
        assert_eq!(advance_frontier(3, &[(4, 9)]).unwrap(), 9);
        assert_eq!(advance_frontier(0, &[(4, 9)]).unwrap(), 0);
    }

    #[test]
    fn oversized_input_is_not_sendable_and_does_not_drop_the_source() {
        let huge = "x".repeat(9_000);
        let units = vec![unit(1, 1, UnitKind::Active, &huge, true, false)];
        let original = units.clone();
        let budget = input_budget(Some(2_000), 100, 100).unwrap();
        let assembly = plan(&ContextInput {
            session_id: "s",
            units: &units,
            summaries: &[],
            compressions: &[],
            budget,
            epoch: 1,
        })
        .unwrap()
        .assembly
        .unwrap();
        assert!(!assembly.sendable);
        assert_eq!(assembly.model_calls, 0);
        assert_eq!(units, original);
        assert_eq!(assembly.pieces[0].range().2, huge);
    }

    #[test]
    fn large_saved_tool_body_is_excerpted_without_deleting_the_source() {
        let body = "y".repeat(9_000);
        let units = vec![
            unit(1, 2, UnitKind::ToolExchange, &body, false, false),
            unit(3, 3, UnitKind::Active, "tail", true, false),
        ];
        let original = units.clone();
        let budget = input_budget(Some(2_000), 100, 100).unwrap();
        let assembly = plan(&ContextInput {
            session_id: "s",
            units: &units,
            summaries: &[],
            compressions: &[],
            budget,
            epoch: 1,
        })
        .unwrap()
        .assembly
        .unwrap();
        assert!(assembly.sendable);
        assert!(assembly.replaces_history);
        assert!(
            assembly.pieces[0]
                .range()
                .2
                .contains("original history was not deleted")
        );
        assert!(assembly.pieces[0].range().2.len() < body.len());
        assert_eq!(assembly.pieces[1].range().2, "tail");
        assert_eq!(units, original);
    }

    #[test]
    fn older_summary_and_compression_do_not_overwrite_a_newer_version() {
        let current = SummaryRecord {
            summary_id: "sum".into(),
            version: 2,
            session_id: "s".into(),
            kind: SummaryKind::UserTurn,
            start_cursor: 1,
            end_cursor: 2,
            text: "newer".into(),
            evidence: vec!["s:1".into()],
            local_reuse: false,
        };
        let older = SummaryRecord {
            version: 1,
            text: "older".into(),
            ..current.clone()
        };
        assert_eq!(
            accept_summary(std::slice::from_ref(&current), &older).unwrap(),
            VersionDecision::Stale
        );
        let newer = SummaryRecord {
            version: 3,
            ..current.clone()
        };
        assert_eq!(
            accept_summary(std::slice::from_ref(&current), &newer).unwrap(),
            VersionDecision::Accepted
        );
        let published = CompressionRecord {
            compression_id: "c".into(),
            version: 2,
            session_id: "s".into(),
            covered_through: 4,
            text: "newer compression".into(),
            evidence: vec![],
        };
        let late = CompressionRecord {
            version: 1,
            text: "late".into(),
            ..published.clone()
        };
        assert_eq!(
            accept_compression(std::slice::from_ref(&published), &late).unwrap(),
            VersionDecision::Stale
        );
        let newer_compression = CompressionRecord {
            version: 3,
            covered_through: 2,
            text: "shorter coverage".into(),
            ..late
        };
        assert_eq!(
            accept_compression(std::slice::from_ref(&published), &newer_compression).unwrap(),
            VersionDecision::Accepted
        );
    }

    #[test]
    fn mid_transcript_fixed_declarations_allow_later_compression_summary_and_excerpt() {
        let large_tool = "t".repeat(9_000);
        let units = vec![
            unit(1, 1, UnitKind::Fixed, "system", false, false),
            unit(2, 2, UnitKind::CompletedTurn, "first prompt", false, false),
            unit(3, 3, UnitKind::Fixed, "", false, false),
            unit(
                4,
                5,
                UnitKind::ToolExchange,
                "first execute_code exchange",
                false,
                false,
            ),
            unit(
                6,
                7,
                UnitKind::CompletedTurn,
                "SECOND LONG TURN TO SUMMARIZE",
                false,
                false,
            ),
            unit(8, 9, UnitKind::ToolExchange, &large_tool, false, false),
            unit(10, 10, UnitKind::Active, "follow-up", true, false),
        ];
        let summaries = vec![SummaryRecord {
            summary_id: "sum-6-7".into(),
            version: 1,
            session_id: "s".into(),
            kind: SummaryKind::UserTurn,
            start_cursor: 6,
            end_cursor: 7,
            text: "sum67".into(),
            evidence: vec!["s:6".into()],
            local_reuse: false,
        }];
        let compressions = vec![CompressionRecord {
            compression_id: "comp-5".into(),
            version: 1,
            session_id: "s".into(),
            covered_through: 5,
            text: "comp25".into(),
            evidence: vec!["s:2".into(), "s:5".into()],
        }];
        let budget = input_budget(Some(2_500), 100, 100).unwrap();
        let planned = plan(&ContextInput {
            session_id: "s",
            units: &units,
            summaries: &summaries,
            compressions: &compressions,
            budget,
            epoch: 1,
        })
        .unwrap();
        assert_eq!(planned.compression_job.unwrap().covered_through, 7);
        let assembly = planned.assembly.expect("threshold reached");
        assert!(assembly.sendable);
        assert!(assembly.replaces_history);
        assert!(matches!(
            &assembly.pieces[1],
            Piece::Compression {
                start_cursor: 2,
                end_cursor: 5,
                ..
            }
        ));
        assert!(matches!(
            &assembly.pieces[2],
            Piece::Summary {
                start_cursor: 6,
                end_cursor: 7,
                ..
            }
        ));
        assert!(
            assembly.pieces[3]
                .range()
                .2
                .contains("original history was not deleted")
        );
    }

    #[test]
    fn pinned_tail_is_not_queued_as_pending_summary() {
        let pinned_body = "z".repeat(600);
        let units = vec![
            unit(1, 2, UnitKind::CompletedTurn, "short", false, false),
            unit(3, 4, UnitKind::ToolExchange, &pinned_body, true, false),
        ];
        let budget = budget_for(50_000);
        let planned = plan(&ContextInput {
            session_id: "s",
            units: &units,
            summaries: &[],
            compressions: &[],
            budget,
            epoch: 1,
        })
        .unwrap();
        assert!(planned.pending_summaries.is_empty());
    }
}
