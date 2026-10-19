use super::*;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalV1 {
    task_id: String,
    call_id: Option<String>,
    capability: String,
    scope: String,
    reason: String,
}

/// Version 1 is decoded only here, never by the running public API. Validate the
/// complete replacement first. DDL, projections, events and version change share
/// the caller's IMMEDIATE transaction, including rollback on any SQLite failure.
pub(super) fn migrate_v1(connection: &Connection) -> Result<(), CoreError> {
    let mut sessions = vec![];
    {
        let mut query =
            connection.prepare("SELECT id, projection FROM core_sessions ORDER BY id")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, json) = row?;
            let mut projection: Value = serde_json::from_str(&json)?;
            let object = projection
                .as_object_mut()
                .ok_or_else(|| invalid("Invalid v1 projection"))?;
            if object.contains_key("executions") || object.contains_key("capabilities") {
                return Err(CoreError::MigrationRequired(
                    "v1 projection already contains unversioned background execution records"
                        .into(),
                ));
            }
            let approvals = object
                .get_mut("approvals")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| invalid("v1 projection has no approvals array"))?;
            for approval in approvals {
                migrate_approval(approval)?;
            }
            if object.contains_key("execution_waits") {
                return Err(CoreError::MigrationRequired(
                    "v1 projection contains unversioned wait bindings".into(),
                ));
            }
            object.insert("executions".into(), serde_json::json!([]));
            object.insert("execution_waits".into(), serde_json::json!([]));
            object.insert("capabilities".into(), serde_json::json!([]));
            let session: StoredSession = serde_json::from_value(projection)?;
            if session.id != id
                || session.cursor > i64::MAX as u64
                || session.fence > i64::MAX as u64
                || session
                    .attempt
                    .as_ref()
                    .is_some_and(|a| a.session_id != id || a.fence != session.fence)
            {
                return Err(CoreError::CorruptStore(
                    "v1 projection identity or fence is inconsistent".into(),
                ));
            }
            encode_readable(&session)?;
            sessions.push(session);
        }
    }
    let mut migrated_events = vec![];
    let mut event_count = 0_u64;
    for session in &sessions {
        let mut query = connection.prepare(
            "SELECT cursor, record FROM core_events WHERE session_id=?1 ORDER BY cursor",
        )?;
        let rows = query.query_map([&session.id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut cursor = 0;
        let mut approvals = BTreeMap::new();
        for row in rows {
            let (stored_cursor, json) = row?;
            let mut raw: Value = serde_json::from_str(&json)?;
            if raw.get("protocol_version").and_then(Value::as_u64) != Some(1) {
                return Err(CoreError::CorruptStore(
                    "v1 event has an incompatible envelope".into(),
                ));
            }
            let event = raw
                .get_mut("event")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| invalid("Invalid v1 event"))?;
            match event.get("type").and_then(Value::as_str) {
                Some("host") => {
                    let kind = event
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if validate_host_kind(&kind).is_err()
                        || ((kind.contains("capability") || kind.contains("execution"))
                            && (kind.contains("start")
                                || kind.contains("plan")
                                || kind.contains("yield")
                                || kind.contains("finish")
                                || kind.contains("settle")
                                || kind.contains("complete")
                                || kind.contains("reconcil")
                                || kind.contains("outcome_unknown")))
                    {
                        return Err(CoreError::MigrationRequired(format!(
                            "session {} cursor {stored_cursor} contains a legacy execution fact ({kind}); the host must reconstruct execution origins and known outcomes before migration",
                            session.id
                        )));
                    }
                }
                Some(
                    "execution_started"
                    | "execution_yielded"
                    | "execution_wait_registered"
                    | "execution_finished"
                    | "capability_planned"
                    | "capability_started"
                    | "capability_settled"
                    | "capability_reconciled",
                ) => {
                    return Err(CoreError::MigrationRequired(format!(
                        "session {} cursor {stored_cursor} contains unversioned execution facts; the host must reconstruct their projection before migration",
                        session.id
                    )));
                }
                Some("approval_requested" | "approval_decided") => {
                    migrate_approval(
                        event
                            .get_mut("approval")
                            .ok_or_else(|| invalid("Missing v1 approval"))?,
                    )?;
                }
                Some("attempt_recovered") => {
                    if event.contains_key("executions") || event.contains_key("capabilities") {
                        return Err(CoreError::MigrationRequired(
                            "v1 recovery contains unversioned execution facts".into(),
                        ));
                    }
                    event.insert("executions".into(), serde_json::json!([]));
                    event.insert("capabilities".into(), serde_json::json!([]));
                }
                _ => {}
            }
            raw["protocol_version"] = Value::from(PROTOCOL_VERSION);
            let record: EventRecord = serde_json::from_value(raw)?;
            cursor = increment(cursor)?;
            if record.cursor != cursor
                || stored_cursor < 1
                || stored_cursor as u64 != cursor
                || record.session_id != session.id
                || record
                    .attempt
                    .as_ref()
                    .is_some_and(|a| a.session_id != session.id)
            {
                return Err(CoreError::CorruptStore(
                    "v1 event cursor or identity is inconsistent".into(),
                ));
            }
            match &record.event {
                CoreEvent::ApprovalRequested { approval } => {
                    validate_approval(session, approval)?;
                    if approval.decision.is_some()
                        || approvals
                            .insert(approval.id.clone(), approval.clone())
                            .is_some()
                    {
                        return Err(CoreError::CorruptStore(
                            "v1 approval request was duplicated or already decided".into(),
                        ));
                    }
                }
                CoreEvent::ApprovalDecided { approval } => {
                    validate_approval(session, approval)?;
                    let previous = approvals.get_mut(&approval.id).ok_or_else(|| {
                        CoreError::CorruptStore("v1 approval decision has no request".into())
                    })?;
                    let mut expected = previous.clone();
                    expected.decision = approval.decision.clone();
                    if previous.decision.is_some()
                        || approval.decision.is_none()
                        || expected != *approval
                    {
                        return Err(CoreError::CorruptStore(
                            "v1 approval decision changed immutable request facts".into(),
                        ));
                    }
                    *previous = approval.clone();
                }
                _ => {}
            }
            encode_readable(&record)?;
            migrated_events.push(record);
            event_count = increment(event_count)?;
        }
        if cursor != session.cursor {
            return Err(CoreError::CorruptStore(
                "v1 projection cursor differs from its log".into(),
            ));
        }
        if session.approvals.len() != approvals.len()
            || session
                .approvals
                .iter()
                .any(|a| approvals.get(&a.id) != Some(a))
        {
            return Err(CoreError::CorruptStore(
                "v1 approval projection differs from its log".into(),
            ));
        }
    }
    let actual_count: i64 =
        connection.query_row("SELECT count(*) FROM core_events", [], |row| row.get(0))?;
    if actual_count < 0 || actual_count as u64 != event_count {
        return Err(CoreError::CorruptStore(
            "v1 event log contains an unknown session".into(),
        ));
    }
    // Normal writes never disable append-only guards. This one-time canonical
    // rewrite retains every cursor, timestamp, lease and known fact.
    connection.execute_batch("DROP TRIGGER core_events_no_update;")?;
    for record in migrated_events {
        connection.execute(
            "UPDATE core_events SET record=?3 WHERE session_id=?1 AND cursor=?2",
            params![
                record.session_id,
                record.cursor as i64,
                encode_readable(&record)?
            ],
        )?;
    }
    for session in sessions {
        connection.execute(
            "UPDATE core_sessions SET projection=?2 WHERE id=?1",
            params![session.id, encode_readable(&session)?],
        )?;
    }
    connection.execute_batch("CREATE TRIGGER core_events_no_update BEFORE UPDATE ON core_events BEGIN SELECT RAISE(ABORT, 'core events are append-only'); END;")?;
    connection.execute("UPDATE core_metadata SET format=?1", [STORAGE_FORMAT])?;
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// V2 has independent execution/capability facts, but no trusted wait binding.
/// Preserve those facts verbatim; never derive new bindings from model arguments.
/// All replacements and DDL run inside open()'s single IMMEDIATE transaction.
pub(super) fn migrate_v2(connection: &Connection) -> Result<(), CoreError> {
    let mut sessions = vec![];
    {
        let mut query =
            connection.prepare("SELECT id, projection FROM core_sessions ORDER BY id")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, json) = row?;
            let mut raw: Value = serde_json::from_str(&json)?;
            let object = raw
                .as_object_mut()
                .ok_or_else(|| invalid("Invalid v2 projection"))?;
            if object.contains_key("execution_waits") {
                return Err(CoreError::MigrationRequired(
                    "v2 projection contains unversioned wait bindings".into(),
                ));
            }
            object.insert("execution_waits".into(), serde_json::json!([]));
            let session: StoredSession = serde_json::from_value(raw)?;
            if session.id != id
                || session.cursor > i64::MAX as u64
                || session.fence > i64::MAX as u64
                || session
                    .attempt
                    .as_ref()
                    .is_some_and(|a| a.session_id != id || a.fence != session.fence)
            {
                return Err(CoreError::CorruptStore(
                    "v2 projection identity or fence is inconsistent".into(),
                ));
            }
            encode_readable(&session)?;
            sessions.push(session);
        }
    }
    let mut events = vec![];
    for session in &sessions {
        let mut query = connection.prepare(
            "SELECT cursor, record FROM core_events WHERE session_id=?1 ORDER BY cursor",
        )?;
        let rows = query.query_map([&session.id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut cursor = 0;
        let mut facts = V2Facts::default();
        for row in rows {
            let (stored_cursor, json) = row?;
            let mut raw: Value = serde_json::from_str(&json)?;
            if raw.get("protocol_version").and_then(Value::as_u64) != Some(2) {
                return Err(CoreError::CorruptStore(
                    "v2 event has an incompatible envelope".into(),
                ));
            }
            if raw["event"]["type"].as_str() == Some("execution_wait_registered") {
                return Err(CoreError::MigrationRequired(
                    "v2 log contains unversioned wait bindings".into(),
                ));
            }
            raw["protocol_version"] = Value::from(PROTOCOL_VERSION);
            let record: EventRecord = serde_json::from_value(raw)?;
            cursor = increment(cursor)?;
            if record.session_id != session.id
                || record.cursor != cursor
                || stored_cursor < 1
                || stored_cursor as u64 != cursor
                || record
                    .attempt
                    .as_ref()
                    .is_some_and(|a| a.session_id != session.id)
            {
                return Err(CoreError::CorruptStore(
                    "v2 event cursor or identity is inconsistent".into(),
                ));
            }
            // Validate caller values before equality checks, clones or replay.
            encode_readable(&record)?;
            facts.apply(&record)?;
            events.push(record);
        }
        facts.validate_projection(session)?;
        if cursor != session.cursor {
            return Err(CoreError::CorruptStore(
                "v2 projection cursor differs from its log".into(),
            ));
        }
    }
    let actual: i64 =
        connection.query_row("SELECT count(*) FROM core_events", [], |row| row.get(0))?;
    if actual < 0 || actual as usize != events.len() {
        return Err(CoreError::CorruptStore(
            "v2 log contains an unknown session".into(),
        ));
    }
    connection.execute_batch("DROP TRIGGER core_events_no_update;")?;
    for record in events {
        connection.execute(
            "UPDATE core_events SET record=?3 WHERE session_id=?1 AND cursor=?2",
            params![
                record.session_id,
                record.cursor as i64,
                encode_readable(&record)?
            ],
        )?;
    }
    for session in sessions {
        connection.execute(
            "UPDATE core_sessions SET projection=?2 WHERE id=?1",
            params![session.id, encode_readable(&session)?],
        )?;
    }
    connection.execute_batch("CREATE TRIGGER core_events_no_update BEFORE UPDATE ON core_events BEGIN SELECT RAISE(ABORT, 'core events are append-only'); END;")?;
    connection.execute("UPDATE core_metadata SET format=?1", [STORAGE_FORMAT])?;
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Replay only the v2 execution/permission contract. Model messages cannot
/// manufacture execution or wait authority. Keep the complete last record, not
/// just its origin, so a projection cannot erase unknown effects or edit proof.
#[derive(Default)]
struct V2Facts {
    attempt: Option<AttemptLease>,
    executions: BTreeMap<String, ExecutionRecord>,
    capabilities: BTreeMap<CapabilityCallKey, CapabilityCallRecord>,
    approvals: BTreeMap<String, ApprovalRequest>,
}

fn v2_corrupt(message: &str) -> CoreError {
    CoreError::CorruptStore(format!("v2 {message}"))
}

impl V2Facts {
    fn verify_lease(&self, record: &EventRecord, lease: &AttemptLease) -> Result<(), CoreError> {
        if lease.session_id != record.session_id
            || record.attempt.as_ref() != Some(lease)
            || self.attempt.as_ref() != Some(lease)
        {
            return Err(v2_corrupt("event differs from its current complete lease"));
        }
        Ok(())
    }

    fn execution_origin(&self, call: &CapabilityCallRecord) -> Result<(), CoreError> {
        let execution = self
            .executions
            .get(&call.key.execution_id)
            .ok_or_else(|| v2_corrupt("capability has no execution start"))?;
        if !execution_active(execution)
            || call.lease != execution.lease
            || call.parent_call_id != execution.parent_call_id
            || call.task_id != execution.task_id
            || call.source_hash != execution.source_hash
        {
            return Err(v2_corrupt(
                "capability differs from its active execution origin",
            ));
        }
        Ok(())
    }

    fn start_proof(&self, call: &CapabilityCallRecord) -> Result<(), CoreError> {
        let proof: CapabilityAuthorization = serde_json::from_value(
            call.authorization
                .clone()
                .ok_or_else(|| v2_corrupt("capability start has no authorization"))?,
        )?;
        identifier(&proof.actor, "authorization actor")?;
        nonempty(&proof.scope, "authorization scope")?;
        if proof.target != call.key
            || proof.task_id != call.task_id
            || proof.capability != call.name
            || proof.decision != ApprovalDecision::Approve
            || proof.external_provenance != call.external_provenance
        {
            return Err(v2_corrupt(
                "capability start changed its authorization or external provenance",
            ));
        }
        let target = ApprovalTarget::CapabilityCall {
            key: call.key.clone(),
        };
        let relevant: Vec<_> = self
            .approvals
            .values()
            .filter(|a| a.attempt_id == call.lease.attempt_id && a.input.target == target)
            .collect();
        if relevant.iter().any(|a| a.decision.is_none())
            || self.approvals.values().any(|a| {
                a.input.task_id == call.task_id
                    && a.input.capability == call.name
                    && a.input.scope == proof.scope
                    && a.decision
                        .as_ref()
                        .is_some_and(|d| d.decision == ApprovalDecision::Deny)
            })
        {
            return Err(v2_corrupt(
                "capability start bypassed an undecided or denied approval",
            ));
        }
        if !relevant.is_empty() || proof.approval_id.is_some() {
            let receipt = relevant
                .iter()
                .find(|a| Some(&a.id) == proof.approval_id.as_ref())
                .ok_or_else(|| {
                    v2_corrupt("capability authorization has no exact approval receipt")
                })?;
            if receipt.input.task_id != proof.task_id
                || receipt.input.capability != proof.capability
                || receipt.input.scope != proof.scope
                || !receipt
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.decision == ApprovalDecision::Approve)
            {
                return Err(v2_corrupt(
                    "capability authorization differs from its approval receipt",
                ));
            }
        }
        Ok(())
    }

    fn apply(&mut self, record: &EventRecord) -> Result<(), CoreError> {
        match &record.event {
            CoreEvent::AttemptStarted { lease } => {
                if self.attempt.is_some()
                    || record.attempt.as_ref() != Some(lease)
                    || lease.session_id != record.session_id
                {
                    return Err(v2_corrupt("attempt start changed worker ownership"));
                }
                self.attempt = Some(lease.clone());
            }
            CoreEvent::AttemptFinished { lease } => {
                self.verify_lease(record, lease)?;
                if self.executions.values().any(execution_active)
                    || self
                        .capabilities
                        .values()
                        .any(|c| c.stage != CapabilityStage::Settled)
                    || self.approvals.values().any(|a| a.decision.is_none())
                {
                    return Err(v2_corrupt(
                        "attempt finished with active execution or approval facts",
                    ));
                }
                self.attempt = None;
            }
            CoreEvent::ExecutionStarted { execution } => {
                self.verify_lease(record, &execution.lease)?;
                for (value, field) in [
                    (&execution.execution_id, "execution_id"),
                    (&execution.parent_call_id, "parent_call_id"),
                    (&execution.task_id, "task_id"),
                    (&execution.source_hash, "source_hash"),
                ] {
                    identifier(value, field)?;
                }
                if execution.state != ExecutionState::Running
                    || execution.terminal.is_some()
                    || self.executions.contains_key(&execution.execution_id)
                {
                    return Err(v2_corrupt(
                        "execution start is duplicated or already terminal",
                    ));
                }
                self.executions
                    .insert(execution.execution_id.clone(), execution.clone());
            }
            CoreEvent::ExecutionYielded { execution }
            | CoreEvent::ExecutionFinished { execution } => {
                self.verify_lease(record, &execution.lease)?;
                let previous = self
                    .executions
                    .get(&execution.execution_id)
                    .ok_or_else(|| v2_corrupt("execution transition has no start"))?;
                if !execution_active(previous) {
                    return Err(v2_corrupt("execution transition follows a terminal"));
                }
                let mut expected = previous.clone();
                if matches!(record.event, CoreEvent::ExecutionYielded { .. }) {
                    if previous.state != ExecutionState::Running {
                        return Err(v2_corrupt("only a running execution can yield"));
                    }
                    expected.state = ExecutionState::Yielded;
                } else {
                    expected.state = match &execution.terminal {
                        Some(ExecutionOutcome::Completed { .. }) => ExecutionState::Completed,
                        Some(ExecutionOutcome::Failed { error }) => {
                            nonempty(error, "error")?;
                            ExecutionState::Failed
                        }
                        Some(ExecutionOutcome::Cancelled { reason }) => {
                            nonempty(reason, "reason")?;
                            ExecutionState::Cancelled
                        }
                        Some(ExecutionOutcome::Interrupted { reason }) => {
                            nonempty(reason, "reason")?;
                            ExecutionState::Interrupted
                        }
                        None => return Err(v2_corrupt("execution finish has no outcome")),
                    };
                    expected.terminal = execution.terminal.clone();
                    if self.capabilities.values().any(|c| {
                        c.key.execution_id == execution.execution_id
                            && (c.stage != CapabilityStage::Settled
                                || (expected.state == ExecutionState::Completed
                                    && matches!(
                                        c.outcome,
                                        Some(CapabilityOutcome::OutcomeUnknown { .. })
                                    )))
                    }) {
                        return Err(v2_corrupt(
                            "execution finished before its child outcomes were settled",
                        ));
                    }
                }
                if expected != *execution {
                    return Err(v2_corrupt(
                        "execution transition changed its origin, state or terminal",
                    ));
                }
                self.executions
                    .insert(execution.execution_id.clone(), execution.clone());
            }
            CoreEvent::CapabilityPlanned { call } => {
                self.verify_lease(record, &call.lease)?;
                self.execution_origin(call)?;
                identifier(&call.name, "capability name")?;
                let next = self
                    .capabilities
                    .keys()
                    .filter(|key| key.execution_id == call.key.execution_id)
                    .count() as u64;
                if call.key.sequence != next
                    || self.capabilities.contains_key(&call.key)
                    || call.stage != CapabilityStage::Planned
                    || call.authorization.is_some()
                    || call.outcome.is_some()
                    || call.external_provenance.is_some()
                {
                    return Err(v2_corrupt(
                        "capability plan is duplicated, out of sequence or already started",
                    ));
                }
                self.capabilities.insert(call.key.clone(), call.clone());
            }
            CoreEvent::CapabilityStarted { call } | CoreEvent::CapabilitySettled { call } => {
                self.verify_lease(record, &call.lease)?;
                self.execution_origin(call)?;
                // Some valid v2 logs first recorded a child at its execution
                // boundary. That start itself is the canonical origin; later
                // records must preserve its complete authorization and proof.
                let initial;
                let previous = match self.capabilities.get(&call.key) {
                    Some(previous) => previous,
                    None if matches!(record.event, CoreEvent::CapabilityStarted { .. }) => {
                        let next = self
                            .capabilities
                            .keys()
                            .filter(|key| key.execution_id == call.key.execution_id)
                            .count() as u64;
                        if call.key.sequence != next {
                            return Err(v2_corrupt("initial capability start is out of sequence"));
                        }
                        identifier(&call.name, "capability name")?;
                        initial = CapabilityCallRecord {
                            stage: CapabilityStage::Planned,
                            authorization: None,
                            external_provenance: None,
                            outcome: None,
                            ..call.clone()
                        };
                        &initial
                    }
                    None => return Err(v2_corrupt("capability terminal has no prior fact")),
                };
                let mut expected = previous.clone();
                if matches!(record.event, CoreEvent::CapabilityStarted { .. }) {
                    if previous.stage != CapabilityStage::Planned {
                        return Err(v2_corrupt("capability started more than once"));
                    }
                    self.start_proof(call)?;
                    expected.stage = CapabilityStage::Started;
                    expected.authorization = call.authorization.clone();
                    expected.external_provenance = call.external_provenance.clone();
                } else {
                    let outcome = call
                        .outcome
                        .as_ref()
                        .ok_or_else(|| v2_corrupt("capability terminal has no outcome"))?;
                    if previous.stage == CapabilityStage::Settled
                        || (previous.stage == CapabilityStage::Planned
                            && !matches!(outcome, CapabilityOutcome::NotExecuted { .. }))
                    {
                        return Err(v2_corrupt("capability terminal has an illegal prior stage"));
                    }
                    expected.stage = CapabilityStage::Settled;
                    expected.outcome = Some(outcome.clone());
                }
                if expected != *call {
                    return Err(v2_corrupt(
                        "capability transition changed its origin, authorization or outcome",
                    ));
                }
                self.capabilities.insert(call.key.clone(), call.clone());
            }
            CoreEvent::CapabilityReconciled {
                reconciliation,
                call,
            } => {
                if self.attempt.is_some() || record.attempt.is_some() {
                    return Err(v2_corrupt(
                        "capability reconciliation occurred during an attempt",
                    ));
                }
                identifier(&reconciliation.actor, "actor")?;
                nonempty(&reconciliation.evidence, "evidence")?;
                let previous = self
                    .capabilities
                    .get(&call.key)
                    .ok_or_else(|| v2_corrupt("reconciled capability has no plan"))?;
                if previous.stage != CapabilityStage::Settled
                    || !matches!(
                        previous.outcome,
                        Some(CapabilityOutcome::OutcomeUnknown { .. })
                    )
                    || reconciliation.key != call.key
                    || matches!(
                        reconciliation.outcome,
                        CapabilityOutcome::OutcomeUnknown { .. }
                    )
                    || reconciliation.external_provenance != previous.external_provenance
                {
                    return Err(v2_corrupt(
                        "reconciliation changed provenance or did not resolve an unknown outcome",
                    ));
                }
                let mut expected = previous.clone();
                expected.outcome = Some(reconciliation.outcome.clone());
                if expected != *call {
                    return Err(v2_corrupt(
                        "reconciliation changed the original capability facts",
                    ));
                }
                self.capabilities.insert(call.key.clone(), call.clone());
            }
            CoreEvent::AttemptRecovered {
                lease,
                executions,
                capabilities,
                ..
            } => {
                if *lease != self.attempt || record.attempt != *lease {
                    return Err(v2_corrupt("recovery changed its original attempt"));
                }
                let expected_executions: BTreeSet<_> = self
                    .executions
                    .values()
                    .filter(|e| execution_active(e))
                    .map(|e| e.execution_id.clone())
                    .collect();
                let expected_capabilities: BTreeSet<_> = self
                    .capabilities
                    .values()
                    .filter(|c| c.stage != CapabilityStage::Settled)
                    .map(|c| c.key.clone())
                    .collect();
                let mut seen_executions = BTreeSet::new();
                for execution in executions {
                    let previous = self
                        .executions
                        .get(&execution.execution_id)
                        .ok_or_else(|| v2_corrupt("recovered execution has no start"))?;
                    let Some(ExecutionOutcome::Interrupted { reason }) = &execution.terminal else {
                        return Err(v2_corrupt("recovery invented a known execution outcome"));
                    };
                    nonempty(reason, "reason")?;
                    let mut expected = previous.clone();
                    expected.state = ExecutionState::Interrupted;
                    expected.terminal = execution.terminal.clone();
                    if expected != *execution
                        || !expected_executions.contains(&execution.execution_id)
                        || !seen_executions.insert(execution.execution_id.clone())
                    {
                        return Err(v2_corrupt("recovery changed or duplicated an execution"));
                    }
                    self.executions
                        .insert(execution.execution_id.clone(), execution.clone());
                }
                let mut seen_capabilities = BTreeSet::new();
                for call in capabilities {
                    let previous = self
                        .capabilities
                        .get(&call.key)
                        .ok_or_else(|| v2_corrupt("recovered capability has no plan"))?;
                    if !matches!(
                        (previous.stage, &call.outcome),
                        (
                            CapabilityStage::Planned,
                            Some(CapabilityOutcome::NotExecuted { .. })
                        ) | (
                            CapabilityStage::Started,
                            Some(CapabilityOutcome::OutcomeUnknown { .. })
                        )
                    ) {
                        return Err(v2_corrupt("recovery invented a known capability outcome"));
                    }
                    let mut expected = previous.clone();
                    expected.stage = CapabilityStage::Settled;
                    expected.outcome = call.outcome.clone();
                    if expected != *call || !seen_capabilities.insert(call.key.clone()) {
                        return Err(v2_corrupt(
                            "recovery changed or duplicated capability facts",
                        ));
                    }
                    self.capabilities.insert(call.key.clone(), call.clone());
                }
                if seen_executions != expected_executions
                    || seen_capabilities != expected_capabilities
                {
                    return Err(v2_corrupt(
                        "recovery omitted active execution or capability facts",
                    ));
                }
                self.attempt = None;
            }
            CoreEvent::ApprovalRequested { approval } => {
                let lease = self
                    .attempt
                    .as_ref()
                    .ok_or_else(|| v2_corrupt("approval request has no attempt"))?;
                self.verify_lease(record, lease)?;
                if approval.session_id != record.session_id
                    || approval.attempt_id != lease.attempt_id
                    || approval.decision.is_some()
                    || self.approvals.contains_key(&approval.id)
                {
                    return Err(v2_corrupt(
                        "approval request is duplicated, decided or has a different origin",
                    ));
                }
                if let ApprovalTarget::CapabilityCall { key } = &approval.input.target {
                    let call = self
                        .capabilities
                        .get(key)
                        .ok_or_else(|| v2_corrupt("approval target has no planned capability"))?;
                    self.execution_origin(call)?;
                    if call.lease != *lease
                        || call.stage != CapabilityStage::Planned
                        || approval.input.task_id != call.task_id
                        || approval.input.capability != call.name
                    {
                        return Err(v2_corrupt(
                            "approval request changed its exact capability target",
                        ));
                    }
                }
                self.approvals.insert(approval.id.clone(), approval.clone());
            }
            CoreEvent::ApprovalDecided { approval } => {
                let previous = self
                    .approvals
                    .get(&approval.id)
                    .ok_or_else(|| v2_corrupt("approval decision has no request"))?;
                let mut expected = previous.clone();
                expected.decision = approval.decision.clone();
                if previous.decision.is_some()
                    || approval.decision.is_none()
                    || record.attempt != self.attempt
                    || expected != *approval
                {
                    return Err(v2_corrupt(
                        "approval decision changed its immutable request or prior decision",
                    ));
                }
                self.approvals.insert(approval.id.clone(), approval.clone());
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_projection(&self, session: &StoredSession) -> Result<(), CoreError> {
        let mut executions = BTreeMap::new();
        for execution in &session.executions {
            if executions
                .insert(execution.execution_id.clone(), execution)
                .is_some()
            {
                return Err(v2_corrupt("projection has duplicate executions"));
            }
            let parent = session
                .calls
                .iter()
                .find(|c| c.record.call.id == execution.parent_call_id)
                .ok_or_else(|| v2_corrupt("execution has no original model call"))?;
            if parent.record.attempt_id != execution.lease.attempt_id
                || parent.record.call.name != "execute_code"
                || !parent.record.execution_started
            {
                return Err(v2_corrupt(
                    "execution differs from its original started model call",
                ));
            }
        }
        let mut capabilities = BTreeMap::new();
        for call in &session.capabilities {
            if capabilities.insert(call.key.clone(), call).is_some() {
                return Err(v2_corrupt("projection has duplicate capabilities"));
            }
        }
        let mut approvals = BTreeMap::new();
        for approval in &session.approvals {
            if approvals.insert(approval.id.clone(), approval).is_some() {
                return Err(v2_corrupt("projection has duplicate approvals"));
            }
            if let ApprovalTarget::ToolCall { call_id } = &approval.input.target
                && !session.calls.iter().any(|c| {
                    c.record.call.id == *call_id && c.record.attempt_id == approval.attempt_id
                })
            {
                return Err(v2_corrupt("approval differs from its original model call"));
            }
        }
        if session.attempt != self.attempt
            || executions
                != self
                    .executions
                    .iter()
                    .map(|(key, value)| (key.clone(), value))
                    .collect()
            || capabilities
                != self
                    .capabilities
                    .iter()
                    .map(|(key, value)| (key.clone(), value))
                    .collect()
            || approvals
                != self
                    .approvals
                    .iter()
                    .map(|(key, value)| (key.clone(), value))
                    .collect()
        {
            return Err(v2_corrupt(
                "execution, capability or approval projection differs from its complete log",
            ));
        }
        Ok(())
    }
}

fn migrate_approval(raw: &mut Value) -> Result<(), CoreError> {
    let input = raw
        .get_mut("input")
        .ok_or_else(|| invalid("v1 approval has no input"))?;
    let old: ApprovalV1 = serde_json::from_value(input.clone())?;
    let call_id = old.call_id.ok_or_else(|| CoreError::MigrationRequired("A v1 session-wide approval has no exact target; the host must assign verified targets before migration".into()))?;
    identifier(&call_id, "call_id")?;
    identifier(&old.task_id, "task_id")?;
    identifier(&old.capability, "capability")?;
    nonempty(&old.scope, "scope")?;
    nonempty(&old.reason, "reason")?;
    *input = serde_json::to_value(ApprovalInput {
        task_id: old.task_id,
        target: ApprovalTarget::ToolCall { call_id },
        capability: old.capability,
        scope: old.scope,
        reason: old.reason,
    })?;
    Ok(())
}

fn validate_approval(session: &StoredSession, approval: &ApprovalRequest) -> Result<(), CoreError> {
    let ApprovalTarget::ToolCall { call_id } = &approval.input.target else {
        return Err(invalid("v1 approval has an invalid target"));
    };
    if approval.session_id != session.id
        || !session
            .calls
            .iter()
            .any(|c| c.record.call.id == *call_id && c.record.attempt_id == approval.attempt_id)
    {
        return Err(CoreError::CorruptStore(
            "v1 approval origin differs from its model call".into(),
        ));
    }
    Ok(())
}
