use crate::*;
use cayw_agent_types::{AgentEvent, ToolResult};
use cayw_ai::{
    AssistantMessage, Content, Message, StopReason, ToolCall, ToolResultMessage, timestamp_ms,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, VecDeque},
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

const APPLICATION_ID: u32 = 0x43415957;
const STORAGE_FORMAT: &str = "cayw-core/3";
const PROJECTION_CACHE_ENTRIES: usize = 32;
// Limits retained JSON, not the decoded sessions' allocator overhead.
const PROJECTION_CACHE_JSON_BYTES: usize = 16 * 1024 * 1024;

mod capabilities;
mod migration;
pub(crate) mod validation;
mod waits;
use validation::{check_values, checked_input, encode_readable};

/// A synchronous store for file-backed sessions. Async hosts should use a blocking
/// task. Every mutation takes an IMMEDIATE transaction, checks the current lease,
/// and commits its event together with the session projection before returning.
/// Recovery is always explicit; opening a store never changes worker ownership.
#[derive(Clone)]
pub struct SessionStore {
    connection: Arc<Mutex<Connection>>,
    projection_cache: Arc<Mutex<ProjectionCache>>,
    reconciliation_verifier: Option<Arc<dyn ReconciliationVerifier>>,
}

struct CachedProjection {
    json: String,
    session: Arc<StoredSession>,
}

#[derive(Default)]
struct ProjectionCache {
    // Most recently used at the back. The entry limit keeps linear lookup small.
    entries: VecDeque<CachedProjection>,
    json_bytes: usize,
}

impl ProjectionCache {
    fn take(&mut self, id: &str) -> Option<CachedProjection> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.session.id == id)?;
        let entry = self.entries.remove(index)?;
        self.json_bytes -= entry.json.len();
        Some(entry)
    }

    fn insert(&mut self, entry: CachedProjection) {
        self.take(&entry.session.id);
        if entry.json.len() > PROJECTION_CACHE_JSON_BYTES {
            return;
        }
        while self.entries.len() >= PROJECTION_CACHE_ENTRIES
            || self.json_bytes + entry.json.len() > PROJECTION_CACHE_JSON_BYTES
        {
            let Some(oldest) = self.entries.pop_front() else {
                break;
            };
            self.json_bytes -= oldest.json.len();
        }
        self.json_bytes += entry.json.len();
        self.entries.push_back(entry);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSession {
    id: String,
    label: String,
    cursor: u64,
    fence: u64,
    attempt: Option<AttemptLease>,
    lifecycle: Option<Lifecycle>,
    history: Vec<HistoryEntry>,
    calls: Vec<StoredCall>,
    approvals: Vec<ApprovalRequest>,
    executions: Vec<ExecutionRecord>,
    execution_waits: Vec<ExecutionWaitRecord>,
    capabilities: Vec<CapabilityCallRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum HistoryEntry {
    Message { message: Box<Message> },
    ToolSlot { call_id: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lifecycle {
    started: bool,
    ended: bool,
    turn_open: bool,
    open_message: Option<Message>,
    assistant: Option<AssistantMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCall {
    record: CallRecord,
    batch_settled: bool,
    result_acknowledged: bool,
}

impl StoredSession {
    fn new(id: String, label: String) -> Self {
        Self {
            id,
            label,
            cursor: 0,
            fence: 0,
            attempt: None,
            lifecycle: None,
            history: vec![],
            calls: vec![],
            approvals: vec![],
            executions: vec![],
            execution_waits: vec![],
            capabilities: vec![],
        }
    }
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            id: self.id.clone(),
            label: self.label.clone(),
            messages: self.messages(),
            cursor: self.cursor,
            state: self.state(),
            attempt: self.attempt.clone(),
            pending_approvals: self
                .approvals
                .iter()
                .filter(|a| a.decision.is_none())
                .cloned()
                .collect(),
            unresolved_calls: self
                .calls
                .iter()
                .filter(|c| unresolved(&c.record))
                .map(|c| c.record.clone())
                .collect(),
            executions: self.executions.clone(),
            unresolved_capabilities: self
                .capabilities
                .iter()
                .filter(|c| capability_unresolved(c))
                .cloned()
                .collect(),
        }
    }
    fn messages(&self) -> Vec<Message> {
        self.history
            .iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Message { message } => Some(message.as_ref().clone()),
                HistoryEntry::ToolSlot { call_id } => self
                    .calls
                    .iter()
                    .find(|c| c.record.call.id == *call_id)
                    .and_then(|c| c.record.result.clone())
                    .map(Message::ToolResult),
            })
            .collect()
    }
    fn state(&self) -> SessionState {
        if self.has_unknown() {
            SessionState::NeedsReconciliation
        } else if self.approvals.iter().any(|a| a.decision.is_none()) {
            SessionState::AwaitingApproval
        } else if self.attempt.is_some() {
            SessionState::Running
        } else {
            SessionState::Idle
        }
    }
    fn verify_lease(&self, lease: &AttemptLease) -> Result<(), CoreError> {
        if self.attempt.as_ref() == Some(lease) {
            Ok(())
        } else {
            Err(CoreError::StaleLease)
        }
    }
    fn call(&self, id: &str) -> Result<&StoredCall, CoreError> {
        self.calls
            .iter()
            .find(|c| c.record.call.id == id)
            .ok_or_else(|| CoreError::CallNotFound(id.into()))
    }
    fn call_mut(&mut self, id: &str) -> Result<&mut StoredCall, CoreError> {
        self.calls
            .iter_mut()
            .find(|c| c.record.call.id == id)
            .ok_or_else(|| CoreError::CallNotFound(id.into()))
    }
    fn lifecycle(&self) -> Result<&Lifecycle, CoreError> {
        self.lifecycle.as_ref().ok_or(CoreError::StaleLease)
    }
    fn lifecycle_mut(&mut self) -> Result<&mut Lifecycle, CoreError> {
        self.lifecycle.as_mut().ok_or(CoreError::StaleLease)
    }
    fn check_approval(&self, call_id: &str) -> Result<(), CoreError> {
        let attempt_id = &self
            .attempt
            .as_ref()
            .ok_or(CoreError::StaleLease)?
            .attempt_id;
        let relevant = self
            .approvals
            .iter()
            .filter(|approval| {
                approval.attempt_id == *attempt_id
                    && matches!(&approval.input.target, ApprovalTarget::ToolCall { call_id: id } if id == call_id)
            })
            .collect::<Vec<_>>();
        if relevant.iter().any(|a| a.decision.is_none()) {
            return Err(CoreError::PendingApproval);
        }
        if relevant.iter().any(|a| {
            a.decision
                .as_ref()
                .is_some_and(|d| d.decision == ApprovalDecision::Deny)
        }) {
            return Err(CoreError::ApprovalDenied);
        }
        Ok(())
    }
}

fn unresolved(call: &CallRecord) -> bool {
    call.state != CallState::Completed
}
fn nonempty(value: &str, field: &str) -> Result<(), CoreError> {
    if value.trim().is_empty() || value.len() > MAX_METADATA_BYTES {
        Err(CoreError::InvalidInput(format!(
            "{field} must be nonempty and at most {MAX_METADATA_BYTES} bytes"
        )))
    } else {
        Ok(())
    }
}
fn identifier(value: &str, field: &str) -> Result<(), CoreError> {
    nonempty(value, field)?;
    if value.len() > MAX_IDENTIFIER_BYTES || value.chars().any(char::is_control) {
        return Err(CoreError::InvalidInput(format!(
            "{field} is not a bounded identifier"
        )));
    }
    Ok(())
}
struct ByteCountWriter {
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl Write for ByteCountWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written = self.written.saturating_add(buf.len());
        if self.written > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Metadata byte limit exceeded",
            ));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_value(value: &Value) -> Result<(), CoreError> {
    let mut pending = vec![(value, 0)];
    let mut nodes = 0;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        let children = match value {
            Value::Array(values) => values.len(),
            Value::Object(values) => values.len(),
            _ => 0,
        };
        if depth > MAX_METADATA_DEPTH || nodes + pending.len() + children > MAX_METADATA_NODES {
            return Err(CoreError::InvalidInput(
                "Metadata nesting or node limit exceeded".into(),
            ));
        }
        match value {
            Value::Array(values) => pending.extend(values.iter().map(|value| (value, depth + 1))),
            Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, depth + 1)))
            }
            _ => {}
        }
    }
    let mut writer = ByteCountWriter {
        written: 0,
        limit: MAX_METADATA_BYTES,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        if writer.exceeded {
            return Err(CoreError::InvalidInput(
                "Metadata byte limit exceeded".into(),
            ));
        }
        return Err(error.into());
    }
    if writer.written > MAX_METADATA_BYTES {
        return Err(CoreError::InvalidInput(
            "Metadata byte limit exceeded".into(),
        ));
    }
    Ok(())
}
fn validate_host_kind(kind: &str) -> Result<(), CoreError> {
    nonempty(kind, "kind")?;
    let kind = kind.to_ascii_lowercase();
    if matches!(
        kind.as_str(),
        "execution_started"
            | "execution_yielded"
            | "execution_wait_registered"
            | "execution_finished"
            | "capability_planned"
            | "capability_started"
            | "capability_settled"
            | "capability_completed"
            | "capability_outcome_unknown"
            | "capability_reconciled"
    ) {
        return Err(invalid(
            "Execution facts require their typed Core API and projection",
        ));
    }
    Ok(())
}
fn capability_unresolved(call: &CapabilityCallRecord) -> bool {
    call.stage != CapabilityStage::Settled
        || matches!(call.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. }))
}
fn execution_active(execution: &ExecutionRecord) -> bool {
    matches!(
        execution.state,
        ExecutionState::Running | ExecutionState::Yielded
    )
}
fn invalid(message: &str) -> CoreError {
    CoreError::InvalidEvent(message.into())
}

impl SessionStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Err(CoreError::InvalidInput(
                "A persistent database file path is required".into(),
            ));
        }
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        // Inspect ownership before persistent pragma changes or schema writes.
        {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let version: u32 =
                transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
            let application_id: u32 =
                transaction.pragma_query_value(None, "application_id", |row| row.get(0))?;
            if version == 0 {
                let existing: i64 = transaction.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE substr(lower(name), 1, 7) <> 'sqlite_'",
                    [],
                    |row| row.get(0),
                )?;
                if existing != 0 || application_id != 0 {
                    return Err(CoreError::ForeignDatabase);
                }
                transaction.execute_batch(
                    "CREATE TABLE core_metadata (format TEXT PRIMARY KEY NOT NULL);
                     INSERT INTO core_metadata (format) VALUES ('cayw-core/3');
                     CREATE TABLE core_sessions (id TEXT PRIMARY KEY NOT NULL, projection TEXT NOT NULL);
                     CREATE TABLE core_events (
                         session_id TEXT NOT NULL REFERENCES core_sessions(id),
                         cursor INTEGER NOT NULL CHECK(cursor > 0),
                         record TEXT NOT NULL,
                         PRIMARY KEY(session_id, cursor)
                     );
                     CREATE TRIGGER core_events_no_update BEFORE UPDATE ON core_events
                     BEGIN SELECT RAISE(ABORT, 'core events are append-only'); END;
                     CREATE TRIGGER core_events_no_delete BEFORE DELETE ON core_events
                     BEGIN SELECT RAISE(ABORT, 'core events are append-only'); END;"
                )?;
                transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            } else {
                if !matches!(version, 1 | 2 | SCHEMA_VERSION) {
                    return Err(CoreError::SchemaVersion {
                        found: version,
                        expected: SCHEMA_VERSION,
                    });
                }
                if application_id != APPLICATION_ID {
                    return Err(CoreError::ForeignDatabase);
                }
            }
            let valid_tables: u32 = transaction.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('core_metadata','core_sessions','core_events')",
                [], |row| row.get(0))?;
            let valid_triggers: u32 = transaction.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND tbl_name='core_events' AND name IN ('core_events_no_update','core_events_no_delete')",
                [], |row| row.get(0))?;
            if valid_tables != 3 || valid_triggers != 2 {
                return Err(CoreError::ForeignDatabase);
            }
            let format: String =
                transaction.query_row("SELECT format FROM core_metadata", [], |row| row.get(0))?;
            let expected_format = match version {
                1 => "cayw-core/1",
                2 => "cayw-core/2",
                _ => STORAGE_FORMAT,
            };
            if format != expected_format {
                return Err(CoreError::ForeignDatabase);
            }
            transaction.prepare("SELECT id, projection FROM core_sessions LIMIT 0")?;
            transaction.prepare("SELECT session_id, cursor, record FROM core_events LIMIT 0")?;
            match version {
                1 => migration::migrate_v1(&transaction)?,
                2 => migration::migrate_v2(&transaction)?,
                _ => {}
            }
            // A current schema cannot make an older weak reconciliation valid.
            // Decode these records before DDL or persistent pragma changes, so a
            // rejected missing-origin event leaves the database bytes untouched.
            {
                let mut query = transaction.prepare("SELECT session_id,cursor,record FROM core_events WHERE json_extract(record,'$.event.type') IN ('reconciled','capability_reconciled')")?;
                let mut rows = query.query([])?;
                while let Some(row) = rows.next()? {
                    let id: String = row.get(0)?;
                    let cursor: i64 = row.get(1)?;
                    let json: String = row.get(2)?;
                    let record = checked_input(serde_json::from_str::<EventRecord>(&json)?)?;
                    if record.protocol_version != PROTOCOL_VERSION
                        || record.session_id != id
                        || cursor <= 0
                        || record.cursor != cursor as u64
                    {
                        return Err(CoreError::CorruptStore(
                            "Reconciliation event envelope is inconsistent".into(),
                        ));
                    }
                }
            }
            // REPLACE can delete a conflicting row without firing DELETE guards
            // on an ordinary SQLite connection. Reject the conflict before that
            // deletion, including databases created by earlier v3 builds. This
            // DDL shares the migration transaction and rolls back with it.
            transaction.execute_batch(
                "CREATE TRIGGER IF NOT EXISTS core_events_no_replace BEFORE INSERT ON core_events
                 WHEN EXISTS (SELECT 1 FROM core_events WHERE session_id=NEW.session_id AND cursor=NEW.cursor)
                 BEGIN SELECT RAISE(ABORT, 'core events are append-only'); END;"
            )?;
            // Every load verifies wait registrations. Index just those events so
            // streaming updates do not reparse all previous message snapshots.
            transaction.execute_batch(
                "CREATE INDEX IF NOT EXISTS core_execution_wait_log ON core_events(session_id, cursor)
                 WHERE json_extract(record, '$.event.type')='execution_wait_registered';
                 CREATE INDEX IF NOT EXISTS core_events_by_type ON core_events(
                     session_id, json_extract(record, '$.event.type'), cursor);
                 CREATE INDEX IF NOT EXISTS core_host_events_by_kind ON core_events(
                     session_id, json_extract(record, '$.event.kind'), cursor)
                 WHERE json_extract(record, '$.event.type')='host';"
            )?;
            transaction.commit()?;
        }
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let journal: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if !journal.eq_ignore_ascii_case("wal") {
            return Err(CoreError::InvalidInput(
                "The database filesystem must support SQLite WAL".into(),
            ));
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            projection_cache: Arc::new(Mutex::new(ProjectionCache::default())),
            reconciliation_verifier: None,
        })
    }

    /// Read the already committed decision for an original unknown observation.
    /// Forks retain their source cursor; this follows only that recorded prefix
    /// and never manufactures a new decision or reauthorizes an uncommitted one.
    pub fn committed_tool_reconciliation(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<Reconciliation, CoreError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction()?;
        let reconciliation = committed_tool_reconciliation(&transaction, session_id, call_id)?;
        transaction.commit()?;
        Ok(reconciliation)
    }

    /// Install host authority in memory. It cannot be supplied by protocol input
    /// or read from caller-owned actor/evidence fields. Clones retain the policy.
    pub fn with_reconciliation_verifier(
        mut self,
        verifier: Arc<dyn ReconciliationVerifier>,
    ) -> Self {
        self.reconciliation_verifier = Some(verifier);
        self
    }

    pub fn verify_reconciliation(&self, request: &ReconciliationRequest) -> Result<(), CoreError> {
        match request {
            ReconciliationRequest::Tool { reconciliation } => check_values(reconciliation)?,
            ReconciliationRequest::Capability { reconciliation } => check_values(reconciliation)?,
        }
        let verifier = self
            .reconciliation_verifier
            .as_ref()
            .ok_or(CoreError::ApprovalDenied)?;
        verifier
            .verify(request)
            .map_err(|_| CoreError::ApprovalDenied)
    }

    pub fn create_session(&self, label: impl Into<String>) -> Result<SessionSnapshot, CoreError> {
        let label = label.into();
        nonempty(&label, "label")?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = StoredSession::new(Uuid::new_v4().to_string(), label.clone());
        insert_session(&transaction, &session)?;
        persist(
            &transaction,
            &mut session,
            None,
            CoreEvent::SessionCreated { label },
        )?;
        transaction.commit()?;
        Ok(session.snapshot())
    }

    /// Fork only a host-confirmed, paired and inactive history. The original
    /// session is unchanged; the new log captures the source cursor and history.
    pub fn fork_session(
        &self,
        source: &str,
        label: &str,
        confirmed: bool,
    ) -> Result<SessionSnapshot, CoreError> {
        nonempty(label, "label")?;
        if !confirmed {
            return Err(CoreError::InvalidInput(
                "Fork requires explicit host confirmation".into(),
            ));
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let original = load(&transaction, source)?;
        if original.attempt.is_some() {
            return Err(CoreError::Busy);
        }
        if original
            .calls
            .iter()
            .any(|c| unresolved(&c.record) || !c.batch_settled)
            || original.executions.iter().any(execution_active)
            || original.capabilities.iter().any(capability_unresolved)
        {
            return Err(CoreError::NeedsReconciliation);
        }
        if original.approvals.iter().any(|a| a.decision.is_none()) {
            return Err(CoreError::PendingApproval);
        }
        validate_paired_history(&original.messages(), |result| {
            if original.call(&result.tool_call_id)?.record.result.as_ref() != Some(result) {
                return Err(invalid(
                    "Fork tool result differs from its original stored observation",
                ));
            }
            committed_tool_reconciliation(&transaction, &original.id, &result.tool_call_id)?;
            Ok(())
        })?;
        let mut session = StoredSession::new(Uuid::new_v4().to_string(), label.into());
        session.history = original.history.clone();
        session.calls = original.calls.clone();
        insert_session(&transaction, &session)?;
        persist(
            &transaction,
            &mut session,
            None,
            CoreEvent::SessionCreated {
                label: label.into(),
            },
        )?;
        let messages = session.messages();
        persist(
            &transaction,
            &mut session,
            None,
            CoreEvent::SessionForked {
                source_session_id: original.id,
                source_cursor: original.cursor,
                messages,
            },
        )?;
        transaction.commit()?;
        Ok(session.snapshot())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSnapshot>, CoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let mut query = connection.prepare("SELECT id FROM core_sessions ORDER BY id")?;
        let mut rows = query.query([])?;
        let mut sessions = vec![];
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            sessions.push(load(&connection, &id)?.snapshot());
        }
        Ok(sessions)
    }

    pub fn snapshot(&self, session_id: &str) -> Result<SessionSnapshot, CoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        Ok(self.cached_projection(&connection, session_id)?.snapshot())
    }

    // Compare the complete persisted value, including within write transactions.
    // Equal cursors alone cannot detect a peer change or a damaged projection.
    fn cached_projection(
        &self,
        connection: &Connection,
        id: &str,
    ) -> Result<Arc<StoredSession>, CoreError> {
        let json = projection_json(connection, id)?;
        let mut cache = self
            .projection_cache
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        if let Some(cached) = cache.take(id)
            && cached.json == json
        {
            waits::validate_wait_log(connection, &cached.session)?;
            let session = cached.session.clone();
            cache.insert(cached);
            return Ok(session);
        }
        let session = Arc::new(decode_projection(connection, id, &json)?);
        cache.insert(CachedProjection {
            json,
            session: session.clone(),
        });
        Ok(session)
    }

    /// Includes resolved requests so a waiting runtime can observe a durable decision.
    pub fn approvals(&self, session_id: &str) -> Result<Vec<ApprovalRequest>, CoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        Ok(self
            .cached_projection(&connection, session_id)?
            .approvals
            .clone())
    }

    pub fn approval(
        &self,
        session_id: &str,
        approval_id: &str,
    ) -> Result<ApprovalRequest, CoreError> {
        self.approvals(session_id)?
            .into_iter()
            .find(|a| a.id == approval_id)
            .ok_or_else(|| CoreError::ApprovalNotFound(approval_id.into()))
    }

    pub fn events(
        &self,
        session_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<EventRecord>, CoreError> {
        if limit == 0 || limit > MAX_EVENT_PAGE {
            return Err(CoreError::InvalidLimit {
                limit,
                max: MAX_EVENT_PAGE,
            });
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction()?;
        let session = self.cached_projection(&transaction, session_id)?;
        if after > session.cursor {
            return Err(CoreError::FutureCursor {
                after,
                current: session.cursor,
            });
        }
        let mut records = vec![];
        {
            let mut query = transaction.prepare("SELECT record FROM core_events WHERE session_id=?1 AND cursor>?2 ORDER BY cursor LIMIT ?3")?;
            let mut rows = query.query(params![session_id, after as i64, limit as i64])?;
            while let Some(row) = rows.next()? {
                let json: String = row.get(0)?;
                let record = checked_input(serde_json::from_str::<EventRecord>(&json)?)?;
                if record.protocol_version != PROTOCOL_VERSION
                    || record.session_id != session_id
                    || record.cursor != after + records.len() as u64 + 1
                {
                    return Err(CoreError::CorruptStore(
                        "The event log envelope or cursor is inconsistent".into(),
                    ));
                }
                records.push(record);
            }
        }
        if (records.len() as u64) < (session.cursor - after).min(limit as u64) {
            return Err(CoreError::CorruptStore(
                "The event log has missing entries".into(),
            ));
        }
        transaction.commit()?;
        Ok(records)
    }

    /// Read selected durable events without decoding unrelated message snapshots.
    /// Cursors remain those of the original log and need not be consecutive.
    pub fn events_by_kind(
        &self,
        session_id: &str,
        after: u64,
        limit: usize,
        event_type: &str,
        host_kind: Option<&str>,
    ) -> Result<Vec<EventRecord>, CoreError> {
        if limit == 0 || limit > MAX_EVENT_PAGE {
            return Err(CoreError::InvalidLimit {
                limit,
                max: MAX_EVENT_PAGE,
            });
        }
        if event_type.is_empty() || (host_kind.is_some() && event_type != "host") {
            return Err(CoreError::InvalidInput("Invalid event selection".into()));
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction()?;
        let session = self.cached_projection(&transaction, session_id)?;
        if after > session.cursor {
            return Err(CoreError::FutureCursor {
                after,
                current: session.cursor,
            });
        }
        // Check the complete prefix through its covering primary-key index.
        // This retains gap detection without reading unrelated event payloads.
        let count: i64 = transaction.query_row(
            "SELECT count(*) FROM core_events WHERE session_id=?1 AND cursor<=?2",
            params![session_id, session.cursor as i64],
            |row| row.get(0),
        )?;
        if count != session.cursor as i64 {
            return Err(CoreError::CorruptStore(
                "The event log has missing entries".into(),
            ));
        }
        let (sql, kind) = match host_kind {
            Some(kind) => (HOST_EVENTS_BY_KIND, kind),
            None => (EVENTS_BY_TYPE, event_type),
        };
        let mut records = Vec::new();
        {
            let mut query = transaction.prepare(sql)?;
            let mut rows = query.query(params![session_id, kind, after as i64, limit as i64])?;
            let mut previous = after;
            while let Some(row) = rows.next()? {
                let cursor: i64 = row.get(0)?;
                let json: String = row.get(1)?;
                let record = checked_input(serde_json::from_str::<EventRecord>(&json)?)?;
                if record.protocol_version != PROTOCOL_VERSION
                    || record.session_id != session_id
                    || cursor <= 0
                    || record.cursor != cursor as u64
                    || record.cursor <= previous
                    || record.cursor > session.cursor
                {
                    return Err(CoreError::CorruptStore(
                        "Selected event envelope or cursor is inconsistent".into(),
                    ));
                }
                previous = record.cursor;
                records.push(record);
            }
        }
        transaction.commit()?;
        Ok(records)
    }

    pub fn begin_attempt(
        &self,
        session_id: &str,
        worker_id: &str,
    ) -> Result<AttemptLease, CoreError> {
        nonempty(worker_id, "worker_id")?;
        self.mutate(session_id, |session| {
            if session.attempt.is_some() {
                return Err(CoreError::Busy);
            }
            if session
                .calls
                .iter()
                .any(|c| unresolved(&c.record) || !c.batch_settled)
                || session.executions.iter().any(execution_active)
                || session.capabilities.iter().any(capability_unresolved)
            {
                return Err(CoreError::NeedsReconciliation);
            }
            if session.approvals.iter().any(|a| a.decision.is_none()) {
                return Err(CoreError::PendingApproval);
            }
            session.fence = increment(session.fence)?;
            let lease = AttemptLease {
                session_id: session.id.clone(),
                attempt_id: Uuid::new_v4().to_string(),
                worker_id: worker_id.into(),
                fence: session.fence,
            };
            session.attempt = Some(lease.clone());
            session.lifecycle = Some(Lifecycle::default());
            Ok((
                lease.clone(),
                Some(lease.clone()),
                CoreEvent::AttemptStarted { lease },
            ))
        })
        .map(|(lease, _)| lease)
    }

    /// Checks current ownership at this instant. Execution writes check again
    /// atomically; a successful read alone cannot reserve a future side effect.
    pub fn validate_lease(&self, lease: &AttemptLease) -> Result<(), CoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        self.cached_projection(&connection, &lease.session_id)?
            .verify_lease(lease)
    }

    pub fn append_host_event(
        &self,
        lease: &AttemptLease,
        kind: &str,
        data: Value,
    ) -> Result<EventRecord, CoreError> {
        let data = checked_input(data)?;
        validate_host_kind(kind)?;
        self.mutate(&lease.session_id, |session| {
            session.verify_lease(lease)?;
            Ok((
                (),
                Some(lease.clone()),
                CoreEvent::Host {
                    kind: kind.into(),
                    data,
                },
            ))
        })
        .map(|(_, record)| record)
    }

    /// Records a trusted host decision against the exact observed session state.
    /// Idle sessions require no lease; active sessions require their current lease.
    /// This cannot alter call outcomes, approve a tool or replace worker ownership.
    pub fn append_control_event(
        &self,
        session_id: &str,
        expected_cursor: u64,
        expected_attempt: Option<&AttemptLease>,
        actor: &str,
        kind: &str,
        data: Value,
    ) -> Result<EventRecord, CoreError> {
        let data = checked_input(data)?;
        nonempty(actor, "actor")?;
        validate_host_kind(kind)?;
        self.mutate(session_id, |session| {
            if session.attempt.as_ref() != expected_attempt {
                return Err(CoreError::StaleLease);
            }
            if session.cursor != expected_cursor {
                return Err(invalid("Host decision requires the current session cursor"));
            }
            Ok((
                (),
                session.attempt.clone(),
                CoreEvent::Host {
                    kind: kind.into(),
                    data: serde_json::json!({"actor": actor, "decision": data}),
                },
            ))
        })
        .map(|(_, record)| record)
    }

    pub fn append_agent_event(
        &self,
        lease: &AttemptLease,
        event: &AgentEvent,
    ) -> Result<EventRecord, CoreError> {
        check_values(event)?;
        if matches!(event, AgentEvent::MessageUpdate { .. }) {
            return self.append_message_update(lease, event);
        }
        self.mutate(&lease.session_id, |session| {
            session.verify_lease(lease)?;
            apply_agent_event(session, lease, event)?;
            Ok((
                (),
                Some(lease.clone()),
                CoreEvent::Agent {
                    event: Box::new(event.clone()),
                },
            ))
        })
        .map(|(_, record)| record)
    }

    fn append_message_update(
        &self,
        lease: &AttemptLease,
        event: &AgentEvent,
    ) -> Result<EventRecord, CoreError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = self.cached_projection(&transaction, &lease.session_id)?;
        // Remove only this session before mutation. On error its next read
        // reloads the committed projection; other sessions remain cached.
        self.projection_cache
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?
            .take(&lease.session_id);
        let state = Arc::make_mut(&mut session);
        state.verify_lease(lease)?;
        apply_agent_event(state, lease, event)?;
        state.cursor = increment(state.cursor)?;
        let record = EventRecord {
            protocol_version: PROTOCOL_VERSION,
            session_id: state.id.clone(),
            cursor: state.cursor,
            timestamp: timestamp_ms(),
            attempt: Some(lease.clone()),
            event: CoreEvent::Agent {
                event: Box::new(event.clone()),
            },
        };
        // The event contains the same message at a greater envelope depth than
        // the projection. Its readable encoding checks every changed JSON value.
        let record_json = encode_readable(&record)?;
        let lifecycle = serde_json::to_string(&state.lifecycle)?;
        transaction.execute(
            "INSERT INTO core_events (session_id, cursor, record) VALUES (?1, ?2, ?3)",
            params![state.id, state.cursor as i64, record_json],
        )?;
        // A stream update changes only these two fields. Keep the already
        // validated history in SQLite instead of decoding and encoding it again.
        let json = transaction.query_row(
            "UPDATE core_sessions SET projection=json_set(projection, '$.cursor', ?2, '$.lifecycle', json(?3)) WHERE id=?1 RETURNING projection",
            params![state.id, state.cursor as i64, lifecycle],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        self.projection_cache
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?
            .insert(CachedProjection { json, session });
        Ok(record)
    }

    /// Commit immediately before invoking the executor. ToolExecutionStart only
    /// announces preflight. Approval counts confer no permissions; the host also
    /// enforces the requested capability, scope, cancellation and budget.
    pub fn mark_tool_started(
        &self,
        lease: &AttemptLease,
        call_id: &str,
    ) -> Result<EventRecord, CoreError> {
        self.mutate(&lease.session_id, |session| {
            session.verify_lease(lease)?;
            if session.has_unknown() {
                return Err(CoreError::NeedsReconciliation);
            }
            if session.lifecycle()?.ended {
                return Err(invalid("The agent already ended"));
            }
            session.check_approval(call_id)?;
            let call = session.call_mut(call_id)?;
            if call.record.attempt_id != lease.attempt_id
                || call.record.state != CallState::Prepared
            {
                return Err(invalid(
                    "A tool may start once, after preflight and before its result",
                ));
            }
            call.record.state = CallState::Started;
            call.record.execution_started = true;
            Ok((
                (),
                Some(lease.clone()),
                CoreEvent::ToolStarted {
                    call_id: call_id.into(),
                },
            ))
        })
        .map(|(_, record)| record)
    }

    pub fn finish_attempt(&self, lease: &AttemptLease) -> Result<(), CoreError> {
        self.mutate(&lease.session_id, |session| {
            session.verify_lease(lease)?;
            if session.approvals.iter().any(|a| a.decision.is_none()) {
                return Err(CoreError::PendingApproval);
            }
            let lifecycle = session.lifecycle()?;
            if (lifecycle.started && !lifecycle.ended)
                || lifecycle.turn_open
                || lifecycle.open_message.is_some()
                || session.calls.iter().any(|c| c.record.attempt_id == lease.attempt_id && !c.batch_settled)
                || session.executions.iter().any(execution_active)
                || session.capabilities.iter().any(|c| c.stage != CapabilityStage::Settled)
            {
                return Err(invalid(
                    "The attempt has no settled agent terminal or has active cells; recover it explicitly",
                ));
            }
            session.attempt = None;
            session.lifecycle = None;
            Ok((
                (),
                Some(lease.clone()),
                CoreEvent::AttemptFinished {
                    lease: lease.clone(),
                },
            ))
        })
        .map(|_| ())
    }

    /// Explicitly invalidate a lost worker. Durable known terminals stay known;
    /// never-started calls receive paired nonexecution errors. Started calls with
    /// no known terminal remain unknown until host reconciliation, never replayed.
    /// The host must stop the old executor; fencing cannot undo external effects.
    /// Pending approvals are denied with recovery as their actor. These decisions
    /// and worker fencing commit together, so no orphan approval blocks recovery.
    pub fn recover(&self, session_id: &str) -> Result<SessionSnapshot, CoreError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load(&transaction, session_id)?;
        let lease = session.attempt.clone();
        if lease.is_none()
            && !session.calls.iter().any(|c| {
                matches!(
                    c.record.state,
                    CallState::Planned | CallState::Prepared | CallState::Started
                ) || !c.batch_settled
            })
            && !session.approvals.iter().any(|a| a.decision.is_none())
            && !session.executions.iter().any(execution_active)
            && !session
                .capabilities
                .iter()
                .any(|c| c.stage != CapabilityStage::Settled)
        {
            return Ok(session.snapshot());
        }
        for index in 0..session.approvals.len() {
            let approval = &mut session.approvals[index];
            if approval.decision.is_some() {
                continue;
            }
            approval.decision = Some(ApprovalResolution {
                decision: ApprovalDecision::Deny,
                actor: "host-recovery".into(),
                decided_at: timestamp_ms(),
            });
            let approval = approval.clone();
            persist(
                &transaction,
                &mut session,
                lease.clone(),
                CoreEvent::ApprovalDecided { approval },
            )?;
        }
        session.attempt = None;
        session.lifecycle = None;
        if lease.is_some() {
            session.fence = increment(session.fence)?;
        }
        // Only canonical launch/wait registrations identify a separately tracked
        // VM. Model-supplied names and arguments never confer recovery authority.
        // Real child uncertainty remains in the independent capability records.
        let tracked_execution_calls: BTreeSet<_> = session
            .executions
            .iter()
            .map(|execution| execution.parent_call_id.clone())
            .chain(
                session
                    .execution_waits
                    .iter()
                    .map(|binding| binding.call_id.clone()),
            )
            .collect();
        let mut unknown_call_ids = vec![];
        let mut not_executed_call_ids = vec![];
        for stored in &mut session.calls {
            match stored.record.state {
                CallState::Planned | CallState::Prepared => {
                    let result = ToolResult::error(
                        "Worker recovered before this call crossed the execution boundary; it was not executed",
                    );
                    stored.record.result = Some(result_message(&stored.record.call, &result, None));
                    stored.record.state = CallState::Completed;
                    not_executed_call_ids.push(stored.record.call.id.clone());
                }
                CallState::Started if tracked_execution_calls.contains(&stored.record.call.id) => {
                    let result = ToolResult::error(
                        "Worker lost the tracked VM response; inspect its independent execution and capability records",
                    );
                    stored.record.result = Some(result_message(&stored.record.call, &result, None));
                    stored.record.state = CallState::Completed;
                }
                CallState::Started => {
                    let result = ToolResult {
                        outcome_unknown: true,
                        ..ToolResult::error(
                            "Worker lost after execution started; inspect effects before continuing",
                        )
                    };
                    stored.record.result = Some(result_message(&stored.record.call, &result, None));
                    stored.record.state = CallState::OutcomeUnknown;
                    unknown_call_ids.push(stored.record.call.id.clone());
                }
                CallState::OutcomeUnknown => unknown_call_ids.push(stored.record.call.id.clone()),
                CallState::Completed => {}
            }
            stored.batch_settled = true;
        }
        let mut recovered_capabilities = vec![];
        for call in &mut session.capabilities {
            match call.stage {
                CapabilityStage::Planned => {
                    call.stage = CapabilityStage::Settled;
                    call.outcome = Some(CapabilityOutcome::NotExecuted {
                        reason: "Worker recovered before capability execution started".into(),
                    });
                    recovered_capabilities.push(call.clone());
                }
                CapabilityStage::Started => {
                    call.stage = CapabilityStage::Settled;
                    call.outcome = Some(CapabilityOutcome::OutcomeUnknown {
                        error: "Worker lost after capability execution started; inspect effects"
                            .into(),
                    });
                    recovered_capabilities.push(call.clone());
                }
                CapabilityStage::Settled => {}
            }
        }
        let mut recovered_executions = vec![];
        for execution in &mut session.executions {
            if execution_active(execution) {
                execution.state = ExecutionState::Interrupted;
                execution.terminal = Some(ExecutionOutcome::Interrupted { reason: "Worker recovered; the VM is unavailable and must never be automatically replayed".into() });
                recovered_executions.push(execution.clone());
            }
        }
        persist(
            &transaction,
            &mut session,
            lease.clone(),
            CoreEvent::AttemptRecovered {
                lease,
                unknown_call_ids,
                not_executed_call_ids,
                executions: recovered_executions,
                capabilities: recovered_capabilities,
            },
        )?;
        transaction.commit()?;
        Ok(session.snapshot())
    }

    pub fn request_approval(
        &self,
        lease: &AttemptLease,
        input: ApprovalInput,
    ) -> Result<ApprovalRequest, CoreError> {
        identifier(&input.task_id, "task_id")?;
        identifier(&input.capability, "capability")?;
        nonempty(&input.scope, "scope")?;
        nonempty(&input.reason, "reason")?;
        self.mutate(&lease.session_id, |session| {
            session.verify_lease(lease)?;
            if session.has_unknown() {
                return Err(CoreError::NeedsReconciliation);
            }
            if session.lifecycle()?.ended {
                return Err(invalid("The agent already ended"));
            }
            session.validate_approval_target(lease, &input)?;
            if session.approvals.len() >= MAX_APPROVALS {
                return Err(invalid("Approval count limit exceeded"));
            }
            let approval = ApprovalRequest {
                id: Uuid::new_v4().to_string(),
                session_id: session.id.clone(),
                attempt_id: lease.attempt_id.clone(),
                input,
                requested_at: timestamp_ms(),
                decision: None,
            };
            session.approvals.push(approval.clone());
            Ok((
                approval.clone(),
                Some(lease.clone()),
                CoreEvent::ApprovalRequested { approval },
            ))
        })
        .map(|(approval, _)| approval)
    }

    /// Only a trusted host records the actor and decision. A prior denial in the
    /// same attempt cannot be overridden by repeated requests or approval counts.
    pub fn decide_approval(
        &self,
        session_id: &str,
        approval_id: &str,
        decision: ApprovalDecision,
        actor: &str,
    ) -> Result<ApprovalRequest, CoreError> {
        nonempty(actor, "actor")?;
        self.mutate(session_id, |session| {
            let approval = session
                .approvals
                .iter_mut()
                .find(|a| a.id == approval_id)
                .ok_or_else(|| CoreError::ApprovalNotFound(approval_id.into()))?;
            if approval.decision.is_some() {
                return Err(CoreError::InvalidInput(
                    "The approval was already decided".into(),
                ));
            }
            approval.decision = Some(ApprovalResolution {
                decision,
                actor: actor.into(),
                decided_at: timestamp_ms(),
            });
            let approval = approval.clone();
            Ok((
                approval.clone(),
                session.attempt.clone(),
                CoreEvent::ApprovalDecided { approval },
            ))
        })
        .map(|(approval, _)| approval)
    }

    /// Bind the original host Task before the tool execution boundary. Exact
    /// retries are read-only; a replacement Task or lease cannot take this call.
    pub fn bind_tool_origin(
        &self,
        lease: &AttemptLease,
        call_id: &str,
        task_id: &str,
    ) -> Result<EventRecord, CoreError> {
        identifier(call_id, "call_id")?;
        identifier(task_id, "task_id")?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load(&transaction, &lease.session_id)?;
        session.verify_lease(lease)?;
        let call = session.call(call_id)?;
        if call.record.attempt_id != lease.attempt_id {
            return Err(CoreError::StaleLease);
        }
        if let Some(record) = tool_origin_event(&transaction, &session, call_id)? {
            if matches!(&record.event, CoreEvent::ToolOriginBound { lease: old, task_id: old_task, .. } if old == lease && old_task == task_id)
            {
                return Ok(record);
            }
            return Err(invalid(
                "Tool origin is already bound to another Task or lease",
            ));
        }
        session.admit_work(lease)?;
        if call.record.state != CallState::Prepared || call.record.execution_started {
            return Err(invalid("Tool origin must be bound before execution starts"));
        }
        if session.approvals.iter().any(|approval| {
            approval.attempt_id == lease.attempt_id
                && approval.input.target
                    == ApprovalTarget::ToolCall {
                        call_id: call_id.into(),
                    }
                && approval.input.task_id != task_id
        }) {
            return Err(invalid(
                "Tool origin differs from its original approval Task",
            ));
        }
        let record = persist(
            &transaction,
            &mut session,
            Some(lease.clone()),
            CoreEvent::ToolOriginBound {
                call_id: call_id.into(),
                lease: lease.clone(),
                task_id: task_id.into(),
            },
        )?;
        transaction.commit()?;
        Ok(record)
    }

    /// Read the immutable origin for a reconciliation request. This supplies no
    /// permission; submission checks the origin again and requires a verifier.
    pub fn tool_reconciliation_origin(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<Box<ToolReconciliationOrigin>, CoreError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let session = load(&transaction, session_id)?;
        let origin = tool_origin(&transaction, &session, call_id)?;
        transaction.commit()?;
        Ok(Box::new(origin))
    }

    /// Record a new decision without replacing the original model tool reply.
    /// The host verifier authorizes the actor and checks its own evidence format.
    pub fn reconcile(
        &self,
        session_id: &str,
        reconciliation: Reconciliation,
    ) -> Result<SessionSnapshot, CoreError> {
        let reconciliation = checked_input(reconciliation)?;
        nonempty(&reconciliation.actor, "actor")?;
        nonempty(&reconciliation.evidence, "evidence")?;
        match &reconciliation.outcome {
            ReconciledOutcome::Completed { result } => {
                if result.outcome_unknown {
                    return Err(invalid("A reconciled result must have a known outcome"));
                }
                validate_result(result)?;
            }
            ReconciledOutcome::NotExecuted { reason } => nonempty(reason, "reason")?,
        }
        self.verify_reconciliation(&ReconciliationRequest::Tool {
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
        if tool_origin(&transaction, &session, &reconciliation.call_id)? != *reconciliation.expected
        {
            return Err(invalid(
                "Reconciliation differs from the complete original tool identity",
            ));
        }
        // A known child terminal proves its launch executed. It does not prove
        // that a later wait observing that execution was itself dispatched.
        if matches!(
            reconciliation.outcome,
            ReconciledOutcome::NotExecuted { .. }
        ) && let Some(execution) = &reconciliation.expected.execution
            && execution.parent_call_id == reconciliation.call_id
            && session.capabilities.iter().any(|call| {
                call.key.execution_id == execution.execution_id
                    && matches!(
                        call.outcome,
                        Some(
                            CapabilityOutcome::Completed { .. } | CapabilityOutcome::Failed { .. }
                        )
                    )
            })
        {
            return Err(invalid(
                "Tool nonexecution claim contradicts its reliable original capability terminal",
            ));
        }
        if let Some(prior) = reconciliation_events(&transaction, &session)?
            .into_iter()
            .find_map(|event| match event {
                CoreEvent::Reconciled {
                    reconciliation: prior,
                } if prior.call_id == reconciliation.call_id => Some(prior),
                _ => None,
            })
        {
            if prior != reconciliation {
                return Err(invalid("Reconciliation contradicts the recorded decision"));
            }
            return Ok(session.snapshot());
        }
        let call = session.call_mut(&reconciliation.call_id)?;
        if call.record.state != CallState::OutcomeUnknown {
            return Err(invalid("Only an unknown tool outcome can be reconciled"));
        }
        // The execution decision is new. The original model reply is evidence
        // of what the worker observed and must remain byte-for-byte intact.
        call.record.state = CallState::Completed;
        call.batch_settled = true;
        persist(
            &transaction,
            &mut session,
            None,
            CoreEvent::Reconciled { reconciliation },
        )?;
        transaction.commit()?;
        Ok(session.snapshot())
    }

    fn mutate<T>(
        &self,
        session_id: &str,
        change: impl FnOnce(
            &mut StoredSession,
        ) -> Result<(T, Option<AttemptLease>, CoreEvent), CoreError>,
    ) -> Result<(T, EventRecord), CoreError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Reuse the projection only when the complete persisted value still
        // matches the cached one; otherwise reload and validate from SQLite.
        let mut session = self.cached_projection(&transaction, session_id)?;
        // Remove only this session before mutation. On error its next read
        // reloads the committed projection; other sessions remain cached.
        self.projection_cache
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?
            .take(session_id);
        let state = Arc::make_mut(&mut session);
        let (result, lease, event) = change(state)?;
        let record = persist(&transaction, state, lease, event)?;
        #[cfg(feature = "fault-injection")]
        let crash_point = match &record.event {
            CoreEvent::Agent { event } => match event.as_ref() {
                AgentEvent::ToolExecutionEnd { call, .. } => Some((
                    "core.tool_result.before",
                    "core.tool_result.committed",
                    serde_json::json!({
                        "session_id": record.session_id, "lease": record.attempt, "call_id": call.id, "cursor": record.cursor,
                    }),
                )),
                _ => None,
            },
            CoreEvent::AttemptFinished { lease } => Some((
                "core.attempt_terminal.before",
                "core.attempt_terminal.committed",
                serde_json::json!({
                    "session_id": record.session_id, "lease": lease, "cursor": record.cursor,
                }),
            )),
            _ => None,
        };
        #[cfg(feature = "fault-injection")]
        if let Some((before, _, data)) = &crash_point {
            cayw_ai::faults::hit(before, data.clone());
        }
        // Read back the projection written in this transaction instead of
        // encoding it again. It is published only after a successful commit.
        let json = projection_json(&transaction, session_id)?;
        transaction.commit()?;
        #[cfg(feature = "fault-injection")]
        if let Some((_, committed, data)) = crash_point {
            cayw_ai::faults::hit(committed, data);
        }
        self.projection_cache
            .lock()
            .map_err(|_| CoreError::StorePoisoned)?
            .insert(CachedProjection { json, session });
        Ok((result, record))
    }
}

const EVENTS_BY_TYPE: &str = "SELECT cursor, record FROM core_events
    WHERE session_id=?1 AND json_extract(record, '$.event.type')=?2 AND cursor>?3
    ORDER BY cursor LIMIT ?4";
const HOST_EVENTS_BY_KIND: &str = "SELECT cursor, record FROM core_events
    WHERE session_id=?1 AND json_extract(record, '$.event.type')='host'
    AND json_extract(record, '$.event.kind')=?2 AND cursor>?3
    ORDER BY cursor LIMIT ?4";

fn recorded_events(
    connection: &Connection,
    session: &StoredSession,
    kind: &str,
) -> Result<Vec<EventRecord>, CoreError> {
    let mut query = connection.prepare(
        "SELECT cursor,record FROM core_events WHERE session_id=?1 AND json_extract(record,'$.event.type')=?2 ORDER BY cursor",
    )?;
    let rows = query.query_map(params![session.id, kind], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut events = Vec::new();
    for row in rows {
        let (cursor, json) = row?;
        let record = checked_input(serde_json::from_str::<EventRecord>(&json)?)?;
        if record.session_id != session.id
            || record.protocol_version != PROTOCOL_VERSION
            || cursor < 1
            || record.cursor != cursor as u64
            || cursor as u64 > session.cursor
        {
            return Err(CoreError::CorruptStore(
                "Original event envelope is inconsistent".into(),
            ));
        }
        events.push(record);
    }
    Ok(events)
}

fn reconciliation_events(
    connection: &Connection,
    session: &StoredSession,
) -> Result<Vec<CoreEvent>, CoreError> {
    Ok(recorded_events(connection, session, "reconciled")?
        .into_iter()
        .chain(recorded_events(
            connection,
            session,
            "capability_reconciled",
        )?)
        .map(|record| record.event)
        .collect())
}

fn tool_origin_event(
    connection: &Connection,
    session: &StoredSession,
    call_id: &str,
) -> Result<Option<EventRecord>, CoreError> {
    let mut found = None;
    for record in recorded_events(connection, session, "tool_origin_bound")? {
        if let CoreEvent::ToolOriginBound {
            call_id: id,
            lease,
            task_id,
        } = &record.event
            && id == call_id
        {
            if found.is_some()
                || record.attempt.as_ref() != Some(lease)
                || lease.session_id != session.id
                || task_id.is_empty()
            {
                return Err(CoreError::CorruptStore(
                    "Tool origin registration is inconsistent".into(),
                ));
            }
            found = Some(record);
        }
    }
    Ok(found)
}

fn tool_origin(
    connection: &Connection,
    session: &StoredSession,
    call_id: &str,
) -> Result<ToolReconciliationOrigin, CoreError> {
    let call = &session.call(call_id)?.record;
    let execution = session
        .executions
        .iter()
        .find(|execution| execution.parent_call_id == call_id)
        .or_else(|| {
            session
                .execution_waits
                .iter()
                .find(|wait| wait.call_id == call_id)
                .and_then(|wait| {
                    session
                        .executions
                        .iter()
                        .find(|execution| execution.execution_id == wait.execution_id)
                })
        });
    let bound = tool_origin_event(connection, session, call_id)?;
    let (lease, task_id) = match bound.as_ref().map(|record| &record.event) {
        Some(CoreEvent::ToolOriginBound { lease, task_id, .. }) => (lease.clone(), task_id.clone()),
        _ => execution
            .map(|execution| (execution.lease.clone(), execution.task_id.clone()))
            .ok_or_else(|| invalid("Tool has no original host Task binding"))?,
    };
    if lease.session_id != session.id
        || lease.attempt_id != call.attempt_id
        || !call.execution_started
    {
        return Err(invalid(
            "Tool identity differs from its original execution boundary",
        ));
    }
    let started = recorded_events(connection, session, "attempt_started")?;
    if started.iter().filter(|record| matches!(&record.event, CoreEvent::AttemptStarted { lease: original } if original == &lease) && record.attempt.as_ref() == Some(&lease)).count() != 1 {
        return Err(CoreError::CorruptStore("Tool has no unique original attempt lease".into()));
    }
    let execution = execution.map(|execution| -> Result<ExecutionInput, CoreError> {
        if execution.lease != lease || execution.task_id != task_id {
            return Err(invalid("Tool Task binding differs from its original execution"));
        }
        let events = recorded_events(connection, session, "execution_started")?;
        if events.iter().filter(|record| matches!(&record.event, CoreEvent::ExecutionStarted { execution: original }
            if original.execution_id == execution.execution_id && original.parent_call_id == execution.parent_call_id
                && original.task_id == task_id && original.source_hash == execution.source_hash && original.lease == lease)
            && record.attempt.as_ref() == Some(&lease)).count() != 1 {
            return Err(CoreError::CorruptStore("Tool execution has no unique original registration".into()));
        }
        Ok(ExecutionInput {
            execution_id: execution.execution_id.clone(), parent_call_id: execution.parent_call_id.clone(),
            task_id: task_id.clone(), source_hash: execution.source_hash.clone(),
        })
    }).transpose()?;
    Ok(ToolReconciliationOrigin {
        lease,
        task_id,
        call: call.call.clone(),
        execution,
    })
}

fn committed_tool_reconciliation(
    connection: &Connection,
    session_id: &str,
    call_id: &str,
) -> Result<Reconciliation, CoreError> {
    let mut session = load(connection, session_id)?;
    let mut cutoff = session.cursor;
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(session.id.clone()) || cutoff > session.cursor {
            return Err(CoreError::CorruptStore(
                "Fork reconciliation source is cyclic or beyond its recorded cursor".into(),
            ));
        }
        let stored = session.call(call_id)?;
        let result = stored
            .record
            .result
            .as_ref()
            .ok_or(CoreError::NeedsReconciliation)?;
        if stored.record.state != CallState::Completed
            || !stored.batch_settled
            || !result.outcome_unknown
            || result.tool_call_id != call_id
            || result.tool_name != stored.record.call.name
        {
            return Err(CoreError::NeedsReconciliation);
        }
        let decisions: Vec<_> = recorded_events(connection, &session, "reconciled")?
            .into_iter()
            .filter(|event| event.cursor <= cutoff)
            .filter_map(|event| match event.event {
                CoreEvent::Reconciled { reconciliation } if reconciliation.call_id == call_id => {
                    Some(reconciliation)
                }
                _ => None,
            })
            .collect();
        if !decisions.is_empty() {
            if decisions.len() != 1 {
                return Err(CoreError::CorruptStore(
                    "Original tool has duplicate committed decisions".into(),
                ));
            }
            let decision = decisions.into_iter().next().expect("one decision");
            if *decision.expected != tool_origin(connection, &session, call_id)? {
                return Err(CoreError::CorruptStore(
                    "Committed tool decision differs from its complete original identity".into(),
                ));
            }
            let known = match &decision.outcome {
                ReconciledOutcome::Completed { result } => {
                    validate_result(result)?;
                    !result.outcome_unknown
                }
                ReconciledOutcome::NotExecuted { reason } => {
                    nonempty(reason, "reason")?;
                    true
                }
            };
            if !known {
                return Err(CoreError::NeedsReconciliation);
            }
            return Ok(decision);
        }
        let forks: Vec<_> = recorded_events(connection, &session, "session_forked")?
            .into_iter()
            .filter(|event| event.cursor <= cutoff)
            .collect();
        if forks.len() != 1 {
            return Err(CoreError::NeedsReconciliation);
        }
        let CoreEvent::SessionForked {
            source_session_id,
            source_cursor,
            messages,
        } = &forks[0].event
        else {
            unreachable!("selected fork event")
        };
        if !messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(original) if original == result))
        {
            return Err(CoreError::NeedsReconciliation);
        }
        let source = load(connection, source_session_id)?;
        let source_call = source.call(call_id)?;
        if *source_cursor == 0
            || *source_cursor > source.cursor
            || source_call.record != stored.record
            || source_call.batch_settled != stored.batch_settled
            || source_call.result_acknowledged != stored.result_acknowledged
            || !source.messages().starts_with(messages)
        {
            return Err(CoreError::CorruptStore(
                "Fork reconciliation differs from its recorded original history".into(),
            ));
        }
        cutoff = *source_cursor;
        session = source;
    }
}

fn increment(value: u64) -> Result<u64, CoreError> {
    value
        .checked_add(1)
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or(CoreError::CounterExhausted)
}
fn projection_json(connection: &Connection, id: &str) -> Result<String, CoreError> {
    connection
        .query_row(
            "SELECT projection FROM core_sessions WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| CoreError::SessionNotFound(id.into()))
}
fn load(connection: &Connection, id: &str) -> Result<StoredSession, CoreError> {
    decode_projection(connection, id, &projection_json(connection, id)?)
}
fn decode_projection(
    connection: &Connection,
    id: &str,
    json: &str,
) -> Result<StoredSession, CoreError> {
    let session = checked_input(serde_json::from_str::<StoredSession>(json)?)?;
    if session.id != id || session.cursor > i64::MAX as u64 || session.fence > i64::MAX as u64 {
        return Err(CoreError::CorruptStore(
            "The session identity or counter is inconsistent".into(),
        ));
    }
    session.validate_wait_bindings()?;
    waits::validate_wait_log(connection, &session)?;
    Ok(session)
}
fn insert_session(connection: &Connection, session: &StoredSession) -> Result<(), CoreError> {
    connection.execute(
        "INSERT INTO core_sessions (id, projection) VALUES (?1, ?2)",
        params![session.id, encode_readable(session)?],
    )?;
    Ok(())
}
fn persist(
    connection: &Connection,
    session: &mut StoredSession,
    lease: Option<AttemptLease>,
    event: CoreEvent,
) -> Result<EventRecord, CoreError> {
    session.cursor = increment(session.cursor)?;
    let record = EventRecord {
        protocol_version: PROTOCOL_VERSION,
        session_id: session.id.clone(),
        cursor: session.cursor,
        timestamp: timestamp_ms(),
        attempt: lease,
        event,
    };
    session.validate_wait_bindings()?;
    let record_json = encode_readable(&record)?;
    let projection_json = encode_readable(session)?;
    connection.execute(
        "INSERT INTO core_events (session_id, cursor, record) VALUES (?1, ?2, ?3)",
        params![session.id, session.cursor as i64, record_json],
    )?;
    connection.execute(
        "UPDATE core_sessions SET projection=?2 WHERE id=?1",
        params![session.id, projection_json],
    )?;
    Ok(record)
}

fn validate_result(result: &ToolResult) -> Result<(), CoreError> {
    check_values(result)?;
    if result
        .content
        .iter()
        .any(|part| !matches!(part, Content::Text { .. } | Content::Image { .. }))
    {
        return Err(invalid("Tool results may contain only text and images"));
    }
    if result.outcome_unknown && !result.is_error {
        return Err(invalid("An unknown tool outcome must also be an error"));
    }
    Ok(())
}
fn result_message(
    call: &ToolCall,
    result: &ToolResult,
    duration_ms: Option<u64>,
) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: result.content.clone(),
        details: result.details.clone(),
        structured_content: result.structured_content.clone(),
        usage: result.usage.clone(),
        is_error: result.is_error,
        outcome_unknown: result.outcome_unknown,
        timestamp: timestamp_ms(),
        duration_ms,
    }
}
fn same_result(a: &ToolResultMessage, b: &ToolResultMessage) -> bool {
    // The durable terminal and the later Agent message have separate observation timestamps.
    a.tool_call_id == b.tool_call_id
        && a.tool_name == b.tool_name
        && a.content == b.content
        && a.details == b.details
        && a.structured_content == b.structured_content
        && a.usage == b.usage
        && a.is_error == b.is_error
        && a.outcome_unknown == b.outcome_unknown
        && a.duration_ms == b.duration_ms
}
fn same_assistant_stream(a: &AssistantMessage, b: &AssistantMessage) -> bool {
    a.provider == b.provider && a.model == b.model
}

fn apply_agent_event(
    session: &mut StoredSession,
    lease: &AttemptLease,
    event: &AgentEvent,
) -> Result<(), CoreError> {
    let lifecycle = session.lifecycle()?;
    if lifecycle.ended {
        return Err(invalid("No agent event may follow AgentEnd"));
    }
    if !lifecycle.started && !matches!(event, AgentEvent::AgentStart) {
        return Err(invalid("The first agent event must be AgentStart"));
    }
    match event {
        AgentEvent::AgentStart => {
            if lifecycle.started {
                return Err(invalid("AgentStart was already recorded"));
            }
            session.lifecycle_mut()?.started = true;
        }
        AgentEvent::AgentEnd { .. } => {
            if lifecycle.turn_open
                || lifecycle.open_message.is_some()
                || session
                    .calls
                    .iter()
                    .any(|c| c.record.attempt_id == lease.attempt_id && !c.batch_settled)
            {
                return Err(invalid("AgentEnd requires all message and turn terminals"));
            }
            session.lifecycle_mut()?.ended = true;
        }
        AgentEvent::TurnStart => {
            if lifecycle.turn_open {
                return Err(invalid("The previous turn has not ended"));
            }
            // An error turn may report an unknown outcome, but cannot start new work.
            let lifecycle = session.lifecycle_mut()?;
            lifecycle.turn_open = true;
            lifecycle.assistant = None;
        }
        AgentEvent::MessageStart { message } => {
            if !lifecycle.turn_open || lifecycle.open_message.is_some() {
                return Err(invalid(
                    "A message requires an open turn and no other open message",
                ));
            }
            if let Message::Assistant(assistant) = message
                && session.has_unknown()
                && !assistant.stop_reason.is_failure()
            {
                return Err(CoreError::NeedsReconciliation);
            }
            if let Message::ToolResult(result) = message {
                let call = session.call(&result.tool_call_id)?;
                let expected = lifecycle
                    .assistant
                    .as_ref()
                    .into_iter()
                    .flat_map(|m| m.tool_calls())
                    .find(|c| {
                        session
                            .call(&c.id)
                            .is_ok_and(|stored| !stored.result_acknowledged)
                    });
                if expected.as_ref().map(|c| c.id.as_str()) != Some(result.tool_call_id.as_str())
                    || call.record.attempt_id != lease.attempt_id
                    || call.result_acknowledged
                    || !call
                        .record
                        .result
                        .as_ref()
                        .is_some_and(|stored| same_result(stored, result))
                {
                    return Err(invalid(
                        "Tool result does not match the next persisted terminal in source order",
                    ));
                }
            }
            session.lifecycle_mut()?.open_message = Some(message.clone());
        }
        AgentEvent::MessageUpdate { message, event } => {
            let Some(Message::Assistant(open)) = &lifecycle.open_message else {
                return Err(invalid("MessageUpdate requires an open assistant message"));
            };
            if !same_assistant_stream(open, message)
                || event.is_terminal()
                || event.message() != message
            {
                return Err(invalid(
                    "A message update must match its open assistant stream and nonterminal event",
                ));
            }
            session.lifecycle_mut()?.open_message = Some(Message::Assistant(message.clone()));
        }
        AgentEvent::MessageEnd { message } => {
            let open = lifecycle
                .open_message
                .as_ref()
                .ok_or_else(|| invalid("MessageEnd has no matching MessageStart"))?;
            let matches = match (open, message) {
                (Message::Assistant(a), Message::Assistant(b)) => same_assistant_stream(a, b),
                _ => open == message,
            };
            if !matches {
                return Err(invalid("MessageEnd does not match MessageStart"));
            }
            append_message(session, lease, message)?;
            session.lifecycle_mut()?.open_message = None;
        }
        AgentEvent::ToolExecutionStart { call } => {
            if !lifecycle.turn_open || lifecycle.open_message.is_some() {
                return Err(invalid(
                    "Tool preflight requires a completed assistant message",
                ));
            }
            // Even after another call becomes unknown, the Agent must be able
            // to record a preflight rejection for each already declared call.
            // mark_tool_started remains the only execution boundary and blocks.
            let stored = session.call_mut(&call.id)?;
            if stored.record.call != *call
                || stored.record.attempt_id != lease.attempt_id
                || stored.record.state != CallState::Planned
            {
                return Err(invalid("Tool preflight must match one planned call"));
            }
            stored.record.state = CallState::Prepared;
        }
        AgentEvent::ToolExecutionUpdate { call, result } => {
            validate_result(result)?;
            let stored = session.call(&call.id)?;
            if stored.record.call != *call
                || stored.record.attempt_id != lease.attempt_id
                || stored.record.state != CallState::Started
            {
                return Err(invalid("Progress requires the matching started tool"));
            }
        }
        AgentEvent::ToolExecutionEnd {
            call,
            result,
            duration_ms,
        } => {
            validate_result(result)?;
            let stored = session.call_mut(&call.id)?;
            if stored.record.call != *call
                || stored.record.attempt_id != lease.attempt_id
                || !matches!(
                    stored.record.state,
                    CallState::Prepared | CallState::Started
                )
            {
                return Err(invalid(
                    "A tool terminal must match an unfinished preflight or execution",
                ));
            }
            if !stored.record.execution_started
                && (!result.is_error || duration_ms.is_some() || result.outcome_unknown)
            {
                return Err(invalid(
                    "A tool must cross mark_tool_started before returning execution results",
                ));
            }
            stored.record.result = Some(result_message(call, result, *duration_ms));
            stored.record.state = if result.outcome_unknown {
                CallState::OutcomeUnknown
            } else {
                CallState::Completed
            };
        }
        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => {
            if !lifecycle.turn_open
                || lifecycle.open_message.is_some()
                || lifecycle.assistant.as_ref() != Some(message)
            {
                return Err(invalid(
                    "TurnEnd must match the completed assistant message in this turn",
                ));
            }
            let expected = if message.stop_reason.is_failure() {
                vec![]
            } else {
                message.tool_calls()
            };
            if expected.len() != tool_results.len() {
                return Err(invalid(
                    "TurnEnd must settle every tool call in source order",
                ));
            }
            for (call, result) in expected.iter().zip(tool_results) {
                let stored = session.call(&call.id)?;
                if stored.record.attempt_id != lease.attempt_id
                    || stored.batch_settled
                    || !stored
                        .record
                        .result
                        .as_ref()
                        .is_some_and(|persisted| same_result(persisted, result))
                {
                    return Err(invalid(
                        "TurnEnd contains an absent, duplicate or mismatched tool result",
                    ));
                }
            }
            for call in expected {
                session.call_mut(&call.id)?.batch_settled = true;
            }
            session.lifecycle_mut()?.turn_open = false;
        }
    }
    Ok(())
}

fn append_message(
    session: &mut StoredSession,
    lease: &AttemptLease,
    message: &Message,
) -> Result<(), CoreError> {
    match message {
        Message::Assistant(assistant) => {
            if session.lifecycle()?.assistant.is_some() {
                return Err(invalid(
                    "A turn can have only one completed assistant message",
                ));
            }
            if assistant.stop_reason == StopReason::Pending {
                return Err(invalid("An assistant terminal may not be pending"));
            }
            if session.has_unknown() && !assistant.stop_reason.is_failure() {
                return Err(CoreError::NeedsReconciliation);
            }
            let calls = if assistant.stop_reason.is_failure() {
                vec![]
            } else {
                assistant.tool_calls()
            };
            if assistant.stop_reason == StopReason::ToolUse && calls.is_empty() {
                return Err(invalid("Tool-use terminal has no calls"));
            }
            if assistant.stop_reason == StopReason::Stop && !calls.is_empty() {
                return Err(invalid("A normal stop cannot execute tool calls"));
            }
            let mut ids = BTreeSet::new();
            for call in &calls {
                nonempty(&call.id, "tool call id")?;
                nonempty(&call.name, "tool name")?;
                if !ids.insert(&call.id)
                    || session
                        .calls
                        .iter()
                        .any(|stored| stored.record.call.id == call.id)
                {
                    return Err(invalid(
                        "Tool call identifiers must be unique throughout a session",
                    ));
                }
            }
            session.history.push(HistoryEntry::Message {
                message: Box::new(message.clone()),
            });
            for call in calls {
                session.history.push(HistoryEntry::ToolSlot {
                    call_id: call.id.clone(),
                });
                session.calls.push(StoredCall {
                    record: CallRecord {
                        call,
                        attempt_id: lease.attempt_id.clone(),
                        state: CallState::Planned,
                        execution_started: false,
                        result: None,
                    },
                    batch_settled: false,
                    result_acknowledged: false,
                });
            }
            session.lifecycle_mut()?.assistant = Some(assistant.clone());
        }
        Message::ToolResult(result) => {
            let stored = session.call_mut(&result.tool_call_id)?;
            if stored.record.attempt_id != lease.attempt_id
                || stored.result_acknowledged
                || !stored
                    .record
                    .result
                    .as_ref()
                    .is_some_and(|persisted| same_result(persisted, result))
            {
                return Err(invalid(
                    "A tool message must acknowledge one matching recorded result",
                ));
            }
            stored.result_acknowledged = true;
        }
        Message::User { content, .. } => {
            if content
                .iter()
                .any(|part| !matches!(part, Content::Text { .. } | Content::Image { .. }))
            {
                return Err(invalid("User messages may contain only text and images"));
            }
            if session.lifecycle()?.assistant.is_some() {
                return Err(invalid(
                    "User input must precede the turn's assistant terminal",
                ));
            }
            session.history.push(HistoryEntry::Message {
                message: Box::new(message.clone()),
            });
        }
        _ => session.history.push(HistoryEntry::Message {
            message: Box::new(message.clone()),
        }),
    }
    Ok(())
}

fn validate_paired_history(
    messages: &[Message],
    mut verify_unknown: impl FnMut(&ToolResultMessage) -> Result<(), CoreError>,
) -> Result<(), CoreError> {
    let mut ids = BTreeSet::new();
    let mut pending = std::collections::VecDeque::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) if !assistant.stop_reason.is_failure() => {
                if !pending.is_empty() {
                    return Err(invalid("Fork history has missing tool results"));
                }
                if assistant.stop_reason == StopReason::Pending {
                    return Err(invalid("Fork history contains a pending assistant"));
                }
                for call in assistant.tool_calls() {
                    if !ids.insert(call.id.clone()) {
                        return Err(invalid("Fork history has duplicate tool identifiers"));
                    }
                    pending.push_back(call);
                }
            }
            Message::ToolResult(result) => {
                let call = pending
                    .pop_front()
                    .ok_or_else(|| invalid("Fork history has an unpaired tool result"))?;
                if call.id != result.tool_call_id || call.name != result.tool_name {
                    return Err(invalid("Fork history has a mismatched tool result"));
                }
                if result.outcome_unknown {
                    verify_unknown(result)?;
                }
            }
            _ if !pending.is_empty() => {
                return Err(invalid(
                    "Fork history contains messages before tool pairing completed",
                ));
            }
            _ => {}
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(invalid("Fork history has unfinished tool calls"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_events_use_indexes_and_preserve_the_original_log() {
        let directory =
            std::env::temp_dir().join(format!("cayw-selected-events-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("session.db");
        let store = SessionStore::open(&path).unwrap();
        let session = store.create_session("selected event fixture").unwrap();
        let lease = store.begin_attempt(&session.id, "worker").unwrap();
        for event in [
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::MessageStart {
                message: Message::Assistant(AssistantMessage::new(&cayw_ai::Model::new(
                    "fixture", "stream",
                ))),
            },
        ] {
            store.append_agent_event(&lease, &event).unwrap();
        }
        let first = store
            .append_host_event(&lease, "ledger_fixture", serde_json::json!({"entry": 1}))
            .unwrap();
        let mut message = AssistantMessage::new(&cayw_ai::Model::new("fixture", "stream"));
        for fragment in 0..512 {
            message.content = vec![Content::text("x".repeat(fragment * 8 + 1))];
            store
                .append_agent_event(
                    &lease,
                    &AgentEvent::MessageUpdate {
                        message: message.clone(),
                        event: Box::new(cayw_ai::AssistantEvent::TextDelta {
                            index: 0,
                            delta: "x".into(),
                            partial: message.clone(),
                        }),
                    },
                )
                .unwrap();
        }
        let peer = SessionStore::open(&path).unwrap();
        let second = peer
            .append_host_event(&lease, "ledger_fixture", serde_json::json!({"entry": 2}))
            .unwrap();
        peer.append_host_event(&lease, "other_fixture", serde_json::json!({"entry": 3}))
            .unwrap();
        let original = store.events(&session.id, 0, 1000).unwrap();
        assert_eq!(original.iter().filter(|record| matches!(record.event, CoreEvent::Agent { ref event } if matches!(event.as_ref(), AgentEvent::MessageUpdate { .. }))).count(), 512);
        assert_eq!(
            store
                .events_by_kind(&session.id, 0, 1, "host", Some("ledger_fixture"))
                .unwrap()
                .as_slice(),
            std::slice::from_ref(&first)
        );
        assert_eq!(
            store
                .events_by_kind(&session.id, first.cursor, 1, "host", Some("ledger_fixture"))
                .unwrap()
                .as_slice(),
            std::slice::from_ref(&second)
        );
        assert!(
            store
                .events_by_kind(
                    &session.id,
                    second.cursor,
                    1,
                    "host",
                    Some("ledger_fixture")
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .events_by_kind(&session.id, 0, 1000, "agent", None)
                .unwrap()
                .len(),
            515
        );
        assert!(matches!(
            store.events_by_kind(&session.id, u64::MAX, 1, "host", Some("ledger_fixture")),
            Err(CoreError::FutureCursor { .. })
        ));
        {
            let connection = store.connection.lock().unwrap();
            for (sql, kind, index) in [
                (EVENTS_BY_TYPE, "session_forked", "core_events_by_type"),
                (
                    HOST_EVENTS_BY_KIND,
                    "ledger_fixture",
                    "core_host_events_by_kind",
                ),
            ] {
                let mut statement = connection
                    .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                    .unwrap();
                let plan = statement
                    .query_map(params![session.id, kind, 0_i64, 1000_i64], |row| {
                        row.get::<_, String>(3)
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                assert!(plan.iter().any(|step| step.contains(index)), "{plan:?}");
                assert!(
                    !plan
                        .iter()
                        .any(|step| step.contains("SCAN") || step.contains("TEMP B-TREE")),
                    "{plan:?}"
                );
            }
        }
        drop(peer);
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("DROP INDEX core_events_by_type; DROP INDEX core_host_events_by_kind;")
            .unwrap();
        drop(connection);
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(reopened.events(&session.id, 0, 1000).unwrap(), original);
        assert_eq!(
            reopened
                .events_by_kind(&session.id, 0, 1000, "host", Some("ledger_fixture"))
                .unwrap(),
            [first, second]
        );
        // The selected reader must still refuse a damaged prefix, including a
        // missing unrelated message. Only this synthetic database loses guards.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("DROP TRIGGER core_events_no_delete")
            .unwrap();
        connection
            .execute(
                "DELETE FROM core_events WHERE session_id=?1 AND cursor=?2",
                params![session.id, original[10].cursor as i64],
            )
            .unwrap();
        assert!(matches!(
            reopened.events_by_kind(&session.id, 0, 1000, "host", Some("ledger_fixture")),
            Err(CoreError::CorruptStore(_))
        ));
        drop(connection);
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cached_sessions_survive_rotation_and_evict_the_least_recently_used() {
        let directory =
            std::env::temp_dir().join(format!("cayw-cache-rotation-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let store = SessionStore::open(directory.join("session.db")).unwrap();
        let mut sessions = Vec::new();
        let mut originals = Vec::new();
        for index in 0..PROJECTION_CACHE_ENTRIES {
            let session = store.create_session(format!("session {index}")).unwrap();
            let lease = store.begin_attempt(&session.id, "worker").unwrap();
            store
                .append_host_event(&lease, "cache_fixture", serde_json::json!({"index": index}))
                .unwrap();
            let connection = store.connection.lock().unwrap();
            originals.push(store.cached_projection(&connection, &session.id).unwrap());
            sessions.push(session.id);
        }
        for (id, original) in sessions.iter().zip(&originals) {
            let connection = store.connection.lock().unwrap();
            let cached = store.cached_projection(&connection, id).unwrap();
            assert!(Arc::ptr_eq(&cached, original));
        }
        // Refresh the oldest, so the next insertion must evict the second.
        store.snapshot(&sessions[0]).unwrap();
        let extra = store.create_session("one more").unwrap();
        store.begin_attempt(&extra.id, "worker").unwrap();
        {
            let connection = store.connection.lock().unwrap();
            assert!(Arc::ptr_eq(
                &store.cached_projection(&connection, &sessions[0]).unwrap(),
                &originals[0]
            ));
            let reloaded = store.cached_projection(&connection, &sessions[1]).unwrap();
            assert!(!Arc::ptr_eq(&reloaded, &originals[1]));
            assert_eq!(reloaded.snapshot(), originals[1].snapshot());
        }
        let cache = store.projection_cache.lock().unwrap();
        assert_eq!(cache.entries.len(), PROJECTION_CACHE_ENTRIES);
        assert_eq!(
            cache.json_bytes,
            cache
                .entries
                .iter()
                .map(|entry| entry.json.len())
                .sum::<usize>()
        );
        drop(cache);
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cache_json_budget_evicts_old_entries_and_skips_oversized_projections() {
        let entry = |id: &str, bytes| CachedProjection {
            json: "x".repeat(bytes),
            session: Arc::new(StoredSession::new(id.into(), id.into())),
        };
        let mut cache = ProjectionCache::default();
        let half = PROJECTION_CACHE_JSON_BYTES / 2;
        cache.insert(entry("first", half));
        cache.insert(entry("second", half));
        cache.insert(entry("third", half));
        assert!(cache.take("first").is_none());
        assert_eq!(cache.json_bytes, PROJECTION_CACHE_JSON_BYTES);
        cache.insert(entry("third", PROJECTION_CACHE_JSON_BYTES + 1));
        assert!(cache.take("third").is_none());
        assert_eq!(cache.json_bytes, half);
        assert!(cache.take("second").is_some());
        assert_eq!(cache.json_bytes, 0);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn ordinary_write_commit_failure_preserves_history_and_allows_retry() {
        let directory = std::env::temp_dir().join(format!("cayw-commit-retry-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let store = SessionStore::open(directory.join("session.db")).unwrap();
        let session = store.create_session("commit retry").unwrap();
        let lease = store.begin_attempt(&session.id, "worker").unwrap();
        let before = store.snapshot(&session.id).unwrap();
        let events = store.events(&session.id, 0, 100).unwrap();
        let peer = store.create_session("unrelated cached session").unwrap();
        store.begin_attempt(&peer.id, "peer-worker").unwrap();
        let peer_cached = {
            let connection = store.connection.lock().unwrap();
            store.cached_projection(&connection, &peer.id).unwrap()
        };
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE fixture_parent(id INTEGER PRIMARY KEY);
                 CREATE TABLE fixture_deferred(parent INTEGER REFERENCES fixture_parent(id) DEFERRABLE INITIALLY DEFERRED);
                 CREATE TRIGGER fail_host_commit AFTER INSERT ON core_events
                 WHEN json_extract(NEW.record, '$.event.type')='host'
                 BEGIN INSERT INTO fixture_deferred VALUES(1); END;",
            )
            .unwrap();
        assert!(matches!(
            store.append_host_event(&lease, "commit_fixture", serde_json::json!({"value": 1})),
            Err(CoreError::Database(_))
        ));
        {
            let cache = store.projection_cache.lock().unwrap();
            assert!(
                !cache
                    .entries
                    .iter()
                    .any(|entry| entry.session.id == session.id)
            );
            assert!(
                cache
                    .entries
                    .iter()
                    .any(|entry| Arc::ptr_eq(&entry.session, &peer_cached))
            );
        }
        assert_eq!(store.snapshot(&session.id).unwrap(), before);
        assert_eq!(store.events(&session.id, 0, 100).unwrap(), events);
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_host_commit")
            .unwrap();
        let committed = store
            .append_host_event(&lease, "commit_fixture", serde_json::json!({"value": 1}))
            .unwrap();
        assert_eq!(committed.cursor, before.cursor + 1);
        assert_eq!(
            store.events(&session.id, before.cursor, 100).unwrap(),
            [committed]
        );
        store.finish_attempt(&lease).unwrap();
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn counter_exhaustion_cannot_wrap_or_cross_sqlite_integer_range() {
        assert_eq!(increment(0).unwrap(), 1);
        assert!(matches!(
            increment(i64::MAX as u64),
            Err(CoreError::CounterExhausted)
        ));
        assert!(matches!(
            increment(u64::MAX),
            Err(CoreError::CounterExhausted)
        ));
    }

    #[test]
    fn memory_database_is_rejected_instead_of_claiming_durability() {
        assert!(matches!(
            SessionStore::open(":memory:"),
            Err(CoreError::InvalidInput(_))
        ));
    }
}
