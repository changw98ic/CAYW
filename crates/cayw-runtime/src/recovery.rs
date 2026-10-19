//! Host recovery retains both task owners across the Core and Cells commits.
//! No code, model request or capability is replayed while repairing projections.
use crate::{Runtime, RuntimeError, SessionView, lock};
use cayw_cells::{
    CellError, CellExecution, CellIdentity, CellRecord, CellStatus, CellTerminal, ExecutorStopped,
    HostCallSettlement, HostCallTerminal, HostExecutionSettlement, HostResult, HostSettlement,
    IntentStatus, StoppedScope,
};
use cayw_core::{
    AttemptLease, CapabilityCallKey, CapabilityCallRecord, CapabilityOutcome,
    CapabilityReconciliation, CoreError, CoreEvent, ExecutionOutcome, ExecutionRecord,
    ExecutionState, Reconciliation, SessionSnapshot, SessionState,
};
use std::{collections::BTreeMap, fs::File};

struct Facts {
    snapshot: SessionSnapshot,
    leases: BTreeMap<String, AttemptLease>,
    calls: BTreeMap<CapabilityCallKey, CapabilityCallRecord>,
    reconciliations: BTreeMap<CapabilityCallKey, CapabilityReconciliation>,
    tool_reconciliations: BTreeMap<String, Reconciliation>,
    shared_reads: BTreeMap<CapabilityCallKey, crate::shared_read::Evidence>,
    admitted_grants: BTreeMap<CapabilityCallKey, serde_json::Value>,
    admitted_requests: BTreeMap<CapabilityCallKey, serde_json::Value>,
}
impl Facts {
    fn read(runtime: &Runtime, id: &str) -> Result<Self, RuntimeError> {
        let snapshot = runtime.inner.store.snapshot(id)?;
        let mut facts = Self {
            snapshot,
            leases: BTreeMap::new(),
            calls: BTreeMap::new(),
            reconciliations: BTreeMap::new(),
            tool_reconciliations: BTreeMap::new(),
            shared_reads: BTreeMap::new(),
            admitted_grants: BTreeMap::new(),
            admitted_requests: BTreeMap::new(),
        };
        let mut cursor = 0;
        while cursor < facts.snapshot.cursor {
            let limit = (facts.snapshot.cursor - cursor).min(cayw_core::MAX_EVENT_PAGE as u64);
            for event in runtime.inner.store.events(id, cursor, limit as usize)? {
                cursor = event.cursor;
                match event.event {
                    CoreEvent::AttemptStarted { lease } => {
                        facts.leases.insert(lease.attempt_id.clone(), lease);
                    }
                    CoreEvent::CapabilityPlanned { call }
                    | CoreEvent::CapabilityStarted { call }
                    | CoreEvent::CapabilitySettled { call } => {
                        facts.calls.insert(call.key.clone(), call);
                    }
                    CoreEvent::CapabilityReconciled {
                        reconciliation,
                        call,
                    } => {
                        facts
                            .reconciliations
                            .insert(call.key.clone(), reconciliation);
                        facts.calls.insert(call.key.clone(), call);
                    }
                    CoreEvent::AttemptRecovered { capabilities, .. } => {
                        for call in capabilities {
                            facts.calls.insert(call.key.clone(), call);
                        }
                    }
                    CoreEvent::Reconciled { reconciliation } => {
                        facts
                            .tool_reconciliations
                            .insert(reconciliation.call_id.clone(), reconciliation);
                    }
                    CoreEvent::Host { kind, data }
                        if kind == "permission_admitted" || kind == "task_request_admitted" =>
                    {
                        let key: CapabilityCallKey =
                            serde_json::from_value(data.get("key").cloned().ok_or_else(|| {
                                corrupt("admission evidence has no original key")
                            })?)?;
                        let call = facts
                            .calls
                            .get(&key)
                            .ok_or_else(|| corrupt("admission evidence has no original plan"))?;
                        if event.attempt.as_ref() != Some(&call.lease)
                            || event.session_id != call.lease.session_id
                            || data.get("task_id").and_then(serde_json::Value::as_str)
                                != Some(call.task_id.as_str())
                        {
                            return Err(corrupt("admission evidence differs from original owner"));
                        }
                        let entries = if kind == "permission_admitted" {
                            &mut facts.admitted_grants
                        } else {
                            &mut facts.admitted_requests
                        };
                        if entries.insert(key, data).is_some() {
                            return Err(corrupt("duplicate original admission evidence"));
                        }
                    }
                    CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => {
                        let evidence: crate::shared_read::Evidence = serde_json::from_value(data)?;
                        if event.attempt.as_ref() != Some(&evidence.lease)
                            || event.session_id != evidence.lease.session_id
                        {
                            return Err(corrupt(
                                "shared read evidence has a different event envelope",
                            ));
                        }
                        let call = facts.calls.get(&evidence.key).ok_or_else(|| {
                            corrupt("shared read evidence has no original Core plan")
                        })?;
                        if call.stage != cayw_core::CapabilityStage::Started
                            || call.outcome.is_some()
                        {
                            return Err(corrupt(
                                "shared read evidence must follow Core start and precede its terminal",
                            ));
                        }
                        facts.verify_read(&evidence, call)?;
                        if facts
                            .shared_reads
                            .insert(evidence.key.clone(), evidence)
                            .is_some()
                        {
                            return Err(corrupt("duplicate shared read evidence"));
                        }
                    }
                    _ => {}
                }
            }
        }
        for (key, evidence) in &facts.shared_reads {
            let call = facts
                .calls
                .get(key)
                .ok_or_else(|| corrupt("shared read call disappeared"))?;
            facts.verify_read(evidence, call)?;
            if let Some(CapabilityOutcome::Completed { result }) = &call.outcome
                && cayw_cells::HostResult::value_hash(result) != evidence.result_sha256
            {
                return Err(corrupt("shared read terminal differs from captured result"));
            }
        }
        Ok(facts)
    }
    fn verify_read(
        &self,
        evidence: &crate::shared_read::Evidence,
        call: &CapabilityCallRecord,
    ) -> Result<(), RuntimeError> {
        let execution = self
            .execution(&evidence.key.execution_id)
            .ok_or_else(|| corrupt("shared read has no canonical execution"))?;
        let origin = &evidence.origin;
        let identity = cell_identity(&evidence.lease, &evidence.task_id);
        let proof: cayw_core::CapabilityAuthorization = serde_json::from_value(
            call.authorization
                .clone()
                .ok_or_else(|| corrupt("shared read has no original authorization"))?,
        )?;
        if self.leases.get(&evidence.lease.attempt_id) != Some(&evidence.lease)
            || execution.lease != evidence.lease
            || call.lease != evidence.lease
            || call.task_id != evidence.task_id
            || execution.task_id != evidence.task_id
            || origin.execution.identity != identity
            || origin.execution.cell_id != evidence.key.execution_id
            || origin.sequence.checked_sub(1) != Some(evidence.key.sequence)
            || origin.execution.parent_call_id != call.parent_call_id
            || execution.parent_call_id != call.parent_call_id
            || origin.execution.source_hash != call.source_hash
            || execution.source_hash != call.source_hash
            || evidence.name != "fs.read"
            || call.name != evidence.name
            || call.arguments != evidence.arguments
            || proof != evidence.authorization
            || proof.target != evidence.key
            || proof.task_id != evidence.task_id
            || proof.capability != evidence.name
            || proof.scope != crate::permissions::canonical_scope(&evidence.arguments)
            || proof.decision != cayw_core::ApprovalDecision::Approve
            || evidence.audit_id
                != format!("{}:{}", evidence.key.execution_id, evidence.key.sequence)
            || evidence.dependencies.permission_context != identity.permission_context
            || evidence.dependencies.fingerprint != evidence.result_sha256
            || evidence.dependencies.versions.get("workspace") != Some(&evidence.stamp.workspace)
            || evidence.dependencies.versions.get("incarnation")
                != Some(&evidence.stamp.incarnation)
            || evidence.dependencies.versions.get("generation")
                != Some(&evidence.stamp.generation.to_string())
            || evidence.dependencies.versions.get("semantics") != Some(&evidence.stamp.semantics)
        {
            return Err(corrupt(
                "shared read does not match original lease, execution, authority or captured dependencies",
            ));
        }
        if let Some(grant) = &evidence.grant
            && (grant.session_id != evidence.lease.session_id
                || grant.attempt_id != evidence.lease.attempt_id
                || grant.capability != evidence.name
                || grant.scope != proof.scope
                || grant.actor != proof.actor
                || grant.state != "active"
                || !matches!(grant.mode.as_str(), "allow_once" | "allow_task")
                || grant.consumed != (grant.mode == "allow_once")
                || grant.expires_at == 0)
        {
            return Err(corrupt(
                "shared read differs from the originally admitted grant",
            ));
        }
        let admitted = self.admitted_grants.get(&evidence.key);
        match &evidence.grant {
            Some(grant) => {
                let admitted = admitted
                    .ok_or_else(|| corrupt("shared read has no original grant admission"))?;
                if admitted.get("grant_id").and_then(serde_json::Value::as_str)
                    != Some(grant.id.as_str())
                    || admitted
                        .get("approval_id")
                        .and_then(serde_json::Value::as_str)
                        != Some(grant.approval_id.as_str())
                    || admitted.get("grant") != Some(&serde_json::to_value(grant)?)
                    || proof
                        .approval_id
                        .as_ref()
                        .is_some_and(|id| id != &grant.approval_id)
                {
                    return Err(corrupt("shared read replaced its original admitted grant"));
                }
            }
            None if admitted.is_some()
                || proof.actor != "runtime-config"
                || proof.approval_id.is_some() =>
            {
                return Err(corrupt(
                    "shared read dropped its original authority evidence",
                ));
            }
            None => {}
        }
        let request = self.admitted_requests.get(&evidence.key);
        if request.and_then(|data| data.get("receipt")) != evidence.request.as_ref() {
            return Err(corrupt("shared read changed its original Request receipt"));
        }
        if let Some(request) = request
            && request.get("name").and_then(serde_json::Value::as_str)
                != Some(evidence.name.as_str())
        {
            return Err(corrupt("Request receipt refers to a different capability"));
        }
        if evidence.dependencies.versions.len() != 4
            || !hash_valid(&evidence.result_sha256)
            || !hash_valid(&evidence.stamp.incarnation)
            || !hash_valid(&evidence.stamp.semantics)
            || evidence.stamp.workspace.is_empty()
            || evidence.stamp.workspace.len() > 256
        {
            return Err(corrupt("shared read snapshot shape is invalid"));
        }
        if evidence.source.execution.identity != identity {
            return Err(corrupt(
                "shared read source belongs to another authority scope",
            ));
        }
        if evidence.role == cayw_cells::ReadRole::Leader
            || evidence.role == cayw_cells::ReadRole::Uncached
        {
            if evidence.source != evidence.origin {
                return Err(corrupt("read leader replaced its original source"));
            }
        } else {
            let source_key = CapabilityCallKey {
                execution_id: evidence.source.execution.cell_id.clone(),
                sequence: evidence
                    .source
                    .sequence
                    .checked_sub(1)
                    .ok_or_else(|| corrupt("read source sequence is zero"))?,
            };
            let source = self
                .shared_reads
                .get(&source_key)
                .ok_or_else(|| corrupt("shared read follower has no committed source evidence"))?;
            let source_call = self
                .calls
                .get(&source_key)
                .ok_or_else(|| corrupt("shared read source has no Core call"))?;
            if source.origin != evidence.source
                || source.stamp != evidence.stamp
                || source.dependencies != evidence.dependencies
                || source.arguments != evidence.arguments
                || !matches!(&source_call.outcome, Some(CapabilityOutcome::Completed { result }) if cayw_cells::HostResult::value_hash(result) == evidence.result_sha256)
            {
                return Err(corrupt(
                    "shared read source does not retain the original known bytes",
                ));
            }
        }
        Ok(())
    }
    fn execution(&self, id: &str) -> Option<&ExecutionRecord> {
        self.snapshot
            .executions
            .iter()
            .find(|item| item.execution_id == id)
    }
    fn clean(&self) -> bool {
        self.snapshot.state == SessionState::Idle
            && self.snapshot.attempt.is_none()
            && self.snapshot.unresolved_calls.is_empty()
            && self.snapshot.unresolved_capabilities.is_empty()
            && self.snapshot.pending_approvals.is_empty()
            && self.snapshot.executions.iter().all(|execution| {
                !matches!(
                    execution.state,
                    ExecutionState::Running | ExecutionState::Yielded
                ) && execution.terminal.is_some()
            })
    }
}

struct Owners {
    _tasks: Vec<File>,
    cells: Vec<StoppedScope>,
}
impl Runtime {
    /// The caller holds the process-shared lifecycle lock until all commits end.
    fn stopped_owners(&self, id: &str, facts: &Facts) -> Result<Owners, RuntimeError> {
        if lock(&self.inner.active).contains_key(id) {
            return Err(RuntimeError::Busy);
        }
        let tasks = self.inner.queue.list(id)?;
        // Cancelled queue entries can still have a worker finishing admission.
        // Keep every original task lock, including its known terminal entries.
        let ownership = tasks
            .iter()
            .map(|task| self.inner.coordination.task(&task.id))
            .collect::<Result<Vec<_>, _>>()?;
        for task in &tasks {
            if self.inner.process_journal.pending(&task.id)? {
                return Err(RuntimeError::RecoveryRequired {
                    cause: Box::new(RuntimeError::Worker(format!(
                        "process cleanup for task {} is not confirmed; drain its original owner or submit host evidence",
                        task.id,
                    ))),
                    cleanup: None,
                    outcome_unknown: true,
                });
            }
        }
        let frozen = self.inner.cells.is_frozen(id).map_err(cell_error)?;
        let mut scopes = Vec::new();
        for task in tasks {
            let Some(attempt) = task.attempt_id.as_ref() else {
                continue;
            };
            let lease = facts
                .leases
                .get(attempt)
                .ok_or_else(|| corrupt("task attempt has no durable Core lease"))?;
            if lease.session_id != id || task.worker_id.as_ref() != Some(&lease.worker_id) {
                return Err(corrupt("task owner differs from its durable Core lease"));
            }
            let identity = cell_identity(lease, &task.id);
            let records = self
                .inner
                .cells
                .records(&identity, &self.inner.config.cells)
                .map_err(cell_error)?;
            if records.is_empty() && !frozen {
                continue;
            }
            let scope = self.inner.cells.stopped_scope(ExecutorStopped {
                identity,
                evidence: format!("Runtime owns original task {} after its worker released the execution lock", task.id),
            }, self.inner.config.cells.clone()).map_err(cell_error)?;
            scopes.push(scope);
        }
        Ok(Owners {
            _tasks: ownership,
            cells: scopes,
        })
    }

    pub fn recover(&self, id: &str) -> Result<SessionView, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let before = Facts::read(self, id)?;
        let owners = self.stopped_owners(id, &before)?;
        let dispatches = crate::bridge::read_log(&self.inner.store, id, &before.snapshot)?;
        crate::requests::close_session(&self.inner, id)?;
        self.inner.store.recover(id)?;
        dispatches.repair_known(&self.inner.store, id)?;
        let facts = Facts::read(self, id)?;
        self.sync_cells(&owners, &facts)?;
        self.inner.journal.revoke_session(id, "worker recovery")?;
        crate::requests::finish_session(&self.inner, id)?;
        // Failed Cells writes retain both its freeze and the admission slot.
        self.inner.queue.recover(id)?;
        self.inner.changed.notify_waiters();
        self.get(id)
    }

    pub fn reconcile(&self, id: &str, input: Reconciliation) -> Result<SessionView, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let before = Facts::read(self, id)?;
        if before.snapshot.attempt.is_some() {
            return Err(RuntimeError::Busy);
        }
        let retry = before.tool_reconciliations.get(&input.call_id) == Some(&input);
        if !retry
            && !before.snapshot.unresolved_calls.iter().any(|call| {
                call.call.id == input.call_id && call.state == cayw_core::CallState::OutcomeUnknown
            })
        {
            return Err(RuntimeError::InvalidInput("only the original unknown tool or its exact recorded reconciliation can be settled".into()));
        }
        let owners = self.stopped_owners(id, &before)?;
        if !retry {
            self.inner.store.reconcile(id, input)?;
        }
        let facts = Facts::read(self, id)?;
        self.sync_cells(&owners, &facts)?;
        self.inner.changed.notify_waiters();
        self.get(id)
    }

    pub fn reconcile_capability(
        &self,
        id: &str,
        input: CapabilityReconciliation,
    ) -> Result<SessionView, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let before = Facts::read(self, id)?;
        if before.snapshot.attempt.is_some() {
            return Err(RuntimeError::Busy);
        }
        crate::bridge::read_log(&self.inner.store, id, &before.snapshot)?;
        if let Some(call) = before.calls.get(&input.key) {
            if call.external_provenance != input.external_provenance {
                return Err(corrupt(
                    "capability reconciliation differs from its canonical original provenance",
                ));
            }
            if matches!(call.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. }))
                && before
                    .snapshot
                    .unresolved_capabilities
                    .iter()
                    .find(|item| item.key == input.key)
                    != Some(call)
            {
                return Err(corrupt(
                    "capability reconciliation projection differs from canonical Core history",
                ));
            }
        }
        let retry = before.reconciliations.get(&input.key) == Some(&input);
        if !retry
            && !before.calls.get(&input.key).is_some_and(|call| {
                matches!(call.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. }))
            })
        {
            return Err(RuntimeError::InvalidInput("only the original unknown capability or its exact recorded reconciliation can be settled".into()));
        }
        let owners = self.stopped_owners(id, &before)?;
        if !retry {
            self.inner.store.reconcile_capability(id, input)?;
        }
        let facts = Facts::read(self, id)?;
        self.sync_cells(&owners, &facts)?;
        self.inner.changed.notify_waiters();
        self.get(id)
    }

    fn sync_host_fences(&self, owners: &Owners, facts: &Facts) -> Result<(), RuntimeError> {
        use futures::FutureExt;
        let host = &self.inner.host_root;
        let pending = host
            .pending_mutation(cayw_ai::CancellationToken::new())
            .now_or_never()
            .ok_or(RuntimeError::Busy)?
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let faults = host
            .pending_read_faults(cayw_ai::CancellationToken::new())
            .now_or_never()
            .ok_or(RuntimeError::Busy)?
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let original = |audit_id: &str,
                        name: &str,
                        hash: &str|
         -> Result<Option<&CapabilityCallRecord>, RuntimeError> {
            let Some(call) = facts.calls.values().find(|call| {
                format!("{}:{}", call.key.execution_id, call.key.sequence) == audit_id
            }) else {
                return Ok(None);
            };
            if call.name != name || HostResult::value_hash(&call.arguments) != hash {
                return Err(corrupt(
                    "Host fence differs from original canonical capability",
                ));
            }
            let scope = owners
                .cells
                .iter()
                .find(|scope| scope.identity() == &cell_identity(&call.lease, &call.task_id))
                .ok_or_else(|| corrupt("Host fence owner has no stopped Cells scope"))?;
            let execution = facts
                .execution(&call.key.execution_id)
                .ok_or_else(|| corrupt("Host fence has no original execution"))?;
            let record = scope
                .record(&CellExecution {
                    identity: cell_identity(&call.lease, &call.task_id),
                    cell_id: execution.execution_id.clone(),
                    parent_call_id: execution.parent_call_id.clone(),
                    source_hash: execution.source_hash.clone(),
                })
                .map_err(cell_error)?;
            if record.unresolved()
                || record.backend_error.is_some()
                || execution.terminal.is_none()
                || !matches!(
                    call.outcome,
                    Some(
                        CapabilityOutcome::Completed { .. }
                            | CapabilityOutcome::Failed { .. }
                            | CapabilityOutcome::NotExecuted { .. }
                    )
                )
                || self.inner.process_journal.pending(&call.task_id)?
            {
                return Ok(None);
            }
            Ok(Some(call))
        };
        if let Some(pending) = pending
            && let Some(call) = original(
                &pending.audit_id,
                &pending.capability,
                &pending.arguments_sha256,
            )?
        {
            host.recover_mutation(
                cayw_host::FenceRecovery {
                    pending,
                    stopped_owner: format!(
                        "task {} worker {} attempt {} fence {} is locked and stopped",
                        call.task_id, call.lease.worker_id, call.lease.attempt_id, call.lease.fence
                    ),
                    cleanup_evidence: format!(
                        "original task {} has no pending Device process cleanup",
                        call.task_id
                    ),
                    terminal_evidence: format!(
                        "canonical Core call {}:{} and exact Cells terminal repaired",
                        call.key.execution_id, call.key.sequence
                    ),
                },
                cayw_ai::CancellationToken::new(),
            )
            .now_or_never()
            .ok_or(RuntimeError::Busy)?
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        }
        for fault in faults {
            if let Some(call) =
                original(&fault.audit_id, &fault.capability, &fault.arguments_sha256)?
            {
                if let Some(read) = facts.shared_reads.get(&call.key)
                    && read.stamp != fault.stamp
                {
                    return Err(corrupt(
                        "abandoned Host read differs from original captured generation",
                    ));
                }
                host.recover_read_fault(
                    cayw_host::ReadFenceRecovery {
                        fault,
                        stopped_owner: format!(
                            "task {} worker {} attempt {} fence {} is locked and stopped",
                            call.task_id,
                            call.lease.worker_id,
                            call.lease.attempt_id,
                            call.lease.fence
                        ),
                        cleanup_evidence: format!(
                            "original task {} has no pending Device process cleanup",
                            call.task_id
                        ),
                        terminal_evidence: format!(
                            "canonical Core call {}:{} and exact Cells terminal repaired",
                            call.key.execution_id, call.key.sequence
                        ),
                    },
                    cayw_ai::CancellationToken::new(),
                )
                .now_or_never()
                .ok_or(RuntimeError::Busy)?
                .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
            }
        }
        Ok(())
    }
    fn sync_cells(&self, owners: &Owners, facts: &Facts) -> Result<(), RuntimeError> {
        for scope in &owners.cells {
            for record in scope.records().map_err(cell_error)? {
                let execution = record.execution();
                let record = scope.record(&execution).map_err(cell_error)?;
                let lease = facts
                    .leases
                    .get(&record.identity.attempt_id)
                    .ok_or_else(|| corrupt("cell has no original Core attempt"))?;
                if record.identity != cell_identity(lease, &record.identity.task_id) {
                    return Err(corrupt(
                        "cell permission context differs from its original Core lease",
                    ));
                }
                let core = facts.execution(&record.cell_id);
                if let Some(core) = core {
                    verify_execution(&execution, core)?;
                    if core.terminal.is_none() {
                        return Err(RuntimeError::Busy);
                    }
                } else if !record.calls.is_empty() {
                    return Err(corrupt(
                        "cell intents exist without their original Core execution",
                    ));
                }
                let mut calls = Vec::new();
                let mut all_known = true;
                for intent in &record.calls {
                    let sequence = intent
                        .sequence
                        .checked_sub(1)
                        .ok_or_else(|| corrupt("cell capability sequence is zero"))?;
                    let key = CapabilityCallKey {
                        execution_id: record.cell_id.clone(),
                        sequence,
                    };
                    let terminal = if let Some(call) = facts.calls.get(&key) {
                        let core =
                            core.ok_or_else(|| corrupt("capability has no original execution"))?;
                        if call.lease != core.lease
                            || call.task_id != core.task_id
                            || call.parent_call_id != core.parent_call_id
                            || call.source_hash != core.source_hash
                            || call.name != intent.name
                            || call.arguments != intent.arguments
                        {
                            return Err(corrupt(
                                "capability settlement differs from its original cell intent",
                            ));
                        }
                        call_terminal(call.outcome.as_ref(), intent, &self.inner.config.cells)?
                    } else {
                        // Backend::call saves Core's plan before any Host dispatch.
                        // An intent absent from the recovered Core log never ran.
                        if matches!(intent.status, IntentStatus::Succeeded) {
                            return Err(corrupt("successful cell intent has no durable Core plan"));
                        }
                        Some(HostCallTerminal::NotExecuted {
                            reason: "Core has no admitted capability for this original intent; no Host dispatch occurred".into(),
                        })
                    };
                    if let Some(terminal) = terminal {
                        calls.push(HostCallSettlement {
                            sequence: intent.sequence,
                            name: intent.name.clone(),
                            arguments: intent.arguments.clone(),
                            dependencies: if matches!(terminal, HostCallTerminal::Succeeded { .. })
                            {
                                facts
                                    .shared_reads
                                    .get(&key)
                                    .map(|read| read.dependencies.clone())
                                    .or_else(|| intent.dependencies.clone())
                            } else {
                                intent.dependencies.clone()
                            },
                            terminal,
                        });
                    } else {
                        all_known = false;
                    }
                }
                let count = facts
                    .calls
                    .keys()
                    .filter(|key| key.execution_id == record.cell_id)
                    .count();
                if count > record.calls.len() {
                    return Err(corrupt(
                        "Core has capabilities absent from the durable Cells intents",
                    ));
                }
                let evidence = format!(
                    "Core session {} retains original execution {} and these exact capability terminals",
                    facts.snapshot.id, record.cell_id
                );
                let execution_terminal = if all_known {
                    let terminal = terminal_for(&record, core);
                    // A retry retains the original lifecycle error reference;
                    // its committed Cells audit is the proof it was repaired.
                    let prior = record.host_settlements.iter().find_map(|entry| {
                        let old = &entry.settlement;
                        (old.execution == execution
                            && old.calls == calls
                            && old.evidence == evidence)
                            .then_some(old.execution_terminal.as_ref())
                            .flatten()
                            .filter(|old| {
                                record.backend_error.is_none() && old.terminal == terminal
                            })
                            .cloned()
                    });
                    Some(prior.unwrap_or(HostExecutionSettlement {
                        expected_backend_error: record.backend_error.clone(),
                        terminal,
                        evidence: evidence.clone(),
                    }))
                } else {
                    None
                };
                scope
                    .settle(HostSettlement {
                        execution,
                        calls,
                        execution_terminal,
                        evidence,
                    })
                    .map_err(cell_error)?;
            }
        }
        self.sync_host_fences(owners, facts)?;
        if facts.clean()
            && let Some(scope) = owners.cells.first()
        {
            // One exclusive session VM gate covers every repaired task. Taking
            // it a second time through another scope would conflict with us.
            self.inner.reads.thaw(scope).map_err(cell_error)?;
        }
        Ok(())
    }
}

fn cell_identity(lease: &AttemptLease, task: &str) -> CellIdentity {
    CellIdentity {
        session_id: lease.session_id.clone(),
        task_id: task.into(),
        attempt_id: lease.attempt_id.clone(),
        permission_context: format!("{}:{}", lease.worker_id, lease.fence),
    }
}
fn verify_execution(cell: &CellExecution, core: &ExecutionRecord) -> Result<(), RuntimeError> {
    if cell.cell_id != core.execution_id
        || cell.parent_call_id != core.parent_call_id
        || cell.source_hash != core.source_hash
        || cell.identity != cell_identity(&core.lease, &core.task_id)
    {
        return Err(corrupt(
            "cell execution differs from its immutable Core origin",
        ));
    }
    Ok(())
}
fn call_terminal(
    outcome: Option<&CapabilityOutcome>,
    intent: &cayw_cells::CapabilityIntent,
    limits: &cayw_cells::CellLimits,
) -> Result<Option<HostCallTerminal>, RuntimeError> {
    let terminal = match outcome {
        Some(CapabilityOutcome::Completed { result }) => {
            let mut pending = vec![(result, 1usize)];
            let mut fits = true;
            while let Some((value, depth)) = pending.pop() {
                if depth > limits.json_depth {
                    fits = false;
                    break;
                }
                match value {
                    serde_json::Value::Array(items) => {
                        pending.extend(items.iter().map(|item| (item, depth + 1)))
                    }
                    serde_json::Value::Object(items) => {
                        pending.extend(items.values().map(|item| (item, depth + 1)))
                    }
                    _ => {}
                }
            }
            fits &= serde_json::to_vec(result)?.len() <= limits.item_bytes;
            HostCallTerminal::Succeeded {
                result: if fits {
                    HostResult::Value(result.clone())
                } else {
                    HostResult::Omitted {
                        sha256: HostResult::value_hash(result),
                    }
                },
            }
        }
        Some(CapabilityOutcome::Failed { error }) => HostCallTerminal::Failed {
            error: cell_message(error),
        },
        Some(CapabilityOutcome::NotExecuted { reason }) => {
            // Cells' ordinary known Err is a Failed intent even when Core knows
            // the more specific fact that dispatch never began. Preserve it.
            if intent.status == IntentStatus::Failed
                && intent.error.as_deref() == Some(cell_message(reason).as_str())
            {
                HostCallTerminal::Failed {
                    error: cell_message(reason),
                }
            } else {
                HostCallTerminal::NotExecuted {
                    reason: cell_message(reason),
                }
            }
        }
        Some(CapabilityOutcome::OutcomeUnknown { .. }) => return Ok(None),
        None => return Err(corrupt("recovered capability has no durable terminal")),
    };
    Ok(Some(terminal))
}
fn terminal_for(record: &CellRecord, core: Option<&ExecutionRecord>) -> CellTerminal {
    // A durable known VM terminal survives a failed backend lifecycle write.
    if record.status.is_terminal() && record.status != CellStatus::OutcomeUnknown {
        return CellTerminal {
            status: record.status,
            error: record.error.clone(),
            outcome_unknown: false,
        };
    }
    let (status, error) = if record.status == CellStatus::OutcomeUnknown {
        (CellStatus::Reconciled, None)
    } else {
        match core.and_then(|execution| execution.terminal.as_ref()) {
            Some(ExecutionOutcome::Failed { error }) => {
                (CellStatus::Failed, Some(cell_message(error)))
            }
            Some(ExecutionOutcome::Cancelled { reason }) => {
                (CellStatus::Cancelled, Some(cell_message(reason)))
            }
            _ => (
                CellStatus::Interrupted,
                Some("Original VM stopped; source will not be replayed".into()),
            ),
        }
    };
    CellTerminal {
        status,
        error,
        outcome_unknown: false,
    }
}
fn cell_message(message: &str) -> String {
    let mut end = message.len().min(160);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].into()
}
fn cell_error(error: CellError) -> RuntimeError {
    RuntimeError::Workspace(format!("Cells recovery: {error}"))
}
fn hash_valid(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn corrupt(message: &str) -> RuntimeError {
    CoreError::CorruptStore(message.into()).into()
}
