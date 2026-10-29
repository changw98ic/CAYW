//! Durable task identity, priority/fairness ordering, and admission. Opening a
//! database never claims work.
use crate::{
    RuntimeError,
    audit::{read_u64, sql_u64},
    coordination,
    input::TaskInput,
    lock,
    resources::{ResourcePolicy, ResourceUsage, TaskResources},
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
fn checked_queue_u64(value: i64, field: &str) -> Result<u64, RuntimeError> {
    u64::try_from(value)
        .map_err(|_| RuntimeError::InvalidInput(format!("corrupt queue {field} value")))
}
const COLUMNS: &str = "id,session,input_json,state,queued_at,worker,attempt,fence,cancel_requested,error,intent,priority,holds_slot,ready_order,ready_since_dispatch,dispatches,declared_memory_bytes";

fn resource_policy_in(db: &Connection) -> Result<ResourcePolicy, RuntimeError> {
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
fn resource_usage_in(db: &Connection) -> Result<ResourceUsage, RuntimeError> {
    let policy = resource_policy_in(db)?;
    let mut statement =
        db.prepare("SELECT declared_memory_bytes FROM runtime_tasks WHERE holds_slot=1")?;
    let mut rows = statement.query([])?;
    let mut reserved = 0u64;
    let mut holding_tasks = 0usize;
    while let Some(row) = rows.next()? {
        reserved = reserved.checked_add(read_u64(row, 0)?).ok_or_else(|| {
            RuntimeError::InvalidInput("resource declared memory accounting overflow".into())
        })?;
        holding_tasks = holding_tasks.checked_add(1).ok_or_else(|| {
            RuntimeError::InvalidInput("resource holding task count overflow".into())
        })?;
        if reserved > policy.max_declared_memory_bytes {
            return Err(RuntimeError::InvalidInput(
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
fn counter_sql(value: u64) -> Result<i64, RuntimeError> {
    i64::try_from(value).map_err(|_| RuntimeError::InvalidInput("queue counter overflow".into()))
}
fn dispatch_count_in(tx: &rusqlite::Transaction<'_>) -> Result<u64, RuntimeError> {
    Ok(tx.query_row(
        "SELECT dispatch_count FROM runtime_queue_config WHERE singleton=1",
        [],
        |row| read_u64(row, 0),
    )?)
}
fn fairness_after_in(tx: &rusqlite::Transaction<'_>) -> Result<u64, RuntimeError> {
    Ok(tx.query_row(
        "SELECT fairness_after FROM runtime_queue_config WHERE singleton=1",
        [],
        |row| read_u64(row, 0),
    )?)
}
/// Count one dispatch in the same transaction that admits the task.
fn take_dispatch_count(tx: &rusqlite::Transaction<'_>) -> Result<u64, RuntimeError> {
    let current = dispatch_count_in(tx)?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| RuntimeError::InvalidInput("queue dispatch counter overflow".into()))?;
    tx.execute(
        "UPDATE runtime_queue_config SET dispatch_count=?1 WHERE singleton=1",
        [counter_sql(next)?],
    )?;
    Ok(next)
}
fn take_ready_order(tx: &rusqlite::Transaction<'_>) -> Result<u64, RuntimeError> {
    let current: u64 = tx.query_row(
        "SELECT next_ready_order FROM runtime_queue_config WHERE singleton=1",
        [],
        |row| read_u64(row, 0),
    )?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| RuntimeError::InvalidInput("queue ready order overflow".into()))?;
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
fn ready_candidates(tx: &rusqlite::Transaction<'_>) -> Result<Vec<ReadyCandidate>, RuntimeError> {
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
) -> Result<(), RuntimeError> {
    let (live, active, ready): (i64, i64, bool) = db.query_row(
        "SELECT count(*) FILTER(WHERE state IN ('queued','running','waiting_approval','draining')),
                count(*) FILTER(WHERE holds_slot=1),
                EXISTS(SELECT 1 FROM runtime_tasks WHERE state='queued' AND worker IS NOT NULL AND cancel_requested=0)
         FROM runtime_tasks",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if ready {
        return Err(RuntimeError::Busy);
    }
    if live >= capacity as i64 || active >= concurrency as i64 {
        return Err(RuntimeError::QueueFull);
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
/// replace it only through `amend_policy`, which proves that every other
/// Runtime connection has stopped and that no task is still live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DurablePolicy {
    concurrency: u64,
    capacity: u64,
    fairness_after: u64,
    max_declared_memory_bytes: u64,
}

fn query_durable_policy(db: &Connection) -> rusqlite::Result<Option<DurablePolicy>> {
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
            Ok(DurablePolicy {
                concurrency: read_u64(row, 0)?,
                capacity: read_u64(row, 1)?,
                fairness_after: read_u64(row, 2)?,
                max_declared_memory_bytes: read_u64(row, 3)?,
            })
        },
    )
    .optional()
}

fn read_durable_policy(path: &Path) -> Result<Option<DurablePolicy>, RuntimeError> {
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
    Ok(query_durable_policy(&db)?)
}

/// Re-reads under the exclusive policy lock. Another opener that already wrote
/// the requested policy wins; otherwise the change requires a settled database.
fn adopt_policy(path: &Path, requested: DurablePolicy) -> Result<Option<()>, RuntimeError> {
    let mut db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if query_durable_policy(&tx)? == Some(requested) {
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
        return Err(RuntimeError::InvalidInput(
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
    ) -> Result<Self, RuntimeError> {
        Self::open_with_resources(
            path,
            concurrency,
            capacity,
            fairness_after,
            &ResourcePolicy::default(),
        )
    }
    pub(crate) fn open_with_resources(
        path: &Path,
        concurrency: usize,
        capacity: usize,
        fairness_after: usize,
        resources: &ResourcePolicy,
    ) -> Result<Self, RuntimeError> {
        resources.validate()?;
        let concurrency = concurrency as u64;
        let capacity = capacity as u64;
        let fairness_after = fairness_after as u64;
        let requested = DurablePolicy {
            concurrency,
            capacity,
            fairness_after,
            max_declared_memory_bytes: resources.max_declared_memory_bytes,
        };
        // A database that already recorded one complete policy adopts a
        // different explicit host policy only after every other Runtime has
        // stopped and no task is live. The exclusive policy lock prevents an
        // old in-memory connection from admitting work against a policy that
        // was already replaced.
        if read_durable_policy(path)?.is_some_and(|stored| stored != requested) {
            coordination::amend_policy(path, || adopt_policy(path, requested))?;
        }
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
            return Err(RuntimeError::InvalidInput(
                "all runtime connections must use the database's queue limits".into(),
            ));
        }
        if resource_policy_in(&tx)? != *resources {
            return Err(RuntimeError::InvalidInput(
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
                .ok_or_else(|| RuntimeError::InvalidInput("queue ready order overflow".into()))?;
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
    pub(crate) fn check_policy(
        &self,
        concurrency: usize,
        capacity: usize,
        fairness_after: usize,
        resources: &ResourcePolicy,
    ) -> Result<(), RuntimeError> {
        let requested = DurablePolicy {
            concurrency: concurrency as u64,
            capacity: capacity as u64,
            fairness_after: fairness_after as u64,
            max_declared_memory_bytes: resources.max_declared_memory_bytes,
        };
        if query_durable_policy(&lock(&self.connection))? != Some(requested) {
            return Err(RuntimeError::InvalidInput(
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
    ) -> Result<TaskRecord, RuntimeError> {
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
    ) -> Result<TaskRecord, RuntimeError> {
        self.enqueue_with_resources(
            session,
            input,
            intent,
            priority,
            &TaskResources::default(),
            capacity,
        )
    }
    pub(crate) fn enqueue_with_resources(
        &self,
        session: &str,
        input: &TaskInput,
        intent: &TaskIntent,
        priority: TaskPriority,
        resources: &TaskResources,
        capacity: usize,
    ) -> Result<TaskRecord, RuntimeError> {
        let input_json = serde_json::to_string(input)?;
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        resources.validate(&resource_policy_in(&tx)?)?;
        let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_tasks WHERE session=?1 AND state IN ('queued','running','waiting_approval','draining'))",[session],|r|r.get(0))?;
        if duplicate {
            return Err(RuntimeError::Busy);
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM runtime_tasks WHERE state IN ('queued','running','waiting_approval','draining')",
            [],
            |r| r.get(0),
        )?;
        if count >= capacity as i64 {
            return Err(RuntimeError::QueueFull);
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
    pub(crate) fn check_child_capacity(
        &self,
        capacity: usize,
        concurrency: usize,
    ) -> Result<(), RuntimeError> {
        let db = lock(&self.connection);
        check_child_capacity_in(&db, capacity, concurrency)
    }
    /// Child work starts as running in one IMMEDIATE transaction. Capacity and
    /// concurrency are checked without waiting, so the caller can return to its
    /// parent instead of occupying a queued slot. It never bypasses ready work.
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
        let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_tasks WHERE session=?1 AND state IN ('queued','running','waiting_approval','draining'))",[session],|r|r.get(0))?;
        if duplicate {
            return Err(RuntimeError::Busy);
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
    /// Explicitly fence a queued worker. Claiming an already claimed queued
    /// entry replaces its worker only while no attempt has started; the ready
    /// order and ready round survive so takeover cannot improve its position.
    pub(crate) fn claim(&self, id: &str) -> Result<TaskLease, RuntimeError> {
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
            RuntimeError::InvalidInput("only an uncancelled queued task may be resumed".into())
        })?;
        if cancelled || attempt.is_some() {
            return Err(RuntimeError::InvalidInput(
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
        let valid: Option<bool> = lock(&self.connection).query_row("SELECT cancel_requested FROM runtime_tasks WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running','waiting_approval')",params![lease.id,lease.worker,sql_u64(lease.fence)?],|r|r.get(0)).optional()?;
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
                "SELECT cancel_requested FROM runtime_tasks WHERE id=?1 AND session=?2 AND worker=?3 AND attempt=?4 AND fence=?5 AND state IN ('running','draining','waiting_approval')",
                params![task.id, attempt.session_id, task.worker, attempt.attempt_id, sql_u64(task.fence)?],
                |row| row.get(0),
            )
            .optional()?;
        cancelled
            .ok_or_else(|| RuntimeError::Worker("model request task identity was fenced".into()))
    }
    /// Park an admitted task while the host approves its pending request. The
    /// accepted capacity stays live, but the execution slot is released so the
    /// parked task cannot block ready work.
    pub(crate) fn park(&self, lease: &TaskLease) -> Result<(), RuntimeError> {
        let n = lock(&self.connection).execute(
            "UPDATE runtime_tasks SET state='waiting_approval',holds_slot=0,ready_order=NULL,ready_since_dispatch=NULL WHERE id=?1 AND worker=?2 AND fence=?3 AND state='running' AND holds_slot=1 AND cancel_requested=0",
            params![lease.id, lease.worker, sql_u64(lease.fence)?],
        )?;
        if n != 1 {
            return Err(RuntimeError::Worker(
                "task can only be parked while holding its admitted slot".into(),
            ));
        }
        Ok(())
    }
    /// Resume a parked task as ready for a new fairness round. Calling it again
    /// with the same queued/running lease is idempotent and keeps its position.
    pub(crate) fn ready(&self, lease: &TaskLease) -> Result<(), RuntimeError> {
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
            record.ok_or_else(|| RuntimeError::Worker("task worker was fenced".into()))?;
        if cancelled {
            return Err(RuntimeError::Cancelled);
        }
        match state(&task_state)? {
            TaskState::Queued | TaskState::Running => {
                tx.commit()?;
                return Ok(());
            }
            TaskState::WaitingApproval => {}
            _ => {
                return Err(RuntimeError::InvalidInput(
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
            return Err(RuntimeError::Worker(
                "task approval changed before it became ready".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn admit(
        &self,
        lease: &TaskLease,
        concurrency: usize,
    ) -> Result<bool, RuntimeError> {
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
            record.ok_or_else(|| RuntimeError::Worker("task worker was fenced".into()))?;
        if cancelled {
            return Err(RuntimeError::Cancelled);
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
            return Err(RuntimeError::InvalidInput(
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
                .ok_or_else(|| RuntimeError::InvalidInput("task dispatch overflow".into()))?;
            let n = tx.execute(
                "UPDATE runtime_tasks SET state='running',holds_slot=1,dispatches=?4 WHERE id=?1 AND worker=?2 AND fence=?3 AND state='queued' AND cancel_requested=0",
                params![lease.id, lease.worker, sql_u64(lease.fence)?, counter_sql(next)?],
            )?;
            if n != 1 {
                return Err(RuntimeError::Worker("task changed before admission".into()));
            }
            take_dispatch_count(&tx)?;
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
            "SELECT session,cancel_requested FROM runtime_tasks WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running','waiting_approval')",
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
            "UPDATE runtime_tasks SET state=?4,error=?5,holds_slot=0 WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running','waiting_approval')",
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
            "UPDATE runtime_tasks SET state='draining',error=?4 WHERE id=?1 AND worker=?2 AND fence=?3 AND state IN ('queued','running','waiting_approval','draining')",
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
    pub(crate) fn recover(&self, session: &str) -> Result<(), RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let overflow: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_tasks WHERE session=?1 AND (state IN ('running','draining','waiting_approval') OR (state='queued' AND (attempt IS NOT NULL OR worker IS NOT NULL))) AND fence>=9223372036854775807)",
            [session],
            |r| r.get(0),
        )?;
        if overflow {
            // SQLite would promote fence+1 to REAL past the signed range.
            return Err(RuntimeError::InvalidInput(
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
    pub(crate) fn resource_usage(&self) -> Result<ResourceUsage, RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction()?;
        let usage = resource_usage_in(&tx)?;
        tx.commit()?;
        Ok(usage)
    }
    pub(crate) fn counts(&self) -> Result<(usize, usize), RuntimeError> {
        Ok(lock(&self.connection).query_row("SELECT count(*) FILTER(WHERE holds_slot=1),count(*) FILTER(WHERE state='queued') FROM runtime_tasks",[],|r|Ok((r.get::<_,i64>(0)? as usize,r.get::<_,i64>(1)? as usize)))?)
    }
    /// Bounded live-task page for host fairness statistics. `after` is the
    /// last returned task id; its ticket is retained even after a terminal.
    pub(crate) fn snapshot_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TaskRecord>, RuntimeError> {
        if !(1..=1000).contains(&limit) {
            return Err(RuntimeError::InvalidInput(
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
                    RuntimeError::InvalidInput("queue snapshot cursor was not found".into())
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
    pub(crate) fn snapshot(&self, limit: usize) -> Result<Vec<TaskRecord>, RuntimeError> {
        self.snapshot_page(None, limit)
    }
    pub(crate) fn dispatch_count(&self) -> Result<u64, RuntimeError> {
        Ok(lock(&self.connection).query_row(
            "SELECT dispatch_count FROM runtime_queue_config WHERE singleton=1",
            [],
            |row| read_u64(row, 0),
        )?)
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
        let owner = Queue::open(&path, 1, 4, 0).unwrap();
        let peer = Queue::open(&path, 1, 4, 0).unwrap();
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
        let owner = Queue::open(&path, 1, 4, 0).unwrap();
        let peer = Queue::open(&path, 1, 4, 0).unwrap();
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
        let owner = Queue::open(&path, 1, 4, 0).unwrap();
        let peer = Queue::open(&path, 1, 4, 0).unwrap();
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
        let owner = Queue::open(&path, 1, 4, 0).unwrap();
        let peer = Queue::open(&path, 1, 4, 0).unwrap();
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
        let owner = Queue::open(&path, 1, 4, 0).unwrap();
        let peer = Queue::open(&path, 1, 4, 0).unwrap();
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
        let owner = Queue::open(&path, 1, 4, 0).unwrap();
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
        let owner = Queue::open(&path, 1, 4, 0).unwrap();
        let peer = Queue::open(&path, 1, 4, 0).unwrap();
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

        let queue = Queue::open(&path, 1, 4, 0).unwrap();
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
        let reopened = Queue::open(&path, 1, 4, 0).unwrap();
        assert_eq!(reopened.list("session").unwrap().len(), 1);
    }

    #[test]
    fn try_start_child_starts_running_without_waiting_and_enforces_limits() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = Queue::open(&path, 2, 2, 0).unwrap();

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

    fn task_input(text: &str) -> TaskInput {
        TaskInput {
            text: text.into(),
            images: vec![],
        }
    }
    fn open_queue(
        path: &Path,
        concurrency: usize,
        capacity: usize,
        fairness_after: usize,
    ) -> Queue {
        Queue::open(path, concurrency, capacity, fairness_after).unwrap()
    }
    fn enqueue(
        queue: &Queue,
        session: &str,
        priority: TaskPriority,
        capacity: usize,
    ) -> TaskRecord {
        queue
            .enqueue_scheduled(
                session,
                &task_input(session),
                &TaskIntent::General,
                priority,
                capacity,
            )
            .unwrap()
    }
    fn claim(queue: &Queue, task: &TaskRecord) -> TaskLease {
        queue.claim(&task.id).unwrap()
    }
    fn admit(queue: &Queue, lease: &TaskLease, concurrency: usize) -> bool {
        queue.admit(lease, concurrency).unwrap()
    }
    fn complete(queue: &Queue, lease: &TaskLease) {
        queue
            .finish(lease, TaskTerminal::Completed, None, |_| Ok(()))
            .unwrap();
    }

    #[test]
    fn priority_serializes_snake_case_and_defaults_to_normal() {
        assert_eq!(TaskPriority::default(), TaskPriority::Normal);
        assert!(TaskPriority::Low < TaskPriority::Normal);
        assert!(TaskPriority::Normal < TaskPriority::High);
        assert_eq!(
            serde_json::to_string(&TaskPriority::Low).unwrap(),
            "\"low\""
        );
        assert_eq!(
            serde_json::to_string(&TaskPriority::Normal).unwrap(),
            "\"normal\""
        );
        assert_eq!(
            serde_json::to_string(&TaskPriority::High).unwrap(),
            "\"high\""
        );
    }

    #[test]
    fn priority_order_prefers_high_then_normal_then_fifo() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let queue = open_queue(&path, 4, 6, 100);
        let low = enqueue(&queue, "low", TaskPriority::Low, 6);
        let first = enqueue(&queue, "first", TaskPriority::Normal, 6);
        let second = enqueue(&queue, "second", TaskPriority::Normal, 6);
        let high = enqueue(&queue, "high", TaskPriority::High, 6);
        let low = claim(&queue, &low);
        let first = claim(&queue, &first);
        let second = claim(&queue, &second);
        let high = claim(&queue, &high);

        assert!(!admit(&queue, &low, 4));
        assert!(!admit(&queue, &first, 4));
        assert!(!admit(&queue, &second, 4));
        assert!(admit(&queue, &high, 4));
        assert!(!admit(&queue, &low, 4));
        assert!(!admit(&queue, &second, 4));
        assert!(admit(&queue, &first, 4));
        assert!(!admit(&queue, &low, 4));
        assert!(admit(&queue, &second, 4));
        assert!(admit(&queue, &low, 4));
        assert_eq!(queue.dispatch_count().unwrap(), 4);
    }

    #[test]
    fn waiting_entry_is_promoted_after_fairness_window() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 1, 6, 2);
        let low = enqueue(&queue, "low", TaskPriority::Low, 6);
        let low = claim(&queue, &low);
        let ready_order = queue.get(&low.id).unwrap().ready_order;
        assert!(ready_order.is_some());

        for session in ["high-one", "high-two"] {
            let high = enqueue(&queue, session, TaskPriority::High, 6);
            let high = claim(&queue, &high);
            assert!(admit(&queue, &high, 1));
            complete(&queue, &high);
        }
        assert_eq!(queue.dispatch_count().unwrap(), 2);

        // New high-priority arrivals cannot overtake an entry that has
        // already waited the full fairness window.
        let late_three = enqueue(&queue, "high-three", TaskPriority::High, 6);
        let late_three = claim(&queue, &late_three);
        let late_four = enqueue(&queue, "high-four", TaskPriority::High, 6);
        let late_four = claim(&queue, &late_four);
        assert!(!admit(&queue, &late_three, 1));
        assert!(!admit(&queue, &late_four, 1));
        assert!(admit(&queue, &low, 1));
        let admitted = queue.get(&low.id).unwrap();
        assert!(admitted.holds_slot);
        assert_eq!(admitted.dispatches, 1);
        assert_eq!(admitted.ready_order, ready_order);
        assert_eq!(queue.dispatch_count().unwrap(), 3);
    }

    #[test]
    fn fairness_state_and_dispatch_count_survive_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 1, 6, 2);
        let low = enqueue(&queue, "low", TaskPriority::Low, 6);
        let low = claim(&queue, &low);
        let ready = queue.get(&low.id).unwrap();
        drop(queue);

        let queue = open_queue(&path, 1, 6, 2);
        let reopened = queue.get(&low.id).unwrap();
        assert_eq!(reopened.ready_order, ready.ready_order);
        assert_eq!(reopened.ready_since_dispatch, ready.ready_since_dispatch);
        assert_eq!(queue.dispatch_count().unwrap(), 0);
        for session in ["high-one", "high-two"] {
            let high = enqueue(&queue, session, TaskPriority::High, 6);
            let high = claim(&queue, &high);
            assert!(admit(&queue, &high, 1));
            complete(&queue, &high);
        }
        assert_eq!(queue.dispatch_count().unwrap(), 2);
        let late = enqueue(&queue, "high-three", TaskPriority::High, 6);
        let late = claim(&queue, &late);
        assert!(!admit(&queue, &late, 1));
        assert!(admit(&queue, &low, 1));
    }

    #[test]
    fn reclaim_preserves_ready_order_and_fences_the_old_worker() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let queue = open_queue(&path, 2, 4, 100);
        let task = enqueue(&queue, "reclaim", TaskPriority::Normal, 4);
        let first = claim(&queue, &task);
        let before = queue.get(&task.id).unwrap();
        assert!(before.ready_order.is_some());
        let second = claim(&queue, &task);
        let after = queue.get(&task.id).unwrap();
        assert_eq!(after.ready_order, before.ready_order);
        assert_eq!(after.ready_since_dispatch, before.ready_since_dispatch);
        assert_eq!(after.fence, first.fence + 1);
        assert_eq!(after.worker_id.as_deref(), Some(second.worker.as_str()));
        assert!(queue.admit(&first, 2).is_err());
        assert!(admit(&queue, &second, 2));
        assert!(queue.claim(&task.id).is_err());
    }

    #[test]
    fn child_start_cannot_bypass_ready_work_and_counts_as_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 2, 4, 100);
        assert_eq!(queue.dispatch_count().unwrap(), 0);
        let child = queue
            .try_start_child("child-a", "child", &TaskIntent::General, 4, 2)
            .unwrap();
        assert_eq!(queue.dispatch_count().unwrap(), 1);
        let record = queue.get(&child.id).unwrap();
        assert!(record.holds_slot);
        assert_eq!(record.dispatches, 1);
        complete(&queue, &child);

        let queued = enqueue(&queue, "waiting", TaskPriority::Normal, 4);
        let _lease = claim(&queue, &queued);
        assert!(matches!(
            queue.try_start_child("child-b", "blocked", &TaskIntent::General, 4, 2),
            Err(RuntimeError::Busy)
        ));
        assert_eq!(queue.dispatch_count().unwrap(), 1);
    }

    #[test]
    fn park_releases_slot_but_keeps_accepted_capacity_and_readies_again() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let queue = open_queue(&path, 1, 1, 2);
        let task = enqueue(&queue, "approval", TaskPriority::Normal, 1);
        let lease = claim(&queue, &task);
        assert!(admit(&queue, &lease, 1));
        assert_eq!(queue.counts().unwrap(), (1, 0));
        queue.park(&lease).unwrap();
        let parked = queue.get(&task.id).unwrap();
        assert_eq!(parked.state, TaskState::WaitingApproval);
        assert!(!parked.holds_slot);
        assert!(parked.ready_order.is_none());
        assert!(parked.ready_since_dispatch.is_none());
        assert_eq!(queue.counts().unwrap(), (0, 0));
        // Waiting approval keeps the accepted capacity but is not executable.
        assert!(matches!(
            queue.enqueue_scheduled(
                "other",
                &task_input("other"),
                &TaskIntent::General,
                TaskPriority::Normal,
                1
            ),
            Err(RuntimeError::QueueFull)
        ));
        assert!(queue.admit(&lease, 1).is_err());
        let wrong = TaskLease {
            worker: "wrong-worker".into(),
            ..lease.clone()
        };
        assert!(queue.park(&wrong).is_err());
        assert!(queue.ready(&wrong).is_err());
        queue.ready(&lease).unwrap();
        let reordered = queue.get(&task.id).unwrap();
        assert_eq!(reordered.state, TaskState::Queued);
        assert!(reordered.ready_order.is_some());
        assert_eq!(
            reordered.ready_since_dispatch,
            Some(queue.dispatch_count().unwrap())
        );
        queue.ready(&lease).unwrap();
        assert_eq!(
            queue.get(&task.id).unwrap().ready_order,
            reordered.ready_order
        );
        assert!(admit(&queue, &lease, 1));
        assert_eq!(queue.get(&task.id).unwrap().dispatches, 2);
        queue.park(&lease).unwrap();
    }

    #[test]
    fn cancelling_waiting_approval_blocks_readmission() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let queue = open_queue(&path, 1, 1, 2);
        let peer = open_queue(&path, 1, 1, 2);
        let task = enqueue(&queue, "approval", TaskPriority::Normal, 1);
        let lease = claim(&queue, &task);
        assert!(admit(&queue, &lease, 1));
        queue.park(&lease).unwrap();
        assert!(
            peer.cancel(&queue.get(&task.id).unwrap().cancel_target())
                .unwrap()
        );
        assert!(matches!(queue.ready(&lease), Err(RuntimeError::Cancelled)));
        let record = queue.get(&task.id).unwrap();
        assert_eq!(record.state, TaskState::WaitingApproval);
        assert!(record.cancel_requested);
        assert_eq!(queue.counts().unwrap(), (0, 0));
    }

    #[test]
    fn failed_storage_does_not_consume_dispatch_or_drop_task() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let queue = open_queue(&path, 1, 4, 2);
        let task = enqueue(&queue, "synthetic", TaskPriority::Normal, 4);
        let lease = claim(&queue, &task);
        lock(&queue.connection)
            .execute_batch(
                "CREATE TRIGGER fail_admit BEFORE UPDATE ON runtime_tasks
                 WHEN NEW.state='running'
                 BEGIN SELECT RAISE(ABORT,'synthetic admit failure'); END;",
            )
            .unwrap();
        assert!(queue.admit(&lease, 1).is_err());
        assert_eq!(queue.dispatch_count().unwrap(), 0);
        let after = queue.get(&task.id).unwrap();
        assert_eq!(after.state, TaskState::Queued);
        assert!(!after.holds_slot);
        assert_eq!(after.dispatches, 0);
        lock(&queue.connection)
            .execute_batch("DROP TRIGGER fail_admit")
            .unwrap();
        assert!(admit(&queue, &lease, 1));
        assert_eq!(queue.dispatch_count().unwrap(), 1);

        lock(&queue.connection)
            .execute_batch(
                "CREATE TRIGGER fail_enqueue BEFORE INSERT ON runtime_tasks
                 BEGIN SELECT RAISE(ABORT,'synthetic enqueue failure'); END;",
            )
            .unwrap();
        assert!(
            queue
                .enqueue_scheduled(
                    "other",
                    &task_input("other"),
                    &TaskIntent::General,
                    TaskPriority::Low,
                    4
                )
                .is_err()
        );
        lock(&queue.connection)
            .execute_batch("DROP TRIGGER fail_enqueue")
            .unwrap();
        assert!(queue.list("other").unwrap().is_empty());
        assert_eq!(queue.dispatch_count().unwrap(), 1);
    }

    #[test]
    fn counter_overflow_fails_closed_without_wrapping() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let queue = open_queue(&path, 1, 4, 2);
        let task = enqueue(&queue, "synthetic", TaskPriority::Normal, 4);
        let lease = claim(&queue, &task);
        lock(&queue.connection)
            .execute(
                "UPDATE runtime_queue_config SET dispatch_count=?1 WHERE singleton=1",
                [i64::MAX],
            )
            .unwrap();
        assert!(queue.admit(&lease, 1).is_err());
        assert_eq!(queue.dispatch_count().unwrap(), i64::MAX as u64);
        let after = queue.get(&task.id).unwrap();
        assert_eq!(after.state, TaskState::Queued);
        assert!(!after.holds_slot);
        assert_eq!(after.dispatches, 0);
    }

    #[test]
    fn recover_keeps_queued_without_attempt_and_interrupts_attempted_work() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let queue = open_queue(&path, 1, 4, 2);
        let ready = enqueue(&queue, "ready", TaskPriority::Normal, 4);
        let ready_lease = claim(&queue, &ready);
        let running = enqueue(&queue, "running", TaskPriority::High, 4);
        let running_lease = claim(&queue, &running);
        assert!(admit(&queue, &running_lease, 1));
        queue.attach(&running_lease, "attempt-running").unwrap();

        let old_target = queue.get(&ready.id).unwrap().cancel_target();
        queue.recover("ready").unwrap();
        let kept = queue.get(&ready.id).unwrap();
        assert_eq!(kept.state, TaskState::Queued);
        assert!(!kept.cancel_requested);
        assert!(kept.worker_id.is_none());
        assert_eq!(kept.fence, ready_lease.fence + 1);
        assert!(kept.ready_order.is_some());
        assert!(kept.ready_since_dispatch.is_some());
        assert!(queue.check(&ready_lease).is_err());
        assert!(queue.admit(&ready_lease, 1).is_err());
        assert!(
            queue
                .finish(&ready_lease, TaskTerminal::Completed, None, |_| panic!(
                    "old finish ran"
                ))
                .is_err()
        );
        assert!(matches!(
            queue.cancel(&old_target),
            Err(RuntimeError::StaleCancellation)
        ));
        // Repeating recovery does not advance the fence again.
        queue.recover("ready").unwrap();
        assert_eq!(queue.get(&ready.id).unwrap().fence, ready_lease.fence + 1);
        let retaken = claim(&queue, &ready);
        assert_ne!(retaken.worker, ready_lease.worker);
        assert_eq!(retaken.fence, ready_lease.fence + 2);
        let reclaimed = queue.get(&ready.id).unwrap();
        assert_eq!(reclaimed.ready_order, kept.ready_order);
        assert_eq!(reclaimed.ready_since_dispatch, kept.ready_since_dispatch);
        assert_eq!(
            reclaimed.worker_id.as_deref(),
            Some(retaken.worker.as_str())
        );

        queue.recover("running").unwrap();
        let interrupted = queue.get(&running.id).unwrap();
        assert_eq!(interrupted.state, TaskState::Interrupted);
        assert!(interrupted.cancel_requested);
        assert!(!interrupted.holds_slot);
        assert_eq!(interrupted.fence, running_lease.fence + 1);
        assert_eq!(queue.counts().unwrap(), (0, 1));
    }

    #[test]
    fn snapshot_is_bounded_to_live_tasks() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 1, 4, 2);
        assert!(matches!(
            queue.snapshot(0),
            Err(RuntimeError::InvalidInput(_))
        ));
        assert!(matches!(
            queue.snapshot(1001),
            Err(RuntimeError::InvalidInput(_))
        ));
        let first = enqueue(&queue, "first", TaskPriority::Normal, 4);
        let second = enqueue(&queue, "second", TaskPriority::High, 4);
        assert_eq!(queue.snapshot(1).unwrap().len(), 1);
        let live = queue.snapshot(10).unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(live[1].priority, TaskPriority::High);
        assert!(live[1].ready_order.is_none());
        let lease = claim(&queue, &first);
        assert!(queue.snapshot(10).unwrap()[0].ready_order.is_some());
        complete(&queue, &lease);
        assert_eq!(queue.snapshot(10).unwrap().len(), 1);
        assert_eq!(queue.snapshot(10).unwrap()[0].id, second.id);
    }

    #[test]
    fn legacy_schema_migrates_priority_slots_and_ready_order() {
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
                    session TEXT NOT NULL, input_json TEXT NOT NULL, state TEXT NOT NULL,
                    queued_at INTEGER NOT NULL, worker TEXT, attempt TEXT,
                    fence INTEGER NOT NULL DEFAULT 0,
                    cancel_requested INTEGER NOT NULL DEFAULT 0, error TEXT,
                    intent TEXT NOT NULL DEFAULT '{\"kind\":\"general\"}');
                 INSERT INTO runtime_queue_config VALUES (1,1,4);
                 INSERT INTO runtime_tasks(id,session,input_json,state,queued_at,worker,attempt,fence,intent) VALUES
                   ('run','run','{\"text\":\"run\",\"images\":[]}','running',1,'worker-run',NULL,1,'{\"kind\":\"general\"}'),
                   ('old','old','{\"text\":\"old\",\"images\":[]}','queued',2,'worker-old',NULL,1,'{\"kind\":\"general\"}'),
                   ('new','new','{\"text\":\"new\",\"images\":[]}','queued',3,'worker-new',NULL,1,'{\"kind\":\"general\"}'),
                   ('idle','idle','{\"text\":\"idle\",\"images\":[]}','queued',4,NULL,NULL,0,'{\"kind\":\"general\"}');",
            )
            .unwrap();
        drop(legacy);
        let _audit = AuditDb::open(&path).unwrap();

        let queue = open_queue(&path, 1, 4, 0);
        assert_eq!(queue.dispatch_count().unwrap(), 0);
        let running = queue.get("run").unwrap();
        assert_eq!(running.priority, TaskPriority::Normal);
        assert!(running.holds_slot);
        assert_eq!(running.dispatches, 1);
        let old = queue.get("old").unwrap();
        let new = queue.get("new").unwrap();
        assert!(old.ready_order.is_some());
        assert!(new.ready_order.is_some());
        assert!(old.ready_order < new.ready_order);
        assert_eq!(old.ready_since_dispatch, Some(0));
        let idle = queue.get("idle").unwrap();
        assert!(idle.ready_order.is_none());
        assert!(!idle.holds_slot);
        assert_eq!(idle.priority, TaskPriority::Normal);
        let reclaimed = claim(&queue, &idle);
        assert!(queue.get(&idle.id).unwrap().ready_order > new.ready_order);
        assert_eq!(queue.get(&idle.id).unwrap().ready_since_dispatch, Some(0));
        for (id, worker) in [
            ("run", "worker-run"),
            ("old", "worker-old"),
            ("new", "worker-new"),
        ] {
            queue
                .finish(
                    &TaskLease {
                        id: id.into(),
                        worker: worker.into(),
                        fence: 1,
                    },
                    TaskTerminal::Completed,
                    None,
                    |_| Ok(()),
                )
                .unwrap();
        }
        assert!(admit(&queue, &reclaimed, 1));
    }

    #[test]
    fn child_capacity_precheck_counts_live_and_slots_consistently() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 1, 3, 100);
        queue.check_child_capacity(3, 1).unwrap();

        let parked = enqueue(&queue, "parked", TaskPriority::Normal, 3);
        let parked_lease = claim(&queue, &parked);
        assert!(admit(&queue, &parked_lease, 1));
        queue.park(&parked_lease).unwrap();
        assert!(queue.check_child_capacity(3, 1).is_ok());
        assert!(matches!(
            queue.check_child_capacity(1, 1),
            Err(RuntimeError::QueueFull)
        ));

        let draining = enqueue(&queue, "draining", TaskPriority::Normal, 3);
        let draining_lease = claim(&queue, &draining);
        assert!(admit(&queue, &draining_lease, 1));
        queue.block(&draining_lease, "synthetic cleanup").unwrap();
        let record = queue.get(&draining.id).unwrap();
        assert_eq!(record.state, TaskState::Draining);
        assert!(record.holds_slot);
        assert!(matches!(
            queue.check_child_capacity(3, 1),
            Err(RuntimeError::QueueFull)
        ));
        assert!(queue.check_child_capacity(3, 2).is_ok());

        let ready = enqueue(&queue, "ready", TaskPriority::High, 3);
        let _ready_lease = claim(&queue, &ready);
        assert!(matches!(
            queue.check_child_capacity(3, 2),
            Err(RuntimeError::Busy)
        ));
    }

    #[test]
    fn legacy_config_adopts_explicit_fairness() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE runtime_queue_config (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    concurrency INTEGER NOT NULL, capacity INTEGER NOT NULL);
                 INSERT INTO runtime_queue_config VALUES (1,1,4);",
            )
            .unwrap();
        drop(legacy);

        // The runtime default is 8. An old database has no fairness column, so
        // it must adopt the explicit value instead of keeping a zero default.
        let queue = open_queue(&path, 1, 4, 8);
        assert_eq!(queue.dispatch_count().unwrap(), 0);
        drop(queue);
        // With the explicit policy already persisted and no live task, a
        // trusted opener may change the fairness window.
        let changed = open_queue(&path, 1, 4, 0);
        assert_eq!(changed.dispatch_count().unwrap(), 0);
        drop(changed);
        // A later opener requests the changed value and keeps it.
        open_queue(&path, 1, 4, 0);
    }

    #[test]
    fn negative_ready_fields_are_rejected_not_wrapped() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 1, 4, 2);
        let task = enqueue(&queue, "corrupt-order", TaskPriority::Normal, 4);
        lock(&queue.connection)
            .execute(
                "UPDATE runtime_tasks SET ready_since_dispatch=0,ready_order=-1 WHERE id=?1",
                [&task.id],
            )
            .unwrap();
        assert!(queue.get(&task.id).is_err());
        assert!(queue.claim(&task.id).is_err());
        assert!(queue.snapshot(10).is_err());

        let other = enqueue(&queue, "corrupt-since", TaskPriority::Normal, 4);
        lock(&queue.connection)
            .execute(
                "UPDATE runtime_tasks SET ready_since_dispatch=-2,ready_order=1 WHERE id=?1",
                [&other.id],
            )
            .unwrap();
        assert!(queue.claim(&other.id).is_err());
    }

    #[test]
    fn recover_fence_overflow_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 1, 4, 2);
        let task = enqueue(&queue, "max-fence", TaskPriority::Normal, 4);
        let lease = claim(&queue, &task);
        assert!(admit(&queue, &lease, 1));
        lock(&queue.connection)
            .execute(
                "UPDATE runtime_tasks SET fence=?1 WHERE id=?2",
                params![i64::MAX, &task.id],
            )
            .unwrap();
        assert!(queue.recover("max-fence").is_err());
        let record = queue.get(&task.id).unwrap();
        assert_eq!(record.state, TaskState::Running);
        assert_eq!(record.fence, i64::MAX as u64);
        assert!(record.holds_slot);

        // The detach path for a queued dead claim is also range checked.
        let queued = enqueue(&queue, "max-queued", TaskPriority::Normal, 4);
        let queued_lease = claim(&queue, &queued);
        let before = queue.get(&queued.id).unwrap();
        lock(&queue.connection)
            .execute(
                "UPDATE runtime_tasks SET fence=?1 WHERE id=?2",
                params![i64::MAX, &queued.id],
            )
            .unwrap();
        assert!(queue.recover("max-queued").is_err());
        let after = queue.get(&queued.id).unwrap();
        assert_eq!(after.state, TaskState::Queued);
        assert_eq!(
            after.worker_id.as_deref(),
            Some(queued_lease.worker.as_str())
        );
        assert_eq!(after.fence, i64::MAX as u64);
        assert_eq!(after.ready_order, before.ready_order);
        assert_eq!(after.ready_since_dispatch, before.ready_since_dispatch);
    }

    #[test]
    fn recover_terminates_cancelled_claimed_queue_with_one_audit() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 1, 4, 2);
        let peer = open_queue(&path, 1, 4, 2);

        let task = enqueue(&queue, "cancelled", TaskPriority::Normal, 4);
        let lease = claim(&queue, &task);
        assert!(
            peer.cancel(&queue.get(&task.id).unwrap().cancel_target())
                .unwrap()
        );
        let cancelled = queue.get(&task.id).unwrap();
        assert_eq!(cancelled.state, TaskState::Queued);
        assert!(cancelled.cancel_requested);

        // The original worker is externally proven stopped. Recovery must end
        // the claimed cancellation instead of leaving it occupying capacity.
        queue.recover("cancelled").unwrap();
        let terminal = queue.get(&task.id).unwrap();
        assert_eq!(terminal.state, TaskState::Cancelled);
        assert!(terminal.cancel_requested);
        assert!(!terminal.holds_slot);
        assert_eq!(terminal.fence, cancelled.fence + 1);
        assert_eq!(queue.counts().unwrap(), (0, 0));
        let records = audit.events("cancelled", 0, 100).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, "run_cancelled");
        assert_eq!(records[0].data["worker_id"], lease.worker);

        queue.recover("cancelled").unwrap();
        assert_eq!(audit.events("cancelled", 0, 100).unwrap().len(), 1);
        assert_eq!(queue.get(&task.id).unwrap(), terminal);

        // A non-cancelled queued entry without an attempt is retained.
        let kept = enqueue(&queue, "kept", TaskPriority::Normal, 4);
        let kept_lease = claim(&queue, &kept);
        queue.recover("kept").unwrap();
        let retained = queue.get(&kept.id).unwrap();
        assert_eq!(retained.state, TaskState::Queued);
        assert!(retained.worker_id.is_none());
        assert_eq!(retained.fence, kept_lease.fence + 1);
        assert!(retained.ready_order.is_some());
        assert!(audit.events("kept", 0, 100).unwrap().is_empty());
    }

    #[test]
    fn snapshot_page_uses_ticket_cursor_without_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 1, 8, 2);
        let mut ids = Vec::new();
        for name in ["a", "b", "c", "d", "e"] {
            ids.push(enqueue(&queue, name, TaskPriority::Normal, 8).id);
        }

        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = queue.snapshot_page(after.as_deref(), 2).unwrap();
            if page.is_empty() {
                break;
            }
            after = Some(page.last().unwrap().id.clone());
            seen.extend(page.into_iter().map(|task| task.id));
        }
        assert_eq!(seen, ids);
        assert_eq!(
            seen.iter().collect::<std::collections::HashSet<_>>().len(),
            seen.len()
        );

        assert!(matches!(
            queue.snapshot_page(Some("missing"), 2),
            Err(RuntimeError::InvalidInput(_))
        ));
        assert!(matches!(
            queue.snapshot_page(None, 0),
            Err(RuntimeError::InvalidInput(_))
        ));
        assert!(matches!(
            queue.snapshot_page(None, 1001),
            Err(RuntimeError::InvalidInput(_))
        ));

        // Terminal rows keep their ticket, so a completed id remains usable.
        let terminal = enqueue(&queue, "terminal", TaskPriority::Normal, 8);
        let lease = claim(&queue, &terminal);
        complete(&queue, &lease);
        assert!(
            queue
                .snapshot_page(Some(&terminal.id), 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn recovered_dead_claim_is_detached_and_unrelated_work_runs() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let queue = open_queue(&path, 2, 4, 100);
        let dormant = enqueue(&queue, "dormant", TaskPriority::High, 4);
        let dormant_lease = claim(&queue, &dormant);
        let old_target = queue.get(&dormant.id).unwrap().cancel_target();
        queue.recover("dormant").unwrap();
        let retained = queue.get(&dormant.id).unwrap();

        // Input, priority, and the ready position survive recovery.
        assert_eq!(retained.state, TaskState::Queued);
        assert_eq!(retained.input, dormant.input);
        assert_eq!(retained.priority, TaskPriority::High);
        assert!(retained.ready_order.is_some());
        assert!(retained.ready_since_dispatch.is_some());
        // The dead claim is detached and fenced.
        assert!(retained.worker_id.is_none());
        assert_eq!(retained.fence, dormant_lease.fence + 1);
        assert!(queue.check(&dormant_lease).is_err());
        assert!(queue.admit(&dormant_lease, 2).is_err());
        assert!(
            queue
                .finish(&dormant_lease, TaskTerminal::Completed, None, |_| panic!(
                    "old finish ran"
                ))
                .is_err()
        );
        assert!(matches!(
            queue.cancel(&old_target),
            Err(RuntimeError::StaleCancellation)
        ));
        // Repeating recovery does not advance the fence again.
        queue.recover("dormant").unwrap();
        assert_eq!(
            queue.get(&dormant.id).unwrap().fence,
            dormant_lease.fence + 1
        );

        // A free slot is usable even though the dormant task keeps capacity.
        let unrelated = enqueue(&queue, "unrelated", TaskPriority::Normal, 4);
        let unrelated_lease = claim(&queue, &unrelated);
        assert!(admit(&queue, &unrelated_lease, 2));

        // A new claim keeps the fairness position and advances the fence.
        let retaken = claim(&queue, &dormant);
        assert_eq!(retaken.fence, dormant_lease.fence + 2);
        let reclaimed = queue.get(&dormant.id).unwrap();
        assert_eq!(reclaimed.ready_order, retained.ready_order);
        assert_eq!(
            reclaimed.ready_since_dispatch,
            retained.ready_since_dispatch
        );
        assert_eq!(
            reclaimed.worker_id.as_deref(),
            Some(retaken.worker.as_str())
        );
    }
}

#[cfg(test)]
mod resources_tests {
    use super::*;
    use crate::{
        audit::AuditDb,
        resources::{ResourcePolicy, TaskResources},
    };

    fn policy(bytes: u64) -> ResourcePolicy {
        ResourcePolicy {
            max_declared_memory_bytes: bytes,
        }
    }
    fn resources(bytes: u64) -> TaskResources {
        TaskResources {
            declared_memory_bytes: bytes,
            ..Default::default()
        }
    }
    fn open(path: &Path, bytes: u64) -> Queue {
        Queue::open_with_resources(path, 3, 8, 2, &policy(bytes)).unwrap()
    }
    fn enqueue(queue: &Queue, session: &str, bytes: u64, priority: TaskPriority) -> TaskRecord {
        queue
            .enqueue_with_resources(
                session,
                &TaskInput {
                    text: session.into(),
                    images: vec![],
                },
                &TaskIntent::General,
                priority,
                &resources(bytes),
                8,
            )
            .unwrap()
    }
    fn finish(queue: &Queue, lease: &TaskLease) {
        queue
            .finish(lease, TaskTerminal::Completed, None, |_| Ok(()))
            .unwrap();
    }
    fn usage(queue: &Queue, bytes: u64, tasks: usize) {
        let usage = queue.resource_usage().unwrap();
        assert_eq!(usage.max_declared_memory_bytes, 100);
        assert_eq!(usage.reserved_declared_memory_bytes, bytes);
        assert_eq!(usage.holding_tasks, tasks);
    }

    #[test]
    fn resources_share_one_durable_total_and_small_tasks_cannot_bypass_the_head() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let owner = open(&path, 100);
        let peer = open(&path, 100);
        let a = enqueue(&owner, "a", 60, TaskPriority::Normal);
        let la = owner.claim(&a.id).unwrap();
        assert!(owner.admit(&la, 3).unwrap());
        let b = enqueue(&peer, "b", 50, TaskPriority::High);
        let lb = peer.claim(&b.id).unwrap();
        let c = enqueue(&peer, "c", 10, TaskPriority::Normal);
        let lc = peer.claim(&c.id).unwrap();
        assert!(!peer.admit(&lb, 3).unwrap());
        assert!(!peer.admit(&lc, 3).unwrap());
        assert!(owner.admit(&la, 3).unwrap());
        usage(&owner, 60, 1);
        usage(&peer, 60, 1);
        assert_eq!(peer.get(&a.id).unwrap().resources, resources(60));
        assert_eq!(peer.get(&a.id).unwrap().session_id, "a");
        assert_eq!(
            peer.get(&a.id).unwrap().worker_id.as_deref(),
            Some(la.worker.as_str())
        );
        finish(&owner, &la);
        assert!(peer.admit(&lb, 3).unwrap());
        assert!(peer.admit(&lc, 3).unwrap());
        usage(&owner, 60, 2);
        finish(&peer, &lb);
        usage(&owner, 10, 1);
        finish(&peer, &lc);
        usage(&owner, 0, 0);
    }

    #[test]
    fn simultaneous_connections_cannot_reserve_more_than_the_total() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let owner = open(&path, 100);
        let peer = open(&path, 100);
        let a = enqueue(&owner, "a", 60, TaskPriority::Normal);
        let b = enqueue(&peer, "b", 60, TaskPriority::Normal);
        let la = owner.claim(&a.id).unwrap();
        let lb = peer.claim(&b.id).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let gate = barrier.clone();
        let first = std::thread::spawn(move || {
            gate.wait();
            owner.admit(&la, 3).unwrap()
        });
        let second = std::thread::spawn(move || {
            barrier.wait();
            peer.admit(&lb, 3).unwrap()
        });
        assert_eq!(
            usize::from(first.join().unwrap()) + usize::from(second.join().unwrap()),
            1
        );
        let reopened = open(&path, 100);
        usage(&reopened, 60, 1);
        assert_eq!(
            reopened
                .snapshot(8)
                .unwrap()
                .iter()
                .filter(|task| task.holds_slot)
                .count(),
            1
        );
    }

    #[test]
    fn approval_park_releases_only_execution_admission_and_resume_rechecks_resources() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let owner = open(&path, 100);
        let peer = open(&path, 100);
        let a = enqueue(&owner, "a", 80, TaskPriority::Normal);
        let la = owner.claim(&a.id).unwrap();
        assert!(owner.admit(&la, 3).unwrap());
        owner.attach(&la, "attempt-a").unwrap();
        let identity = owner.get(&a.id).unwrap().cancel_target();
        owner.park(&la).unwrap();
        usage(&peer, 0, 0);
        assert_eq!(peer.get(&a.id).unwrap().resources, resources(80));
        let b = enqueue(&peer, "b", 50, TaskPriority::Normal);
        let lb = peer.claim(&b.id).unwrap();
        assert!(peer.admit(&lb, 3).unwrap());
        owner.ready(&la).unwrap();
        assert!(!owner.admit(&la, 3).unwrap());
        usage(&owner, 50, 1);
        finish(&peer, &lb);
        assert!(owner.admit(&la, 3).unwrap());
        assert_eq!(owner.get(&a.id).unwrap().cancel_target(), identity);
        usage(&owner, 80, 1);
        finish(&owner, &la);
    }

    #[test]
    fn cleanup_and_terminal_persistence_failures_keep_original_resources_on_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let owner = open(&path, 100);
        let a = enqueue(&owner, "a", 80, TaskPriority::Normal);
        let la = owner.claim(&a.id).unwrap();
        assert!(owner.admit(&la, 3).unwrap());
        owner.attach(&la, "attempt-a").unwrap();
        lock(&owner.connection)
            .execute_batch(
                "CREATE TRIGGER fail_resource_terminal BEFORE UPDATE ON runtime_tasks
             WHEN NEW.holds_slot=0 AND OLD.holds_slot=1
             BEGIN SELECT RAISE(ABORT,'synthetic release persistence failure'); END;",
            )
            .unwrap();
        assert!(
            owner
                .finish(&la, TaskTerminal::Completed, None, |_| Ok(()))
                .is_err()
        );
        usage(&owner, 80, 1);
        assert!(owner.park(&la).is_err());
        owner.block(&la, "synthetic unconfirmed cleanup").unwrap();
        let identity = owner.get(&a.id).unwrap();
        assert_eq!(identity.state, TaskState::Draining);
        drop(owner);
        let peer = open(&path, 100);
        assert_eq!(peer.get(&a.id).unwrap(), identity);
        usage(&peer, 80, 1);
        let b = enqueue(&peer, "b", 30, TaskPriority::Normal);
        let lb = peer.claim(&b.id).unwrap();
        assert!(!peer.admit(&lb, 3).unwrap());
        assert!(peer.recover("a").is_err());
        usage(&peer, 80, 1);
        assert_eq!(peer.get(&a.id).unwrap(), identity);
        lock(&peer.connection)
            .execute_batch("DROP TRIGGER fail_resource_terminal")
            .unwrap();
        // This queue primitive is invoked only after Runtime proved the original owner stopped.
        peer.recover("a").unwrap();
        usage(&peer, 0, 0);
        assert!(peer.admit(&la, 3).is_err());
        assert_eq!(peer.get(&a.id).unwrap().state, TaskState::Interrupted);
        assert!(peer.admit(&lb, 3).unwrap());
        usage(&peer, 30, 1);
    }

    #[test]
    fn failed_authority_settlement_does_not_return_reserved_memory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let owner = open(&path, 100);
        let a = enqueue(&owner, "a", 70, TaskPriority::Normal);
        let la = owner.claim(&a.id).unwrap();
        assert!(owner.admit(&la, 3).unwrap());
        assert!(
            owner
                .finish(&la, TaskTerminal::Completed, None, |_| {
                    Err(RuntimeError::Worker(
                        "synthetic authority settlement failure".into(),
                    ))
                })
                .is_err()
        );
        usage(&owner, 70, 1);
        assert_eq!(owner.get(&a.id).unwrap().state, TaskState::Running);
        assert!(audit.events("a", 0, 100).unwrap().is_empty());
    }

    #[test]
    fn invalid_and_unsupported_requests_leave_no_accepted_task_or_reservation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let owner = open(&path, 100);
        for request in [
            resources(101),
            TaskResources {
                hard_memory_limit_bytes: Some(0),
                ..resources(1)
            },
            TaskResources {
                cpu_quota_percent: Some(0),
                ..resources(1)
            },
            TaskResources {
                gpu_units: 1,
                ..resources(1)
            },
            TaskResources {
                io_bandwidth_bytes_per_sec: Some(0),
                ..resources(1)
            },
        ] {
            let error = owner
                .enqueue_with_resources(
                    "rejected",
                    &TaskInput {
                        text: "input".into(),
                        images: vec![],
                    },
                    &TaskIntent::General,
                    TaskPriority::High,
                    &request,
                    8,
                )
                .unwrap_err();
            assert!(matches!(error, RuntimeError::InvalidInput(_)));
            assert!(owner.list("rejected").unwrap().is_empty());
            usage(&owner, 0, 0);
        }
        let child = owner
            .try_start_child("child", "fixed input", &TaskIntent::General, 8, 3)
            .unwrap();
        assert_eq!(
            owner.get(&child.id).unwrap().resources,
            TaskResources::default()
        );
        usage(&owner, 0, 1);
    }

    #[test]
    fn accepted_cancellation_retains_resources_until_the_original_terminal_commits() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let _audit = AuditDb::open(&path).unwrap();
        let owner = open(&path, 100);
        let peer = open(&path, 100);
        let a = enqueue(&owner, "a", 90, TaskPriority::Normal);
        let la = owner.claim(&a.id).unwrap();
        assert!(owner.admit(&la, 3).unwrap());
        owner.attach(&la, "attempt-a").unwrap();
        let target = owner.get(&a.id).unwrap().cancel_target();
        assert!(peer.cancel(&target).unwrap());
        usage(&peer, 90, 1);
        let b = enqueue(&peer, "b", 20, TaskPriority::Normal);
        let lb = peer.claim(&b.id).unwrap();
        assert!(!peer.admit(&lb, 3).unwrap());
        let mut wrong = target.clone();
        wrong.worker_id = Some("other-worker".into());
        assert!(matches!(
            peer.cancel(&wrong),
            Err(RuntimeError::StaleCancellation)
        ));
        usage(&peer, 90, 1);
        owner
            .finish(&la, TaskTerminal::Cancelled, None, |_| Ok(()))
            .unwrap();
        assert_eq!(peer.get(&a.id).unwrap().cancel_target(), target);
        usage(&peer, 0, 0);
        assert!(peer.admit(&lb, 3).unwrap());
    }

    #[test]
    fn commit_failure_rolls_back_the_resource_release_and_keeps_original_identity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let audit = AuditDb::open(&path).unwrap();
        let owner = open(&path, 100);
        let a = enqueue(&owner, "a", 85, TaskPriority::Normal);
        let la = owner.claim(&a.id).unwrap();
        assert!(owner.admit(&la, 3).unwrap());
        owner.attach(&la, "attempt-a").unwrap();
        let before = owner.get(&a.id).unwrap();
        lock(&owner.connection).execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE resource_parent(id INTEGER PRIMARY KEY);
             CREATE TABLE resource_deferred(parent INTEGER REFERENCES resource_parent(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER fail_resource_commit AFTER INSERT ON runtime_audit WHEN NEW.kind='run_finished'
             BEGIN INSERT INTO resource_deferred VALUES(1); END;"
        ).unwrap();
        let settled = std::cell::Cell::new(false);
        assert!(
            owner
                .finish(&la, TaskTerminal::Completed, None, |_| {
                    settled.set(true);
                    Ok(())
                })
                .is_err()
        );
        assert!(
            settled.get(),
            "failure must happen after settlement, at COMMIT"
        );
        assert_eq!(owner.get(&a.id).unwrap(), before);
        usage(&owner, 85, 1);
        assert!(audit.events("a", 0, 100).unwrap().is_empty());
        drop(owner);
        let reopened = open(&path, 100);
        assert_eq!(reopened.get(&a.id).unwrap(), before);
        usage(&reopened, 85, 1);
    }

    #[test]
    fn reopened_policy_adopts_a_trusted_change_only_when_settled() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let owner = open(&path, 100);
        drop(owner);
        // Invalid values are rejected before any policy is written.
        for bytes in [0, u64::MAX] {
            assert!(matches!(
                Queue::open_with_resources(&path, 3, 8, 2, &policy(bytes)),
                Err(RuntimeError::InvalidInput(_))
            ));
        }

        // A complete trusted policy is adopted after a safe stop and remains
        // the one canonical policy for later connections.
        let adopted = open(&path, 101);
        assert_eq!(
            adopted.resource_usage().unwrap().max_declared_memory_bytes,
            101
        );
        let task = enqueue(&adopted, "live", 1, TaskPriority::Normal);
        assert_eq!(task.state, TaskState::Queued);
        drop(adopted);
        // A durable live task blocks replacing the effective policy.
        assert!(matches!(
            Queue::open_with_resources(&path, 3, 8, 2, &policy(102)),
            Err(RuntimeError::InvalidInput(_))
        ));
        let restored = open(&path, 101);
        assert_eq!(restored.get(&task.id).unwrap().state, TaskState::Queued);
        assert_eq!(
            restored.resource_usage().unwrap().max_declared_memory_bytes,
            101
        );
        drop(restored);

        let large = root.path().join("large.sqlite");
        let owner = open(&large, i64::MAX as u64);
        let a = enqueue(&owner, "a", i64::MAX as u64, TaskPriority::Normal);
        let la = owner.claim(&a.id).unwrap();
        assert!(owner.admit(&la, 3).unwrap());
        let b = enqueue(&owner, "b", 1, TaskPriority::Normal);
        let lb = owner.claim(&b.id).unwrap();
        assert!(!owner.admit(&lb, 3).unwrap());
        assert_eq!(
            owner
                .resource_usage()
                .unwrap()
                .reserved_declared_memory_bytes,
            i64::MAX as u64
        );
    }

    #[test]
    fn migration_adds_zero_declarations_without_claiming_or_replaying_legacy_tasks() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite");
        let legacy = Connection::open(&path).unwrap();
        legacy.execute_batch("CREATE TABLE runtime_queue_config (
            singleton INTEGER PRIMARY KEY, concurrency INTEGER NOT NULL, capacity INTEGER NOT NULL,
            fairness_after INTEGER NOT NULL, dispatch_count INTEGER NOT NULL, next_ready_order INTEGER NOT NULL);
            INSERT INTO runtime_queue_config VALUES (1,3,8,2,1,1);
            CREATE TABLE runtime_tasks (
            ticket INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, session TEXT NOT NULL,
            input_json TEXT NOT NULL, state TEXT NOT NULL, queued_at INTEGER NOT NULL,
            worker TEXT, attempt TEXT, fence INTEGER NOT NULL DEFAULT 0, cancel_requested INTEGER NOT NULL DEFAULT 0,
            error TEXT, intent TEXT NOT NULL DEFAULT '{\"kind\":\"general\"}', priority TEXT NOT NULL DEFAULT 'normal',
            holds_slot INTEGER NOT NULL DEFAULT 0, ready_order INTEGER, ready_since_dispatch INTEGER,
            dispatches INTEGER NOT NULL DEFAULT 0);
            INSERT INTO runtime_tasks(id,session,input_json,state,queued_at,worker,attempt,fence,holds_slot,dispatches)
            VALUES ('old','legacy','{\"text\":\"original\",\"images\":[]}','running',1,'original-worker','original-attempt',7,1,1);").unwrap();
        drop(legacy);
        let owner = open(&path, 100);
        let original = owner.get("old").unwrap();
        assert_eq!(original.resources, TaskResources::default());
        assert_eq!(original.worker_id.as_deref(), Some("original-worker"));
        assert_eq!(original.attempt_id.as_deref(), Some("original-attempt"));
        assert_eq!(original.fence, 7);
        assert_eq!(original.state, TaskState::Running);
        usage(&owner, 0, 1);
        drop(owner);
        assert_eq!(open(&path, 100).get("old").unwrap(), original);
    }
}
