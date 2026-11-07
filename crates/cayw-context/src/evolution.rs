//! Request-level context evolution steps (first cut).

use serde::Serialize;

use crate::tokens::TokenTotals;
use crate::types::{RequestTerminal, RequestUsage, UsageReportStatus};

/// One evolution step = one model request's contribution to context growth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvolutionStep {
    pub request_id: String,
    pub session_id: String,
    pub task_id: String,
    pub turn: u64,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<RequestTerminal>,
    pub usage_status: UsageReportStatus,
    pub usage: TokenTotals,
}

/// Build chronological evolution steps from request usages (input order preserved).
pub fn build_evolution(requests: &[RequestUsage]) -> Vec<EvolutionStep> {
    requests
        .iter()
        .map(|request| EvolutionStep {
            request_id: request.request_id.clone(),
            session_id: request.session_id.clone(),
            task_id: request.task_id.clone(),
            turn: request.turn,
            provider: request.provider.clone(),
            model: request.model.clone(),
            terminal: request.terminal,
            usage_status: request.usage_status,
            usage: request.usage,
        })
        .collect()
}
