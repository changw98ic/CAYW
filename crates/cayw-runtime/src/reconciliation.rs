//! Host reconciliation is distinct from the original model observation.
use crate::{
    Runtime, RuntimeError,
    recovery::{Facts, Owners},
};
use cayw_ai::Message;
use cayw_cells::{CellExecution, HostCallTerminal, IntentStatus};
use cayw_core::{
    AttemptLease, CallState, CapabilityCallRecord, CapabilityOutcome, CapabilityReconciliation,
    ReconciliationOrigin, ReconciliationRequest, SessionState,
};

impl Runtime {
    pub(crate) fn verify_reconciliation_request(
        &self,
        request: ReconciliationRequest,
    ) -> Result<(), RuntimeError> {
        self.inner.store.verify_reconciliation(&request)?;
        Ok(())
    }

    pub(crate) fn verify_original_task(
        &self,
        id: &str,
        lease: &AttemptLease,
        task_id: &str,
        facts: &Facts,
    ) -> Result<(), RuntimeError> {
        let task = self.inner.queue.get(task_id)?;
        if lease.session_id != id
            || facts.leases.get(&lease.attempt_id) != Some(lease)
            || task.session_id != id
            || task.attempt_id.as_ref() != Some(&lease.attempt_id)
            || task.worker_id.as_ref() != Some(&lease.worker_id)
        {
            return Err(RuntimeError::InvalidInput(
                "reconciliation differs from the original Task and complete attempt lease".into(),
            ));
        }
        Ok(())
    }

    /// Inspect exact original Cells facts while retaining stopped ownership.
    /// This is read-only and precedes the Core reconciliation transaction.
    pub(crate) fn verify_cells_reconciliation(
        &self,
        owners: &Owners,
        facts: &Facts,
        call: &CapabilityCallRecord,
        input: &CapabilityReconciliation,
    ) -> Result<(), RuntimeError> {
        if *input.expected != ReconciliationOrigin::from(call) || input.key != call.key {
            return Err(RuntimeError::InvalidInput(
                "capability reconciliation changed its original identity".into(),
            ));
        }
        let core = facts
            .execution(&call.key.execution_id)
            .ok_or_else(|| RuntimeError::InvalidInput("original execution is absent".into()))?;
        let identity = crate::recovery::cell_identity(&call.lease, &call.task_id);
        let scope = owners
            .cells
            .iter()
            .find(|scope| scope.identity() == &identity)
            .ok_or_else(|| {
                RuntimeError::InvalidInput("original stopped Cells owner is absent".into())
            })?;
        let record = scope
            .record(&CellExecution {
                identity,
                cell_id: core.execution_id.clone(),
                parent_call_id: core.parent_call_id.clone(),
                source_hash: core.source_hash.clone(),
            })
            .map_err(crate::recovery::cell_error)?;
        crate::recovery::verify_execution(&record.execution(), core)?;
        let sequence = call
            .key
            .sequence
            .checked_add(1)
            .ok_or_else(|| RuntimeError::InvalidInput("capability sequence overflow".into()))?;
        let intent = record
            .calls
            .iter()
            .find(|intent| intent.sequence == sequence)
            .ok_or_else(|| RuntimeError::InvalidInput("original Cells intent is absent".into()))?;
        if call.lease != core.lease
            || call.task_id != core.task_id
            || call.parent_call_id != core.parent_call_id
            || call.source_hash != core.source_hash
            || call.name != intent.name
            || call.arguments != intent.arguments
        {
            return Err(RuntimeError::InvalidInput(
                "original capability differs from Cells intent".into(),
            ));
        }
        let terminal =
            crate::recovery::call_terminal(Some(&input.outcome), intent, &self.inner.config.cells)?
                .ok_or_else(|| {
                    RuntimeError::InvalidInput("reconciliation still has an unknown outcome".into())
                })?;
        verify_reliable_terminal(intent, &terminal, &input.outcome)
    }

    pub(crate) fn verify_tool_cells_reconciliation(
        &self,
        owners: &Owners,
        input: &cayw_core::Reconciliation,
    ) -> Result<(), RuntimeError> {
        if !matches!(
            input.outcome,
            cayw_core::ReconciledOutcome::NotExecuted { .. }
        ) {
            return Ok(());
        }
        let Some(execution) = &input.expected.execution else {
            return Ok(());
        };
        // A wait observes an existing execution. Its child results do not prove
        // that this particular wait was dispatched. Only compare launch claims.
        if execution.parent_call_id != input.call_id {
            return Ok(());
        }
        let identity =
            crate::recovery::cell_identity(&input.expected.lease, &input.expected.task_id);
        let scope = owners
            .cells
            .iter()
            .find(|scope| scope.identity() == &identity)
            .ok_or_else(|| {
                RuntimeError::InvalidInput("original stopped Cells owner is absent".into())
            })?;
        let record = scope
            .record(&CellExecution {
                identity,
                cell_id: execution.execution_id.clone(),
                parent_call_id: execution.parent_call_id.clone(),
                source_hash: execution.source_hash.clone(),
            })
            .map_err(crate::recovery::cell_error)?;
        if record.calls.iter().any(|intent| {
            matches!(
                intent.status,
                IntentStatus::Succeeded | IntentStatus::Failed
            ) || intent.result.is_some()
                || intent.result_hash.is_some()
        }) {
            return Err(RuntimeError::InvalidInput(
                "tool nonexecution claim contradicts its reliable original Cells result".into(),
            ));
        }
        Ok(())
    }

    /// Build only the next Agent's input. Durable messages and events remain
    /// untouched. Each cleared stop has an exact, already committed host decision.
    pub(crate) fn continuation_messages(&self, id: &str) -> Result<Vec<Message>, RuntimeError> {
        let facts = Facts::read(self, id)?;
        if facts.snapshot.state == SessionState::NeedsReconciliation
            || facts
                .snapshot
                .unresolved_calls
                .iter()
                .any(|call| call.state == CallState::OutcomeUnknown)
        {
            return Err(RuntimeError::NeedsReconciliation);
        }
        let mut messages = facts.snapshot.messages.clone();
        let mut tool_decisions = std::collections::BTreeMap::new();
        for message in &mut messages {
            if let Message::ToolResult(result) = message
                && result.outcome_unknown
            {
                let decision = self
                    .inner
                    .store
                    .committed_tool_reconciliation(id, &result.tool_call_id)?;
                tool_decisions.insert(result.tool_call_id.clone(), decision);
                result.outcome_unknown = false;
            }
        }
        for decision in tool_decisions.values() {
            messages.push(Message::system(format!(
                "Trusted host reconciliation for the original tool call. The earlier tool reply remains the original observation. Do not replay its execution. Decision: {}",
                serde_json::to_string(decision)?
            )));
        }
        for decision in facts.reconciliations.values() {
            messages.push(Message::system(format!(
                "Trusted host reconciliation for the original capability. Do not replay its execution. Decision: {}",
                serde_json::to_string(decision)?
            )));
        }
        Ok(messages)
    }
}

fn verify_reliable_terminal(
    intent: &cayw_cells::CapabilityIntent,
    terminal: &HostCallTerminal,
    outcome: &CapabilityOutcome,
) -> Result<(), RuntimeError> {
    let conflict = || {
        RuntimeError::InvalidInput(
            "reconciliation contradicts the reliable original Cells result".into(),
        )
    };
    let proposed_value = match outcome {
        CapabilityOutcome::Completed { result } => Some(result),
        _ => None,
    };
    let proposed_hash = proposed_value.map(cayw_cells::HostResult::value_hash);
    if intent
        .result
        .as_ref()
        .is_some_and(|result| proposed_value != Some(result))
        || intent
            .result_hash
            .as_ref()
            .is_some_and(|hash| proposed_hash.as_ref() != Some(hash))
    {
        return Err(conflict());
    }
    if matches!(
        intent.status,
        IntentStatus::Pending | IntentStatus::OutcomeUnknown
    ) {
        return Ok(());
    }
    match (intent.status, terminal) {
        (IntentStatus::Succeeded, HostCallTerminal::Succeeded { .. }) => Ok(()),
        (IntentStatus::Failed, HostCallTerminal::Failed { error })
            if intent.error.as_ref() == Some(error) =>
        {
            Ok(())
        }
        (IntentStatus::NotExecuted, HostCallTerminal::NotExecuted { reason })
            if intent.error.as_ref() == Some(reason) =>
        {
            Ok(())
        }
        _ => Err(conflict()),
    }
}
