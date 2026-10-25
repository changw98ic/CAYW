use crate::{Runtime, RuntimeError, TaskRecord, TaskState, lock};
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
impl Runtime {
    /// Cancel only the exact original task owner. A stale target never selects
    /// another task, including a replacement in the same logical Session.
    pub fn cancel(&self, target: &CancelTarget) -> Result<bool, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        self.cancel_coordinated(target)
    }
    pub(crate) fn cancel_coordinated(&self, target: &CancelTarget) -> Result<bool, RuntimeError> {
        let task = self.inner.queue.get(&target.task_id)?;
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
        crate::requests::check_task_cancel(&self.inner, &task)?;
        let closing = crate::requests::closing(&self.inner, &task.id);
        let cancelled = self.inner.queue.cancel(target);
        if cancelled.as_ref().is_ok_and(|changed| *changed)
            && let Some(run) = lock(&self.inner.active).get(&target.session_id)
            && run.task.id == target.task_id
            && Some(&run.task.worker) == target.worker_id.as_ref()
            && run.task.fence == target.fence
        {
            run.cancel.cancel();
        }
        self.inner.approvals.notify_waiters();
        self.inner.changed.notify_waiters();
        closing?;
        cancelled
    }
}
