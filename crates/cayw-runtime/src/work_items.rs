//! Durable WorkItems, idempotent dispatch records, input states, proactive
//! rules and notification intents for the main session.
//!
//! WorkItems live in the main-session workspace database, next to the Core
//! session log. A dispatched workspace only records `work_id` and
//! `dispatch_key` on its own Task; it never stores the WorkItem body, a second
//! queue, a lease or another usage ledger.
//!
//! Acceptance can only move a WorkItem to `completed` after every criterion
//! has a passing record. Program-checkable criteria are evaluated by this
//! module against the actual files; a model report is
//! refused and never becomes a passing record.

use std::{
    path::{Component, Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::RuntimeError;

const MAX_TITLE_BYTES: usize = 1024;
const MAX_GOAL_BYTES: usize = 64 * 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn read_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn sql_u64(value: u64, field: &str) -> Result<i64, RuntimeError> {
    i64::try_from(value).map_err(|_| RuntimeError::InvalidInput(format!("{field} is too large")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkLifecycle {
    Open,
    Paused,
    Completed,
    Cancelled,
}

impl WorkLifecycle {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A check the program can run without a model. Paths are workspace-relative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AcceptanceCheck {
    PathExists { path: String },
    FileContains { path: String, text: String },
    FileEquals { path: String, text: String },
}

impl AcceptanceCheck {
    fn validate(&self) -> Result<(), RuntimeError> {
        match self {
            Self::PathExists { path }
            | Self::FileContains { path, .. }
            | Self::FileEquals { path, .. } => {
                relative_path(path).ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "acceptance check paths must be workspace-relative".into(),
                    )
                })?;
            }
        }
        Ok(())
    }

    /// Actually observe the workspace. A model cannot substitute its own text.
    pub fn evaluate(&self, workspace: &Path) -> Result<bool, RuntimeError> {
        let workspace = workspace
            .canonicalize()
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
        let root = cayw_host::ReadOnlyWorkspace::open(&workspace)
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
        self.evaluate_root(&root)
    }

    pub(crate) fn evaluate_root(
        &self,
        root: &cayw_host::ReadOnlyWorkspace,
    ) -> Result<bool, RuntimeError> {
        let result = match self {
            Self::PathExists { path } => root.exists(&relative(path)?),
            Self::FileContains { path, text } => root
                .open_file(&relative(path)?)
                .and_then(|file| match_check_file(file, text, false)),
            Self::FileEquals { path, text } => root
                .open_file(&relative(path)?)
                .and_then(|file| match_check_file(file, text, true)),
        };
        result.map_err(|error| RuntimeError::Workspace(format!("acceptance check: {error}")))
    }
}

fn relative_path(path: &str) -> Option<PathBuf> {
    let candidate = Path::new(path);
    if candidate.as_os_str().is_empty() || candidate.is_absolute() {
        return None;
    }
    candidate
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        .then(|| candidate.to_path_buf())
}

fn relative(path: &str) -> Result<PathBuf, RuntimeError> {
    relative_path(path).ok_or_else(|| {
        RuntimeError::InvalidInput("acceptance check paths must be workspace-relative".into())
    })
}

fn match_check_file(mut file: std::fs::File, text: &str, equals: bool) -> std::io::Result<bool> {
    use std::io::{Error, ErrorKind, Read};
    let pattern = text.as_bytes();
    let mut prefix = vec![0; pattern.len()];
    for i in 1..pattern.len() {
        let mut j = prefix[i - 1];
        while j > 0 && pattern[i] != pattern[j] {
            j = prefix[j - 1];
        }
        if pattern[i] == pattern[j] {
            j += 1;
        }
        prefix[i] = j;
    }
    let mut block = [0u8; 8192];
    let mut utf8 = Vec::with_capacity(block.len() + 3);
    let mut offset = 0usize;
    let mut matched = 0usize;
    let mut found = pattern.is_empty();
    let mut equal = true;
    loop {
        let count = match file.read(&mut block) {
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        utf8.extend_from_slice(&block[..count]);
        match std::str::from_utf8(&utf8) {
            Ok(_) => utf8.clear(),
            Err(error) if error.error_len().is_none() => {
                utf8.drain(..error.valid_up_to());
            }
            Err(error) => return Err(Error::new(ErrorKind::InvalidData, error)),
        }
        for byte in &block[..count] {
            if equals {
                equal &= pattern.get(offset) == Some(byte);
                offset = offset.saturating_add(1);
            } else if !found {
                while matched > 0 && pattern[matched] != *byte {
                    matched = prefix[matched - 1];
                }
                if pattern[matched] == *byte {
                    matched += 1;
                }
                found = matched == pattern.len();
            }
        }
    }
    if !utf8.is_empty() {
        return Err(Error::new(ErrorKind::InvalidData, "incomplete UTF-8"));
    }
    Ok(if equals {
        equal && offset == pattern.len()
    } else {
        found
    })
}

/// The text a dispatched session receives. It is built only from the durable
/// WorkItem and the caller's additional instructions, so the same key and the
/// same instructions still match the original task. Acceptance records are not
/// included: they change after a check and must not turn a retry into a conflict.
pub(crate) fn task_requirements(item: &WorkItem, instructions: &str) -> String {
    let mut text = String::new();
    text.push_str("Work item\n");
    text.push_str("title: ");
    text.push_str(&item.title);
    text.push('\n');
    text.push_str("work_id: ");
    text.push_str(&item.work_id);
    text.push('\n');
    text.push_str("goal: ");
    text.push_str(&item.goal);
    text.push('\n');
    text.push_str("acceptance criteria:\n");
    for criterion in &item.acceptance_criteria {
        text.push_str("- ");
        text.push_str(&criterion.criterion_id);
        text.push_str(": ");
        text.push_str(&criterion.description);
        text.push('\n');
        text.push_str(&program_check_line(&criterion.check));
        text.push('\n');
    }
    text.push_str(
        "A file in a subdirectory does not satisfy a check for a path at the workspace root.\n",
    );
    text.push_str(
        "Do this work in this workspace and review it here. Run each program check with work.accept.program. You decide whether to start a subagent.\n",
    );
    text.push_str("Additional instructions:\n");
    text.push_str(instructions);
    text
}

/// The text the main session relays when a WorkItem's own check time arrives.
/// It is the stored lifecycle, not a review and not a new dispatch.
pub(crate) fn due_report_text(item: &WorkItem) -> String {
    let mut text = String::new();
    text.push_str("Scheduled work check is due.\n");
    text.push_str("work_id: ");
    text.push_str(&item.work_id);
    text.push('\n');
    text.push_str("title: ");
    text.push_str(&item.title);
    text.push('\n');
    text.push_str("lifecycle: ");
    text.push_str(item.lifecycle.as_str());
    text.push('\n');
    text.push_str(
        "Tell the user this status. Do not read files, run checks, or dispatch another task.\n",
    );
    text
}

fn program_check_line(check: &Option<AcceptanceCheck>) -> String {
    match check {
        None => "The user judges this criterion. There is no program check.".to_owned(),
        Some(AcceptanceCheck::PathExists { path }) => {
            format!("Program check: the workspace-relative path `{path}` must exist.")
        }
        Some(AcceptanceCheck::FileContains { path, text }) => format!(
            "Program check: the workspace-relative file `{path}` must contain this text: {text}"
        ),
        Some(AcceptanceCheck::FileEquals { path, text }) => format!(
            "Program check: the workspace-relative file `{path}` must equal this text exactly: {text}"
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCriterion {
    pub criterion_id: String,
    pub description: String,
    /// `None` means the condition cannot be fixed as a program check. The main
    /// session asks the user and records the answer.
    #[serde(default)]
    pub check: Option<AcceptanceCheck>,
}

impl AcceptanceCriterion {
    fn validate(&self) -> Result<(), RuntimeError> {
        if self.criterion_id.trim().is_empty() || self.criterion_id.len() > 256 {
            return Err(RuntimeError::InvalidInput(
                "acceptance criterion_id must contain 1..256 bytes".into(),
            ));
        }
        if self.description.trim().is_empty() || self.description.len() > 4096 {
            return Err(RuntimeError::InvalidInput(
                "acceptance description must contain 1..4096 bytes".into(),
            ));
        }
        if let Some(check) = &self.check {
            check.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceResult {
    Passed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceSource {
    /// The program executed or observed the check itself.
    Program,
    /// The user answered the main-session question.
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceRecord {
    pub criterion_id: String,
    pub result: AcceptanceResult,
    pub evidence: Vec<String>,
    pub writer: String,
    pub source: AcceptanceSource,
    pub version: u64,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSessionRef {
    pub session_id: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkTaskRef {
    pub task_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LateWorkResult {
    pub task_id: String,
    pub result_ref: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub pause_version_at_dispatch: u64,
    pub stop_version_at_dispatch: u64,
    /// True only when the WorkItem is still open and its versions did not move.
    pub redispatch_allowed: bool,
    pub received_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub work_id: String,
    pub owner_main_session_id: String,
    pub title: String,
    pub goal: String,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub acceptance_records: Vec<AcceptanceRecord>,
    #[serde(default)]
    pub user_source_refs: Vec<String>,
    #[serde(default)]
    pub action_scope: Vec<String>,
    /// Audit/display only. It never authorizes a capability call.
    #[serde(default)]
    pub approval_refs: Vec<String>,
    #[serde(default)]
    pub session_refs: Vec<WorkSessionRef>,
    #[serde(default)]
    pub task_refs: Vec<WorkTaskRef>,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    pub lifecycle: WorkLifecycle,
    #[serde(default)]
    pub waiting_conditions: Vec<String>,
    #[serde(default)]
    pub pending_decisions: Vec<String>,
    #[serde(default)]
    pub next_check_at: Option<u64>,
    #[serde(default)]
    pub late_results: Vec<LateWorkResult>,
    pub work_version: u64,
    pub pause_version: u64,
    pub stop_version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

impl WorkItem {
    fn schedules_a_check(&self) -> bool {
        matches!(self.lifecycle, WorkLifecycle::Open | WorkLifecycle::Paused)
            && self.next_check_at.is_some()
    }

    fn check_is_due(&self, now: u64) -> bool {
        matches!(self.lifecycle, WorkLifecycle::Open | WorkLifecycle::Paused)
            && self.next_check_at.is_some_and(|due| due <= now)
    }

    /// Every criterion has a latest passing record. A running Task or an idle
    /// Session never satisfies this check by itself.
    pub fn acceptance_complete(&self) -> bool {
        !self.acceptance_criteria.is_empty()
            && self.acceptance_criteria.iter().all(|criterion| {
                self.acceptance_records
                    .iter()
                    .rev()
                    .find(|record| record.criterion_id == criterion.criterion_id)
                    .is_some_and(|record| record.result == AcceptanceResult::Passed)
            })
    }
}

/// Trusted creation input. Only the main session creates a WorkItem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItemSpec {
    #[serde(default)]
    pub work_id: Option<String>,
    pub owner_main_session_id: String,
    pub title: String,
    pub goal: String,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub user_source_refs: Vec<String>,
    #[serde(default)]
    pub action_scope: Vec<String>,
    #[serde(default)]
    pub approval_refs: Vec<String>,
    #[serde(default)]
    pub session_refs: Vec<WorkSessionRef>,
    #[serde(default)]
    pub task_refs: Vec<WorkTaskRef>,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub waiting_conditions: Vec<String>,
    #[serde(default)]
    pub pending_decisions: Vec<String>,
    #[serde(default)]
    pub next_check_at: Option<u64>,
}

impl WorkItemSpec {
    fn validate(&self) -> Result<(), RuntimeError> {
        if self.owner_main_session_id.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "owner_main_session_id is required".into(),
            ));
        }
        if self.title.trim().is_empty() || self.title.len() > MAX_TITLE_BYTES {
            return Err(RuntimeError::InvalidInput(
                "WorkItem title must contain 1..1024 bytes".into(),
            ));
        }
        if self.goal.trim().is_empty() || self.goal.len() > MAX_GOAL_BYTES {
            return Err(RuntimeError::InvalidInput(
                "WorkItem goal must contain 1..65536 bytes".into(),
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        for criterion in &self.acceptance_criteria {
            criterion.validate()?;
            if !ids.insert(criterion.criterion_id.as_str()) {
                return Err(RuntimeError::InvalidInput(
                    "acceptance criterion ids must be unique".into(),
                ));
            }
        }
        Ok(())
    }
}

/// The four append-only input states. `Included` means the input entered one
/// model request; it never means the input was handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputState {
    Received,
    Attached,
    Included,
    Handled,
}

impl InputState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Attached => "attached",
            Self::Included => "included",
            Self::Handled => "handled",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Received => 0,
            Self::Attached => 1,
            Self::Included => 2,
            Self::Handled => 3,
        }
    }

    fn parse(value: &str) -> Result<Self, RuntimeError> {
        match value {
            "received" => Ok(Self::Received),
            "attached" => Ok(Self::Attached),
            "included" => Ok(Self::Included),
            "handled" => Ok(Self::Handled),
            _ => Err(RuntimeError::InvalidInput(
                "stored input state is unknown".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputStateEvent {
    pub state: InputState,
    pub work_id: Option<String>,
    pub result_ref: Option<String>,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRecord {
    pub log_id: String,
    pub session_id: String,
    pub source: String,
    pub source_event_id: String,
    pub state: InputState,
    pub events: Vec<InputStateEvent>,
}

impl InputRecord {
    /// A duplicate source event produced no second `received` record.
    pub fn received_count(&self) -> usize {
        self.events
            .iter()
            .filter(|event| event.state == InputState::Received)
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProactiveMode {
    Off,
    Observe,
    Auto,
}

impl ProactiveMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Auto => "auto",
        }
    }

    fn parse(value: &str) -> Result<Self, RuntimeError> {
        match value {
            "off" => Ok(Self::Off),
            "observe" => Ok(Self::Observe),
            "auto" => Ok(Self::Auto),
            other => Err(RuntimeError::InvalidInput(format!(
                "invalid stored proactive mode: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProactiveStatus {
    pub mode: ProactiveMode,
    pub recorded_triggers: u64,
    pub total_usage_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProactiveTriggerOutcome {
    /// Proactive mode is off. The user's explicit work is unaffected.
    Ignored,
    Suggested {
        trigger_id: String,
        rule_id: String,
        condition_version: u64,
        work_id: String,
    },
    Dispatched {
        trigger_id: String,
        rule_id: String,
        condition_version: u64,
        work_id: String,
        dispatch_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    /// The same `(rule_id, condition_version)` already recorded usage and its
    /// dispatch intent. Concurrent triggers merge onto this one record.
    Duplicate {
        trigger_id: String,
        dispatch_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
}

impl ProactiveTriggerOutcome {
    pub fn with_task_id(mut self, resolved_task_id: Option<String>) -> Self {
        match &mut self {
            Self::Dispatched { task_id, .. } | Self::Duplicate { task_id, .. } => {
                if resolved_task_id.is_some() {
                    *task_id = resolved_task_id;
                }
            }
            Self::Ignored | Self::Suggested { .. } => {}
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    Pending,
    Queued,
    Accepted,
    Sent,
    Delivered,
    Read,
    Unknown,
    Failed,
}

impl NotificationStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Queued => "queued",
            Self::Accepted => "accepted",
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Read => "read",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, RuntimeError> {
        match value {
            "pending" => Ok(Self::Pending),
            "queued" => Ok(Self::Queued),
            "accepted" => Ok(Self::Accepted),
            "sent" => Ok(Self::Sent),
            "delivered" => Ok(Self::Delivered),
            "read" => Ok(Self::Read),
            "unknown" => Ok(Self::Unknown),
            "failed" => Ok(Self::Failed),
            _ => Err(RuntimeError::InvalidInput(
                "stored notification status is unknown".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub owner_session_id: Option<String>,
    pub notification_id: String,
    pub work_id: String,
    pub work_version: u64,
    pub target: String,
    pub content_ref: Option<String>,
    pub artifact_refs: Vec<String>,
    pub status: NotificationStatus,
    pub receipt_ref: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

pub(crate) enum NotificationEnqueue {
    Existing(Notification),
    Ready {
        notification: Notification,
        envelope: serde_json::Value,
    },
}

/// What the sender may do next. An unknown result never means "resend".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationDecision {
    /// The intent is saved but no send attempt is recorded.
    Send,
    /// Written to the local outbox; awaiting an external channel to deliver and report a receipt.
    AwaitDelivery,
    /// The previous attempt's outcome is unknown. Query the receipt first.
    QueryReceipt,
    /// A real service status already arrived.
    Complete,
}

/// Durable store for WorkItems and their supporting records.
pub struct WorkItems {
    connection: Mutex<Connection>,
}

impl WorkItems {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS runtime_work_items (
                work_id TEXT PRIMARY KEY,
                lifecycle TEXT NOT NULL,
                work_version INTEGER NOT NULL,
                pause_version INTEGER NOT NULL,
                stop_version INTEGER NOT NULL,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS runtime_work_inputs (
                source TEXT NOT NULL,
                source_event_id TEXT NOT NULL,
                log_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                PRIMARY KEY(source, source_event_id)
             );
             CREATE TABLE IF NOT EXISTS runtime_work_input_events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                log_id TEXT NOT NULL,
                state TEXT NOT NULL,
                work_id TEXT,
                result_ref TEXT,
                at INTEGER NOT NULL,
                UNIQUE(log_id, state)
             );
             CREATE INDEX IF NOT EXISTS runtime_work_input_log
                ON runtime_work_input_events(log_id);
             CREATE UNIQUE INDEX IF NOT EXISTS runtime_work_input_identity
                ON runtime_work_inputs(log_id);
             CREATE TABLE IF NOT EXISTS runtime_proactive_triggers (
                rule_id TEXT NOT NULL,
                condition_version INTEGER NOT NULL,
                work_id TEXT NOT NULL,
                mode TEXT NOT NULL,
                action TEXT NOT NULL,
                dispatch_key TEXT,
                usage_tokens INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY(rule_id, condition_version)
             );
             CREATE TABLE IF NOT EXISTS runtime_notifications (
                notification_id TEXT PRIMARY KEY,
                owner_session_id TEXT,
                envelope TEXT,
                work_id TEXT NOT NULL,
                work_version INTEGER NOT NULL,
                target TEXT NOT NULL,
                content_ref TEXT,
                artifact_refs TEXT NOT NULL,
                status TEXT NOT NULL,
                receipt_ref TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS runtime_work_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn create(&self, spec: WorkItemSpec) -> Result<WorkItem, RuntimeError> {
        self.create_at(spec, now_ms())
    }

    pub fn create_at(&self, spec: WorkItemSpec, at: u64) -> Result<WorkItem, RuntimeError> {
        spec.validate()?;
        let work_id = spec
            .work_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if work_id.trim().is_empty() || work_id.len() > 256 {
            return Err(RuntimeError::InvalidInput(
                "work_id must contain 1..256 bytes".into(),
            ));
        }
        let item = WorkItem {
            work_id,
            owner_main_session_id: spec.owner_main_session_id,
            title: spec.title,
            goal: spec.goal,
            acceptance_criteria: spec.acceptance_criteria,
            acceptance_records: Vec::new(),
            user_source_refs: spec.user_source_refs,
            action_scope: spec.action_scope,
            approval_refs: spec.approval_refs,
            session_refs: spec.session_refs,
            task_refs: spec.task_refs,
            artifact_refs: spec.artifact_refs,
            lifecycle: WorkLifecycle::Open,
            waiting_conditions: spec.waiting_conditions,
            pending_decisions: spec.pending_decisions,
            next_check_at: spec.next_check_at,
            late_results: Vec::new(),
            work_version: 1,
            pause_version: 0,
            stop_version: 0,
            created_at: at,
            updated_at: at,
        };
        let body = serde_json::to_string(&item)?;
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_work_items WHERE work_id=?1)",
            [&item.work_id],
            |row| row.get(0),
        )?;
        if exists {
            return Err(RuntimeError::InvalidInput("WorkItem already exists".into()));
        }
        tx.execute(
            "INSERT INTO runtime_work_items(work_id,lifecycle,work_version,pause_version,stop_version,body,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                item.work_id,
                item.lifecycle.as_str(),
                sql_u64(item.work_version, "work_version")?,
                sql_u64(item.pause_version, "pause_version")?,
                sql_u64(item.stop_version, "stop_version")?,
                body,
                sql_u64(item.created_at, "created_at")?,
                sql_u64(item.updated_at, "updated_at")?
            ],
        )?;
        tx.commit()?;
        Ok(item)
    }

    pub fn get(&self, work_id: &str) -> Result<Option<WorkItem>, RuntimeError> {
        load(&lock(&self.connection), work_id)
    }

    pub fn list(&self) -> Result<Vec<WorkItem>, RuntimeError> {
        let db = lock(&self.connection);
        let mut statement =
            db.prepare("SELECT body FROM runtime_work_items ORDER BY created_at, work_id")?;
        let bodies = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        bodies
            .into_iter()
            .map(|body| serde_json::from_str(&body).map_err(RuntimeError::Json))
            .collect()
    }

    /// Open or paused WorkItems whose own check time has arrived.
    /// Completed and cancelled items are not woken. Stopping a work item
    /// cancels its due check by leaving it out of this list.
    pub fn due_items(&self, now: u64) -> Result<Vec<WorkItem>, RuntimeError> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|item| item.check_is_due(now))
            .collect())
    }

    /// The soonest check time still scheduled on an open or paused WorkItem.
    pub fn earliest_open_check(&self) -> Result<Option<u64>, RuntimeError> {
        self.earliest_open_check_except(&std::collections::BTreeSet::new())
    }

    /// The soonest check whose owning session is not in `busy`. A busy session
    /// keeps its own time; it does not hide another session's later check.
    pub fn earliest_open_check_except(
        &self,
        busy: &std::collections::BTreeSet<String>,
    ) -> Result<Option<u64>, RuntimeError> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|item| item.schedules_a_check())
            .filter(|item| !busy.contains(&item.owner_main_session_id))
            .filter_map(|item| item.next_check_at)
            .min())
    }

    /// Clear one check time after that report was accepted. A newer time, or a
    /// WorkItem that is no longer open or paused, is left as it is.
    pub fn consume_due_check(
        &self,
        work_id: &str,
        expected: u64,
        at: u64,
    ) -> Result<bool, RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(mut item) = load(&tx, work_id)? else {
            return Ok(false);
        };
        if item.next_check_at != Some(expected) || !item.schedules_a_check() {
            return Ok(false);
        }
        item.next_check_at = None;
        item.work_version = item.work_version.saturating_add(1);
        item.updated_at = at;
        save(&tx, &item, at)?;
        tx.commit()?;
        Ok(true)
    }

    /// Record the exact Task created for this WorkItem. The target workspace
    /// keeps only `work_id`/`dispatch_key`; the owner keeps this reference so
    /// later query, pause and stop reach the right workspace.
    pub fn attach_task_ref(
        &self,
        work_id: &str,
        task: WorkTaskRef,
    ) -> Result<WorkItem, RuntimeError> {
        if task.task_id.trim().is_empty() || task.task_id.len() > 256 {
            return Err(RuntimeError::InvalidInput(
                "task reference needs 1..256 bytes".into(),
            ));
        }
        if task
            .session_id
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
            || task
                .workspace
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
        {
            return Err(RuntimeError::InvalidInput(
                "task reference session and workspace cannot be empty".into(),
            ));
        }
        self.transition_work(work_id, now_ms(), |item| {
            if item
                .task_refs
                .iter()
                .any(|existing| existing.task_id == task.task_id)
            {
                return Ok(());
            }
            item.task_refs.push(task);
            Ok(())
        })
    }

    /// Record the exact session a WorkItem is associated with.
    pub fn attach_session_ref(
        &self,
        work_id: &str,
        session: WorkSessionRef,
    ) -> Result<WorkItem, RuntimeError> {
        if session.session_id.trim().is_empty() || session.session_id.len() > 256 {
            return Err(RuntimeError::InvalidInput(
                "session reference needs 1..256 bytes".into(),
            ));
        }
        if session
            .workspace
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(RuntimeError::InvalidInput(
                "session reference workspace cannot be empty".into(),
            ));
        }
        self.transition_work(work_id, now_ms(), |item| {
            if item
                .session_refs
                .iter()
                .any(|existing| existing.session_id == session.session_id)
            {
                return Ok(());
            }
            item.session_refs.push(session);
            Ok(())
        })
    }

    /// Run the fixed check for one criterion and append the observed result.
    pub fn evaluate_acceptance(
        &self,
        work_id: &str,
        criterion_id: &str,
        workspace: &Path,
    ) -> Result<WorkItem, RuntimeError> {
        self.evaluate_acceptance_at(work_id, criterion_id, workspace, "program", now_ms())
    }

    pub fn evaluate_acceptance_at(
        &self,
        work_id: &str,
        criterion_id: &str,
        workspace: &Path,
        writer: &str,
        at: u64,
    ) -> Result<WorkItem, RuntimeError> {
        let workspace = workspace
            .canonicalize()
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
        let root = cayw_host::ReadOnlyWorkspace::open(&workspace)
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
        self.evaluate_acceptance_root_at(work_id, criterion_id, &root, writer, at)
    }

    pub(crate) fn evaluate_acceptance_root_at(
        &self,
        work_id: &str,
        criterion_id: &str,
        root: &cayw_host::ReadOnlyWorkspace,
        writer: &str,
        at: u64,
    ) -> Result<WorkItem, RuntimeError> {
        // Observe the check before taking the write transaction. Running an
        // external program while holding a SQLite IMMEDIATE lock would block
        // unrelated durable writes for its whole duration.
        let check = {
            let item = load(&lock(&self.connection), work_id)?
                .ok_or_else(|| RuntimeError::InvalidInput("WorkItem not found".into()))?;
            if !matches!(item.lifecycle, WorkLifecycle::Open | WorkLifecycle::Paused) {
                return Err(RuntimeError::InvalidInput(
                    "WorkItem is no longer open for acceptance records".into(),
                ));
            }
            item.acceptance_criteria
                .iter()
                .find(|criterion| criterion.criterion_id == criterion_id)
                .ok_or_else(|| RuntimeError::InvalidInput("unknown acceptance criterion".into()))?
                .check
                .clone()
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "this acceptance criterion has no program check; ask the user".into(),
                    )
                })?
        };
        // The observable result comes from the check, never from caller text.
        let passed = check.evaluate_root(root)?;
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut item = load(&tx, work_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("WorkItem not found".into()))?;
        if !matches!(item.lifecycle, WorkLifecycle::Open | WorkLifecycle::Paused) {
            return Err(RuntimeError::InvalidInput(
                "WorkItem is no longer open for acceptance records".into(),
            ));
        }
        append_record(
            &mut item,
            criterion_id,
            if passed {
                AcceptanceResult::Passed
            } else {
                AcceptanceResult::Failed
            },
            vec![format!("program check: {check:?}")],
            writer,
            AcceptanceSource::Program,
            at,
        )?;
        save(&tx, &item, at)?;
        tx.commit()?;
        Ok(item)
    }

    /// Record the user's own answer to a non-program condition. The answer
    /// reference is the main-session log id or question id; a model statement
    /// cannot call this path.
    pub fn record_user_acceptance(
        &self,
        work_id: &str,
        criterion_id: &str,
        passed: bool,
        answer_ref: &str,
    ) -> Result<WorkItem, RuntimeError> {
        self.record_user_acceptance_at(work_id, criterion_id, passed, answer_ref, "user", now_ms())
    }

    pub fn record_user_acceptance_at(
        &self,
        work_id: &str,
        criterion_id: &str,
        passed: bool,
        answer_ref: &str,
        writer: &str,
        at: u64,
    ) -> Result<WorkItem, RuntimeError> {
        if answer_ref.trim().is_empty() || answer_ref.len() > 1024 {
            return Err(RuntimeError::InvalidInput(
                "a user acceptance record needs an answer reference".into(),
            ));
        }
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut item = load(&tx, work_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("WorkItem not found".into()))?;
        if !matches!(item.lifecycle, WorkLifecycle::Open | WorkLifecycle::Paused) {
            return Err(RuntimeError::InvalidInput(
                "WorkItem is no longer open for acceptance records".into(),
            ));
        }
        if !item
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.criterion_id == criterion_id)
        {
            return Err(RuntimeError::InvalidInput(
                "unknown acceptance criterion".into(),
            ));
        }
        append_record(
            &mut item,
            criterion_id,
            if passed {
                AcceptanceResult::Passed
            } else {
                AcceptanceResult::Failed
            },
            vec![answer_ref.to_owned()],
            writer,
            AcceptanceSource::User,
            at,
        )?;
        save(&tx, &item, at)?;
        tx.commit()?;
        Ok(item)
    }

    /// Append one already-observed program result. The caller must have run the
    /// fixed check itself; this method never accepts caller text as a pass.
    pub fn record_program_acceptance_at(
        &self,
        work_id: &str,
        criterion_id: &str,
        passed: bool,
        evidence: Vec<String>,
        writer: &str,
        at: u64,
    ) -> Result<WorkItem, RuntimeError> {
        if writer.trim().is_empty() || writer.len() > 1024 {
            return Err(RuntimeError::InvalidInput(
                "a program acceptance record needs its observer".into(),
            ));
        }
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut item = load(&tx, work_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("WorkItem not found".into()))?;
        if !matches!(item.lifecycle, WorkLifecycle::Open | WorkLifecycle::Paused) {
            return Err(RuntimeError::InvalidInput(
                "WorkItem is no longer open for acceptance records".into(),
            ));
        }
        if !item
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.criterion_id == criterion_id)
        {
            return Err(RuntimeError::InvalidInput(
                "unknown acceptance criterion".into(),
            ));
        }
        append_record(
            &mut item,
            criterion_id,
            if passed {
                AcceptanceResult::Passed
            } else {
                AcceptanceResult::Failed
            },
            evidence,
            writer,
            AcceptanceSource::Program,
            at,
        )?;
        save(&tx, &item, at)?;
        tx.commit()?;
        Ok(item)
    }

    /// A model or executing session saying "done" is only a hint. It never
    /// creates a passing acceptance record.
    pub fn record_model_report(&self, _work_id: &str, _text: &str) -> Result<(), RuntimeError> {
        Err(RuntimeError::InvalidInput(
            "model completion reports cannot write acceptance results".into(),
        ))
    }

    /// Pause stops new dispatches and increments the version written into
    /// already-dispatched Tasks.
    pub fn pause(&self, work_id: &str) -> Result<WorkItem, RuntimeError> {
        self.transition_work(work_id, now_ms(), |item| {
            if item.lifecycle != WorkLifecycle::Open {
                return Err(RuntimeError::InvalidInput(
                    "only an open WorkItem can be paused".into(),
                ));
            }
            item.pause_version = item.pause_version.saturating_add(1);
            item.lifecycle = WorkLifecycle::Paused;
            Ok(())
        })
    }

    pub fn resume(&self, work_id: &str) -> Result<WorkItem, RuntimeError> {
        self.transition_work(work_id, now_ms(), |item| {
            if item.lifecycle != WorkLifecycle::Paused {
                return Err(RuntimeError::InvalidInput(
                    "only a paused WorkItem can be resumed".into(),
                ));
            }
            item.lifecycle = WorkLifecycle::Open;
            Ok(())
        })
    }

    /// Stop is terminal for the current WorkItem. A later user decision creates
    /// a new version rather than silently reopening this one.
    pub fn stop(&self, work_id: &str) -> Result<WorkItem, RuntimeError> {
        self.transition_work(work_id, now_ms(), |item| {
            if matches!(
                item.lifecycle,
                WorkLifecycle::Completed | WorkLifecycle::Cancelled
            ) {
                return Err(RuntimeError::InvalidInput(
                    "WorkItem is already closed".into(),
                ));
            }
            item.stop_version = item.stop_version.saturating_add(1);
            item.lifecycle = WorkLifecycle::Cancelled;
            Ok(())
        })
    }

    fn transition_work(
        &self,
        work_id: &str,
        at: u64,
        change: impl FnOnce(&mut WorkItem) -> Result<(), RuntimeError>,
    ) -> Result<WorkItem, RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut item = load(&tx, work_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("WorkItem not found".into()))?;
        change(&mut item)?;
        item.work_version = item.work_version.saturating_add(1);
        item.updated_at = at;
        save(&tx, &item, at)?;
        tx.commit()?;
        Ok(item)
    }

    /// A result that arrives after pause/stop is durable evidence, but it does
    /// not reopen the WorkItem and does not trigger a second dispatch.
    pub fn record_late_result(
        &self,
        work_id: &str,
        task_id: &str,
        result_ref: &str,
        pause_version_at_dispatch: u64,
        stop_version_at_dispatch: u64,
    ) -> Result<LateWorkResult, RuntimeError> {
        if task_id.trim().is_empty() || result_ref.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "a late result needs a task id and a result reference".into(),
            ));
        }
        let at = now_ms();
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut item = load(&tx, work_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("WorkItem not found".into()))?;
        let redispatch_allowed = item.lifecycle == WorkLifecycle::Open
            && item.pause_version == pause_version_at_dispatch
            && item.stop_version == stop_version_at_dispatch;
        let late = LateWorkResult {
            task_id: task_id.into(),
            result_ref: result_ref.into(),
            evidence: Vec::new(),
            pause_version_at_dispatch,
            stop_version_at_dispatch,
            redispatch_allowed,
            received_at: at,
        };
        item.late_results.push(late.clone());
        item.work_version = item.work_version.saturating_add(1);
        item.updated_at = at;
        save(&tx, &item, at)?;
        tx.commit()?;
        Ok(late)
    }

    /// Persist one input identity. Repeated delivery of the same
    /// `(source, source_event_id)` returns the original record and appends no
    /// second `received` state. State changes are appended as events.
    pub fn ingest_input(
        &self,
        source: &str,
        source_event_id: &str,
        log_id: &str,
        session_id: &str,
    ) -> Result<InputRecord, RuntimeError> {
        if source.trim().is_empty() || source_event_id.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "input source identity is required".into(),
            ));
        }
        if log_id.trim().is_empty() || log_id.len() > 1024 || session_id.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "input log and session identity are required".into(),
            ));
        }
        let at = now_ms();
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Input identity is the original log record. A second delivery of the
        // same log, or of the same source event, returns the original record.
        let existing_log: Option<String> = tx
            .query_row(
                "SELECT log_id FROM runtime_work_inputs WHERE log_id=?1",
                [log_id],
                |row| row.get(0),
            )
            .optional()?;
        if existing_log.is_none() {
            let bound: Option<String> = tx
                .query_row(
                    "SELECT log_id FROM runtime_work_inputs WHERE source=?1 AND source_event_id=?2",
                    params![source, source_event_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(bound) = bound
                && bound != log_id
            {
                return Err(RuntimeError::InvalidInput(
                    "input source identity is already bound to another log record".into(),
                ));
            }
            tx.execute(
                "INSERT INTO runtime_work_inputs(source,source_event_id,log_id,session_id) VALUES (?1,?2,?3,?4)",
                params![source, source_event_id, log_id, session_id],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO runtime_work_input_events(log_id,state,work_id,result_ref,at) VALUES (?1,'received',NULL,NULL,?2)",
                params![log_id, sql_u64(at, "input time")?],
            )?;
        }
        let record = input_record(&tx, log_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("input record was not persisted".into()))?;
        tx.commit()?;
        Ok(record)
    }

    /// Append one state event. Included is a separate state from handled.
    pub fn transition_input(
        &self,
        log_id: &str,
        state: InputState,
        work_id: Option<&str>,
        result_ref: Option<&str>,
    ) -> Result<InputRecord, RuntimeError> {
        if state == InputState::Received {
            return Err(RuntimeError::InvalidInput(
                "received is recorded by ingest_input".into(),
            ));
        }
        if state == InputState::Handled && result_ref.is_none_or(|value| value.trim().is_empty()) {
            return Err(RuntimeError::InvalidInput(
                "handled input needs a result or hand-off reference".into(),
            ));
        }
        if work_id.is_some_and(|value| value.trim().is_empty()) {
            return Err(RuntimeError::InvalidInput(
                "attached work_id cannot be empty".into(),
            ));
        }
        let at = now_ms();
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = input_record(&tx, log_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("input was not received".into()))?;
        if record.state.rank() > state.rank() {
            return Err(RuntimeError::InvalidInput(
                "input state cannot move backwards".into(),
            ));
        }
        tx.execute(
            "INSERT OR IGNORE INTO runtime_work_input_events(log_id,state,work_id,result_ref,at) VALUES (?1,?2,?3,?4,?5)",
            params![
                log_id,
                state.as_str(),
                work_id,
                result_ref,
                sql_u64(at, "input time")?
            ],
        )?;
        let record = input_record(&tx, log_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("input record was not persisted".into()))?;
        tx.commit()?;
        Ok(record)
    }

    pub fn input(&self, log_id: &str) -> Result<Option<InputRecord>, RuntimeError> {
        input_record(&lock(&self.connection), log_id)
    }

    /// Record one proactive trigger and its usage in the same transaction. The
    /// unique `(rule_id, condition_version)` merges concurrent triggers; there
    /// is no usage cap and no second ledger.
    pub fn trigger_proactive(
        &self,
        mode: ProactiveMode,
        rule_id: &str,
        condition_version: u64,
        work_id: &str,
        usage_tokens: u64,
    ) -> Result<ProactiveTriggerOutcome, RuntimeError> {
        if rule_id.trim().is_empty() || work_id.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "proactive rule and work ids are required".into(),
            ));
        }
        if mode == ProactiveMode::Off {
            return Ok(ProactiveTriggerOutcome::Ignored);
        }
        let trigger_id = format!("{rule_id}:{condition_version}");
        let at = now_ms();
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT mode,dispatch_key FROM runtime_proactive_triggers WHERE rule_id=?1 AND condition_version=?2",
                params![rule_id, sql_u64(condition_version, "condition_version")?],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((_mode, dispatch_key)) = existing {
            tx.commit()?;
            return Ok(ProactiveTriggerOutcome::Duplicate {
                trigger_id,
                dispatch_key,
                task_id: None,
            });
        }
        let lifecycle: Option<String> = tx
            .query_row(
                "SELECT lifecycle FROM runtime_work_items WHERE work_id=?1",
                [work_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(lifecycle) = lifecycle else {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown work item: {work_id}"
            )));
        };
        if mode == ProactiveMode::Auto && lifecycle != "open" {
            return Err(RuntimeError::InvalidInput(format!(
                "cannot dispatch proactive trigger while work item {work_id} is {lifecycle}"
            )));
        }
        let (mode_text, action, dispatch_key) = match mode {
            ProactiveMode::Off => unreachable!("returned above"),
            ProactiveMode::Observe => ("observe", "suggest", None),
            ProactiveMode::Auto => (
                "auto",
                "dispatch",
                Some(format!("{rule_id}:{condition_version}:{work_id}")),
            ),
        };
        tx.execute(
            "INSERT INTO runtime_proactive_triggers(rule_id,condition_version,work_id,mode,action,dispatch_key,usage_tokens,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                rule_id,
                sql_u64(condition_version, "condition_version")?,
                work_id,
                mode_text,
                action,
                dispatch_key,
                sql_u64(usage_tokens, "usage_tokens")?,
                sql_u64(at, "trigger time")?
            ],
        )?;
        tx.commit()?;
        Ok(match (mode, dispatch_key) {
            (ProactiveMode::Observe, _) => ProactiveTriggerOutcome::Suggested {
                trigger_id,
                rule_id: rule_id.into(),
                condition_version,
                work_id: work_id.into(),
            },
            (ProactiveMode::Auto, Some(key)) => ProactiveTriggerOutcome::Dispatched {
                trigger_id,
                rule_id: rule_id.into(),
                condition_version,
                work_id: work_id.into(),
                dispatch_key: key,
                task_id: None,
            },
            _ => unreachable!("only observe and auto reach this branch"),
        })
    }

    pub fn configured_proactive_mode(
        &self,
        default: ProactiveMode,
    ) -> Result<ProactiveMode, RuntimeError> {
        let db = lock(&self.connection);
        Self::read_proactive_mode(&db, default)
    }

    pub fn set_proactive_mode(&self, mode: ProactiveMode) -> Result<ProactiveStatus, RuntimeError> {
        let at = now_ms();
        let db = lock(&self.connection);
        db.execute(
            "INSERT INTO runtime_work_settings(key,value,updated_at) VALUES ('proactive_mode',?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            params![mode.as_str(), sql_u64(at, "proactive setting time")?],
        )?;
        Self::read_proactive_status(&db, mode)
    }

    pub fn proactive_status(
        &self,
        default: ProactiveMode,
    ) -> Result<ProactiveStatus, RuntimeError> {
        let db = lock(&self.connection);
        let mode = Self::read_proactive_mode(&db, default)?;
        Self::read_proactive_status(&db, mode)
    }

    fn read_proactive_mode(
        db: &Connection,
        default: ProactiveMode,
    ) -> Result<ProactiveMode, RuntimeError> {
        let raw: Option<String> = db
            .query_row(
                "SELECT value FROM runtime_work_settings WHERE key='proactive_mode'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            Some(value) => ProactiveMode::parse(&value),
            None => Ok(default),
        }
    }

    fn read_proactive_status(
        db: &Connection,
        mode: ProactiveMode,
    ) -> Result<ProactiveStatus, RuntimeError> {
        let mut stmt = db.prepare("SELECT usage_tokens FROM runtime_proactive_triggers")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        let mut recorded_triggers = 0_u64;
        let mut total_usage_tokens = 0_u64;
        for row in rows {
            let tokens = u64::try_from(row?).map_err(|_| {
                RuntimeError::InvalidInput("stored proactive usage_tokens is negative".into())
            })?;
            recorded_triggers = recorded_triggers.saturating_add(1);
            total_usage_tokens = total_usage_tokens.saturating_add(tokens);
        }
        Ok(ProactiveStatus {
            mode,
            recorded_triggers,
            total_usage_tokens,
        })
    }

    /// Look up the original `(rule_id, condition_version, work_id)` recorded for a
    /// proactive `dispatch_key` so duplicate triggers can recover unfinished dispatches.
    pub fn proactive_trigger_by_dispatch_key(
        &self,
        dispatch_key: &str,
    ) -> Result<Option<(String, u64, String)>, RuntimeError> {
        let db = lock(&self.connection);
        let row: Option<(String, i64, String)> = db
            .query_row(
                "SELECT rule_id,condition_version,work_id FROM runtime_proactive_triggers WHERE dispatch_key=?1",
                [dispatch_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        row.map(|(rule_id, version, work_id)| {
            let condition_version = u64::try_from(version).map_err(|_| {
                RuntimeError::InvalidInput("stored condition_version is negative".into())
            })?;
            Ok((rule_id, condition_version, work_id))
        })
        .transpose()
    }

    /// Save the send intent before any external action. The target must come
    /// from user configuration; a model cannot add one here.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_notification(
        &self,
        work_id: &str,
        work_version: u64,
        target: &str,
        content_ref: Option<&str>,
        artifact_refs: &[String],
        user_targets: &[String],
        owner_session_id: Option<&str>,
    ) -> Result<Notification, RuntimeError> {
        if !user_targets.iter().any(|allowed| allowed == target) {
            return Err(RuntimeError::InvalidInput(
                "notification target is not in the user configuration".into(),
            ));
        }
        let at = now_ms();
        let notification = Notification {
            owner_session_id: owner_session_id.map(str::to_owned),
            notification_id: uuid::Uuid::new_v4().to_string(),
            work_id: work_id.into(),
            work_version,
            target: target.into(),
            content_ref: content_ref.map(str::to_owned),
            artifact_refs: artifact_refs.to_vec(),
            status: NotificationStatus::Pending,
            receipt_ref: None,
            created_at: at,
            updated_at: at,
        };
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO runtime_notifications(notification_id,work_id,work_version,target,content_ref,artifact_refs,status,receipt_ref,created_at,updated_at,owner_session_id) VALUES (?1,?2,?3,?4,?5,?6,'pending',NULL,?7,?8,?9)",
            params![
                notification.notification_id,
                notification.work_id,
                sql_u64(notification.work_version, "work_version")?,
                notification.target,
                notification.content_ref,
                serde_json::to_string(&notification.artifact_refs)?,
                sql_u64(at, "notification time")?,
                sql_u64(at, "notification time")?,
                notification.owner_session_id,
            ],
        )?;
        tx.commit()?;
        Ok(notification)
    }

    pub fn notification(
        &self,
        notification_id: &str,
    ) -> Result<Option<Notification>, RuntimeError> {
        load_notification(&lock(&self.connection), notification_id)
    }

    pub fn notifications_for_work(&self, work_id: &str) -> Result<Vec<Notification>, RuntimeError> {
        let db = lock(&self.connection);
        let mut statement = db.prepare(
            "SELECT notification_id,work_id,work_version,target,content_ref,artifact_refs,status,receipt_ref,created_at,updated_at,owner_session_id FROM runtime_notifications WHERE work_id=?1 ORDER BY created_at",
        )?;
        let rows = statement
            .query_map([work_id], notification_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(notification_from_row).collect()
    }

    /// Commit one writer's ownership before touching the external JSONL outbox.
    /// A crash after this point leaves this notification Unknown for host verification.
    pub(crate) fn claim_notification_enqueue(
        &self,
        id: &str,
        text: Option<&str>,
    ) -> Result<NotificationEnqueue, RuntimeError> {
        self.notification_enqueue(id, text, true)
    }

    /// Fix the payload while waiting without claiming that any write may have begun.
    pub(crate) fn prepare_notification_enqueue(
        &self,
        id: &str,
        text: Option<&str>,
    ) -> Result<NotificationEnqueue, RuntimeError> {
        self.notification_enqueue(id, text, false)
    }

    fn notification_enqueue(
        &self,
        id: &str,
        text: Option<&str>,
        claim: bool,
    ) -> Result<NotificationEnqueue, RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let notification = load_notification(&tx, id)?
            .ok_or_else(|| RuntimeError::InvalidInput("notification not found".into()))?;
        let saved: Option<String> = tx.query_row(
            "SELECT envelope FROM runtime_notifications WHERE notification_id=?1",
            [id],
            |row| row.get(0),
        )?;
        let envelope = match saved {
            Some(saved) => {
                let envelope: serde_json::Value = serde_json::from_str(&saved)?;
                if text.is_some_and(|text| envelope["text"].as_str() != Some(text)) {
                    return Err(RuntimeError::InvalidInput("notification payload is already fixed; create a new intent for different text".into()));
                }
                envelope
            }
            None => serde_json::json!({
                "notification_id": notification.notification_id,
                "work_id": notification.work_id,
                "work_version": notification.work_version,
                "target": notification.target,
                "text": text.or(notification.content_ref.as_deref()).unwrap_or(""),
                "content_ref": notification.content_ref,
                "artifact_refs": notification.artifact_refs,
                "status": "queued",
            }),
        };
        match notification.status {
            NotificationStatus::Pending => {
                tx.execute(
                    "UPDATE runtime_notifications SET status=?4,envelope=?2,updated_at=?3 WHERE notification_id=?1 AND status='pending'",
                    params![id, serde_json::to_string(&envelope)?, sql_u64(now_ms(), "notification time")?, if claim { "unknown" } else { "pending" }],
                )?;
                tx.commit()?;
                Ok(NotificationEnqueue::Ready {
                    notification,
                    envelope,
                })
            }
            NotificationStatus::Unknown => Err(RuntimeError::InvalidInput(format!(
                "notification {id} enqueue or delivery outcome is unknown; verify the outbox or query a real receipt before resending"
            ))),
            _ => Ok(NotificationEnqueue::Existing(notification)),
        }
    }

    /// Only the writer that committed the exact envelope may settle its local enqueue.
    /// A concurrently recorded real receipt takes precedence over this local observation.
    pub(crate) fn settle_notification_enqueue(
        &self,
        id: &str,
        envelope: &serde_json::Value,
        written: bool,
    ) -> Result<Notification, RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE runtime_notifications SET status=?3,updated_at=?4 WHERE notification_id=?1 AND envelope=?2 AND status='unknown' AND receipt_ref IS NULL",
            params![id, serde_json::to_string(envelope)?, if written { "queued" } else { "pending" }, sql_u64(now_ms(), "notification time")?],
        )?;
        let notification = load_notification(&tx, id)?
            .ok_or_else(|| RuntimeError::InvalidInput("notification not found".into()))?;
        tx.commit()?;
        Ok(notification)
    }

    /// Record that the notification envelope has been persisted to the local
    /// outbox and is waiting for an external delivery channel to report a receipt.
    pub fn mark_notification_queued(
        &self,
        notification_id: &str,
    ) -> Result<Notification, RuntimeError> {
        self.set_notification(notification_id, NotificationStatus::Queued, None, true)
    }

    /// A send attempt whose outcome is unknown. The next step is a receipt
    /// query, never a blind resend.
    pub fn mark_notification_unknown(
        &self,
        notification_id: &str,
    ) -> Result<Notification, RuntimeError> {
        self.set_notification(notification_id, NotificationStatus::Unknown, None, false)
    }

    pub fn record_notification_receipt(
        &self,
        notification_id: &str,
        status: NotificationStatus,
        receipt_ref: &str,
    ) -> Result<Notification, RuntimeError> {
        if matches!(
            status,
            NotificationStatus::Pending | NotificationStatus::Queued | NotificationStatus::Unknown
        ) {
            return Err(RuntimeError::InvalidInput(
                "a receipt needs a real service status".into(),
            ));
        }
        if receipt_ref.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "a notification receipt needs its reference".into(),
            ));
        }
        self.set_notification(notification_id, status, Some(receipt_ref.to_owned()), true)
    }

    fn set_notification(
        &self,
        notification_id: &str,
        status: NotificationStatus,
        receipt_ref: Option<String>,
        update_receipt: bool,
    ) -> Result<Notification, RuntimeError> {
        let at = now_ms();
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = if update_receipt {
            tx.execute(
                "UPDATE runtime_notifications SET status=?2,receipt_ref=?3,updated_at=?4 WHERE notification_id=?1",
                params![
                    notification_id,
                    status.as_str(),
                    receipt_ref,
                    sql_u64(at, "notification time")?
                ],
            )?
        } else {
            tx.execute(
                "UPDATE runtime_notifications SET status=?2,updated_at=?3 WHERE notification_id=?1",
                params![
                    notification_id,
                    status.as_str(),
                    sql_u64(at, "notification time")?
                ],
            )?
        };
        if changed == 0 {
            return Err(RuntimeError::InvalidInput("notification not found".into()));
        }
        let notification = load_notification(&tx, notification_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("notification not found".into()))?;
        tx.commit()?;
        Ok(notification)
    }

    /// Decide the next delivery action from durable facts only. An unknown
    /// result asks for the receipt; it never resends.
    pub fn notification_decision(
        &self,
        notification_id: &str,
    ) -> Result<NotificationDecision, RuntimeError> {
        let notification = self
            .notification(notification_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("notification not found".into()))?;
        Ok(match notification.status {
            NotificationStatus::Pending => NotificationDecision::Send,
            NotificationStatus::Queued => NotificationDecision::AwaitDelivery,
            NotificationStatus::Unknown => NotificationDecision::QueryReceipt,
            _ => NotificationDecision::Complete,
        })
    }
}

fn load(connection: &Connection, work_id: &str) -> Result<Option<WorkItem>, RuntimeError> {
    let body: Option<String> = connection
        .query_row(
            "SELECT body FROM runtime_work_items WHERE work_id=?1",
            [work_id],
            |row| row.get(0),
        )
        .optional()?;
    body.map(|body| serde_json::from_str(&body).map_err(RuntimeError::Json))
        .transpose()
}

fn save(connection: &Connection, item: &WorkItem, at: u64) -> Result<(), RuntimeError> {
    let body = serde_json::to_string(item)?;
    let changed = connection.execute(
        "UPDATE runtime_work_items SET lifecycle=?2,work_version=?3,pause_version=?4,stop_version=?5,body=?6,updated_at=?7 WHERE work_id=?1",
        params![
            item.work_id,
            item.lifecycle.as_str(),
            sql_u64(item.work_version, "work_version")?,
            sql_u64(item.pause_version, "pause_version")?,
            sql_u64(item.stop_version, "stop_version")?,
            body,
            sql_u64(at, "updated_at")?
        ],
    )?;
    if changed != 1 {
        return Err(RuntimeError::InvalidInput(
            "WorkItem was not updated".into(),
        ));
    }
    Ok(())
}

fn append_record(
    item: &mut WorkItem,
    criterion_id: &str,
    result: AcceptanceResult,
    evidence: Vec<String>,
    writer: &str,
    source: AcceptanceSource,
    at: u64,
) -> Result<(), RuntimeError> {
    let version = item.work_version.saturating_add(1);
    item.acceptance_records.push(AcceptanceRecord {
        criterion_id: criterion_id.into(),
        result,
        evidence,
        writer: writer.into(),
        source,
        version,
        at,
    });
    item.work_version = version;
    item.updated_at = at;
    if item.lifecycle == WorkLifecycle::Open && item.acceptance_complete() {
        item.lifecycle = WorkLifecycle::Completed;
    }
    Ok(())
}

fn input_record(
    connection: &Connection,
    log_id: &str,
) -> Result<Option<InputRecord>, RuntimeError> {
    let identity: Option<(String, String, String)> = connection
        .query_row(
            "SELECT source,source_event_id,session_id FROM runtime_work_inputs WHERE log_id=?1",
            [log_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((source, source_event_id, session_id)) = identity else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT state,work_id,result_ref,at FROM runtime_work_input_events WHERE log_id=?1 ORDER BY seq",
    )?;
    let events = statement
        .query_map([log_id], |row| {
            let state: String = row.get(0)?;
            Ok((
                state,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                read_u64(row, 3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut events_out = Vec::with_capacity(events.len());
    let mut state = InputState::Received;
    for (state_text, work_id, result_ref, at) in events {
        let parsed = InputState::parse(&state_text)?;
        state = parsed;
        events_out.push(InputStateEvent {
            state: parsed,
            work_id,
            result_ref,
            at,
        });
    }
    Ok(Some(InputRecord {
        log_id: log_id.into(),
        session_id,
        source,
        source_event_id,
        state,
        events: events_out,
    }))
}

fn notification_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NotificationRow> {
    Ok(NotificationRow {
        owner_session_id: row.get(10)?,
        notification_id: row.get(0)?,
        work_id: row.get(1)?,
        work_version: read_u64(row, 2)?,
        target: row.get(3)?,
        content_ref: row.get(4)?,
        artifact_refs: row.get(5)?,
        status: row.get(6)?,
        receipt_ref: row.get(7)?,
        created_at: read_u64(row, 8)?,
        updated_at: read_u64(row, 9)?,
    })
}

struct NotificationRow {
    owner_session_id: Option<String>,
    notification_id: String,
    work_id: String,
    work_version: u64,
    target: String,
    content_ref: Option<String>,
    artifact_refs: String,
    status: String,
    receipt_ref: Option<String>,
    created_at: u64,
    updated_at: u64,
}

fn notification_from_row(row: NotificationRow) -> Result<Notification, RuntimeError> {
    Ok(Notification {
        owner_session_id: row.owner_session_id,
        notification_id: row.notification_id,
        work_id: row.work_id,
        work_version: row.work_version,
        target: row.target,
        content_ref: row.content_ref,
        artifact_refs: serde_json::from_str(&row.artifact_refs).map_err(RuntimeError::Json)?,
        status: NotificationStatus::parse(&row.status)?,
        receipt_ref: row.receipt_ref,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn load_notification(
    connection: &Connection,
    notification_id: &str,
) -> Result<Option<Notification>, RuntimeError> {
    let row: Option<NotificationRow> = connection
        .query_row(
            "SELECT notification_id,work_id,work_version,target,content_ref,artifact_refs,status,receipt_ref,created_at,updated_at,owner_session_id FROM runtime_notifications WHERE notification_id=?1",
            [notification_id],
            notification_row,
        )
        .optional()?;
    row.map(notification_from_row).transpose()
}

#[cfg(test)]
mod acceptance_read_tests {
    use super::*;

    #[test]
    fn stream_checks_cross_blocks_and_validate_the_tail() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut content = "x".repeat(8191);
        content.push_str("你好 needle tail");
        std::fs::write(root.join("file"), &content).unwrap();
        assert!(
            AcceptanceCheck::FileContains {
                path: "file".into(),
                text: "你好 needle".into()
            }
            .evaluate(&root)
            .unwrap()
        );
        assert!(
            AcceptanceCheck::FileEquals {
                path: "file".into(),
                text: content.clone()
            }
            .evaluate(&root)
            .unwrap()
        );
        let mut bytes = content.into_bytes();
        bytes.push(0xff);
        std::fs::write(root.join("file"), bytes).unwrap();
        assert!(
            AcceptanceCheck::FileContains {
                path: "file".into(),
                text: "needle".into()
            }
            .evaluate(&root)
            .is_err()
        );
        assert!(
            !AcceptanceCheck::PathExists {
                path: "missing/file".into()
            }
            .evaluate(&root)
            .unwrap()
        );
    }

    #[test]
    fn text_check_rejects_fifo_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("fifo");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live, NUL-terminated path in this test's temporary directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(
            AcceptanceCheck::FileContains {
                path: "fifo".into(),
                text: "anything".into()
            }
            .evaluate(&root)
            .is_err()
        );
    }

    #[test]
    fn check_rejects_external_links_and_keeps_the_open_root() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(base.join("outside"), "secret").unwrap();
        symlink(base.join("outside"), root.join("link")).unwrap();
        assert!(
            AcceptanceCheck::FileContains {
                path: "link".into(),
                text: "secret".into()
            }
            .evaluate(&root)
            .is_err()
        );
        assert!(
            AcceptanceCheck::PathExists {
                path: "link".into()
            }
            .evaluate(&root)
            .is_err()
        );
        symlink(&base, root.join("directory")).unwrap();
        assert!(
            AcceptanceCheck::PathExists {
                path: "directory/outside".into()
            }
            .evaluate(&root)
            .is_err()
        );
        let anchored = cayw_host::ReadOnlyWorkspace::open(&root).unwrap();
        std::fs::write(root.join("inside"), "original").unwrap();
        std::fs::rename(&root, base.join("moved")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("inside"), "replacement").unwrap();
        assert!(
            AcceptanceCheck::FileEquals {
                path: "inside".into(),
                text: "original".into()
            }
            .evaluate_root(&anchored)
            .unwrap()
        );
    }
}

#[cfg(test)]
mod notification_enqueue_tests {
    use super::*;

    fn pending(store: &WorkItems) -> Notification {
        store
            .prepare_notification(
                "main-session",
                1,
                "desk",
                Some("initial"),
                &[],
                &["desk".into()],
                Some("owner"),
            )
            .unwrap()
    }

    #[test]
    fn only_one_connection_claims_and_a_real_receipt_wins() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("notifications.sqlite");
        let store = WorkItems::open(&path).unwrap();
        let notification = pending(&store);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads = (0..2)
            .map(|_| {
                let path = path.clone();
                let id = notification.notification_id.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let store = WorkItems::open(path).unwrap();
                    barrier.wait();
                    match store.claim_notification_enqueue(&id, Some("final")) {
                        Ok(NotificationEnqueue::Ready { envelope, .. }) => Some(envelope),
                        Err(_) => None,
                        Ok(NotificationEnqueue::Existing(_)) => panic!("no writer has settled yet"),
                    }
                })
            })
            .collect::<Vec<_>>();
        let claimed = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(claimed.len(), 1);
        let envelope = &claimed[0];
        assert_eq!(envelope["text"], "final");
        assert!(
            store
                .claim_notification_enqueue(&notification.notification_id, Some("changed"))
                .is_err()
        );
        store
            .record_notification_receipt(
                &notification.notification_id,
                NotificationStatus::Delivered,
                "service:verified",
            )
            .unwrap();
        let final_status = store
            .settle_notification_enqueue(&notification.notification_id, envelope, true)
            .unwrap();
        assert_eq!(final_status.status, NotificationStatus::Delivered);
        assert_eq!(
            final_status.receipt_ref.as_deref(),
            Some("service:verified")
        );
        assert!(matches!(
            store
                .claim_notification_enqueue(&notification.notification_id, Some("final"))
                .unwrap(),
            NotificationEnqueue::Existing(_)
        ));
    }

    #[test]
    fn crash_after_claim() {
        let Ok(path) = std::env::var("CAYW_TEST_ENQUEUE_DB") else {
            return;
        };
        let id = std::env::var("CAYW_TEST_ENQUEUE_ID").unwrap();
        let store = WorkItems::open(&path).unwrap();
        let NotificationEnqueue::Ready { envelope, .. } = store
            .claim_notification_enqueue(&id, Some("final"))
            .unwrap()
        else {
            panic!("expected claim");
        };
        if let Ok(outbox) = std::env::var("CAYW_TEST_ENQUEUE_FILE") {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(outbox)
                .unwrap();
            serde_json::to_writer(&mut file, &envelope).unwrap();
            file.write_all(b"\n").unwrap();
            file.sync_all().unwrap();
        }
        // Deliberately skip destructors and the local Queued acknowledgement.
        std::process::exit(86);
    }

    #[test]
    fn reopened_claims_do_not_repeat_an_uncertain_append() {
        for written in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("notifications.sqlite");
            let outbox = temp.path().join("notifications.jsonl");
            let store = WorkItems::open(&path).unwrap();
            let notification = pending(&store);
            drop(store);
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "work_items::notification_enqueue_tests::crash_after_claim",
                    "--nocapture",
                ])
                .env("CAYW_TEST_ENQUEUE_DB", &path)
                .env("CAYW_TEST_ENQUEUE_ID", &notification.notification_id)
                .env_remove("CAYW_TEST_ENQUEUE_FILE");
            if written {
                child.env("CAYW_TEST_ENQUEUE_FILE", &outbox);
            }
            assert_eq!(child.status().unwrap().code(), Some(86));
            let before = std::fs::read(&outbox).ok();
            assert_eq!(before.is_some(), written);
            let store = WorkItems::open(&path).unwrap();
            assert_eq!(
                store
                    .notification(&notification.notification_id)
                    .unwrap()
                    .unwrap()
                    .status,
                NotificationStatus::Unknown
            );
            assert!(
                store
                    .claim_notification_enqueue(&notification.notification_id, Some("final"))
                    .is_err()
            );
            assert_eq!(std::fs::read(&outbox).ok(), before);
            store
                .record_notification_receipt(
                    &notification.notification_id,
                    if written {
                        NotificationStatus::Delivered
                    } else {
                        NotificationStatus::Failed
                    },
                    "fixture-host-verified-outcome",
                )
                .unwrap();
            assert!(matches!(
                store
                    .claim_notification_enqueue(&notification.notification_id, None)
                    .unwrap(),
                NotificationEnqueue::Existing(_)
            ));
        }
    }
}
