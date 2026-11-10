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
pub mod local_cli;
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
    ModelCostStatus, ModelRequestIdentity, ModelRequestLedger, ModelRequestObservation,
    ModelRequestOrigin, ModelRequestReconciledOutcome, ModelRequestReconciliation,
    ModelRequestReconciliationRecord, ModelRequestReconciliationVerifier, ModelRequestRecord,
    ModelRequestTerminal, ModelUsageObservation,
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
use cayw_core::{ApprovalRequest, EventRecord, SessionStore};
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
    pub max_prompt_bytes: Option<usize>,
    pub max_history_bytes: Option<usize>,
    pub max_event_bytes: Option<usize>,
    /// Task budgets are off unless the trusted host configures them. `None`
    /// means the limit is not applied at all.
    pub max_total_tokens: Option<u64>,
    pub max_turns: Option<usize>,
    pub max_run_time: Option<Duration>,
    pub max_queue_time: Option<Duration>,
    pub max_capability_calls: Option<usize>,
    pub max_capability_bytes: Option<usize>,
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
            max_prompt_bytes: None,
            max_history_bytes: None,
            max_event_bytes: None,
            max_total_tokens: None,
            max_turns: None,
            max_run_time: None,
            max_queue_time: None,
            max_capability_calls: None,
            max_capability_bytes: None,
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
        let budgets_invalid = self.max_total_tokens == Some(0)
            || self.max_turns == Some(0)
            || self.max_capability_calls == Some(0)
            || self.max_capability_bytes == Some(0)
            || self.max_prompt_bytes == Some(0)
            || self.max_history_bytes == Some(0)
            || self.max_event_bytes == Some(0)
            || [self.max_run_time, self.max_queue_time]
                .into_iter()
                .flatten()
                .any(|d| d.is_zero() || std::time::Instant::now().checked_add(d).is_none());
        if self.max_concurrent_runs == 0
            || self.max_concurrent_runs > 1024
            || self.max_queued_runs < self.max_concurrent_runs
            || self.max_queued_runs > 100_000
            || self.fairness_after_dispatches == 0
            || self.fairness_after_dispatches > 100_000
            || budgets_invalid
            || self.grant_lifetime.is_zero()
            || std::time::Instant::now()
                .checked_add(self.grant_lifetime)
                .is_none()
        {
            return Err(RuntimeError::InvalidInput(
                "runtime budgets must be positive and queue capacity must cover concurrency".into(),
            ));
        }
        Ok(())
    }
}

/// One optional Runtime service that was disabled instead of stopping startup.
/// The reason is user-facing and never contains credentials or file contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DisabledService {
    pub name: String,
    pub reason: String,
}

impl From<cayw_host::DisabledService> for DisabledService {
    fn from(service: cayw_host::DisabledService) -> Self {
        Self {
            name: service.name.into(),
            reason: service.reason,
        }
    }
}

/// Map a capability name to the optional service that owns it.
pub(crate) fn service_for_capability(name: &str) -> Option<&'static str> {
    if name.starts_with("web.") {
        Some("web_access")
    } else if name.starts_with("vision.") {
        Some("vision")
    } else if name.starts_with("bg.") {
        Some("background")
    } else if name.starts_with("mcp.") || name.starts_with("skills.") {
        Some("mcp")
    } else {
        None
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
    pub disabled_services: Vec<DisabledService>,
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
    disabled_services: Vec<DisabledService>,
    /// Operation IDs from a configured request policy whose owning service is
    /// disabled. Opening succeeds; using one reports the reason.
    disabled_operations: BTreeMap<String, String>,
    selection: Mutex<ConfiguredProvider>,
    config: RuntimeConfig,
    active: Mutex<BTreeMap<String, ActiveRun>>,
    budget_locks: Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>,
    /// Requests this process currently has in flight, keyed by request id.
    /// The projection uses this to tell live holds from crash-abandoned ones.
    in_flight: Mutex<BTreeMap<String, ModelRequestIdentity>>,
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
    pub(crate) fn disabled_operation_reason(&self, arguments: &serde_json::Value) -> Option<&str> {
        let id = arguments.get("operation_id")?.as_str()?;
        self.disabled_operations.get(id).map(String::as_str)
    }

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

    pub(crate) fn in_flight_requests(&self) -> BTreeMap<String, ModelRequestIdentity> {
        lock(&self.in_flight).clone()
    }

    pub(crate) fn mark_in_flight(&self, identity: ModelRequestIdentity) {
        lock(&self.in_flight).insert(identity.request_id.clone(), identity);
    }

    pub(crate) fn clear_in_flight(&self, request_id: &str) {
        lock(&self.in_flight).remove(request_id);
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
        let mut config = config;
        // Normalize orderable optional limits before validation, binding and
        // store construction so every layer sees the same values.
        if let Some(content) = config.content.take() {
            config.content = Some(content.normalized());
        }
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
        // The read coordinator validates captured bytes before they settle. Use
        // the Host's own result cap here; the durable Core bound still applies
        // before the terminal, Cells projection or model presentation.
        let mut read_limits = config.cells.clone();
        if let Some(host_cap) = config.host.max_result_bytes {
            read_limits.item_bytes = Some(
                read_limits
                    .item_bytes
                    .map_or(host_cap, |current| current.max(host_cap)),
            );
        }
        if let (Some(item_cap), Some(token_cap)) =
            (read_limits.item_bytes, read_limits.max_output_tokens)
        {
            read_limits.max_output_tokens = Some(token_cap.max(item_cap.saturating_add(1024)));
        }
        let reads = cayw_cells::ReadCoordinator::new(
            vec![cayw_cells::ReadPolicy {
                capability: "fs.read".into(),
                reuse: true,
                coalesce: true,
            }],
            read_limits,
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
        let mut disabled_services = Vec::new();
        let background = match config.background.take() {
            Some(cfg) => {
                let opened = (|| -> Result<Arc<background::BackgroundRegistry>, RuntimeError> {
                    let host_cfg = bridge::effective_host_config(&config);
                    let runner = Arc::new(
                        cayw_host::runner(&host_cfg)
                            .map_err(|e| RuntimeError::Workspace(e.to_string()))?,
                    );
                    Ok(Arc::new(background::BackgroundRegistry::open_with_runner(
                        store_path.as_ref(),
                        &cfg,
                        runner,
                        coordination.clone(),
                    )?))
                })();
                match opened {
                    Ok(registry) => {
                        config.background = Some(cfg);
                        Some(registry)
                    }
                    Err(error) => {
                        disabled_services.push(DisabledService {
                            name: "background".into(),
                            reason: error.to_string(),
                        });
                        None
                    }
                }
            }
            None => None,
        };
        if config.content.is_none() && config.host.output_filter.take().is_some() {
            disabled_services.push(DisabledService {
                name: "output_filter".into(),
                reason: "output_filter requires content".into(),
            });
        }
        let host_root = cells::catalog_host(&root, bridge::effective_host_config(&config))?;
        disabled_services.extend(
            host_root
                .disabled_services()
                .iter()
                .cloned()
                .map(DisabledService::from),
        );
        let mut disabled_operations = BTreeMap::new();
        // Without a bridge or an explicit external Host there is no MCP or Pi
        // catalog in the Host. Keep the policy usable and report the reason when
        // one of its operations is used instead of failing the whole workspace.
        let external_unavailable = config.bridge.is_none() && config.host.external.is_none();
        if let Some(policy) = config.request_policy.as_mut() {
            if policy.operations.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "request policy must select at least one operation".into(),
                ));
            }
            policy.operations.retain(|operation| {
                let reason = match service_for_capability(&operation.capability) {
                    Some("mcp") if external_unavailable => {
                        Some("MCP and Pi sources are not installed".to_string())
                    }
                    Some(service) => disabled_services
                        .iter()
                        .find(|service_status| service_status.name == service)
                        .map(|service_status| service_status.reason.clone()),
                    None => None,
                };
                match reason {
                    Some(reason) => {
                        disabled_operations.insert(operation.id.clone(), reason);
                        false
                    }
                    None => true,
                }
            });
            // Keep the policy even when every operation is disabled, so the
            // request tools stay advertised and using one reports its reason.
        }
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
                disabled_services,
                disabled_operations,
                selection: Mutex::new(ConfiguredProvider { provider, model }),
                active: Mutex::new(BTreeMap::new()),
                budget_locks: Mutex::new(BTreeMap::new()),
                in_flight: Mutex::new(BTreeMap::new()),
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
    /// Trusted host selection for future task attempts. Running attempts retain
    /// their exact provider/model snapshot, including accounting and output limits.
    pub fn select_model(&self, selection: ConfiguredProvider) -> Result<(), RuntimeError> {
        if selection.model.provider.trim().is_empty()
            || selection.model.id.trim().is_empty()
            || selection.model.max_output_tokens == Some(0)
        {
            return Err(RuntimeError::InvalidInput("invalid model selection".into()));
        }
        *lock(&self.inner.selection) = selection;
        Ok(())
    }

    /// Optional services that were disabled during open, with a user-safe
    /// reason. An empty slice means every configured service is running.
    pub fn disabled_services(&self) -> &[DisabledService] {
        &self.inner.disabled_services
    }

    pub fn health(&self) -> Result<Health, RuntimeError> {
        let (running, queued) = self.inner.queue.counts()?;
        let selection = lock(&self.inner.selection);
        Ok(Health {
            status: "ok",
            protocol_version: cayw_core::PROTOCOL_VERSION,
            running,
            queued,
            max_concurrent_runs: self.inner.config.max_concurrent_runs,
            provider: selection.model.provider.clone(),
            model: selection.model.id.clone(),
            disabled_services: self.inner.disabled_services.clone(),
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
        {
            return Err(RuntimeError::RecoveryRequired {
                cause: Box::new(RuntimeError::Worker(
                    "cells storage is frozen pending recovery".into(),
                )),
                cleanup: None,
                outcome_unknown: true,
            });
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
        self.check_ready_snapshot(id, snapshot)
    }
    /// Same readiness contract while the caller already owns this Session's
    /// artifact executor. Artifact state is observed without re-acquiring the
    /// executor, so a live action does not reject itself.
    pub(crate) fn check_ready_locked(&self, id: &str) -> Result<SessionView, RuntimeError> {
        let snapshot = self.get(id)?;
        let _ = self.artifact_unknown(id, true)?;
        self.check_ready_snapshot(id, snapshot)
    }
    fn check_ready_snapshot(
        &self,
        id: &str,
        snapshot: SessionView,
    ) -> Result<SessionView, RuntimeError> {
        if self
            .inner
            .cells
            .is_frozen(id)
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?
        {
            return Err(RuntimeError::RecoveryRequired {
                cause: Box::new(RuntimeError::Worker(
                    "cells storage is frozen pending recovery".into(),
                )),
                cleanup: None,
                outcome_unknown: true,
            });
        }
        if snapshot.attempt.is_some() {
            return Err(RuntimeError::Busy);
        }
        if let Some(max_history_bytes) = self.inner.config.max_history_bytes
            && serde_json::to_vec(&snapshot.messages)?.len() > max_history_bytes
        {
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

    #[test]
    fn readiness_rejects_a_frozen_binding() {
        // Break caught: artifact InvokeTool must reuse the same readiness
        // contract as prompts, including a frozen plugin binding.
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let id = runtime.create(None).unwrap().id;
        let mut view = runtime.get(&id).unwrap();
        view.frozen = true;
        let error = runtime.check_ready_snapshot(&id, view).unwrap_err();
        assert!(matches!(error, RuntimeError::Frozen(_)), "{error:?}");
    }

    #[test]
    fn readiness_rechecks_state_after_the_first_gate() {
        // Break caught: readiness observed before the artifact executor lock can
        // go stale. The second gate must observe a freeze that landed after the
        // first gate returned successfully.
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let id = runtime.create(None).unwrap().id;
        runtime.check_ready(&id).unwrap();
        let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
        db.execute(
            "INSERT OR IGNORE INTO cells_frozen_v1(session) VALUES(?1)",
            [&id],
        )
        .unwrap();
        let error = runtime.check_ready_locked(&id).unwrap_err();
        assert!(
            matches!(
                error,
                RuntimeError::RecoveryRequired {
                    outcome_unknown: true,
                    ..
                }
            ),
            "{error:?}"
        );
    }
}
