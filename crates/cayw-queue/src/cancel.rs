use crate::TaskRecord;
use serde::{Deserialize, Serialize};

/// The original durable task owner observed by a trusted control client.
/// A queued task has no Worker or Attempt until it is claimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelTarget {
    pub session_id: String,
    pub task_id: String,
    pub worker_id: Option<String>,
    pub attempt_id: Option<String>,
    /// Queue fence, scoped to this Task; this is not the Core Session fence.
    pub fence: u64,
}
impl TaskRecord {
    pub fn cancel_target(&self) -> CancelTarget {
        CancelTarget {
            session_id: self.session_id.clone(),
            task_id: self.id.clone(),
            worker_id: self.worker_id.clone(),
            attempt_id: self.attempt_id.clone(),
            fence: self.fence,
        }
    }
}
