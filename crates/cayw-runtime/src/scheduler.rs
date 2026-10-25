//! One admission slot is shared by a task's model stream and live code cells.
//! Waiting for permission can release it only when every live cell is stopped
//! before a capability boundary, no model stream is active and all owned
//! process work has finished safely.
use crate::{Runtime, RuntimeError, TaskRecord, execution::RunContext, lock};
use cayw_ai::CancellationToken;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Debug, Clone, Serialize)]
pub struct SchedulerView {
    pub max_concurrent_runs: usize,
    pub max_accepted_tasks: usize,
    pub fairness_after_dispatches: usize,
    pub max_bypasses: usize,
    pub dispatch_count: u64,
    pub next_after: Option<String>,
    pub tasks: Vec<TaskRecord>,
}
impl Runtime {
    /// Bounded live-task snapshot. Terminal history remains on each Session.
    pub fn scheduler(&self, limit: usize) -> Result<SchedulerView, RuntimeError> {
        self.scheduler_page(None, limit)
    }
    pub fn scheduler_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<SchedulerView, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let tasks = self.inner.queue.snapshot_page(after, limit)?;
        let next_after =
            (tasks.len() == limit).then(|| tasks.last().expect("nonzero page").id.clone());
        Ok(SchedulerView {
            max_concurrent_runs: self.inner.config.max_concurrent_runs,
            max_accepted_tasks: self.inner.config.max_queued_runs,
            fairness_after_dispatches: self.inner.config.fairness_after_dispatches,
            max_bypasses: self.inner.config.fairness_after_dispatches
                + self.inner.config.max_queued_runs
                - 1,
            dispatch_count: self.inner.queue.dispatch_count()?,
            next_after,
            tasks,
        })
    }
}

#[derive(Default)]
struct State {
    model_active: bool,
    /// false means the VM may still be running; true is a pre-execution wait.
    cells: BTreeMap<String, bool>,
}
#[derive(Default)]
pub(crate) struct Activity {
    state: Mutex<State>,
    changed: Notify,
}
impl Activity {
    fn park_if_idle(&self, context: &RunContext, state: &State) -> Result<(), RuntimeError> {
        if !context.cancel.is_cancelled()
            && !context.unknown.load(std::sync::atomic::Ordering::Acquire)
            && lock(&context.model_recording_error).is_none()
            && !state.model_active
            && !state.cells.is_empty()
            && state.cells.values().all(|wait| *wait)
        {
            let host = lock(&context.inner.processes)
                .get(&context.task.id)
                .cloned();
            if host.is_some_and(|host| {
                host.active_processes() != 0
                    || host.active_terminal_sessions() != 0
                    || host.outcome_unknown()
                    || host.audit_failure().is_some()
                    || host.terminal_completion_error().is_some()
            }) {
                return Ok(());
            }
            let record = context.inner.queue.get(&context.task.id)?;
            if record.state == crate::TaskState::Running && !record.cancel_requested {
                context.inner.queue.park(&context.task)?;
                context.inner.changed.notify_waiters();
            }
        }
        Ok(())
    }
    pub(crate) fn cell_created(&self, context: &RunContext, id: &str) -> Result<(), RuntimeError> {
        let _coordinate = context.inner.coordination.enter()?;
        let mut state = lock(&self.state);
        let task = context.inner.queue.get(&context.task.id)?;
        if !task.holds_slot || task.state != crate::TaskState::Running {
            return Err(RuntimeError::Worker(
                "code cell requires its task's execution slot".into(),
            ));
        }
        if state.cells.insert(id.into(), false).is_some() {
            return Err(RuntimeError::Worker(
                "code cell already owns scheduler activity".into(),
            ));
        }
        // The executing tool now hands activity to its independently owned VM.
        // Keeping the tool future active until its timed reply would prevent a
        // permission wait inside that VM from returning the task's slot.
        state.model_active = false;
        Ok(())
    }
    pub(crate) fn cell_finished(&self, context: &RunContext, id: &str) -> Result<(), RuntimeError> {
        let _coordinate = context.inner.coordination.enter()?;
        let mut state = lock(&self.state);
        state.cells.remove(id);
        self.park_if_idle(context, &state)?;
        self.changed.notify_waiters();
        Ok(())
    }
    pub(crate) fn waiting(&self, context: &RunContext, id: &str) -> Result<(), RuntimeError> {
        let _coordinate = context.inner.coordination.enter()?;
        let mut state = lock(&self.state);
        let waiting = state
            .cells
            .get_mut(id)
            .ok_or_else(|| RuntimeError::Worker("approval has no live code cell".into()))?;
        *waiting = true;
        self.park_if_idle(context, &state)?;
        self.changed.notify_waiters();
        Ok(())
    }
    pub(crate) async fn resume_cell(
        &self,
        context: &RunContext,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        self.acquire(context, cancel, Some(id)).await
    }
    async fn acquire(
        &self,
        context: &RunContext,
        cancel: &CancellationToken,
        cell: Option<&str>,
    ) -> Result<(), RuntimeError> {
        loop {
            let changed = self.changed.notified();
            let queue_changed = context.inner.changed.notified();
            context.check(cancel)?;
            let admitted = {
                let _coordinate = context.inner.coordination.enter()?;
                let mut state = lock(&self.state);
                context.inner.queue.check(&context.task)?;
                // A following model turn and a newly created cell must acquire
                // the same slot as an approved call. Existing cells can remain
                // parked while the model prepares another independent cell.
                {
                    let record = context.inner.queue.get(&context.task.id)?;
                    if record.state == crate::TaskState::WaitingApproval {
                        context.inner.queue.ready(&context.task)?;
                    }
                    let admitted = context
                        .inner
                        .queue
                        .admit(&context.task, context.inner.config.max_concurrent_runs)?;
                    if admitted {
                        if let Some(id) = cell {
                            let waiting = state.cells.get_mut(id).ok_or_else(|| {
                                RuntimeError::Worker("approval lost its original code cell".into())
                            })?;
                            *waiting = false;
                        } else {
                            if state.model_active {
                                return Err(RuntimeError::Worker(
                                    "model stream already active".into(),
                                ));
                            }
                            state.model_active = true;
                        }
                    }
                    admitted
                }
            };
            if admitted {
                self.changed.notify_waiters();
                context.inner.changed.notify_waiters();
                return Ok(());
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
                _ = context.cancel.cancelled() => return Err(RuntimeError::Cancelled),
                _ = changed => {},
                _ = queue_changed => {},
                _ = tokio::time::sleep(Duration::from_millis(20)) => {},
            }
        }
    }
    pub(crate) async fn model(
        context: &Arc<RunContext>,
        cancel: &CancellationToken,
    ) -> Result<ModelActivity, RuntimeError> {
        context.activity.acquire(context, cancel, None).await?;
        Ok(ModelActivity {
            context: context.clone(),
            active: true,
        })
    }
}

pub(crate) struct ModelActivity {
    context: Arc<RunContext>,
    active: bool,
}
impl ModelActivity {
    pub(crate) fn finish(&mut self) -> Result<(), RuntimeError> {
        if self.active {
            let context = &self.context;
            let _coordinate = context.inner.coordination.enter()?;
            let mut state = lock(&context.activity.state);
            state.model_active = false;
            self.active = false;
            context.activity.park_if_idle(context, &state)?;
            context.activity.changed.notify_waiters();
        }
        Ok(())
    }
}
impl Drop for ModelActivity {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            self.context.persistence_failed(error);
        }
    }
}
