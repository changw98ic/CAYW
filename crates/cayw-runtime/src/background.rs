//! F13 persistent background commands (SLICE P / #33).
//!
//! Design reference: [pi-background-tasks](https://github.com/ismailsaleekh/pi-background-tasks)
//! (shell jobs, log paths, completion notify). CAYW does **not** expose a free-form
//! shell: launches select a host-owned process profile only. Results and logs are
//! keyed by the owning Session so a gateway restart still finds the same owner.
//! Live OS children continue while this Runtime process lives (browser close does
//! not kill them). Re-opening Runtime after process death rehydrates metadata and
//! logs; orphaned `running` rows become `failed` with reason `runtime_restarted`
//! (same class of boundary as pi's non-surviving process restart).
use crate::{Inner, Runtime, RuntimeError, cells::CallScope, lock};
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
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

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
#[derive(Clone, Debug)]
pub struct BackgroundConfig {
    pub max_tasks: usize,
    pub max_log_bytes: usize,
    pub max_retained_finished: usize,
}

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
}

impl BackgroundStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
        }
    }
    fn parse(value: &str) -> Result<Self, RuntimeError> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "killed" => Ok(Self::Killed),
            _ => Err(RuntimeError::InvalidInput(format!(
                "unknown background status {value}"
            ))),
        }
    }
    fn is_terminal(self) -> bool {
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
}

pub(crate) struct BackgroundRegistry {
    connection: Mutex<Connection>,
    root: PathBuf,
    config: BackgroundConfig,
    live: Mutex<BTreeMap<String, LiveHandle>>,
    notify: Notify,
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

impl BackgroundRegistry {
    pub(crate) fn open(store_path: &Path, config: &BackgroundConfig) -> Result<Self, RuntimeError> {
        config.validate()?;
        let connection = Connection::open(store_path)?;
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
                pid INTEGER
            );
            CREATE INDEX IF NOT EXISTS runtime_background_by_session
                ON runtime_background_tasks(session_id, created_ms);
            CREATE TABLE IF NOT EXISTS runtime_background_log_index (
                id TEXT NOT NULL,
                offset INTEGER NOT NULL,
                line INTEGER NOT NULL,
                PRIMARY KEY (id, offset)
            );",
        )?;
        let parent = store_path.parent().ok_or_else(|| {
            RuntimeError::InvalidInput("background store needs a parent path".into())
        })?;
        let root = parent.join("background-tasks");
        fs::create_dir_all(&root).map_err(|e| RuntimeError::Workspace(e.to_string()))?;
        let registry = Self {
            connection: Mutex::new(connection),
            root,
            config: config.clone(),
            live: Mutex::new(BTreeMap::new()),
            notify: Notify::new(),
        };
        registry.mark_orphans()?;
        Ok(registry)
    }

    fn mark_orphans(&self) -> Result<(), RuntimeError> {
        let connection = lock(&self.connection);
        connection.execute(
            "UPDATE runtime_background_tasks
             SET status='failed', error='runtime_restarted', finished_ms=?1
             WHERE status='running'",
            params![now_ms()],
        )?;
        Ok(())
    }

    fn count_running(connection: &Connection) -> Result<usize, RuntimeError> {
        Ok(connection.query_row(
            "SELECT count(*) FROM runtime_background_tasks WHERE status='running'",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize)
    }

    fn prune_finished(&self, connection: &Connection) -> Result<(), RuntimeError> {
        let finished: usize = connection.query_row(
            "SELECT count(*) FROM runtime_background_tasks WHERE status!='running'",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        if finished <= self.config.max_retained_finished {
            return Ok(());
        }
        let excess = finished - self.config.max_retained_finished;
        let mut stmt = connection.prepare(
            "SELECT id, log_path FROM runtime_background_tasks
             WHERE status!='running'
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
                "DELETE FROM runtime_background_tasks WHERE id=?1",
                params![id],
            )?;
        }
        Ok(())
    }

    fn row_to_view(
        id: String,
        name: String,
        profile: String,
        session_id: String,
        parent_task_id: Option<String>,
        status: String,
        exit_code: Option<i64>,
        log_bytes: i64,
        log_path: String,
        error: Option<String>,
        created_ms: i64,
        finished_ms: Option<i64>,
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
        connection
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
            .map(|(a, b, c, d, e, f, g, h, i, j, k, l)| {
                Self::row_to_view(a, b, c, d, e, f, g, h, i, j, k, l)
            })
            .transpose()
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
        rows.into_iter()
            .map(|(a, b, c, d, e, f, g, h, i, j, k, l)| {
                Self::row_to_view(a, b, c, d, e, f, g, h, i, j, k, l)
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
        let mut connection = lock(&self.connection);
        let tx = connection.transaction()?;
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
        tx.commit()?;
        Ok(())
    }

    fn set_pid(&self, id: &str, pid: u32) -> Result<(), RuntimeError> {
        lock(&self.connection).execute(
            "UPDATE runtime_background_tasks SET pid=?2 WHERE id=?1 AND status='running'",
            params![id, pid as i64],
        )?;
        Ok(())
    }

    fn append_log_index(&self, id: &str, offset: u64, line: u64) -> Result<(), RuntimeError> {
        lock(&self.connection).execute(
            "INSERT OR IGNORE INTO runtime_background_log_index(id,offset,line) VALUES(?1,?2,?3)",
            params![id, offset as i64, line as i64],
        )?;
        Ok(())
    }

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
        {
            let mut connection = lock(&self.connection);
            let tx = connection.transaction()?;
            let updated = tx.execute(
                "UPDATE runtime_background_tasks
                 SET status=?2, exit_code=?3, error=?4, finished_ms=?5
                 WHERE id=?1 AND status='running'",
                params![
                    id,
                    status.as_str(),
                    exit_code.map(|c| c as i64),
                    error,
                    now_ms()
                ],
            )?;
            if updated == 0 {
                // Already terminal (e.g. kill raced with exit).
            }
            self.prune_finished(&tx)?;
            tx.commit()?;
        }
        lock(&self.live).remove(id);
        self.notify.notify_waiters();
        self.load(id)?.ok_or_else(|| {
            RuntimeError::InvalidInput("background task missing after finish".into())
        })
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
        }
        // Soft mark; the waiter finishes with killed once the child exits.
        Ok(view)
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
    let log_path = registry.root.join(format!("{id}.log"));
    File::create(&log_path).map_err(|e| known(e))?;
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
        },
    );
    let inner = scope.context.inner.clone();
    let registry_worker = registry.clone();
    let session_id = session.to_owned();
    let task_id = id.clone();
    let profile_label = profile_name.to_owned();
    let workspace = scope.context.inner.root.clone();
    tokio::spawn(async move {
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
        };
        if let Ok(view) = registry_worker.finish(&task_id, status, code, error.as_deref()) {
            publish_completion(&inner, &session_id, &view, &profile_label);
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
}

async fn drive_child(
    registry: Arc<BackgroundRegistry>,
    id: String,
    profile: Profile,
    workspace: PathBuf,
    log_path: PathBuf,
    cancel: CancellationToken,
) -> ChildOutcome {
    let mut command = Command::new(&profile.program);
    command.args(&profile.args);
    for (key, value) in &profile.env {
        command.env(key, value);
    }
    command.current_dir(&workspace);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return ChildOutcome::Failed(format!("spawn failed: {error}")),
    };
    if let Some(pid) = child.id() {
        let _ = registry.set_pid(&id, pid);
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let max_log = registry.config.max_log_bytes;
    let registry_logs = registry.clone();
    let id_logs = id.clone();
    let log_for_pump = log_path.clone();
    let pump = tokio::spawn(async move {
        pump_output(
            registry_logs,
            id_logs,
            log_for_pump,
            stdout,
            stderr,
            max_log,
        )
        .await
    });
    let wait = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                ChildOutcome::Killed
            }
            status = child.wait() => match status {
                Ok(status) => ChildOutcome::Completed(status.code().unwrap_or(-1)),
                Err(error) => ChildOutcome::Failed(format!("wait failed: {error}")),
            }
        }
    });
    let outcome = wait
        .await
        .unwrap_or_else(|e| ChildOutcome::Failed(e.to_string()));
    let _ = pump.await;
    let _ = profile.timeout;
    outcome
}

async fn pump_output(
    registry: Arc<BackgroundRegistry>,
    id: String,
    log_path: PathBuf,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    max_log: usize,
) {
    let mut file = match OpenOptions::new().append(true).open(&log_path) {
        Ok(file) => file,
        Err(_) => return,
    };
    let mut offset: u64 = 0;
    let mut line: u64 = 1;
    let _ = registry.append_log_index(&id, 0, 1);
    let mut merge = |chunk: &[u8]| {
        if offset >= max_log as u64 {
            return;
        }
        let remain = max_log as u64 - offset;
        let take = chunk.len().min(remain as usize);
        if take == 0 {
            return;
        }
        let bytes = &chunk[..take];
        if file.write_all(bytes).is_err() {
            return;
        }
        let _ = file.flush();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                line += 1;
                let _ = registry.append_log_index(&id, offset + i as u64 + 1, line);
            }
        }
        offset += take as u64;
        let _ = registry.update_log_bytes(&id, offset);
    };
    let mut out = stdout.map(BufReader::new);
    let mut err = stderr.map(BufReader::new);
    let mut out_buf = [0u8; 4096];
    let mut err_buf = [0u8; 4096];
    loop {
        tokio::select! {
            result = async {
                match out.as_mut() {
                    Some(reader) => reader.read(&mut out_buf).await,
                    None => std::future::pending::<Result<usize, std::io::Error>>().await,
                }
            } => {
                match result {
                    Ok(0) | Err(_) => out = None,
                    Ok(n) => merge(&out_buf[..n]),
                }
            }
            result = async {
                match err.as_mut() {
                    Some(reader) => reader.read(&mut err_buf).await,
                    None => std::future::pending::<Result<usize, std::io::Error>>().await,
                }
            } => {
                match result {
                    Ok(0) | Err(_) => err = None,
                    Ok(n) => merge(&err_buf[..n]),
                }
            }
        }
        if out.is_none() && err.is_none() {
            break;
        }
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn hex32(seed: u8) -> String {
        format!("{seed:02x}").repeat(16)
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
            registry.append_log_index(&id, 0, 1).unwrap();
            registry.append_log_index(&id, 6, 2).unwrap();
            let view = registry.load(&id).unwrap().unwrap();
            assert_eq!(view.session_id, "sess-1");
            assert_eq!(view.status, BackgroundStatus::Running);
            let page = registry.read_logs(&id, 0, 64).unwrap();
            assert!(page.text.contains("hello"));
            assert_eq!(page.next_offset, 12);
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
    fn reopen_marks_orphaned_running_as_failed() {
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
        assert_eq!(view.status, BackgroundStatus::Failed);
        assert_eq!(view.error.as_deref(), Some("runtime_restarted"));
        assert_eq!(view.session_id, "sess-2");
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
