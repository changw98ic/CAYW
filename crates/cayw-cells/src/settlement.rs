use crate::*;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

/// Host-only ownership of an original session/task. The task lock is held across
/// host-store and Cells transactions, and across awaits if the host needs them.
/// Acquiring this guard never starts a VM or creates a fake CellBackend. Device
/// shutdown remains the host's responsibility; a Core fence alone is insufficient.
/// Drop releases ownership, but does not unfreeze storage or replay any work.
/// After finish succeeds, the session VM gate is retained until this guard drops.
pub struct StoppedScope {
    journal: Arc<SqliteJournal>,
    stopped: ExecutorStopped,
    limits: CellLimits,
    _task_lock: crate::journal::JournalLock,
    session_gate: Mutex<Option<crate::journal::JournalLock>>,
}

impl SqliteJournal {
    /// Acquire this before changing Core facts. A live manager (even one that
    /// only loaded old records) rejects this claim. Stop/join and drop it first.
    /// Persist Core facts, read them back, then submit those facts through settle.
    pub fn stopped_scope(
        self: &Arc<Self>,
        stopped: ExecutorStopped,
        limits: CellLimits,
    ) -> Result<StoppedScope, CellError> {
        stopped.identity.validate()?;
        limits.validate()?;
        reference(&stopped.evidence)?;
        let task_lock = self.claim(&stopped.identity)?;
        // Freeze before returning the cross-store guard. Any failure or caller
        // crash between the two databases therefore leaves execution blocked.
        let connection = self.connection.lock().expect("journal lock poisoned");
        connection.execute(
            "INSERT OR IGNORE INTO cells_frozen_v1(session) VALUES(?1)",
            [&stopped.identity.session_id],
        )?;
        drop(connection);
        Ok(StoppedScope {
            journal: self.clone(),
            stopped,
            limits,
            _task_lock: task_lock,
            session_gate: Mutex::new(None),
        })
    }
}

impl StoppedScope {
    pub fn identity(&self) -> &CellIdentity {
        &self.stopped.identity
    }

    /// Includes earlier attempts in the same session/task. Settlement still
    /// requires the complete, exact original CellExecution for each record.
    pub fn records(&self) -> Result<Vec<CellRecord>, CellError> {
        self.journal.records(self.identity(), &self.limits)
    }

    pub fn record(&self, execution: &CellExecution) -> Result<CellRecord, CellError> {
        execution.identity.validate()?;
        if execution.identity.session_id != self.identity().session_id
            || execution.identity.task_id != self.identity().task_id
        {
            return Err(CellError::Conflict);
        }
        let connection = self
            .journal
            .connection
            .lock()
            .expect("journal lock poisoned");
        let body: String = connection
            .query_row(
                "SELECT body FROM cells_v1 WHERE id=?1 AND session=?2 AND task=?3",
                params![
                    execution.cell_id,
                    self.identity().session_id,
                    self.identity().task_id
                ],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(CellError::NotFound)?;
        let record: CellRecord = serde_json::from_str(&body)?;
        if record.execution() != *execution || record.source_hash != source_hash(&record.source) {
            return Err(CellError::Conflict);
        }
        Ok(record)
    }

    /// Accept durable host facts for original intents. Known results are compared
    /// exactly and preserved; a previously omitted success may add its first
    /// observed digest. Neither retries nor backend repair can rewrite terminals.
    /// This commits the audit with the changes. It never thaws the read cache.
    pub fn settle(&self, settlement: HostSettlement) -> Result<CellRecord, CellError> {
        // Own unchecked JSON before any fallible lookup, so invalid identity or
        // depth cannot recursively drop a hostile/deep host-created Value.
        let settlement = SettlementInput(settlement);
        let record = self.record(&settlement.0.execution)?;
        let (record, changed) = apply_input(record, settlement, &self.stopped, &self.limits)?;
        if changed {
            self.journal.persist(&record, record.revision - 1)?;
        }
        Ok(record)
    }

    /// Thaw durable storage only when every session record is known and no VM is
    /// alive, including VMs from other tasks/connections/processes. An exclusive
    /// session VM gate is retained until this guard drops, so Runtime may now
    /// explicitly thaw ReadCoordinator while still holding the same ownership.
    /// Failures keep the session frozen and permit an idempotent retry.
    pub fn finish(&self) -> Result<(), CellError> {
        let mut gate = self
            .session_gate
            .lock()
            .expect("stopped scope gate poisoned");
        if gate.is_none() {
            *gate = Some(
                self.journal
                    .claim_session(&self.identity().session_id, true)?,
            );
        }
        self.journal
            .thaw_stopped_session(&self.identity().session_id)
    }
}

struct SettlementInput(HostSettlement);
impl Drop for SettlementInput {
    fn drop(&mut self) {
        for call in &mut self.0.calls {
            discard_json(std::mem::take(&mut call.arguments));
            if let HostCallTerminal::Succeeded {
                result: HostResult::Value(value),
            } = &mut call.terminal
            {
                discard_json(std::mem::take(value));
            }
        }
    }
}

pub(crate) fn apply_settlement(
    record: CellRecord,
    settlement: HostSettlement,
    stopped: &ExecutorStopped,
    limits: &CellLimits,
) -> Result<(CellRecord, bool), CellError> {
    apply_input(record, SettlementInput(settlement), stopped, limits)
}

fn apply_input(
    mut record: CellRecord,
    input: SettlementInput,
    stopped: &ExecutorStopped,
    limits: &CellLimits,
) -> Result<(CellRecord, bool), CellError> {
    let settlement = &input.0;
    reference(&settlement.evidence)?;
    // Validate before Eq/Clone/Serialize, which recursively traverse Values.
    // Containers accepted here are bounded to the configured Cells depth.
    for call in &settlement.calls {
        validate_json(&call.arguments, limits.json_depth, limits.item_bytes)?;
        if let Some(dependencies) = &call.dependencies {
            dependencies.validate(&record.identity.permission_context, limits)?;
        }
        if let HostCallTerminal::Succeeded {
            result: HostResult::Value(value),
        } = &call.terminal
        {
            validate_json(value, limits.json_depth, limits.item_bytes)?;
        }
    }
    if record.execution() != settlement.execution
        || record.source_hash != source_hash(&record.source)
        || record.identity.session_id != stopped.identity.session_id
        || record.identity.task_id != stopped.identity.task_id
    {
        return Err(CellError::Conflict);
    }
    if settlement.calls.len() > record.calls.len() {
        return Err(CellError::Conflict);
    }
    let already_applied = record
        .host_settlements
        .iter()
        .any(|entry| &entry.settlement == settlement);
    let mut seen = BTreeSet::new();
    for incoming in &settlement.calls {
        if !seen.insert(incoming.sequence) {
            return Err(CellError::Conflict);
        }
        validate_json(&incoming.arguments, limits.json_depth, limits.item_bytes)?;
        let index = incoming
            .sequence
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or(CellError::Conflict)?;
        let call = record.calls.get_mut(index).ok_or(CellError::Conflict)?;
        if call.sequence != incoming.sequence
            || call.name != incoming.name
            || call.arguments != incoming.arguments
        {
            return Err(CellError::Conflict);
        }
        if call.dependencies.as_ref() != incoming.dependencies.as_ref() {
            if call.dependencies.is_some()
                || !matches!(
                    call.status,
                    IntentStatus::Pending | IntentStatus::OutcomeUnknown
                )
            {
                return Err(CellError::Conflict);
            }
            call.dependencies = incoming.dependencies.clone();
        }
        settle_call(call, &incoming.terminal, limits)?;
    }
    if let Some(execution) = &settlement.execution_terminal {
        reference(&execution.evidence)?;
        if let Some(error) = &execution.terminal.error {
            validate_json(&Value::String(error.clone()), 1, limits.item_bytes)?;
        }
        if record.unresolved()
            || settlement.calls.len() != record.calls.len()
            || execution.terminal.outcome_unknown
            || !execution.terminal.status.is_terminal()
            || execution.terminal.status == CellStatus::OutcomeUnknown
        {
            return Err(CellError::Conflict);
        }
        // Retried backend repair refers to its original error. The earlier
        // committed audit is the only acceptable proof that it was cleared.
        if record.backend_error != execution.expected_backend_error && !already_applied {
            return Err(CellError::Conflict);
        }
        let terminal_error = execution.terminal.error.as_deref().map(bounded_error);
        if record.backend_error.is_none()
            && record.status.is_terminal()
            && record.status != CellStatus::OutcomeUnknown
            && (record.status != execution.terminal.status
                || record.error != terminal_error
                || record.stop != execution.terminal.stop)
            && !already_applied
        {
            return Err(CellError::Conflict);
        }
        record.backend_error = None;
        record.status = execution.terminal.status;
        record.error = terminal_error;
        record.stop = execution.terminal.stop;
    } else if !record.unresolved()
        && record.backend_error.is_none()
        && matches!(
            record.status,
            CellStatus::OutcomeUnknown | CellStatus::Running | CellStatus::Yielded
        )
    {
        record.status = CellStatus::Reconciled;
        record.error = None;
    }
    if already_applied {
        return Ok((record, false));
    }
    if record.host_settlements.len() >= record.calls.len().saturating_mul(2).saturating_add(8) {
        return Err(CellError::Capacity);
    }
    record.host_settlements.push(HostSettlementEvidence {
        stopped: stopped.clone(),
        settlement: settlement.clone(),
    });
    let previous = record.revision;
    record.revision = previous.checked_add(1).ok_or(CellError::Capacity)?;
    Ok((record, true))
}

impl SqliteJournal {
    pub(crate) fn thaw_stopped_session(&self, session: &str) -> Result<(), CellError> {
        let mut connection = self.connection.lock().expect("journal lock poisoned");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let bodies = {
            let mut statement = tx.prepare("SELECT body FROM cells_v1 WHERE session=?1")?;
            statement
                .query_map([session], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for body in bodies {
            let record: CellRecord = serde_json::from_str(&body)?;
            if record.unresolved()
                || record.backend_error.is_some()
                || matches!(
                    record.status,
                    CellStatus::Running | CellStatus::Yielded | CellStatus::OutcomeUnknown
                )
            {
                return Err(CellError::Frozen);
            }
        }
        tx.execute("DELETE FROM cells_frozen_v1 WHERE session=?1", [session])?;
        tx.commit()?;
        Ok(())
    }
}

fn reference(value: &str) -> Result<(), CellError> {
    if value.trim().is_empty() || value.len() > 1024 {
        return Err(CellError::Invalid(
            "trusted evidence requires 1..=1024 bytes".into(),
        ));
    }
    Ok(())
}

fn settle_call(
    call: &mut CapabilityIntent,
    terminal: &HostCallTerminal,
    limits: &CellLimits,
) -> Result<(), CellError> {
    let (status, result, omitted, hash, error) = match terminal {
        HostCallTerminal::Succeeded {
            result: HostResult::Value(value),
        } => {
            validate_json(value, limits.json_depth, limits.item_bytes)?;
            (
                IntentStatus::Succeeded,
                Some(value.clone()),
                false,
                Some(HostResult::value_hash(value)),
                None,
            )
        }
        HostCallTerminal::Succeeded {
            result: HostResult::Omitted { sha256 },
        } => {
            if !valid_hash(sha256) {
                return Err(CellError::Invalid(
                    "omitted result requires a SHA-256 digest".into(),
                ));
            }
            (
                IntentStatus::Succeeded,
                None,
                true,
                Some(sha256.clone()),
                None,
            )
        }
        HostCallTerminal::Failed { error } => {
            validate_json(&Value::String(error.clone()), 1, limits.item_bytes)?;
            (
                IntentStatus::Failed,
                None,
                false,
                None,
                Some(bounded_error(error)),
            )
        }
        HostCallTerminal::NotExecuted { reason } => {
            validate_json(&Value::String(reason.clone()), 1, limits.item_bytes)?;
            (
                IntentStatus::NotExecuted,
                None,
                false,
                None,
                Some(bounded_error(reason)),
            )
        }
    };
    if matches!(
        call.status,
        IntentStatus::Pending | IntentStatus::OutcomeUnknown
    ) {
        if call
            .result
            .as_ref()
            .is_some_and(|old| Some(old) != result.as_ref())
            || call
                .result_hash
                .as_ref()
                .is_some_and(|old| Some(old) != hash.as_ref())
        {
            return Err(CellError::Conflict);
        }
        call.status = status;
        call.result = result;
        call.result_omitted = omitted;
        call.result_hash = hash;
        call.error = error;
    } else {
        if call.status != status {
            return Err(CellError::Conflict);
        }
        if status == IntentStatus::Succeeded {
            let stored_hash = call
                .result_hash
                .clone()
                .or_else(|| call.result.as_ref().map(HostResult::value_hash));
            if stored_hash.is_none() && call.result_omitted && call.result.is_none() {
                // An earlier durable success may have omitted its value without
                // a digest. Bind the first host observation in the audit, while
                // retaining the exact known status and omitted delivery. Future
                // evidence must match this digest and cannot replace success.
                call.result_hash = hash;
            } else if stored_hash != hash
                || (call.result.is_some() && result.is_some() && call.result != result)
            {
                return Err(CellError::Conflict);
            }
        } else if call.error != error {
            return Err(CellError::Conflict);
        }
        // Known status and delivery stay intact; only a missing digest may be added.
    }
    Ok(())
}

fn valid_hash(hash: &str) -> bool {
    hash.strip_prefix("sha256:").is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

pub(crate) fn source_hash(source: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{:x}", Sha256::digest(source.as_bytes()))
}

impl HostResult {
    /// Digest of the canonical serde_json representation, without recursively
    /// serializing unchecked containers. The host can bind an oversized success
    /// using this digest while retaining its full result in its own durable store.
    pub fn value_hash(value: &serde_json::Value) -> String {
        use sha2::{Digest, Sha256};
        use std::io::{self, Write};
        struct Writer(Sha256);
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        enum Part<'a> {
            Value(&'a serde_json::Value),
            Key(&'a str),
            Byte(u8),
        }
        let mut writer = Writer(Sha256::new());
        let mut stack = vec![Part::Value(value)];
        while let Some(part) = stack.pop() {
            match part {
                Part::Byte(byte) => writer.write_all(&[byte]).expect("hash writer cannot fail"),
                Part::Key(key) => serde_json::to_writer(&mut writer, key).expect("JSON string"),
                Part::Value(serde_json::Value::Array(values)) => {
                    writer.write_all(b"[").expect("hash writer cannot fail");
                    stack.push(Part::Byte(b']'));
                    for (index, value) in values.iter().enumerate().rev() {
                        stack.push(Part::Value(value));
                        if index != 0 {
                            stack.push(Part::Byte(b','));
                        }
                    }
                }
                Part::Value(serde_json::Value::Object(values)) => {
                    writer.write_all(b"{").expect("hash writer cannot fail");
                    stack.push(Part::Byte(b'}'));
                    for (index, (key, value)) in values.iter().enumerate().rev() {
                        stack.push(Part::Value(value));
                        stack.push(Part::Byte(b':'));
                        stack.push(Part::Key(key));
                        if index != 0 {
                            stack.push(Part::Byte(b','));
                        }
                    }
                }
                Part::Value(scalar) => {
                    serde_json::to_writer(&mut writer, scalar).expect("JSON scalar")
                }
            }
        }
        format!("sha256:{:x}", writer.0.finalize())
    }
}
