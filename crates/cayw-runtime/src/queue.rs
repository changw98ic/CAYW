//! Durable task identity and FIFO admission. Opening a database never claims work.
use crate::{
    RuntimeError,
    audit::{read_u64, sql_u64},
    input::TaskInput,
    lock,
};
use cayw_ai::timestamp_ms;
use cayw_requests::TaskIntent;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Mutex};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    /// Execution has stopped admitting work, but durable cleanup needs recovery.
    Draining,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub session_id: String,
    pub input: TaskInput,
    pub intent: TaskIntent,
    pub state: TaskState,
    pub queued_at: u64,
    pub worker_id: Option<String>,
    pub attempt_id: Option<String>,
    pub fence: u64,
    pub cancel_requested: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskLease {
    pub id: String,
    pub worker: String,
    pub fence: u64,
}
pub(crate) struct Queue {
    connection: Mutex<Connection>,
}

/// One decision supplies both the queue terminal and its authority/audit outcome.
#[derive(Clone, Copy)]
pub(crate) enum TaskTerminal {
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
}
impl TaskTerminal {
    fn records(self) -> (&'static str, &'static str) {
        match self {
            Self::Completed => ("completed", "run_finished"),
            Self::Failed => ("failed", "run_failed"),
            Self::Cancelled => ("cancelled", "run_cancelled"),
            Self::OutcomeUnknown => ("outcome_unknown", "run_outcome_unknown"),
        }
    }
    pub(crate) fn request_outcome(self) -> cayw_requests::TaskOutcome {
        match self {
            Self::Completed => cayw_requests::TaskOutcome::Completed,
            Self::Cancelled => cayw_requests::TaskOutcome::Cancelled,
            Self::Failed | Self::OutcomeUnknown => cayw_requests::TaskOutcome::Failed,
        }
    }
}

fn state(value: &str) -> Result<TaskState, rusqlite::Error> {
    match value {
        "queued" => Ok(TaskState::Queued),
        "running" => Ok(TaskState::Running),
        "draining" => Ok(TaskState::Draining),
        "completed" => Ok(TaskState::Completed),
        "failed" => Ok(TaskState::Failed),
        "cancelled" => Ok(TaskState::Cancelled),
        "interrupted" => Ok(TaskState::Interrupted),
        "outcome_unknown" => Ok(TaskState::OutcomeUnknown),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        input: serde_json::from_str(&row.get::<_, String>(2)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        state: state(&row.get::<_, String>(3)?)?,
        queued_at: read_u64(row, 4)?,
        worker_id: row.get(5)?,
        attempt_id: row.get(6)?,
        fence: read_u64(row, 7)?,
        cancel_requested: row.get(8)?,
        error: row.get(9)?,
        intent: serde_json::from_str(&row.get::<_, String>(10)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}
const COLUMNS: &str =
    "id,session,input_json,state,queued_at,worker,attempt,fence,cancel_requested,error,intent";

impl Queue {
    pub(crate) fn open(
        path: &Path,
        concurrency: usize,
        capacity: usize,
    ) -> Result<Self, RuntimeError> {
        let mut db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS runtime_queue_config (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), concurrency INTEGER NOT NULL, capacity INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_tasks (
            ticket INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, session TEXT NOT NULL,
            input_json TEXT NOT NULL, state TEXT NOT NULL, queued_at INTEGER NOT NULL,
            worker TEXT, attempt TEXT, fence INTEGER NOT NULL DEFAULT 0,
            cancel_requested INTEGER NOT NULL DEFAULT 0, error TEXT, intent TEXT NOT NULL DEFAULT '{\"kind\":\"general\"}');")?;
        let (has_intent, has_text, has_input): (bool, bool, bool) = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runtime_tasks') WHERE name='intent'),
             EXISTS(SELECT 1 FROM pragma_table_info('runtime_tasks') WHERE name='text'),
             EXISTS(SELECT 1 FROM pragma_table_info('runtime_tasks') WHERE name='input_json')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if !has_intent {
            // Existing tasks had no explicit host intent. Migrate them to the
            // canonical General value; never infer permission from prompt text.
            tx.execute_batch("ALTER TABLE runtime_tasks ADD COLUMN intent TEXT NOT NULL DEFAULT '{\"kind\":\"general\"}';")?;
        }
        if has_text {
            if has_input {
                return Err(RuntimeError::InvalidInput(
                    "runtime task schema has both text and input_json".into(),
                ));
            }
            // One migration replaces the old text-only column. A temporary
            // default fills existing rows before their real text is copied.
            tx.execute_batch("ALTER TABLE runtime_tasks ADD COLUMN input_json TEXT NOT NULL DEFAULT '{\"text\":\"\",\"images\":[]}';")?;
            tx.execute(
                "UPDATE runtime_tasks SET input_json=json_object('text',text,'images',json('[]'))",
                [],
            )?;
            tx.execute_batch("ALTER TABLE runtime_tasks DROP COLUMN text;")?;
        } else if !has_input {
            return Err(RuntimeError::InvalidInput(
                "runtime task schema has no input_json column".into(),
            ));
        }
        tx.execute_batch(
            "DROP INDEX IF EXISTS runtime_one_live_task;
             CREATE UNIQUE INDEX runtime_one_live_task ON runtime_tasks(session) WHERE state IN ('queued','running','draining');",
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO runtime_queue_config VALUES (1,?1,?2)",
            params![concurrency as i64, capacity as i64],
        )?;
        let configured: (i64, i64) = tx.query_row(
            "SELECT concurrency,capacity FROM runtime_queue_config WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if configured != (concurrency as i64, capacity as i64) {
            return Err(RuntimeError::InvalidInput(
                "all runtime connections must use the database's queue limits".into(),
            ));
        }
        tx.commit()?;
        Ok(Self {
            connection: Mutex::new(db),
        })
    }
    pub(crate) fn enqueue_input(
        &self,
        session: &str,
        input: &TaskInput,
        intent: &TaskIntent,
        capacity: usize,
    ) -> Result<TaskRecord, RuntimeError> {
        let input_json = serde_json::to_string(input)?;
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_tasks WHERE session=?1 AND state IN ('queued','running','draining'))",[session],|r|r.get(0))?;
        if duplicate {
            return Err(RuntimeError::Busy);
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM runtime_tasks WHERE state IN ('queued','running','draining')",
            [],
            |r| r.get(0),
        )?;
        if count >= capacity as i64 {
            return Err(RuntimeError::QueueFull);
        }
        let id = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO runtime_tasks(id,session,input_json,state,queued_at,intent) VALUES (?1,?2,?3,'queued',?4,?5)",params![id,session,input_json,sql_u64(timestamp_ms())?,serde_json::to_string(intent)?])?;
        let task = tx.query_row(
            &format!("SELECT {COLUMNS} FROM runtime_tasks WHERE id=?1"),
            [id],
            row,
        )?;
        tx.commit()?;
        Ok(task)
    }
    /// Child work starts as running in one IMMEDIATE transaction. Capacity and
    /// concurrency are checked without waiting, so the caller can return to its
    /// parent instead of occupying a queued slot.
    pub(crate) fn try_start_child(
        &self,
        session: &str,
        text: &str,
        intent: &TaskIntent,
        capacity: usize,
        concurrency: usize,
    ) -> Result<TaskLease, RuntimeError> {
        let input = TaskInput {
            text: text.into(),
            images: vec![],
        };
        let input_json = serde_json::to_string(&input)?;
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_tasks WHERE session=?1 AND state IN ('queued','running','draining'))",[session],|r|r.get(0))?;
        if duplicate {
            return Err(RuntimeError::Busy);
        }
        let live: i64 = tx.query_row(
            "SELECT count(*) FROM runtime_tasks WHERE state IN ('queued','running','draining')",
            [],
            |r| r.get(0),
        )?;
        if live >= capacity as i64 {
            return Err(RuntimeError::QueueFull);
        }
        let active: i64 = tx.query_row(
            "SELECT count(*) FROM runtime_tasks WHERE state IN ('running','draining')",
            [],
            |r| r.get(0),
        )?;
        if active >= concurrency as i64 {
            return Err(RuntimeError::QueueFull);
        }
        let id = Uuid::new_v4().to_string();
        let worker = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO runtime_tasks(id,session,input_json,state,queued_at,worker,fence,intent) VALUES (?1,?2,?3,'running',?4,?5,1,?6)",params![id,session,input_json,sql_u64(timestamp_ms())?,worker,serde_json::to_string(intent)?])?;
        tx.commit()?;
        Ok(TaskLease {
            id,
            worker,
            fence: 1,
        })
    }
    pub(crate) fn get(&self, id: &str) -> Result<TaskRecord, RuntimeError> {
        lock(&self.connection)
            .query_row(
                &format!("SELECT {COLUMNS} FROM runtime_tasks WHERE id=?1"),
                [id],
                row,
            )
            .optional()?
            .ok_or_else(|| RuntimeError::InvalidInput("task not found".into()))
    }
    pub(crate) fn intent(&self, id: &str) -> Result<TaskIntent, RuntimeError> {
        Ok(self.get(id)?.intent)
    }
    pub(crate) fn list(&self, session: &str) -> Result<Vec<TaskRecord>, RuntimeError> {
        let db = lock(&self.connection);
        let mut stmt = db.prepare(&format!(
            "SELECT {COLUMNS} FROM runtime_tasks WHERE session=?1 ORDER BY ticket"
        ))?;
        Ok(stmt
            .query_map([session], row)?
            .collect::<Result<Vec<_>, _>>()?)
    }
    /// Explicitly fence a queued worker. A running task can only be recovered,
    /// never resumed or reissued; this operation does not replay an attempt.
    pub(crate) fn claim(&self, id: &str) -> Result<TaskLease, RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let worker = Uuid::new_v4().to_string();
        let n = tx.execute("UPDATE runtime_tasks SET worker=?2,fence=fence+1 WHERE id=?1 AND state='queued' AND cancel_requested=0 AND fence<9223372036854775807",params![id,worker])?;
        if n != 1 {
            return Err(RuntimeError::InvalidInput(
                "only an uncancelled queued task may be resumed".into(),
            ));
        }
        let fence = tx.query_row("SELECT fence FROM runtime_tasks WHERE id=?1", [id], |r| {
            read_u64(r, 0)
        })?;
        tx.commit()?;
        Ok(TaskLease {
            id: id.into(),
            worker,
            fence,
        })
    }
    pub(crate) fn check(&self, lease: &TaskLease) -> Result<(), RuntimeError> {
        let valid: Option<bool> = lock(&self.connection).query_row("SELECT cancel_requested FROM runtime_tasks WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running')",params![lease.id,lease.worker,sql_u64(lease.fence)?],|r|r.get(0)).optional()?;
        match valid {
            Some(false) => Ok(()),
            Some(true) => Err(RuntimeError::Cancelled),
            None => Err(RuntimeError::Worker("task worker was fenced".into())),
        }
    }
    /// Model admission/settlement reads this under lifecycle coordination, the
    /// same fence used by peer cancellation. Cancellation may still be recorded
    /// while cleanup is draining; never infer it from a local token alone.
    pub(crate) fn model_cancelled(
        &self,
        task: &TaskLease,
        attempt: &cayw_core::AttemptLease,
    ) -> Result<bool, RuntimeError> {
        let cancelled = lock(&self.connection)
            .query_row(
                "SELECT cancel_requested FROM runtime_tasks WHERE id=?1 AND session=?2 AND worker=?3 AND attempt=?4 AND fence=?5 AND state IN ('running','draining')",
                params![task.id, attempt.session_id, task.worker, attempt.attempt_id, sql_u64(task.fence)?],
                |row| row.get(0),
            )
            .optional()?;
        cancelled
            .ok_or_else(|| RuntimeError::Worker("model request task identity was fenced".into()))
    }
    pub(crate) fn admit(
        &self,
        lease: &TaskLease,
        concurrency: usize,
    ) -> Result<bool, RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_tasks WHERE id=?1 AND worker=?2 AND fence=?3 AND state='queued' AND cancel_requested=0)",params![lease.id,lease.worker,sql_u64(lease.fence)?],|r|r.get(0))?;
        if !valid {
            return Err(RuntimeError::Cancelled);
        }
        let earliest: Option<String> = tx.query_row("SELECT id FROM runtime_tasks WHERE state='queued' AND cancel_requested=0 ORDER BY ticket LIMIT 1",[],|r|r.get(0)).optional()?;
        let running: i64 = tx.query_row(
            "SELECT count(*) FROM runtime_tasks WHERE state IN ('running','draining')",
            [],
            |r| r.get(0),
        )?;
        let admitted = earliest.as_deref() == Some(&lease.id) && running < concurrency as i64;
        if admitted {
            tx.execute(
                "UPDATE runtime_tasks SET state='running' WHERE id=?1",
                [&lease.id],
            )?;
        }
        tx.commit()?;
        Ok(admitted)
    }
    pub(crate) fn attach(&self, task: &TaskLease, attempt: &str) -> Result<(), RuntimeError> {
        let n = lock(&self.connection).execute("UPDATE runtime_tasks SET attempt=?4 WHERE id=?1 AND worker=?2 AND fence=?3 AND state='running' AND cancel_requested=0",params![task.id,task.worker,sql_u64(task.fence)?,attempt])?;
        if n != 1 {
            return Err(RuntimeError::Worker(
                "task was fenced before attempt attachment".into(),
            ));
        }
        Ok(())
    }
    /// Serialize cancel versus finish in the same IMMEDIATE transaction that
    /// commits the task and audit. The caller holds lifecycle coordination while
    /// ending Requests from this decision. Failed bookkeeping retains admission.
    pub(crate) fn finish(
        &self,
        lease: &TaskLease,
        proposed: TaskTerminal,
        error: Option<String>,
        settle: impl FnOnce(TaskTerminal) -> Result<(), RuntimeError>,
    ) -> Result<(), RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: Option<(String, bool)> = tx.query_row(
            "SELECT session,cancel_requested FROM runtime_tasks WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running')",
            params![lease.id, lease.worker, sql_u64(lease.fence)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let (session, cancelled) = record
            .ok_or_else(|| RuntimeError::Worker("task terminal rejected after fencing".into()))?;
        let terminal = match proposed {
            TaskTerminal::OutcomeUnknown => TaskTerminal::OutcomeUnknown,
            _ if cancelled => TaskTerminal::Cancelled,
            other => other,
        };
        let (state, kind) = terminal.records();
        let data = serde_json::json!({"worker_id":lease.worker,"error":error});
        tx.execute(
            "INSERT INTO runtime_audit(session,task,kind,timestamp,payload) VALUES (?1,?2,?3,?4,?5)",
            params![session, lease.id, kind, sql_u64(timestamp_ms())?, serde_json::to_string(&data)?],
        )?;
        tx.execute(
            "UPDATE runtime_tasks SET state=?4,error=?5 WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running')",
            params![lease.id, lease.worker, sql_u64(lease.fence)?, state, error],
        )?;
        settle(terminal)?;
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "queue.task_terminal.commit.before",
            serde_json::json!({"session_id":session,"task_id":lease.id,"worker_id":lease.worker,"fence":lease.fence,"state":state}),
        );
        tx.commit()?;
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "queue.task_terminal.commit.committed",
            serde_json::json!({"session_id":session,"task_id":lease.id,"worker_id":lease.worker,"fence":lease.fence,"state":state}),
        );
        Ok(())
    }
    pub(crate) fn block(&self, lease: &TaskLease, error: &str) -> Result<(), RuntimeError> {
        let n = lock(&self.connection).execute(
            "UPDATE runtime_tasks SET state='draining',error=?4 WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running','draining')",
            params![lease.id, lease.worker, sql_u64(lease.fence)?, error],
        )?;
        if n != 1 {
            return Err(RuntimeError::Worker(
                "cleanup record rejected after fencing".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn cancel(&self, target: &crate::CancelTarget) -> Result<bool, RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = tx.query_row(
            &format!("SELECT {COLUMNS} FROM runtime_tasks WHERE id=?1"),
            [&target.task_id],
            row,
        )?;
        if task.cancel_target() != *target {
            return Err(RuntimeError::StaleCancellation);
        }
        if !matches!(
            task.state,
            TaskState::Queued | TaskState::Running | TaskState::Draining
        ) {
            return Ok(false);
        }
        // A claimed entry retains its original worker and capacity until cleanup.
        if task.state == TaskState::Queued && task.worker_id.is_none() {
            tx.execute(
                "INSERT INTO runtime_audit(session,task,kind,timestamp,payload) VALUES (?1,?2,'run_cancelled',?3,'{\"worker_id\":null,\"error\":null}')",
                params![target.session_id, target.task_id, sql_u64(timestamp_ms())?],
            )?;
        }
        let changed = tx.execute(
            "UPDATE runtime_tasks SET cancel_requested=1,state=CASE WHEN state='queued' AND worker IS NULL THEN 'cancelled' ELSE state END WHERE id=?1 AND session=?2 AND worker IS ?3 AND attempt IS ?4 AND fence=?5 AND state IN ('queued','running','draining')",
            params![target.task_id, target.session_id, target.worker_id, target.attempt_id, sql_u64(target.fence)?],
        )?;
        tx.commit()?;
        Ok(changed != 0)
    }
    /// Fences first and releases slots only on explicit host recovery. An old
    /// owner cannot settle or admit a task after this transaction commits.
    pub(crate) fn recover(&self, session: &str) -> Result<(), RuntimeError> {
        lock(&self.connection).execute("UPDATE runtime_tasks SET state='interrupted',cancel_requested=1,fence=fence+1,error='explicit worker recovery; inspect Core outcomes' WHERE session=?1 AND state IN ('queued','running','draining')",[session])?;
        Ok(())
    }
    pub(crate) fn counts(&self) -> Result<(usize, usize), RuntimeError> {
        Ok(lock(&self.connection).query_row("SELECT count(*) FILTER(WHERE state IN ('running','draining')),count(*) FILTER(WHERE state='queued') FROM runtime_tasks",[],|r|Ok((r.get::<_,i64>(0)? as usize,r.get::<_,i64>(1)? as usize)))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditDb;

    #[test]
    fn peer_cancellation_wins_before_finish_for_task_and_audit() {
        // Two real connections impose the failing ordering without timers.
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let owner = Queue::open(&path, 1, 4).unwrap();
        let peer = Queue::open(&path, 1, 4).unwrap();
        let task = owner
            .enqueue_input(
                "session",
                &TaskInput {
                    text: "synthetic input".into(),
                    images: vec![],
                },
                &TaskIntent::General,
                4,
            )
            .unwrap();
        let lease = owner.claim(&task.id).unwrap();
        assert!(owner.admit(&lease, 1).unwrap());
        owner.attach(&lease, "original-attempt").unwrap();
        assert!(
            peer.cancel(&owner.get(&task.id).unwrap().cancel_target())
                .unwrap()
        );
        owner
            .finish(&lease, TaskTerminal::Completed, None, |_| Ok(()))
            .unwrap();
        let terminal = peer.get(&task.id).unwrap();
        let records = audit.events("session", 0, 100).unwrap();
        assert_eq!(
            (terminal.state, records[0].kind.as_str()),
            (TaskState::Cancelled, "run_cancelled")
        );
        assert!(terminal.cancel_requested);
        assert_eq!(terminal.worker_id.as_ref(), Some(&lease.worker));
        assert_eq!(terminal.attempt_id.as_deref(), Some("original-attempt"));
        assert_eq!(terminal.fence, lease.fence);
        assert_eq!(records.len(), 1);
        assert!(
            owner
                .finish(&lease, TaskTerminal::Completed, None, |_| Ok(()))
                .is_err()
        );
        assert!(
            !peer
                .cancel(&owner.get(&task.id).unwrap().cancel_target())
                .unwrap()
        );
        assert_eq!(peer.get(&task.id).unwrap(), terminal);
        assert_eq!(audit.events("session", 0, 100).unwrap().len(), 1);
    }

    #[test]
    fn finish_before_peer_cancel_is_immutable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let owner = Queue::open(&path, 1, 4).unwrap();
        let peer = Queue::open(&path, 1, 4).unwrap();
        let task = owner
            .enqueue_input(
                "session",
                &TaskInput {
                    text: "synthetic input".into(),
                    images: vec![],
                },
                &TaskIntent::General,
                4,
            )
            .unwrap();
        let lease = owner.claim(&task.id).unwrap();
        assert!(owner.admit(&lease, 1).unwrap());
        owner
            .finish(&lease, TaskTerminal::Completed, None, |_| Ok(()))
            .unwrap();
        assert!(
            !peer
                .cancel(&owner.get(&task.id).unwrap().cancel_target())
                .unwrap()
        );
        assert_eq!(owner.get(&task.id).unwrap().state, TaskState::Completed);
        assert_eq!(
            audit.events("session", 0, 100).unwrap()[0].kind,
            "run_finished"
        );
    }

    #[test]
    fn unknown_terminal_takes_priority_over_accepted_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let owner = Queue::open(&path, 1, 4).unwrap();
        let peer = Queue::open(&path, 1, 4).unwrap();
        let task = owner
            .enqueue_input(
                "session",
                &TaskInput {
                    text: "synthetic input".into(),
                    images: vec![],
                },
                &TaskIntent::General,
                4,
            )
            .unwrap();
        let lease = owner.claim(&task.id).unwrap();
        assert!(owner.admit(&lease, 1).unwrap());
        assert!(
            peer.cancel(&owner.get(&task.id).unwrap().cancel_target())
                .unwrap()
        );
        owner
            .finish(&lease, TaskTerminal::OutcomeUnknown, None, |terminal| {
                assert_eq!(
                    terminal.request_outcome(),
                    cayw_requests::TaskOutcome::Failed
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(
            owner.get(&task.id).unwrap().state,
            TaskState::OutcomeUnknown
        );
        assert_eq!(
            audit.events("session", 0, 100).unwrap()[0].kind,
            "run_outcome_unknown"
        );
    }

    #[test]
    fn unclaimed_cancel_commits_one_terminal_without_a_worker() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let owner = Queue::open(&path, 1, 4).unwrap();
        let peer = Queue::open(&path, 1, 4).unwrap();
        let task = owner
            .enqueue_input(
                "session",
                &TaskInput {
                    text: "synthetic input".into(),
                    images: vec![],
                },
                &TaskIntent::General,
                4,
            )
            .unwrap();
        assert!(
            peer.cancel(&owner.get(&task.id).unwrap().cancel_target())
                .unwrap()
        );
        assert!(
            !peer
                .cancel(&owner.get(&task.id).unwrap().cancel_target())
                .unwrap()
        );
        assert!(owner.claim(&task.id).is_err());
        assert_eq!(owner.get(&task.id).unwrap().state, TaskState::Cancelled);
        assert_eq!(owner.counts().unwrap(), (0, 0));
        let records = audit.events("session", 0, 100).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, "run_cancelled");
        assert!(records[0].data["worker_id"].is_null());
    }

    #[test]
    fn fenced_or_wrong_worker_cannot_append_terminal_or_audit() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let owner = Queue::open(&path, 1, 4).unwrap();
        let peer = Queue::open(&path, 1, 4).unwrap();
        let task = owner
            .enqueue_input(
                "session",
                &TaskInput {
                    text: "synthetic input".into(),
                    images: vec![],
                },
                &TaskIntent::General,
                4,
            )
            .unwrap();
        let lease = owner.claim(&task.id).unwrap();
        assert!(owner.admit(&lease, 1).unwrap());
        let wrong = TaskLease {
            worker: "wrong-worker".into(),
            ..lease.clone()
        };
        assert!(
            owner
                .finish(&wrong, TaskTerminal::Completed, None, |_| panic!(
                    "fenced settlement ran"
                ))
                .is_err()
        );
        peer.recover("session").unwrap();
        assert!(
            owner
                .finish(&lease, TaskTerminal::Completed, None, |_| panic!(
                    "old settlement ran"
                ))
                .is_err()
        );
        assert_eq!(owner.get(&task.id).unwrap().state, TaskState::Interrupted);
        assert!(audit.events("session", 0, 100).unwrap().is_empty());
    }

    #[test]
    fn failed_authority_settlement_rolls_back_task_and_audit() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let owner = Queue::open(&path, 1, 4).unwrap();
        let task = owner
            .enqueue_input(
                "session",
                &TaskInput {
                    text: "synthetic input".into(),
                    images: vec![],
                },
                &TaskIntent::General,
                4,
            )
            .unwrap();
        let lease = owner.claim(&task.id).unwrap();
        assert!(owner.admit(&lease, 1).unwrap());
        assert!(
            owner
                .finish(&lease, TaskTerminal::Completed, None, |_| Err(
                    RuntimeError::Worker("synthetic authority failure".into())
                ))
                .is_err()
        );
        assert_eq!(owner.get(&task.id).unwrap().state, TaskState::Running);
        assert_eq!(owner.counts().unwrap(), (1, 0));
        assert!(audit.events("session", 0, 100).unwrap().is_empty());
    }

    #[tokio::test]
    async fn authority_ended_before_failed_queue_commit_cannot_accept_cancellation() {
        use crate::{RequestPolicy, Runtime, RuntimeConfig};
        use cayw_ai::{
            AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
            ModelRequest, Provider, StopReason,
        };
        use std::sync::Arc;
        struct TerminalProvider;
        impl Provider for TerminalProvider {
            fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
                let mut message = AssistantMessage::new(&request.model);
                message
                    .content
                    .push(Content::text("synthetic known response"));
                message.stop_reason = StopReason::Stop;
                Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
            }
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.sqlite");
        let config = RuntimeConfig {
            max_concurrent_runs: 1,
            max_queued_runs: 1,
            request_policy: Some(RequestPolicy {
                permission_policy_id: "synthetic-commit-failure".into(),
                operations: vec![cayw_requests::OperationScope::exact(
                    "read-fixture",
                    "fs.read",
                    serde_json::json!({"path":"unused.txt"}),
                    "workspace-policy",
                )],
            }),
            ..Default::default()
        };
        let rt = Runtime::open(
            &path,
            root.path(),
            Arc::new(TerminalProvider),
            Model::new("fixture", "commit-failure"),
            config.clone(),
        )
        .unwrap();
        let peer = Runtime::open(
            &path,
            root.path(),
            Arc::new(TerminalProvider),
            Model::new("fixture", "commit-failure"),
            config,
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        // A deferred foreign key fails at COMMIT, after real Requests end_task
        // succeeded. No event/history is rewritten and no production hook exists.
        lock(&rt.inner.queue.connection).execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE fixture_parent(id INTEGER PRIMARY KEY);
             CREATE TABLE fixture_deferred(parent INTEGER REFERENCES fixture_parent(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER fail_terminal_commit AFTER INSERT ON runtime_audit WHEN NEW.kind='run_finished'
             BEGIN INSERT INTO fixture_deferred VALUES(1); END;"
        ).unwrap();
        let error = rt
            .prompt(
                &id,
                "synthetic final commit failure",
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::RecoveryRequired { .. }));
        let task = rt.tasks(&id).unwrap().remove(0);
        assert_eq!(task.state, TaskState::Draining);
        assert_eq!(rt.health().unwrap().running, 1);
        assert!(matches!(
            rt.request_task_state(&task.id).unwrap(),
            cayw_requests::TaskState::Ended {
                outcome: cayw_requests::TaskOutcome::Completed,
                ..
            }
        ));
        assert!(
            !rt.audit(&id, 0, 100)
                .unwrap()
                .iter()
                .any(|event| event.kind.starts_with("run_"))
        );
        assert!(
            matches!(
                peer.cancel(&task.cancel_target()),
                Err(RuntimeError::RecoveryRequired { .. })
            ),
            "a committed authority terminal must not acknowledge a conflicting cancellation"
        );
        lock(&rt.inner.queue.connection)
            .execute_batch("DROP TRIGGER fail_terminal_commit")
            .unwrap();
        peer.recover(&id).unwrap();
        assert_eq!(rt.health().unwrap().running, 0);
        assert!(matches!(
            rt.request_task_state(&task.id).unwrap(),
            cayw_requests::TaskState::Ended {
                outcome: cayw_requests::TaskOutcome::Completed,
                ..
            }
        ));
    }

    #[test]
    fn queued_peer_cancel_can_only_record_one_terminal() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let owner = Queue::open(&path, 1, 4).unwrap();
        let peer = Queue::open(&path, 1, 4).unwrap();
        let task = owner
            .enqueue_input(
                "session",
                &TaskInput {
                    text: "synthetic input".into(),
                    images: vec![],
                },
                &TaskIntent::General,
                4,
            )
            .unwrap();
        let lease = owner.claim(&task.id).unwrap();
        assert!(
            peer.cancel(&owner.get(&task.id).unwrap().cancel_target())
                .unwrap()
        );
        owner
            .finish(&lease, TaskTerminal::Cancelled, None, |_| Ok(()))
            .unwrap();
        assert!(
            owner
                .finish(&lease, TaskTerminal::Cancelled, None, |_| Ok(()))
                .is_err()
        );
        assert_eq!(audit.events("session", 0, 100).unwrap().len(), 1);
    }

    #[test]
    fn legacy_text_column_migrates_once_into_task_input_json() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE runtime_queue_config (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    concurrency INTEGER NOT NULL, capacity INTEGER NOT NULL);
                 CREATE TABLE runtime_tasks (
                    ticket INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
                    session TEXT NOT NULL, text TEXT NOT NULL, state TEXT NOT NULL,
                    queued_at INTEGER NOT NULL, worker TEXT, attempt TEXT,
                    fence INTEGER NOT NULL DEFAULT 0,
                    cancel_requested INTEGER NOT NULL DEFAULT 0, error TEXT,
                    intent TEXT NOT NULL DEFAULT '{\"kind\":\"general\"}');
                 INSERT INTO runtime_queue_config VALUES (1,1,4);
                 INSERT INTO runtime_tasks(id,session,text,state,queued_at,intent)
                 VALUES ('legacy','session','legacy text','queued',1,'{\"kind\":\"general\"}');",
            )
            .unwrap();
        drop(legacy);

        let queue = Queue::open(&path, 1, 4).unwrap();
        let tasks = queue.list("session").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].input,
            TaskInput {
                text: "legacy text".into(),
                images: vec![],
            }
        );
        let db = Connection::open(&path).unwrap();
        let text_columns: i64 = db
            .query_row(
                "SELECT count(*) FROM pragma_table_info('runtime_tasks') WHERE name='text'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(text_columns, 0);
        let input_columns: i64 = db
            .query_row(
                "SELECT count(*) FROM pragma_table_info('runtime_tasks') WHERE name='input_json'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(input_columns, 1);
        drop(db);
        drop(queue);

        // Reopening performs no second migration and keeps the same record.
        let reopened = Queue::open(&path, 1, 4).unwrap();
        assert_eq!(reopened.list("session").unwrap().len(), 1);
    }

    #[test]
    fn try_start_child_starts_running_without_waiting_and_enforces_limits() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = Queue::open(&path, 2, 2).unwrap();

        let first = queue
            .try_start_child("child-a", "first child", &TaskIntent::General, 2, 1)
            .unwrap();
        assert_eq!(first.fence, 1);
        let record = queue.get(&first.id).unwrap();
        assert_eq!(record.state, TaskState::Running);
        assert_eq!(record.worker_id.as_deref(), Some(first.worker.as_str()));
        assert_eq!(
            record.input,
            TaskInput {
                text: "first child".into(),
                images: vec![],
            }
        );
        assert!(matches!(
            queue.try_start_child("child-b", "blocked", &TaskIntent::General, 2, 1),
            Err(RuntimeError::QueueFull)
        ));
        assert!(matches!(
            queue.try_start_child("child-a", "duplicate", &TaskIntent::General, 2, 2),
            Err(RuntimeError::Busy)
        ));
        queue
            .finish(&first, TaskTerminal::Completed, None, |_| Ok(()))
            .unwrap();

        let second = queue
            .try_start_child("child-b", "second child", &TaskIntent::General, 2, 2)
            .unwrap();
        let third = queue
            .try_start_child("child-c", "third child", &TaskIntent::General, 2, 2)
            .unwrap();
        assert_eq!(second.fence, 1);
        assert_eq!(third.fence, 1);
        assert!(matches!(
            queue.try_start_child("child-d", "fourth child", &TaskIntent::General, 2, 2),
            Err(RuntimeError::QueueFull)
        ));
    }
}
