//! Live F05 context overview: adapt the durable model-request ledger into
//! `cayw-context` snapshots and fold a session card. No second ledger.
use crate::model_requests::{
    ModelCostStatus, ModelRequestLedger, ModelRequestRecord, ModelRequestTerminal, visit,
};
use crate::{Runtime, RuntimeError};
use cayw_ai::UsageStatus;
use cayw_context::{
    CostStatus, RequestTerminal, RequestUsage, SessionLedgerSnapshot, SessionOverview, TokenTotals,
    UsageReportStatus, build_session_overview,
};
use cayw_core::{CoreEvent, SessionStore};

/// Map one durable ledger into a session-scoped overview input.
///
/// Request rows are filtered to `session_id`. Budget fields (`token_limit`,
/// reserved/remaining, cost, `budget_unknown`) stay the lineage authority from
/// [`ModelRequestLedger`] — this adapter does not invent a second budget.
pub fn session_ledger_snapshot_from_model_requests(
    ledger: &ModelRequestLedger,
    session_id: &str,
    parent_session_id: Option<String>,
    label: Option<String>,
) -> SessionLedgerSnapshot {
    let requests = ledger
        .requests
        .iter()
        .filter(|record| record.identity.lease.session_id == session_id)
        .map(|record| request_usage_from_record(record, parent_session_id.as_deref()))
        .collect();
    SessionLedgerSnapshot {
        session_id: session_id.into(),
        budget_session_id: ledger.budget_session_id.clone(),
        parent_session_id,
        label,
        token_limit: ledger.token_limit,
        settled_tokens: ledger.settled_tokens,
        reserved_tokens: ledger.reserved_tokens,
        remaining_tokens: ledger.remaining_tokens,
        budget_unknown: ledger.budget_unknown,
        cost_status: map_cost_status(ledger.cost_status),
        cost_amount: ledger.cost_amount.clone(),
        cost_currency: ledger.cost_currency.clone(),
        requests,
    }
}

fn request_usage_from_record(
    record: &ModelRequestRecord,
    parent_session_id: Option<&str>,
) -> RequestUsage {
    let (usage_status, usage) = match record.observations.last() {
        Some(observation) => (
            map_usage_status(observation.observation.usage_status),
            TokenTotals {
                input: observation.observation.usage.input,
                output: observation.observation.usage.output,
                cache_read: observation.observation.usage.cache_read,
                cache_write: observation.observation.usage.cache_write,
                reasoning: observation.observation.usage.reasoning,
                total_tokens: observation.observation.usage.total_tokens,
            },
        ),
        None => (UsageReportStatus::Unknown, TokenTotals::default()),
    };
    RequestUsage {
        request_id: record.identity.request_id.clone(),
        session_id: record.identity.lease.session_id.clone(),
        task_id: record.identity.task_id.clone(),
        budget_session_id: record.identity.budget_session_id.clone(),
        parent_session_id: parent_session_id.map(str::to_owned),
        turn: record.identity.turn,
        provider: record.identity.provider.clone(),
        model: record.identity.model.clone(),
        terminal: record.terminal.map(map_terminal),
        usage_status,
        usage,
        cost_status: map_cost_status(record.cost_status),
        cost_amount: record.cost_amount.clone(),
        cost_currency: record.cost_currency.clone(),
    }
}

fn map_terminal(terminal: ModelRequestTerminal) -> RequestTerminal {
    match terminal {
        ModelRequestTerminal::Completed => RequestTerminal::Completed,
        ModelRequestTerminal::Failed => RequestTerminal::Failed,
        ModelRequestTerminal::Cancelled => RequestTerminal::Cancelled,
        ModelRequestTerminal::CancelledBeforeDispatch => RequestTerminal::CancelledBeforeDispatch,
        ModelRequestTerminal::Dropped => RequestTerminal::Dropped,
        ModelRequestTerminal::MissingTerminal => RequestTerminal::MissingTerminal,
        ModelRequestTerminal::RecordingFailed => RequestTerminal::RecordingFailed,
        ModelRequestTerminal::InvalidUsage => RequestTerminal::InvalidUsage,
        ModelRequestTerminal::BudgetExceeded => RequestTerminal::BudgetExceeded,
        ModelRequestTerminal::Rejected => RequestTerminal::Rejected,
    }
}

fn map_usage_status(status: UsageStatus) -> UsageReportStatus {
    match status {
        UsageStatus::Unknown => UsageReportStatus::Unknown,
        UsageStatus::Partial => UsageReportStatus::Partial,
        UsageStatus::Reported => UsageReportStatus::Reported,
        UsageStatus::Invalid => UsageReportStatus::Invalid,
    }
}

fn map_cost_status(status: ModelCostStatus) -> CostStatus {
    match status {
        ModelCostStatus::Unknown => CostStatus::Unknown,
    }
}

fn immediate_parent(store: &SessionStore, session: &str) -> Result<Option<String>, RuntimeError> {
    let snapshot = store.snapshot(session)?;
    let mut parent = None;
    visit(store, &snapshot, |event| {
        let found = match &event.event {
            CoreEvent::SessionForked {
                source_session_id, ..
            } => Some(source_session_id.clone()),
            CoreEvent::Host { kind, data } if kind == "agent_child_linked" => Some(
                data.get("decision")
                    .unwrap_or(data)["parent_session"]
                    .as_str()
                    .ok_or_else(|| {
                        RuntimeError::Worker(
                            "model request ledger is inconsistent; keep its reservations and inspect the original Core events"
                                .into(),
                        )
                    })?
                    .to_owned(),
            ),
            _ => None,
        };
        if let Some(found) = found
            && parent.replace(found).is_some()
        {
            return Err(RuntimeError::Worker(
                "model request ledger is inconsistent; keep its reservations and inspect the original Core events"
                    .into(),
            ));
        }
        Ok(())
    })?;
    Ok(parent)
}

impl Runtime {
    /// Project the durable model-request ledger into a `cayw-context` snapshot
    /// for one session. Reuses [`Self::model_requests`]; does not open a second
    /// accounting store.
    pub fn session_ledger_snapshot(
        &self,
        session: &str,
    ) -> Result<SessionLedgerSnapshot, RuntimeError> {
        let view = self.get(session)?;
        let ledger = self.model_requests(session)?;
        let parent = immediate_parent(&self.inner.store, session)?;
        let label = Some(view.label).filter(|value| !value.is_empty());
        Ok(session_ledger_snapshot_from_model_requests(
            &ledger, session, parent, label,
        ))
    }

    /// Session context / token / cost / cache overview from the live ledger.
    pub fn session_context_overview(&self, session: &str) -> Result<SessionOverview, RuntimeError> {
        Ok(build_session_overview(
            &self.session_ledger_snapshot(session)?,
        ))
    }
}
