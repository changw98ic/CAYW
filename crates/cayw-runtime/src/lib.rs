//! Headless runtime with one durable session history and explicit host authority.
mod artifacts;
mod audit;
mod background;
pub mod bridge;
mod cancellation;
mod cells;
mod children;
mod collaboration;
mod completion;
mod content;
mod context_overview;
mod coordination;
mod error;
mod execution;
pub mod fixture;
mod fleet;
mod input;
pub mod launch;
mod model_requests;
mod permissions;
mod plannotator;
#[cfg(feature = "plugins")]
mod plugins;
#[cfg(all(test, target_os = "macos"))]
mod policy_tests;
mod processes;
mod provider;
mod queue;
mod reconciliation;
mod recovery;
#[cfg(feature = "remote")]
mod remote;
mod request_recovery;
mod requests;
mod resources;
mod results;
mod rpc;
mod scheduler;
mod session;
mod shared_read;
mod values;

pub use artifacts::RuntimeHostDispatch;
pub use audit::{AuditRecord, GrantRecord};
pub use background::{
    BackgroundCleanupInspection, BackgroundConfig, BackgroundLogPage, BackgroundStatus,
    BackgroundTaskView,
};
pub use bridge::{BridgeDispatchEvent, BridgeDispatchOrigin, BridgeDispatchPage};
pub use cancellation::CancelTarget;
pub use cayw_artifacts::{
    ActionIntent, ActionKind, ActionOutcome, ActionRecord, ActionRequest, ActionStatus,
    ArtifactKind, ArtifactPreview, ArtifactReconciledOutcome, ArtifactReconciliation,
    ArtifactReconciliationVerifier, ArtifactRecord, HostActionDispatch, PutArtifactRequest,
    RecoveryBundle, UpdateArtifactRequest,
};
pub use cayw_context::{CostStatus, SessionLedgerSnapshot, SessionOverview};
pub use cayw_plannotator::{FeedbackDecision, FeedbackDraft, LiveFileVersion, StructuredFeedback};
pub use cayw_queue::{QueueError, TaskIntent};
pub use children::LocalAgents;
pub use collaboration::{
    PlanStep, PlanStepStatus, PlanView, QuestionAnswer, QuestionState, QuestionView,
};
pub use content::{ContentConfig, ContentScope, ContentView};
pub use context_overview::session_ledger_snapshot_from_model_requests;
pub use error::{RuntimeError, StorageFailure, WorkerFailure};
pub use fleet::{AgentRunMode, FleetMember, FleetMemberState, FleetView};
pub use input::{ImageInput, TaskInput};
pub use launch::{HeadlessConfig, PiCompatConfig};
pub use model_requests::{
    ModelBudgetContinuation, ModelBudgetContinuationRecord, ModelCostStatus, ModelRequestIdentity,
    ModelRequestLedger, ModelRequestObservation, ModelRequestOrigin, ModelRequestReconciledOutcome,
    ModelRequestReconciliation, ModelRequestReconciliationRecord,
    ModelRequestReconciliationVerifier, ModelRequestRecord, ModelRequestTerminal,
    ModelReservationPolicy, ModelUsageObservation,
};
pub use permissions::{ApprovalChoice, ScopedPermission};
pub use plannotator::PlanFeedbackInput;
pub use processes::{ProcessCleanupInspection, ProcessCleanupProcess};
pub use provider::{ConfiguredProvider, ProviderApi, ProviderConfig, ProviderService};
pub use queue::{TaskPriority, TaskRecord, TaskState};
pub use requests::RequestPolicy;
pub use resources::{
    ResourceCapability, ResourcePolicy, ResourceUsage, ResourceView, TaskResources,
};
pub use results::{
    ResultHandleView, ResultPage, ResultScope, ResultSnippet, ResultStorageContent,
    ResultStorageIdentity, ResultStorageInspection, ResultStorageObservation, ResultStoragePending,
    ResultStorageState,
};
pub use scheduler::SchedulerView;
pub use session::{SessionBindingView, SessionView};

use cayw_ai::{CancellationToken, Model, Provider, ToolDefinition};
use cayw_core::{ApprovalRequest, EventRecord, SessionState, SessionStore};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Clone)]
pub struct RuntimeConfig {
    pub max_concurrent_runs: usize,
    /// Includes running, approval-waiting and draining tasks. One live task per session.
    pub max_queued_runs: usize,
    /// Promote a ready task after this many other dispatches. New arrivals cannot
    /// increase its bound of fairness_after_dispatches + max_queued_runs - 1.
    pub fairness_after_dispatches: usize,
    /// Persistent execution admission declarations; these do not limit actual RSS.
    pub resources: ResourcePolicy,
    /// Optional Device-authoritative text outside the model's workspace.
    pub content: Option<ContentConfig>,
    /// Optional persistent background commands (F13).
    pub background: Option<BackgroundConfig>,
    pub max_prompt_bytes: usize,
    pub max_history_bytes: usize,
    pub max_event_bytes: usize,
    pub max_total_tokens: u64,
    pub max_turns: usize,
    pub max_run_time: Duration,
    pub max_queue_time: Duration,
    pub max_capability_calls: usize,
    pub max_capability_bytes: usize,
    /// Soft UTF-8 JSON size (bytes) above which Host/tool results are stored in
    /// ContentStore and replaced with a result handle stub for the model.
    /// Requires `content`. `0` disables. Default: 32 KiB.
    pub auto_externalize_threshold_bytes: usize,
    pub grant_lifetime: Duration,
    pub codemode: cayw_codemode::Limits,
    pub cells: cayw_cells::CellLimits,
    /// Process profiles, Git executable and existing host services are explicit.
    pub host: cayw_host::HostConfig,
    /// Explicit opt-in to local child sessions with independent permissions.
    pub local_agents: Option<LocalAgents>,
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
    /// Trusted host policy for original-effect receipts. Absence denies reconciliation.
    pub reconciliation_verifier: Option<Arc<dyn cayw_core::ReconciliationVerifier>>,
    /// Trusted host policy for exact model-request non-dispatch or complete
    /// reported-usage evidence. Absence denies new reconciliation decisions.
    pub model_request_reconciliation_verifier: Option<Arc<dyn ModelRequestReconciliationVerifier>>,
    /// Trusted host policy for artifact action / tool outcome receipts.
    pub artifact_reconciliation_verifier:
        Option<Arc<dyn cayw_artifacts::ArtifactReconciliationVerifier>>,
    /// Optional externally supplied shared plugin registry.
    #[cfg(feature = "plugins")]
    pub plugin_registry: Option<Arc<cayw_plugins::PluginRegistry>>,
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_concurrent_runs: 4,
            max_queued_runs: 64,
            fairness_after_dispatches: 8,
            resources: ResourcePolicy::default(),
            content: None,
            background: None,
            max_prompt_bytes: 256 * 1024,
            max_history_bytes: 16 * 1024 * 1024,
            max_event_bytes: 2 * 1024 * 1024,
            max_total_tokens: 128_000,
            max_turns: 32,
            max_run_time: Duration::from_secs(300),
            max_queue_time: Duration::from_secs(60),
            max_capability_calls: 128,
            max_capability_bytes: 8 * 1024 * 1024,
            auto_externalize_threshold_bytes:
                crate::results::DEFAULT_AUTO_EXTERNALIZE_THRESHOLD_BYTES,
            grant_lifetime: Duration::from_secs(300),
            codemode: Default::default(),
            cells: Default::default(),
            host: Default::default(),
            local_agents: None,
            bridge: None,
            permissions: vec![],
            request_policy: None,
            request_limits: Default::default(),
            reconciliation_verifier: None,
            model_request_reconciliation_verifier: None,
            artifact_reconciliation_verifier: None,
            #[cfg(feature = "plugins")]
            plugin_registry: None,
        }
    }
}
impl RuntimeConfig {
    fn validate(&self) -> Result<(), RuntimeError> {
        bridge::validate_config(self)?;
        children::validate(self.local_agents.as_ref())?;
        self.resources.validate()?;
        if let Some(content) = &self.content {
            content.validate()?;
        }
        background::validate(self.background.as_ref())?;
        if self.auto_externalize_threshold_bytes > 1024 * 1024 {
            return Err(RuntimeError::InvalidInput(
                "auto_externalize_threshold_bytes must be at most 1 MiB".into(),
            ));
        }
        if self.max_concurrent_runs == 0
            || self.max_concurrent_runs > 1024
            || self.max_queued_runs < self.max_concurrent_runs
            || self.max_queued_runs > 100_000
            || self.fairness_after_dispatches == 0
            || self.fairness_after_dispatches > 100_000
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
    content: Option<Arc<cayw_content::ContentStore>>,
    background: Option<Arc<background::BackgroundRegistry>>,
    root: PathBuf,
    host_root: cayw_host::Host,
    provider: Arc<dyn Provider>,
    model: Model,
    config: RuntimeConfig,
    active: Mutex<BTreeMap<String, ActiveRun>>,
    budget_locks: Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>,
    fleet: fleet::FleetRegistry,
    binding_cache: session::BindingCache,
    changed: Notify,
    approvals: Notify,
    plannotator: Mutex<plannotator::PlannotatorState>,
    artifacts: Mutex<artifacts::ArtifactState>,
    #[cfg(feature = "plugins")]
    registry: Arc<cayw_plugins::PluginRegistry>,
}

impl Inner {
    pub(crate) fn budget_lock(&self, budget_session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = lock(&self.budget_locks);
        locks.retain(|_, weak| weak.strong_count() > 0);
        if let Some(existing) = locks.get(budget_session_id).and_then(Weak::upgrade) {
            existing
        } else {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            locks.insert(budget_session_id.to_string(), Arc::downgrade(&lock));
            lock
        }
    }
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
        if model.max_output_tokens == Some(0) {
            return Err(RuntimeError::InvalidInput(
                "model output token limit must be positive".into(),
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
        let store = SessionStore::open(store_path.as_ref())?;
        let store = Arc::new(match &config.reconciliation_verifier {
            Some(verifier) => store.with_reconciliation_verifier(verifier.clone()),
            None => store,
        });
        let coordination = coordination::Coordination::new(store_path.as_ref())?;
        let _opening_policy = coordination.opening_policy(store_path.as_ref())?;
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
        let queue = queue::Queue::open_with_resources(
            store_path.as_ref(),
            config.max_concurrent_runs,
            config.max_queued_runs,
            config.fairness_after_dispatches,
            &config.resources,
        )?;
        // Open ContentStore before Host so F02 can externalize filtered originals
        // into the same Device store Runtime already holds for content.*.
        let content = content::open(store_path.as_ref(), &root, config.content.as_ref())?;
        let background = match &config.background {
            Some(cfg) => {
                let host_cfg = bridge::effective_host_config(&config);
                let runner = Arc::new(
                    cayw_host::runner(&host_cfg)
                        .map_err(|e| RuntimeError::Workspace(e.to_string()))?,
                );
                Some(Arc::new(background::BackgroundRegistry::open_with_runner(
                    store_path.as_ref(),
                    cfg,
                    runner,
                    coordination.clone(),
                )?))
            }
            None => None,
        };
        let host_root = cells::catalog_host(&root, bridge::effective_host_config(&config))?;
        // RequestStore owns its authority fence and database as one unit. It
        // must not mistake the pre-existing Core database for a new authority.
        let requests = Arc::new(request_recovery::open_requests(
            store_path.as_ref(),
            config.request_limits.clone(),
        )?);
        // An amendment in a later domain temporarily releases this opener's
        // shared policy lock. Check all domains after the last such transition;
        // no check may amend policy or reopen the held content root.
        drop(_opening_policy);
        queue.check_policy(
            config.max_concurrent_runs,
            config.max_queued_runs,
            config.fairness_after_dispatches,
            &config.resources,
        )?;
        content::check_binding(
            store_path.as_ref(),
            config.content.as_ref(),
            content.as_deref(),
        )?;
        if cayw_requests::RequestStore::persisted_config(requests.path())? != requests.config() {
            return Err(cayw_requests::RequestError::PolicyMismatch.into());
        }
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
                content,
                background,
                root,
                host_root,
                provider,
                model,
                active: Mutex::new(BTreeMap::new()),
                budget_locks: Mutex::new(BTreeMap::new()),
                fleet: fleet::FleetRegistry::default(),
                binding_cache: session::BindingCache::default(),
                changed: Notify::new(),
                approvals: Notify::new(),
                plannotator: Mutex::new(plannotator::PlannotatorState::default()),
                // Placeholder; replaced immediately after Arc construction with Weak dispatch.
                artifacts: Mutex::new(artifacts::ArtifactState {
                    workspace: cayw_artifacts::ArtifactWorkspace::new(),
                }),
                #[cfg(feature = "plugins")]
                registry: config
                    .plugin_registry
                    .clone()
                    .unwrap_or_else(|| Arc::new(cayw_plugins::PluginRegistry::new())),
                config,
            }),
        };
        *crate::lock(&rt.inner.artifacts) = artifacts::new_state(Arc::downgrade(&rt.inner));
        rt.capabilities()?; // Validate actual injected Host configuration before serving.
        Ok(rt)
    }
    /// Trusted host access to Core storage. Runtime recovery must use
    /// [`Self::recover`] so original executor cleanup is checked first.
    pub fn store(&self) -> &SessionStore {
        &self.inner.store
    }
    /// Read one durable approval without exposing mutable Core operations.
    pub fn approval(
        &self,
        session_id: &str,
        approval_id: &str,
    ) -> Result<ApprovalRequest, RuntimeError> {
        Ok(self.inner.store.approval(session_id, approval_id)?)
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
    /// Fold durable session events into a run-inspector view (F06).
    ///
    /// Pages the Core event log from `after` (same authority as [`Self::events`])
    /// until `limit` events are scanned or the session cursor is reached.
    /// When `limit` is `None`, at most 200,000 events are included via internal
    /// `MAX_EVENT_PAGE` paging.
    ///
    /// The response always carries `events_total` / `has_more` / `next_after`;
    /// callers use these cursors to continue through longer histories.
    pub fn run_inspector(
        &self,
        id: &str,
    ) -> Result<cayw_run_inspector::RunInspectorView, RuntimeError> {
        self.run_inspector_page(id, 0, None)
    }

    /// Windowed run-inspector read. See [`Self::run_inspector`].
    ///
    /// `limit` caps how many Core events are folded into this response. Values
    /// above [`cayw_core::MAX_EVENT_PAGE`] are allowed; the store is still read
    /// in `MAX_EVENT_PAGE` chunks. `None` selects the 200,000-event response cap;
    /// continue with `has_more` and `next_after` when more events remain.
    pub fn run_inspector_page(
        &self,
        id: &str,
        after: u64,
        limit: Option<usize>,
    ) -> Result<cayw_run_inspector::RunInspectorView, RuntimeError> {
        let view = self.get(id)?;
        let events_total = view.cursor;
        if after > events_total {
            return Err(RuntimeError::Core(cayw_core::CoreError::FutureCursor {
                after,
                current: events_total,
            }));
        }
        if matches!(limit, Some(0)) {
            return Err(RuntimeError::InvalidInput(
                "run inspector limit must be at least 1 when set".into(),
            ));
        }
        // Bound a single response so a runaway client cannot ask for unbounded
        // materialization in one RPC; full history requires cursor continuation.
        const MAX_INSPECTOR_EVENTS: usize = 200_000;
        if let Some(limit) = limit
            && limit > MAX_INSPECTOR_EVENTS
        {
            return Err(RuntimeError::InvalidInput(format!(
                "run inspector limit may not exceed {MAX_INSPECTOR_EVENTS}"
            )));
        }
        let mut records = Vec::new();
        let mut cursor = after;
        let remaining_cap = limit.unwrap_or(MAX_INSPECTOR_EVENTS);
        while cursor < events_total && records.len() < remaining_cap {
            let want = (events_total - cursor)
                .min(cayw_core::MAX_EVENT_PAGE as u64)
                .min((remaining_cap - records.len()) as u64) as usize;
            if want == 0 {
                break;
            }
            let page = self.inner.store.events(id, cursor, want)?;
            if page.is_empty() {
                break;
            }
            let next = page.last().map_or(cursor, |record| record.cursor);
            if next <= cursor {
                break;
            }
            cursor = next;
            records.extend(page);
        }
        let events_scanned = records.len() as u64;
        let next_after = records.last().map(|record| record.cursor).unwrap_or(after);
        // None still has the response cap; longer histories use next_after.
        // Signal remaining events whenever this response stops before the cursor.
        let has_more = next_after < events_total;
        Ok(cayw_run_inspector::inspect_runtime_events_page(
            id,
            Some(view.label),
            None,
            &records,
            cayw_run_inspector::RunInspectorPage {
                events_total,
                after,
                events_scanned,
                has_more,
                next_after,
            },
        ))
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
        self.ensure_artifact_executor_idle(id)?;
        let snapshot = self.inner.store.snapshot(id)?;
        if results::has_pending_storage(self, id, snapshot.cursor)? {
            return Err(RuntimeError::NeedsReconciliation);
        }
        if self.tasks(id)?.iter().any(|t| {
            matches!(
                t.state,
                TaskState::Queued
                    | TaskState::Running
                    | TaskState::WaitingApproval
                    | TaskState::Draining
            )
        }) {
            return Err(RuntimeError::Busy);
        }
        if self
            .inner
            .cells
            .is_frozen(id)
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?
            || self.has_artifact_unknown(id)?
        {
            return Err(RuntimeError::NeedsReconciliation);
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
        let _coordinate = self.inner.coordination.enter()?;
        self.inner.journal.revoke(id, grant, actor)?;
        self.inner.reads.invalidate(id);
        for task in self.inner.queue.list(id)? {
            if matches!(
                task.state,
                TaskState::Queued
                    | TaskState::Running
                    | TaskState::WaitingApproval
                    | TaskState::Draining
            ) {
                self.cancel_coordinated(&task.cancel_target())?;
            }
        }
        self.inner.approvals.notify_waiters();
        Ok(())
    }
    pub fn capabilities(&self) -> Result<Vec<ToolDefinition>, RuntimeError> {
        execution::capabilities(&self.inner)
    }
    pub(crate) fn check_ready(&self, id: &str) -> Result<SessionView, RuntimeError> {
        let snapshot = self.get(id)?;
        self.ensure_artifact_executor_idle(id)?;
        if snapshot.state == SessionState::NeedsReconciliation
            || self
                .inner
                .cells
                .is_frozen(id)
                .map_err(|error| RuntimeError::Workspace(error.to_string()))?
            || self.has_artifact_unknown(id)?
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
                    .unwrap_or_else(|| "plugin unavailable".into()),
            ));
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_ai::{AssistantStream, Model, ModelRequest, Provider};

    struct NoopProvider;
    impl Provider for NoopProvider {
        fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
            Box::pin(futures::stream::empty())
        }
    }

    fn test_runtime(dir: &Path) -> Runtime {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        Runtime::open(
            dir.join("session.db"),
            workspace,
            Arc::new(NoopProvider),
            Model::new("fixture", "test"),
            RuntimeConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn released_budget_roots_are_pruned_on_next_access() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());

        let root1 = "session-root-1";
        let root2 = "session-root-2";
        let lock1 = runtime.inner.budget_lock(root1);
        let lock2 = runtime.inner.budget_lock(root2);

        {
            let locks = lock(&runtime.inner.budget_locks);
            assert_eq!(locks.len(), 2);
            assert!(locks.contains_key(root1));
            assert!(locks.contains_key(root2));
        }

        drop(lock1);
        drop(lock2);

        // The next access removes keys whose last holder has gone.
        {
            let locks = lock(&runtime.inner.budget_locks);
            assert_eq!(locks.len(), 2);
        }

        // Subsequent access for root-3 triggers lazy cleanup of dead keys
        let root3 = "session-root-3";
        let lock3 = runtime.inner.budget_lock(root3);

        {
            let locks = lock(&runtime.inner.budget_locks);
            assert_eq!(locks.len(), 1);
            assert!(locks.contains_key(root3));
            assert!(!locks.contains_key(root1));
            assert!(!locks.contains_key(root2));
        }

        drop(lock3);
        let root4 = "session-root-4";
        let _lock4 = runtime.inner.budget_lock(root4);

        {
            let locks = lock(&runtime.inner.budget_locks);
            assert_eq!(locks.len(), 1);
            assert!(locks.contains_key(root4));
            assert!(!locks.contains_key(root3));
        }
    }

    #[tokio::test]
    async fn active_and_waiting_locks_preserve_one_mutex() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let root = "shared-session-root";
        let active = runtime.inner.budget_lock(root);
        let waiting = runtime.inner.budget_lock(root);
        assert!(Arc::ptr_eq(&active, &waiting));
        let permit = active.try_lock_owned().unwrap();
        let mut waiter = Box::pin(waiting.lock_owned());
        assert!(futures::poll!(waiter.as_mut()).is_pending());

        // Only the owned guard and queued future keep the mutex alive.
        let _unrelated = runtime.inner.budget_lock("unrelated-root");
        assert!(lock(&runtime.inner.budget_locks).contains_key(root));
        let same = runtime.inner.budget_lock(root);
        assert!(same.try_lock_owned().is_err());
        drop(permit);
        let waiter_permit = waiter.await;
        let same = runtime.inner.budget_lock(root);
        assert!(same.try_lock_owned().is_err());
        drop(waiter_permit);

        let _another = runtime.inner.budget_lock("another-root");
        assert!(!lock(&runtime.inner.budget_locks).contains_key(root));
    }

    #[tokio::test]
    async fn canceled_waiter_is_reclaimed_and_later_requests_serialize() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let root = "cancellation-session-root";
        let holder = runtime.inner.budget_lock(root);
        let weak = Arc::downgrade(&holder);
        let permit = holder.try_lock_owned().unwrap();
        let waiting = runtime.inner.budget_lock(root);
        let cancel = CancellationToken::new();
        let child = cancel.child_token();
        let mut waiter = Box::pin(async move {
            tokio::select! {
                biased;
                _ = child.cancelled() => Err(RuntimeError::Cancelled),
                permit = waiting.lock_owned() => Ok(permit),
            }
        });
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        cancel.cancel();
        assert!(matches!(waiter.await, Err(RuntimeError::Cancelled)));
        drop(permit);
        assert!(weak.upgrade().is_none());

        let first = runtime.inner.budget_lock(root);
        let second = runtime.inner.budget_lock(root);
        assert!(Arc::ptr_eq(&first, &second));
        let permit = first.try_lock_owned().unwrap();
        assert!(second.clone().try_lock_owned().is_err());
        drop(permit);
        assert!(second.try_lock_owned().is_ok());
    }
}
