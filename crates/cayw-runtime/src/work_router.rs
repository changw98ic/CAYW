//! Host routing boundary for main-session WorkItem dispatches.
//!
//! The main-session Runtime owns the WorkItem, while the dispatched Task lives
//! in another workspace Runtime. Only the unique local Client knows that
//! workspace catalog, so it injects one router into the main-session Runtime.
//! A Headless/SDK Runtime without the local Client has no router and therefore
//! cannot claim that another workspace was updated.

use crate::{AcceptanceCheck, TaskPriority, TaskRecord, TaskState, WorkItem};
use serde::{Deserialize, Serialize};
use std::fmt;

/// One target workspace selection. `workspace` is the Client workspace id or a
/// registered canonical workspace path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDispatchRequest {
    pub work_id: String,
    pub workspace: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    pub text: String,
    pub dispatch_key: String,
    pub pause_version: u64,
    pub stop_version: u64,
    #[serde(default)]
    pub priority: TaskPriority,
    #[serde(default)]
    pub declared_memory_bytes: u64,
}

/// The exact Task identity returned to the main session. The target workspace
/// stores only `work_id` and `dispatch_key`; this reply is the owner's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkDispatchReply {
    pub workspace: String,
    pub session_id: String,
    pub task_id: String,
    pub state: TaskState,
    pub work_id: String,
    pub dispatch_key: String,
    pub reused: bool,
}

/// One dispatched Task lookup. At least one of `task_id` and `dispatch_key`
/// must be supplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskQuery {
    pub workspace: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub dispatch_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkControl {
    Pause,
    Stop,
}

/// One workspace result for pause/stop. A failure on one target never hides
/// the other targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkTargetOutcome {
    pub workspace: String,
    #[serde(default)]
    pub updated: Option<usize>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRouterError {
    pub message: String,
    pub outcome_unknown: bool,
}

impl WorkRouterError {
    pub fn known(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: false,
        }
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: true,
        }
    }
}

impl fmt::Display for WorkRouterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkRouterError {}

/// Host-owned routing to the unique Client's workspace runtimes. Implementations
/// must open only registered workspaces, must use the existing Task queue, and
/// must never create a second queue, lease or usage ledger.
#[async_trait::async_trait]
pub trait WorkRouter: Send + Sync {
    /// Registered workspaces visible to the main session. Read-only.
    async fn workspaces(&self) -> Result<Vec<serde_json::Value>, WorkRouterError>;

    /// Sessions in one workspace. A dispatch target must name one of these.
    async fn sessions(&self, workspace: &str) -> Result<Vec<serde_json::Value>, WorkRouterError>;

    /// Create one ordinary Session in a target workspace. It does not run work
    /// by itself; the returned id is the explicit dispatch target.
    async fn create_session(
        &self,
        workspace: &str,
        label: &str,
    ) -> Result<serde_json::Value, WorkRouterError>;

    /// Create or recover one dispatched Task by its stable `dispatch_key`.
    async fn dispatch(
        &self,
        request: WorkDispatchRequest,
    ) -> Result<WorkDispatchReply, WorkRouterError>;

    /// Read one dispatched Task from its owning workspace.
    async fn task(&self, query: WorkTaskQuery) -> Result<Option<TaskRecord>, WorkRouterError>;

    /// Push one pause/stop version to each target workspace. Failures stay in
    /// the per-target result so the caller can see which workspace was missed.
    async fn control(
        &self,
        action: WorkControl,
        work_id: &str,
        version: u64,
        workspaces: &[String],
    ) -> Result<Vec<WorkTargetOutcome>, WorkRouterError>;

    /// Observe one already-fixed acceptance check in its target workspace, so a
    /// model can never substitute text for the observed result.
    async fn evaluate_check(
        &self,
        workspace: &str,
        check: &AcceptanceCheck,
    ) -> Result<bool, WorkRouterError>;

    /// Tell one dispatched session that a program check failed. A running session
    /// is steered. A finished session gets one new task. A session that already
    /// has a live task and cannot be steered is reported as not delivered. This
    /// does not run the check again.
    async fn notify_session(
        &self,
        notice: WorkSessionNotice,
    ) -> Result<WorkSessionNoticeReply, WorkRouterError>;
}

/// One failed program check to deliver to the session that is doing the work.
/// `sessions` is newest first. The dispatch key identifies this notice, not the
/// original task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSessionNotice {
    pub workspace: String,
    pub sessions: Vec<String>,
    pub text: String,
    pub work_id: String,
    pub dispatch_key: String,
    pub pause_version: u64,
    pub stop_version: u64,
}

/// One terminal state of a dispatched task, sent from the child workspace to the
/// main session. It names the task and its state. It does not describe files or
/// acceptance evidence; the child session owns that review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkStatusReport {
    pub work_id: String,
    pub task_id: String,
    pub session_id: String,
    pub state: String,
    pub error: Option<String>,
}

/// The text the main session relays to the user. It is the status, not a review.
pub(crate) fn status_notice_text(report: &WorkStatusReport) -> String {
    let mut text = String::new();
    text.push_str("Task status updated.\n");
    text.push_str("work_id: ");
    text.push_str(&report.work_id);
    text.push('\n');
    text.push_str("task_id: ");
    text.push_str(&report.task_id);
    text.push('\n');
    text.push_str("state: ");
    text.push_str(&report.state);
    text.push('\n');
    if let Some(error) = &report.error {
        text.push_str("error: ");
        text.push_str(error);
        text.push('\n');
    }
    text.push_str(
        "Tell the user this status. Do not read files, run checks, or dispatch another task.\n",
    );
    text
}

/// The child workspace's link back to the main session that owns the WorkItem.
/// The child reviews in its own workspace and reports task status. The main
/// session does not open the child's files.
#[async_trait::async_trait]
pub trait WorkOwner: Send + Sync {
    async fn work_item(&self, work_id: &str) -> Result<WorkItem, WorkRouterError>;

    async fn record_program_acceptance(
        &self,
        work_id: &str,
        criterion_id: &str,
        passed: bool,
        evidence: String,
    ) -> Result<WorkItem, WorkRouterError>;

    async fn report_status(
        &self,
        report: WorkStatusReport,
    ) -> Result<WorkSessionNoticeReply, WorkRouterError>;
}

/// What happened to that notice. A failure here does not undo the acceptance record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSessionNoticeReply {
    pub delivered: bool,
    pub steered: bool,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}
