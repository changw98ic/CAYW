//! Parent / child agent fleet overview for a shared budget lineage.

use serde::Serialize;

use crate::overview::build_session_overview;
use crate::tokens::{TokenTotals, aggregate_tokens, cache_hit_rate, merge_cost};
use crate::types::*;

/// One agent (root or child) on the fleet board.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FleetMember {
    pub session_id: String,
    pub budget_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub is_root: bool,
    pub request_count: u64,
    pub turn_count: u64,
    pub tokens: TokenTotals,
    pub cost: CostView,
    pub cache: CacheView,
}

/// Fleet overview: every member that shares one budget lineage, on one screen.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FleetOverview {
    pub budget_session_id: String,
    pub root_session_id: String,
    pub member_count: u64,
    pub tokens: TokenTotals,
    pub cost: CostView,
    pub cache: CacheView,
    pub members: Vec<FleetMember>,
}

fn cache_from(tokens: &TokenTotals) -> CacheView {
    CacheView {
        cache_read: tokens.cache_read,
        cache_write: tokens.cache_write,
        hit_rate: cache_hit_rate(tokens),
    }
}

/// Build a fleet overview from sessions that share `budget_session_id`.
///
/// Sessions with a different budget id are ignored. The root is the member whose
/// `session_id` equals the budget lineage id, or else the member without a parent.
pub fn build_fleet_overview(
    budget_session_id: &str,
    snapshots: &[SessionLedgerSnapshot],
) -> Result<FleetOverview, ContextError> {
    if budget_session_id.is_empty() {
        return Err(ContextError::Invalid("budget_session_id is empty".into()));
    }
    let mut members = Vec::new();
    for snapshot in snapshots {
        if snapshot.budget_session_id != budget_session_id {
            continue;
        }
        let overview = build_session_overview(snapshot);
        let is_root =
            snapshot.session_id == budget_session_id || snapshot.parent_session_id.is_none();
        members.push(FleetMember {
            session_id: overview.session_id,
            budget_session_id: overview.budget_session_id,
            parent_session_id: overview.parent_session_id,
            label: overview.label,
            is_root,
            request_count: overview.request_count,
            turn_count: overview.turn_count,
            tokens: overview.tokens,
            cost: overview.cost,
            cache: overview.cache,
        });
    }
    if members.is_empty() {
        return Err(ContextError::Invalid(format!(
            "no sessions share budget_session_id {budget_session_id}"
        )));
    }
    members.sort_by(|left, right| {
        right
            .is_root
            .cmp(&left.is_root)
            .then(left.session_id.cmp(&right.session_id))
    });
    let root_session_id = members
        .iter()
        .find(|member| member.is_root)
        .map(|member| member.session_id.clone())
        .unwrap_or_else(|| members[0].session_id.clone());
    let tokens = aggregate_tokens(members.iter().map(|member| &member.tokens));
    let cost = merge_cost(members.iter().map(|member| &member.cost));
    Ok(FleetOverview {
        budget_session_id: budget_session_id.into(),
        root_session_id,
        member_count: members.len() as u64,
        tokens,
        cost,
        cache: cache_from(&tokens),
        members,
    })
}
