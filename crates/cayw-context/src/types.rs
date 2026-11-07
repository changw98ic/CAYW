//! Serializable snapshots and overview DTOs for F05 first cut.

use serde::{Deserialize, Serialize};

use crate::tokens::TokenTotals;

/// Availability of a trusted monetary settlement. Missing is not zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CostStatus {
    /// No trusted price or monetary settlement is available.
    #[default]
    Unknown,
    /// A trusted amount was supplied by the host / ledger.
    Reported,
}

/// Provider usage report quality for one request observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UsageReportStatus {
    #[default]
    Unknown,
    Partial,
    Reported,
    Invalid,
}

/// Terminal class mirrored from the durable model-request ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestTerminal {
    Completed,
    Failed,
    Cancelled,
    CancelledBeforeDispatch,
    Dropped,
    MissingTerminal,
    RecordingFailed,
    InvalidUsage,
    BudgetExceeded,
    Rejected,
}

/// One settled (or last-observed) model request, ready for overview folding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestUsage {
    pub request_id: String,
    pub session_id: String,
    pub task_id: String,
    pub budget_session_id: String,
    /// Parent session when this request ran in a child agent; `None` for roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// One-based model request within the task attempt.
    pub turn: u64,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<RequestTerminal>,
    pub usage_status: UsageReportStatus,
    pub usage: TokenTotals,
    pub cost_status: CostStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_currency: Option<String>,
}

/// One session's ledger projection used as overview input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLedgerSnapshot {
    pub session_id: String,
    pub budget_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub token_limit: u64,
    pub settled_tokens: u64,
    pub reserved_tokens: u64,
    pub remaining_tokens: u64,
    pub budget_unknown: bool,
    pub cost_status: CostStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_currency: Option<String>,
    pub requests: Vec<RequestUsage>,
}

/// Cost view: never fabricates a number when status is [`CostStatus::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostView {
    pub status: CostStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

impl CostView {
    pub fn unknown() -> Self {
        Self {
            status: CostStatus::Unknown,
            amount: None,
            currency: None,
        }
    }

    pub fn reported(amount: impl Into<String>, currency: impl Into<String>) -> Self {
        Self {
            status: CostStatus::Reported,
            amount: Some(amount.into()),
            currency: Some(currency.into()),
        }
    }

    pub fn from_ledger_fields(
        status: CostStatus,
        amount: Option<String>,
        currency: Option<String>,
    ) -> Self {
        match status {
            CostStatus::Unknown => Self::unknown(),
            CostStatus::Reported => {
                let Some(amount) = amount.filter(|value| !value.is_empty()) else {
                    return Self::unknown();
                };
                let Some(currency) = currency.filter(|value| !value.is_empty()) else {
                    return Self::unknown();
                };
                Self::reported(amount, currency)
            }
        }
    }
}

/// Cache hit rate when computable; otherwise omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheView {
    pub cache_read: u64,
    pub cache_write: u64,
    /// `cache_read / (input + cache_read)` when the denominator is > 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_rate: Option<f64>,
}

/// First-cut composition buckets = provider-reported token parts.
/// Message-category composition is deferred to a later slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionPart {
    pub kind: CompositionKind,
    pub tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositionKind {
    Input,
    Output,
    CacheRead,
    CacheWrite,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    #[error("invalid context overview input: {0}")]
    Invalid(String),
}
