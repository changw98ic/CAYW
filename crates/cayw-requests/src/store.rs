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
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use uuid::Uuid;

const TASK_EVENT_RESERVE: u64 = 3; // start, optional closing, and end
const ATTEMPT_EVENT_RESERVE: u64 = 1;
const REQUEST_EVENT_RESERVE: u64 = 6; // submitted, evaluation, decision, terminal (+ margin)
const TASK_RESERVE: u64 = (TASK_BYTES as u64) * 3;
const REQUEST_RESERVE: u64 = (RECORD_BYTES as u64) * 8;
// Defensive bound for explicit authority inspection, not an admission quota.
const INSPECTION_TASKS: usize = 1_000_000;

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
CREATE TABLE IF NOT EXISTS request_config_history (
 id TEXT PRIMARY KEY, at_ms INTEGER NOT NULL CHECK(at_ms>=0),
 actor TEXT NOT NULL, reason TEXT NOT NULL, previous BLOB NOT NULL, next BLOB NOT NULL
) STRICT;
CREATE TRIGGER IF NOT EXISTS request_config_history_no_update BEFORE UPDATE ON request_config_history
 BEGIN SELECT RAISE(ABORT,'request configuration history is immutable'); END;
CREATE TRIGGER IF NOT EXISTS request_config_history_no_delete BEFORE DELETE ON request_config_history
 BEGIN SELECT RAISE(ABORT,'request configuration history is immutable'); END;
";

// Repair preparation and activation have independent durable records. A new
// ordinary operation may fail after activation; an old repair must not clear it.
const REPAIR_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS request_authority_repairs (
 repair_id TEXT PRIMARY KEY, previous_authority_id TEXT NOT NULL UNIQUE,
 authority_id TEXT NOT NULL UNIQUE, data BLOB NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS request_authority_activations (
 repair_id TEXT PRIMARY KEY REFERENCES request_authority_repairs(repair_id)
) STRICT;
CREATE TRIGGER IF NOT EXISTS request_repairs_no_update BEFORE UPDATE ON request_authority_repairs
 BEGIN SELECT RAISE(ABORT,'authority repairs are immutable'); END;
CREATE TRIGGER IF NOT EXISTS request_repairs_no_delete BEFORE DELETE ON request_authority_repairs
 BEGIN SELECT RAISE(ABORT,'authority repairs are immutable'); END;
CREATE TRIGGER IF NOT EXISTS request_activations_no_update BEFORE UPDATE ON request_authority_activations
 BEGIN SELECT RAISE(ABORT,'authority activations are immutable'); END;
CREATE TRIGGER IF NOT EXISTS request_activations_no_delete BEFORE DELETE ON request_authority_activations
 BEGIN SELECT RAISE(ABORT,'authority activations are immutable'); END;
";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairData {
    inspection: AuthorityInspection,
    actor: String,
    evidence: String,
    receipt: AuthorityRepairReceipt,
    prepared_digest: String,
    at_ms: u64,
}

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
/// stops this authority, including reopen. After inspecting the retained facts
/// and stopping every owner, the trusted host may explicitly repair it. Repair
/// rotates the authority, closes old tasks and revokes grants; retries of normal
/// operations never reset the stop.
/// If both durable stop writes fail, only the process-local stop is guaranteed.
/// The path and clock are supplied only by trusted host code.
pub struct RequestStore {
    path: PathBuf,
    connection: Mutex<Connection>,
    authority: AuthorityFence,
    authority_id: String,
    /// Operational limits of this handle. A trusted adjustment replaces it and
    /// is durably persisted before this value changes. Other handles that were
    /// opened with an older configuration fail closed with PolicyMismatch.
    config: RwLock<RequestConfig>,
    clock: Arc<dyn Clock>,
}
impl RequestStore {
    /// Inspect an existing authority without opening its admission path. No
    /// observation of the host clock or update of stored facts is performed.
    pub fn inspect_authority(
        path: impl AsRef<Path>,
        config: RequestConfig,
    ) -> Result<AuthorityInspection> {
        config.validate()?;
        let path = recovery_path(path.as_ref())?;
        let authority = AuthorityFence::inspect(&path)?;
        let mut fence =
            authority.inspect_lock(Duration::from_millis(config.sqlite_busy_timeout_ms))?;
        let mut connection = recovery_connection(&path, &config, false)?;
        let tx = connection.transaction()?;
        let inspection = inspect_locked(&path, &config, &authority, &mut fence, &tx)?;
        tx.commit()?;
        Ok(inspection)
    }

    /// Explicit trusted recovery. The returned guard is retained across both
    /// repair transactions, sidecar synchronization, and generation publication.
    /// It must own all original Task locks, Cells scopes and Device stop proofs.
    /// A completed repair only closes old tasks; it never replays or authorizes.
    pub fn repair_authority<G: AuthorityRecoveryGuard>(
        path: impl AsRef<Path>,
        config: RequestConfig,
        request: &AuthorityRepairRequest,
        verify_stopped: impl FnOnce(&AuthorityInspection) -> Result<G>,
    ) -> Result<AuthorityRepairReceipt> {
        config.validate()?;
        identifier(&request.repair_id)?;
        text(&request.evidence)?;
        let path = recovery_path(path.as_ref())?;
        let authority = AuthorityFence::inspect(&path)?;
        let mut fence =
            authority.inspect_lock(Duration::from_millis(config.sqlite_busy_timeout_ms))?;
        let mut connection = recovery_connection(&path, &config, true)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = inspect_locked(&path, &config, &authority, &mut fence, &tx)?;
        if current.database != request.inspection.database
            || current.sidecar != request.inspection.sidecar
        {
            return Err(RequestError::Stale);
        }
        let existing = read_repair(&tx, &request.repair_id)?;
        let prepared = if let Some(repair) = existing {
            if repair.inspection != request.inspection
                || repair.actor != request.actor.as_str()
                || repair.evidence != request.evidence
                || repair.receipt.authority_id != current.authority_id
            {
                return Err(RequestError::Stale);
            }
            if repair_activated(&tx, &request.repair_id)? {
                // A later ordinary failure is a new incident, even if all of
                // the old repair request's bytes still match its receipt.
                let database_stopped: i64 = tx.query_row(
                    "SELECT authority_stopped FROM request_meta WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                let ready = serde_json::from_slice::<crate::authority::Record>(&current.marker)
                    .ok()
                    .is_some_and(|record| {
                        record.id == current.authority_id
                            && record.phase == Phase::Ready
                            && record.last_now_ms == current.last_now_ms
                    });
                if database_stopped != 0 || !ready || authority.stopped_for(&current.authority_id) {
                    return Err(RequestError::AuthorityStopped);
                }
                tx.commit()?;
                // An older process may still retain only the previous local
                // generation after another process committed activation.
                if authority.is_stopped() {
                    authority.publish_repaired(&current.authority_id)?;
                }
                return Ok(repair.receipt);
            }
            if repair.prepared_digest != current.inspection_id {
                return Err(RequestError::Stale);
            }
            tx.commit()?;
            repair
        } else {
            if request.inspection.authority_id != current.authority_id
                || request.inspection.inspection_id != current.inspection_id
                || request.inspection.tasks != current.tasks
                || !request.inspection.stopped
                || !current.stopped
            {
                return Err(RequestError::Stale);
            }
            // No database or marker write precedes the original owner check.
            let owners = verify_stopped(&request.inspection)?;
            check_recovery_owners(&owners, &request.inspection)?;
            let now = SystemClock
                .now_ms()?
                .max(current.last_now_ms)
                .max(marker_time(&current.marker));
            let next_id = Uuid::new_v4().to_string();
            let receipt = AuthorityRepairReceipt {
                repair_id: request.repair_id.clone(),
                previous_authority_id: current.authority_id.clone(),
                authority_id: next_id.clone(),
                inspection_id: current.inspection_id.clone(),
            };
            // Until activation commits, even a Ready sidecar cannot authorize.
            let prepared_result = (|| -> Result<RepairData> {
                fence.write(&current.authority_id, now, Phase::Pending)?;
                tx.execute_batch(REPAIR_SCHEMA)?;
                let inspection_bytes = bounded_json(
                    &request.inspection,
                    repair_record_limit(current.tasks.len())?,
                )?
                .len() as u64;
                let repair_bytes = inspection_bytes
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(16 * 1024))
                    .ok_or(RequestError::Budget)?;
                reserve(&tx, &config, 0, repair_bytes)?;
                for task in &current.tasks {
                    let mut stored = identity_task(&tx, &task.identity)?;
                    terminalize_all(
                        &tx,
                        &stored,
                        now,
                        RequestState::Revoked,
                        DecisionKind::Revoked,
                        &request.actor,
                        &request.evidence,
                    )?;
                    if stored.state == TaskState::Active {
                        stored.state = TaskState::Closing {
                            actor: request.actor.as_str().into(),
                            reason: request.evidence.clone(),
                            at_ms: now,
                        };
                        save_task(&tx, &stored)?;
                        // Uses the lifecycle event reserved at task creation.
                        event(
                            &tx,
                            now,
                            &stored.identity,
                            None,
                            "task_closing",
                            request.actor.as_str(),
                            &stored.state,
                        )?;
                    }
                }
                tx.execute("UPDATE request_meta SET authority_id=?1,authority_stopped=1,last_now_ms=?2 WHERE singleton=1", params![next_id, sql_integer(now)?])?;
                let repair = RepairData {
                    inspection: request.inspection.clone(),
                    actor: request.actor.as_str().into(),
                    evidence: request.evidence.clone(),
                    receipt,
                    prepared_digest: facts_digest(&tx)?,
                    at_ms: now,
                };
                let bytes = bounded_json(
                    &repair,
                    repair_record_limit(request.inspection.tasks.len())?,
                )?;
                tx.execute(
                    "INSERT INTO request_authority_repairs VALUES(?1,?2,?3,?4)",
                    params![request.repair_id, current.authority_id, next_id, bytes],
                )?;
                tx.commit()?;
                Ok(repair)
            })();
            let repair = match prepared_result {
                Ok(repair) => repair,
                Err(error) => {
                    let _ = fence.stop(&current.authority_id, now);
                    let _ = connection.execute(
                        "UPDATE request_meta SET authority_stopped=1 WHERE singleton=1",
                        [],
                    );
                    return Err(error);
                }
            };
            #[cfg(test)]
            recovery_tests::crash_window("prepared");
            let result = activate_repair(
                &path,
                &config,
                &authority,
                &mut fence,
                &mut connection,
                &repair,
            );
            drop(owners);
            return result;
        };
        // The preparation commit survived but activation did not. The same
        // original inspection and owner proof must be supplied again.
        let owners = verify_stopped(&request.inspection)?;
        check_recovery_owners(&owners, &request.inspection)?;
        let result = activate_repair(
            &path,
            &config,
            &authority,
            &mut fence,
            &mut connection,
            &prepared,
        );
        drop(owners);
        result
    }

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
        let mut fence = authority.lock(Duration::from_millis(config.sqlite_busy_timeout_ms))?;
        let existed = path.try_exists()?;
        let previous = match fence.read() {
            Ok(record) => record,
            Err(error) => {
                // Retain the torn bytes for explicit inspection. Rewriting
                // them as "unavailable" would erase the original evidence and
                // invent a conflicting authority identity.
                authority.stop();
                if existed && let Ok(connection) = recovery_connection(&path, &config, true) {
                    let _ = connection.execute(
                        "UPDATE request_meta SET authority_stopped=1 WHERE singleton=1",
                        [],
                    );
                }
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
            // Task-count and state reservations are operational projections of
            // unfinished tasks, not a lifetime admission count. Recomputing
            // them on open migrates databases created by the earlier
            // lifetime-counter semantics without deleting any task, request,
            // event, or decision tombstone. Reservation growth is deterministic
            // from the retained projection, so repeated opens agree.
            let (active, reserved_bytes) = live_usage(&tx)?;
            tx.execute(
                "UPDATE request_meta SET task_count=?1,reserved_bytes=?2 WHERE singleton=1",
                params![active, sql_integer(reserved_bytes)?],
            )?;
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
        authority.bind_identity(&authority_id)?;
        drop(fence);
        Ok(Self {
            path,
            connection: Mutex::new(connection),
            authority,
            authority_id,
            config: RwLock::new(config),
            clock,
        })
    }
    /// Read the durable operational configuration without opening the admission
    /// path or changing any fact. A trusted host can use this to open with the
    /// persisted limits after a previous process changed them.
    pub fn persisted_config(path: impl AsRef<Path>) -> Result<RequestConfig> {
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
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_millis(
            RequestConfig::default().sqlite_busy_timeout_ms,
        ))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let bytes: Vec<u8> = connection
            .query_row(
                "SELECT configuration FROM request_meta WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(RequestError::NotFound)?;
        let config: RequestConfig = serde_json::from_slice(&bytes)?;
        config.validate()?;
        Ok(config)
    }

    /// Open with the persisted operational configuration, then durably apply
    /// the caller's requested configuration if it differs. This is the
    /// supported restart path after a trusted adjustment: the caller does not
    /// have to know the previously persisted limits. The adjustment keeps an
    /// immutable previous/next history row and fails closed if the caller also
    /// tries to lower limits below current active usage.
    pub fn open_with_trusted_adjustment(
        path: impl AsRef<Path>,
        config: RequestConfig,
        actor: &HostActor,
        reason: &str,
    ) -> Result<Self> {
        config.validate()?;
        // A new database has no persisted configuration to adopt; open creates
        // it with this configuration exactly like `open`.
        if !path.as_ref().try_exists()? {
            return Self::open(path, config);
        }
        let stored = Self::persisted_config(&path)?;
        let store = Self::open(&path, stored)?;
        if store.config() != config {
            store.adjust_config(config, actor, reason)?;
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Current operational configuration. A trusted [`Self::adjust_config`]
    /// replaces this value only after the new limits are durably persisted.
    pub fn config(&self) -> RequestConfig {
        self.config
            .read()
            .map(|config| config.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Trusted operational-limit adjustment. The new configuration is validated
    /// and persisted together with an immutable previous/next history record.
    /// Limits cannot be lowered below current active usage. Every other handle
    /// that was opened with the previous configuration fails closed with
    /// `PolicyMismatch` on its next operation instead of silently admitting
    /// work under stale limits.
    pub fn adjust_config(
        &self,
        next: RequestConfig,
        actor: &HostActor,
        reason: &str,
    ) -> Result<()> {
        next.validate()?;
        text(reason)?;
        let mut guard = self.config.write().map_err(|_| RequestError::Poisoned)?;
        let current = guard.clone();
        // The write guard serializes adjustment against handles of this store;
        // the operation itself still checks the durable configuration so an
        // independent process's adjustment is never overwritten silently.
        self.transaction_with(&current, |tx, now, _| {
            let (active, reserved_events, reserved_bytes): (i64, i64, i64) = tx.query_row(
                "SELECT task_count,reserved_events,reserved_bytes FROM request_meta WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            if u64::from(next.max_tasks) < read_unsigned(&active)?
                || next.max_events < read_unsigned(&reserved_events)?
                || next.max_state_bytes < read_unsigned(&reserved_bytes)?
            {
                return Err(RequestError::InvalidInput(
                    "configuration limits cannot be lowered below current active usage".into(),
                ));
            }
            let previous = bounded_json(&current, 8192)?;
            let next_bytes = bounded_json(&next, 8192)?;
            tx.execute(
                "UPDATE request_meta SET configuration=?1 WHERE singleton=1",
                [&next_bytes],
            )?;
            tx.execute(
                "INSERT INTO request_config_history VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    Uuid::new_v4().to_string(),
                    sql_integer(now)?,
                    actor.as_str(),
                    reason,
                    previous,
                    next_bytes
                ],
            )?;
            Ok(())
        })?;
        *guard = next;
        Ok(())
    }

    /// Host-only immutable history of operational-limit adjustments, oldest
    /// first. The original audit rows are retained exactly as written.
    pub fn config_history(&self) -> Result<Vec<ConfigAdjustment>> {
        self.transaction(|tx, _, _| {
            let mut statement = tx.prepare(
                "SELECT at_ms,actor,reason,previous,next FROM request_config_history ORDER BY at_ms,id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })?;
            let mut history = Vec::new();
            for row in rows {
                let (at_ms, actor, reason, previous, next) = row?;
                history.push(ConfigAdjustment {
                    at_ms: read_unsigned(&at_ms)?,
                    actor,
                    reason,
                    previous: serde_json::from_slice(&previous)?,
                    next: serde_json::from_slice(&next)?,
                });
            }
            Ok(history)
        })
    }

    fn transaction<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>, u64, &RequestConfig) -> Result<T>,
    ) -> Result<T> {
        // Keep the same lock order as adjustment: configuration, authority,
        // then connection. An adjustment cannot replace this configuration
        // between the snapshot and its durable transaction check.
        let config = self.config.read().map_err(|_| RequestError::Poisoned)?;
        self.transaction_with(&config, operation)
    }
    fn transaction_with<T>(
        &self,
        config: &RequestConfig,
        operation: impl FnOnce(&Transaction<'_>, u64, &RequestConfig) -> Result<T>,
    ) -> Result<T> {
        let mut fence = self
            .authority
            .lock(Duration::from_millis(config.sqlite_busy_timeout_ms))?;
        let mut connection = self.connection.lock().map_err(|_| {
            self.authority.stop();
            RequestError::Poisoned
        })?;
        let current_id: String = match connection.query_row(
            "SELECT authority_id FROM request_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        ) {
            Ok(id) => id,
            Err(error) => {
                self.authority.stop();
                // Without a readable SQLite identity, only an intact marker
                // bearing this handle's ID can be stopped. Never overwrite a
                // different or torn generation with this handle's old ID.
                if let Ok(Some(record)) = fence.read()
                    && record.id == self.authority_id
                {
                    let _ = fence.stop(&self.authority_id, record.last_now_ms);
                }
                let _ = connection.execute("UPDATE request_meta SET authority_stopped=1 WHERE singleton=1 AND authority_id=?1", [&self.authority_id]);
                return Err(error.into());
            }
        };
        if current_id != self.authority_id {
            // Check SQLite first: a stale process must not rewrite a torn
            // marker belonging to the repaired generation with its own ID.
            return Err(RequestError::AuthorityStopped);
        }
        let record = match fence.read() {
            Ok(Some(record)) if record.id == self.authority_id && record.phase == Phase::Ready => {
                record
            }
            Ok(Some(record)) if record.id != self.authority_id => {
                // A repaired generation belongs to another authority. A stale
                // handle cannot stop or overwrite that generation's marker/DB.
                return Err(RequestError::AuthorityStopped);
            }
            Ok(_) => {
                self.authority.stop();
                let _ = connection.execute("UPDATE request_meta SET authority_stopped=1 WHERE singleton=1 AND authority_id=?1", [&self.authority_id]);
                return Err(RequestError::AuthorityStopped);
            }
            Err(error) => {
                // Keep the failed marker bytes as original inspection evidence.
                self.authority.stop();
                let _ = connection.execute("UPDATE request_meta SET authority_stopped=1 WHERE singleton=1 AND authority_id=?1", [&self.authority_id]);
                return Err(error);
            }
        };
        if let Err(error) = fence.write(&self.authority_id, record.last_now_ms, Phase::Pending) {
            self.authority.stop();
            // If the sidecar failed without invalidating its old Ready bytes,
            // independently stop SQLite before releasing the authority lock.
            self.stop_authority(&mut fence, record.last_now_ms, &connection);
            return Err(error);
        }
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
            let (last, id, stopped, stored_configuration): (i64, String, i64, Vec<u8>) = tx
                .query_row(
                    "SELECT last_now_ms,authority_id,authority_stopped,configuration FROM request_meta WHERE singleton=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;
            if id != self.authority_id
                || stopped != 0
                || read_unsigned(&last)? != record.last_now_ms
            {
                return Err(RequestError::AuthorityStopped);
            }
            // Operational limits are part of the durable policy. A handle that
            // was opened with a configuration another trusted adjustment has
            // replaced must fail closed instead of admitting with stale limits.
            if stored_configuration != bounded_json(config, 8192)? {
                return Err(RequestError::PolicyMismatch);
            }
            tx.execute(
                "UPDATE request_meta SET last_now_ms=?1 WHERE singleton=1",
                [sql_integer(observed)?],
            )?;
            // Keep the IMMEDIATE lock through the clock commit. Only projection
            // and events are rolled back on a rejected business operation.
            tx.execute_batch("SAVEPOINT request_operation")?;
            let result = operation(&tx, observed, config);
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
        self.transaction(|tx, now, config| {
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
            if count >= config.max_tasks {
                return Err(RequestError::Budget);
            }
            reserve(tx, config, TASK_EVENT_RESERVE, TASK_RESERVE)?;
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
        self.transaction(|tx, now, config| {
            let mut task = active_task(tx, current)?;
            if task.attempts.iter().any(|a| a == next_attempt_id) { return Err(RequestError::Stale); }
            if task.attempts.len() >= config.max_attempts_per_task as usize { return Err(RequestError::Budget); }
            reserve(tx, config, ATTEMPT_EVENT_RESERVE, TASK_BYTES as u64 + 16 * 1024)?;
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
        self.transaction(|tx, now, _| {
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
        self.transaction(|tx, now, _| {
            let mut task = identity_task(tx, identity)?;
            if matches!(task.state, TaskState::Ended { .. }) {
                return Err(RequestError::Stale);
            }
            let release = task_reservation(&task);
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
            // A finished task stops running work. Its task count and
            // conservative active-state reservation return to the operational
            // pool; its immutable projection and events remain tombstones.
            release_task_quota(tx, release)?;
            Ok(task.state)
        })
    }
    /// Check the authoritative active task and the exact captured host policy.
    pub fn check_task(&self, identity: &TaskIdentity, catalog: &OperationCatalog) -> Result<()> {
        identity.validate()?;
        self.transaction(|tx, now, _| {
            let task = active_task(tx, identity)?;
            check_catalog(&task, catalog)?;
            expire_task(tx, &task, now)?;
            Ok(())
        })
    }
    pub fn task_state(&self, identity: &TaskIdentity) -> Result<TaskState> {
        identity.validate()?;
        self.transaction(|tx, _, _| Ok(identity_task(tx, identity)?.state))
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
        self.transaction(|tx, now, config| {
            let mut task = active_task(tx, identity)?;
            check_catalog(&task, catalog)?;
            expire_task(tx, &task, now)?;
            if task.request_count >= config.max_requests_per_task {
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
            // Below-threshold state is only a visibility hint for the host and
            // model. Any structurally valid request is already evaluable by a
            // trusted host; this branch never blocks evaluation or grants.
            let state = if !valid {
                RequestState::Invalid
            } else if explicit_profile || valid_count >= config.repeated_valid_threshold {
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
                request_expires_at_ms: checked_deadline(now, config.request_lifetime_ms)?,
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
            reserve(tx, config, REQUEST_EVENT_RESERVE, REQUEST_RESERVE)?;
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
        self.transaction(|tx, now, _| {
            let task = identity_task(tx, identity)?;
            expire_task(tx, &task, now)?;
            Ok(owned_request(tx, identity, request_id)?.record)
        })
    }
    /// Start a bounded evaluation, claimed by a trusted host actor. Does not
    /// grant. A structurally valid request is evaluable immediately; the repeat
    /// threshold only changes the informational state (`requested` versus
    /// `awaiting_evaluation`) and never withholds a ticket from the host.
    pub fn evaluate(
        &self,
        identity: &TaskIdentity,
        request_id: &str,
        actor: &HostActor,
    ) -> Result<EvaluationTicket> {
        identity.validate()?;
        identifier(request_id)?;
        self.transaction(|tx, now, config| {
            let task = active_task(tx, identity)?;
            expire_task(tx, &task, now)?;
            let mut request = owned_request(tx, identity, request_id)?;
            if !matches!(
                request.record.state,
                RequestState::Requested | RequestState::AwaitingEvaluation
            ) {
                return Err(RequestError::Stale);
            }
            let nonce = Uuid::new_v4().to_string();
            request.evaluation_nonce = Some(nonce.clone());
            request.record.evaluator = Some(actor.as_str().into());
            request.record.evaluation_deadline_ms = Some(checked_deadline(now, config.evaluation_timeout_ms)?.min(request.record.request_expires_at_ms));
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
        self.transaction(|tx, now, config| {
            if lifetime.is_some_and(|ms| ms == 0 || ms > config.max_grant_lifetime_ms) {
                return Err(RequestError::InvalidInput(
                    "grant lifetime exceeds host maximum".into(),
                ));
            }
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
        self.transaction(|tx, now, _| {
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
        self.transaction(|tx, now, _| expire_task(tx, &identity_task(tx, identity)?, now))
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
        self.transaction(|tx, now, _| {
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
        self.transaction(|tx, now, _| {
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
        self.transaction(|tx, _, config| {
            identity_task(tx, identity)?;
            let mut statement = tx.prepare("SELECT sequence,at_ms,task_id,request_id,kind,actor,payload FROM request_events WHERE task_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
            let mut rows = statement.query(params![identity.task_id, sql_integer(after)?, i64::try_from(limit).map_err(|_| RequestError::Budget)?])?;
            let mut events = Vec::new();
            let mut bytes = 2_usize; // JSON array brackets; commas are charged below.
            while let Some(row) = rows.next()? {
                let payload: Vec<u8> = row.get(6)?;
                let event = RequestEvent { sequence: read_unsigned(&row.get::<_, i64>(0)?)?, at_ms: read_unsigned(&row.get::<_, i64>(1)?)?, task_id: row.get(2)?, request_id: row.get(3)?, kind: row.get(4)?, actor: row.get(5)?, payload: serde_json::from_slice(&payload)? };
                let size = bounded_json(&event, TASK_BYTES + RECORD_BYTES)?.len().checked_add(usize::from(!events.is_empty())).ok_or(RequestError::Budget)?;
                if size > config.max_result_bytes.saturating_sub(bytes) {
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

fn recovery_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(RequestError::InvalidInput(
            "authority recovery requires an absolute existing file path".into(),
        ));
    }
    let path = path
        .parent()
        .ok_or_else(|| RequestError::InvalidInput("database parent is missing".into()))?
        .canonicalize()?
        .join(path.file_name().expect("filename checked"));
    if !fs::symlink_metadata(&path)?.file_type().is_file() {
        return Err(RequestError::InvalidInput(
            "authority database must be a regular file".into(),
        ));
    }
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".authority");
    if !fs::symlink_metadata(PathBuf::from(sidecar))?
        .file_type()
        .is_file()
    {
        return Err(RequestError::AuthorityStopped);
    }
    Ok(path)
}
fn recovery_connection(path: &Path, config: &RequestConfig, writable: bool) -> Result<Connection> {
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let connection = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    connection.busy_timeout(Duration::from_millis(config.sqlite_busy_timeout_ms))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    if writable {
        connection.pragma_update(None, "synchronous", "FULL")?;
    }
    Ok(connection)
}
fn file_identity(metadata: &fs::Metadata) -> AuthorityFileIdentity {
    AuthorityFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}
fn inspect_locked(
    path: &Path,
    config: &RequestConfig,
    authority: &AuthorityFence,
    fence: &mut FenceGuard<'_>,
    tx: &Transaction<'_>,
) -> Result<AuthorityInspection> {
    let (version, stored_config, authority_id, stopped, last): (i64, Vec<u8>, String, i64, i64) = tx.query_row("SELECT version,configuration,authority_id,authority_stopped,last_now_ms FROM request_meta WHERE singleton=1", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?;
    if version != 2 || stored_config != bounded_json(config, 8192)? {
        return Err(RequestError::PolicyMismatch);
    }
    identifier(&authority_id)?;
    let last_now_ms = read_unsigned(&last)?;
    let marker = fence.bytes()?;
    let parsed = serde_json::from_slice::<crate::authority::Record>(&marker).ok();
    if let Some(record) = &parsed {
        if record.last_now_ms > i64::MAX as u64 {
            return Err(RequestError::AuthorityStopped);
        }
        if record.id != authority_id {
            // Preparation can commit before marker rotation. Only its exact,
            // immutable repair record may explain the old marker identity.
            let table: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='request_authority_repairs')", [], |r| r.get(0))?;
            let repair_id: Option<String> = if table {
                tx.query_row("SELECT repair_id FROM request_authority_repairs WHERE previous_authority_id=?1 AND authority_id=?2", params![record.id, authority_id], |r| r.get(0)).optional()?
            } else {
                None
            };
            let repair = repair_id
                .as_deref()
                .map(|id| read_repair(tx, id))
                .transpose()?
                .flatten()
                .ok_or(RequestError::AuthorityStopped)?;
            if record.phase != Phase::Pending && record.phase != Phase::Stopped
                || repair_activated(tx, &repair.receipt.repair_id)?
                || repair.prepared_digest != facts_digest(tx)?
                || repair.inspection.database != file_identity(&fs::metadata(path)?)
                || repair.inspection.sidecar != file_identity(&fence.metadata()?)
            {
                return Err(RequestError::AuthorityStopped);
            }
        }
    }
    let stopped = stopped != 0
        || authority.is_stopped()
        || parsed
            .as_ref()
            .is_none_or(|record| record.phase != Phase::Ready || record.last_now_ms != last_now_ms);
    let mut statement = tx.prepare("SELECT id,data FROM request_tasks ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut tasks = Vec::new();
    while let Some(row) = rows.next()? {
        // Completed tasks are permanent tombstones, so the operational
        // `max_tasks` admission limit must not bound inspection. This wider
        // defensive parse bound only stops a corrupt table from exhausting
        // memory during explicit recovery.
        if tasks.len() >= INSPECTION_TASKS {
            return Err(RequestError::Budget);
        }
        let id: String = row.get(0)?;
        let data: Vec<u8> = row.get(1)?;
        if data.len() > TASK_BYTES {
            return Err(RequestError::Budget);
        }
        let task: TaskData = serde_json::from_slice(&data)?;
        task.identity.validate()?;
        if task.identity.task_id != id {
            return Err(RequestError::Stale);
        }
        tasks.push(AuthorityTaskInspection {
            identity: task.identity,
            state: task.state,
        });
    }
    Ok(AuthorityInspection {
        authority_id,
        inspection_id: facts_digest(tx)?,
        tasks,
        database: file_identity(&fs::metadata(path)?),
        sidecar: file_identity(&fence.metadata()?),
        marker,
        stopped,
        last_now_ms,
    })
}
fn facts_digest(tx: &Transaction<'_>) -> Result<String> {
    let mut hash = Sha256::new();
    // Exclude only the stop flag: normal failed reopen can set it without
    // changing any original permission/task fact. Marker bytes are retained in
    // the repair record but Pending->Stopped cannot destroy exact retry.
    for sql in [
        "SELECT singleton,version,configuration,authority_id,last_now_ms,task_count,reserved_events,reserved_bytes FROM request_meta ORDER BY singleton",
        "SELECT id,data FROM request_tasks ORDER BY id",
        "SELECT id,task_id,data FROM capability_requests ORDER BY id",
        "SELECT sequence,at_ms,task_id,request_id,kind,actor,payload FROM request_events ORDER BY sequence",
    ] {
        hash.update(sql.as_bytes());
        let mut statement = tx.prepare(sql)?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            for index in 0..columns {
                use rusqlite::types::ValueRef;
                match row.get_ref(index)? {
                    ValueRef::Null => hash.update([0]),
                    ValueRef::Integer(value) => {
                        hash.update([1]);
                        hash.update(value.to_le_bytes());
                    }
                    ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
                        hash.update([if matches!(row.get_ref(index)?, ValueRef::Text(_)) {
                            2
                        } else {
                            3
                        }]);
                        hash.update((bytes.len() as u64).to_le_bytes());
                        hash.update(bytes);
                    }
                    ValueRef::Real(_) => return Err(RequestError::AuthorityStopped),
                }
            }
            hash.update([255]);
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn marker_time(marker: &[u8]) -> u64 {
    serde_json::from_slice::<crate::authority::Record>(marker)
        .ok()
        .map_or(0, |record| record.last_now_ms)
}
fn repair_record_limit(tasks: usize) -> Result<usize> {
    tasks
        .checked_mul(4096)
        .and_then(|n| n.checked_add(16 * 1024))
        .ok_or(RequestError::Budget)
}
fn read_repair(tx: &Transaction<'_>, repair_id: &str) -> Result<Option<RepairData>> {
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='request_authority_repairs')", [], |r| r.get(0))?;
    if !exists {
        return Ok(None);
    }
    let bytes: Option<Vec<u8>> = tx
        .query_row(
            "SELECT data FROM request_authority_repairs WHERE repair_id=?1",
            [repair_id],
            |r| r.get(0),
        )
        .optional()?;
    bytes
        .map(|data| Ok(serde_json::from_slice(&data)?))
        .transpose()
}
fn repair_activated(tx: &Transaction<'_>, repair_id: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM request_authority_activations WHERE repair_id=?1)",
        [repair_id],
        |r| r.get(0),
    )?)
}
fn check_recovery_owners(
    owners: &impl AuthorityRecoveryGuard,
    inspection: &AuthorityInspection,
) -> Result<()> {
    let tasks = owners.verified_tasks();
    if tasks.len() != inspection.tasks.len()
        || inspection
            .tasks
            .iter()
            .any(|task| tasks.iter().filter(|id| **id == task.identity).count() != 1)
    {
        return Err(RequestError::Stale);
    }
    Ok(())
}
fn activate_repair(
    path: &Path,
    config: &RequestConfig,
    authority: &AuthorityFence,
    fence: &mut FenceGuard<'_>,
    connection: &mut Connection,
    repair: &RepairData,
) -> Result<AuthorityRepairReceipt> {
    let result = (|| -> Result<()> {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if facts_digest(&tx)? != repair.prepared_digest
            || repair_activated(&tx, &repair.receipt.repair_id)?
        {
            return Err(RequestError::Stale);
        }
        if file_identity(&fs::metadata(path)?) != repair.inspection.database
            || file_identity(&fence.metadata()?) != repair.inspection.sidecar
        {
            return Err(RequestError::Stale);
        }
        // Clear the old local generation before publishing a distinct one.
        authority.stop();
        // Ready comes first, but SQLite still has stopped=1 until the activation
        // commit. A process loss in either window therefore cannot admit work.
        fence.write(&repair.receipt.authority_id, repair.at_ms, Phase::Ready)?;
        #[cfg(test)]
        recovery_tests::crash_window("ready");
        tx.execute(
            "INSERT INTO request_authority_activations VALUES(?1)",
            [&repair.receipt.repair_id],
        )?;
        // Preparation held a temporary reservation through the activation
        // commit. Rebuild the unfinished-task projection in this same commit
        // so completed repairs never become a lifetime admission charge. The
        // immutable repair and activation records remain retained.
        let (active, reserved_bytes) = live_usage(&tx)?;
        tx.execute(
            "UPDATE request_meta SET authority_stopped=0,task_count=?2,reserved_bytes=?3 WHERE singleton=1 AND authority_id=?1",
            params![repair.receipt.authority_id, active, sql_integer(reserved_bytes)?],
        )?;
        tx.commit()?;
        #[cfg(test)]
        recovery_tests::crash_window("activated");
        authority.publish_repaired(&repair.receipt.authority_id)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fence.stop(&repair.receipt.authority_id, repair.at_ms);
        let _ = connection.execute(
            "UPDATE request_meta SET authority_stopped=1 WHERE singleton=1",
            [],
        );
        return Err(error);
    }
    // config was validated by the caller and kept identical across both phases.
    let _ = config;
    Ok(repair.receipt.clone())
}

#[cfg(test)]
#[path = "authority_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "configuration_tests.rs"]
mod configuration_tests;

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
/// Conservative state reservation held by one unfinished task. It covers the
/// original projection, every terminal rewrite, each attempt replacement, and
/// all request lifecycles recorded in that task. `end_task` returns exactly
/// this amount, so completed ordinary work does not consume an operational
/// allowance forever. Terminal tombstones and event rows remain retained.
fn task_reservation(task: &TaskData) -> u64 {
    let attempts = task.attempts.len().saturating_sub(1) as u64;
    let requests = task.request_count as u64;
    TASK_RESERVE
        .saturating_add(attempts.saturating_mul(TASK_BYTES as u64 + 16 * 1024))
        .saturating_add(requests.saturating_mul(REQUEST_RESERVE))
}
/// Rebuild the operational counters from the durable task projection. Called
/// on open so databases from the earlier lifetime-counter semantics migrate
/// without deleting any audit fact. Terminal tasks hold no reservation.
fn live_usage(tx: &Transaction<'_>) -> Result<(u32, u64)> {
    let mut statement = tx.prepare("SELECT data FROM request_tasks ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut active = 0_u32;
    let mut reserved = 0_u64;
    while let Some(row) = rows.next()? {
        let data: Vec<u8> = row.get(0)?;
        let task: TaskData = serde_json::from_slice(&data)?;
        if matches!(task.state, TaskState::Active | TaskState::Closing { .. }) {
            active = active.checked_add(1).ok_or(RequestError::Budget)?;
            reserved = reserved
                .checked_add(task_reservation(&task))
                .ok_or(RequestError::Budget)?;
        }
    }
    Ok((active, reserved))
}
/// Return one finished task's count and state reservation to the operational
/// pool. Saturating arithmetic cannot silently wrap on a migrated projection
/// whose older reservation was already released or never charged.
fn release_task_quota(tx: &Transaction<'_>, release: u64) -> Result<()> {
    let (count, reserved): (i64, i64) = tx.query_row(
        "SELECT task_count,reserved_bytes FROM request_meta WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let count = read_unsigned(&count)?.saturating_sub(1);
    let reserved = read_unsigned(&reserved)?.saturating_sub(release);
    tx.execute(
        "UPDATE request_meta SET task_count=?1,reserved_bytes=?2 WHERE singleton=1",
        params![sql_integer(count)?, sql_integer(reserved)?],
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
