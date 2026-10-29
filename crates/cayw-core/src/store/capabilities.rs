use super::*;

impl StoredSession {
    pub(super) fn has_unknown(&self) -> bool {
        self.calls
            .iter()
            .any(|c| c.record.state == CallState::OutcomeUnknown)
            || self
                .capabilities
                .iter()
                .any(|c| matches!(c.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. })))
    }

    pub(super) fn admit_work(&self, lease: &AttemptLease) -> Result<(), CoreError> {
        self.verify_lease(lease)?;
        if self.has_unknown() {
            return Err(CoreError::NeedsReconciliation);
        }
        if !self.lifecycle()?.started || self.lifecycle()?.ended {
            return Err(invalid("New execution work requires a live agent"));
        }
        Ok(())
    }

    fn execution(&self, id: &str) -> Result<&ExecutionRecord, CoreError> {
        self.executions
            .iter()
            .find(|e| e.execution_id == id)
            .ok_or_else(|| CoreError::ExecutionNotFound(id.into()))
    }

    fn capability(&self, key: &CapabilityCallKey) -> Result<&CapabilityCallRecord, CoreError> {
        self.capabilities
            .iter()
            .find(|c| c.key == *key)
            .ok_or_else(|| CoreError::CapabilityNotFound(key.clone()))
    }

    fn capability_mut(
        &mut self,
        key: &CapabilityCallKey,
    ) -> Result<&mut CapabilityCallRecord, CoreError> {
        self.capabilities
            .iter_mut()
            .find(|c| c.key == *key)
            .ok_or_else(|| CoreError::CapabilityNotFound(key.clone()))
    }

    fn verify_execution(
        &self,
        lease: &AttemptLease,
        id: &str,
    ) -> Result<&ExecutionRecord, CoreError> {
        self.verify_lease(lease)?;
        let execution = self.execution(id)?;
        let parent = self.call(&execution.parent_call_id)?;
        if execution.lease != *lease
            || parent.record.attempt_id != lease.attempt_id
            || parent.record.call.name != "execute_code"
            || !parent.record.execution_started
        {
            return Err(invalid(
                "Execution origin does not match its original lease and task",
            ));
        }
        if !execution_active(execution) {
            return Err(invalid("Execution is already terminal"));
        }
        Ok(execution)
    }

    fn verify_capability(
        &self,
        lease: &AttemptLease,
        key: &CapabilityCallKey,
    ) -> Result<&CapabilityCallRecord, CoreError> {
        let execution = self.verify_execution(lease, &key.execution_id)?;
        let call = self.capability(key)?;
        if call.lease != *lease
            || call.parent_call_id != execution.parent_call_id
            || call.task_id != execution.task_id
            || call.source_hash != execution.source_hash
        {
            return Err(invalid("Capability origin does not match its execution"));
        }
        Ok(call)
    }

    pub(super) fn validate_approval_target(
        &self,
        lease: &AttemptLease,
        input: &ApprovalInput,
    ) -> Result<(), CoreError> {
        match &input.target {
            ApprovalTarget::ToolCall { call_id } => {
                identifier(call_id, "call_id")?;
                let call = self.call(call_id)?;
                if call.record.attempt_id != lease.attempt_id
                    || !matches!(call.record.state, CallState::Planned | CallState::Prepared)
                    || call.record.execution_started
                {
                    return Err(invalid("Tool approval must precede execution"));
                }
                if self.approvals.iter().any(|a| {
                    a.attempt_id == lease.attempt_id
                        && a.input.target == input.target
                        && a.input.task_id != input.task_id
                }) {
                    return Err(invalid(
                        "Tool approval cannot change its trusted task binding",
                    ));
                }
            }
            ApprovalTarget::CapabilityCall { key } => {
                validate_key(key)?;
                let call = self.verify_capability(lease, key)?;
                if call.stage != CapabilityStage::Planned
                    || call.authorization.is_some()
                    || call.outcome.is_some()
                    || input.task_id != call.task_id
                    || input.capability != call.name
                {
                    return Err(invalid(
                        "Capability approval requires its exact planned task and capability",
                    ));
                }
            }
        }
        if self.denied_scope(input) {
            return Err(CoreError::ApprovalDenied);
        }
        Ok(())
    }

    fn denied_scope(&self, input: &ApprovalInput) -> bool {
        // Replacing a worker or VM cannot erase the host's denial for this task.
        self.approvals.iter().any(|a| {
            a.input.task_id == input.task_id
                && a.input.capability == input.capability
                && a.input.scope == input.scope
                && a.decision
                    .as_ref()
                    .is_some_and(|d| d.decision == ApprovalDecision::Deny)
        })
    }
}

fn validate_key(key: &CapabilityCallKey) -> Result<(), CoreError> {
    identifier(&key.execution_id, "execution_id")?;
    if key.sequence > MAX_CAPABILITY_CALLS as u64 {
        return Err(CoreError::CounterExhausted);
    }
    Ok(())
}

fn validate_outcome(outcome: &CapabilityOutcome) -> Result<(), CoreError> {
    match outcome {
        CapabilityOutcome::Completed { .. } => Ok(()),
        CapabilityOutcome::Failed { error } | CapabilityOutcome::OutcomeUnknown { error } => {
            nonempty(error, "error")
        }
        CapabilityOutcome::NotExecuted { reason } => nonempty(reason, "reason"),
    }
}

impl SessionStore {
    /// Called by the host after mark_tool_started for the original execute_code.
    /// A wait tool cannot create or change this origin.
    pub fn begin_execution(
        &self,
        lease: &AttemptLease,
        input: ExecutionInput,
    ) -> Result<ExecutionRecord, CoreError> {
        identifier(&input.execution_id, "execution_id")?;
        identifier(&input.parent_call_id, "parent_call_id")?;
        identifier(&input.task_id, "task_id")?;
        identifier(&input.source_hash, "source_hash")?;
        self.mutate(&lease.session_id, |session| {
            session.admit_work(lease)?;
            if session.executions.len() >= MAX_EXECUTIONS {
                return Err(invalid("Execution count limit exceeded"));
            }
            if session
                .executions
                .iter()
                .any(|e| e.execution_id == input.execution_id)
            {
                return Err(invalid("Execution identifiers cannot be reused"));
            }
            let parent = session.call(&input.parent_call_id)?;
            if parent.record.attempt_id != lease.attempt_id
                || parent.record.state != CallState::Started
                || !parent.record.execution_started
                || parent.record.call.name != "execute_code"
                || session
                    .execution_waits
                    .iter()
                    .any(|binding| binding.call_id == input.parent_call_id)
            {
                return Err(invalid(
                    "Execution requires its current started execute_code parent and exact task",
                ));
            }
            // The host supplies task_id from its durable TaskLease. Model
            // arguments are deliberately irrelevant to this authority boundary.
            let target = ApprovalTarget::ToolCall {
                call_id: input.parent_call_id.clone(),
            };
            if session.approvals.iter().any(|a| {
                a.attempt_id == lease.attempt_id
                    && a.input.target == target
                    && a.input.task_id != input.task_id
            }) || session
                .executions
                .iter()
                .any(|e| e.parent_call_id == input.parent_call_id && e.task_id != input.task_id)
            {
                return Err(invalid(
                    "Execution cannot change its parent's trusted task binding",
                ));
            }
            let execution = ExecutionRecord {
                execution_id: input.execution_id,
                parent_call_id: input.parent_call_id,
                task_id: input.task_id,
                source_hash: input.source_hash,
                lease: lease.clone(),
                state: ExecutionState::Running,
                terminal: None,
            };
            session.executions.push(execution.clone());
            Ok((
                execution.clone(),
                Some(lease.clone()),
                CoreEvent::ExecutionStarted { execution },
            ))
        })
        .map(|(execution, _)| execution)
    }

    pub fn yield_execution(
        &self,
        lease: &AttemptLease,
        execution_id: &str,
    ) -> Result<EventRecord, CoreError> {
        self.mutate(&lease.session_id, |session| {
            let execution = session.verify_execution(lease, execution_id)?;
            if execution.state != ExecutionState::Running {
                return Err(invalid("Only a running execution may yield"));
            }
            let execution = session
                .executions
                .iter_mut()
                .find(|e| e.execution_id == execution_id)
                .expect("verified execution");
            execution.state = ExecutionState::Yielded;
            Ok((
                (),
                Some(lease.clone()),
                CoreEvent::ExecutionYielded {
                    execution: execution.clone(),
                },
            ))
        })
        .map(|(_, event)| event)
    }

    pub fn plan_capability(
        &self,
        lease: &AttemptLease,
        input: CapabilityInput,
    ) -> Result<CapabilityCallRecord, CoreError> {
        let input = checked_input(input)?;
        validate_key(&input.key)?;
        identifier(&input.name, "capability name")?;
        self.mutate(&lease.session_id, |session| {
            session.admit_work(lease)?;
            if session.capabilities.len() >= MAX_CAPABILITY_CALLS {
                return Err(invalid("Capability count limit exceeded"));
            }
            let execution = session.verify_execution(lease, &input.key.execution_id)?;
            let next = session
                .capabilities
                .iter()
                .filter(|c| c.key.execution_id == input.key.execution_id)
                .count() as u64;
            if input.key.sequence != next {
                return Err(invalid(
                    "Capability sequence must be unique and contiguous from zero",
                ));
            }
            let call = CapabilityCallRecord {
                key: input.key,
                name: input.name,
                arguments: input.arguments,
                parent_call_id: execution.parent_call_id.clone(),
                task_id: execution.task_id.clone(),
                source_hash: execution.source_hash.clone(),
                lease: lease.clone(),
                stage: CapabilityStage::Planned,
                authorization: None,
                outcome: None,
                external_provenance: None,
            };
            session.capabilities.push(call.clone());
            Ok((
                call.clone(),
                Some(lease.clone()),
                CoreEvent::CapabilityPlanned { call },
            ))
        })
        .map(|(call, _)| call)
    }

    /// authorization must be a serialized CapabilityAuthorization from the host.
    pub fn start_capability(
        &self,
        lease: &AttemptLease,
        key: &CapabilityCallKey,
        authorization: Value,
    ) -> Result<EventRecord, CoreError> {
        let authorization = checked_input(authorization)?;
        let proof: CapabilityAuthorization = serde_json::from_value(authorization.clone())?;
        identifier(&proof.actor, "authorization actor")?;
        identifier(&proof.task_id, "task_id")?;
        identifier(&proof.capability, "capability")?;
        nonempty(&proof.scope, "scope")?;
        if let Some(id) = &proof.approval_id {
            identifier(id, "approval_id")?;
        }
        if proof.target != *key || proof.decision != ApprovalDecision::Approve {
            return Err(CoreError::ApprovalDenied);
        }
        self.mutate(&lease.session_id, |session| {
            session.admit_work(lease)?;
            let call = session.verify_capability(lease, key)?;
            if call.stage != CapabilityStage::Planned
                || call.authorization.is_some()
                || call.outcome.is_some()
            {
                return Err(invalid(
                    "Capability may cross its execution boundary only once",
                ));
            }
            if proof.task_id != call.task_id || proof.capability != call.name {
                return Err(CoreError::ApprovalDenied);
            }
            let target = ApprovalTarget::CapabilityCall { key: key.clone() };
            let input = ApprovalInput {
                task_id: call.task_id.clone(),
                target: target.clone(),
                capability: call.name.clone(),
                scope: proof.scope.clone(),
                reason: "host authorization".into(),
            };
            if session.denied_scope(&input) {
                return Err(CoreError::ApprovalDenied);
            }
            let approvals: Vec<_> = session
                .approvals
                .iter()
                .filter(|a| a.attempt_id == lease.attempt_id && a.input.target == target)
                .collect();
            if approvals.iter().any(|a| a.decision.is_none()) {
                return Err(CoreError::PendingApproval);
            }
            if !approvals.is_empty() || proof.approval_id.is_some() {
                let receipt = approvals
                    .iter()
                    .find(|a| Some(&a.id) == proof.approval_id.as_ref())
                    .ok_or(CoreError::ApprovalDenied)?;
                if receipt.input.task_id != proof.task_id
                    || receipt.input.capability != proof.capability
                    || receipt.input.scope != proof.scope
                    || !receipt
                        .decision
                        .as_ref()
                        .is_some_and(|d| d.decision == ApprovalDecision::Approve)
                {
                    return Err(CoreError::ApprovalDenied);
                }
            }
            let call = session.capability_mut(key)?;
            call.stage = CapabilityStage::Started;
            call.authorization = Some(authorization);
            call.external_provenance = proof.external_provenance;
            Ok((
                (),
                Some(lease.clone()),
                CoreEvent::CapabilityStarted { call: call.clone() },
            ))
        })
        .map(|(_, event)| event)
    }

    /// Terminals are accepted for admitted work even after AgentEnd or an unknown
    /// sibling result. They still require the original complete current lease.
    pub fn settle_capability(
        &self,
        lease: &AttemptLease,
        key: &CapabilityCallKey,
        outcome: CapabilityOutcome,
    ) -> Result<EventRecord, CoreError> {
        let outcome = checked_input(outcome)?;
        validate_outcome(&outcome)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load(&transaction, &lease.session_id)?;
        let call = session.verify_capability(lease, key)?;
        if call.stage == CapabilityStage::Settled || call.outcome.is_some() {
            return Err(invalid("Capability already has a terminal"));
        }
        if call.stage == CapabilityStage::Planned
            && !matches!(outcome, CapabilityOutcome::NotExecuted { .. })
        {
            return Err(invalid(
                "A planned capability can only settle as NotExecuted",
            ));
        }
        let target = ApprovalTarget::CapabilityCall { key: key.clone() };
        for index in 0..session.approvals.len() {
            let approval = &mut session.approvals[index];
            if approval.input.target != target || approval.decision.is_some() {
                continue;
            }
            // A terminal nonexecution decision also closes its prompts. The
            // denial and capability terminal must succeed or roll back together.
            approval.decision = Some(ApprovalResolution {
                decision: ApprovalDecision::Deny,
                actor: "host-capability-cancellation".into(),
                decided_at: timestamp_ms(),
            });
            let approval = approval.clone();
            persist(
                &transaction,
                &mut session,
                Some(lease.clone()),
                CoreEvent::ApprovalDecided { approval },
            )?;
        }
        let call = session.capability_mut(key)?;
        call.stage = CapabilityStage::Settled;
        call.outcome = Some(outcome);
        let call = call.clone();
        let event = persist(
            &transaction,
            &mut session,
            Some(lease.clone()),
            CoreEvent::CapabilitySettled { call },
        )?;
        transaction.commit()?;
        Ok(event)
    }

    pub fn finish_execution(
        &self,
        lease: &AttemptLease,
        execution_id: &str,
        outcome: ExecutionOutcome,
    ) -> Result<EventRecord, CoreError> {
        let outcome = checked_input(outcome)?;
        let state = match &outcome {
            ExecutionOutcome::Completed { .. } => ExecutionState::Completed,
            ExecutionOutcome::Failed { error } => {
                nonempty(error, "error")?;
                ExecutionState::Failed
            }
            ExecutionOutcome::Cancelled { reason } => {
                nonempty(reason, "reason")?;
                ExecutionState::Cancelled
            }
            ExecutionOutcome::Interrupted { reason } => {
                nonempty(reason, "reason")?;
                ExecutionState::Interrupted
            }
        };
        self.mutate(&lease.session_id, |session| {
            session.verify_execution(lease, execution_id)?;
            if session
                .capabilities
                .iter()
                .any(|c| c.key.execution_id == execution_id && c.stage != CapabilityStage::Settled)
            {
                return Err(invalid("Execution still has unfinished capabilities"));
            }
            if state == ExecutionState::Completed
                && session.capabilities.iter().any(|c| {
                    c.key.execution_id == execution_id
                        && matches!(c.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. }))
                })
            {
                return Err(CoreError::NeedsReconciliation);
            }
            let execution = session
                .executions
                .iter_mut()
                .find(|e| e.execution_id == execution_id)
                .expect("verified execution");
            execution.state = state;
            execution.terminal = Some(outcome);
            Ok((
                (),
                Some(lease.clone()),
                CoreEvent::ExecutionFinished {
                    execution: execution.clone(),
                },
            ))
        })
        .map(|(_, event)| event)
    }

    pub fn reconcile_capability(
        &self,
        session_id: &str,
        reconciliation: CapabilityReconciliation,
    ) -> Result<SessionSnapshot, CoreError> {
        let reconciliation = checked_input(reconciliation)?;
        validate_key(&reconciliation.key)?;
        identifier(&reconciliation.actor, "actor")?;
        nonempty(&reconciliation.evidence, "evidence")?;
        validate_outcome(&reconciliation.outcome)?;
        if matches!(
            reconciliation.outcome,
            CapabilityOutcome::OutcomeUnknown { .. }
        ) {
            return Err(invalid("Reconciliation requires a known outcome"));
        }
        self.verify_reconciliation(&ReconciliationRequest::Capability {
            reconciliation: reconciliation.clone(),
        })?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load(&transaction, session_id)?;
        if session.attempt.is_some() {
            return Err(CoreError::Busy);
        }
        let execution = session.execution(&reconciliation.key.execution_id)?;
        let call = session.capability(&reconciliation.key)?;
        if call.parent_call_id != execution.parent_call_id
            || call.lease != execution.lease
            || call.task_id != execution.task_id
            || call.source_hash != execution.source_hash
            || call.lease.session_id != session.id
            || *reconciliation.expected != ReconciliationOrigin::from(call)
        {
            return Err(invalid(
                "Reconciliation differs from the complete original capability identity",
            ));
        }
        if call.external_provenance != reconciliation.external_provenance {
            return Err(invalid(
                "Reconciliation must match the external provenance bound at execution start",
            ));
        }
        if let Some(prior) = reconciliation_events(&transaction, &session)?
            .into_iter()
            .find_map(|event| match event {
                CoreEvent::CapabilityReconciled {
                    reconciliation: prior,
                    ..
                } if prior.key == reconciliation.key => Some(prior),
                _ => None,
            })
        {
            if prior != reconciliation {
                return Err(invalid("Reconciliation contradicts the recorded decision"));
            }
            return Ok(session.snapshot());
        }
        if call.stage != CapabilityStage::Settled
            || !matches!(call.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. }))
        {
            return Err(invalid(
                "Only an unknown capability outcome can be reconciled",
            ));
        }
        let call = session.capability_mut(&reconciliation.key)?;
        call.outcome = Some(reconciliation.outcome.clone());
        let call = call.clone();
        persist(
            &transaction,
            &mut session,
            None,
            CoreEvent::CapabilityReconciled {
                reconciliation,
                call,
            },
        )?;
        transaction.commit()?;
        Ok(session.snapshot())
    }
}
