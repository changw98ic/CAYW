//! F13 persistent background commands (SLICE P / #33).
//!
//! Design reference: [pi-background-tasks](https://github.com/ismailsaleekh/pi-background-tasks)
//! (shell jobs, log paths, completion notify). CAYW does **not** expose a free-form
//! shell: launches select a host-owned process profile only. Results and logs are
//! keyed by the owning Session so a gateway restart still finds the same owner.
//! Live OS children continue while this Runtime process lives (browser close does
//! not kill them). Re-opening Runtime after process death rehydrates metadata and
//! logs; orphaned `running` rows remain `outcome_unknown` until the host reconciles
//! the result and this background task's own Device cleanup records.
use crate::{Inner, Runtime, RuntimeError, cells::CallScope, coordination::BackgroundLock, lock};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use cayw_host::Profile;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const RUN: &str = "bg.run";
const STATUS: &str = "bg.status";
const LIST: &str = "bg.list";
const LOGS: &str = "bg.logs";
const KILL: &str = "bg.kill";
const RESULT: &str = "bg.result";

const MAX_NAME_BYTES: usize = 128;
const MAX_LOG_PAGE: usize = 64 * 1024;
const DEFAULT_MAX_TASKS: usize = 64;
const DEFAULT_MAX_LOG_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_RETAINED: usize = 128;

/// Trusted host opt-in. `None` omits every `bg.*` tool from the catalog.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundConfig {
    pub max_tasks: usize,
    pub max_log_bytes: usize,
    pub max_retained_finished: usize,
}

/// Raw `runtime_background_tasks` row, in SELECT column order.
type TaskRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<i64>,
    i64,
    String,
    Option<String>,
    i64,
    Option<i64>,
);

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            max_tasks: DEFAULT_MAX_TASKS,
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
            max_retained_finished: DEFAULT_RETAINED,
        }
    }
}

impl BackgroundConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.max_tasks == 0
            || self.max_tasks > 1024
            || self.max_log_bytes < 4096
            || self.max_log_bytes > 64 * 1024 * 1024
            || self.max_retained_finished == 0
            || self.max_retained_finished > 10_000
        {
            return Err(RuntimeError::InvalidInput(
                "background task limits must be positive and bounded".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundStatus {
    Running,
    Completed,
    Failed,
    Killed,
    OutcomeUnknown,
}

impl BackgroundStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
    pub fn parse(value: &str) -> Result<Self, RuntimeError> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "killed" => Ok(Self::Killed),
            "outcome_unknown" => Ok(Self::OutcomeUnknown),
            _ => Err(RuntimeError::InvalidInput(format!(
                "unknown background status {value}"
            ))),
        }
    }
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundTaskView {
    pub id: String,
    pub name: String,
    pub profile: String,
    pub session_id: String,
    pub parent_task_id: Option<String>,
    pub status: BackgroundStatus,
    pub exit_code: Option<i32>,
    pub log_bytes: u64,
    pub log_path: String,
    pub error: Option<String>,
    pub created_ms: i64,
    pub finished_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundLogPage {
    pub id: String,
    pub offset: u64,
    pub next_offset: u64,
    pub bytes_returned: u64,
    pub has_more: bool,
    pub truncated: bool,
    pub text: String,
}

struct LiveHandle {
    cancel: CancellationToken,
    _lock: Arc<BackgroundLock>,
    persistence_failure: Option<String>,
    execution_done: bool,
}

pub(crate) struct BackgroundRegistry {
    connection: Mutex<Connection>,
    root: PathBuf,
    config: BackgroundConfig,
    runner: Arc<cayw_host::ProcessRunner>,
    coordination: crate::coordination::Coordination,
    live: Mutex<BTreeMap<String, LiveHandle>>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn known(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(error.to_string())
}

fn new_id() -> Result<String, CapabilityError> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| known("background task id entropy unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Clone, Debug, Serialize)]
struct RawProcessIdentity {
    pid: Option<i64>,
    parent_pid: Option<i64>,
    process_group: Option<i64>,
    started_seconds: Option<i64>,
    started_microseconds: Option<i64>,
}

impl RawProcessIdentity {
    fn read(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            pid: row.get(offset)?,
            parent_pid: row.get(offset + 1)?,
            process_group: row.get(offset + 2)?,
            started_seconds: row.get(offset + 3)?,
            started_microseconds: row.get(offset + 4)?,
        })
    }

    fn decode(&self) -> Option<cayw_host::ProcessIdentity> {
        let identity = cayw_host::ProcessIdentity {
            pid: u32::try_from(self.pid?).ok()?,
            parent_pid: u32::try_from(self.parent_pid?).ok()?,
            process_group: u32::try_from(self.process_group?).ok()?,
            started_seconds: u64::try_from(self.started_seconds?).ok()?,
            started_microseconds: u32::try_from(self.started_microseconds?).ok()?,
        };
        (identity.pid > 0
            && i32::try_from(identity.pid).is_ok()
            && identity.started_seconds > 0
            && identity.started_microseconds < 1_000_000)
            .then_some(identity)
    }

    fn is_absent(&self) -> bool {
        self.pid.is_none()
            && self.parent_pid.is_none()
            && self.process_group.is_none()
            && self.started_seconds.is_none()
            && self.started_microseconds.is_none()
    }
}

/// Original background owners presented to the trusted host's cleanup verifier.
/// Missing or incomplete identities require independent host records; an empty
/// process list alone never establishes that no program ran.
#[derive(Debug)]
pub struct BackgroundCleanupInspection {
    pub task: BackgroundTaskView,
    pub supervisor: Option<cayw_host::ProcessIdentity>,
    pub processes: Vec<cayw_host::ProcessIdentity>,
    pub incomplete_identities: bool,
}

struct CleanupRecords {
    owner: (String, String, String, String, Option<String>, i64),
    supervisor: Option<RawProcessIdentity>,
    root: RawProcessIdentity,
    processes: Vec<(RawProcessIdentity, String)>,
    state: Option<String>,
    verified_snapshot: Option<String>,
}

impl CleanupRecords {
    fn snapshot(&self) -> Result<String, RuntimeError> {
        serde_json::to_string(&(
            &self.owner,
            &self.supervisor,
            &self.root,
            &self.processes,
            &self.state,
        ))
        .map_err(|error| RuntimeError::Worker(error.to_string()))
    }

    fn inspection(&self, task: BackgroundTaskView) -> BackgroundCleanupInspection {
        let supervisor = self
            .supervisor
            .as_ref()
            .and_then(RawProcessIdentity::decode);
        let root = self.root.decode();
        let mut incomplete = supervisor.is_none()
            || (root.is_none() && self.state.as_deref() != Some("not_started"));
        let mut processes = Vec::new();
        if let Some(identity) = root {
            processes.push(identity);
        }
        for (raw, _) in &self.processes {
            if let Some(identity) = raw.decode() {
                if !processes.contains(&identity) {
                    processes.push(identity);
                }
            } else {
                incomplete = true;
            }
        }
        BackgroundCleanupInspection {
            task,
            supervisor,
            processes,
            incomplete_identities: incomplete,
        }
    }
}

fn observe_supervisor() -> Result<cayw_host::ProcessIdentity, RuntimeError> {
    #[cfg(target_os = "macos")]
    {
        cayw_host::ProcessIdentity::observe(std::process::id()).map_err(|error| {
            RuntimeError::Worker(format!("observe background supervisor: {error}"))
        })
    }
    #[cfg(not(target_os = "macos"))]
    Err(RuntimeError::Worker(
        "background process identity is unavailable on this platform".into(),
    ))
}

fn require_stopped_with(
    identity: cayw_host::ProcessIdentity,
    probe: impl FnOnce(cayw_host::ProcessIdentity) -> std::io::Result<bool>,
) -> Result<(), RuntimeError> {
    match probe(identity) {
        Ok(false) => Ok(()),
        Ok(true) => Err(RuntimeError::Busy),
        Err(error) => Err(RuntimeError::Worker(format!(
            "original background process cleanup cannot be observed: {error}"
        ))),
    }
}

fn require_stopped(identity: cayw_host::ProcessIdentity) -> Result<(), RuntimeError> {
    #[cfg(target_os = "macos")]
    {
        require_stopped_with(identity, cayw_host::ProcessIdentity::is_running)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = identity;
        Err(RuntimeError::Worker(
            "background process identity is unavailable on this platform".into(),
        ))
    }
}

fn require_evidence(evidence: Option<&str>) -> Result<&str, RuntimeError> {
    evidence
        .filter(|value| !value.trim().is_empty() && value.len() <= 4096)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(
                "background recovery requires 1..4096 bytes of explicit host evidence".into(),
            )
        })
}

impl BackgroundRegistry {
    #[cfg(test)]
    pub(crate) fn open(store_path: &Path, config: &BackgroundConfig) -> Result<Self, RuntimeError> {
        let runner = Self::default_runner()?;
        Connection::open(store_path)?;
        let coordination = crate::coordination::Coordination::new(store_path)?;
        Self::open_with_runner(store_path, config, runner, coordination)
    }

    #[cfg(test)]
    fn default_runner() -> Result<Arc<cayw_host::ProcessRunner>, RuntimeError> {
        let programs = [
            "/bin/echo",
            "/bin/sh",
            "/bin/sleep",
            "/bin/cat",
            "/bin/date",
            "/usr/bin/printf",
            "/usr/bin/true",
            "/usr/bin/false",
        ]
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_file());
        let runner = cayw_host::ProcessRunner::new(programs)
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        Ok(Arc::new(runner))
    }

    pub(crate) fn open_with_runner(
        store_path: &Path,
        config: &BackgroundConfig,
        runner: Arc<cayw_host::ProcessRunner>,
        coordination: crate::coordination::Coordination,
    ) -> Result<Self, RuntimeError> {
        config.validate()?;
        let canonical_store = store_path
            .canonicalize()
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let connection = Connection::open(&canonical_store)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS runtime_background_tasks (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                profile TEXT NOT NULL,
                session_id TEXT NOT NULL,
                parent_task_id TEXT,
                status TEXT NOT NULL,
                exit_code INTEGER,
                log_bytes INTEGER NOT NULL,
                log_path TEXT NOT NULL,
                error TEXT,
                created_ms INTEGER NOT NULL,
                finished_ms INTEGER,
                pid INTEGER,
                parent_pid INTEGER,
                process_group INTEGER,
                started_seconds INTEGER,
                started_microseconds INTEGER
            );
            CREATE INDEX IF NOT EXISTS runtime_background_by_session
                ON runtime_background_tasks(session_id, created_ms);
            CREATE TABLE IF NOT EXISTS runtime_background_log_index (
                id TEXT NOT NULL,
                offset INTEGER NOT NULL,
                line INTEGER NOT NULL,
                PRIMARY KEY (id, offset)
            );
            CREATE TABLE IF NOT EXISTS runtime_background_processes (
                task_id TEXT NOT NULL,
                pid INTEGER NOT NULL,
                parent_pid INTEGER NOT NULL,
                process_group INTEGER NOT NULL,
                started_seconds INTEGER NOT NULL,
                started_microseconds INTEGER NOT NULL,
                state TEXT NOT NULL,
                PRIMARY KEY (task_id, pid, started_seconds, started_microseconds)
            );
            CREATE TABLE IF NOT EXISTS runtime_background_executors (
                task_id TEXT PRIMARY KEY,
                pid INTEGER NOT NULL,
                parent_pid INTEGER NOT NULL,
                process_group INTEGER NOT NULL,
                started_seconds INTEGER NOT NULL,
                started_microseconds INTEGER NOT NULL,
                cleanup_state TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtime_background_cleanup_confirmations (
                task_id TEXT PRIMARY KEY,
                identity_snapshot TEXT NOT NULL,
                evidence TEXT NOT NULL,
                confirmed_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtime_background_cancels (
                task_id TEXT PRIMARY KEY,
                requested_ms INTEGER NOT NULL
            );",
        )?;
        let parent = canonical_store.parent().ok_or_else(|| {
            RuntimeError::InvalidInput("background store needs a parent path".into())
        })?;
        let root = parent.join("background-tasks");
        fs::create_dir_all(&root).map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let registry = Self {
            connection: Mutex::new(connection),
            root,
            config: config.clone(),
            runner,
            coordination,
            live: Mutex::new(BTreeMap::new()),
        };
        registry.mark_orphans()?;
        Ok(registry)
    }

    pub(crate) fn try_lock_background(
        &self,
        id: &str,
    ) -> Result<Option<BackgroundLock>, RuntimeError> {
        self.coordination.try_background(id)
    }

    fn cleanup_records(connection: &Connection, id: &str) -> Result<CleanupRecords, RuntimeError> {
        let owner = connection.query_row(
            "SELECT id,name,profile,session_id,parent_task_id,created_ms FROM runtime_background_tasks WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )?;
        let root = connection.query_row(
            "SELECT pid,parent_pid,process_group,started_seconds,started_microseconds
             FROM runtime_background_tasks WHERE id=?1",
            [id],
            |row| RawProcessIdentity::read(row, 0),
        )?;
        let executor = connection
            .query_row(
                "SELECT pid,parent_pid,process_group,started_seconds,started_microseconds,cleanup_state
                 FROM runtime_background_executors WHERE task_id=?1",
                [id],
                |row| Ok((RawProcessIdentity::read(row, 0)?, row.get::<_, String>(5)?)),
            )
            .optional()?;
        let (supervisor, state) = match executor {
            Some((identity, state)) => (Some(identity), Some(state)),
            None => (None, None),
        };
        let mut statement = connection.prepare(
            "SELECT pid,parent_pid,process_group,started_seconds,started_microseconds,state
             FROM runtime_background_processes WHERE task_id=?1
             ORDER BY pid,started_seconds,started_microseconds",
        )?;
        let processes = statement
            .query_map([id], |row| {
                Ok((RawProcessIdentity::read(row, 0)?, row.get::<_, String>(5)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let verified_snapshot = connection
            .query_row(
                "SELECT identity_snapshot FROM runtime_background_cleanup_confirmations WHERE task_id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(CleanupRecords {
            owner,
            supervisor,
            root,
            processes,
            state,
            verified_snapshot,
        })
    }

    fn require_cleanup(records: &CleanupRecords) -> Result<(), RuntimeError> {
        let snapshot = records.snapshot()?;
        if records.verified_snapshot.as_deref() == Some(snapshot.as_str()) {
            for raw in
                std::iter::once(&records.root).chain(records.processes.iter().map(|(raw, _)| raw))
            {
                if let Some(identity) = raw.decode() {
                    require_stopped(identity)?;
                }
            }
            return Ok(());
        }
        if records
            .supervisor
            .as_ref()
            .and_then(RawProcessIdentity::decode)
            .is_none()
        {
            return Err(RuntimeError::Worker(
                "background cleanup has no complete original executor identity".into(),
            ));
        }
        if records.state.as_deref() == Some("not_started")
            && records.root.is_absent()
            && records.processes.is_empty()
        {
            return Ok(());
        }
        let root = records.root.decode().ok_or_else(|| {
            RuntimeError::Worker(
                "background cleanup has no complete original process identity".into(),
            )
        })?;
        if records.state.as_deref() != Some("device_confirmed")
            || !records
                .processes
                .iter()
                .any(|(raw, state)| raw.decode() == Some(root) && state == "stopped")
        {
            return Err(RuntimeError::Worker(
                "background task has no confirmation from its own Device supervisor".into(),
            ));
        }
        for (raw, state) in &records.processes {
            let identity = raw.decode().ok_or_else(|| {
                RuntimeError::Worker(
                    "background cleanup contains an incomplete process identity".into(),
                )
            })?;
            if state != "stopped" {
                return Err(RuntimeError::Worker(
                    "background process cleanup is not confirmed".into(),
                ));
            }
            require_stopped(identity)?;
        }
        require_stopped(root)
    }

    fn mark_orphans(&self) -> Result<(), RuntimeError> {
        let connection = lock(&self.connection);
        let mut statement =
            connection.prepare("SELECT id FROM runtime_background_tasks WHERE status='running'")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for id in ids {
            let Some(_task_lock) = self.try_lock_background(&id)? else {
                continue;
            };
            // Losing an executor leaves the command's result unknown, even if
            // its parent task or every currently visible PID has stopped.
            connection.execute(
                "UPDATE runtime_background_tasks
                 SET status='outcome_unknown', error='unreconciled_runtime_restart', finished_ms=?2
                 WHERE id=?1 AND status='running'",
                params![id, now_ms()],
            )?;
        }
        Ok(())
    }

    fn count_running(connection: &Connection) -> Result<usize, RuntimeError> {
        Ok(connection.query_row(
            "SELECT count(*) FROM runtime_background_tasks WHERE status IN ('running','outcome_unknown')",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize)
    }

    fn prune_finished(&self, connection: &Connection) -> Result<(), RuntimeError> {
        let finished: usize = connection.query_row(
            "SELECT count(*) FROM runtime_background_tasks WHERE status IN ('completed', 'failed', 'killed')",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        if finished <= self.config.max_retained_finished {
            return Ok(());
        }
        let excess = finished - self.config.max_retained_finished;
        let mut stmt = connection.prepare(
            "SELECT id, log_path FROM runtime_background_tasks
             WHERE status IN ('completed', 'failed', 'killed')
             ORDER BY COALESCE(finished_ms, created_ms) ASC
             LIMIT ?1",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map(params![excess as i64], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for (id, log_path) in rows {
            let _ = fs::remove_file(&log_path);
            connection.execute(
                "DELETE FROM runtime_background_log_index WHERE id=?1",
                params![id],
            )?;
            connection.execute(
                "DELETE FROM runtime_background_processes WHERE task_id=?1",
                params![id],
            )?;
            connection.execute(
                "DELETE FROM runtime_background_executors WHERE task_id=?1",
                params![id],
            )?;
            connection.execute(
                "DELETE FROM runtime_background_cleanup_confirmations WHERE task_id=?1",
                params![id],
            )?;
            connection.execute(
                "DELETE FROM runtime_background_cancels WHERE task_id=?1",
                params![id],
            )?;
            connection.execute(
                "DELETE FROM runtime_background_tasks WHERE id=?1",
                params![id],
            )?;
        }
        Ok(())
    }

    fn row_to_view(
        (
            id,
            name,
            profile,
            session_id,
            parent_task_id,
            status,
            exit_code,
            log_bytes,
            log_path,
            error,
            created_ms,
            finished_ms,
        ): TaskRow,
    ) -> Result<BackgroundTaskView, RuntimeError> {
        Ok(BackgroundTaskView {
            id,
            name,
            profile,
            session_id,
            parent_task_id,
            status: BackgroundStatus::parse(&status)?,
            exit_code: exit_code.map(|c| c as i32),
            log_bytes: log_bytes as u64,
            log_path,
            error,
            created_ms,
            finished_ms,
        })
    }

    fn load(&self, id: &str) -> Result<Option<BackgroundTaskView>, RuntimeError> {
        let connection = lock(&self.connection);
        let mut view = connection
            .query_row(
                "SELECT id,name,profile,session_id,parent_task_id,status,exit_code,log_bytes,log_path,error,created_ms,finished_ms
                 FROM runtime_background_tasks WHERE id=?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                    ))
                },
            )
            .optional()?
            .map(Self::row_to_view)
            .transpose()?;
        drop(connection);
        if let Some(view) = view.as_mut() {
            self.apply_persistence_failure(view);
        }
        Ok(view)
    }

    fn list_session(&self, session_id: &str) -> Result<Vec<BackgroundTaskView>, RuntimeError> {
        let connection = lock(&self.connection);
        let mut stmt = connection.prepare(
            "SELECT id,name,profile,session_id,parent_task_id,status,exit_code,log_bytes,log_path,error,created_ms,finished_ms
             FROM runtime_background_tasks WHERE session_id=?1
             ORDER BY created_ms DESC LIMIT 256",
        )?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(connection);
        rows.into_iter()
            .map(|row| {
                let mut view = Self::row_to_view(row)?;
                self.apply_persistence_failure(&mut view);
                Ok(view)
            })
            .collect()
    }

    fn insert_running(
        &self,
        id: &str,
        name: &str,
        profile: &str,
        session_id: &str,
        parent_task_id: &str,
        log_path: &Path,
    ) -> Result<(), RuntimeError> {
        let supervisor = observe_supervisor()?;
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if Self::count_running(&tx)? >= self.config.max_tasks {
            return Err(RuntimeError::InvalidInput(
                "background task capacity is full".into(),
            ));
        }
        tx.execute(
            "INSERT INTO runtime_background_tasks(
                id,name,profile,session_id,parent_task_id,status,exit_code,log_bytes,log_path,error,created_ms,finished_ms,pid)
             VALUES(?1,?2,?3,?4,?5,'running',NULL,0,?6,NULL,?7,NULL,NULL)",
            params![
                id,
                name,
                profile,
                session_id,
                parent_task_id,
                log_path.to_string_lossy(),
                now_ms()
            ],
        )?;
        tx.execute(
            "INSERT INTO runtime_background_executors(
                task_id,pid,parent_pid,process_group,started_seconds,started_microseconds,cleanup_state)
             VALUES(?1,?2,?3,?4,?5,?6,'open')",
            params![
                id,
                i64::from(supervisor.pid),
                i64::from(supervisor.parent_pid),
                i64::from(supervisor.process_group),
                crate::audit::sql_u64(supervisor.started_seconds)?,
                i64::from(supervisor.started_microseconds),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn require_executor(connection: &Connection, id: &str) -> Result<(), RuntimeError> {
        let supervisor = observe_supervisor()?;
        let identity = connection.query_row(
            "SELECT pid,parent_pid,process_group,started_seconds,started_microseconds
             FROM runtime_background_executors WHERE task_id=?1 AND cleanup_state='open'",
            [id],
            |row| RawProcessIdentity::read(row, 0),
        )?;
        let owned: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_background_tasks WHERE id=?1 AND status='running')",
            [id],
            |row| row.get(0),
        )?;
        if !owned || identity.decode() != Some(supervisor) {
            return Err(RuntimeError::Worker(
                "background process record differs from its original executor".into(),
            ));
        }
        Ok(())
    }

    fn record_cleanup(&self, id: &str, state: &str) -> Result<(), RuntimeError> {
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::require_executor(&tx, id)?;
        tx.execute(
            "UPDATE runtime_background_executors SET cleanup_state=?2 WHERE task_id=?1 AND cleanup_state='open'",
            params![id, state],
        )?;
        if matches!(state, "device_confirmed" | "not_started") {
            Self::require_cleanup(&Self::cleanup_records(&tx, id)?)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn record_process_started(
        &self,
        id: &str,
        identity: &cayw_host::ProcessIdentity,
    ) -> Result<(), RuntimeError> {
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::require_executor(&tx, id)?;
        let updated = tx.execute(
            "UPDATE runtime_background_tasks
             SET pid=?2, parent_pid=?3, process_group=?4, started_seconds=?5, started_microseconds=?6
             WHERE id=?1 AND status='running' AND pid IS NULL",
            params![
                id,
                identity.pid as i64,
                identity.parent_pid as i64,
                identity.process_group as i64,
                crate::audit::sql_u64(identity.started_seconds)?,
                identity.started_microseconds as i64,
            ],
        )?;
        if updated != 1 {
            return Err(RuntimeError::Worker(
                "background process already has an original identity".into(),
            ));
        }
        tx.execute(
            "INSERT INTO runtime_background_processes(
                task_id, pid, parent_pid, process_group, started_seconds, started_microseconds, state)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'started')",
            params![
                id,
                identity.pid as i64,
                identity.parent_pid as i64,
                identity.process_group as i64,
                crate::audit::sql_u64(identity.started_seconds)?,
                identity.started_microseconds as i64,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn record_processes_observed(
        &self,
        id: &str,
        identities: &[cayw_host::ProcessIdentity],
    ) -> Result<(), RuntimeError> {
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::require_executor(&tx, id)?;
        for identity in identities {
            tx.execute(
                "INSERT OR IGNORE INTO runtime_background_processes(
                    task_id, pid, parent_pid, process_group, started_seconds, started_microseconds, state)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'observed')",
                params![
                    id,
                    identity.pid as i64,
                    identity.parent_pid as i64,
                    identity.process_group as i64,
                    crate::audit::sql_u64(identity.started_seconds)?,
                    identity.started_microseconds as i64,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn record_process_stopped(
        &self,
        id: &str,
        identity: &cayw_host::ProcessIdentity,
    ) -> Result<(), RuntimeError> {
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::require_executor(&tx, id)?;
        let updated = tx.execute(
            "UPDATE runtime_background_processes
             SET state='stopped'
             WHERE task_id=?1 AND pid=?2 AND started_seconds=?3 AND started_microseconds=?4
             AND parent_pid=?5 AND process_group=?6",
            params![
                id,
                i64::from(identity.pid),
                crate::audit::sql_u64(identity.started_seconds)?,
                i64::from(identity.started_microseconds),
                i64::from(identity.parent_pid),
                i64::from(identity.process_group),
            ],
        )?;
        if updated != 1 {
            return Err(RuntimeError::Worker(
                "stopped background process has no matching original record".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    fn update_log_bytes(&self, id: &str, bytes: u64) -> Result<(), RuntimeError> {
        lock(&self.connection).execute(
            "UPDATE runtime_background_tasks SET log_bytes=?2 WHERE id=?1",
            params![id, bytes as i64],
        )?;
        Ok(())
    }

    fn finish(
        &self,
        id: &str,
        status: BackgroundStatus,
        exit_code: Option<i32>,
        error: Option<&str>,
    ) -> Result<BackgroundTaskView, RuntimeError> {
        if status == BackgroundStatus::Running {
            return Err(RuntimeError::InvalidInput(
                "background finish requires a stopped execution".into(),
            ));
        }
        {
            let mut connection = lock(&self.connection);
            let tx =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if status != BackgroundStatus::OutcomeUnknown {
                Self::require_cleanup(&Self::cleanup_records(&tx, id)?)?;
            }
            tx.execute(
                "UPDATE runtime_background_tasks
                 SET status=?2, exit_code=?3, error=?4, finished_ms=?5
                 WHERE id=?1 AND status='running'",
                params![
                    id,
                    status.as_str(),
                    exit_code.map(i64::from),
                    error,
                    now_ms()
                ],
            )?;
            // Prune older known results after recording this terminal. Unknown
            // tasks and their ownership records are never retention candidates.
            self.prune_finished(&tx)?;
            tx.commit()?;
        }
        let view = self.load(id)?.ok_or_else(|| {
            RuntimeError::InvalidInput("background task missing after finish".into())
        })?;
        let cleanup_confirmed =
            Self::require_cleanup(&Self::cleanup_records(&lock(&self.connection), id)?).is_ok();
        if cleanup_confirmed {
            lock(&self.live).remove(id);
        } else if let Some(live) = lock(&self.live).get_mut(id) {
            live.execution_done = true;
        }
        Ok(view)
    }

    fn persistence_failed(&self, id: &str, error: impl std::fmt::Display) {
        if let Some(live) = lock(&self.live).get_mut(id) {
            live.persistence_failure = Some(format!("background result is not durable: {error}"));
            live.execution_done = true;
        }
    }

    fn apply_persistence_failure(&self, view: &mut BackgroundTaskView) {
        if let Some(error) = lock(&self.live)
            .get(&view.id)
            .and_then(|live| live.persistence_failure.clone())
        {
            view.status = BackgroundStatus::OutcomeUnknown;
            view.exit_code = None;
            view.error = Some(error);
        }
    }

    fn read_logs(
        &self,
        id: &str,
        offset: u64,
        limit: usize,
    ) -> Result<BackgroundLogPage, RuntimeError> {
        let view = self
            .load(id)?
            .ok_or_else(|| RuntimeError::InvalidInput("background task not found".into()))?;
        let limit = limit.clamp(1, MAX_LOG_PAGE);
        let mut file =
            File::open(&view.log_path).map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let total = file
            .metadata()
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?
            .len();
        let offset = offset.min(total);
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let mut buf = vec![0u8; limit];
        let read = file
            .read(&mut buf)
            .map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        buf.truncate(read);
        let truncated = total > self.config.max_log_bytes as u64
            || view.log_bytes > self.config.max_log_bytes as u64;
        let text = String::from_utf8_lossy(&buf).into_owned();
        let next = offset + read as u64;
        Ok(BackgroundLogPage {
            id: view.id,
            offset,
            next_offset: next,
            bytes_returned: read as u64,
            has_more: next < total,
            truncated,
            text,
        })
    }

    fn request_kill(&self, id: &str) -> Result<BackgroundTaskView, RuntimeError> {
        let view = self
            .load(id)?
            .ok_or_else(|| RuntimeError::InvalidInput("background task not found".into()))?;
        if view.status.is_terminal() {
            return Ok(view);
        }
        if let Some(live) = lock(&self.live).get(id) {
            live.cancel.cancel();
            let connection = lock(&self.connection);
            let _ = connection.execute(
                "INSERT INTO runtime_background_cancels(task_id, requested_ms) VALUES(?1, ?2)
                 ON CONFLICT(task_id) DO UPDATE SET requested_ms=excluded.requested_ms",
                params![id, now_ms()],
            );
            return Ok(view);
        }
        let connection = lock(&self.connection);
        let executor: Option<(RawProcessIdentity, String)> = connection
            .query_row(
                "SELECT pid,parent_pid,process_group,started_seconds,started_microseconds,cleanup_state
                 FROM runtime_background_executors WHERE task_id=?1",
                [id],
                |row| Ok((RawProcessIdentity::read(row, 0)?, row.get::<_, String>(5)?)),
            )
            .optional()?;
        let Some((executor_raw, cleanup_state)) = executor else {
            return Err(RuntimeError::Worker(
                "background task has no original executor record".into(),
            ));
        };
        if cleanup_state != "open" {
            return Err(RuntimeError::Worker(
                "background task original executor is already closed".into(),
            ));
        }
        let Some(executor_id) = executor_raw.decode() else {
            return Err(RuntimeError::Worker(
                "background task executor identity is incomplete".into(),
            ));
        };
        #[cfg(target_os = "macos")]
        {
            match executor_id.is_running() {
                Ok(true) => {}
                Ok(false) => {
                    return Err(RuntimeError::Worker(
                        "background task original executor is not running".into(),
                    ));
                }
                Err(error) => {
                    return Err(RuntimeError::Worker(format!(
                        "observe background executor: {error}"
                    )));
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = executor_id;
            return Err(RuntimeError::Worker(
                "background task executor cannot be verified on this platform".into(),
            ));
        }
        connection.execute(
            "INSERT INTO runtime_background_cancels(task_id, requested_ms) VALUES(?1, ?2)
             ON CONFLICT(task_id) DO UPDATE SET requested_ms=excluded.requested_ms",
            params![id, now_ms()],
        )?;
        Ok(view)
    }

    fn recovery_lock(&self, id: &str) -> Result<Arc<BackgroundLock>, RuntimeError> {
        if let Some(live) = lock(&self.live).get(id) {
            if !live.execution_done {
                return Err(RuntimeError::Busy);
            }
            return Ok(live._lock.clone());
        }
        self.try_lock_background(id)?
            .map(Arc::new)
            .ok_or(RuntimeError::Busy)
    }

    fn confirm_cleanup(
        &self,
        id: &str,
        evidence: &str,
        verify_stopped: impl FnOnce(&BackgroundCleanupInspection) -> Result<(), String>,
    ) -> Result<(), RuntimeError> {
        let evidence = require_evidence(Some(evidence))?;
        let _task_lock = self.recovery_lock(id)?;
        let task = self
            .load(id)?
            .ok_or_else(|| RuntimeError::InvalidInput("background task not found".into()))?;
        if task.status != BackgroundStatus::OutcomeUnknown {
            return Err(RuntimeError::InvalidInput(
                "background cleanup confirmation requires an unknown outcome".into(),
            ));
        }
        let records = Self::cleanup_records(&lock(&self.connection), id)?;
        let snapshot = records.snapshot()?;
        let inspection = records.inspection(task);
        // A live original executor may still own Device cleanup resources after
        // its command stops. Only its own durable cleanup can release that owner.
        if Self::require_cleanup(&records).is_err()
            && let Some(supervisor) = inspection.supervisor
        {
            require_stopped(supervisor)?;
        }
        for identity in &inspection.processes {
            require_stopped(*identity)?;
        }
        verify_stopped(&inspection).map_err(RuntimeError::Worker)?;
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if Self::cleanup_records(&tx, id)?.snapshot()? != snapshot {
            return Err(RuntimeError::Busy);
        }
        let status: String = tx.query_row(
            "SELECT status FROM runtime_background_tasks WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        let local_unknown = lock(&self.live)
            .get(id)
            .is_some_and(|live| live.persistence_failure.is_some());
        if status != "outcome_unknown" && !(status == "running" && local_unknown) {
            return Err(RuntimeError::Busy);
        }
        tx.execute(
            "INSERT INTO runtime_background_cleanup_confirmations(task_id,identity_snapshot,evidence,confirmed_ms)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(task_id) DO UPDATE SET identity_snapshot=excluded.identity_snapshot,
                 evidence=excluded.evidence,confirmed_ms=excluded.confirmed_ms",
            params![id, snapshot, evidence, now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn recover(
        &self,
        id: &str,
        status: BackgroundStatus,
        error: Option<&str>,
    ) -> Result<BackgroundTaskView, RuntimeError> {
        if matches!(
            status,
            BackgroundStatus::Running | BackgroundStatus::OutcomeUnknown
        ) {
            return Err(RuntimeError::InvalidInput(
                "background recovery requires a known terminal result".into(),
            ));
        }
        let evidence = require_evidence(error)?;
        let _task_lock = self.recovery_lock(id)?;
        let view = self
            .load(id)?
            .ok_or_else(|| RuntimeError::InvalidInput("background task not found".into()))?;
        if view.status != BackgroundStatus::OutcomeUnknown {
            return Err(RuntimeError::InvalidInput(
                "only an unknown background result can be recovered".into(),
            ));
        }
        {
            let mut connection = lock(&self.connection);
            let tx =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            Self::require_cleanup(&Self::cleanup_records(&tx, id)?)?;
            let local_unknown = lock(&self.live)
                .get(id)
                .is_some_and(|live| live.persistence_failure.is_some());
            let updated = tx.execute(
                "UPDATE runtime_background_tasks
                 SET status=?2, exit_code=NULL, error=?3, finished_ms=?4
                 WHERE id=?1 AND (status='outcome_unknown' OR (status='running' AND ?5))",
                params![id, status.as_str(), evidence, now_ms(), local_unknown],
            )?;
            if updated != 1 {
                return Err(RuntimeError::Busy);
            }
            tx.commit()?;
        }
        lock(&self.live).remove(id);
        self.load(id)?.ok_or_else(|| {
            RuntimeError::InvalidInput("background task missing after recover".into())
        })
    }
}

pub(crate) fn validate(config: Option<&BackgroundConfig>) -> Result<(), RuntimeError> {
    if let Some(config) = config {
        config.validate()?;
    }
    Ok(())
}

pub(crate) fn definitions(
    config: Option<&BackgroundConfig>,
    profiles: &BTreeMap<String, Profile>,
) -> Vec<ToolDefinition> {
    let Some(_config) = config else {
        return vec![];
    };
    if profiles.is_empty() {
        return vec![];
    }
    let names: Vec<String> = profiles.keys().cloned().collect();
    let profile = json!({"type":"string","enum":names});
    let id = json!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[a-f0-9]{32}$"});
    vec![
        ToolDefinition {
            name: RUN.into(),
            description: "Start one host-selected process profile as a persistent background command. Returns immediately with a durable task id. The child keeps running after the browser disconnects while this Runtime process lives. Logs and the eventual result stay bound to the owning Session.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["profile"],"properties":{
                "profile":profile,
                "name":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES}
            }}),
        },
        ToolDefinition {
            name: STATUS.into(),
            description: "Read one background task's durable status, log size and exit code. Only tasks owned by the current Session are visible.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":id}}),
        },
        ToolDefinition {
            name: LIST.into(),
            description: "List background tasks for the current Session, newest first.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{}}),
        },
        ToolDefinition {
            name: LOGS.into(),
            description: "Read a byte page from a background task's durable log. offset/limit are absolute UTF-8-lossy byte ranges; the log index records line starts for navigation.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["id"],"properties":{
                "id":id,
                "offset":{"type":"integer","minimum":0},
                "limit":{"type":"integer","minimum":1,"maximum":MAX_LOG_PAGE}
            }}),
        },
        ToolDefinition {
            name: KILL.into(),
            description: "Request cancellation of a running background task owned by the current Session.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":id}}),
        },
        ToolDefinition {
            name: RESULT.into(),
            description: "Fetch the terminal result of a background task owned by the current Session. Refuses while status is still running.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":id}}),
        },
    ]
}

pub(crate) fn is_tool(name: &str) -> bool {
    matches!(name, RUN | STATUS | LIST | LOGS | KILL | RESULT)
}

pub(crate) async fn invoke(
    scope: &Arc<CallScope>,
    base: &cayw_host::Host,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let definition = definitions(
        scope.context.inner.config.background.as_ref(),
        &scope.context.inner.config.host.profiles,
    )
    .into_iter()
    .find(|definition| definition.name == scope.name)
    .ok_or_else(|| known("background tasks are not configured"))?;
    let host = crate::bridge::scoped_host(base, scope)?;
    let service = Service { scope };
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let run = host.call_service(&definition, arguments, child.clone(), &service);
    crate::requests::watch(scope, child, run).await
}

struct Service<'a> {
    scope: &'a Arc<CallScope>,
}

#[async_trait]
impl cayw_host::ExternalHost for Service<'_> {
    async fn call(
        &self,
        capability: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if capability != self.scope.name {
            return Err(known(
                "background service differs from the original capability",
            ));
        }
        self.scope.check_authority(&cancel)?;
        let registry = self
            .scope
            .context
            .inner
            .background
            .as_ref()
            .ok_or_else(|| known("background tasks are not configured"))?;
        let session = self.scope.context.lease.session_id.as_str();
        match capability {
            RUN => run_task(self.scope, registry, session, arguments).await,
            STATUS => status_task(registry, session, arguments),
            LIST => list_tasks(registry, session),
            LOGS => logs_task(registry, session, arguments),
            KILL => kill_task(registry, session, arguments),
            RESULT => result_task(registry, session, arguments),
            _ => Err(known("not a background capability")),
        }
    }
}

fn ensure_owner(view: &BackgroundTaskView, session: &str) -> Result<(), CapabilityError> {
    if view.session_id != session {
        return Err(known("background task belongs to another Session"));
    }
    Ok(())
}

fn status_task(
    registry: &BackgroundRegistry,
    session: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let id = arguments
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| known("background task id is required"))?;
    let view = registry
        .load(id)
        .map_err(known)?
        .ok_or_else(|| known("background task not found"))?;
    ensure_owner(&view, session)?;
    serde_json::to_value(view).map_err(known)
}

fn list_tasks(registry: &BackgroundRegistry, session: &str) -> Result<Value, CapabilityError> {
    let tasks = registry.list_session(session).map_err(known)?;
    serde_json::to_value(json!({"tasks": tasks, "session_id": session})).map_err(known)
}

fn logs_task(
    registry: &BackgroundRegistry,
    session: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let id = arguments
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| known("background task id is required"))?;
    let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let limit = arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(8192) as usize;
    let view = registry
        .load(id)
        .map_err(known)?
        .ok_or_else(|| known("background task not found"))?;
    ensure_owner(&view, session)?;
    let page = registry.read_logs(id, offset, limit).map_err(known)?;
    serde_json::to_value(page).map_err(known)
}

fn kill_task(
    registry: &BackgroundRegistry,
    session: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let id = arguments
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| known("background task id is required"))?;
    let view = registry
        .load(id)
        .map_err(known)?
        .ok_or_else(|| known("background task not found"))?;
    ensure_owner(&view, session)?;
    if view.status.is_terminal() {
        return serde_json::to_value(json!({
            "id": view.id,
            "status": view.status,
            "cancel_requested": false
        }))
        .map_err(known);
    }
    let view = registry.request_kill(id).map_err(known)?;
    serde_json::to_value(json!({"id": view.id, "status": view.status, "cancel_requested": true}))
        .map_err(known)
}

fn result_task(
    registry: &BackgroundRegistry,
    session: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let id = arguments
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| known("background task id is required"))?;
    let view = registry
        .load(id)
        .map_err(known)?
        .ok_or_else(|| known("background task not found"))?;
    ensure_owner(&view, session)?;
    if !view.status.is_terminal() {
        return Err(known("background task is still running"));
    }
    serde_json::to_value(view).map_err(known)
}

async fn run_task(
    scope: &Arc<CallScope>,
    registry: &Arc<BackgroundRegistry>,
    session: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let profile_name = arguments
        .get("profile")
        .and_then(Value::as_str)
        .ok_or_else(|| known("background profile is required"))?;
    let profile = scope
        .context
        .inner
        .config
        .host
        .profiles
        .get(profile_name)
        .cloned()
        .ok_or_else(|| known("background profile is not configured"))?;
    let name = arguments
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(profile_name);
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(known("background task name is invalid"));
    }
    let id = new_id()?;
    let lock_file = registry
        .try_lock_background(&id)
        .map_err(known)?
        .ok_or_else(|| known("background task already locked"))?;
    let lock_file = Arc::new(lock_file);
    let log_path = registry.root.join(format!("{id}.log"));
    File::create(&log_path).map_err(known)?;
    registry
        .insert_running(
            &id,
            name,
            profile_name,
            session,
            &scope.context.task.id,
            &log_path,
        )
        .map_err(known)?;
    let cancel = CancellationToken::new();
    lock(&registry.live).insert(
        id.clone(),
        LiveHandle {
            cancel: cancel.clone(),
            _lock: lock_file.clone(),
            persistence_failure: None,
            execution_done: false,
        },
    );
    let inner = scope.context.inner.clone();
    let registry_worker = registry.clone();
    let session_id = session.to_owned();
    let task_id = id.clone();
    let profile_label = profile_name.to_owned();
    let workspace = scope.context.inner.root.clone();
    tokio::spawn(async move {
        // Keep ownership through the terminal write, including its failure path.
        let _task_lock = lock_file;
        let outcome = drive_child(
            registry_worker.clone(),
            task_id.clone(),
            profile,
            workspace,
            log_path,
            cancel,
        )
        .await;
        let (status, code, error) = match outcome {
            ChildOutcome::Completed(code) => {
                if code == 0 {
                    (BackgroundStatus::Completed, Some(code), None)
                } else {
                    (
                        BackgroundStatus::Failed,
                        Some(code),
                        Some(format!("exit_code={code}")),
                    )
                }
            }
            ChildOutcome::Killed => (BackgroundStatus::Killed, None, Some("cancelled".into())),
            ChildOutcome::Failed(message) => (BackgroundStatus::Failed, None, Some(message)),
            ChildOutcome::Unknown(message) => {
                (BackgroundStatus::OutcomeUnknown, None, Some(message))
            }
        };
        match registry_worker.finish(&task_id, status, code, error.as_deref()) {
            Ok(view) => publish_completion(&inner, &session_id, &view, &profile_label),
            Err(failure) => registry_worker.persistence_failed(&task_id, failure),
        }
    });
    let view = registry
        .load(&id)
        .map_err(known)?
        .ok_or_else(|| known("background task missing after start"))?;
    serde_json::to_value(view).map_err(known)
}

enum ChildOutcome {
    Completed(i32),
    Killed,
    Failed(String),
    Unknown(String),
}

fn device_error_unknown(error: &cayw_host::DeviceError) -> bool {
    matches!(
        error,
        cayw_host::DeviceError::CleanupUnconfirmed(_)
            | cayw_host::DeviceError::SupervisorStopped
            | cayw_host::DeviceError::Io { .. }
            | cayw_host::DeviceError::ProcessClosed
    )
}

struct BackgroundObserver {
    registry: Arc<BackgroundRegistry>,
    id: String,
}

impl std::fmt::Debug for BackgroundObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundObserver")
            .field("id", &self.id)
            .finish()
    }
}

impl cayw_host::ProcessObserver for BackgroundObserver {
    fn started(&self, identity: &cayw_host::ProcessIdentity) -> std::io::Result<()> {
        self.registry
            .record_process_started(&self.id, identity)
            .map_err(|error| {
                std::io::Error::other(format!("background process start is not durable: {error}"))
            })
    }

    fn observed(&self, identities: &[cayw_host::ProcessIdentity]) -> std::io::Result<()> {
        self.registry
            .record_processes_observed(&self.id, identities)
            .map_err(|error| {
                std::io::Error::other(format!(
                    "background process observation is not durable: {error}"
                ))
            })
    }

    fn stopped(&self, identity: &cayw_host::ProcessIdentity) -> std::io::Result<()> {
        self.registry
            .record_process_stopped(&self.id, identity)
            .map_err(|error| {
                std::io::Error::other(format!("background process stop is not durable: {error}"))
            })
    }
}

async fn drive_child(
    registry: Arc<BackgroundRegistry>,
    id: String,
    profile: Profile,
    workspace: PathBuf,
    log_path: PathBuf,
    cancel: CancellationToken,
) -> ChildOutcome {
    let canonical_program = match fs::canonicalize(&profile.program) {
        Ok(path) => path,
        Err(error) => {
            return match registry.record_cleanup(&id, "not_started") {
                Ok(()) => ChildOutcome::Failed(format!("canonicalize program failed: {error}")),
                Err(record_error) => ChildOutcome::Unknown(format!(
                    "program preparation failed: {error}; cleanup record failed: {record_error}"
                )),
            };
        }
    };
    let observer = Arc::new(BackgroundObserver {
        registry: registry.clone(),
        id: id.clone(),
    });
    let runner = registry.runner.observed(observer);

    let timeout = if profile.timeout.is_zero() {
        Duration::from_secs(3600)
    } else {
        profile.timeout
    };
    let spec = cayw_host::ProcessSpec {
        program: canonical_program,
        args: profile.args.clone(),
        cwd: workspace,
        env: profile.env.clone(),
        timeout,
        max_output_bytes: registry.config.max_log_bytes.min(profile.max_output_bytes),
        io: profile.io,
    };

    let cancel_poll = cancel.clone();
    let registry_poll = registry.clone();
    let id_poll = id.clone();
    let is_cancelled = {
        let connection = lock(&registry.connection);
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_background_cancels WHERE task_id=?1)",
                [&id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
    };
    if is_cancelled {
        cancel.cancel();
    }
    let watcher = tokio::spawn(async move {
        while !cancel_poll.is_cancelled() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if cancel_poll.is_cancelled() {
                break;
            }
            let cancelled = {
                let connection = lock(&registry_poll.connection);
                connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM runtime_background_cancels WHERE task_id=?1)",
                        [&id_poll],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(false)
            };
            if cancelled {
                cancel_poll.cancel();
                break;
            }
        }
    });

    let result = runner.run(spec, cancel).await;
    watcher.abort();
    match result {
        Ok(proc_res) => {
            if let Err(error) = registry.record_cleanup(&id, "device_confirmed") {
                return ChildOutcome::Unknown(format!("Device cleanup record failed: {error}"));
            }
            if let Err(error) = write_logs(
                &registry,
                &id,
                &log_path,
                &proc_res.stdout,
                &proc_res.stderr,
            ) {
                return ChildOutcome::Unknown(format!("background log is not durable: {error}"));
            }
            match proc_res.termination {
                cayw_host::TerminationReason::Cancelled => ChildOutcome::Killed,
                cayw_host::TerminationReason::TimedOut => ChildOutcome::Failed("timed_out".into()),
                cayw_host::TerminationReason::OutputLimit => {
                    ChildOutcome::Failed("output_limit_exceeded".into())
                }
                cayw_host::TerminationReason::Exited => {
                    ChildOutcome::Completed(proc_res.status.code().unwrap_or(-1))
                }
            }
        }
        Err(error) => {
            if device_error_unknown(&error) {
                let message = match registry.record_cleanup(&id, "unconfirmed") {
                    Ok(()) => error.to_string(),
                    Err(record_error) => {
                        format!("{error}; background cleanup record failed: {record_error}")
                    }
                };
                return ChildOutcome::Unknown(message);
            }
            match registry.record_cleanup(&id, "not_started") {
                Err(record_error) => ChildOutcome::Unknown(format!(
                    "{error}; background cleanup record failed: {record_error}"
                )),
                Ok(()) if matches!(error, cayw_host::DeviceError::Cancelled) => {
                    ChildOutcome::Killed
                }
                Ok(()) => ChildOutcome::Failed(error.to_string()),
            }
        }
    }
}

fn write_logs(
    registry: &Arc<BackgroundRegistry>,
    id: &str,
    log_path: &Path,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(), RuntimeError> {
    let io_error = |error: std::io::Error| RuntimeError::Workspace(error.to_string());
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(log_path)
        .map_err(io_error)?;
    file.write_all(stdout).map_err(io_error)?;
    file.write_all(stderr).map_err(io_error)?;
    file.flush().map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    let total_bytes = (stdout.len() + stderr.len()) as i64;
    let mut connection = lock(&registry.connection);
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let updated = tx.execute(
        "UPDATE runtime_background_tasks SET log_bytes=?2 WHERE id=?1 AND status='running'",
        params![id, total_bytes],
    )?;
    if updated != 1 {
        return Err(RuntimeError::Worker(
            "background log has no running owner".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

fn publish_completion(
    inner: &Arc<Inner>,
    session_id: &str,
    view: &BackgroundTaskView,
    profile: &str,
) {
    let data = json!({
        "background_task_id": view.id,
        "name": view.name,
        "profile": profile,
        "status": view.status.as_str(),
        "exit_code": view.exit_code,
        "log_bytes": view.log_bytes,
        "session_id": view.session_id,
        "parent_task_id": view.parent_task_id,
    });
    for _ in 0..8 {
        let Ok(snapshot) = inner.store.snapshot(session_id) else {
            return;
        };
        match inner.store.append_control_event(
            session_id,
            snapshot.cursor,
            snapshot.attempt.as_ref(),
            "runtime-background",
            "background_task_completed",
            data.clone(),
        ) {
            Ok(_) => {
                inner.changed.notify_waiters();
                return;
            }
            Err(cayw_core::CoreError::StaleLease) => continue,
            Err(_) => return,
        }
    }
}

impl Runtime {
    /// Trusted host inspection of one background task (any Session the host names).
    pub fn background_inspect(&self, id: &str) -> Result<Option<BackgroundTaskView>, RuntimeError> {
        let Some(registry) = self.inner.background.as_ref() else {
            return Err(RuntimeError::InvalidInput(
                "background tasks are not configured".into(),
            ));
        };
        registry.load(id)
    }

    pub fn background_list_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<BackgroundTaskView>, RuntimeError> {
        self.get(session_id)?;
        let Some(registry) = self.inner.background.as_ref() else {
            return Err(RuntimeError::InvalidInput(
                "background tasks are not configured".into(),
            ));
        };
        registry.list_session(session_id)
    }

    pub fn background_logs(
        &self,
        id: &str,
        offset: u64,
        limit: usize,
    ) -> Result<BackgroundLogPage, RuntimeError> {
        let Some(registry) = self.inner.background.as_ref() else {
            return Err(RuntimeError::InvalidInput(
                "background tasks are not configured".into(),
            ));
        };
        registry.read_logs(id, offset, limit)
    }

    pub fn background_kill(&self, id: &str) -> Result<BackgroundTaskView, RuntimeError> {
        let Some(registry) = self.inner.background.as_ref() else {
            return Err(RuntimeError::InvalidInput(
                "background tasks are not configured".into(),
            ));
        };
        registry.request_kill(id)
    }

    pub fn background_result(&self, id: &str) -> Result<BackgroundTaskView, RuntimeError> {
        let Some(registry) = self.inner.background.as_ref() else {
            return Err(RuntimeError::InvalidInput(
                "background tasks are not configured".into(),
            ));
        };
        let view = registry
            .load(id)?
            .ok_or_else(|| RuntimeError::InvalidInput("background task not found".into()))?;
        if !view.status.is_terminal() {
            return Err(RuntimeError::InvalidInput(
                "background task is still running".into(),
            ));
        }
        Ok(view)
    }

    /// Confirm cleanup with independent host records while holding this exact
    /// background task's ownership. This records cleanup only, never its result.
    /// The verifier must resolve every missing identity against its own records.
    pub fn background_confirm_cleanup(
        &self,
        id: &str,
        evidence: &str,
        verify_stopped: impl FnOnce(&BackgroundCleanupInspection) -> Result<(), String>,
    ) -> Result<(), RuntimeError> {
        let Some(registry) = self.inner.background.as_ref() else {
            return Err(RuntimeError::InvalidInput(
                "background tasks are not configured".into(),
            ));
        };
        registry.confirm_cleanup(id, evidence, verify_stopped)
    }

    pub fn background_recover(
        &self,
        id: &str,
        status: BackgroundStatus,
        error: Option<&str>,
    ) -> Result<BackgroundTaskView, RuntimeError> {
        let Some(registry) = self.inner.background.as_ref() else {
            return Err(RuntimeError::InvalidInput(
                "background tasks are not configured".into(),
            ));
        };
        registry.recover(id, status, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn hex32(seed: u8) -> String {
        format!("{seed:02x}").repeat(16)
    }

    fn insert(registry: &BackgroundRegistry, id: &str) -> PathBuf {
        let log = registry.root.join(format!("{id}.log"));
        File::create(&log).unwrap();
        registry
            .insert_running(id, "demo", "build", "sess-1", "task-1", &log)
            .unwrap();
        log
    }

    fn stopped_identity() -> cayw_host::ProcessIdentity {
        // Synthetic journal fixture, deliberately a different incarnation from
        // the current process. Real descendant cleanup is tested separately.
        let mut identity = observe_supervisor().unwrap();
        identity.started_seconds -= 1;
        identity
    }

    fn record_stopped_process(registry: &BackgroundRegistry, id: &str) {
        let identity = stopped_identity();
        registry.record_process_started(id, &identity).unwrap();
        registry.record_process_stopped(id, &identity).unwrap();
        registry.record_cleanup(id, "device_confirmed").unwrap();
    }

    #[test]
    fn journal_round_trip_binds_session_and_survives_reopen() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let config = BackgroundConfig::default();
        let id = hex32(0xab);
        {
            let registry = BackgroundRegistry::open(&db, &config).unwrap();
            let log = registry.root.join(format!("{id}.log"));
            File::create(&log).unwrap();
            registry
                .insert_running(&id, "demo", "build", "sess-1", "task-1", &log)
                .unwrap();
            std::fs::write(&log, b"hello\nworld\n").unwrap();
            registry.update_log_bytes(&id, 12).unwrap();
            let view = registry.load(&id).unwrap().unwrap();
            assert_eq!(view.session_id, "sess-1");
            assert_eq!(view.status, BackgroundStatus::Running);
            let page = registry.read_logs(&id, 0, 64).unwrap();
            assert!(page.text.contains("hello"));
            assert_eq!(page.next_offset, 12);
            record_stopped_process(&registry, &id);
            registry
                .finish(&id, BackgroundStatus::Completed, Some(0), None)
                .unwrap();
        }
        let registry = BackgroundRegistry::open(&db, &config).unwrap();
        let view = registry.load(&id).unwrap().unwrap();
        assert_eq!(view.session_id, "sess-1");
        assert_eq!(view.status, BackgroundStatus::Completed);
        assert_eq!(view.exit_code, Some(0));
        let listed = registry.list_session("sess-1").unwrap();
        assert_eq!(listed.len(), 1);
        assert!(registry.list_session("other").unwrap().is_empty());
    }

    #[test]
    fn reopen_marks_orphaned_running_without_cleanup_as_unknown() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let config = BackgroundConfig::default();
        let id = hex32(0xcd);
        {
            let registry = BackgroundRegistry::open(&db, &config).unwrap();
            let log = registry.root.join(format!("{id}.log"));
            File::create(&log).unwrap();
            registry
                .insert_running(&id, "orphan", "build", "sess-2", "task-9", &log)
                .unwrap();
        }
        let registry = BackgroundRegistry::open(&db, &config).unwrap();
        let view = registry.load(&id).unwrap().unwrap();
        assert_eq!(view.status, BackgroundStatus::OutcomeUnknown);
        assert_eq!(view.error.as_deref(), Some("unreconciled_runtime_restart"));
        assert_eq!(view.session_id, "sess-2");

        assert!(
            registry
                .recover(&id, BackgroundStatus::Completed, None)
                .is_err()
        );
        assert!(
            registry
                .recover(&id, BackgroundStatus::Completed, Some("host_reconciled"))
                .is_err()
        );
        assert!(
            registry
                .confirm_cleanup(&id, "still alive", |_| Ok(()))
                .is_err()
        );
        assert_eq!(
            registry.load(&id).unwrap().unwrap().status,
            BackgroundStatus::OutcomeUnknown
        );
    }

    #[test]
    fn parent_drain_never_confirms_background_cleanup() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let config = BackgroundConfig::default();
        let id = hex32(0xef);
        {
            let registry = BackgroundRegistry::open(&db, &config).unwrap();
            let log = registry.root.join(format!("{id}.log"));
            File::create(&log).unwrap();
            registry
                .insert_running(&id, "orphan", "build", "sess-2", "task-clean", &log)
                .unwrap();
            // Simulate confirmed cleanup in runtime_process_scopes
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "CREATE TABLE IF NOT EXISTS runtime_process_scopes (task TEXT PRIMARY KEY, state TEXT NOT NULL)",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO runtime_process_scopes (task, state) VALUES ('task-clean', 'drained')",
                [],
            )
            .unwrap();
        }
        let registry = BackgroundRegistry::open(&db, &config).unwrap();
        let view = registry.load(&id).unwrap().unwrap();
        assert_eq!(view.status, BackgroundStatus::OutcomeUnknown);
        assert_eq!(view.error.as_deref(), Some("unreconciled_runtime_restart"));
        assert_eq!(view.session_id, "sess-2");
        assert!(
            registry
                .recover(&id, BackgroundStatus::Failed, Some("parent drained"))
                .is_err()
        );
    }

    #[test]
    fn probe_errors_and_incomplete_identities_are_not_cleanup() {
        let identity = stopped_identity();
        assert!(
            require_stopped_with(identity, |_| Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "probe denied",
            )))
            .is_err()
        );
        assert!(matches!(
            require_stopped_with(identity, |_| Ok(true)),
            Err(RuntimeError::Busy)
        ));
        let mut raw = RawProcessIdentity {
            pid: Some(i64::from(identity.pid)),
            parent_pid: None,
            process_group: Some(i64::from(identity.process_group)),
            started_seconds: Some(identity.started_seconds as i64),
            started_microseconds: Some(i64::from(identity.started_microseconds)),
        };
        assert!(raw.decode().is_none());
        raw.parent_pid = Some(-1);
        assert!(raw.decode().is_none());
        raw.parent_pid = Some(i64::from(identity.parent_pid));
        raw.pid = Some(i64::MAX);
        assert!(raw.decode().is_none());
    }

    #[test]
    fn cleanup_does_not_reconcile_the_command_result_or_rewrite_a_terminal() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let id = hex32(0x42);
        {
            let registry = BackgroundRegistry::open(&db, &BackgroundConfig::default()).unwrap();
            insert(&registry, &id);
            record_stopped_process(&registry, &id);
        }
        let registry = BackgroundRegistry::open(&db, &BackgroundConfig::default()).unwrap();
        assert_eq!(
            registry.load(&id).unwrap().unwrap().status,
            BackgroundStatus::OutcomeUnknown
        );
        assert!(
            registry
                .recover(&id, BackgroundStatus::Completed, None)
                .is_err()
        );
        let recovered = registry
            .recover(
                &id,
                BackgroundStatus::Completed,
                Some("host checked command receipt"),
            )
            .unwrap();
        assert_eq!(recovered.status, BackgroundStatus::Completed);
        assert!(
            registry
                .recover(&id, BackgroundStatus::Failed, Some("overwrite"))
                .is_err()
        );
        assert_eq!(
            registry
                .finish(&id, BackgroundStatus::Failed, None, Some("late worker"))
                .unwrap(),
            recovered
        );
    }

    #[test]
    fn changed_identity_records_invalidate_host_confirmation() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let id = hex32(0x43);
        let registry = BackgroundRegistry::open(&db, &BackgroundConfig::default()).unwrap();
        insert(&registry, &id);
        let connection = lock(&registry.connection);
        connection
            .execute(
                "DELETE FROM runtime_background_executors WHERE task_id=?1",
                [&id],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE runtime_background_tasks SET status='outcome_unknown' WHERE id=?1",
                [&id],
            )
            .unwrap();
        drop(connection);
        registry
            .confirm_cleanup(&id, "independent fixture receipt", |inspection| {
                assert!(inspection.incomplete_identities);
                Ok(())
            })
            .unwrap();
        lock(&registry.connection)
            .execute(
                "UPDATE runtime_background_tasks SET parent_pid=1 WHERE id=?1",
                [&id],
            )
            .unwrap();
        assert!(
            registry
                .recover(&id, BackgroundStatus::Failed, Some("different identity"))
                .is_err()
        );
        registry
            .confirm_cleanup(&id, "host verified the updated fixture", |_| Ok(()))
            .unwrap();
        lock(&registry.connection)
            .execute(
                "UPDATE runtime_background_tasks SET session_id='different-session' WHERE id=?1",
                [&id],
            )
            .unwrap();
        assert!(
            registry
                .recover(&id, BackgroundStatus::Failed, Some("different owner"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn failed_observer_record_reaches_device_and_keeps_unknown() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let id = hex32(0x44);
        let registry =
            Arc::new(BackgroundRegistry::open(&db, &BackgroundConfig::default()).unwrap());
        let log = insert(&registry, &id);
        lock(&registry.connection).execute_batch(
            "CREATE TRIGGER fail_background_identity BEFORE INSERT ON runtime_background_processes
             BEGIN SELECT RAISE(ABORT,'injected identity storage failure'); END;",
        ).unwrap();
        let observer = BackgroundObserver {
            registry: registry.clone(),
            id: id.clone(),
        };
        let identity = stopped_identity();
        assert!(cayw_host::ProcessObserver::started(&observer, &identity).is_err());
        let outcome = drive_child(
            registry.clone(),
            id.clone(),
            test_profile(vec!["echo side-effect".into()]),
            dir.path().canonicalize().unwrap(),
            log,
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(outcome, ChildOutcome::Unknown(_)));
        assert!(registry.runner.cleanup_failure().is_some());
        registry
            .finish(
                &id,
                BackgroundStatus::OutcomeUnknown,
                None,
                Some("identity storage failed"),
            )
            .unwrap();
        assert!(
            registry
                .recover(&id, BackgroundStatus::Completed, Some("process exited"))
                .is_err()
        );
    }

    fn test_profile(args: Vec<String>) -> Profile {
        Profile {
            kind: cayw_host::ProfileKind::Check,
            program: "/bin/sh".into(),
            args: std::iter::once("-c".into()).chain(args).collect(),
            env: BTreeMap::new(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 4096,
            allow_filter: false,
            io: cayw_host::ProcessIo::Pipes,
        }
    }

    #[tokio::test]
    async fn failed_logs_preserve_unknown_and_device_cleanup_allows_reconciliation() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let id = hex32(0x45);
        let registry =
            Arc::new(BackgroundRegistry::open(&db, &BackgroundConfig::default()).unwrap());
        let log = insert(&registry, &id);
        fs::remove_file(&log).unwrap();
        fs::create_dir(&log).unwrap();
        let outcome = drive_child(
            registry.clone(),
            id.clone(),
            test_profile(vec!["echo executed".into()]),
            dir.path().canonicalize().unwrap(),
            log,
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(outcome, ChildOutcome::Unknown(_)));
        let view = registry
            .finish(
                &id,
                BackgroundStatus::OutcomeUnknown,
                None,
                Some("log storage failed"),
            )
            .unwrap();
        assert_eq!(view.status, BackgroundStatus::OutcomeUnknown);
        let view = registry
            .recover(
                &id,
                BackgroundStatus::Completed,
                Some("host checked command receipt despite missing log"),
            )
            .unwrap();
        assert_eq!(view.status, BackgroundStatus::Completed);
    }

    #[tokio::test]
    async fn failed_log_byte_count_keeps_unknown() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let id = hex32(0x48);
        let registry =
            Arc::new(BackgroundRegistry::open(&db, &BackgroundConfig::default()).unwrap());
        let log = insert(&registry, &id);
        lock(&registry.connection)
            .execute_batch(
                "CREATE TRIGGER fail_log_bytes BEFORE UPDATE OF log_bytes ON runtime_background_tasks BEGIN SELECT RAISE(ABORT,'injected log bytes failure'); END;",
            )
            .unwrap();
        let outcome = drive_child(
            registry.clone(),
            id.clone(),
            test_profile(vec!["echo executed".into()]),
            dir.path().canonicalize().unwrap(),
            log.clone(),
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(outcome, ChildOutcome::Unknown(_)));
        assert!(fs::read_to_string(log).unwrap().contains("executed"));
        assert_eq!(registry.load(&id).unwrap().unwrap().log_bytes, 0);
        let view = registry
            .finish(
                &id,
                BackgroundStatus::OutcomeUnknown,
                None,
                Some("log metadata failed"),
            )
            .unwrap();
        assert_eq!(view.status, BackgroundStatus::OutcomeUnknown);
    }

    #[test]
    fn failed_terminal_write_retains_ownership_and_unknown_view() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let id = hex32(0x46);
        let registry = BackgroundRegistry::open(&db, &BackgroundConfig::default()).unwrap();
        let file = registry.try_lock_background(&id).unwrap().unwrap();
        insert(&registry, &id);
        record_stopped_process(&registry, &id);
        lock(&registry.live).insert(
            id.clone(),
            LiveHandle {
                cancel: CancellationToken::new(),
                _lock: Arc::new(file),
                persistence_failure: None,
                execution_done: false,
            },
        );
        lock(&registry.connection).execute_batch(
            "CREATE TRIGGER fail_background_terminal BEFORE UPDATE OF status ON runtime_background_tasks
             BEGIN SELECT RAISE(ABORT,'injected terminal failure'); END;",
        ).unwrap();
        let error = registry
            .finish(&id, BackgroundStatus::Completed, Some(0), None)
            .unwrap_err();
        registry.persistence_failed(&id, error);
        assert!(registry.try_lock_background(&id).unwrap().is_none());
        assert_eq!(
            registry.load(&id).unwrap().unwrap().status,
            BackgroundStatus::OutcomeUnknown
        );
        assert_eq!(
            registry.list_session("sess-1").unwrap()[0].status,
            BackgroundStatus::OutcomeUnknown
        );
        lock(&registry.connection)
            .execute_batch("DROP TRIGGER fail_background_terminal;")
            .unwrap();
        registry
            .recover(
                &id,
                BackgroundStatus::Completed,
                Some("host checked result receipt after storage repair"),
            )
            .unwrap();
        assert!(registry.try_lock_background(&id).unwrap().is_some());
    }

    #[test]
    fn config_rejects_unknown_fields_and_legacy_status() {
        let mut value = serde_json::to_value(BackgroundConfig::default()).unwrap();
        value["max_task"] = json!(10);
        assert!(serde_json::from_value::<BackgroundConfig>(value).is_err());
        assert!(BackgroundStatus::parse("unknown").is_err());
        assert!(serde_json::from_value::<BackgroundStatus>(json!("unknown")).is_err());
        assert_eq!(
            BackgroundStatus::parse("outcome_unknown").unwrap(),
            BackgroundStatus::OutcomeUnknown
        );
        assert!(device_error_unknown(
            &cayw_host::DeviceError::SupervisorStopped
        ));
        assert!(device_error_unknown(
            &cayw_host::DeviceError::CleanupUnconfirmed("unconfirmed".into())
        ));
        assert!(!device_error_unknown(&cayw_host::DeviceError::NotStarted {
            operation: "fixture",
            source: std::io::Error::other("before exec"),
        }));
    }

    #[test]
    fn config_rejects_zero_capacity() {
        let bad = BackgroundConfig {
            max_tasks: 0,
            ..BackgroundConfig::default()
        };
        assert!(bad.validate().is_err());
    }
}
