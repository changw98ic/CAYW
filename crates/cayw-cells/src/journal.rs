use crate::*;
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

/// File-backed SQLite only, WAL + synchronous FULL. Opening never recovers or
/// claims an executor. Managers lock only their canonical database/session/task;
/// unrelated tasks can use independent connections and processes.
/// The filesystem must implement SQLite locking and durable fsync correctly.
/// Methods commit synchronously so dropping an async caller cannot detach a
/// durability write. Runtime should place this on a suitable execution thread.
pub struct SqliteJournal {
    pub(crate) connection: Mutex<Connection>,
    path: PathBuf,
}
/// Explicit unlock prevents a forked process's transient inherited descriptor
/// from extending a stopped executor lease until it reaches exec/exit.
pub(crate) struct JournalLock(File);
impl Drop for JournalLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
impl SqliteJournal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CellError> {
        let path = path.as_ref();
        if !path.is_absolute()
            || path.file_name().is_none()
            || path.file_name().is_some_and(|name| name == ":memory:")
        {
            return Err(CellError::Invalid(
                "journal requires an absolute file path".into(),
            ));
        }
        // Resolve aliases before choosing the lock file; otherwise two symlink
        // spellings could acquire different locks for the same SQLite database.
        let path = if path.exists() {
            std::fs::canonicalize(path)
        } else {
            std::fs::canonicalize(path.parent().expect("absolute file has parent"))
                .map(|parent| parent.join(path.file_name().expect("file name checked")))
        }
        .map_err(|e| CellError::Journal(e.to_string()))?;
        let path = path.as_path();
        let io = |e: std::io::Error| CellError::Journal(e.to_string());
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS cells_v1 (
                id TEXT PRIMARY KEY, session TEXT NOT NULL, task TEXT NOT NULL,
                revision INTEGER NOT NULL, body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS cells_scope_v1 ON cells_v1(session,task);
            CREATE TABLE IF NOT EXISTS cells_frozen_v1 (session TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS cells_values_v1 (
                session TEXT NOT NULL, key TEXT NOT NULL, version INTEGER NOT NULL,
                body TEXT NOT NULL, bytes INTEGER NOT NULL, PRIMARY KEY(session,key));",
        )?;
        // Persist directory entries for a newly created database/lock/WAL.
        File::open(
            path.parent()
                .ok_or_else(|| CellError::Invalid("journal parent missing".into()))?,
        )
        .and_then(|f| f.sync_all())
        .map_err(io)?;
        Ok(Self {
            connection: Mutex::new(connection),
            path: path.to_path_buf(),
        })
    }
    pub(crate) fn claim(&self, identity: &CellIdentity) -> Result<JournalLock, CellError> {
        use sha2::{Digest, Sha256};
        identity.validate()?;
        let key = serde_json::to_vec(&(&identity.session_id, &identity.task_id))?;
        let mut lock_path = self.path.as_os_str().to_owned();
        lock_path.push(format!(".cells-task-{:x}.lock", Sha256::digest(key)));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| CellError::Journal(e.to_string()))?;
        FileExt::try_lock_exclusive(&lock).map_err(|e| {
            CellError::Journal(format!(
                "session/task already has a live executor or lock unavailable: {e}"
            ))
        })?;
        Ok(JournalLock(lock))
    }
    /// Shared for the exact lifetime of a VM, exclusive while validating/thawing
    /// a stopped session. Independent managers for different tasks still run.
    pub(crate) fn claim_session(
        &self,
        session: &str,
        exclusive: bool,
    ) -> Result<JournalLock, CellError> {
        use sha2::{Digest, Sha256};
        let mut path = self.path.as_os_str().to_owned();
        path.push(format!(
            ".cells-vm-{:x}.lock",
            Sha256::digest(session.as_bytes())
        ));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| CellError::Journal(error.to_string()))?;
        let result = if exclusive {
            FileExt::try_lock_exclusive(&lock)
        } else {
            FileExt::try_lock_shared(&lock)
        };
        result.map_err(|_| CellError::Frozen)?;
        Ok(JournalLock(lock))
    }
    pub fn is_frozen(&self, session: &str) -> Result<bool, CellError> {
        let connection = self.connection.lock().expect("journal lock poisoned");
        frozen(&connection, session)
    }
    pub(crate) fn create(&self, record: &CellRecord, limits: &CellLimits) -> Result<(), CellError> {
        let mut connection = self.connection.lock().expect("journal lock poisoned");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if frozen(&tx, &record.identity.session_id)? {
            return Err(CellError::Frozen);
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM cells_v1 WHERE session=?1 AND task=?2",
            params![record.identity.session_id, record.identity.task_id],
            |row| row.get(0),
        )?;
        if count >= limits.max_cells as i64 {
            return Err(CellError::Capacity);
        }
        tx.execute(
            "INSERT INTO cells_v1(id,session,task,revision,body) VALUES(?1,?2,?3,?4,?5)",
            params![
                record.cell_id,
                record.identity.session_id,
                record.identity.task_id,
                sql_u64(record.revision)?,
                serde_json::to_string(record)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn persist(
        &self,
        record: &CellRecord,
        previous_revision: u64,
    ) -> Result<(), CellError> {
        let mut connection = self.connection.lock().expect("journal lock poisoned");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute("UPDATE cells_v1 SET body=?1,revision=?2 WHERE id=?3 AND revision=?4 AND session=?5 AND task=?6", params![serde_json::to_string(record)?, sql_u64(record.revision)?, record.cell_id, sql_u64(previous_revision)?, record.identity.session_id, record.identity.task_id])?;
        if changed != 1 {
            return Err(CellError::Conflict);
        }
        if record.status == CellStatus::OutcomeUnknown {
            tx.execute(
                "INSERT OR IGNORE INTO cells_frozen_v1(session) VALUES(?1)",
                [&record.identity.session_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Reads persistent records without interpreting an open connection as a crash.
    pub fn records(
        &self,
        identity: &CellIdentity,
        limits: &CellLimits,
    ) -> Result<Vec<CellRecord>, CellError> {
        identity.validate()?;
        limits.validate()?;
        let connection = self.connection.lock().expect("journal lock poisoned");
        let mut stmt = connection.prepare(
            "SELECT body FROM cells_v1 WHERE session=?1 AND task=?2 ORDER BY id LIMIT ?3",
        )?;
        let records = stmt
            .query_map(
                params![
                    identity.session_id,
                    identity.task_id,
                    (limits.max_cells + 1) as i64
                ],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| Ok(serde_json::from_str::<CellRecord>(&row?)?))
            .collect::<Result<Vec<_>, CellError>>()?;
        if records.len() > limits.max_cells {
            return Err(CellError::Capacity);
        }
        Ok(records)
    }
    /// Trusted explicit recovery. The caller asserts its former executor has
    /// stopped; a live task lock rejects recovery even with that assertion.
    /// No source, host operation, or Lua stack is ever replayed. This classifies
    /// lost VMs without host observations. Runtime settlement instead retains a
    /// StoppedScope across its own durable repair and the Cells synchronization.
    pub fn recover_stopped(
        &self,
        stopped: &ExecutorStopped,
        limits: &CellLimits,
    ) -> Result<Vec<CellRecord>, CellError> {
        stopped.identity.validate()?;
        limits.validate()?;
        if stopped.evidence.trim().is_empty() || stopped.evidence.len() > 1024 {
            return Err(CellError::Invalid(
                "executor-stop assertion requires 1..=1024 bytes of trusted evidence".into(),
            ));
        }
        let identity = &stopped.identity;
        let _execution_lock = self.claim(identity)?;
        let mut connection = self.connection.lock().expect("journal lock poisoned");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut records = {
            let mut stmt = tx.prepare(
                "SELECT body FROM cells_v1 WHERE session=?1 AND task=?2 ORDER BY id LIMIT ?3",
            )?;
            stmt.query_map(
                params![
                    identity.session_id,
                    identity.task_id,
                    (limits.max_cells + 1) as i64
                ],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| Ok(serde_json::from_str::<CellRecord>(&row?)?))
            .collect::<Result<Vec<_>, CellError>>()?
        };
        if records.len() > limits.max_cells {
            return Err(CellError::Capacity);
        }
        for record in &mut records {
            if matches!(record.status, CellStatus::Running | CellStatus::Yielded)
                || record.unresolved()
            {
                for call in &mut record.calls {
                    if call.status == IntentStatus::Pending {
                        call.status = IntentStatus::OutcomeUnknown;
                    }
                }
                record.status = if record.unresolved() {
                    CellStatus::OutcomeUnknown
                } else {
                    CellStatus::Interrupted
                };
                record.error = Some("Process stopped; the Luau stack is unavailable. The script will not be replayed.".into());
                record.revision += 1;
                tx.execute(
                    "UPDATE cells_v1 SET body=?1,revision=?2 WHERE id=?3",
                    params![
                        serde_json::to_string(record)?,
                        sql_u64(record.revision)?,
                        record.cell_id
                    ],
                )?;
                if record.unresolved() {
                    tx.execute(
                        "INSERT OR IGNORE INTO cells_frozen_v1(session) VALUES(?1)",
                        [&identity.session_id],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(records)
    }
    pub fn load(&self, session: &str, key: &str) -> Result<Option<StoreSnapshot>, CellError> {
        validate_key(key)?;
        let connection = self.connection.lock().expect("journal lock poisoned");
        let row = connection
            .query_row(
                "SELECT version,body FROM cells_values_v1 WHERE session=?1 AND key=?2",
                params![session, key],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(version, body)| {
            Ok(StoreSnapshot {
                version,
                value: serde_json::from_str(&body)?,
            })
        })
        .transpose()
    }
    /// Compare-and-set: expected_version=0 creates; otherwise it must exactly
    /// match the last immutable snapshot. All values share a session byte cap.
    pub fn store(
        &self,
        session: &str,
        key: &str,
        expected_version: u64,
        value: Value,
        limits: &CellLimits,
    ) -> Result<StoreSnapshot, CellError> {
        // Every early rejection still owns an unchecked host-created Value.
        // Validate limits before using their depth to bound recursive operations.
        if let Err(error) = validate_key(key)
            .and_then(|()| limits.validate())
            .and_then(|()| validate_json(&value, limits.json_depth, limits.store_bytes))
        {
            discard_json(value);
            return Err(error);
        }
        let body = serde_json::to_string(&value)?;
        let mut connection = self.connection.lock().expect("journal lock poisoned");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if frozen(&tx, session)? {
            return Err(CellError::Frozen);
        }
        let old = tx
            .query_row(
                "SELECT version,bytes FROM cells_values_v1 WHERE session=?1 AND key=?2",
                params![session, key],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as usize)),
            )
            .optional()?;
        let (version, old_bytes) = old.unwrap_or_default();
        if expected_version != version {
            return Err(CellError::Conflict);
        }
        let (keys, bytes): (usize, usize) = tx.query_row(
            "SELECT count(*),coalesce(sum(bytes),0) FROM cells_values_v1 WHERE session=?1",
            [session],
            |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
        )?;
        if (old.is_none() && keys >= limits.store_keys)
            || bytes.saturating_sub(old_bytes).saturating_add(body.len()) > limits.store_bytes
        {
            return Err(CellError::Capacity);
        }
        let version = version
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or(CellError::Capacity)?;
        tx.execute("INSERT INTO cells_values_v1(session,key,version,body,bytes) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session,key) DO UPDATE SET version=excluded.version,body=excluded.body,bytes=excluded.bytes", params![session,key,sql_u64(version)?,body,i64::try_from(body.len()).map_err(|_| CellError::Capacity)?])?;
        tx.commit()?;
        Ok(StoreSnapshot { version, value })
    }
}
fn sql_u64(value: u64) -> Result<i64, CellError> {
    i64::try_from(value).map_err(|_| CellError::Capacity)
}
fn frozen(connection: &Connection, session: &str) -> Result<bool, CellError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM cells_frozen_v1 WHERE session=?1)",
        [session],
        |r| r.get(0),
    )?)
}
fn validate_key(key: &str) -> Result<(), CellError> {
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(CellError::Invalid(
            "store key requires 1..=128 non-control bytes".into(),
        ));
    }
    Ok(())
}
