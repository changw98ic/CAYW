//! Token and cost aggregation helpers. Never invent monetary amounts.

use serde::{Deserialize, Serialize};

use crate::types::{CostStatus, CostView};

/// Provider-shaped token counters. Mirrors `cayw_ai::Usage` field meanings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenTotals {
    /// Input excludes cache reads/writes; reasoning is a subset of output.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub total_tokens: u64,
}

impl TokenTotals {
    pub fn update_total(&mut self) {
        self.total_tokens = self
            .input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write);
    }

    pub fn saturating_add(self, other: Self) -> Self {
        let mut sum = Self {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            cache_read: self.cache_read.saturating_add(other.cache_read),
            cache_write: self.cache_write.saturating_add(other.cache_write),
            reasoning: self.reasoning.saturating_add(other.reasoning),
            total_tokens: 0,
        };
        sum.update_total();
        sum
    }
}

/// Sum request usages. Callers decide which requests to include.
pub fn aggregate_tokens<'a, I>(usages: I) -> TokenTotals
where
    I: IntoIterator<Item = &'a TokenTotals>,
{
    let mut total = TokenTotals::default();
    for usage in usages {
        total = total.saturating_add(*usage);
    }
    total
}

/// Cache hit rate = cache_read / (input + cache_read). `None` when denominator is 0.
pub fn cache_hit_rate(tokens: &TokenTotals) -> Option<f64> {
    let denom = tokens.input.saturating_add(tokens.cache_read);
    if denom == 0 {
        return None;
    }
    Some((tokens.cache_read as f64) / (denom as f64))
}

/// Merge cost views. Any unknown participant yields unknown with empty amount.
///
/// First cut does **not** sum heterogeneous monetary strings. Multiple independent
/// reported amounts collapse to unknown rather than inventing a total or deduplicating
/// independent expenses by value.
pub fn merge_cost<'a, I>(views: I) -> CostView
where
    I: IntoIterator<Item = &'a CostView>,
{
    let mut reported: Option<(&str, &str)> = None;
    for view in views {
        match view.status {
            CostStatus::Unknown => return CostView::unknown(),
            CostStatus::Reported => {
                let amount = match view.amount.as_deref() {
                    Some(value) if !value.is_empty() => value,
                    _ => return CostView::unknown(),
                };
                let currency = match view.currency.as_deref() {
                    Some(value) if !value.is_empty() => value,
                    _ => return CostView::unknown(),
                };
                if reported.is_some() {
                    // Two or more independent reported amounts cannot be summed without
                    // an exact monetary type, and independent costs must not be deduplicated.
                    return CostView::unknown();
                }
                reported = Some((amount, currency));
            }
        }
    }
    match reported {
        Some((amount, currency)) => CostView::reported(amount, currency),
        None => CostView::unknown(),
    }
}
