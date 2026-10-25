//! Per-session and cross-session overview builders.

use serde::Serialize;

use crate::evolution::{EvolutionStep, build_evolution};
use crate::tokens::{TokenTotals, aggregate_tokens, cache_hit_rate};
use crate::types::*;

/// One session's context / usage overview card.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOverview {
    pub session_id: String,
    pub budget_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub request_count: u64,
    pub turn_count: u64,
    pub tokens: TokenTotals,
    pub ledger_settled_tokens: u64,
    pub ledger_reserved_tokens: u64,
    pub ledger_remaining_tokens: u64,
    pub budget_unknown: bool,
    pub cost: CostView,
    pub cache: CacheView,
    pub composition: Vec<CompositionPart>,
    pub evolution: Vec<EvolutionStep>,
}

/// Cross-session KPI band for Insights-style overview.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CrossSessionOverview {
    pub session_count: u64,
    pub request_count: u64,
    pub turn_count: u64,
    pub tokens: TokenTotals,
    pub cost: CostView,
    pub cache: CacheView,
    pub composition: Vec<CompositionPart>,
    pub sessions: Vec<SessionOverview>,
}

fn composition_from(tokens: &TokenTotals) -> Vec<CompositionPart> {
    [
        (CompositionKind::Input, tokens.input),
        (CompositionKind::Output, tokens.output),
        (CompositionKind::CacheRead, tokens.cache_read),
        (CompositionKind::CacheWrite, tokens.cache_write),
        (CompositionKind::Reasoning, tokens.reasoning),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(kind, tokens)| CompositionPart { kind, tokens })
    .collect()
}

fn cache_from(tokens: &TokenTotals) -> CacheView {
    CacheView {
        cache_read: tokens.cache_read,
        cache_write: tokens.cache_write,
        hit_rate: cache_hit_rate(tokens),
    }
}

fn cost_from_snapshot(snapshot: &SessionLedgerSnapshot) -> CostView {
    CostView::from_ledger_fields(
        snapshot.cost_status,
        snapshot.cost_amount.clone(),
        snapshot.cost_currency.clone(),
    )
}

fn distinct_turns(requests: &[RequestUsage]) -> u64 {
    let mut seen = std::collections::BTreeSet::new();
    for request in requests {
        seen.insert((request.task_id.as_str(), request.turn));
    }
    seen.len() as u64
}

/// Build one session overview from a ledger snapshot.
pub fn build_session_overview(snapshot: &SessionLedgerSnapshot) -> SessionOverview {
    let tokens = aggregate_tokens(snapshot.requests.iter().map(|request| &request.usage));
    SessionOverview {
        session_id: snapshot.session_id.clone(),
        budget_session_id: snapshot.budget_session_id.clone(),
        parent_session_id: snapshot.parent_session_id.clone(),
        label: snapshot.label.clone(),
        request_count: snapshot.requests.len() as u64,
        turn_count: distinct_turns(&snapshot.requests),
        tokens,
        ledger_settled_tokens: snapshot.settled_tokens,
        ledger_reserved_tokens: snapshot.reserved_tokens,
        ledger_remaining_tokens: snapshot.remaining_tokens,
        budget_unknown: snapshot.budget_unknown,
        cost: cost_from_snapshot(snapshot),
        cache: cache_from(&tokens),
        composition: composition_from(&tokens),
        evolution: build_evolution(&snapshot.requests),
    }
}

/// Aggregate several session overviews. Unknown costs stay empty.
pub fn build_cross_session_overview(snapshots: &[SessionLedgerSnapshot]) -> CrossSessionOverview {
    let sessions: Vec<SessionOverview> = snapshots.iter().map(build_session_overview).collect();
    let tokens = aggregate_tokens(sessions.iter().map(|session| &session.tokens));
    let cost = crate::tokens::merge_cost(sessions.iter().map(|session| &session.cost));
    let request_count = sessions.iter().map(|session| session.request_count).sum();
    let turn_count = sessions.iter().map(|session| session.turn_count).sum();
    CrossSessionOverview {
        session_count: sessions.len() as u64,
        request_count,
        turn_count,
        tokens,
        cost,
        cache: cache_from(&tokens),
        composition: composition_from(&tokens),
        sessions,
    }
}
