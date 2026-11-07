//! Durable task identity, priority/fairness ordering, and admission. Opening a
//! database never claims work. Policy amendment is coordinated by the host
//! Runtime; this module only reads and adopts the durable policy.
use crate::{
    QueueError, TaskInput, TaskIntent,
    cancel::CancelTarget,
    resources::{ResourcePolicy, ResourceUsage, TaskResources},
};
use cayw_ai::timestamp_ms;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};
use uuid::Uuid;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
fn sql_u64(n: u64) -> Result<i64, QueueError> {
    i64::try_from(n)
        .map_err(|_| QueueError::InvalidInput("integer exceeds persistent counter range".into()))
}
fn read_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let n: i64 = row.get(index)?;
    u64::try_from(n).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, n))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    /// Admission is parked until the host approves the pending request.
    WaitingApproval,
    /// Execution has stopped admitting work, but durable cleanup needs recovery.
    Draining,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Low,
    #[default]
    Normal,
    High,
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
    pub priority: TaskPriority,
    pub holds_slot: bool,
    pub ready_order: Option<u64>,
    pub ready_since_dispatch: Option<u64>,
    pub dispatches: u64,
    pub resources: TaskResources,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLease {
    pub id: String,
    pub worker: String,
    pub fence: u64,
}
pub struct Queue {
    connection: Mutex<Connection>,
}

/// One decision supplies both the queue terminal and its authority/audit outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskTerminal {
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
}

fn state(value: &str) -> Result<TaskState, rusqlite::Error> {
    match value {
        "queued" => Ok(TaskState::Queued),
        "running" => Ok(TaskState::Running),
        "waiting_approval" => Ok(TaskState::WaitingApproval),
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
        priority: priority(&row.get::<_, String>(11)?)?,
        holds_slot: row.get(12)?,
        ready_order: optional_u64(row, 13)?,
        ready_since_dispatch: optional_u64(row, 14)?,
        dispatches: read_u64(row, 15)?,
        resources: TaskResources {
            declared_memory_bytes: read_u64(row, 16)?,
            ..Default::default()
        },
    })
}
fn priority(value: &str) -> Result<TaskPriority, rusqlite::Error> {
    match value {
        "low" => Ok(TaskPriority::Low),
        "normal" => Ok(TaskPriority::Normal),
        "high" => Ok(TaskPriority::High),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}
fn priority_sql(value: TaskPriority) -> &'static str {
    match value {
        TaskPriority::Low => "low",
        TaskPriority::Normal => "normal",
        TaskPriority::High => "high",
    }
}
fn optional_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| {
            u64::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })
        })
        .transpose()
}
fn checked_queue_u64(value: i64, field: &str) -> Result<u64, QueueError> {
    u64::try_from(value)
        .map_err(|_| QueueError::InvalidInput(format!("corrupt queue {field} value")))
}
const COLUMNS: &str = "id,session,input_json,state,queued_at,worker,attempt,fence,cancel_requested,error,intent,priority,holds_slot,ready_order,ready_since_dispatch,dispatches,declared_memory_bytes";

fn resource_policy_in(db: &Connection) -> Result<ResourcePolicy, QueueError> {
    let policy = ResourcePolicy {
        max_declared_memory_bytes: db.query_row(
            "SELECT max_declared_memory_bytes FROM runtime_queue_config WHERE singleton=1",
            [],
            |row| read_u64(row, 0),
        )?,
    };
    policy.validate()?;
    Ok(policy)
}

/// Called inside the caller's consistent transaction. Checked accumulation also
/// fails closed on corrupt data rather than letting SQLite SUM promote or wrap.
fn resource_usage_in(db: &Connection) -> Result<ResourceUsage, QueueError> {
    let policy = resource_policy_in(db)?;
    let mut statement =
        db.prepare("SELECT declared_memory_bytes FROM runtime_tasks WHERE holds_slot=1")?;
    let mut rows = statement.query([])?;
    let mut reserved = 0u64;
    let mut holding_tasks = 0usize;
    while let Some(row) = rows.next()? {
        reserved = reserved.checked_add(read_u64(row, 0)?).ok_or_else(|| {
            QueueError::InvalidInput("resource declared memory accounting overflow".into())
        })?;
        holding_tasks = holding_tasks.checked_add(1).ok_or_else(|| {
            QueueError::InvalidInput("resource holding task count overflow".into())
        })?;
        if reserved > policy.max_declared_memory_bytes {
            return Err(QueueError::InvalidInput(
                "resource reservations exceed the persisted policy".into(),
            ));
        }
    }
    Ok(ResourceUsage {
        max_declared_memory_bytes: policy.max_declared_memory_bytes,
        reserved_declared_memory_bytes: reserved,
        holding_tasks,
    })
}

/// Convert an internal counter without wrapping. SQLite INTEGER is signed.
fn counter_sql(value: u64) -> Result<i64, QueueError> {
    i64::try_from(value).map_err(|_| QueueError::InvalidInput("queue counter overflow".into()))
}
fn dispatch_count_in(tx: &rusqlite::Transaction<'_>) -> Result<u64, QueueError> {
    Ok(tx.query_row(
        "SELECT dispatch_count FROM runtime_queue_config WHERE singleton=1",
        [],
        |row| read_u64(row, 0),
    )?)
}
fn fairness_after_in(tx: &rusqlite::Transaction<'_>) -> Result<u64, QueueError> {
    Ok(tx.query_row(
        "SELECT fairness_after FROM runtime_queue_config WHERE singleton=1",
        [],
        |row| read_u64(row, 0),
    )?)
}
/// Count one dispatch in the same transaction that admits the task.
fn take_dispatch_count(tx: &rusqlite::Transaction<'_>) -> Result<u64, QueueError> {
    let current = dispatch_count_in(tx)?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| QueueError::InvalidInput("queue dispatch counter overflow".into()))?;
    tx.execute(
        "UPDATE runtime_queue_config SET dispatch_count=?1 WHERE singleton=1",
        [counter_sql(next)?],
    )?;
    Ok(next)
}
fn take_ready_order(tx: &rusqlite::Transaction<'_>) -> Result<u64, QueueError> {
    let current: u64 = tx.query_row(
        "SELECT next_ready_order FROM runtime_queue_config WHERE singleton=1",
        [],
        |row| read_u64(row, 0),
    )?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| QueueError::InvalidInput("queue ready order overflow".into()))?;
    tx.execute(
        "UPDATE runtime_queue_config SET next_ready_order=?1 WHERE singleton=1",
        [counter_sql(next)?],
    )?;
    Ok(current)
}
struct ReadyCandidate {
    id: String,
    priority: TaskPriority,
    order: u64,
    since: u64,
}
/// Only claimed, uncancelled queued entries are executable. Ready order and the
/// dispatch count at ready time are durable, so fairness survives restarts.
fn ready_candidates(tx: &rusqlite::Transaction<'_>) -> Result<Vec<ReadyCandidate>, QueueError> {
    let mut statement = tx.prepare(
        "SELECT id,priority,ready_order,ready_since_dispatch FROM runtime_tasks
         WHERE state='queued' AND cancel_requested=0 AND worker IS NOT NULL
           AND ready_order IS NOT NULL AND ready_since_dispatch IS NOT NULL
         ORDER BY ticket",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ReadyCandidate {
            id: row.get(0)?,
            priority: priority(&row.get::<_, String>(1)?)?,
            order: read_u64(row, 2)?,
            since: read_u64(row, 3)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
/// One read-only capacity decision shared by the host precheck and child start.
/// Live rows include queued, running, waiting approval, and draining; slots
/// count only admitted rows; any claimed queued row blocks a direct child.
fn check_child_capacity_in(
    db: &Connection,
    capacity: usize,
    concurrency: usize,
) -> Result<(), QueueError> {
    let (live, active, ready): (i64, i64, bool) = db.query_row(
        "SELECT count(*) FILTER(WHERE state IN ('queued','running','waiting_approval','draining')),
                count(*) FILTER(WHERE holds_slot=1),
                EXISTS(SELECT 1 FROM runtime_tasks WHERE state='queued' AND worker IS NOT NULL AND cancel_requested=0)
         FROM runtime_tasks",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if ready {
        return Err(QueueError::Busy);
    }
    if live >= capacity as i64 || active >= concurrency as i64 {
        return Err(QueueError::QueueFull);
    }
    Ok(())
}
/// Promoted entries are ordered FIFO by the round they became ready. Everything
/// else keeps priority DESC then ready order ASC. A promoted entry is never
/// pushed back by a later arrival, so overtaking is bounded by the window.
fn order_ready(candidates: &mut [ReadyCandidate], dispatch: u64, fairness_after: u64) {
    candidates.sort_by(|left, right| {
        let left_promoted = dispatch.saturating_sub(left.since) >= fairness_after;
        let right_promoted = dispatch.saturating_sub(right.since) >= fairness_after;
        right_promoted.cmp(&left_promoted).then_with(|| {
            if left_promoted {
                left.order.cmp(&right.order)
            } else {
                right
                    .priority
                    .cmp(&left.priority)
                    .then_with(|| left.order.cmp(&right.order))
            }
        })
    });
}

/// The complete durable scheduling/resource policy. A trusted opener may
/// replace it only through the host Runtime's coordinated amendment, which
/// proves that every other Runtime connection has stopped and that no task is
/// live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueuePolicy {
    pub concurrency: u64,
    pub capacity: u64,
    pub fairness_after: u64,
    pub max_declared_memory_bytes: u64,
}
impl QueuePolicy {
    pub fn new(
        concurrency: usize,
        capacity: usize,
        fairness_after: usize,
        resources: &ResourcePolicy,
    ) -> Result<Self, QueueError> {
        resources.validate()?;
        Ok(Self {
            concurrency: concurrency as u64,
            capacity: capacity as u64,
            fairness_after: fairness_after as u64,
            max_declared_memory_bytes: resources.max_declared_memory_bytes,
        })
    }
}

fn query_policy(db: &Connection) -> rusqlite::Result<Option<QueuePolicy>> {
    let complete: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runtime_queue_config') WHERE name='fairness_after')
            AND EXISTS(SELECT 1 FROM pragma_table_info('runtime_queue_config') WHERE name='max_declared_memory_bytes')",
        [],
        |row| row.get(0),
    )?;
    if !complete {
        return Ok(None);
    }
    db.query_row(
        "SELECT concurrency,capacity,fairness_after,max_declared_memory_bytes
         FROM runtime_queue_config WHERE singleton=1",
        [],
        |row| {
            Ok(QueuePolicy {
                concurrency: read_u64(row, 0)?,
                capacity: read_u64(row, 1)?,
                fairness_after: read_u64(row, 2)?,
                max_declared_memory_bytes: read_u64(row, 3)?,
            })
        },
    )
    .optional()
}

/// Read the durable policy without opening a queue connection. The host Runtime
/// calls this while holding its existing policy coordination.
pub fn read_policy(path: &Path) -> Result<Option<QueuePolicy>, QueueError> {
    if !path.exists() {
        return Ok(None);
    }
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    let has_table: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_queue_config')",
        [],
        |row| row.get(0),
    )?;
    if !has_table {
        return Ok(None);
    }
    Ok(query_policy(&db)?)
}

/// Re-reads under the host Runtime's exclusive policy lock. Another opener that
/// already wrote the requested policy wins; otherwise the change requires a
/// settled database. The host owns the lock; this operation does not.
pub fn adopt_policy(path: &Path, requested: QueuePolicy) -> Result<Option<()>, QueueError> {
    let mut db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if query_policy(&tx)? == Some(requested) {
        tx.commit()?;
        return Ok(None);
    }
    let live: i64 = tx.query_row(
        "SELECT count(*) FROM runtime_tasks
         WHERE holds_slot=1 OR state IN ('queued','running','waiting_approval','draining')",
        [],
        |row| row.get(0),
    )?;
    if live != 0 {
        return Err(QueueError::InvalidInput(
            "queue and resource policy can change only when no task is live; settle or recover live tasks first".into(),
        ));
    }
    tx.execute(
        "UPDATE runtime_queue_config SET concurrency=?1,capacity=?2,fairness_after=?3,max_declared_memory_bytes=?4 WHERE singleton=1",
        params![
            counter_sql(requested.concurrency)?,
            counter_sql(requested.capacity)?,
            counter_sql(requested.fairness_after)?,
            counter_sql(requested.max_declared_memory_bytes)?
        ],
    )?;
    tx.commit()?;
    Ok(Some(()))
}

impl Queue {
    #[cfg(test)]
    pub(crate) fn open(
        path: &Path,
        concurrency: usize,
        capacity: usize,
        fairness_after: usize,
    ) -> Result<Self, QueueError> {
        // Tests mirror the host Runtime's coordinated adoption without the host
        // locks: a different durable policy is adopted only when no task is live.
        let resources = ResourcePolicy::default();
        let requested = QueuePolicy::new(concurrency, capacity, fairness_after, &resources)?;
        if read_policy(path)?.is_some_and(|stored| stored != requested) {
            adopt_policy(path, requested)?;
        }
        Self::open_with_resources(path, concurrency, capacity, fairness_after, &resources)
    }
    pub fn open_with_resources(
        path: &Path,
        concurrency: usize,
        capacity: usize,
        fairness_after: usize,
        resources: &ResourcePolicy,
    ) -> Result<Self, QueueError> {
        let requested = QueuePolicy::new(concurrency, capacity, fairness_after, resources)?;
        let concurrency = requested.concurrency;
        let capacity = requested.capacity;
        let fairness_after = requested.fairness_after;
        let mut db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS runtime_queue_config (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), concurrency INTEGER NOT NULL,
            capacity INTEGER NOT NULL, fairness_after INTEGER NOT NULL DEFAULT 0,
            dispatch_count INTEGER NOT NULL DEFAULT 0, next_ready_order INTEGER NOT NULL DEFAULT 0,
            max_declared_memory_bytes INTEGER NOT NULL CHECK(max_declared_memory_bytes>0));
            CREATE TABLE IF NOT EXISTS runtime_tasks (
            ticket INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, session TEXT NOT NULL,
            input_json TEXT NOT NULL, state TEXT NOT NULL, queued_at INTEGER NOT NULL,
            worker TEXT, attempt TEXT, fence INTEGER NOT NULL DEFAULT 0,
            cancel_requested INTEGER NOT NULL DEFAULT 0, error TEXT,
            intent TEXT NOT NULL DEFAULT '{\"kind\":\"general\"}',
            priority TEXT NOT NULL DEFAULT 'normal', holds_slot INTEGER NOT NULL DEFAULT 0,
            ready_order INTEGER, ready_since_dispatch INTEGER, dispatches INTEGER NOT NULL DEFAULT 0,
            declared_memory_bytes INTEGER NOT NULL DEFAULT 0 CHECK(declared_memory_bytes>=0));")?;
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
                return Err(QueueError::InvalidInput(
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
            return Err(QueueError::InvalidInput(
                "runtime task schema has no input_json column".into(),
            ));
        }
        let (has_priority, has_holds_slot, has_ready_order, has_ready_since, has_dispatches): (
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runtime_tasks') WHERE name='priority'),
             EXISTS(SELECT 1 FROM pragma_table_info('runtime_tasks') WHERE name='holds_slot'),
             EXISTS(SELECT 1 FROM pragma_table_info('runtime_tasks') WHERE name='ready_order'),
             EXISTS(SELECT 1 FROM pragma_table_info('runtime_tasks') WHERE name='ready_since_dispatch'),
             EXISTS(SELECT 1 FROM pragma_table_info('runtime_tasks') WHERE name='dispatches')",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        if !has_priority {
            tx.execute_batch(
                "ALTER TABLE runtime_tasks ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal';",
            )?;
        }
        if !has_holds_slot {
            tx.execute_batch(
                "ALTER TABLE runtime_tasks ADD COLUMN holds_slot INTEGER NOT NULL DEFAULT 0;",
            )?;
            // Original running/draining rows held an execution slot.
            tx.execute_batch(
                "UPDATE runtime_tasks SET holds_slot=1 WHERE state IN ('running','draining');",
            )?;
        }
        if !has_ready_order {
            tx.execute_batch("ALTER TABLE runtime_tasks ADD COLUMN ready_order INTEGER;")?;
        }
        if !has_ready_since {
            tx.execute_batch("ALTER TABLE runtime_tasks ADD COLUMN ready_since_dispatch INTEGER;")?;
        }
        if !has_dispatches {
            tx.execute_batch(
                "ALTER TABLE runtime_tasks ADD COLUMN dispatches INTEGER NOT NULL DEFAULT 0;",
            )?;
            tx.execute_batch(
                "UPDATE runtime_tasks SET dispatches=1 WHERE state IN ('running','draining');",
            )?;
        }
        let (has_task_resources, has_resource_policy): (bool, bool) = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runtime_tasks') WHERE name='declared_memory_bytes'),
             EXISTS(SELECT 1 FROM pragma_table_info('runtime_queue_config') WHERE name='max_declared_memory_bytes')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if !has_task_resources {
            // Existing rows made no explicit resource declaration. Preserve their
            // original execution/owner facts and add one canonical zero value.
            tx.execute_batch("ALTER TABLE runtime_tasks ADD COLUMN declared_memory_bytes INTEGER NOT NULL DEFAULT 0 CHECK(declared_memory_bytes>=0);")?;
        }
        if !has_resource_policy {
            // Adopt the trusted policy once, before it becomes shared authority.
            tx.execute_batch("ALTER TABLE runtime_queue_config ADD COLUMN max_declared_memory_bytes INTEGER NOT NULL DEFAULT 1073741824 CHECK(max_declared_memory_bytes>0);")?;
            tx.execute(
                "UPDATE runtime_queue_config SET max_declared_memory_bytes=?1 WHERE singleton=1",
                [counter_sql(resources.max_declared_memory_bytes)?],
            )?;
        }
        let (has_fairness, has_dispatch_count, has_next_ready): (bool, bool, bool) = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runtime_queue_config') WHERE name='fairness_after'),
             EXISTS(SELECT 1 FROM pragma_table_info('runtime_queue_config') WHERE name='dispatch_count'),
             EXISTS(SELECT 1 FROM pragma_table_info('runtime_queue_config') WHERE name='next_ready_order')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if !has_fairness {
            tx.execute_batch("ALTER TABLE runtime_queue_config ADD COLUMN fairness_after INTEGER NOT NULL DEFAULT 0;")?;
            // A database from the older schema has no stored window. Adopt the
            // opener's explicit value so a nonzero runtime default can reopen it
            // instead of failing the consistency check forever.
            tx.execute(
                "UPDATE runtime_queue_config SET fairness_after=?1 WHERE singleton=1",
                [counter_sql(fairness_after)?],
            )?;
        }
        if !has_dispatch_count {
            tx.execute_batch("ALTER TABLE runtime_queue_config ADD COLUMN dispatch_count INTEGER NOT NULL DEFAULT 0;")?;
        }
        if !has_next_ready {
            tx.execute_batch("ALTER TABLE runtime_queue_config ADD COLUMN next_ready_order INTEGER NOT NULL DEFAULT 0;")?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO runtime_queue_config(singleton,concurrency,capacity,fairness_after,max_declared_memory_bytes) VALUES (1,?1,?2,?3,?4)",
            params![
                counter_sql(concurrency)?,
                counter_sql(capacity)?,
                counter_sql(fairness_after)?,
                counter_sql(resources.max_declared_memory_bytes)?
            ],
        )?;
        let configured: (u64, u64, u64) = tx.query_row(
            "SELECT concurrency,capacity,fairness_after FROM runtime_queue_config WHERE singleton=1",
            [],
            |r| Ok((read_u64(r, 0)?, read_u64(r, 1)?, read_u64(r, 2)?)),
        )?;
        if configured != (concurrency, capacity, fairness_after) {
            return Err(QueueError::InvalidInput(
                "all runtime connections must use the database's queue limits".into(),
            ));
        }
        if resource_policy_in(&tx)? != *resources {
            return Err(QueueError::InvalidInput(
                "all runtime connections must use the database's resource policy".into(),
            ));
        }
        resource_usage_in(&tx)?;
        // Claimed queued rows from older schemas are ready now: assign one
        // stable order in ticket order and keep the single ordering algorithm.
        let base: u64 = tx.query_row(
            "SELECT next_ready_order FROM runtime_queue_config WHERE singleton=1",
            [],
            |r| read_u64(r, 0),
        )?;
        let mut next = base;
        let ids: Vec<String> = {
            let mut statement = tx.prepare(
                "SELECT id FROM runtime_tasks WHERE state='queued' AND worker IS NOT NULL AND ready_order IS NULL ORDER BY ticket",
            )?;
            statement
                .query_map([], |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for id in ids {
            let order = counter_sql(next)?;
            next = next
                .checked_add(1)
                .ok_or_else(|| QueueError::InvalidInput("queue ready order overflow".into()))?;
            tx.execute(
                "UPDATE runtime_tasks SET ready_order=?1,ready_since_dispatch=0 WHERE id=?2 AND ready_order IS NULL",
                params![order, id],
            )?;
        }
        if next != base {
            tx.execute(
                "UPDATE runtime_queue_config SET next_ready_order=?1 WHERE singleton=1",
                [counter_sql(next)?],
            )?;
        }
        tx.execute_batch(
            "DROP INDEX IF EXISTS runtime_one_live_task;
             CREATE UNIQUE INDEX runtime_one_live_task ON runtime_tasks(session) WHERE state IN ('queued','running','waiting_approval','draining');",
        )?;
        tx.commit()?;
        Ok(Self {
            connection: Mutex::new(db),
        })
    }
    /// Final check after the host Runtime's last coordinated amendment. It must
    /// not amend the persisted policy.
    pub fn check_policy(
        &self,
        concurrency: usize,
        capacity: usize,
        fairness_after: usize,
        resources: &ResourcePolicy,
    ) -> Result<(), QueueError> {
        let requested = QueuePolicy::new(concurrency, capacity, fairness_after, resources)?;
        if query_policy(&lock(&self.connection))? != Some(requested) {
            return Err(QueueError::InvalidInput(
                "runtime queue or resource policy changed while opening".into(),
            ));
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn enqueue_input(
        &self,
        session: &str,
        input: &TaskInput,
        intent: &TaskIntent,
        capacity: usize,
    ) -> Result<TaskRecord, QueueError> {
        self.enqueue_scheduled(session, input, intent, TaskPriority::Normal, capacity)
    }
    #[cfg(test)]
    pub(crate) fn enqueue_scheduled(
        &self,
        session: &str,
        input: &TaskInput,
        intent: &TaskIntent,
        priority: TaskPriority,
        capacity: usize,
    ) -> Result<TaskRecord, QueueError> {
        self.enqueue_with_resources(
            session,
            input,
            intent,
            priority,
            &TaskResources::default(),
            capacity,
        )
    }
    pub fn enqueue_with_resources(
        &self,
        session: &str,
        input: &TaskInput,
        intent: &TaskIntent,
        priority: TaskPriority,
        resources: &TaskResources,
        capacity: usize,
    ) -> Result<TaskRecord, QueueError> {
        let input_json = serde_json::to_string(input)?;
        intent.validate()?;
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        resources.validate(&resource_policy_in(&tx)?)?;
        let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_tasks WHERE session=?1 AND state IN ('queued','running','waiting_approval','draining'))",[session],|r|r.get(0))?;
        if duplicate {
            return Err(QueueError::Busy);
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM runtime_tasks WHERE state IN ('queued','running','waiting_approval','draining')",
            [],
            |r| r.get(0),
        )?;
        if count >= capacity as i64 {
            return Err(QueueError::QueueFull);
        }
        let id = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO runtime_tasks(id,session,input_json,state,queued_at,intent,priority,declared_memory_bytes) VALUES (?1,?2,?3,'queued',?4,?5,?6,?7)",params![id,session,input_json,sql_u64(timestamp_ms())?,serde_json::to_string(intent)?,priority_sql(priority),counter_sql(resources.declared_memory_bytes)?])?;
        let task = tx.query_row(
            &format!("SELECT {COLUMNS} FROM runtime_tasks WHERE id=?1"),
            [id],
            row,
        )?;
        tx.commit()?;
        Ok(task)
    }
    /// Read-only child precheck for the host: live capacity, held execution
    /// slots, and any ready queued work are checked from one consistent read.
    pub fn check_child_capacity(
        &self,
        capacity: usize,
        concurrency: usize,
    ) -> Result<(), QueueError> {
        let db = lock(&self.connection);
        check_child_capacity_in(&db, capacity, concurrency)
    }
    /// Child work starts as running in one IMMEDIATE transaction. Capacity and
    /// concurrency are checked without waiting, so the caller can return to its
    /// parent instead of occupying a queued slot. It never bypasses ready work.
    pub fn try_start_child(
        &self,
        session: &str,
        text: &str,
        intent: &TaskIntent,
        capacity: usize,
        concurrency: usize,
    ) -> Result<TaskLease, QueueError> {
        let input = TaskInput {
            text: text.into(),
            images: vec![],
        };
        let input_json = serde_json::to_string(&input)?;
        intent.validate()?;
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_tasks WHERE session=?1 AND state IN ('queued','running','waiting_approval','draining'))",[session],|r|r.get(0))?;
        if duplicate {
            return Err(QueueError::Busy);
        }
        check_child_capacity_in(&tx, capacity, concurrency)?;
        // Children currently make the canonical zero declaration. Still verify
        // the shared ledger before reserving a slot; no child path bypasses it.
        resource_usage_in(&tx)?;
        let id = Uuid::new_v4().to_string();
        let worker = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO runtime_tasks(id,session,input_json,state,queued_at,worker,fence,intent,priority,holds_slot,dispatches) VALUES (?1,?2,?3,'running',?4,?5,1,?6,'normal',1,1)",params![id,session,input_json,sql_u64(timestamp_ms())?,worker,serde_json::to_string(intent)?])?;
        take_dispatch_count(&tx)?;
        tx.commit()?;
        Ok(TaskLease {
            id,
            worker,
            fence: 1,
        })
    }
    pub fn get(&self, id: &str) -> Result<TaskRecord, QueueError> {
        lock(&self.connection)
            .query_row(
                &format!("SELECT {COLUMNS} FROM runtime_tasks WHERE id=?1"),
                [id],
                row,
            )
            .optional()?
            .ok_or_else(|| QueueError::InvalidInput("task not found".into()))
    }
    pub fn intent(&self, id: &str) -> Result<TaskIntent, QueueError> {
        Ok(self.get(id)?.intent)
    }
    pub fn list(&self, session: &str) -> Result<Vec<TaskRecord>, QueueError> {
        let db = lock(&self.connection);
        let mut stmt = db.prepare(&format!(
            "SELECT {COLUMNS} FROM runtime_tasks WHERE session=?1 ORDER BY ticket"
        ))?;
        Ok(stmt
            .query_map([session], row)?
            .collect::<Result<Vec<_>, _>>()?)
    }
    /// Explicitly fence a queued worker. Claiming an already claimed queued
    /// entry replaces its worker only while no attempt has started; the ready
    /// order and ready round survive so takeover cannot improve its position.
    pub fn claim(&self, id: &str) -> Result<TaskLease, QueueError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: Option<(Option<String>, bool)> = tx
            .query_row(
                "SELECT attempt,cancel_requested FROM runtime_tasks WHERE id=?1 AND state='queued'",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (attempt, cancelled) = record.ok_or_else(|| {
            QueueError::InvalidInput("only an uncancelled queued task may be resumed".into())
        })?;
        if cancelled || attempt.is_some() {
            return Err(QueueError::InvalidInput(
                "only an uncancelled queued task without an attempt may be resumed".into(),
            ));
        }
        let ready: (Option<i64>, Option<i64>) = tx.query_row(
            "SELECT ready_since_dispatch,ready_order FROM runtime_tasks WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let (ready_since, ready_order) = match ready {
            (Some(since), Some(order)) => (
                checked_queue_u64(since, "ready_since_dispatch")?,
                checked_queue_u64(order, "ready_order")?,
            ),
            _ => (dispatch_count_in(&tx)?, take_ready_order(&tx)?),
        };
        let worker = Uuid::new_v4().to_string();
        let n = tx.execute("UPDATE runtime_tasks SET worker=?2,fence=fence+1,ready_since_dispatch=?3,ready_order=?4 WHERE id=?1 AND state='queued' AND cancel_requested=0 AND attempt IS NULL AND fence<9223372036854775807",params![id,worker,counter_sql(ready_since)?,counter_sql(ready_order)?])?;
        if n != 1 {
            return Err(QueueError::InvalidInput(
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
    pub fn check(&self, lease: &TaskLease) -> Result<(), QueueError> {
        let valid: Option<bool> = lock(&self.connection).query_row("SELECT cancel_requested FROM runtime_tasks WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running','waiting_approval')",params![lease.id,lease.worker,sql_u64(lease.fence)?],|r|r.get(0)).optional()?;
        match valid {
            Some(false) => Ok(()),
            Some(true) => Err(QueueError::Cancelled),
            None => Err(QueueError::Worker("task worker was fenced".into())),
        }
    }
    /// Model admission/settlement reads this under lifecycle coordination, the
    /// same fence used by peer cancellation. Cancellation may still be recorded
    /// while cleanup is draining; never infer it from a local token alone.
    pub fn model_cancelled(
        &self,
        task: &TaskLease,
        session_id: &str,
        attempt_id: &str,
    ) -> Result<bool, QueueError> {
        let cancelled = lock(&self.connection)
            .query_row(
                "SELECT cancel_requested FROM runtime_tasks WHERE id=?1 AND session=?2 AND worker=?3 AND attempt=?4 AND fence=?5 AND state IN ('running','draining','waiting_approval')",
                params![task.id, session_id, task.worker, attempt_id, sql_u64(task.fence)?],
                |row| row.get(0),
            )
            .optional()?;
        cancelled.ok_or_else(|| QueueError::Worker("model request task identity was fenced".into()))
    }
    /// Park an admitted task while the host approves its pending request. The
    /// accepted capacity stays live, but the execution slot is released so the
    /// parked task cannot block ready work.
    pub fn park(&self, lease: &TaskLease) -> Result<(), QueueError> {
        let n = lock(&self.connection).execute(
            "UPDATE runtime_tasks SET state='waiting_approval',holds_slot=0,ready_order=NULL,ready_since_dispatch=NULL WHERE id=?1 AND worker=?2 AND fence=?3 AND state='running' AND holds_slot=1 AND cancel_requested=0",
            params![lease.id, lease.worker, sql_u64(lease.fence)?],
        )?;
        if n != 1 {
            return Err(QueueError::Worker(
                "task can only be parked while holding its admitted slot".into(),
            ));
        }
        Ok(())
    }
    /// Resume a parked task as ready for a new fairness round. Calling it again
    /// with the same queued/running lease is idempotent and keeps its position.
    pub fn ready(&self, lease: &TaskLease) -> Result<(), QueueError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: Option<(String, bool)> = tx
            .query_row(
                "SELECT state,cancel_requested FROM runtime_tasks WHERE id=?1 AND worker=?2 AND fence=?3",
                params![lease.id, lease.worker, sql_u64(lease.fence)?],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (task_state, cancelled) =
            record.ok_or_else(|| QueueError::Worker("task worker was fenced".into()))?;
        if cancelled {
            return Err(QueueError::Cancelled);
        }
        match state(&task_state)? {
            TaskState::Queued | TaskState::Running => {
                tx.commit()?;
                return Ok(());
            }
            TaskState::WaitingApproval => {}
            _ => {
                return Err(QueueError::InvalidInput(
                    "only an approved waiting task can become ready".into(),
                ));
            }
        }
        let since = dispatch_count_in(&tx)?;
        let order = take_ready_order(&tx)?;
        let n = tx.execute(
            "UPDATE runtime_tasks SET state='queued',ready_since_dispatch=?4,ready_order=?5 WHERE id=?1 AND worker=?2 AND fence=?3 AND state='waiting_approval' AND cancel_requested=0",
            params![
                lease.id,
                lease.worker,
                sql_u64(lease.fence)?,
                counter_sql(since)?,
                counter_sql(order)?
            ],
        )?;
        if n != 1 {
            return Err(QueueError::Worker(
                "task approval changed before it became ready".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }
    pub fn admit(&self, lease: &TaskLease, concurrency: usize) -> Result<bool, QueueError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: Option<(String, bool, bool, u64, u64)> = tx
            .query_row(
                "SELECT state,cancel_requested,holds_slot,dispatches,declared_memory_bytes FROM runtime_tasks WHERE id=?1 AND worker=?2 AND fence=?3",
                params![lease.id, lease.worker, sql_u64(lease.fence)?],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, read_u64(r, 3)?, read_u64(r, 4)?)),
            )
            .optional()?;
        let (task_state, cancelled, holds_slot, dispatches, declared_memory_bytes) =
            record.ok_or_else(|| QueueError::Worker("task worker was fenced".into()))?;
        if cancelled {
            return Err(QueueError::Cancelled);
        }
        let task_state = state(&task_state)?;
        let usage = resource_usage_in(&tx)?;
        TaskResources {
            declared_memory_bytes,
            ..Default::default()
        }
        .validate(&ResourcePolicy {
            max_declared_memory_bytes: usage.max_declared_memory_bytes,
        })?;
        // The same admitted identity may retry admission after a lost reply.
        if task_state == TaskState::Running && holds_slot {
            tx.commit()?;
            return Ok(true);
        }
        if task_state != TaskState::Queued {
            return Err(QueueError::InvalidInput(
                "task is not ready for admission".into(),
            ));
        }
        if usage.holding_tasks >= concurrency {
            tx.commit()?;
            return Ok(false);
        }
        let dispatch = dispatch_count_in(&tx)?;
        let fairness_after = fairness_after_in(&tx)?;
        let mut candidates = ready_candidates(&tx)?;
        order_ready(&mut candidates, dispatch, fairness_after);
        // Pick the same priority/fairness head first. A smaller declaration may
        // not steal capacity while that selected head is waiting for resources.
        let admitted = candidates.first().map(|candidate| candidate.id.as_str())
            == Some(lease.id.as_str())
            && declared_memory_bytes
                <= usage.max_declared_memory_bytes - usage.reserved_declared_memory_bytes;
        if admitted {
            let next = dispatches
                .checked_add(1)
                .ok_or_else(|| QueueError::InvalidInput("task dispatch overflow".into()))?;
            let n = tx.execute(
                "UPDATE runtime_tasks SET state='running',holds_slot=1,dispatches=?4 WHERE id=?1 AND worker=?2 AND fence=?3 AND state='queued' AND cancel_requested=0",
                params![lease.id, lease.worker, sql_u64(lease.fence)?, counter_sql(next)?],
            )?;
            if n != 1 {
                return Err(QueueError::Worker("task changed before admission".into()));
            }
            take_dispatch_count(&tx)?;
        }
        tx.commit()?;
        Ok(admitted)
    }
    pub fn attach(&self, task: &TaskLease, attempt: &str) -> Result<(), QueueError> {
        let n = lock(&self.connection).execute("UPDATE runtime_tasks SET attempt=?4 WHERE id=?1 AND worker=?2 AND fence=?3 AND state='running' AND cancel_requested=0",params![task.id,task.worker,sql_u64(task.fence)?,attempt])?;
        if n != 1 {
            return Err(QueueError::Worker(
                "task was fenced before attempt attachment".into(),
            ));
        }
        Ok(())
    }
    /// Serialize cancel versus finish in the same IMMEDIATE transaction that
    /// commits the task and audit. The caller holds lifecycle coordination while
    /// ending Requests from this decision. Failed bookkeeping retains admission.
    /// The host settlement runs inside the same transaction, before commit, and
    /// its own error type propagates unchanged.
    pub fn finish<E>(
        &self,
        lease: &TaskLease,
        proposed: TaskTerminal,
        error: Option<String>,
        settle: impl FnOnce(TaskTerminal) -> Result<(), E>,
    ) -> Result<(), E>
    where
        E: From<QueueError>,
    {
        let mut db = lock(&self.connection);
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(QueueError::from)?;
        let record: Option<(String, bool)> = tx
            .query_row(
                "SELECT session,cancel_requested FROM runtime_tasks WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running','waiting_approval')",
                params![lease.id, lease.worker, sql_u64(lease.fence)?],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(QueueError::from)?;
        let (session, cancelled) = record
            .ok_or_else(|| QueueError::Worker("task terminal rejected after fencing".into()))?;
        let terminal = match proposed {
            TaskTerminal::OutcomeUnknown => TaskTerminal::OutcomeUnknown,
            _ if cancelled => TaskTerminal::Cancelled,
            other => other,
        };
        let (state, kind) = terminal.records();
        let data = serde_json::json!({"worker_id":lease.worker,"error":error});
        let payload = serde_json::to_string(&data).map_err(QueueError::from)?;
        tx.execute(
            "INSERT INTO runtime_audit(session,task,kind,timestamp,payload) VALUES (?1,?2,?3,?4,?5)",
            params![session, lease.id, kind, sql_u64(timestamp_ms())?, payload],
        )
        .map_err(QueueError::from)?;
        tx.execute(
            "UPDATE runtime_tasks SET state=?4,error=?5,holds_slot=0 WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running','waiting_approval')",
            params![lease.id, lease.worker, sql_u64(lease.fence)?, state, error],
        )
        .map_err(QueueError::from)?;
        settle(terminal)?;
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "queue.task_terminal.commit.before",
            serde_json::json!({"session_id":session,"task_id":lease.id,"worker_id":lease.worker,"fence":lease.fence,"state":state}),
        );
        tx.commit().map_err(QueueError::from)?;
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "queue.task_terminal.commit.committed",
            serde_json::json!({"session_id":session,"task_id":lease.id,"worker_id":lease.worker,"fence":lease.fence,"state":state}),
        );
        Ok(())
    }
    pub fn block(&self, lease: &TaskLease, error: &str) -> Result<(), QueueError> {
        let n = lock(&self.connection).execute(
            "UPDATE runtime_tasks SET state='draining',error=?4 WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running','waiting_approval','draining')",
            params![lease.id, lease.worker, sql_u64(lease.fence)?, error],
        )?;
        if n != 1 {
            return Err(QueueError::Worker(
                "cleanup record rejected after fencing".into(),
            ));
        }
        Ok(())
    }
    pub fn cancel(&self, target: &CancelTarget) -> Result<bool, QueueError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = tx.query_row(
            &format!("SELECT {COLUMNS} FROM runtime_tasks WHERE id=?1"),
            [&target.task_id],
            row,
        )?;
        if task.cancel_target() != *target {
            return Err(QueueError::StaleCancellation);
        }
        if !matches!(
            task.state,
            TaskState::Queued
                | TaskState::Running
                | TaskState::WaitingApproval
                | TaskState::Draining
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
            "UPDATE runtime_tasks SET cancel_requested=1,state=CASE WHEN state='queued' AND worker IS NULL THEN 'cancelled' ELSE state END WHERE id=?1 AND session=?2 AND worker IS ?3 AND attempt IS ?4 AND fence=?5 AND state IN ('queued','running','waiting_approval','draining')",
            params![target.task_id, target.session_id, target.worker_id, target.attempt_id, sql_u64(target.fence)?],
        )?;
        tx.commit()?;
        Ok(changed != 0)
    }
    /// Fences first and releases slots only on explicit host recovery. Queued
    /// entries that never began an attempt keep their accepted capacity and
    /// readiness, so an externally stopped worker can be replaced. Everything
    /// that may have started an attempt becomes terminal.
    pub fn recover(&self, session: &str) -> Result<(), QueueError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let overflow: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_tasks WHERE session=?1 AND (state IN ('running','draining','waiting_approval') OR (state='queued' AND (attempt IS NOT NULL OR worker IS NOT NULL))) AND fence>=9223372036854775807)",
            [session],
            |r| r.get(0),
        )?;
        if overflow {
            // SQLite would promote fence+1 to REAL past the signed range.
            return Err(QueueError::InvalidInput(
                "queue fence counter overflow".into(),
            ));
        }
        // A claimed cancellation whose worker stopped has no one left to run
        // finish. External recovery already proved the owner stopped, so close
        // it and record the single cancellation terminal in the same commit.
        let cancelled: Vec<(String, String)> = {
            let mut statement = tx.prepare(
                "SELECT id,worker FROM runtime_tasks WHERE session=?1 AND state='queued' AND attempt IS NULL AND cancel_requested=1 AND worker IS NOT NULL",
            )?;
            statement
                .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (id, worker) in cancelled {
            let payload =
                serde_json::to_string(&serde_json::json!({"worker_id": worker, "error": null}))?;
            tx.execute(
                "INSERT INTO runtime_audit(session,task,kind,timestamp,payload) VALUES (?1,?2,'run_cancelled',?3,?4)",
                params![session, id, sql_u64(timestamp_ms())?, payload],
            )?;
            tx.execute(
                "UPDATE runtime_tasks SET state='cancelled',fence=fence+1,holds_slot=0 WHERE id=?1 AND state='queued' AND attempt IS NULL AND cancel_requested=1",
                [&id],
            )?;
        }
        // The owner was externally proven stopped. Keep the accepted task and
        // its input/priority/ready position, but detach the dead claim so it
        // cannot starve unrelated ready work; a later claim takes over.
        tx.execute(
            "UPDATE runtime_tasks SET worker=NULL,fence=fence+1,holds_slot=0 WHERE session=?1 AND state='queued' AND attempt IS NULL AND cancel_requested=0 AND worker IS NOT NULL",
            [session],
        )?;
        tx.execute("UPDATE runtime_tasks SET state='interrupted',cancel_requested=1,fence=fence+1,holds_slot=0,error='explicit worker recovery; inspect Core outcomes' WHERE session=?1 AND (state IN ('running','draining','waiting_approval') OR (state='queued' AND attempt IS NOT NULL))",[session])?;
        tx.commit()?;
        Ok(())
    }
    pub fn resource_usage(&self) -> Result<ResourceUsage, QueueError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction()?;
        let usage = resource_usage_in(&tx)?;
        tx.commit()?;
        Ok(usage)
    }
    pub fn counts(&self) -> Result<(usize, usize), QueueError> {
        Ok(lock(&self.connection).query_row("SELECT count(*) FILTER(WHERE holds_slot=1),count(*) FILTER(WHERE state='queued') FROM runtime_tasks",[],|r|Ok((r.get::<_,i64>(0)? as usize,r.get::<_,i64>(1)? as usize)))?)
    }
    /// Bounded live-task page for host fairness statistics. `after` is the
    /// last returned task id; its ticket is retained even after a terminal.
    pub fn snapshot_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TaskRecord>, QueueError> {
        if !(1..=1000).contains(&limit) {
            return Err(QueueError::InvalidInput(
                "queue snapshot limit must be between 1 and 1000".into(),
            ));
        }
        let db = lock(&self.connection);
        let after_ticket: i64 = match after {
            Some(id) => db
                .query_row("SELECT ticket FROM runtime_tasks WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()?
                .ok_or_else(|| {
                    QueueError::InvalidInput("queue snapshot cursor was not found".into())
                })?,
            None => 0,
        };
        let mut statement = db.prepare(&format!(
            "SELECT {COLUMNS} FROM runtime_tasks WHERE state IN ('queued','running','waiting_approval','draining') AND ticket>?1 ORDER BY ticket LIMIT ?2"
        ))?;
        Ok(statement
            .query_map(params![after_ticket, limit as i64], row)?
            .collect::<Result<Vec<_>, _>>()?)
    }
    #[cfg(test)]
    pub(crate) fn snapshot(&self, limit: usize) -> Result<Vec<TaskRecord>, QueueError> {
        self.snapshot_page(None, limit)
    }
    pub fn dispatch_count(&self) -> Result<u64, QueueError> {
        Ok(lock(&self.connection).query_row(
            "SELECT dispatch_count FROM runtime_queue_config WHERE singleton=1",
            [],
            |row| read_u64(row, 0),
        )?)
    }
    /// Test-only access used by the Runtime integration test that injects a
    /// deferred foreign-key failure at COMMIT. It is not part of the normal
    /// queue contract and is compiled only with `test-support`.
    #[cfg(feature = "test-support")]
    pub fn connection_for_test(&self) -> MutexGuard<'_, Connection> {
        lock(&self.connection)
    }
}

#[cfg(test)]
mod resources_tests;
#[cfg(test)]
mod tests;
