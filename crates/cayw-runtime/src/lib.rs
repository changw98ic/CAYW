//! Headless runtime with one durable session history and explicit host authority.
mod audit;
mod bridge;
mod cells;
mod coordination;
mod execution;
pub mod fixture;
#[cfg(feature = "modules")]
mod modules;
mod permissions;
mod processes;
mod queue;
mod recovery;
#[cfg(feature = "remote")]
mod remote;
mod requests;
mod rpc;
mod session;
mod shared_read;

pub use audit::{AuditRecord, GrantRecord};
pub use bridge::{BridgeDispatchEvent, BridgeDispatchOrigin, BridgeDispatchPage};
pub use cayw_requests::TaskIntent;
pub use permissions::{ApprovalChoice, ScopedPermission};
pub use queue::{TaskRecord, TaskState};
pub use requests::RequestPolicy;
pub use session::{SessionBindingView, SessionView};

use cayw_ai::{CancellationToken, Model, Provider, ToolDefinition};
use cayw_core::{ApprovalRequest, CoreError, EventRecord, SessionState, SessionStore};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("session already has a queued or running task")]
    Busy,
    #[error("runtime task queue is full")]
    QueueFull,
    #[error("task cancelled before execution")]
    Cancelled,
    #[error("task exceeded its {0} budget")]
    Budget(&'static str),
    #[error("session requires reconciliation before it can continue")]
    NeedsReconciliation,
    #[error("session is frozen: {0}")]
    Frozen(String),
    #[error("approval is no longer pending for an active attempt")]
    StaleApproval,
    #[error("persistent session error: {0}")]
    Core(#[from] CoreError),
    #[error("runtime journal error: {0}")]
    Journal(#[from] rusqlite::Error),
    #[error("task request authority error: {0}")]
    Request(#[from] cayw_requests::RequestError),
    #[error("invalid serialized data: {0}")]
    Json(#[from] serde_json::Error),
    #[error("agent error: {0}")]
    Agent(#[from] cayw_agent::AgentError),
    #[error("workspace error: {0}")]
    Workspace(String),
    #[error("runtime worker failed: {0}")]
    Worker(String),
    #[error("task requires explicit recovery: {cause}; cleanup error: {cleanup:?}")]
    RecoveryRequired {
        #[source]
        cause: Box<RuntimeError>,
        cleanup: Option<Box<RuntimeError>>,
        outcome_unknown: bool,
    },
}

#[derive(Clone)]
pub struct RuntimeConfig {
    pub max_concurrent_runs: usize,
    /// Includes running tasks. One live task per logical session, across connections.
    pub max_queued_runs: usize,
    pub max_prompt_bytes: usize,
    pub max_history_bytes: usize,
    pub max_event_bytes: usize,
    pub max_total_tokens: u64,
    pub max_turns: usize,
    pub max_run_time: Duration,
    pub max_queue_time: Duration,
    pub max_capability_calls: usize,
    pub max_capability_bytes: usize,
    pub grant_lifetime: Duration,
    pub codemode: cayw_codemode::Limits,
    pub cells: cayw_cells::CellLimits,
    /// Process profiles, Git executable and existing host services are explicit.
    pub host: cayw_host::HostConfig,
    /// Trusted SDK input: the host's already-created Registry.consumer(). Each
    /// external call receives its own immutable scope; Runtime creates no source.
    /// Cannot be combined with host.external, the raw platform service boundary.
    pub bridge: Option<cayw_bridge::Bridge>,
    /// Exact capability + canonical JSON arguments, supplied by the trusted host.
    pub permissions: Vec<ScopedPermission>,
    /// Additional task request approval for these exact existing Host operations.
    pub request_policy: Option<RequestPolicy>,
    /// Persisted authority configuration must agree across Runtime connections.
    pub request_limits: cayw_requests::RequestConfig,
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_concurrent_runs: 4,
            max_queued_runs: 64,
            max_prompt_bytes: 256 * 1024,
            max_history_bytes: 16 * 1024 * 1024,
            max_event_bytes: 2 * 1024 * 1024,
            max_total_tokens: 128_000,
            max_turns: 32,
            max_run_time: Duration::from_secs(300),
            max_queue_time: Duration::from_secs(60),
            max_capability_calls: 128,
            max_capability_bytes: 8 * 1024 * 1024,
            grant_lifetime: Duration::from_secs(300),
            codemode: Default::default(),
            cells: Default::default(),
            host: Default::default(),
            bridge: None,
            permissions: vec![],
            request_policy: None,
            request_limits: Default::default(),
        }
    }
}
impl RuntimeConfig {
    fn validate(&self) -> Result<(), RuntimeError> {
        bridge::validate_config(self)?;
        if self.max_concurrent_runs == 0
            || self.max_concurrent_runs > 1024
            || self.max_queued_runs < self.max_concurrent_runs
            || self.max_queued_runs > 100_000
            || self.max_prompt_bytes == 0
            || self.max_history_bytes == 0
            || self.max_event_bytes == 0
            || self.max_total_tokens == 0
            || self.max_turns == 0
            || self.max_capability_calls == 0
            || self.max_capability_bytes == 0
            || [self.max_run_time, self.max_queue_time, self.grant_lifetime]
                .into_iter()
                .any(|d| d.is_zero() || std::time::Instant::now().checked_add(d).is_none())
        {
            return Err(RuntimeError::InvalidInput(
                "runtime budgets must be positive and queue capacity must cover concurrency".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub protocol_version: u32,
    pub running: usize,
    pub queued: usize,
    pub max_concurrent_runs: usize,
    pub provider: String,
    pub model: String,
}

struct ActiveRun {
    task: queue::TaskLease,
    cancel: CancellationToken,
    unknown: Arc<std::sync::atomic::AtomicBool>,
}
pub(crate) struct Inner {
    store: Arc<SessionStore>,
    cells: Arc<cayw_cells::SqliteJournal>,
    reads: Arc<cayw_cells::ReadCoordinator>,
    journal: audit::AuditDb,
    queue: queue::Queue,
    requests: Arc<cayw_requests::RequestStore>,
    coordination: coordination::Coordination,
    process_journal: processes::ProcessJournal,
    processes: Mutex<BTreeMap<String, Arc<cayw_host::Host>>>,
    root: PathBuf,
    host_root: cayw_host::Host,
    provider: Arc<dyn Provider>,
    model: Model,
    config: RuntimeConfig,
    active: Mutex<BTreeMap<String, ActiveRun>>,
    changed: Notify,
    approvals: Notify,
    #[cfg(feature = "modules")]
    registry: Arc<cayw_modules::ModuleRegistry>,
}

#[derive(Clone)]
pub struct Runtime {
    pub(crate) inner: Arc<Inner>,
}
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

impl Runtime {
    /// Opens existing sessions and tasks without reclaiming or replaying work.
    /// Queued tasks require `resume`; running tasks require explicit `recover`.
    pub fn open(
        store_path: impl AsRef<Path>,
        workspace: impl AsRef<Path>,
        provider: Arc<dyn Provider>,
        model: Model,
        config: RuntimeConfig,
    ) -> Result<Self, RuntimeError> {
        config.validate()?;
        if model.provider.trim().is_empty() || model.id.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "model and provider must be explicit".into(),
            ));
        }
        let root = workspace
            .as_ref()
            .canonicalize()
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
        if !root.is_dir() {
            return Err(RuntimeError::Workspace(
                "workspace must be a directory".into(),
            ));
        }
        let host_root = cells::catalog_host(&root, bridge::effective_host_config(&config))?;
        let store = Arc::new(SessionStore::open(store_path.as_ref())?);
        let coordination = coordination::Coordination::new(store_path.as_ref())?;
        let cells = Arc::new(
            cayw_cells::SqliteJournal::open(
                store_path
                    .as_ref()
                    .canonicalize()
                    .map_err(|error| RuntimeError::Workspace(error.to_string()))?,
            )
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?,
        );
        let reads = cayw_cells::ReadCoordinator::new(
            vec![cayw_cells::ReadPolicy {
                capability: "fs.read".into(),
                reuse: true,
                coalesce: true,
            }],
            config.cells.clone(),
        )
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        let journal = audit::AuditDb::open(store_path.as_ref())?;
        let process_journal = processes::ProcessJournal::open(store_path.as_ref())?;
        let queue = queue::Queue::open(
            store_path.as_ref(),
            config.max_concurrent_runs,
            config.max_queued_runs,
        )?;
        // RequestStore owns its authority fence and database as one unit. It
        // must not mistake the pre-existing Core database for a new authority.
        let mut request_path = store_path
            .as_ref()
            .canonicalize()
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?
            .into_os_string();
        request_path.push(".requests.sqlite");
        let requests = Arc::new(cayw_requests::RequestStore::open(
            PathBuf::from(request_path),
            config.request_limits.clone(),
        )?);
        let rt = Self {
            inner: Arc::new(Inner {
                store,
                cells,
                reads,
                journal,
                queue,
                requests,
                coordination,
                process_journal,
                processes: Mutex::new(BTreeMap::new()),
                root,
                host_root,
                provider,
                model,
                config,
                active: Mutex::new(BTreeMap::new()),
                changed: Notify::new(),
                approvals: Notify::new(),
                #[cfg(feature = "modules")]
                registry: Arc::new(cayw_modules::ModuleRegistry::new()),
            }),
        };
        rt.capabilities()?; // Validate actual injected Host configuration before serving.
        Ok(rt)
    }
    pub fn store(&self) -> &SessionStore {
        &self.inner.store
    }
    pub fn workspace(&self) -> &Path {
        &self.inner.root
    }
    pub fn health(&self) -> Result<Health, RuntimeError> {
        let (running, queued) = self.inner.queue.counts()?;
        Ok(Health {
            status: "ok",
            protocol_version: cayw_core::PROTOCOL_VERSION,
            running,
            queued,
            max_concurrent_runs: self.inner.config.max_concurrent_runs,
            provider: self.inner.model.provider.clone(),
            model: self.inner.model.id.clone(),
        })
    }
    pub fn create(&self, label: Option<String>) -> Result<SessionView, RuntimeError> {
        let label = label.unwrap_or_else(|| "Chat".into());
        if label.trim().is_empty() || label.len() > 1024 {
            return Err(RuntimeError::InvalidInput(
                "session label must contain 1..1024 bytes".into(),
            ));
        }
        let snapshot = self.inner.store.create_session(label)?;
        self.get(&snapshot.id)
    }
    pub fn list(&self) -> Result<Vec<SessionView>, RuntimeError> {
        self.inner
            .store
            .list_sessions()?
            .into_iter()
            .map(|snapshot| self.get(&snapshot.id))
            .collect()
    }
    pub fn get(&self, id: &str) -> Result<SessionView, RuntimeError> {
        self.session_view(id)
    }
    pub fn events(
        &self,
        id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<EventRecord>, RuntimeError> {
        self.get(id)?;
        Ok(self.inner.store.events(id, after, limit)?)
    }
    pub async fn wait_events(
        &self,
        id: &str,
        after: u64,
        limit: usize,
        wait: Duration,
        cancel: CancellationToken,
    ) -> Result<Vec<EventRecord>, RuntimeError> {
        if wait > Duration::from_secs(30) {
            return Err(RuntimeError::InvalidInput(
                "event wait may not exceed 30 seconds".into(),
            ));
        }
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.inner.changed.notified();
            let page = self.events(id, after, limit)?;
            if !page.is_empty() || wait.is_zero() {
                return Ok(page);
            }
            tokio::select! { biased;
                _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => return Ok(vec![]),
                _ = notified => {},
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
        }
    }
    pub fn audit(
        &self,
        id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, RuntimeError> {
        self.get(id)?;
        self.inner.journal.events(id, after, limit)
    }
    pub fn grants(&self, id: &str) -> Result<Vec<GrantRecord>, RuntimeError> {
        self.get(id)?;
        self.inner.journal.grants(id)
    }
    pub fn tasks(&self, id: &str) -> Result<Vec<TaskRecord>, RuntimeError> {
        self.get(id)?;
        self.inner.queue.list(id)
    }
    pub fn task(&self, id: &str) -> Result<TaskRecord, RuntimeError> {
        self.inner.queue.get(id)
    }
    pub fn cancel(&self, id: &str) -> Result<bool, RuntimeError> {
        self.get(id)?;
        let _coordinate = self.inner.coordination.enter()?;
        requests::check_cancel(&self.inner, id)?;
        let closing = requests::close_session(&self.inner, id);
        let cancelled = self.inner.queue.cancel(id);
        if let Some(run) = lock(&self.inner.active).get(id) {
            run.cancel.cancel();
        }
        self.inner.approvals.notify_waiters();
        self.inner.changed.notify_waiters();
        closing?;
        cancelled
    }
    pub async fn wait_idle(&self, id: &str) {
        loop {
            let wait = self.inner.changed.notified();
            if !lock(&self.inner.active).contains_key(id) {
                return;
            }
            tokio::select! { _ = wait => {}, _ = tokio::time::sleep(Duration::from_millis(20)) => {} }
        }
    }
    pub fn fork(
        &self,
        id: &str,
        label: &str,
        confirmed: bool,
    ) -> Result<SessionView, RuntimeError> {
        if label.trim().is_empty() || label.len() > 1024 {
            return Err(RuntimeError::InvalidInput(
                "session label must contain 1..1024 bytes".into(),
            ));
        }
        let _coordinate = self.inner.coordination.enter()?;
        if self.tasks(id)?.iter().any(|t| {
            matches!(
                t.state,
                TaskState::Queued | TaskState::Running | TaskState::Draining
            )
        }) {
            return Err(RuntimeError::Busy);
        }
        let fork = self.inner.store.fork_session(id, label, confirmed)?;
        self.get(&fork.id)
    }
    pub fn approve(
        &self,
        id: &str,
        approval: &str,
        choice: ApprovalChoice,
        actor: &str,
    ) -> Result<ApprovalRequest, RuntimeError> {
        permissions::decide(&self.inner, id, approval, choice, actor)
    }
    pub fn revoke(&self, id: &str, grant: &str, actor: &str) -> Result<(), RuntimeError> {
        if actor.trim().is_empty() {
            return Err(RuntimeError::InvalidInput("actor is required".into()));
        }
        self.inner.journal.revoke(id, grant, actor)?;
        self.cancel(id)?;
        self.inner.approvals.notify_waiters();
        Ok(())
    }
    pub fn capabilities(&self) -> Result<Vec<ToolDefinition>, RuntimeError> {
        execution::capabilities(&self.inner)
    }
    pub(crate) fn check_ready(&self, id: &str) -> Result<SessionView, RuntimeError> {
        let snapshot = self.get(id)?;
        if snapshot.state == SessionState::NeedsReconciliation
            || self
                .inner
                .cells
                .is_frozen(id)
                .map_err(|error| RuntimeError::Workspace(error.to_string()))?
        {
            return Err(RuntimeError::NeedsReconciliation);
        }
        if snapshot.attempt.is_some() {
            return Err(RuntimeError::Busy);
        }
        if serde_json::to_vec(&snapshot.messages)?.len() > self.inner.config.max_history_bytes {
            return Err(RuntimeError::Budget("history byte"));
        }
        if snapshot.frozen {
            return Err(RuntimeError::Frozen(
                snapshot
                    .binding
                    .as_ref()
                    .and_then(|binding| binding.reason.clone())
                    .unwrap_or_else(|| "module unavailable".into()),
            ));
        }
        Ok(snapshot)
    }
}
