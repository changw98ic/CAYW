//! Host-owned, permission-gated capabilities for bounded CodeMode execution.
//!
//! The host supplies permission decisions, durable audit storage, process profiles,
//! and optional external services. Catalog membership never grants permission.

mod catalog;
mod filesystem;
mod git;
mod process;
mod read_fence;
pub mod terminal;
pub use git::GitWritePolicy;
pub use read_fence::{
    FenceRecovery, FrozenReadStamp, FrozenReadTicket, FrozenReadValue, PendingMutation,
    ReadFenceFault, ReadFenceRecovery,
};
pub use terminal::{TerminalConfig, TerminalLimits};

use async_trait::async_trait;
use cap_std::fs::Dir;
use cayw_ai::{ArgumentValidator, CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost, CapabilityStop};
pub use cayw_device::{DeviceError, ProcessIdentity, ProcessIo, ProcessObserver};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// A permission decision for the exact capability and validated JSON arguments.
/// Implementations must honor cancellation while awaiting a human decision.
#[async_trait]
pub trait PermissionGate: Send + Sync {
    async fn authorize(
        &self,
        capability: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError>;
}

/// Persistence boundary for capability execution.
///
/// `started` must durably commit a running record before returning its unique ID.
/// `settled` must durably record the complete result before returning success.
/// Neither method may report success for a buffered, uncommitted write. A running
/// record left by a crash is unknown until the application reconciles it.
/// `dropped` must synchronously retain unknown status; its failure is also exposed
/// through `Host::audit_failure`, and permanently stops this host instance.
#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn started(&self, capability: &str, arguments: &Value)
    -> Result<String, CapabilityError>;
    async fn settled(
        &self,
        call_id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError>;
    fn dropped(&self, call_id: &str) -> Result<(), CapabilityError>;
}

/// Existing application services, injected without creating a loader or connection.
///
/// Names are `search.query`, `mcp.discover`, `mcp.describe`, `mcp.call`,
/// `skills.list`, and `skills.read`. Their schemas are in `Host::capabilities`.
/// The child cancellation token is cancelled on timeout, cancellation, or drop.
/// Dropping a future must not silently detach work. If completion cannot be
/// established, return `CapabilityError::unknown` and retain reconciliation data.
#[async_trait]
pub trait ExternalHost: Send + Sync {
    async fn call(
        &self,
        capability: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileKind {
    Build,
    Test,
    Check,
    Lint,
    Format,
}

/// A command selected entirely by the application. No shell is inserted.
/// `allow_filter` permits one restricted, positional filter for a test profile.
#[derive(Debug, Clone)]
pub struct Profile {
    pub kind: ProfileKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub allow_filter: bool,
    pub io: cayw_device::ProcessIo,
}

#[derive(Clone)]
pub struct HostConfig {
    /// Trusted opt-in to a cooperating, explicitly frozen fs.read view. Outside
    /// editors ignoring Host fences are not observed automatically.
    pub frozen_reads: bool,
    pub max_argument_bytes: usize,
    pub max_result_bytes: usize,
    /// Combined serialized argument and result bytes across this host instance.
    pub max_total_bytes: usize,
    pub max_calls: usize,
    /// Per-file bound for ordinary file reads and for each copied Git metadata
    /// file. Git reads no longer copy object data or the worktree, so repository
    /// size and ignored build data do not gate a read. Directory listings remain
    /// bounded by `max_list_entries`.
    pub max_file_bytes: usize,
    /// Directory listing bound and copied Git metadata entry allowance.
    pub max_list_entries: usize,
    /// Includes permission waiting and execution. Durable audit writes are awaited
    /// even after cancellation so a caller cannot mistake an uncommitted result.
    pub call_timeout: Duration,
    /// Absolute application-selected git executable; absent means no git catalog.
    /// Read commands use a sanitized private metadata directory and the held root
    /// in place. Worktree symlinks are not followed; a changed object-store
    /// identity discards the result. Concurrent in-place source edits do not have
    /// transaction snapshot semantics.
    pub git_program: Option<PathBuf>,
    /// Opt-in for real-repository Git writes. Absent means no write capability.
    /// The identity is host-owned; capabilities are individually policy-gated.
    pub git_writes: Option<GitWritePolicy>,
    pub process_timeout: Duration,
    pub max_process_output_bytes: usize,
    pub profiles: BTreeMap<String, Profile>,
    /// Host-selected interactive profiles. None omits terminal handoffs.
    pub terminal_handoffs: Option<TerminalConfig>,
    /// Pure, bounded data capabilities. None omits them from the catalog.
    pub data: Option<cayw_data::Limits>,
    /// Tree, change-batch and archive capabilities. Opening is deferred until an
    /// authorized, audited execution; catalog inspection never creates a store.
    /// Pending batches fence ordinary writes and profiles even when this is None.
    pub workspace: Option<cayw_workspace::Limits>,
    pub external: Option<Arc<dyn ExternalHost>>,
}
impl Default for HostConfig {
    fn default() -> Self {
        Self {
            frozen_reads: false,
            max_argument_bytes: 2 * 1024 * 1024,
            max_result_bytes: 2 * 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
            max_calls: 256,
            max_file_bytes: 1024 * 1024,
            max_list_entries: 4096,
            call_timeout: Duration::from_secs(60),
            git_program: None,
            git_writes: None,
            process_timeout: Duration::from_secs(30),
            max_process_output_bytes: 256 * 1024,
            profiles: BTreeMap::new(),
            terminal_handoffs: None,
            data: Some(cayw_data::Limits::default()),
            workspace: Some(cayw_workspace::Limits::default()),
            external: None,
        }
    }
}
impl HostConfig {
    fn validate(&self) -> Result<(), CapabilityError> {
        if self.max_argument_bytes == 0
            || self.max_result_bytes < 2
            || self.max_total_bytes < 2
            || self.max_calls == 0
            || self.max_file_bytes == 0
            || self.max_list_entries == 0
            || self.max_process_output_bytes == 0
            || self.call_timeout.is_zero()
            || self.process_timeout.is_zero()
            || Instant::now().checked_add(self.call_timeout).is_none()
            || Instant::now().checked_add(self.process_timeout).is_none()
            || self.max_file_bytes > 64 * 1024 * 1024
            || self.max_list_entries > 100_000
        {
            return Err(failure(
                "host budgets must be positive, bounded, and have representable deadlines",
            ));
        }
        if let Some(limits) = self.data {
            limits.validate()?;
        }
        if let Some(limits) = &self.workspace {
            limits.validate()?;
        }
        if let Some(program) = &self.git_program {
            process::validate_program(program)?;
        }
        git::validate_config(self)?;
        if let Some(config) = &self.terminal_handoffs {
            terminal::validate_config(config)?;
            if config
                .limits
                .max_read_bytes
                .saturating_mul(4)
                .saturating_add(4096)
                > self.max_result_bytes
            {
                return Err(failure("terminal read page exceeds host result budget"));
            }
        }
        for (name, profile) in &self.profiles {
            if !process::valid_name(name) {
                return Err(failure("invalid profile name"));
            }
            process::validate_program(&profile.program)?;
            if let cayw_device::ProcessIo::Pty { columns, rows } = profile.io
                && (columns == 0 || rows == 0)
            {
                return Err(failure("terminal dimensions must be positive"));
            }
            #[cfg(not(target_os = "macos"))]
            if matches!(profile.io, cayw_device::ProcessIo::Pty { .. }) {
                return Err(failure("terminal execution is only supported on macOS"));
            }
            if profile.timeout.is_zero()
                || Instant::now().checked_add(profile.timeout).is_none()
                || profile.max_output_bytes == 0
                || (profile.allow_filter && profile.kind != ProfileKind::Test)
                || profile.args.len() > 256
                || profile
                    .args
                    .iter()
                    .any(|arg| arg.contains('\0') || arg.len() > 8192)
                || profile.env.len() > 256
                || profile.env.iter().any(|(key, value)| {
                    key.is_empty()
                        || key.contains(['=', '\0'])
                        || value.contains('\0')
                        || key.len() > 1024
                        || value.len() > 32768
                })
            {
                return Err(failure("invalid process profile"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Executor {
    Filesystem,
    Git,
    Profile,
    Terminal,
    Data,
    Workspace,
    External,
}

struct Capability {
    definition: ToolDefinition,
    validator: ArgumentValidator,
    executor: Executor,
}
#[derive(Default)]
struct Budget {
    calls: usize,
    bytes: usize,
    reserved: usize,
    exhausted: bool,
}
#[derive(Default)]
struct Safety {
    unknown: AtomicBool,
    audit_failure: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct Host {
    root: Arc<Dir>,
    gate: Arc<dyn PermissionGate>,
    audit: Arc<dyn AuditSink>,
    config: Arc<HostConfig>,
    catalog: Arc<BTreeMap<String, Capability>>,
    budget: Arc<Mutex<Budget>>,
    execution: Arc<tokio::sync::Mutex<()>>,
    safety: Arc<Safety>,
    runner: Arc<cayw_device::ProcessRunner>,
    terminal: Option<Arc<terminal::TerminalManager>>,
    external: Option<Arc<dyn ExternalHost>>,
}
impl Host {
    /// Opens one directory capability. File operations never reopen an ambient
    /// path after validation; each component is opened without following links.
    pub fn new(
        root: impl AsRef<Path>,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
        config: HostConfig,
    ) -> Result<Self, CapabilityError> {
        let root_path = root.as_ref().canonicalize().map_err(io_error)?;
        let root =
            Dir::open_ambient_dir(&root_path, cap_std::ambient_authority()).map_err(io_error)?;
        Self::from_root(root, gate, audit, config)
    }
    /// Creates a separate task's admission and process state over this same held
    /// directory. It never clears this Host's safety, budgets or pending fences.
    pub fn task_host(
        &self,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
    ) -> Result<Self, CapabilityError> {
        Self::from_root(
            self.root.try_clone().map_err(io_error)?,
            gate,
            audit,
            (*self.config).clone(),
        )
    }
    fn from_root(
        root: Dir,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
        config: HostConfig,
    ) -> Result<Self, CapabilityError> {
        config.validate()?;
        let mut catalog = BTreeMap::new();
        for (definition, executor) in catalog::definitions(&config) {
            let validator = ArgumentValidator::new(&definition.parameters)
                .map_err(|error| failure(error.to_string()))?;
            if catalog
                .insert(
                    definition.name.clone(),
                    Capability {
                        definition,
                        validator,
                        executor,
                    },
                )
                .is_some()
            {
                return Err(failure("duplicate host capability"));
            }
        }
        let runner = process::runner(&config)?;
        let terminal = config
            .terminal_handoffs
            .clone()
            .map(terminal::TerminalManager::new)
            .transpose()?
            .map(Arc::new);
        let external = config.external.clone();
        Ok(Self {
            root: Arc::new(root),
            gate,
            audit,
            config: Arc::new(config),
            catalog: Arc::new(catalog),
            budget: Arc::default(),
            execution: Arc::new(tokio::sync::Mutex::new(())),
            safety: Arc::default(),
            runner: Arc::new(runner),
            terminal,
            external,
        })
    }
    /// Identity of the directory actually held open by this host. Independent
    /// opens of the same live directory agree; renaming/replacing its pathname
    /// cannot change this identity or redirect the metadata lookup.
    pub fn workspace_identity(&self) -> Result<String, CapabilityError> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            let metadata = self.root.dir_metadata().map_err(io_error)?;
            Ok(format!("unix:{:x}:{:x}", metadata.dev(), metadata.ino()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[repr(C)]
            struct FileIdInfo {
                volume: u64,
                id: [u8; 16],
            }
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetFileInformationByHandleEx(
                    file: *mut std::ffi::c_void,
                    class: i32,
                    info: *mut std::ffi::c_void,
                    size: u32,
                ) -> i32;
            }
            let mut info = FileIdInfo {
                volume: 0,
                id: [0; 16],
            };
            // SAFETY: root owns this live directory handle; FileIdInfo matches
            // FILE_ID_INFO and the API writes only the supplied output buffer.
            let result = unsafe {
                GetFileInformationByHandleEx(
                    self.root.as_raw_handle(),
                    18, // FileIdInfo
                    (&mut info as *mut FileIdInfo).cast(),
                    std::mem::size_of::<FileIdInfo>() as u32,
                )
            };
            if result == 0 {
                return Err(io_error(io::Error::last_os_error()));
            }
            Ok(format!(
                "windows:{:x}:{:032x}",
                info.volume,
                u128::from_le_bytes(info.id)
            ))
        }
        #[cfg(not(any(unix, windows)))]
        Err(failure(
            "opened workspace identity is unsupported on this platform",
        ))
    }

    /// Creates an immutable permission/audit scope over the same open root,
    /// configuration, catalog, budgets, execution lane, safety and process runner.
    /// No filesystem paths are reopened and no state is reset, even if stopped.
    pub fn scoped(
        &self,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
    ) -> Result<Self, CapabilityError> {
        Ok(Self {
            root: self.root.clone(),
            gate,
            audit,
            config: self.config.clone(),
            catalog: self.catalog.clone(),
            budget: self.budget.clone(),
            execution: self.execution.clone(),
            safety: self.safety.clone(),
            runner: self.runner.clone(),
            terminal: self.terminal.clone(),
            external: self.external.clone(),
        })
    }

    /// Adds durable OS process observations to this scope. The original runner's
    /// ownership, admission, cancellation and cleanup ledger remain shared.
    pub fn with_process_observer(&self, observer: Arc<dyn ProcessObserver>) -> Self {
        let mut scope = self.clone();
        scope.runner = Arc::new(self.runner.observed(observer));
        scope
    }

    /// Overrides this scope's application service while sharing the original
    /// root, config, catalog, cumulative budgets, execution lane, safety, and
    /// process runner. Ordinary scopes derived from this one inherit the service.
    /// A host without an external catalog cannot enable one through a scope.
    pub fn scoped_external(
        &self,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
        external: Arc<dyn ExternalHost>,
    ) -> Result<Self, CapabilityError> {
        if self.config.external.is_none() {
            return Err(failure("external capability catalog is not configured"));
        }
        let mut scope = self.scoped(gate, audit)?;
        scope.external = Some(external);
        Ok(scope)
    }

    /// Calls an application-supplied service through this host's existing
    /// permissions, cumulative budget, ordered execution, safety and audit. The
    /// definition is trusted Rust SDK input and never expands the base catalog.
    /// Services may mutate state; an unretainable response remains unknown.
    pub async fn call_service(
        &self,
        definition: &ToolDefinition,
        arguments: Value,
        cancel: CancellationToken,
        service: &dyn ExternalHost,
    ) -> Result<Value, CapabilityError> {
        let arguments = Arguments(arguments);
        let admitted = (|| {
            if self.catalog.contains_key(&definition.name) {
                return Err(failure("service name already belongs to the host catalog"));
            }
            if !process::valid_name(&definition.name) {
                return Err(failure("invalid service capability name"));
            }
            if definition.description.len() > self.config.max_argument_bytes {
                return Err(failure("service description exceeds byte limit"));
            }
            json_size(&definition.parameters, self.config.max_argument_bytes)?;
            let validator = ArgumentValidator::new(&definition.parameters)
                .map_err(|error| failure(error.to_string()))?;
            Ok(Capability {
                definition: definition.clone(),
                validator,
                executor: Executor::External,
            })
        })();
        let capability = admitted.map_err(|error| self.diagnostic(error))?;
        self.call_inner(&capability, arguments, cancel, Some(service))
            .await
    }

    /// Permanently closes process admission across this task's host scopes and
    /// requests cancellation. Cells should be closed first by the runtime.
    pub fn close_processes(&self) {
        if let Some(terminal) = &self.terminal {
            terminal.close();
        }
        self.runner.close();
    }

    /// Supervisors still owned by this task, including dropped call futures.
    pub fn active_processes(&self) -> usize {
        self.runner.active_count()
    }

    /// Terminal sessions whose original supervisor has not joined.
    pub fn active_terminal_sessions(&self) -> usize {
        self.terminal
            .as_ref()
            .map_or(0, |terminal| terminal.active_sessions())
    }

    /// Checks terminal completion before cleanup cancels any remaining sessions.
    pub fn terminal_completion_error(&self) -> Option<String> {
        self.terminal
            .as_ref()
            .and_then(|terminal| terminal.completion_error())
    }

    /// Waits for actual process cleanup and supervisor exit across all scopes.
    /// Runtime task reservations must remain held until this returns Ok. Dropping
    /// this future only requests cancellation; call again to establish cleanup.
    pub async fn drain_processes(&self) -> Result<(), cayw_device::DeviceError> {
        if let Some(terminal) = &self.terminal {
            terminal.close();
        }
        let terminal_result = match &self.terminal {
            Some(terminal) => terminal.drain().await,
            None => Ok(()),
        };
        let process_result = self.runner.drain().await;
        match (terminal_result, process_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(terminal), Err(process)) => Err(cayw_device::DeviceError::CleanupUnconfirmed(
                format!("terminal cleanup: {terminal}; process cleanup: {process}"),
            )),
        }
    }

    /// Unknown effects cannot be cleared by resetting budgets or reusing a host.
    /// The application must reconcile its durable records before creating another.
    pub fn outcome_unknown(&self) -> bool {
        self.safety.unknown.load(Ordering::Acquire)
            || self.runner.cleanup_failure().is_some()
            || self
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.unknown().is_some())
    }
    pub fn audit_failure(&self) -> Option<String> {
        lock(&self.safety.audit_failure).clone()
    }
    fn check(&self, cancel: &CancellationToken, deadline: Instant) -> Result<(), CapabilityError> {
        if self.outcome_unknown() {
            let reason = self.audit_failure().map_or_else(
                || "a previous capability requires reconciliation".into(),
                |error| format!("audit persistence failed: {error}"),
            );
            return Err(CapabilityError::unknown(reason));
        }
        check(cancel, deadline)?;
        if lock(&self.budget).exhausted {
            return Err(CapabilityError::stopped(
                CapabilityStop::Budget,
                "host total byte budget exhausted",
            ));
        }
        Ok(())
    }
    fn admit(&self, bytes: usize) -> Result<ResponseBudget, CapabilityError> {
        let mut budget = lock(&self.budget);
        let available = self
            .config
            .max_total_bytes
            .saturating_sub(budget.bytes)
            .saturating_sub(budget.reserved);
        // Reserve empty JSON messages for both the result sent to audit and a
        // possible settlement failure. Concurrent queued calls cannot consume it.
        if budget.exhausted
            || budget.calls >= self.config.max_calls
            || bytes > available.saturating_sub(4)
            || available < 4
        {
            return Err(CapabilityError::stopped(
                CapabilityStop::Budget,
                "host call or total byte budget exhausted",
            ));
        }
        budget.calls += 1;
        budget.bytes += bytes;
        budget.reserved += 4;
        Ok(ResponseBudget {
            host: self.clone(),
            reserved: 4,
        })
    }
    fn diagnostic(&self, error: CapabilityError) -> CapabilityError {
        ResponseBudget {
            host: self.clone(),
            reserved: 0,
        }
        .error(error)
    }
    async fn execute(
        &self,
        executor: Executor,
        name: &str,
        args: Value,
        cancel: &CancellationToken,
        deadline: Instant,
        service: Option<&dyn ExternalHost>,
    ) -> Result<Value, CapabilityError> {
        match executor {
            Executor::Filesystem => {
                if matches!(name, "fs.write" | "fs.patch") {
                    let (workspace, workspace_deadline) =
                        self.open_workspace(cancel, deadline).await?;
                    let _lock = filesystem::write_lock(&self.root, cancel, deadline).await?;
                    self.ensure_workspace_settled(&workspace, cancel, workspace_deadline)
                        .await?;
                    filesystem::execute(self, name, &args, cancel, deadline).await
                } else {
                    filesystem::execute(self, name, &args, cancel, deadline).await
                }
            }
            Executor::Git => git::execute(self, name, &args, cancel, deadline).await,
            Executor::Terminal => {
                let manager = self
                    .terminal
                    .as_ref()
                    .ok_or_else(|| failure("terminal handoffs are disabled"))?;
                terminal::execute(
                    manager,
                    &self.root,
                    self.runner.clone(),
                    name,
                    &args,
                    cancel,
                    deadline,
                )
                .await
            }
            Executor::Profile => {
                let (workspace, workspace_deadline) = self.open_workspace(cancel, deadline).await?;
                let _lock = filesystem::write_lock(&self.root, cancel, deadline).await?;
                self.ensure_workspace_settled(&workspace, cancel, workspace_deadline)
                    .await?;
                process::execute(self, name, &args, cancel, deadline).await
            }
            Executor::Data => {
                let limits = self
                    .config
                    .data
                    .ok_or_else(|| failure("data capabilities are disabled"))?;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(CapabilityError::stopped(CapabilityStop::Cancelled, "data capability cancelled")),
                    _ = tokio::time::sleep_until(deadline.into()) => Err(CapabilityError::stopped(CapabilityStop::Timeout, "data capability deadline reached")),
                    result = cayw_data::execute(name, args, cancel.clone(), limits) => result,
                }
            }
            Executor::Workspace => {
                let (workspace, workspace_deadline) = self.open_workspace(cancel, deadline).await?;
                workspace
                    .execute_until(name, &args, cancel.clone(), workspace_deadline)
                    .await
            }
            Executor::External => {
                let external = service
                    .or(self.external.as_deref())
                    .ok_or_else(|| failure("external service is not configured"))?;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(CapabilityError::unknown("external capability cancelled after admission")),
                    _ = tokio::time::sleep_until(deadline.into()) => Err(CapabilityError::unknown("external capability timed out after admission")),
                    result = external.call(name, args, cancel.clone()) => result,
                }
            }
        }
    }

    async fn open_workspace(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<(cayw_workspace::Workspace, Instant), CapabilityError> {
        check(cancel, deadline)?;
        let mut limits = self.config.workspace.clone().unwrap_or_default();
        limits.max_file_bytes = limits.max_file_bytes.min(self.config.max_file_bytes);
        limits.max_tree_entries = limits.max_tree_entries.min(self.config.max_list_entries);
        let deadline = deadline.min(
            Instant::now()
                .checked_add(limits.timeout)
                .ok_or_else(|| failure("invalid workspace deadline"))?,
        );
        loop {
            check(cancel, deadline)?;
            match cayw_workspace::Workspace::open_dir_until(
                self.root.try_clone().map_err(io_error)?,
                limits.clone(),
                cancel.clone(),
                deadline,
            ) {
                Ok(workspace) => return Ok((workspace, deadline)),
                Err(error)
                    if !error.outcome_unknown
                        && error.message.starts_with("workspace is busy:") =>
                {
                    // A concurrent fork briefly inherits unrelated lock fds until
                    // exec. Treat that known, nonmutating refusal like write-lock
                    // admission, without extending the original call deadline.
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(CapabilityError::stopped(CapabilityStop::Cancelled, "cancelled while opening workspace")),
                        _ = tokio::time::sleep_until(deadline.into()) => return Err(CapabilityError::stopped(CapabilityStop::Timeout, "workspace opening deadline reached")),
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {},
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Must be called while holding the same OS lock as the subsequent mutation.
    /// A pending journal is a known refusal before this call has changed anything;
    /// it does not clear any earlier unknown outcome on this host.
    async fn ensure_workspace_settled(
        &self,
        workspace: &cayw_workspace::Workspace,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), CapabilityError> {
        check(cancel, deadline)?;
        let status = workspace
            .execute_until(
                "fs.batch_status",
                &serde_json::json!({}),
                cancel.clone(),
                deadline,
            )
            .await?;
        let batches = status
            .get("batches")
            .and_then(Value::as_array)
            .ok_or_else(|| failure("workspace status evidence is unavailable"))?;
        for batch in batches {
            if batch.get("outcome_unknown").and_then(Value::as_bool) != Some(false) {
                return Err(failure(
                    "workspace batch requires explicit recovery before writes or process profiles",
                ));
            }
        }
        check(cancel, deadline)
    }
}

#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.catalog
            .values()
            .map(|cap| cap.definition.clone())
            .collect()
    }
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let arguments = Arguments(arguments);
        let capability = self
            .catalog
            .get(name)
            .ok_or_else(|| self.diagnostic(failure("capability is not configured")))?;
        self.call_inner(capability, arguments, cancel, None).await
    }
}

impl Host {
    async fn call_inner(
        &self,
        capability: &Capability,
        arguments: Arguments,
        cancel: CancellationToken,
        service: Option<&dyn ExternalHost>,
    ) -> Result<Value, CapabilityError> {
        let deadline = Instant::now()
            .checked_add(self.config.call_timeout)
            .ok_or_else(|| self.diagnostic(failure("invalid deadline")))?;
        // Ordinary calls retain ordered permission/audit admission. Frozen reads
        // never need this lane to settle or acknowledge their original ticket.
        let lane = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(self.diagnostic(CapabilityError::stopped(CapabilityStop::Cancelled, "capability cancelled while queued"))),
            _ = tokio::time::sleep_until(deadline.into()) => return Err(self.diagnostic(CapabilityError::stopped(CapabilityStop::Timeout, "capability queue deadline reached"))),
            guard = self.execution.lock() => guard,
        };
        let mut call = self
            .admit_owned(capability, arguments, cancel, service.is_some(), deadline)
            .await?;
        let name = capability.definition.name.as_str();
        let mutation = if service.is_some()
            || catalog::may_mutate(name)
            || matches!(capability.executor, Executor::External)
        {
            match self
                .mutation(
                    name,
                    &call.arguments.0,
                    &call.guard.id,
                    service.is_some()
                        || matches!(capability.executor, Executor::External | Executor::Profile),
                    &call.child,
                    call.deadline,
                )
                .await
            {
                Ok(mutation) => Some(mutation),
                Err(error) => return call.settle(Err(error), false).await,
            }
        } else {
            None
        };
        let result = match self.check(&call.child, call.deadline) {
            Ok(()) => {
                self.execute(
                    capability.executor,
                    name,
                    std::mem::take(&mut call.arguments.0),
                    &call.child,
                    call.deadline,
                    service,
                )
                .await
            }
            Err(error) => Err(error),
        };
        let result = match self.runner.cleanup_failure() {
            Some(message) => Err(CapabilityError::unknown(format!(
                "process resource cleanup is unconfirmed: {message}"
            ))),
            None => result,
        };
        let result = call.settle(result, mutation.is_some()).await;
        if !result.as_ref().is_err_and(|error| error.outcome_unknown)
            && let Some(mutation) = mutation
        {
            mutation.settle(self)?;
        }
        drop(lane);
        result
    }

    /// The one admission pipeline used by ordinary calls and frozen read tickets.
    async fn admit_owned(
        &self,
        capability: &Capability,
        arguments: Arguments,
        cancel: CancellationToken,
        service: bool,
        deadline: Instant,
    ) -> Result<OwnedCall, CapabilityError> {
        let name = capability.definition.name.as_str();
        let child = cancel.child_token();
        let cancel_on_drop = CancelCall(child.clone());
        let admission = (|| {
            self.check(&child, deadline)?;
            let argument_limit = match capability.executor {
                Executor::Data => self.config.data.map(|limits| limits.input_bytes),
                Executor::Workspace => self
                    .config
                    .workspace
                    .as_ref()
                    .map(|limits| limits.max_argument_bytes),
                _ => None,
            }
            .unwrap_or(self.config.max_argument_bytes)
            .min(self.config.max_argument_bytes);
            let size = json_size(&arguments.0, argument_limit)?;
            capability
                .validator
                .validate(&arguments.0)
                .map_err(|error| failure(error.to_string()))?;
            if !service {
                catalog::validate_arguments(name, &arguments.0, &self.config)?;
            }
            self.admit(size)
        })();
        let mut response = match admission {
            Ok(admitted) => admitted,
            Err(error) => return Err(self.diagnostic(error)),
        };
        // Permission may wait for a user. It holds no lane needed by a reader
        // completing its original Core/Cells acknowledgment.
        self.check(&child, deadline)
            .map_err(|error| response.error(error))?;
        tokio::select! {
            biased;
            _ = child.cancelled() => return Err(response.error(CapabilityError::stopped(CapabilityStop::Cancelled, "capability cancelled before permission"))),
            _ = tokio::time::sleep_until(deadline.into()) => return Err(response.error(CapabilityError::stopped(CapabilityStop::Timeout, "capability permission deadline reached"))),
            result = self.gate.authorize(name, &arguments.0, child.clone()) => {
                if let Err(error) = result {
                    if error.outcome_unknown { self.safety.unknown.store(true, Ordering::Release); }
                    return Err(response.error(error));
                }
            },
        }
        self.check(&child, deadline)
            .map_err(|error| response.error(error))?;
        let mut starting = StartingGuard {
            safety: self.safety.clone(),
            cancel: child.clone(),
            max_error_bytes: self.config.max_result_bytes,
            pending: true,
        };
        let started = self.audit.started(name, &arguments.0).await;
        starting.pending = false;
        let id = started.map_err(|error| {
            if error.outcome_unknown {
                self.safety.unknown.store(true, Ordering::Release);
            }
            response.error(error)
        })?;
        let guard = CallGuard {
            id,
            audit: self.audit.clone(),
            safety: self.safety.clone(),
            cancel: child.clone(),
            max_error_bytes: self.config.max_result_bytes,
            settled: false,
        };
        Ok(OwnedCall {
            host: self.clone(),
            arguments,
            response,
            child,
            deadline,
            guard,
            _cancel_on_drop: cancel_on_drop,
        })
    }
    /// Independently validates, charges and authorizes this exact original call.
    /// The non-Clone ticket owns its audit and the shared generation lease.
    pub async fn admit_frozen_read(
        &self,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<FrozenReadTicket, CapabilityError> {
        let capability = self.catalog.get("fs.read").expect("filesystem catalog");
        let deadline = Instant::now()
            .checked_add(self.config.call_timeout)
            .ok_or_else(|| self.diagnostic(failure("invalid deadline")))?;
        let mut call = self
            .admit_owned(capability, Arguments(arguments), cancel, false, deadline)
            .await?;
        match self.read_lease(&call.child, call.deadline).await {
            Ok(lease) => Ok(FrozenReadTicket::new(call, lease)),
            Err(error) => {
                let error = call
                    .settle(Err(error), false)
                    .await
                    .expect_err("read lease failed");
                Err(error)
            }
        }
    }
}

struct OwnedCall {
    host: Host,
    arguments: Arguments,
    response: ResponseBudget,
    child: CancellationToken,
    deadline: Instant,
    guard: CallGuard,
    _cancel_on_drop: CancelCall,
}
struct CancelCall(CancellationToken);
impl Drop for CancelCall {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
impl OwnedCall {
    async fn settle(
        &mut self,
        result: Result<Value, CapabilityError>,
        may_mutate: bool,
    ) -> Result<Value, CapabilityError> {
        if self.guard.settled {
            return Err(failure("Host call already settled"));
        }
        self.child.cancel();
        let result = self.response.result(result, may_mutate);
        if result.as_ref().is_err_and(|error| error.outcome_unknown) {
            self.host.safety.unknown.store(true, Ordering::Release);
        }
        if let Err(mut error) = self.host.audit.settled(&self.guard.id, &result).await {
            error.message = fit_message(error.message, self.host.config.max_result_bytes);
            *lock(&self.host.safety.audit_failure) = Some(error.message.clone());
            self.host.safety.unknown.store(true, Ordering::Release);
            return Err(self.response.error(CapabilityError::unknown(format!(
                "could not durably settle capability: {}",
                error.message
            ))));
        }
        self.guard.settled = true;
        result
    }
}

// Each admitted call reserves a result plus a possible audit-settlement error.
// Reservations are shared across scopes and released even if its future drops.
struct ResponseBudget {
    host: Host,
    reserved: usize,
}
impl ResponseBudget {
    fn available(&self, budget: &Budget, keep: usize) -> usize {
        self.host
            .config
            .max_total_bytes
            .saturating_sub(budget.bytes)
            .saturating_sub(budget.reserved.saturating_sub(self.reserved))
            .saturating_sub(keep)
    }
    fn release(&mut self, budget: &mut Budget, keep: usize) {
        budget.reserved -= self.reserved.saturating_sub(keep);
        self.reserved = keep;
    }
    fn error(&mut self, error: CapabilityError) -> CapabilityError {
        self.error_keeping(error, 0)
    }
    fn error_keeping(&mut self, mut error: CapabilityError, keep: usize) -> CapabilityError {
        let shared = self.host.budget.clone();
        let mut budget = lock(&shared);
        let available = self.available(&budget, keep);
        // An already exhausted caller has no encodable result allowance. Return
        // only the empty terminal control error, without another admission. All
        // encodable diagnostics below are charged at their full serialized size.
        if available < 2 {
            budget.exhausted = true;
            error.stop.get_or_insert(CapabilityStop::Budget);
            error.message.clear();
            self.release(&mut budget, keep);
            return error;
        }
        let result_limit = self.host.config.max_result_bytes;
        error.message = fit_message(error.message, result_limit);
        let limit = result_limit.min(available);
        let size = serde_json::to_vec(&error.message)
            .expect("string JSON serialization")
            .len();
        if size > available {
            budget.exhausted = true;
            error.stop.get_or_insert(CapabilityStop::Budget);
            error.message = fit_message(error.message, limit);
        }
        let size = serde_json::to_vec(&error.message)
            .expect("string JSON serialization")
            .len();
        budget.bytes += size;
        self.release(&mut budget, keep);
        error
    }
    fn result(
        &mut self,
        result: Result<Value, CapabilityError>,
        may_mutate: bool,
    ) -> Result<Value, CapabilityError> {
        let value = match result {
            Ok(value) => value,
            Err(error) => return Err(self.error_keeping(error, 2)),
        };
        let charged = {
            let shared = self.host.budget.clone();
            let mut budget = lock(&shared);
            let limit = self
                .host
                .config
                .max_result_bytes
                .min(self.available(&budget, 2));
            match json_size(&value, limit) {
                Ok(size) => {
                    budget.bytes += size;
                    self.release(&mut budget, 2);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        };
        match charged {
            Ok(()) => Ok(value),
            Err(error) => Err(self.error_keeping(
                if may_mutate {
                    CapabilityError::unknown(error.message)
                } else {
                    error
                },
                2,
            )),
        }
    }
}
impl Drop for ResponseBudget {
    fn drop(&mut self) {
        lock(&self.host.budget).reserved -= self.reserved;
    }
}

/// Fits the serialized JSON string, including quotes, escapes and UTF-8 bytes.
fn fit_message(mut message: String, limit: usize) -> String {
    let mut used = 2usize;
    let mut end = 0;
    for (at, character) in message.char_indices() {
        let bytes = match character {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            '\u{0}'..='\u{1f}' => 6,
            _ => character.len_utf8(),
        };
        if bytes > limit.saturating_sub(used) {
            break;
        }
        used += bytes;
        end = at + character.len_utf8();
    }
    message.truncate(end);
    message
}

// Rejected direct Rust calls may supply a Value far deeper than the host limit.
// Dispose of those iteratively even on cancellation or an unconfigured name.
struct Arguments(Value);
impl Drop for Arguments {
    fn drop(&mut self) {
        let mut pending = vec![std::mem::take(&mut self.0)];
        while let Some(value) = pending.pop() {
            match value {
                Value::Array(values) => pending.extend(values),
                Value::Object(values) => pending.extend(values.into_values()),
                _ => {}
            }
        }
    }
}

// If starting was interrupted, the sink may already have durably committed a
// running record whose ID was never returned. Preserve that uncertainty too.
struct StartingGuard {
    safety: Arc<Safety>,
    cancel: CancellationToken,
    max_error_bytes: usize,
    pending: bool,
}
impl Drop for StartingGuard {
    fn drop(&mut self) {
        if self.pending {
            self.cancel.cancel();
            self.safety.unknown.store(true, Ordering::Release);
            *lock(&self.safety.audit_failure) = Some(fit_message(
                "audit start was interrupted before its durable result was confirmed".into(),
                self.max_error_bytes,
            ));
        }
    }
}

struct CallGuard {
    id: String,
    audit: Arc<dyn AuditSink>,
    safety: Arc<Safety>,
    cancel: CancellationToken,
    max_error_bytes: usize,
    settled: bool,
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        if self.settled {
            return;
        }
        self.safety.unknown.store(true, Ordering::Release);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.audit.dropped(&self.id)
        }));
        let failure = match result {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error.message,
            Err(_) => "audit dropped callback panicked".into(),
        };
        *lock(&self.safety.audit_failure) = Some(fit_message(failure, self.max_error_bytes));
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
pub(crate) fn failure(message: impl Into<String>) -> CapabilityError {
    CapabilityError::new(message)
}
pub(crate) fn io_error(error: io::Error) -> CapabilityError {
    failure(error.to_string())
}
pub(crate) fn check(cancel: &CancellationToken, deadline: Instant) -> Result<(), CapabilityError> {
    if cancel.is_cancelled() {
        return Err(CapabilityError::stopped(
            CapabilityStop::Cancelled,
            "capability cancelled",
        ));
    }
    if Instant::now() >= deadline {
        return Err(CapabilityError::stopped(
            CapabilityStop::Timeout,
            "capability deadline reached",
        ));
    }
    Ok(())
}
fn json_size(value: &Value, limit: usize) -> Result<usize, CapabilityError> {
    // Bound recursion before serde or a schema validator can traverse arbitrary
    // values. This also caps traversal work before exact serialized accounting.
    fn shape(
        value: &Value,
        depth: usize,
        used: &mut usize,
        limit: usize,
    ) -> Result<(), CapabilityError> {
        if depth > 128 {
            return Err(failure("JSON nesting limit exceeded"));
        }
        *used = used.saturating_add(1);
        if *used > limit {
            return Err(CapabilityError::stopped(
                CapabilityStop::Budget,
                "JSON byte budget exceeded",
            ));
        }
        match value {
            Value::Array(values) => {
                for value in values {
                    shape(value, depth + 1, used, limit)?;
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    *used = used.saturating_add(key.len());
                    shape(value, depth + 1, used, limit)?;
                }
            }
            Value::String(text) => {
                *used = used.saturating_add(text.len());
                if *used > limit {
                    return Err(CapabilityError::stopped(
                        CapabilityStop::Budget,
                        "JSON byte budget exceeded",
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
    shape(value, 0, &mut 0, limit)?;
    struct Counter {
        used: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.used) {
                return Err(io::Error::other("JSON byte budget exceeded"));
            }
            self.used += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { used: 0, limit };
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| CapabilityError::stopped(CapabilityStop::Budget, error.to_string()))?;
    Ok(counter.used)
}
