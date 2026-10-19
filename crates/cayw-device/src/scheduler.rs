use crate::DeviceError;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// A host-assigned session identity; it is an ownership label, not authentication.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Result<Self, DeviceError> {
        let id = id.into();
        if id.trim().is_empty() || id.chars().any(char::is_control) {
            return Err(DeviceError::InvalidSessionId);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Admission ceilings, not OS memory limits or CPU quotas.
#[derive(Clone, Copy, Debug)]
pub struct ResourceLimits {
    pub max_concurrent_tasks: usize,
    pub max_declared_memory_bytes: u64,
}

/// Resources declared by the host for a task.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceRequest {
    pub declared_memory_bytes: u64,
    /// Any nonzero GPU request is rejected by this local baseline.
    pub gpu_units: u32,
    /// Any `Some` is rejected: this baseline does not enforce hard memory limits.
    pub hard_memory_limit_bytes: Option<u64>,
    /// Any `Some` is rejected: this baseline does not enforce CPU quotas.
    pub cpu_quota_percent: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceUsage {
    pub active_tasks: usize,
    pub declared_memory_bytes: u64,
}

#[derive(Debug, Default)]
struct State {
    usage: ResourceUsage,
    sessions: BTreeMap<SessionId, ResourceUsage>,
    waiters: VecDeque<Arc<()>>,
}

#[derive(Debug)]
struct Shared {
    limits: ResourceLimits,
    state: Mutex<State>,
    changed: Notify,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn fits(&self, state: &State, request: ResourceRequest) -> bool {
        state.usage.active_tasks < self.limits.max_concurrent_tasks
            && request.declared_memory_bytes
                <= self.limits.max_declared_memory_bytes - state.usage.declared_memory_bytes
    }

    fn reserve(
        self: &Arc<Self>,
        state: &mut State,
        session: SessionId,
        request: ResourceRequest,
    ) -> ResourcePermit {
        state.usage.active_tasks += 1;
        state.usage.declared_memory_bytes += request.declared_memory_bytes;
        let usage = state.sessions.entry(session.clone()).or_default();
        usage.active_tasks += 1;
        usage.declared_memory_bytes += request.declared_memory_bytes;
        ResourcePermit {
            shared: self.clone(),
            session,
            declared_memory_bytes: request.declared_memory_bytes,
        }
    }
}

/// Clones share one ledger. Waiters are admitted in the order first polled.
///
/// Both memory and concurrency must fit together. A smaller request cannot
/// bypass the head of the queue, and `try_acquire` cannot bypass queued work.
#[derive(Clone, Debug)]
pub struct ResourceScheduler {
    shared: Arc<Shared>,
}

/// A non-cloneable, `Send` reservation owned by one task and session.
///
/// For a process, transfer this to `ProcessRunner::run_with_permit` so cancelling
/// its future does not release resources before process cleanup has completed.
#[derive(Debug)]
#[must_use = "dropping the permit releases the resource reservation"]
pub struct ResourcePermit {
    shared: Arc<Shared>,
    session: SessionId,
    declared_memory_bytes: u64,
}

impl ResourcePermit {
    pub fn session_id(&self) -> &SessionId {
        &self.session
    }
}

// A pending future owns its queue slot. Its drop must unblock later requests
// even when the dropped request has never held any resource reservation.
struct QueueSlot {
    shared: Arc<Shared>,
    ticket: Arc<()>,
    queued: bool,
}

impl Drop for QueueSlot {
    fn drop(&mut self) {
        if self.queued {
            {
                let mut state = self.shared.state();
                if let Some(index) = state
                    .waiters
                    .iter()
                    .position(|ticket| Arc::ptr_eq(ticket, &self.ticket))
                {
                    state.waiters.remove(index);
                }
            }
            self.shared.changed.notify_waiters();
        }
    }
}

impl ResourceScheduler {
    pub fn new(limits: ResourceLimits) -> Result<Self, DeviceError> {
        if limits.max_concurrent_tasks == 0 {
            return Err(DeviceError::InvalidLimits(
                "max_concurrent_tasks must be positive",
            ));
        }
        Ok(Self {
            shared: Arc::new(Shared {
                limits,
                state: Mutex::new(State::default()),
                changed: Notify::new(),
            }),
        })
    }

    fn validate(&self, request: ResourceRequest) -> Result<(), DeviceError> {
        if request.gpu_units != 0 {
            return Err(DeviceError::UnsupportedResource("GPU"));
        }
        if request.hard_memory_limit_bytes.is_some() {
            return Err(DeviceError::UnsupportedResource("hard memory limit"));
        }
        if request.cpu_quota_percent.is_some() {
            return Err(DeviceError::UnsupportedResource("CPU quota"));
        }
        if request.declared_memory_bytes > self.shared.limits.max_declared_memory_bytes {
            return Err(DeviceError::ResourceRequestTooLarge);
        }
        Ok(())
    }

    pub fn try_acquire(
        &self,
        session: SessionId,
        request: ResourceRequest,
    ) -> Result<ResourcePermit, DeviceError> {
        self.validate(request)?;
        let mut state = self.shared.state();
        if !state.waiters.is_empty() || !self.shared.fits(&state, request) {
            return Err(DeviceError::ResourcesBusy);
        }
        Ok(self.shared.reserve(&mut state, session, request))
    }

    /// Wait for both budgets together; cancellation/drop removes the queue slot.
    pub async fn acquire(
        &self,
        session: SessionId,
        request: ResourceRequest,
        cancellation: CancellationToken,
    ) -> Result<ResourcePermit, DeviceError> {
        self.validate(request)?;
        if cancellation.is_cancelled() {
            return Err(DeviceError::Cancelled);
        }
        let mut slot = QueueSlot {
            shared: self.shared.clone(),
            ticket: Arc::new(()),
            queued: true,
        };
        self.shared.state().waiters.push_back(slot.ticket.clone());
        loop {
            // notify_waiters records every notification after this creation,
            // including notifications before this future is first polled.
            let changed = self.shared.changed.notified();
            if cancellation.is_cancelled() {
                return Err(DeviceError::Cancelled);
            }
            let permit = {
                let mut state = self.shared.state();
                let first = state
                    .waiters
                    .front()
                    .is_some_and(|ticket| Arc::ptr_eq(ticket, &slot.ticket));
                if first && self.shared.fits(&state, request) {
                    state.waiters.pop_front();
                    slot.queued = false;
                    Some(self.shared.reserve(&mut state, session.clone(), request))
                } else {
                    None
                }
            };
            if let Some(permit) = permit {
                // More than one queued request may fit without another release.
                self.shared.changed.notify_waiters();
                return Ok(permit);
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(DeviceError::Cancelled),
                _ = changed => {}
            }
        }
    }

    pub fn usage(&self) -> ResourceUsage {
        self.shared.state().usage
    }

    pub fn session_usage(&self, session: &SessionId) -> ResourceUsage {
        self.shared
            .state()
            .sessions
            .get(session)
            .copied()
            .unwrap_or_default()
    }
}

impl Drop for ResourcePermit {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state();
            state.usage.active_tasks -= 1;
            state.usage.declared_memory_bytes -= self.declared_memory_bytes;
            if let Some(usage) = state.sessions.get_mut(&self.session) {
                usage.active_tasks -= 1;
                usage.declared_memory_bytes -= self.declared_memory_bytes;
                if usage.active_tasks == 0 {
                    state.sessions.remove(&self.session);
                }
            }
        }
        self.shared.changed.notify_waiters();
    }
}
