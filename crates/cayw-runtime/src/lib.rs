//! Headless runtime with one durable session history and explicit host authority.
mod approval_rules;
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
mod main_memory;
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
pub mod recovery_points;
#[cfg(feature = "remote")]
mod remote;
mod request_recovery;
mod requests;
mod resources;
mod results;
mod rpc;
mod scheduler;
mod session;
mod session_context;
mod shared_read;
mod values;
mod work_items;
mod work_router;
mod work_tools;

pub use approval_rules::{
    ApprovalRule, ApprovalRuleSource, ApprovalRulesFile, FileRules, RecoveryScope,
    RuleChangeOutcome, RuleChangeQuestion, RuleContext, RuleDecision, RulesSnapshot, device_rules,
};
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
pub use cayw_queue::{QueueError, TaskDispatch, TaskIntent};
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
pub use main_memory::{
    ApplyReport, MainMemory, MemoryCandidate, MemoryEntry, MemoryEvent, MemoryOperation,
    MemoryOrigin, MemoryQuery, MemoryQueryEntry, MemoryQueryView, MemoryScan, MemoryStatus,
    MemoryStatusView, RejectedCandidate, SourceType, UserEvidence, UserSource, user_evidence,
};
pub use model_requests::{
    ModelBudgetContinuation, ModelBudgetContinuationRecord, ModelCostStatus, ModelRequestIdentity,
    ModelRequestLedger, ModelRequestObservation, ModelRequestOrigin, ModelRequestReconciledOutcome,
    ModelRequestReconciliation, ModelRequestReconciliationRecord,
    ModelRequestReconciliationVerifier, ModelRequestRecord, ModelRequestTerminal,
    ModelReservationPolicy, ModelUsageObservation,
};
pub use permissions::{ApprovalChoice, ApprovalQuestion, ScopedPermission};
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
pub use session_context::{MaintenanceReconciliation, MaintenanceReconciliationRecord};
pub use work_items::{
    AcceptanceCheck, AcceptanceCriterion, AcceptanceRecord, AcceptanceResult, AcceptanceSource,
    InputRecord, InputState, InputStateEvent, LateWorkResult, Notification, NotificationDecision,
    NotificationStatus, ProactiveMode, ProactiveTriggerOutcome, WorkItem, WorkItemSpec, WorkItems,
    WorkLifecycle, WorkSessionRef, WorkTaskRef,
};
pub use work_router::{
    WorkControl, WorkDispatchReply, WorkDispatchRequest, WorkRouter, WorkRouterError,
    WorkTargetOutcome, WorkTaskQuery,
};

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
    /// This remains only a one-time exact pre-authorization for this Runtime.
    pub permissions: Vec<ScopedPermission>,
    /// Device-level user approval rules. `None` keeps the explicit Headless/SDK
    /// path where every capability needs an `approval.decide` answer. The
    /// Client captures the current version for each dispatched task.
    pub approval_rules: Option<Arc<dyn approval_rules::ApprovalRuleSource>>,
    /// This Runtime serves the main-session workspace. Only then may the
    /// built-in rules release its own local reads, memory and dispatches.
    pub main_session: bool,
    /// Proactive work mode. It is separate from approval rules: changing it
    /// never changes which operations are allowed or asked about.
    pub proactive_mode: work_items::ProactiveMode,
    /// User-configured notification targets that `notify.send` may use.
    pub notification_targets: Vec<String>,
    /// Directories covered by the current recovery points. With no scope, all
    /// file rewrites and deletions ask.
    pub recovery_scope: Option<approval_rules::RecoveryScope>,
    /// Private Git recovery points. `None` (SDK and fully explicit Headless)
    /// leaves the workspace without automatic snapshots; the local Client
    /// supplies a default configuration.
    pub recovery: Option<recovery_points::RecoveryConfig>,
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
            approval_rules: None,
            main_session: false,
            proactive_mode: work_items::ProactiveMode::Off,
            notification_targets: Vec::new(),
            recovery_scope: None,
            recovery: None,
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
    selection: Mutex<ConfiguredProvider>,
    context: Mutex<session_context::SessionContexts>,
    memory: main_memory::MainMemory,
    work_items: work_items::WorkItems,
    /// Injected by the unique local Client. Headless/SDK Runtimes have none and
    /// cannot claim another workspace was updated.
    work_router: Mutex<Option<Arc<dyn work_router::WorkRouter>>>,
    recovery: Option<recovery_points::RecoveryPoints>,
    config: RuntimeConfig,
    active: Mutex<BTreeMap<String, ActiveRun>>,
    budget_locks: Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>,
    fleet: fleet::FleetRegistry,
    binding_cache: session::BindingCache,
    changed: Notify,
    approvals: Notify,
    /// Rule modifications shown in a main-session conversation, never saved
    /// until the user's exact confirmation. Keyed by session id.
    pending_rule_changes: Mutex<BTreeMap<String, approval_rules::PendingRuleChange>>,
    /// Serializes reading the device rule version, appending, and writing it.
    rule_edit: Mutex<()>,
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

    /// Current recovery coverage for approval decisions. Disabling recovery
    /// turns this into `None`, so rewrites and deletions ask again. The
    /// snapshot state's out-of-scope sample is added as explicit exclusions.
    pub(crate) fn recovery_scope(&self) -> Option<RecoveryScope> {
        let base = self.config.recovery_scope.as_ref()?;
        match self.recovery.as_ref() {
            Some(recovery) => recovery.approval_scope(base),
            None => Some(base.clone()),
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

fn recovery_outcome(outcome: &recovery_points::SnapshotOutcome) -> serde_json::Value {
    match outcome {
        recovery_points::SnapshotOutcome::Disabled => {
            serde_json::json!({"status":"disabled"})
        }
        recovery_points::SnapshotOutcome::Unavailable { warning } => {
            serde_json::json!({"status":"unavailable","warning":warning})
        }
        recovery_points::SnapshotOutcome::Ready {
            id,
            created,
            complete,
            out_of_scope,
            out_of_scope_sample,
            warning,
        } => serde_json::json!({
            "status":"ready",
            "snapshot":id,
            "created":created,
            "complete":complete,
            "out_of_scope":out_of_scope,
            "out_of_scope_sample":out_of_scope_sample,
            "warning":warning,
        }),
    }
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
        // The read coordinator validates captured bytes before they settle. Use
        // the Host's own result cap here; the durable Core bound still applies
        // before the terminal, Cells projection or model presentation.
        let mut read_limits = config.cells.clone();
        let read_result_cap = config.host.max_result_bytes.min(16 * 1024 * 1024 - 1024);
        read_limits.item_bytes = read_limits.item_bytes.max(read_result_cap);
        read_limits.max_output_tokens = read_limits
            .max_output_tokens
            .max(read_limits.item_bytes.saturating_add(1024));
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
        let recovery = match &config.recovery {
            Some(recovery) => {
                let state_dir = store_path
                    .as_ref()
                    .parent()
                    .ok_or_else(|| {
                        RuntimeError::Workspace(
                            "runtime database path has no recovery state directory".into(),
                        )
                    })?
                    .to_path_buf();
                Some(recovery_points::RecoveryPoints::open(
                    &root,
                    state_dir,
                    recovery.clone(),
                ))
            }
            None => None,
        };
        let memory = main_memory::MainMemory::open(&root);
        let work_items = work_items::WorkItems::open(store_path.as_ref())?;
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
                selection: Mutex::new(ConfiguredProvider { provider, model }),
                context: Mutex::new(session_context::SessionContexts::new(
                    store_path.as_ref().parent().map(Path::to_path_buf),
                )),
                memory,
                work_items,
                work_router: Mutex::new(None),
                recovery,
                active: Mutex::new(BTreeMap::new()),
                budget_locks: Mutex::new(BTreeMap::new()),
                fleet: fleet::FleetRegistry::default(),
                binding_cache: session::BindingCache::default(),
                changed: Notify::new(),
                approvals: Notify::new(),
                pending_rule_changes: Mutex::new(BTreeMap::new()),
                rule_edit: Mutex::new(()),
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
    /// The exact question and three reply options to show in the main-session
    /// conversation for one still-pending original request.
    pub fn approval_question(
        &self,
        session_id: &str,
        approval_id: &str,
    ) -> Result<permissions::ApprovalQuestion, RuntimeError> {
        let approval = self.approval(session_id, approval_id)?;
        if approval.decision.is_some() {
            return Err(RuntimeError::StaleApproval);
        }
        Ok(permissions::question(&approval))
    }
    /// Apply one main-session text answer to the original request. The reply must
    /// exactly equal one of the three option labels; it never goes through a
    /// model and cannot be supplied by tool output.
    pub fn answer_approval(
        &self,
        session_id: &str,
        approval_id: &str,
        reply: &str,
        actor: &str,
    ) -> Result<ApprovalRequest, RuntimeError> {
        let choice = ApprovalChoice::from_reply(reply).ok_or_else(|| {
            RuntimeError::InvalidInput(
                "approval reply must exactly match one of the offered options".into(),
            )
        })?;
        self.approve(session_id, approval_id, choice, actor)
    }
    pub fn workspace(&self) -> &Path {
        &self.inner.root
    }
    /// Durable base memory and preferences. The authoritative directory exists
    /// only in the main-session workspace.
    pub fn memory(&self) -> &main_memory::MainMemory {
        &self.inner.memory
    }
    fn require_main_session(&self) -> Result<(), RuntimeError> {
        if self.inner.config.main_session {
            Ok(())
        } else {
            Err(RuntimeError::InvalidInput(
                "memory is only available in the main session workspace".into(),
            ))
        }
    }
    pub fn memory_status(&self) -> Result<main_memory::MemoryStatusView, RuntimeError> {
        self.require_main_session()?;
        self.inner.memory.status()
    }
    pub fn memory_entries(&self) -> Result<Vec<main_memory::MemoryEntry>, RuntimeError> {
        self.require_main_session()?;
        self.inner.memory.visible_entries()
    }
    /// Read conversation, work and recorded decisions inside one source range.
    /// This is read-only and omits forgotten ranges; `session.events` still
    /// returns the original records by cursor.
    pub fn query_memory(
        &self,
        query: main_memory::MemoryQuery,
    ) -> Result<main_memory::MemoryQueryView, RuntimeError> {
        self.require_main_session()?;
        let events = self.session_events(&query.session_id)?;
        self.inner.memory.query(&query, &events)
    }
    /// The verified user-input source set for one session's durable log.
    pub fn memory_user_evidence(
        &self,
        session_id: &str,
    ) -> Result<main_memory::UserEvidence, RuntimeError> {
        self.require_main_session()?;
        Ok(main_memory::user_evidence(
            &self.session_events(session_id)?,
        ))
    }
    /// Complete durable event log for one session, in cursor order.
    pub(crate) fn session_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<cayw_core::EventRecord>, RuntimeError> {
        let cursor = self.get(session_id)?.cursor;
        let mut events = Vec::new();
        let mut after = 0;
        while after < cursor {
            let page = self
                .inner
                .store
                .events(session_id, after, cayw_core::MAX_EVENT_PAGE)?;
            if page.is_empty() {
                break;
            }
            after = page.last().expect("nonempty page").cursor;
            events.extend(page);
        }
        Ok(events)
    }
    /// Apply proposed memory writes. Every cited source must be a verified
    /// user-input turn; tool output, web pages and execution results are refused.
    pub fn apply_memory_candidates(
        &self,
        session_id: &str,
        candidates: &[main_memory::MemoryCandidate],
    ) -> Result<main_memory::ApplyReport, RuntimeError> {
        self.require_main_session()?;
        let evidence = self.memory_user_evidence(session_id)?;
        self.inner.memory.apply(&evidence, candidates)
    }
    /// Record a forget range. Original events and raw logs stay unchanged; later
    /// context and memory reads skip the range.
    pub fn forget_memory_range(
        &self,
        range: cayw_context::SourceRange,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        self.require_main_session()?;
        self.inner
            .memory
            .forget(range, reason, main_memory::now_ms())
    }
    /// Dream organizes references of existing summaries only. Without explicit
    /// model text the body stays empty and no model call is made.
    pub fn dream_memory(
        &self,
        summaries: &[cayw_context::SummaryRecord],
        model_text: Option<&str>,
    ) -> Result<cayw_context::DreamPlan, RuntimeError> {
        self.require_main_session()?;
        self.inner.memory.dream(summaries, model_text)
    }
    pub fn memory_context_block(&self) -> Result<Option<String>, RuntimeError> {
        self.require_main_session()?;
        self.inner.memory.context_block()
    }
    /// Create a durable WorkItem in the main-session workspace. Acceptance
    /// criteria with an executable check are fixed here; the rest depend on the
    /// user's answer.
    pub fn create_work_item(
        &self,
        spec: work_items::WorkItemSpec,
    ) -> Result<work_items::WorkItem, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.create(spec)
    }
    pub fn work_item(&self, work_id: &str) -> Result<Option<work_items::WorkItem>, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.get(work_id)
    }
    /// Install the unique local Client's workspace router. Only the main-session
    /// Runtime may receive one. Headless/SDK paths keep `None`, so a main session
    /// never claims that a dispatched workspace was updated without the Client.
    pub fn set_work_router(
        &self,
        router: Arc<dyn work_router::WorkRouter>,
    ) -> Result<(), RuntimeError> {
        self.require_main_session()?;
        let mut slot = lock(&self.inner.work_router);
        if slot.is_some() {
            return Err(RuntimeError::InvalidInput(
                "a work router is already installed".into(),
            ));
        }
        *slot = Some(router);
        Ok(())
    }
    pub fn list_work_items(&self) -> Result<Vec<work_items::WorkItem>, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.list()
    }
    /// Pause increments the version that is written into already-dispatched
    /// tasks. The target workspace refuses later capability calls at once.
    pub fn pause_work_item(&self, work_id: &str) -> Result<work_items::WorkItem, RuntimeError> {
        self.require_main_session()?;
        let item = self.inner.work_items.pause(work_id)?;
        self.inner
            .queue
            .pause_work(&item.work_id, item.pause_version)?;
        Ok(item)
    }
    pub fn resume_work_item(&self, work_id: &str) -> Result<work_items::WorkItem, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.resume(work_id)
    }
    /// Stop is terminal for this WorkItem and cancels every live task that was
    /// dispatched for it. Late results stay as facts and never redispatch.
    pub fn stop_work_item(&self, work_id: &str) -> Result<work_items::WorkItem, RuntimeError> {
        self.require_main_session()?;
        let item = self.inner.work_items.stop(work_id)?;
        self.inner
            .queue
            .stop_work(&item.work_id, item.stop_version)?;
        Ok(item)
    }
    /// Run the fixed program check for one criterion. The result is observed
    /// here, so a model cannot map arbitrary tool output to a pass.
    pub fn evaluate_work_acceptance(
        &self,
        work_id: &str,
        criterion_id: &str,
    ) -> Result<work_items::WorkItem, RuntimeError> {
        self.require_main_session()?;
        self.inner
            .work_items
            .evaluate_acceptance(work_id, criterion_id, &self.inner.root)
    }
    /// Write the user's own answer to a condition that cannot be checked by a
    /// program. The reference identifies the main-session answer.
    pub fn record_work_user_acceptance(
        &self,
        work_id: &str,
        criterion_id: &str,
        passed: bool,
        answer_ref: &str,
    ) -> Result<work_items::WorkItem, RuntimeError> {
        self.require_main_session()?;
        let item = self
            .inner
            .work_items
            .get(work_id)?
            .ok_or_else(|| RuntimeError::InvalidInput("WorkItem not found".into()))?;
        // The answer must be a real user-input log record in the owner main
        // session. Model text, tool output and web content are not answers.
        let evidence = self.memory_user_evidence(&item.owner_main_session_id)?;
        if !evidence
            .sources()
            .any(|source| source.log_id() == answer_ref)
        {
            return Err(RuntimeError::InvalidInput(
                "acceptance answer is not a verified main-session user input".into(),
            ));
        }
        self.inner
            .work_items
            .record_user_acceptance(work_id, criterion_id, passed, answer_ref)
    }
    /// Model completion reports are hints only. This always refuses to write a
    /// passing acceptance record.
    pub fn record_work_model_report(&self, work_id: &str, text: &str) -> Result<(), RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.record_model_report(work_id, text)
    }
    /// Persist a late result. It never reopens a paused/stopped WorkItem and
    /// never starts a second dispatch.
    pub fn record_work_late_result(
        &self,
        work_id: &str,
        task_id: &str,
        result_ref: &str,
        pause_version_at_dispatch: u64,
        stop_version_at_dispatch: u64,
    ) -> Result<work_items::LateWorkResult, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.record_late_result(
            work_id,
            task_id,
            result_ref,
            pause_version_at_dispatch,
            stop_version_at_dispatch,
        )
    }
    pub fn ingest_input(
        &self,
        source: &str,
        source_event_id: &str,
        log_id: &str,
        session_id: &str,
    ) -> Result<work_items::InputRecord, RuntimeError> {
        self.require_main_session()?;
        self.inner
            .work_items
            .ingest_input(source, source_event_id, log_id, session_id)
    }
    pub fn transition_input(
        &self,
        log_id: &str,
        state: work_items::InputState,
        work_id: Option<&str>,
        result_ref: Option<&str>,
    ) -> Result<work_items::InputRecord, RuntimeError> {
        self.require_main_session()?;
        self.inner
            .work_items
            .transition_input(log_id, state, work_id, result_ref)
    }
    pub fn input_record(
        &self,
        log_id: &str,
    ) -> Result<Option<work_items::InputRecord>, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.input(log_id)
    }
    /// Record a proactive trigger and its usage once. It never changes approval
    /// rules and never imposes a usage cap.
    pub fn trigger_proactive(
        &self,
        mode: work_items::ProactiveMode,
        rule_id: &str,
        condition_version: u64,
        work_id: &str,
        usage_tokens: u64,
    ) -> Result<work_items::ProactiveTriggerOutcome, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.trigger_proactive(
            mode,
            rule_id,
            condition_version,
            work_id,
            usage_tokens,
        )
    }
    pub fn proactive_mode(&self) -> work_items::ProactiveMode {
        self.inner.config.proactive_mode
    }
    /// Apply the configured proactive mode. This never touches approval rules.
    pub fn trigger_configured_proactive(
        &self,
        rule_id: &str,
        condition_version: u64,
        work_id: &str,
        usage_tokens: u64,
    ) -> Result<work_items::ProactiveTriggerOutcome, RuntimeError> {
        self.trigger_proactive(
            self.inner.config.proactive_mode,
            rule_id,
            condition_version,
            work_id,
            usage_tokens,
        )
    }
    /// Save a notification intent. Only a target from user configuration is
    /// accepted.
    pub fn prepare_notification(
        &self,
        work_id: &str,
        work_version: u64,
        target: &str,
        content_ref: Option<&str>,
        artifact_refs: &[String],
    ) -> Result<work_items::Notification, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.prepare_notification(
            work_id,
            work_version,
            target,
            content_ref,
            artifact_refs,
            &self.inner.config.notification_targets,
        )
    }
    pub fn record_notification_receipt(
        &self,
        notification_id: &str,
        status: work_items::NotificationStatus,
        receipt_ref: &str,
    ) -> Result<work_items::Notification, RuntimeError> {
        self.require_main_session()?;
        self.inner
            .work_items
            .record_notification_receipt(notification_id, status, receipt_ref)
    }
    pub fn notification_decision(
        &self,
        notification_id: &str,
    ) -> Result<work_items::NotificationDecision, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.notification_decision(notification_id)
    }
    pub fn notification(
        &self,
        notification_id: &str,
    ) -> Result<Option<work_items::Notification>, RuntimeError> {
        self.require_main_session()?;
        self.inner.work_items.notification(notification_id)
    }
    /// Push a pause version received from the Client to this workspace's live
    /// tasks. The WorkItem body stays in the main-session workspace.
    pub fn apply_work_pause(
        &self,
        work_id: &str,
        pause_version: u64,
    ) -> Result<usize, RuntimeError> {
        Ok(self.inner.queue.pause_work(work_id, pause_version)?)
    }
    pub fn apply_work_stop(&self, work_id: &str, stop_version: u64) -> Result<usize, RuntimeError> {
        Ok(self.inner.queue.stop_work(work_id, stop_version)?)
    }
    /// Read-only Git recovery status for this workspace. `None` when the host
    /// did not opt in (SDK or fully explicit Headless).
    pub fn recovery_status(&self) -> Option<serde_json::Value> {
        self.inner
            .recovery
            .as_ref()
            .map(|recovery| recovery.status())
    }
    /// Read-only inspection of an unfinished restore. It never rewrites the
    /// intent, reports success, or replays a file write.
    pub fn recovery_reconciliation(&self) -> Option<serde_json::Value> {
        self.inner
            .recovery
            .as_ref()
            .map(|recovery| recovery.reconciliation())
    }
    /// Disable Git recovery for this workspace through the runtime API. Calls
    /// after this stop creating points and local backups, and existing-file
    /// rewrites or deletions ask again. Existing points and backups stay.
    pub fn disable_git_recovery(&self) -> Result<serde_json::Value, RuntimeError> {
        let recovery =
            self.inner.recovery.as_ref().ok_or_else(|| {
                RuntimeError::InvalidInput("Git recovery is not configured".into())
            })?;
        recovery.disable().map_err(|error| {
            if error.outcome_unknown {
                RuntimeError::RecoveryRequired {
                    cause: Box::new(RuntimeError::Workspace(error.message)),
                    cleanup: None,
                    outcome_unknown: true,
                }
            } else {
                RuntimeError::Workspace(error.message)
            }
        })?;
        let retained = recovery.point_ids().map_or(0, |points| points.len());
        Ok(serde_json::json!({
            "enabled":false,
            "points_retained":retained,
            "snapshots_stopped":true,
            "existing_file_changes":"ask",
            "deleted_existing_points":0,
        }))
    }
    /// Newest-first ids of points this workspace created. The program never
    /// deletes a point or collects its old objects.
    pub fn recovery_points(&self) -> Result<Vec<String>, RuntimeError> {
        let recovery =
            self.inner.recovery.as_ref().ok_or_else(|| {
                RuntimeError::InvalidInput("Git recovery is not configured".into())
            })?;
        recovery.point_ids().map_err(RuntimeError::Workspace)
    }
    /// Explicitly establish a point now; identical content is not committed
    /// again. Git unavailability is reported instead of blocking work.
    pub fn create_recovery_point(&self) -> Result<serde_json::Value, RuntimeError> {
        let recovery =
            self.inner.recovery.as_ref().ok_or_else(|| {
                RuntimeError::InvalidInput("Git recovery is not configured".into())
            })?;
        let outcome = recovery
            .ensure_point("explicit request")
            .map_err(|error| RuntimeError::Workspace(error.message))?;
        serde_json::to_value(recovery_outcome(&outcome)).map_err(RuntimeError::Json)
    }
    /// Restore exact workspace-relative files from one CAYW recovery point.
    /// Conflicting files are reported and are not overwritten until the caller
    /// repeats those exact paths in `confirm_overwrite`.
    pub fn restore_recovery_point(
        &self,
        request: recovery_points::RestoreRequest,
    ) -> Result<recovery_points::RestoreReport, RuntimeError> {
        let recovery =
            self.inner.recovery.as_ref().ok_or_else(|| {
                RuntimeError::InvalidInput("Git recovery is not configured".into())
            })?;
        recovery.restore(request).map_err(|error| {
            if error.outcome_unknown {
                RuntimeError::RecoveryRequired {
                    cause: Box::new(RuntimeError::Workspace(error.message)),
                    cleanup: None,
                    outcome_unknown: true,
                }
            } else {
                RuntimeError::Workspace(error.message)
            }
        })
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
    /// Lost-response lookup for a dispatched task. The caller asks by the stable
    /// `dispatch_key` instead of creating a second task.
    pub fn task_by_dispatch_key(&self, key: &str) -> Result<Option<TaskRecord>, RuntimeError> {
        Ok(self.inner.queue.find_by_dispatch_key(key)?)
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
        let artifact_unknown = self.has_artifact_unknown(id)?;
        self.check_ready_snapshot(id, snapshot, artifact_unknown)
    }
    /// Same readiness contract while the caller already owns this Session's
    /// artifact executor. Artifact state is observed without re-acquiring the
    /// executor, so a live action does not reject itself.
    pub(crate) fn check_ready_locked(&self, id: &str) -> Result<SessionView, RuntimeError> {
        let snapshot = self.get(id)?;
        let artifact_unknown = self.artifact_unknown(id, false)?;
        self.check_ready_snapshot(id, snapshot, artifact_unknown)
    }
    fn check_ready_snapshot(
        &self,
        id: &str,
        snapshot: SessionView,
        artifact_unknown: bool,
    ) -> Result<SessionView, RuntimeError> {
        if snapshot.state == SessionState::NeedsReconciliation
            || self
                .inner
                .cells
                .is_frozen(id)
                .map_err(|error| RuntimeError::Workspace(error.to_string()))?
            || artifact_unknown
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

    #[test]
    fn readiness_rejects_a_frozen_binding() {
        // Break caught: artifact InvokeTool must reuse the same readiness
        // contract as prompts, including a frozen plugin binding.
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let id = runtime.create(None).unwrap().id;
        let mut view = runtime.get(&id).unwrap();
        view.frozen = true;
        let error = runtime.check_ready_snapshot(&id, view, false).unwrap_err();
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
            matches!(error, RuntimeError::NeedsReconciliation),
            "{error:?}"
        );
    }
}
