use crate::{
    OperationCatalog, RequestError, Result,
    authority::{AuthorityFence, FenceGuard, Phase},
    catalog::CatalogSnapshot,
    types::*,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

const TASK_EVENT_RESERVE: u64 = 3; // start, optional closing, and end
const ATTEMPT_EVENT_RESERVE: u64 = 1;
const REQUEST_EVENT_RESERVE: u64 = 6; // submitted, evaluation, decision, terminal (+ margin)
const TASK_RESERVE: u64 = (TASK_BYTES as u64) * 3;
const REQUEST_RESERVE: u64 = (RECORD_BYTES as u64) * 8;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS request_meta (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 version INTEGER NOT NULL CHECK(version=2), configuration BLOB NOT NULL,
 authority_id TEXT NOT NULL, authority_stopped INTEGER NOT NULL CHECK(authority_stopped IN (0,1)),
 last_now_ms INTEGER NOT NULL CHECK(last_now_ms>=0),
 task_count INTEGER NOT NULL CHECK(task_count>=0),
 reserved_events INTEGER NOT NULL CHECK(reserved_events>=0),
 reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>=0)
) STRICT;
CREATE TABLE IF NOT EXISTS request_tasks (id TEXT PRIMARY KEY, data BLOB NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS capability_requests (
 id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES request_tasks(id), data BLOB NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS capability_requests_task ON capability_requests(task_id);
CREATE TABLE IF NOT EXISTS request_events (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT, at_ms INTEGER NOT NULL CHECK(at_ms>=0),
 task_id TEXT NOT NULL REFERENCES request_tasks(id), request_id TEXT,
 kind TEXT NOT NULL, actor TEXT NOT NULL, payload BLOB NOT NULL
) STRICT;
CREATE TRIGGER IF NOT EXISTS request_events_no_update BEFORE UPDATE ON request_events
 BEGIN SELECT RAISE(ABORT,'request events are immutable'); END;
CREATE TRIGGER IF NOT EXISTS request_events_no_delete BEFORE DELETE ON request_events
 BEGIN SELECT RAISE(ABORT,'request events are immutable'); END;
CREATE TRIGGER IF NOT EXISTS request_tasks_no_delete BEFORE DELETE ON request_tasks
 BEGIN SELECT RAISE(ABORT,'task tombstones are immutable'); END;
CREATE TRIGGER IF NOT EXISTS capability_requests_no_delete BEFORE DELETE ON capability_requests
 BEGIN SELECT RAISE(ABORT,'request tombstones are immutable'); END;
";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskData {
    identity: TaskIdentity,
    intent: TaskIntent,
    state: TaskState,
    catalog: CatalogSnapshot,
    attempts: Vec<String>,
    request_count: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRequest {
    record: RequestRecord,
    evaluation_nonce: Option<String>,
}

/// Durable file-backed lifecycle. Reopening requires the same configuration.
/// Every operation observes the current SQLite projection in an IMMEDIATE
/// transaction; no in-memory grant cache can outlive a revocation or task end.
/// The adjacent `.authority` record is part of this store and must be retained
/// with its SQLite files. An interrupted observation or persistence failure
/// permanently stops this authority, including reopen. After reconciliation the
/// host must explicitly replace the store and tasks; no retry resets the stop.
/// If both durable stop writes fail, only the process-local stop is guaranteed.
/// The path and clock are supplied only by trusted host code.
pub struct RequestStore {
    path: PathBuf,
    connection: Mutex<Connection>,
    authority: AuthorityFence,
    authority_id: String,
    config: RequestConfig,
    clock: Arc<dyn Clock>,
}
impl RequestStore {
    pub fn open(path: impl AsRef<Path>, config: RequestConfig) -> Result<Self> {
        Self::open_with_clock(path, config, Arc::new(SystemClock))
    }
    pub fn open_with_clock(
        path: impl AsRef<Path>,
        config: RequestConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        config.validate()?;
        let path = path.as_ref();
        if !path.is_absolute() || path.file_name().is_none() || path == Path::new(":memory:") {
            return Err(RequestError::InvalidInput(
                "RequestStore requires an absolute SQLite file path".into(),
            ));
        }
        let path = path
            .parent()
            .ok_or_else(|| RequestError::InvalidInput("database parent is missing".into()))?
            .canonicalize()?
            .join(path.file_name().expect("database filename checked"));
        let authority = AuthorityFence::open(&path)?;
        let mut fence = authority.lock()?;
        let existed = path.try_exists()?;
        let previous = match fence.read() {
            Ok(record) => record,
            Err(error) => {
                let _ = fence.stop("unavailable", 0);
                return Err(error);
            }
        };
        if previous
            .as_ref()
            .is_some_and(|record| record.phase != Phase::Ready)
            || previous.is_some() != existed
        {
            let id = previous
                .as_ref()
                .map_or("unavailable", |record| record.id.as_str());
            let last = previous.as_ref().map_or(0, |record| record.last_now_ms);
            let _ = fence.stop(id, last);
            return Err(RequestError::AuthorityStopped);
        }
        let authority_id = previous
            .as_ref()
            .map_or_else(|| Uuid::new_v4().to_string(), |record| record.id.clone());
        let last = previous.map_or(0, |record| record.last_now_ms);
        if let Err(error) = fence.write(&authority_id, last, Phase::Pending) {
            authority.stop();
            if existed
                && let Ok(connection) =
                    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            {
                let _ = connection.execute(
                    "UPDATE request_meta SET authority_stopped=1 WHERE singleton=1",
                    [],
                );
            }
            return Err(error);
        }
        let opened = (|| -> Result<Connection> {
            let mut connection = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            connection.busy_timeout(Duration::from_millis(config.sqlite_busy_timeout_ms))?;
            connection.pragma_update(None, "foreign_keys", "ON")?;
            connection.pragma_update(None, "journal_mode", "WAL")?;
            connection.pragma_update(None, "synchronous", "FULL")?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            let configuration = bounded_json(&config, 8192)?;
            let existing: Option<(Vec<u8>, String, i64, i64, i64)> = tx
                .query_row(
                    "SELECT configuration,authority_id,authority_stopped,last_now_ms,version FROM request_meta WHERE singleton=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            if let Some((existing, id, stopped, stored_last, version)) = existing {
                if id != authority_id || stopped != 0 || read_unsigned(&stored_last)? != last {
                    return Err(RequestError::AuthorityStopped);
                }
                if version != 2 || existing != configuration {
                    return Err(RequestError::PolicyMismatch);
                }
            } else if existed {
                return Err(RequestError::AuthorityStopped);
            } else {
                tx.execute(
                    "INSERT INTO request_meta VALUES(1,2,?1,?2,0,0,0,0,0)",
                    params![configuration, authority_id],
                )?;
            }
            tx.commit()?;
            Ok(connection)
        })();
        let connection = match opened {
            Ok(connection) => connection,
            Err(error) => {
                // A configuration mismatch has observed no new time or changed
                // business state. Preserve the ready authority for its owner.
                if matches!(error, RequestError::PolicyMismatch) {
                    if let Err(error) = fence.write(&authority_id, last, Phase::Ready) {
                        let _ = fence.stop(&authority_id, last);
                        return Err(error);
                    }
                } else {
                    let _ = fence.stop(&authority_id, last);
                }
                return Err(error);
            }
        };
        if let Err(error) = fence.write(&authority_id, last, Phase::Ready) {
            let _ = fence.stop(&authority_id, last);
            let _ = connection.execute(
                "UPDATE request_meta SET authority_stopped=1 WHERE singleton=1",
                [],
            );
            return Err(error);
        }
        drop(fence);
        Ok(Self {
            path,
            connection: Mutex::new(connection),
            authority,
            authority_id,
            config,
            clock,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn config(&self) -> &RequestConfig {
        &self.config
    }

    fn transaction<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>, u64) -> Result<T>,
    ) -> Result<T> {
        let mut fence = self.authority.lock()?;
        let record = match fence.read() {
            Ok(Some(record)) if record.id == self.authority_id && record.phase == Phase::Ready => {
                record
            }
            Ok(_) => {
                if let Ok(connection) = self.connection.lock() {
                    self.stop_authority(&mut fence, 0, &connection);
                } else {
                    self.authority.stop();
                }
                return Err(RequestError::AuthorityStopped);
            }
            Err(error) => {
                if let Ok(connection) = self.connection.lock() {
                    self.stop_authority(&mut fence, 0, &connection);
                } else {
                    self.authority.stop();
                }
                return Err(error);
            }
        };
        if let Err(error) = fence.write(&self.authority_id, record.last_now_ms, Phase::Pending) {
            self.authority.stop();
            // If the sidecar failed without invalidating its old Ready bytes,
            // independently stop SQLite before releasing the authority lock.
            if let Ok(connection) = self.connection.lock() {
                self.stop_authority(&mut fence, record.last_now_ms, &connection);
            }
            return Err(error);
        }
        let mut connection = self.connection.lock().map_err(|_| {
            self.authority.stop();
            RequestError::Poisoned
        })?;
        let mut observed = record.last_now_ms;
        let result = (|| {
            // Pending is already durable when the clock is observed. Even BEGIN
            // or clock UPDATE/COMMIT failure cannot erase the stop condition.
            let current = self.clock.now_ms()?;
            if current > i64::MAX as u64 {
                return Err(RequestError::Clock);
            }
            observed = observed.max(current);
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (last, id, stopped): (i64, String, i64) = tx.query_row(
                "SELECT last_now_ms,authority_id,authority_stopped FROM request_meta WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            if id != self.authority_id
                || stopped != 0
                || read_unsigned(&last)? != record.last_now_ms
            {
                return Err(RequestError::AuthorityStopped);
            }
            tx.execute(
                "UPDATE request_meta SET last_now_ms=?1 WHERE singleton=1",
                [sql_integer(observed)?],
            )?;
            // Keep the IMMEDIATE lock through the clock commit. Only projection
            // and events are rolled back on a rejected business operation.
            tx.execute_batch("SAVEPOINT request_operation")?;
            let result = operation(&tx, observed);
            if result.is_err()
                && !matches!(
                    result,
                    Err(RequestError::Stale | RequestError::NotFound | RequestError::Denied(_))
                )
            {
                tx.execute_batch("ROLLBACK TO request_operation")?;
            }
            tx.execute_batch("RELEASE request_operation")?;
            tx.commit()?;
            result
        })();
        if result.as_ref().is_err_and(persistence_failure) {
            // Independent durable records: Pending already exists; preserve a
            // stopped sidecar and try SQLite's independent flag. Never
            // overwrite either failure with a successful automatic retry.
            self.stop_authority(&mut fence, observed, &connection);
            return result;
        }
        if let Err(error) = fence.write(&self.authority_id, observed, Phase::Ready) {
            self.stop_authority(&mut fence, observed, &connection);
            return Err(error);
        }
        result
    }

    fn stop_authority(&self, fence: &mut FenceGuard<'_>, observed: u64, connection: &Connection) {
        let _ = fence.stop(&self.authority_id, observed);
        // A failed rollback may leave an open transaction. The fallback stop
        // must not be accidentally rolled back with that failed operation.
        if !connection.is_autocommit() {
            let _ = connection.execute_batch("ROLLBACK");
        }
        if connection.is_autocommit() {
            let _ = connection.execute(
                "UPDATE request_meta SET authority_stopped=1 WHERE singleton=1",
                [],
            );
        }
    }

    pub fn start_task(
        &self,
        identity: &TaskIdentity,
        intent: TaskIntent,
        catalog: &OperationCatalog,
        actor: &HostActor,
    ) -> Result<()> {
        identity.validate()?;
        intent.validate()?;
        self.transaction(|tx, now| {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM request_tasks WHERE id=?1)",
                [&identity.task_id],
                |r| r.get(0),
            )?;
            if exists {
                return Err(RequestError::Stale);
            }
            let count: u32 = tx.query_row(
                "SELECT task_count FROM request_meta WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            if count >= self.config.max_tasks {
                return Err(RequestError::Budget);
            }
            reserve(tx, &self.config, TASK_EVENT_RESERVE, TASK_RESERVE)?;
            let task = TaskData {
                identity: identity.clone(),
                intent,
                state: TaskState::Active,
                catalog: catalog.snapshot.clone(),
                attempts: vec![identity.attempt_id.clone()],
                request_count: 0,
            };
            let data = bounded_json(&task, TASK_BYTES)?;
            tx.execute(
                "INSERT INTO request_tasks VALUES(?1,?2)",
                params![identity.task_id, data],
            )?;
            tx.execute(
                "UPDATE request_meta SET task_count=?1 WHERE singleton=1",
                [count.checked_add(1).ok_or(RequestError::Budget)?],
            )?;
            event(
                tx,
                now,
                identity,
                None,
                "task_started",
                actor.as_str(),
                &task,
            )?;
            Ok(())
        })
    }
    /// Replace an execution attempt, invalidating every old pending decision and
    /// grant. A terminal task cannot restart, and attempt IDs cannot be reused.
    pub fn replace_attempt(
        &self,
        current: &TaskIdentity,
        next_attempt_id: &str,
        catalog: &OperationCatalog,
        actor: &HostActor,
        reason: &str,
    ) -> Result<TaskIdentity> {
        current.validate()?;
        identifier(next_attempt_id)?;
        text(reason)?;
        self.transaction(|tx, now| {
            let mut task = active_task(tx, current)?;
            if task.attempts.iter().any(|a| a == next_attempt_id) { return Err(RequestError::Stale); }
            if task.attempts.len() >= self.config.max_attempts_per_task as usize { return Err(RequestError::Budget); }
            reserve(tx, &self.config, ATTEMPT_EVENT_RESERVE, TASK_BYTES as u64 + 16 * 1024)?;
            terminalize_all(tx, &task, now, RequestState::Superseded, DecisionKind::Superseded, actor, reason)?;
            task.identity.attempt_id = next_attempt_id.into();
            task.attempts.push(next_attempt_id.into());
            task.catalog = catalog.snapshot.clone();
            save_task(tx, &task)?;
            event(tx, now, &task.identity, None, "attempt_replaced", actor.as_str(), &json!({"previous":current,"current":task.identity,"reason":reason,"catalog_fingerprint":task.catalog.fingerprint(),"catalog":task.catalog}))?;
            Ok(task.identity)
        })
    }
    /// Atomically stop every new admission and withdraw all live requests.
    /// Repeating this for the same active attempt returns the original closing
    /// record without another event. Audit settlement of started calls remains
    /// the Host's responsibility and must not require an active receipt.
    pub fn begin_closing(
        &self,
        identity: &TaskIdentity,
        actor: &HostActor,
        reason: &str,
    ) -> Result<TaskState> {
        identity.validate()?;
        text(reason)?;
        self.transaction(|tx, now| {
            let mut task = identity_task(tx, identity)?;
            match task.state {
                TaskState::Closing { .. } => return Ok(task.state),
                TaskState::Ended { .. } => return Err(RequestError::Stale),
                TaskState::Active => {}
            }
            terminalize_all(
                tx,
                &task,
                now,
                RequestState::Revoked,
                DecisionKind::Revoked,
                actor,
                reason,
            )?;
            task.state = TaskState::Closing {
                actor: actor.as_str().into(),
                reason: reason.into(),
                at_ms: now,
            };
            save_task(tx, &task)?;
            event(
                tx,
                now,
                identity,
                None,
                "task_closing",
                actor.as_str(),
                &task.state,
            )?;
            Ok(task.state)
        })
    }
    /// Persist the terminal outcome after Runtime has drained running work.
    /// Both Active and Closing can finish; ended tasks and old attempts cannot.
    pub fn end_task(
        &self,
        identity: &TaskIdentity,
        outcome: TaskOutcome,
        actor: &HostActor,
        reason: &str,
    ) -> Result<TaskState> {
        identity.validate()?;
        text(reason)?;
        self.transaction(|tx, now| {
            let mut task = identity_task(tx, identity)?;
            if matches!(task.state, TaskState::Ended { .. }) {
                return Err(RequestError::Stale);
            }
            terminalize_all(
                tx,
                &task,
                now,
                RequestState::TaskEnded,
                DecisionKind::TaskEnded,
                actor,
                reason,
            )?;
            task.state = TaskState::Ended {
                outcome,
                actor: actor.as_str().into(),
                reason: reason.into(),
                at_ms: now,
            };
            save_task(tx, &task)?;
            event(
                tx,
                now,
                identity,
                None,
                "task_ended",
                actor.as_str(),
                &task.state,
            )?;
            Ok(task.state)
        })
    }
    /// Check the authoritative active task and the exact captured host policy.
    pub fn check_task(&self, identity: &TaskIdentity, catalog: &OperationCatalog) -> Result<()> {
        identity.validate()?;
        self.transaction(|tx, now| {
            let task = active_task(tx, identity)?;
            check_catalog(&task, catalog)?;
            expire_task(tx, &task, now)?;
            Ok(())
        })
    }
    pub fn task_state(&self, identity: &TaskIdentity) -> Result<TaskState> {
        identity.validate()?;
        self.transaction(|tx, _| Ok(identity_task(tx, identity)?.state))
    }

    /// Record structural validity and purpose/gap claims, never trust claims as a
    /// host evaluation. Only exact same-operation valid requests count together.
    pub fn request(
        &self,
        identity: &TaskIdentity,
        catalog: &OperationCatalog,
        mut input: RequestInput,
    ) -> Result<RequestRecord> {
        identity.validate()?;
        identifier(&input.operation_id)?;
        identifier(&input.capability)?;
        if input.purpose.len() > TEXT_BYTES || input.gap.len() > TEXT_BYTES {
            return Err(RequestError::InvalidInput(
                "purpose or gap exceeds byte limit".into(),
            ));
        }
        argument_bounds(&input.arguments)?;
        input.arguments = canonical(&input.arguments);
        self.transaction(|tx, now| {
            let mut task = active_task(tx, identity)?;
            check_catalog(&task, catalog)?;
            expire_task(tx, &task, now)?;
            if task.request_count >= self.config.max_requests_per_task {
                return Err(RequestError::Budget);
            }
            let mut reasons = Vec::new();
            if text(&input.purpose).is_err() {
                reasons.push(ValidityReason::PurposeMissing);
            }
            if text(&input.gap).is_err() {
                reasons.push(ValidityReason::ConcreteGapMissing);
            }
            let operation = task
                .catalog
                .operations
                .iter()
                .find(|op| op.scope.id == input.operation_id);
            match operation {
                None => reasons.push(ValidityReason::UnsupportedOperation),
                Some(operation)
                    if operation.scope.capability != input.capability
                        || operation.scope.arguments != input.arguments =>
                {
                    reasons.push(ValidityReason::ScopeMismatch)
                }
                Some(_) => {}
            }
            let valid = reasons.is_empty();
            let existing = task_requests(tx, &identity.task_id)?;
            let matching: Vec<_> = existing
                .iter()
                .filter(|r| {
                    r.record.identity == *identity
                        && r.record.input.operation_id == input.operation_id
                        && r.record.input.capability == input.capability
                        && r.record.input.arguments == input.arguments
                })
                .collect();
            let mut valid_count = 0_u32;
            for request in &matching {
                if request.record.validity_reasons == [ValidityReason::ExactHostOperation] {
                    valid_count = valid_count.checked_add(1).ok_or(RequestError::Budget)?;
                }
            }
            if valid {
                valid_count = valid_count.checked_add(1).ok_or(RequestError::Budget)?;
                reasons.push(ValidityReason::ExactHostOperation);
            }
            let explicit_profile =
                matches!(task.intent, TaskIntent::ExplicitProjectTestOrScript { .. })
                    && matches!(input.capability.as_str(), "build.run" | "test.run")
                    && input
                        .arguments
                        .get("profile")
                        .and_then(Value::as_str)
                        .is_some();
            let state = if !valid {
                RequestState::Invalid
            } else if explicit_profile || valid_count >= self.config.repeated_valid_threshold {
                RequestState::AwaitingEvaluation
            } else {
                RequestState::Requested
            };
            let record = RequestRecord {
                id: Uuid::new_v4().to_string(),
                identity: identity.clone(),
                input,
                state,
                validity_reasons: reasons,
                valid_request_count: valid_count,
                catalog_fingerprint: task.catalog.fingerprint(),
                created_at_ms: now,
                request_expires_at_ms: checked_deadline(now, self.config.request_lifetime_ms)?,
                evaluator: None,
                evaluation_deadline_ms: None,
                grant_expires_at_ms: None,
                decision: None,
            };
            let stored = StoredRequest {
                record: record.clone(),
                evaluation_nonce: None,
            };
            let data = bounded_json(&stored, RECORD_BYTES)?;
            reserve(tx, &self.config, REQUEST_EVENT_RESERVE, REQUEST_RESERVE)?;
            // A new valid request replaces older requests for exactly this scope;
            // any outstanding evaluator ticket immediately becomes stale.
            if valid {
                let actor = HostActor::new("request-store")?;
                for request in matching {
                    if request.record.state.live() {
                        let mut old = request.clone();
                        terminalize(
                            tx,
                            &mut old,
                            now,
                            RequestState::Superseded,
                            DecisionKind::Superseded,
                            &actor,
                            "a newer valid request replaced this exact operation",
                        )?;
                    }
                }
            }
            tx.execute(
                "INSERT INTO capability_requests VALUES(?1,?2,?3)",
                params![record.id, identity.task_id, data],
            )?;
            task.request_count = task
                .request_count
                .checked_add(1)
                .ok_or(RequestError::Budget)?;
            save_task(tx, &task)?;
            event(
                tx,
                now,
                identity,
                Some(&record.id),
                "request_recorded",
                "model-request",
                &record,
            )?;
            Ok(record)
        })
    }
    pub fn status(&self, identity: &TaskIdentity, request_id: &str) -> Result<RequestRecord> {
        identity.validate()?;
        identifier(request_id)?;
        self.transaction(|tx, now| {
            let task = identity_task(tx, identity)?;
            expire_task(tx, &task, now)?;
            Ok(owned_request(tx, identity, request_id)?.record)
        })
    }
    /// Start a bounded evaluation, claimed by a trusted host actor. Does not grant.
    pub fn evaluate(
        &self,
        identity: &TaskIdentity,
        request_id: &str,
        actor: &HostActor,
    ) -> Result<EvaluationTicket> {
        identity.validate()?;
        identifier(request_id)?;
        self.transaction(|tx, now| {
            let task = active_task(tx, identity)?;
            expire_task(tx, &task, now)?;
            let mut request = owned_request(tx, identity, request_id)?;
            if request.record.state != RequestState::AwaitingEvaluation { return Err(RequestError::Stale); }
            let nonce = Uuid::new_v4().to_string();
            request.evaluation_nonce = Some(nonce.clone());
            request.record.evaluator = Some(actor.as_str().into());
            request.record.evaluation_deadline_ms = Some(checked_deadline(now, self.config.evaluation_timeout_ms)?.min(request.record.request_expires_at_ms));
            request.record.state = RequestState::Evaluating;
            save_request(tx, &request)?;
            event(tx, now, identity, Some(request_id), "evaluation_started", actor.as_str(), &json!({"request_id":request_id,"deadline_ms":request.record.evaluation_deadline_ms}))?;
            Ok(EvaluationTicket { identity: identity.clone(), request_id: request_id.into(), nonce, actor: actor.as_str().into() })
        })
    }
    pub fn approve(
        &self,
        ticket: &EvaluationTicket,
        catalog: &OperationCatalog,
        lifetime_ms: u64,
        reason: &str,
    ) -> Result<RequestRecord> {
        text(reason)?;
        if lifetime_ms == 0 || lifetime_ms > self.config.max_grant_lifetime_ms {
            return Err(RequestError::InvalidInput(
                "grant lifetime exceeds host maximum".into(),
            ));
        }
        self.finish_evaluation(
            ticket,
            Some(catalog),
            RequestState::Granted,
            DecisionKind::Approved,
            Some(lifetime_ms),
            reason,
        )
    }
    pub fn deny(&self, ticket: &EvaluationTicket, reason: &str) -> Result<RequestRecord> {
        text(reason)?;
        self.finish_evaluation(
            ticket,
            None,
            RequestState::Denied,
            DecisionKind::Denied,
            None,
            reason,
        )
    }
    pub fn evaluator_unavailable(
        &self,
        ticket: &EvaluationTicket,
        reason: &str,
    ) -> Result<RequestRecord> {
        text(reason)?;
        self.finish_evaluation(
            ticket,
            None,
            RequestState::EvaluatorUnavailable,
            DecisionKind::EvaluatorUnavailable,
            None,
            reason,
        )
    }
    fn finish_evaluation(
        &self,
        ticket: &EvaluationTicket,
        catalog: Option<&OperationCatalog>,
        state: RequestState,
        kind: DecisionKind,
        lifetime: Option<u64>,
        reason: &str,
    ) -> Result<RequestRecord> {
        self.transaction(|tx, now| {
            let task = active_task(tx, &ticket.identity)?;
            if let Some(catalog) = catalog { check_catalog(&task, catalog)?; }
            expire_task(tx, &task, now)?;
            let mut request = owned_request(tx, &ticket.identity, &ticket.request_id)?;
            if request.record.state != RequestState::Evaluating || request.evaluation_nonce.as_deref() != Some(&ticket.nonce) || request.record.evaluator.as_deref() != Some(&ticket.actor) { return Err(RequestError::Stale); }
            if task.catalog.exact(&request.record.input).is_none() { return Err(RequestError::PolicyMismatch); }
            request.evaluation_nonce = None;
            request.record.state = state;
            request.record.grant_expires_at_ms = lifetime.map(|ms| checked_deadline(now, ms)).transpose()?;
            request.record.decision = Some(DecisionRecord { kind, actor: ticket.actor.clone(), reason: reason.into(), at_ms: now });
            save_request(tx, &request)?;
            event(tx, now, &ticket.identity, Some(&ticket.request_id), "evaluation_decided", &ticket.actor, &json!({"decision":request.record.decision,"grant_expires_at_ms":request.record.grant_expires_at_ms}))?;
            Ok(request.record)
        })
    }
    pub fn revoke(
        &self,
        identity: &TaskIdentity,
        request_id: &str,
        actor: &HostActor,
        reason: &str,
    ) -> Result<RequestRecord> {
        identity.validate()?;
        identifier(request_id)?;
        text(reason)?;
        self.transaction(|tx, now| {
            let task = active_task(tx, identity)?;
            expire_task(tx, &task, now)?;
            let mut request = owned_request(tx, identity, request_id)?;
            if !request.record.state.live() {
                return Err(RequestError::Stale);
            }
            terminalize(
                tx,
                &mut request,
                now,
                RequestState::Revoked,
                DecisionKind::Revoked,
                actor,
                reason,
            )?;
            Ok(request.record)
        })
    }
    /// Proactively settle observed deadlines. authorize/status/evaluate do the
    /// same check in their own transaction, so a scheduler is never required.
    pub fn expire(&self, identity: &TaskIdentity) -> Result<usize> {
        identity.validate()?;
        self.transaction(|tx, now| expire_task(tx, &identity_task(tx, identity)?, now))
    }
    /// Select a real, exact granted request for this call. Unprotected capability
    /// names have no receipt and must use the ordinary upstream permission path.
    /// Recheck the returned receipt after each await before starting work.
    pub fn admit(
        &self,
        identity: &TaskIdentity,
        catalog: &OperationCatalog,
        capability: &str,
        arguments: &Value,
    ) -> Result<AuthorizationReceipt> {
        identity.validate()?;
        identifier(capability)?;
        argument_bounds(arguments)?;
        if !catalog.requires_request(capability) {
            return Err(RequestError::Denied(
                "capability has no request-protected scope".into(),
            ));
        }
        let arguments = canonical(arguments);
        self.transaction(|tx, now| {
            let task = active_task(tx, identity)?;
            check_catalog(&task, catalog)?;
            expire_task(tx, &task, now)?;
            let fingerprint = catalog.fingerprint();
            for request in task_requests(tx, &identity.task_id)? {
                let record = &request.record;
                if record.identity != *identity
                    || record.state != RequestState::Granted
                    || record.input.capability != capability
                    || record.input.arguments != arguments
                    || record.catalog_fingerprint != fingerprint
                {
                    continue;
                }
                let Some(operation) = task.catalog.exact(&record.input) else {
                    continue;
                };
                let Some(deadline) = record
                    .grant_expires_at_ms
                    .filter(|deadline| now < *deadline)
                else {
                    continue;
                };
                let Some(decision) = record
                    .decision
                    .as_ref()
                    .filter(|decision| decision.kind == DecisionKind::Approved)
                else {
                    continue;
                };
                return Ok(AuthorizationReceipt {
                    identity: identity.clone(),
                    request_id: record.id.clone(),
                    catalog_fingerprint: fingerprint,
                    scope: operation.scope.clone(),
                    grant_expires_at_ms: deadline,
                    decision: decision.clone(),
                });
            }
            Err(RequestError::Denied(
                "no active host grant for these exact arguments".into(),
            ))
        })
    }
    /// Re-read the SAME request ID and decision. A newer grant can never replace
    /// this receipt; another connection's revoke, expiry, closing, end, catalog
    /// change, or attempt replacement defeats it.
    pub fn check_receipt(
        &self,
        receipt: &AuthorizationReceipt,
        catalog: &OperationCatalog,
    ) -> Result<()> {
        self.transaction(|tx, now| {
            let task = active_task(tx, &receipt.identity)?;
            check_catalog(&task, catalog)?;
            if receipt.catalog_fingerprint != catalog.fingerprint() {
                return Err(RequestError::PolicyMismatch);
            }
            expire_task(tx, &task, now)?;
            let request = owned_request(tx, &receipt.identity, &receipt.request_id)?;
            let record = &request.record;
            if record.state != RequestState::Granted
                || record.catalog_fingerprint != receipt.catalog_fingerprint
                || record.grant_expires_at_ms != Some(receipt.grant_expires_at_ms)
                || now >= receipt.grant_expires_at_ms
                || record.decision.as_ref() != Some(&receipt.decision)
                || receipt.decision.kind != DecisionKind::Approved
                || task
                    .catalog
                    .exact(&record.input)
                    .is_none_or(|operation| operation.scope != receipt.scope)
            {
                return Err(RequestError::Denied(
                    "admitted request is no longer an active exact grant".into(),
                ));
            }
            Ok(())
        })
    }
    /// Host-only bounded event page. Model status never enumerates other tasks.
    pub fn events(
        &self,
        identity: &TaskIdentity,
        after: u64,
        limit: usize,
    ) -> Result<Vec<RequestEvent>> {
        identity.validate()?;
        if limit == 0 || limit > 64 || after > i64::MAX as u64 {
            return Err(RequestError::InvalidInput(
                "event page limit must be 1..64".into(),
            ));
        }
        self.transaction(|tx, _| {
            identity_task(tx, identity)?;
            let mut statement = tx.prepare("SELECT sequence,at_ms,task_id,request_id,kind,actor,payload FROM request_events WHERE task_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
            let mut rows = statement.query(params![identity.task_id, sql_integer(after)?, i64::try_from(limit).map_err(|_| RequestError::Budget)?])?;
            let mut events = Vec::new();
            let mut bytes = 2_usize; // JSON array brackets; commas are charged below.
            while let Some(row) = rows.next()? {
                let payload: Vec<u8> = row.get(6)?;
                let event = RequestEvent { sequence: read_unsigned(&row.get::<_, i64>(0)?)?, at_ms: read_unsigned(&row.get::<_, i64>(1)?)?, task_id: row.get(2)?, request_id: row.get(3)?, kind: row.get(4)?, actor: row.get(5)?, payload: serde_json::from_slice(&payload)? };
                let size = bounded_json(&event, TASK_BYTES + RECORD_BYTES)?.len().checked_add(usize::from(!events.is_empty())).ok_or(RequestError::Budget)?;
                if size > self.config.max_result_bytes.saturating_sub(bytes) {
                    if events.is_empty() { return Err(RequestError::Budget); }
                    break;
                }
                bytes = bytes.checked_add(size).ok_or(RequestError::Budget)?;
                events.push(event);
            }
            Ok(events)
        })
    }
}

fn persistence_failure(error: &RequestError) -> bool {
    matches!(
        error,
        RequestError::Database(_)
            | RequestError::Io(_)
            | RequestError::Clock
            | RequestError::Json(_)
            | RequestError::Poisoned
            | RequestError::AuthorityStopped
    )
}
fn sql_integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| RequestError::Clock)
}
fn read_unsigned(value: &i64) -> Result<u64> {
    u64::try_from(*value).map_err(|_| RequestError::Clock)
}
fn reserve(tx: &Transaction<'_>, config: &RequestConfig, events: u64, bytes: u64) -> Result<()> {
    let (old_events, old_bytes): (i64, i64) = tx.query_row(
        "SELECT reserved_events,reserved_bytes FROM request_meta WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let old_events = read_unsigned(&old_events)?;
    let old_bytes = read_unsigned(&old_bytes)?;
    let next_events = old_events
        .checked_add(events)
        .filter(|n| *n <= config.max_events)
        .ok_or(RequestError::Budget)?;
    let next_bytes = old_bytes
        .checked_add(bytes)
        .filter(|n| *n <= config.max_state_bytes)
        .ok_or(RequestError::Budget)?;
    tx.execute(
        "UPDATE request_meta SET reserved_events=?1,reserved_bytes=?2 WHERE singleton=1",
        params![sql_integer(next_events)?, sql_integer(next_bytes)?],
    )?;
    Ok(())
}
fn event(
    tx: &Transaction<'_>,
    now: u64,
    identity: &TaskIdentity,
    request_id: Option<&str>,
    kind: &str,
    actor: &str,
    payload: &impl Serialize,
) -> Result<()> {
    let payload = bounded_json(payload, TASK_BYTES + RECORD_BYTES)?;
    tx.execute("INSERT INTO request_events(at_ms,task_id,request_id,kind,actor,payload) VALUES(?1,?2,?3,?4,?5,?6)", params![sql_integer(now)?,identity.task_id,request_id,kind,actor,payload])?;
    Ok(())
}
fn identity_task(tx: &Transaction<'_>, identity: &TaskIdentity) -> Result<TaskData> {
    let data: Vec<u8> = tx
        .query_row(
            "SELECT data FROM request_tasks WHERE id=?1",
            [&identity.task_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(RequestError::NotFound)?;
    let task: TaskData = serde_json::from_slice(&data)?;
    if task.identity != *identity {
        return Err(RequestError::Stale);
    }
    Ok(task)
}
fn active_task(tx: &Transaction<'_>, identity: &TaskIdentity) -> Result<TaskData> {
    let task = identity_task(tx, identity)?;
    if task.state != TaskState::Active {
        return Err(RequestError::Stale);
    }
    Ok(task)
}
fn check_catalog(task: &TaskData, catalog: &OperationCatalog) -> Result<()> {
    if !catalog.matches(&task.catalog) {
        return Err(RequestError::PolicyMismatch);
    }
    Ok(())
}
fn save_task(tx: &Transaction<'_>, task: &TaskData) -> Result<()> {
    let bytes = bounded_json(task, TASK_BYTES)?;
    tx.execute(
        "UPDATE request_tasks SET data=?1 WHERE id=?2",
        params![bytes, task.identity.task_id],
    )?;
    Ok(())
}
fn owned_request(
    tx: &Transaction<'_>,
    identity: &TaskIdentity,
    request_id: &str,
) -> Result<StoredRequest> {
    let bytes: Vec<u8> = tx
        .query_row(
            "SELECT data FROM capability_requests WHERE id=?1 AND task_id=?2",
            params![request_id, identity.task_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(RequestError::NotFound)?;
    let request: StoredRequest = serde_json::from_slice(&bytes)?;
    if request.record.identity != *identity {
        return Err(RequestError::Stale);
    }
    Ok(request)
}
fn task_requests(tx: &Transaction<'_>, task_id: &str) -> Result<Vec<StoredRequest>> {
    let mut statement =
        tx.prepare("SELECT data FROM capability_requests WHERE task_id=?1 ORDER BY rowid")?;
    let rows = statement.query_map([task_id], |r| r.get::<_, Vec<u8>>(0))?;
    rows.map(|row| Ok(serde_json::from_slice(&row?)?)).collect()
}
fn save_request(tx: &Transaction<'_>, request: &StoredRequest) -> Result<()> {
    let bytes = bounded_json(request, RECORD_BYTES)?;
    tx.execute(
        "UPDATE capability_requests SET data=?1 WHERE id=?2",
        params![bytes, request.record.id],
    )?;
    Ok(())
}
fn terminalize(
    tx: &Transaction<'_>,
    request: &mut StoredRequest,
    now: u64,
    state: RequestState,
    kind: DecisionKind,
    actor: &HostActor,
    reason: &str,
) -> Result<()> {
    request.record.state = state;
    request.evaluation_nonce = None;
    request.record.decision = Some(DecisionRecord {
        kind,
        actor: actor.as_str().into(),
        reason: reason.into(),
        at_ms: now,
    });
    save_request(tx, request)?;
    event(
        tx,
        now,
        &request.record.identity,
        Some(&request.record.id),
        "request_terminal",
        actor.as_str(),
        &json!({"state":state,"decision":request.record.decision}),
    )
}
fn terminalize_all(
    tx: &Transaction<'_>,
    task: &TaskData,
    now: u64,
    state: RequestState,
    kind: DecisionKind,
    actor: &HostActor,
    reason: &str,
) -> Result<()> {
    for mut request in task_requests(tx, &task.identity.task_id)? {
        if request.record.state.live() {
            terminalize(tx, &mut request, now, state, kind, actor, reason)?;
        }
    }
    Ok(())
}
fn expire_task(tx: &Transaction<'_>, task: &TaskData, now: u64) -> Result<usize> {
    let actor = HostActor::new("request-clock")?;
    let mut expired = 0_usize;
    for mut request in task_requests(tx, &task.identity.task_id)? {
        let record = &request.record;
        let deadline = match record.state {
            RequestState::Granted => record.grant_expires_at_ms,
            RequestState::Evaluating => record.evaluation_deadline_ms,
            RequestState::Requested | RequestState::AwaitingEvaluation => {
                Some(record.request_expires_at_ms)
            }
            _ => None,
        };
        if deadline.is_some_and(|deadline| now >= deadline) {
            terminalize(
                tx,
                &mut request,
                now,
                RequestState::Expired,
                DecisionKind::Expired,
                &actor,
                "request, evaluation, or grant deadline reached",
            )?;
            expired = expired.checked_add(1).ok_or(RequestError::Budget)?;
        }
    }
    Ok(expired)
}
