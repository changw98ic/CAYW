use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArguments {
    action: String,
    cell_id: String,
    #[serde(deserialize_with = "wait_integer")]
    cursor: u64,
    #[serde(deserialize_with = "wait_integer")]
    yield_time_ms: u64,
    #[serde(deserialize_with = "wait_integer")]
    max_output_tokens: u64,
    #[serde(default = "continue_default")]
    continue_execution: bool,
}
fn continue_default() -> bool {
    true
}
fn wait_integer<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    cayw_ai::unsigned_tool_integer(&value).map_err(serde::de::Error::custom)
}

impl StoredSession {
    fn validate_wait_binding(&self, binding: &ExecutionWaitRecord) -> Result<(), CoreError> {
        check_values(&binding.arguments)?;
        identifier(&binding.call_id, "call_id")?;
        identifier(&binding.execution_id, "execution_id")?;
        identifier(&binding.task_id, "task_id")?;
        let arguments: WaitArguments = serde_json::from_value(binding.arguments.clone())?;
        // Cursor, wait duration and continue control are observation/resume
        // arguments, not authority. The Cells host enforces its runtime budgets.
        let _ = (
            arguments.cursor,
            arguments.yield_time_ms,
            arguments.continue_execution,
        );
        let execution = self
            .executions
            .iter()
            .find(|e| e.execution_id == binding.execution_id)
            .ok_or_else(|| CoreError::ExecutionNotFound(binding.execution_id.clone()))?;
        let origin = self.call(&execution.parent_call_id)?;
        let call = self.call(&binding.call_id)?;
        if arguments.action != "wait"
            || arguments.cell_id != execution.execution_id
            || arguments.max_output_tokens < 1024
            || binding.lease.session_id != self.id
            || execution.lease != binding.lease
            || execution.task_id != binding.task_id
            || origin.record.attempt_id != binding.lease.attempt_id
            || origin.record.call.name != "execute_code"
            || !origin.record.execution_started
            || call.record.attempt_id != binding.lease.attempt_id
            || call.record.call.name != "execute_code"
            || !call.record.execution_started
            || call.record.call.arguments != binding.arguments
            || call.record.call.arguments_error.is_some()
            || self
                .executions
                .iter()
                .any(|e| e.parent_call_id == binding.call_id)
            || self.approvals.iter().any(|a| {
                a.attempt_id == binding.lease.attempt_id
                    && a.input.target
                        == ApprovalTarget::ToolCall {
                            call_id: binding.call_id.clone(),
                        }
                    && a.input.task_id != binding.task_id
            })
        {
            return Err(invalid(
                "Wait binding requires its exact started execute_code call, original lease, task and wait arguments",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_wait_bindings(&self) -> Result<(), CoreError> {
        let mut ids = BTreeSet::new();
        for binding in &self.execution_waits {
            if !ids.insert(&binding.call_id) {
                return Err(CoreError::CorruptStore(
                    "Duplicate execution wait binding".into(),
                ));
            }
            self.validate_wait_binding(binding)?;
        }
        Ok(())
    }
}

/// The projection alone cannot grant recovery authority. Every binding must
/// have exactly one matching append-only registration with its complete lease.
pub(super) fn validate_wait_log(
    connection: &Connection,
    session: &StoredSession,
) -> Result<(), CoreError> {
    let mut seen = BTreeSet::new();
    let mut query = connection.prepare("SELECT cursor, record FROM core_events WHERE session_id=?1 AND json_extract(record, '$.event.type')='execution_wait_registered' ORDER BY cursor")?;
    let rows = query.query_map([&session.id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (cursor, json) = row?;
        let record = checked_input(serde_json::from_str::<EventRecord>(&json)?)?;
        let CoreEvent::ExecutionWaitRegistered { binding } = &record.event else {
            return Err(CoreError::CorruptStore(
                "Invalid wait registration event".into(),
            ));
        };
        if record.protocol_version != PROTOCOL_VERSION
            || record.session_id != session.id
            || cursor < 1
            || record.cursor != cursor as u64
            || record.cursor > session.cursor
            || record.attempt.as_ref() != Some(&binding.lease)
            || !seen.insert(binding.call_id.clone())
            || !session.execution_waits.contains(binding)
        {
            return Err(CoreError::CorruptStore(
                "Wait registration log differs from its projection or original lease".into(),
            ));
        }
    }
    if seen.len() != session.execution_waits.len() {
        return Err(CoreError::CorruptStore(
            "Wait projection has no canonical registration event".into(),
        ));
    }
    Ok(())
}

impl SessionStore {
    /// Trusted host-only registration, immediately after mark_tool_started and
    /// before dispatching the already validated wait to Cells. It records no VM
    /// replay or capability authorization. Both observation and resume waits use
    /// the existing VM/capability facts for recovery. A model's name/arguments
    /// alone never grants this status. Exact registration retries are read-only.
    pub fn register_execution_wait(
        &self,
        lease: &AttemptLease,
        call_id: &str,
        execution_id: &str,
        task_id: &str,
        arguments: &Value,
    ) -> Result<EventRecord, CoreError> {
        check_values(arguments)?;
        let binding = ExecutionWaitRecord {
            call_id: call_id.into(),
            execution_id: execution_id.into(),
            task_id: task_id.into(),
            lease: lease.clone(),
            arguments: arguments.clone(),
        };
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load(&transaction, &lease.session_id)?;
        session.admit_work(lease)?;
        session.validate_wait_binding(&binding)?;
        if session.call(call_id)?.record.state != CallState::Started {
            return Err(invalid(
                "Wait registration requires a currently started call",
            ));
        }
        session.check_approval(call_id)?;
        if let Some(previous) = session
            .execution_waits
            .iter()
            .find(|b| b.call_id == call_id)
        {
            if previous != &binding {
                return Err(invalid(
                    "A wait binding cannot change its original execution",
                ));
            }
            let mut query = transaction
                .prepare("SELECT record FROM core_events WHERE session_id=?1 ORDER BY cursor")?;
            let rows = query.query_map([&lease.session_id], |row| row.get::<_, String>(0))?;
            for row in rows {
                let event: EventRecord = serde_json::from_str(&row?)?;
                if matches!(&event.event, CoreEvent::ExecutionWaitRegistered { binding: saved } if saved == &binding)
                {
                    return Ok(event);
                }
            }
            return Err(CoreError::CorruptStore(
                "Wait binding has no canonical registration event".into(),
            ));
        }
        session.execution_waits.push(binding.clone());
        let event = persist(
            &transaction,
            &mut session,
            Some(lease.clone()),
            CoreEvent::ExecutionWaitRegistered { binding },
        )?;
        transaction.commit()?;
        Ok(event)
    }
}
