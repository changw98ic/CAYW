//! Durable task identity, priority/fairness ordering, admission, and
//! cancellation. Opening a database never claims work. Permission audit and
//! cross-component coordination stay in the host Runtime.
mod cancel;
mod input;
mod queue;
mod resources;

pub use cancel::CancelTarget;
pub use input::{ImageInput, TaskInput};
pub use queue::{
    Queue, QueuePolicy, TaskLease, TaskPriority, TaskRecord, TaskState, TaskTerminal, adopt_policy,
    read_policy,
};
pub use resources::{ResourcePolicy, ResourceUsage, TaskResources};

use serde::{Deserialize, Serialize};

/// Trusted host intent recorded with the original task. It is never inferred
/// from prompt text or model output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskIntent {
    General,
    ExplicitProjectTestOrScript { user_instruction: String },
}
impl TaskIntent {
    /// The durable task row must never record an unusable trusted instruction.
    pub fn validate(&self) -> Result<(), QueueError> {
        if let Self::ExplicitProjectTestOrScript { user_instruction } = self
            && (user_instruction.trim().is_empty()
                || user_instruction.len() > 2048
                || user_instruction
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t'))
        {
            return Err(QueueError::InvalidInput(
                "explicit task instruction must contain 1..2048 ordinary bytes".into(),
            ));
        }
        Ok(())
    }
}

/// Failures produced by the durable queue itself. The host maps these onto its
/// own error type; the queue does not depend on Runtime, Host, Device or the
/// Agent execution implementation.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("invalid queue input: {0}")]
    InvalidInput(String),
    #[error("session already has a queued or running task")]
    Busy,
    #[error("runtime task queue is full")]
    QueueFull,
    #[error("task cancelled before execution")]
    Cancelled,
    #[error("queue worker failed: {0}")]
    Worker(String),
    #[error("cancellation does not match the original task execution identity")]
    StaleCancellation,
    #[error("queue storage failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("invalid serialized queue data: {0}")]
    Json(#[from] serde_json::Error),
}
