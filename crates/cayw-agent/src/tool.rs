use crate::ExecutionMode;
use async_trait::async_trait;
use cayw_ai::{CancellationToken, Content, ToolDefinition, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<Content>,
    pub details: Option<Value>,
    pub structured_content: Option<Value>,
    pub usage: Option<Usage>,
    pub is_error: bool,
    pub terminate: bool,
    pub outcome_unknown: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(text)],
            ..Default::default()
        }
    }
    pub fn error(error: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(error)
        }
    }
    pub(crate) fn cancelled(started: bool) -> Self {
        Self {
            outcome_unknown: started,
            ..Self::error(if started {
                "Tool execution cancelled; its effects may have occurred. Reconcile before retrying."
            } else {
                "Tool call cancelled before execution"
            })
        }
    }
    pub(crate) fn valid_content(&self) -> bool {
        self.content
            .iter()
            .all(|part| matches!(part, Content::Text { .. } | Content::Image { .. }))
    }
}

#[derive(Clone)]
pub struct ToolProgress {
    pub(crate) sender: mpsc::Sender<ToolResult>,
    pub(crate) active: Arc<AtomicBool>,
}

impl ToolProgress {
    /// Returns false after this invocation has settled. Backpressure is bounded.
    pub async fn emit(&self, update: ToolResult) -> bool {
        self.active.load(Ordering::Acquire) && self.sender.send(update).await.is_ok()
    }
}

pub struct ToolContext {
    pub call_id: String,
    pub arguments: Value,
    pub cancel: CancellationToken,
    pub progress: ToolProgress,
}

pub(crate) fn validate_shutdown_grace(grace: Duration) -> Result<(), String> {
    if grace.is_zero() || Instant::now().checked_add(grace).is_none() {
        return Err("Tool shutdown_grace must be positive and fit an Instant".into());
    }
    Ok(())
}

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn execution_mode(&self) -> Option<ExecutionMode> {
        None
    }
    /// Additional time to poll this same started invocation after cancelling its
    /// child token. Must be positive and fit an Instant; registration validates it.
    /// Hosts configure a tool by overriding this method. A tool that itself waits
    /// for nested cleanup must allow more time here than its inner grace budget.
    /// Completed results are retained, including known failure or unknown effects.
    /// Expiry drops the future and returns unknown; no cleanup task is detached.
    /// Dropping the entire agent future only requests cancellation synchronously.
    /// Code that does not yield cannot be preempted by this async time budget.
    fn shutdown_grace(&self) -> Duration {
        Duration::from_secs(3)
    }
    /// Strict schema validation follows this explicit, tool-owned conversion.
    fn prepare_arguments(&self, arguments: Value) -> Result<Value, String> {
        Ok(arguments)
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String>;
}
