//! Host-only workspace catalog and in-process Runtime routing. Every local
//! workspace shares the one Client endpoint; workspace roots and databases stay isolated.
use crate::{UnconfiguredProvider, WorkspaceHost, cli_settings};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, Model, Provider};
use cayw_bridge::{AccessRequest, Limits, Provenance, Registry, RegistryPolicy, Visibility};
use cayw_codemode::CapabilityError;
use cayw_runtime::{
    HeadlessConfig, PiCompatConfig, Runtime, RuntimeConfig, TaskDispatch, TaskInput, TaskIntent,
    TaskResources, WorkControl, WorkDispatchReply, WorkDispatchRequest, WorkOwner, WorkRouter,
    WorkRouterError, WorkSessionNotice, WorkSessionNoticeReply, WorkStatusReport,
    WorkTargetOutcome, WorkTaskQuery,
    local_cli::{LocalPaths, local_model_setup_hint},
};
use cayw_sources::{InstalledServer, install_mcp_servers_file};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    error::Error,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct ClientBridgePolicy;
#[async_trait]
impl RegistryPolicy for ClientBridgePolicy {
    fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
        true
    }

    async fn authorize(
        &self,
        _: &AccessRequest,
        _: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OptionalServicesReport {
    pub mcp_installed: usize,
    pub mcp_skipped: usize,
    pub pi_installed: usize,
    pub mcp_error: Option<String>,
    pub pi_error: Option<String>,
    pub cleanup_error: Option<String>,
}

impl OptionalServicesReport {
    /// Disabled optional sources with user-facing reasons. Sources that were
    /// not configured never appear here.
    pub fn disabled_services(&self) -> Vec<cayw_runtime::DisabledService> {
        let mut disabled = Vec::new();
        for (name, reason) in [
            ("mcp", &self.mcp_error),
            ("pi_compat", &self.pi_error),
            ("optional_service_cleanup", &self.cleanup_error),
        ] {
            if let Some(reason) = reason {
                disabled.push(cayw_runtime::DisabledService {
                    name: name.into(),
                    reason: reason.clone(),
                });
            }
        }
        disabled
    }
}

#[derive(Default)]
pub(crate) struct WorkspaceResources {
    mcp: Option<(Registry, Vec<InstalledServer>)>,
    #[cfg(feature = "pi-compat")]
    pi: Option<(
        Arc<cayw_pi_compat::PiCompatHost>,
        Vec<cayw_pi_compat::PiPackageId>,
    )>,
}

impl WorkspaceResources {
    pub async fn shutdown(mut self) -> Result<()> {
        let mut failures = Vec::new();
        #[cfg(feature = "pi-compat")]
        if let Some((pi_host, loaded)) = self.pi.take() {
            for package in loaded {
                if let Err(error) = pi_host.unload(&package).await {
                    failures.push(format!("Pi plugin {package:?}: {error}"));
                }
            }
        }
        if let Some((registry, installed)) = self.mcp.take() {
            for entry in installed {
                if let Err(error) = registry.disconnect(&entry.id) {
                    failures.push(format!("MCP source {}: {error}", entry.id));
                }
                entry.control.close();
                match tokio::time::timeout(Duration::from_secs(10), entry.control.wait_closed())
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        failures.push(format!("MCP source {} cleanup: {error}", entry.id))
                    }
                    Err(_) => {
                        failures.push(format!("MCP source {} did not confirm shutdown", entry.id))
                    }
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; ").into())
        }
    }
}

pub(crate) async fn cleanup_failed_setup<T>(
    resources: WorkspaceResources,
    primary: String,
) -> Result<T> {
    match resources.shutdown().await {
        Ok(()) => Err(primary.into()),
        Err(cleanup) => {
            Err(format!("{primary}; optional service cleanup also failed: {cleanup}").into())
        }
    }
}

pub(crate) async fn prepare_optional_services(
    workspace: &Path,
    mcp_servers_file: Option<&Path>,
    pi_compat_enabled: bool,
    pi_compat: Option<PiCompatConfig>,
    pi_fixture_fallback: bool,
    config: &mut RuntimeConfig,
) -> Result<(WorkspaceResources, OptionalServicesReport)> {
    let mut resources = WorkspaceResources::default();
    let mut report = OptionalServicesReport::default();
    if mcp_servers_file.is_none() && !pi_compat_enabled {
        return Ok((resources, report));
    }

    let registry = Registry::new(Limits::default(), Arc::new(ClientBridgePolicy))?;
    // Each source installs independently. One failing source keeps the other
    // usable and never stops the workspace from opening.
    if let Some(path) = mcp_servers_file {
        match install_mcp_servers_file(path, workspace, &registry, CancellationToken::new()).await {
            Ok(installed) => {
                report.mcp_installed = installed.installed.len();
                report.mcp_skipped = installed.skipped.len();
                resources.mcp = Some((registry.clone(), installed.installed));
            }
            Err(error) => report.mcp_error = Some(format!("MCP servers install failed: {error}")),
        }
    }
    if pi_compat_enabled {
        #[cfg(feature = "pi-compat")]
        install_pi(
            &registry,
            &mut resources,
            &mut report,
            pi_compat,
            pi_fixture_fallback,
            config,
        )
        .await;
        #[cfg(not(feature = "pi-compat"))]
        {
            let _ = (pi_compat, pi_fixture_fallback);
            report.pi_error = Some(
                "pi-compat requires the `pi-compat` feature to be enabled in this binary".into(),
            );
        }
    }
    if report.mcp_installed + report.pi_installed > 0 {
        config.bridge = Some(registry.consumer());
    } else if report.mcp_error.is_some() || report.pi_error.is_some() {
        // Nothing usable was installed. Release partially prepared sources once
        // and report any failure; cleanup is never retried or replayed.
        match std::mem::take(&mut resources).shutdown().await {
            Ok(()) => {}
            Err(error) => report.cleanup_error = Some(error.to_string()),
        }
    }
    Ok((resources, report))
}

#[cfg(feature = "pi-compat")]
async fn install_pi(
    registry: &Registry,
    resources: &mut WorkspaceResources,
    report: &mut OptionalServicesReport,
    pi_compat: Option<PiCompatConfig>,
    pi_fixture_fallback: bool,
    config: &mut RuntimeConfig,
) {
    let plugin_registry = Arc::new(cayw_plugins::PluginRegistry::new());
    let pi_host = Arc::new(cayw_pi_compat::PiCompatHost::with_registry(
        Arc::clone(&plugin_registry),
        true,
    ));
    resources.pi = Some((Arc::clone(&pi_host), Vec::new()));
    let pi = pi_compat.unwrap_or_default();
    let mut failures = Vec::new();
    if pi.fixture || (pi_fixture_fallback && !pi.npm_hello && pi.sample_extension.is_none()) {
        match pi_host.install_fixture() {
            Ok(_) => {
                let package = cayw_pi_compat::PiFixturePlugin::package_id();
                resources
                    .pi
                    .as_mut()
                    .expect("Pi resources were initialized")
                    .1
                    .push(package.clone());
                match pi_host.register_into_bridge(&package, registry) {
                    Ok(_) => report.pi_installed += 1,
                    Err(error) => failures.push(format!("bridge pi fixture: {error}")),
                }
            }
            Err(error) => failures.push(format!("install pi fixture: {error}")),
        }
    }
    if let Some(sample_path) = pi.sample_extension {
        match pi_host
            .install_sample_extension(&sample_path, CancellationToken::new())
            .await
        {
            Ok(_) => {
                let package = cayw_pi_compat::PiSampleExtension::package_id();
                resources
                    .pi
                    .as_mut()
                    .expect("Pi resources were initialized")
                    .1
                    .push(package.clone());
                match pi_host.register_into_bridge(&package, registry) {
                    Ok(_) => report.pi_installed += 1,
                    Err(error) => failures.push(format!("bridge pi sample extension: {error}")),
                }
            }
            Err(error) => failures.push(format!("install pi sample extension: {error}")),
        }
    }
    if pi.npm_hello {
        match pi_host
            .install_npm_hello_extension(CancellationToken::new())
            .await
        {
            Ok(_) => {
                let package = cayw_pi_compat::PiNpmHelloExtension::package_id();
                resources
                    .pi
                    .as_mut()
                    .expect("Pi resources were initialized")
                    .1
                    .push(package.clone());
                match pi_host.register_into_bridge(&package, registry) {
                    Ok(_) => report.pi_installed += 1,
                    Err(error) => failures.push(format!("bridge pi npm hello extension: {error}")),
                }
            }
            Err(error) => failures.push(format!("install pi npm hello extension: {error}")),
        }
    }
    config.plugin_registry = Some(plugin_registry);
    if !failures.is_empty() {
        report.pi_error = Some(failures.join("; "));
    }
}

#[derive(Clone)]
pub struct Workspaces {
    inner: Arc<Inner>,
}

struct Inner {
    paths: LocalPaths,
    explicit_config: Option<PathBuf>,
    offline: bool,
    discoverable: bool,
    hosts: tokio::sync::RwLock<BTreeMap<String, Arc<WorkspaceHost>>>,
    opening: tokio::sync::Mutex<()>,
    main: OnceLock<Weak<WorkspaceHost>>,
    #[cfg(feature = "plugins")]
    plugins: crate::cli_plugins::Plugins,
    #[cfg(feature = "plugins")]
    monitor: std::sync::OnceLock<SessionMonitor>,
}

#[cfg(feature = "plugins")]
struct SessionMonitor {
    observer: cayw_plugins::observer::Observer,
    worker: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Empty {}

impl Workspaces {
    pub fn new(
        paths: LocalPaths,
        explicit_config: Option<PathBuf>,
        offline: bool,
        discoverable: bool,
        primary: Arc<WorkspaceHost>,
        #[cfg(feature = "plugins")] plugins: crate::cli_plugins::Plugins,
    ) -> Self {
        let mut hosts = BTreeMap::new();
        hosts.insert("local".into(), primary);
        Self {
            inner: Arc::new(Inner {
                paths,
                explicit_config,
                offline,
                discoverable,
                hosts: tokio::sync::RwLock::new(hosts),
                opening: tokio::sync::Mutex::new(()),
                main: OnceLock::new(),
                #[cfg(feature = "plugins")]
                plugins,
                #[cfg(feature = "plugins")]
                monitor: std::sync::OnceLock::new(),
            }),
        }
    }

    #[cfg(feature = "plugins")]
    pub fn start_observer(&self, runtime: Arc<Runtime>) -> Result<()> {
        use cayw_plugins::observer::{Observer, ObserverConfig};
        let session_id = Runtime::OBSERVER_SESSION_ID;
        runtime
            .store()
            .ensure_host_session(session_id, "Observer")?;
        let (observer, mut reader) = Observer::channel(ObserverConfig::default())?;
        let worker = tokio::spawn(async move {
            while let Some(event) = reader.recv().await {
                let stored = (|| -> Result<()> {
                    let snapshot = runtime.store().snapshot(session_id)?;
                    runtime.store().append_control_event(
                        session_id,
                        snapshot.cursor,
                        None,
                        "observer",
                        &event.kind,
                        serde_json::to_value(event.fields)?,
                    )?;
                    Ok(())
                })();
                if let Err(error) = stored {
                    eprintln!("Observer could not record a background task status: {error}");
                }
            }
        });
        self.inner
            .monitor
            .set(SessionMonitor {
                observer,
                worker: tokio::sync::Mutex::new(Some(worker)),
            })
            .map_err(|_| "Observer is already running")?;
        Ok(())
    }

    // Called once during Client startup, before IPC begins accepting requests.
    pub async fn register_main(&self, host: &Arc<WorkspaceHost>) -> Result<()> {
        if !host.runtime.is_main_session() {
            return Err("work owner must be the main session".into());
        }
        self.inner
            .main
            .set(Arc::downgrade(host))
            .map_err(|_| "main session is already registered")?;
        let primary = self
            .inner
            .hosts
            .read()
            .await
            .get("local")
            .cloned()
            .ok_or("primary workspace is unavailable")?;
        if !primary.runtime.is_main_session() {
            primary.runtime.set_work_owner(Arc::new(ClientWorkOwner {
                inner: Arc::downgrade(&self.inner),
                #[cfg(feature = "plugins")]
                workspace: "local".into(),
            }))?;
        }
        Ok(())
    }

    pub fn id_for(&self, path: &Path) -> String {
        self.id(path)
    }

    fn id(&self, path: &Path) -> String {
        if path == self.inner.paths.workspace {
            "local".into()
        } else {
            format!(
                "workspace-{:x}",
                Sha256::digest(path.as_os_str().as_bytes())
            )
        }
    }

    pub async fn open_count(&self) -> usize {
        self.inner.hosts.read().await.len()
    }

    /// The router installed into the main-session Runtime. It holds only a weak
    /// reference to the Client catalog, so the Runtime cannot keep the Client
    /// and its workspace processes alive after shutdown.
    pub(crate) fn work_router(&self) -> Arc<dyn WorkRouter> {
        Arc::new(ClientWorkRouter {
            inner: Arc::downgrade(&self.inner),
        })
    }

    async fn router_resolve(
        &self,
        workspace: &str,
    ) -> std::result::Result<String, WorkRouterError> {
        if workspace.trim().is_empty() {
            return Err(WorkRouterError::known("workspace must not be empty"));
        }
        if self.inner.hosts.read().await.contains_key(workspace) {
            return Ok(workspace.to_string());
        }
        let known = self
            .inner
            .paths
            .known_workspaces()
            .map_err(|error| WorkRouterError::known(error.to_string()))?;
        if let Some(path) = known
            .iter()
            .find(|path| self.id(path) == workspace || path.to_string_lossy() == workspace)
        {
            return Ok(self.id(path));
        }
        if let Ok(candidate) = Path::new(workspace).canonicalize()
            && let Some(path) = known.iter().find(|path| **path == candidate)
        {
            return Ok(self.id(path));
        }
        Err(WorkRouterError::known(
            "workspace is not registered; register it locally with cayw init --workspace DIR",
        ))
    }

    async fn router_open(
        &self,
        id: &str,
    ) -> std::result::Result<Arc<WorkspaceHost>, WorkRouterError> {
        if let Some(host) = self.inner.hosts.read().await.get(id).cloned() {
            return Ok(host);
        }
        if !self.inner.discoverable {
            return Err(WorkRouterError::known(
                "workspace routing requires the default local Client",
            ));
        }
        self.call(
            "settings.workspaces.open",
            json!({"id":id}),
            CancellationToken::new(),
        )
        .await
        .map_err(|error| WorkRouterError::known(error.to_string()))?;
        self.host(id)
            .await
            .map_err(|error| WorkRouterError::known(error.to_string()))
    }

    async fn router_workspaces(&self) -> std::result::Result<Vec<Value>, WorkRouterError> {
        let value = self
            .call(
                "settings.workspaces.list",
                json!({}),
                CancellationToken::new(),
            )
            .await
            .map_err(|error| WorkRouterError::known(error.to_string()))?;
        Ok(value
            .get("workspaces")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    async fn router_sessions(
        &self,
        workspace: &str,
    ) -> std::result::Result<Vec<Value>, WorkRouterError> {
        let workspace = self.router_resolve(workspace).await?;
        let host = self.router_open(&workspace).await?;
        host.runtime
            .list()
            .map_err(router_runtime_error)?
            .into_iter()
            .map(|session| {
                serde_json::to_value(session)
                    .map_err(|error| WorkRouterError::known(error.to_string()))
            })
            .collect()
    }

    async fn router_create_session(
        &self,
        workspace: &str,
        label: &str,
    ) -> std::result::Result<Value, WorkRouterError> {
        if label.trim().is_empty() || label.len() > 1024 {
            return Err(WorkRouterError::known(
                "session label must contain 1..1024 bytes",
            ));
        }
        let workspace = self.router_resolve(workspace).await?;
        let host = self.router_open(&workspace).await?;
        let session = host
            .runtime
            .create(Some(label.to_string()))
            .map_err(router_runtime_error)?;
        serde_json::to_value(session).map_err(|error| WorkRouterError::known(error.to_string()))
    }

    async fn router_dispatch(
        &self,
        request: WorkDispatchRequest,
    ) -> std::result::Result<WorkDispatchReply, WorkRouterError> {
        let workspace = self.router_resolve(&request.workspace).await?;
        let host = self.router_open(&workspace).await?;
        let session_id = request.session_id.clone().ok_or_else(|| {
            WorkRouterError::known("dispatch requires an existing target session_id")
        })?;
        host.runtime
            .get(&session_id)
            .map_err(|error| WorkRouterError::known(error.to_string()))?;
        let existing = host
            .runtime
            .task_by_dispatch_key(&request.dispatch_key)
            .map_err(|error| WorkRouterError::known(error.to_string()))?;
        let reused = existing.is_some();
        if existing
            .as_ref()
            .is_none_or(|record| record.state == cayw_runtime::TaskState::Queued)
        {
            host.ensure_model_available()
                .await
                .map_err(|error| WorkRouterError::known(error.message))?;
        }
        let task = host
            .runtime
            .enqueue_dispatched(
                &session_id,
                TaskInput {
                    text: request.text,
                    images: Vec::new(),
                },
                TaskIntent::General,
                request.priority,
                TaskResources {
                    declared_memory_bytes: request.declared_memory_bytes,
                    ..Default::default()
                },
                TaskDispatch {
                    work_id: request.work_id.clone(),
                    dispatch_key: request.dispatch_key.clone(),
                    pause_version: request.pause_version,
                    stop_version: request.stop_version,
                },
            )
            .map_err(router_runtime_error)?;
        let task = if task.state == cayw_runtime::TaskState::Queued {
            host.runtime
                .start_queued(&task.id)
                .map_err(router_runtime_error)?
        } else {
            task
        };
        Ok(WorkDispatchReply {
            workspace,
            session_id: task.session_id,
            task_id: task.id,
            state: task.state,
            work_id: task
                .work_id
                .ok_or_else(|| WorkRouterError::known("dispatched task has no work_id"))?,
            dispatch_key: task
                .dispatch_key
                .ok_or_else(|| WorkRouterError::known("dispatched task has no dispatch_key"))?,
            reused,
        })
    }

    async fn router_task(
        &self,
        query: WorkTaskQuery,
    ) -> std::result::Result<Option<cayw_runtime::TaskRecord>, WorkRouterError> {
        let workspace = self.router_resolve(&query.workspace).await?;
        let host = self.router_open(&workspace).await?;
        if let Some(task_id) = query.task_id {
            return match host.runtime.task(&task_id) {
                Ok(task) => Ok(Some(task)),
                Err(cayw_runtime::RuntimeError::InvalidInput(message))
                    if message == "task not found" =>
                {
                    Ok(None)
                }
                Err(error) => Err(router_runtime_error(error)),
            };
        }
        let key = query
            .dispatch_key
            .ok_or_else(|| WorkRouterError::known("task query needs task_id or dispatch_key"))?;
        host.runtime
            .task_by_dispatch_key(&key)
            .map_err(router_runtime_error)
    }

    async fn router_control(
        &self,
        action: WorkControl,
        work_id: &str,
        version: u64,
        workspaces: &[String],
    ) -> std::result::Result<Vec<WorkTargetOutcome>, WorkRouterError> {
        let mut outcomes = Vec::new();
        for workspace in workspaces {
            let result = match self.router_resolve(workspace).await {
                Ok(id) => match self.router_open(&id).await {
                    Ok(host) => match action {
                        WorkControl::Pause => host.runtime.apply_work_pause(work_id, version),
                        WorkControl::Stop => host.runtime.apply_work_stop(work_id, version),
                    }
                    .map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(error.to_string()),
            };
            outcomes.push(match result {
                Ok(updated) => WorkTargetOutcome {
                    workspace: workspace.clone(),
                    updated: Some(updated),
                    error: None,
                },
                Err(error) => WorkTargetOutcome {
                    workspace: workspace.clone(),
                    updated: None,
                    error: Some(error),
                },
            });
        }
        Ok(outcomes)
    }

    async fn router_evaluate_check(
        &self,
        workspace: &str,
        check: &cayw_runtime::AcceptanceCheck,
    ) -> std::result::Result<bool, WorkRouterError> {
        let workspace = self.router_resolve(workspace).await?;
        let host = self.router_open(&workspace).await?;
        host.runtime
            .evaluate_acceptance(check)
            .map_err(router_runtime_error)
    }

    async fn router_notify_session(
        &self,
        notice: WorkSessionNotice,
    ) -> std::result::Result<WorkSessionNoticeReply, WorkRouterError> {
        let workspace = self.router_resolve(&notice.workspace).await?;
        let host = self.router_open(&workspace).await?;
        host.runtime
            .deliver_session_notice(&notice)
            .map_err(router_runtime_error)
    }

    pub async fn host(&self, id: &str) -> Result<Arc<WorkspaceHost>> {
        self.inner
            .hosts
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| {
                "workspace is not open in this Client; call client.workspaces.open first".into()
            })
    }

    pub async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        if !self.inner.discoverable {
            return Err("workspace routing requires the default local Client".into());
        }
        match method {
            "settings.workspaces.list" => {
                let _: Empty = serde_json::from_value(params)?;
                let config_path = self
                    .inner
                    .explicit_config
                    .as_deref()
                    .unwrap_or(&self.inner.paths.config_file);
                let configured_main = if config_path.try_exists()? {
                    HeadlessConfig::load(config_path)?
                        .main_session_dir()
                        .map(Path::to_path_buf)
                } else {
                    None
                };
                let main_dir =
                    cayw_runtime::local_cli::main_session_directory(configured_main.as_deref())
                        .ok()
                        .and_then(|directory| {
                            std::fs::canonicalize(&directory).ok().or(Some(directory))
                        });
                let hosts = self.inner.hosts.read().await;
                let mut rows = Vec::new();
                for path in self.inner.paths.known_workspaces()? {
                    let id = self.id(&path);
                    let state = if hosts.contains_key(&id) {
                        "open"
                    } else if path.is_dir() {
                        "registered"
                    } else {
                        "unavailable"
                    };
                    let is_main = hosts
                        .get(&id)
                        .map(|host| host.runtime.is_main_session())
                        .unwrap_or_else(|| main_dir.as_ref().is_some_and(|dir| dir == &path));
                    rows.push(json!({"id":id,"path":path,"state":state,"main_session":is_main}));
                }
                Ok(json!({
                    "current":"local",
                    "workspaces":rows,
                    "registration":"cayw init --workspace DIR"
                }))
            }
            "settings.workspaces.open" => {
                let open: Open = serde_json::from_value(params)?;
                if cancel.is_cancelled() {
                    return Err("workspace open was cancelled before dispatch".into());
                }
                let path = self
                    .inner
                    .paths
                    .known_workspaces()?
                    .into_iter()
                    .find(|path| self.id(path) == open.id)
                    .ok_or(
                        "workspace is not registered; register it locally with cayw init --workspace DIR",
                    )?;
                let paths = LocalPaths::from_environment(&path)?;
                if paths.workspace != path {
                    return Err("registered workspace path changed; explicitly register its new canonical directory".into());
                }
                if self.inner.hosts.read().await.contains_key(&open.id) {
                    return Ok(json!({
                        "id":open.id,"path":path,"state":"open","reused":true
                    }));
                }

                let _opening = self.inner.opening.lock().await;
                if self.inner.hosts.read().await.contains_key(&open.id) {
                    return Ok(json!({
                        "id":open.id,"path":path,"state":"open","reused":true
                    }));
                }
                if cancel.is_cancelled() {
                    return Err("workspace open was cancelled before Runtime creation".into());
                }
                let host = Arc::new(
                    open_workspace(
                        paths,
                        self.inner.explicit_config.as_deref(),
                        self.inner.offline,
                        #[cfg(feature = "plugins")]
                        self.inner.plugins.clone(),
                    )
                    .await?,
                );
                if !host.runtime.is_main_session() {
                    host.runtime
                        .set_work_owner(Arc::new(ClientWorkOwner {
                            inner: Arc::downgrade(&self.inner),
                            #[cfg(feature = "plugins")]
                            workspace: open.id.clone(),
                        }))
                        .map_err(|error| error.to_string())?;
                }
                self.inner.hosts.write().await.insert(open.id.clone(), host);
                Ok(json!({
                    "id":open.id,"path":path,"state":"open","reused":false
                }))
            }
            _ => Err("unsupported workspace method".into()),
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        let hosts = self
            .inner
            .hosts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        for host in hosts {
            if let Err(error) = host.shutdown().await {
                failures.push(error.to_string());
            }
        }
        #[cfg(feature = "plugins")]
        if let Some(monitor) = self.inner.monitor.get() {
            use cayw_plugins::Plugin;
            monitor.observer.request_shutdown();
            let mut worker = monitor.worker.lock().await;
            if let Some(task) = worker.as_mut() {
                if let Err(error) = task.await {
                    failures.push(format!("Observer stopped unexpectedly: {error}"));
                }
                *worker = None;
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!("workspace cleanup failed: {}", failures.join("; ")).into())
        }
    }
}

/// Runtime-facing router. It upgrades the Client catalog only for the duration
/// of one call and never owns it.
struct ClientWorkRouter {
    inner: Weak<Inner>,
}

impl ClientWorkRouter {
    fn client(&self) -> std::result::Result<Workspaces, WorkRouterError> {
        self.inner
            .upgrade()
            .map(|inner| Workspaces { inner })
            .ok_or_else(|| WorkRouterError::known("the local Client is shut down"))
    }
}

#[async_trait]
impl WorkRouter for ClientWorkRouter {
    async fn workspaces(&self) -> std::result::Result<Vec<Value>, WorkRouterError> {
        self.client()?.router_workspaces().await
    }

    async fn sessions(&self, workspace: &str) -> std::result::Result<Vec<Value>, WorkRouterError> {
        self.client()?.router_sessions(workspace).await
    }

    async fn create_session(
        &self,
        workspace: &str,
        label: &str,
    ) -> std::result::Result<Value, WorkRouterError> {
        self.client()?.router_create_session(workspace, label).await
    }

    async fn dispatch(
        &self,
        request: WorkDispatchRequest,
    ) -> std::result::Result<WorkDispatchReply, WorkRouterError> {
        self.client()?.router_dispatch(request).await
    }

    async fn task(
        &self,
        query: WorkTaskQuery,
    ) -> std::result::Result<Option<cayw_runtime::TaskRecord>, WorkRouterError> {
        self.client()?.router_task(query).await
    }

    async fn control(
        &self,
        action: WorkControl,
        work_id: &str,
        version: u64,
        workspaces: &[String],
    ) -> std::result::Result<Vec<WorkTargetOutcome>, WorkRouterError> {
        self.client()?
            .router_control(action, work_id, version, workspaces)
            .await
    }

    async fn evaluate_check(
        &self,
        workspace: &str,
        check: &cayw_runtime::AcceptanceCheck,
    ) -> std::result::Result<bool, WorkRouterError> {
        self.client()?.router_evaluate_check(workspace, check).await
    }

    async fn notify_session(
        &self,
        notice: WorkSessionNotice,
    ) -> std::result::Result<WorkSessionNoticeReply, WorkRouterError> {
        self.client()?.router_notify_session(notice).await
    }
}

/// The child workspace's link back to the main session. It holds the same weak
/// Client catalog as the router, so a shut-down Client cannot be kept alive.
struct ClientWorkOwner {
    inner: Weak<Inner>,
    #[cfg(feature = "plugins")]
    workspace: String,
}

impl ClientWorkOwner {
    fn client(&self) -> std::result::Result<Workspaces, WorkRouterError> {
        self.inner
            .upgrade()
            .map(|inner| Workspaces { inner })
            .ok_or_else(|| WorkRouterError::known("the local Client is shut down"))
    }

    fn main(&self) -> std::result::Result<Arc<WorkspaceHost>, WorkRouterError> {
        self.client()?
            .inner
            .main
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| WorkRouterError::known("the main session is unavailable"))
    }
}

#[async_trait]
impl WorkOwner for ClientWorkOwner {
    async fn work_item(
        &self,
        work_id: &str,
    ) -> std::result::Result<cayw_runtime::WorkItem, WorkRouterError> {
        let host = self.main()?;
        host.runtime
            .work_item(work_id)
            .map_err(router_runtime_error)?
            .ok_or_else(|| WorkRouterError::known("WorkItem not found"))
    }

    async fn record_program_acceptance(
        &self,
        work_id: &str,
        criterion_id: &str,
        passed: bool,
        evidence: String,
    ) -> std::result::Result<cayw_runtime::WorkItem, WorkRouterError> {
        let host = self.main()?;
        host.runtime
            .record_child_program_acceptance(work_id, criterion_id, passed, evidence)
            .map_err(router_runtime_error)
    }

    async fn report_status(
        &self,
        report: WorkStatusReport,
    ) -> std::result::Result<WorkSessionNoticeReply, WorkRouterError> {
        #[cfg(feature = "plugins")]
        if let Some(monitor) = self.client()?.inner.monitor.get() {
            use cayw_plugins::observer::{Observation, ObservedField, PublishOutcome, Sensitivity};
            let fields = [
                ("workspace", json!(self.workspace)),
                ("work_id", json!(report.work_id)),
                ("task_id", json!(report.task_id)),
                ("session_id", json!(report.session_id)),
                ("state", json!(report.state)),
                (
                    "needs_attention",
                    json!(matches!(
                        report.state.as_str(),
                        "failed" | "outcome_unknown"
                    )),
                ),
            ]
            .into_iter()
            .map(|(name, value)| {
                (
                    name.into(),
                    ObservedField {
                        value,
                        sensitivity: Sensitivity::Public,
                    },
                )
            })
            .collect();
            let delivery = monitor.observer.try_publish(Observation {
                kind: "observer.task_status".into(),
                fields,
            });
            if delivery != Some(PublishOutcome::Queued) {
                eprintln!("Observer missed task {}: {delivery:?}", report.task_id);
            }
        }
        let host = self.main()?;
        host.runtime
            .relay_task_status(report)
            .map_err(router_runtime_error)
    }
}

fn router_runtime_error(error: cayw_runtime::RuntimeError) -> WorkRouterError {
    if error.outcome_unknown() {
        WorkRouterError::unknown(error.to_string())
    } else {
        WorkRouterError::known(error.to_string())
    }
}

async fn open_workspace(
    paths: LocalPaths,
    explicit_config: Option<&Path>,
    offline: bool,
    #[cfg(feature = "plugins")] plugins: crate::cli_plugins::Plugins,
) -> Result<WorkspaceHost> {
    paths.validate_existing()?;
    let config_path = explicit_config.unwrap_or(&paths.config_file);
    let headless = HeadlessConfig::load(config_path)?;
    let choice = cli_settings::model_choice(&paths)?;
    if let Some(message) = &choice.fallback {
        eprintln!("{message}");
    }
    let selected = choice.model;
    if offline && selected.is_some() {
        return Err(
            "model configuration conflicts with the Client's offline mode for this workspace"
                .into(),
        );
    }
    let model_available = offline || selected.is_some();
    let mcp_servers_file = headless.mcp_servers_file().map(Path::to_path_buf);
    let pi_compat = headless.pi_compat().cloned();
    let pi_enabled = pi_compat.as_ref().is_some_and(|config| config.enabled);
    let configured_main_session = headless.main_session_dir().map(Path::to_path_buf);
    let mut config = headless.into_runtime_config()?;
    // Local Client tasks use model decisions through CodeMode. Fully explicit
    // Headless never opens this workspace path.
    config.approval_rules = Some(cayw_runtime::device_rules(&paths.data_dir));
    config.approval_mode = cayw_runtime::ApprovalMode::ModelAuto;
    config.reconciliation_verifier = Some(Arc::new(
        cayw_runtime::local_cli::LocalReconciliationVerifier(paths.clone()),
    ));
    config.main_session =
        cayw_runtime::local_cli::main_session_directory(configured_main_session.as_deref())
            .ok()
            .and_then(|directory| std::fs::canonicalize(directory).ok())
            .is_some_and(|directory| directory == paths.workspace);
    // Git recovery is a local-Client product default. The private repository
    // lives in this workspace's CAYW state directory, never in the project.
    config.recovery = Some(cayw_runtime::recovery_points::RecoveryConfig {
        data_dir: Some(paths.data_dir.clone()),
        ..Default::default()
    });
    if let Some(warning) =
        cayw_runtime::recovery_points::availability_warning(&paths.workspace, paths.state_dir())
    {
        eprintln!("{warning}");
    }
    if cayw_runtime::recovery_points::workspace_enabled(&paths.workspace, paths.state_dir()) {
        config.recovery_scope = Some(cayw_runtime::RecoveryScope::new([paths.workspace.clone()]));
    }
    let request_timeout = match config.max_run_time {
        Some(max_run_time) => Some(
            max_run_time
                .checked_add(Duration::from_secs(60))
                .ok_or("workspace request timeout calculation overflowed")?,
        ),
        None => None,
    };
    let request_timeout_ms = match request_timeout {
        Some(request_timeout) => {
            let timeout_ms = u64::try_from(request_timeout.as_millis())
                .map_err(|_| "workspace request timeout exceeds u64")?;
            if request_timeout.is_zero() || timeout_ms == 0 {
                return Err("workspace request timeout must be positive".into());
            }
            Some(timeout_ms)
        }
        None => None,
    };
    let (provider, model): (Arc<dyn Provider>, Model) = if let Some(selected) = selected {
        if let Some(warning) = selected.context_window_warning(config.main_session) {
            eprintln!("{warning}");
        }
        let configured = selected.build()?;
        (configured.provider, configured.model)
    } else if offline {
        (
            Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
            Model::new("fixture", "offline-runtime"),
        )
    } else {
        (
            Arc::new(UnconfiguredProvider),
            Model::new("unconfigured", "unconfigured"),
        )
    };
    let (resources, optional_services) = prepare_optional_services(
        &paths.workspace,
        mcp_servers_file.as_deref(),
        pi_enabled,
        pi_compat,
        false,
        &mut config,
    )
    .await?;
    let runtime = match Runtime::open(&paths.database, &paths.workspace, provider, model, config) {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => return cleanup_failed_setup(resources, error.to_string()).await,
    };
    let mut disabled = runtime.disabled_services().to_vec();
    disabled.extend(optional_services.disabled_services());
    crate::warn_disabled_services(&disabled);
    // First access establishes the initial point, including uncommitted files.
    // A failure is a warning; the workspace still opens.
    if let Err(error) = runtime.create_recovery_point() {
        eprintln!("首次恢复点未能建立：{error}；工作继续");
    }
    let settings = cli_settings::Settings {
        #[cfg(feature = "plugins")]
        plugins,
        pi_enabled,
        offline,
        available: model_available,
        paths: paths.clone(),
    };
    #[cfg(feature = "plugins")]
    if let Err(error) = settings.plugins.restore(&paths, &runtime).await {
        // Plugin restore only degrades the plugin subsystem. The workspace keeps
        // opening, and doctor plus the settings RPC report the same reason.
        eprintln!(
            "cayw: workspace plugin restore failed in {}; the workspace continues without plugin supervision: {error}",
            paths.workspace.display()
        );
    }
    Ok(WorkspaceHost::new(
        runtime,
        request_timeout_ms,
        Some(settings),
        resources,
        model_available,
        local_model_setup_hint(&paths),
        optional_services,
    ))
}

pub fn optional_tools(pi_enabled: bool) -> Result<Value> {
    use std::os::unix::fs::PermissionsExt;
    let executable = std::env::current_exe()?;
    let installed = |name: &str| {
        std::fs::metadata(executable.with_file_name(name))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let prefix = executable
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "bin"))
        .and_then(Path::parent);
    let install = |package: &str, program: &str, features: &str| match prefix {
        Some(prefix) => format!(
            "cargo install --path crates/{package} --bin {program} {features} --locked --root '{}'",
            prefix.to_string_lossy().replace('\'', "'\"'\"'")
        ),
        None => format!("cargo build -p {package} --bin {program} {features} --locked"),
    };
    Ok(json!({
        "note":"在 CAYW 源码根目录运行；安装构建需将可选程序放在当前 cayw 的同一目录。外部服务需单独启动并保持独立认证。",
        "editor":{"installed":installed("cayw-visual-editor"),"status":"external_service_required","install":install("cayw-visual-editor","cayw-visual-editor",""),"start":"cayw-visual-editor --help"},
        "trace":{"installed":installed("cayw-trace-debug"),"status":"external_service_required","install":install("cayw-trace-debug","cayw-trace-debug",""),"start":"cayw-trace-debug --help"},
        "pi":{"installed":cfg!(feature="pi-compat"),"enabled":pi_enabled,"install":install("cayw-runtime","cayw","--features pi-compat"),"start":"cayw serve --pi-compat"}
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routed_acceptance_keeps_the_original_workspace_after_rename() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("workspace");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("result.txt"), "original").unwrap();
        let runtime = Arc::new(
            Runtime::open(
                temp.path().join("session.sqlite"),
                &directory,
                Arc::new(UnconfiguredProvider),
                Model::new("fixture", "check"),
                RuntimeConfig::default(),
            )
            .unwrap(),
        );
        let paths = LocalPaths::from_environment(&directory).unwrap();
        let host = Arc::new(WorkspaceHost::new(
            runtime,
            None,
            None,
            WorkspaceResources::default(),
            false,
            "fixture".into(),
            OptionalServicesReport::default(),
        ));
        let client = Workspaces::new(
            paths,
            None,
            true,
            false,
            host,
            #[cfg(feature = "plugins")]
            crate::cli_plugins::Plugins::new(temp.path().join("plugins.json")),
        );
        std::fs::rename(&directory, temp.path().join("moved")).unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("result.txt"), "replacement").unwrap();
        let router = client.work_router();
        assert!(
            router
                .evaluate_check(
                    "local",
                    &cayw_runtime::AcceptanceCheck::FileEquals {
                        path: "result.txt".into(),
                        text: "original".into(),
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            !router
                .evaluate_check(
                    "local",
                    &cayw_runtime::AcceptanceCheck::FileEquals {
                        path: "result.txt".into(),
                        text: "replacement".into(),
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            !router
                .evaluate_check(
                    "local",
                    &cayw_runtime::AcceptanceCheck::PathExists {
                        path: "missing.txt".into(),
                    }
                )
                .await
                .unwrap()
        );
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn child_owner_uses_registered_main_not_primary() {
        let temp = tempfile::tempdir().unwrap();
        let mut hosts = Vec::new();
        for (name, main_session) in [("primary", false), ("main", true), ("other", false)] {
            let path = temp.path().join(name);
            std::fs::create_dir(&path).unwrap();
            let runtime = Arc::new(
                Runtime::open(
                    temp.path().join(format!("{name}.sqlite")),
                    &path,
                    Arc::new(UnconfiguredProvider),
                    Model::new("fixture", name),
                    RuntimeConfig {
                        main_session,
                        ..Default::default()
                    },
                )
                .unwrap(),
            );
            hosts.push(Arc::new(WorkspaceHost::new(
                runtime,
                None,
                None,
                WorkspaceResources::default(),
                false,
                "fixture".into(),
                OptionalServicesReport::default(),
            )));
        }
        let primary = &hosts[0];
        let main = &hosts[1];
        let paths = LocalPaths::from_environment(temp.path().join("primary")).unwrap();
        let client = Workspaces::new(
            paths,
            None,
            true,
            true,
            primary.clone(),
            #[cfg(feature = "plugins")]
            crate::cli_plugins::Plugins::new(temp.path().join("plugins.json")),
        );
        client
            .inner
            .hosts
            .write()
            .await
            .insert("main-fixture".into(), main.clone());
        client.register_main(main).await.unwrap();
        let owner = Arc::new(ClientWorkOwner {
            inner: Arc::downgrade(&client.inner),
            #[cfg(feature = "plugins")]
            workspace: "other".into(),
        });
        assert!(Arc::ptr_eq(&owner.main().unwrap(), main));
        // register_main must have installed the owner on the already-cached primary.
        assert!(
            primary
                .runtime
                .set_work_owner(owner.clone())
                .unwrap_err()
                .to_string()
                .contains("already installed")
        );
        hosts[2].runtime.set_work_owner(owner.clone()).unwrap();
        let session = main.runtime.create(None).unwrap().id;
        let spec = serde_json::from_value(json!({
            "work_id":"routed-work", "owner_main_session_id":session,
            "title":"route", "goal":"record on actual main",
            "acceptance_criteria":[{"criterion_id":"check", "description":"exists"}]
        }))
        .unwrap();
        main.runtime.create_work_item(spec).unwrap();
        assert_eq!(
            owner.work_item("routed-work").await.unwrap().work_id,
            "routed-work"
        );
        owner
            .record_program_acceptance("routed-work", "check", true, "fixture evidence".into())
            .await
            .unwrap();
        let observed = main.runtime.work_item("routed-work").unwrap().unwrap();
        assert_eq!(observed, owner.work_item("routed-work").await.unwrap());
        let reply = owner
            .report_status(WorkStatusReport {
                work_id: "routed-work".into(),
                task_id: "fixture-task".into(),
                session_id: "child-session".into(),
                state: "completed".into(),
                error: None,
            })
            .await
            .unwrap();
        assert_eq!(reply.session_id.as_deref(), Some(session.as_str()));
        assert!(client.register_main(main).await.is_err());
        client.shutdown().await.unwrap();
        drop(client);
        assert!(owner.main().is_err());
    }

    #[tokio::test]
    async fn mcp_failure_is_reported_without_stopping_setup() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let missing = temp.path().join("missing-mcp.json");
        let mut config = RuntimeConfig::default();
        let (resources, report) =
            prepare_optional_services(&workspace, Some(&missing), false, None, false, &mut config)
                .await
                .expect("an optional source failure must not fail setup");
        assert!(report.mcp_error.is_some());
        assert_eq!(report.mcp_installed, 0);
        assert!(config.bridge.is_none());
        assert!(
            report
                .disabled_services()
                .iter()
                .any(|service| service.name == "mcp" && !service.reason.is_empty())
        );
        resources
            .shutdown()
            .await
            .expect("nothing installed needs no cleanup");
    }

    #[tokio::test]
    async fn all_skipped_mcp_file_with_request_policy_still_opens() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mcp = temp.path().join("mcp-remote.json");
        std::fs::write(
            &mcp,
            serde_json::to_vec(
                &json!({"mcpServers":{"remote":{"url":"https://example.invalid/mcp"}}}),
            )
            .unwrap(),
        )
        .unwrap();
        let mut config = RuntimeConfig {
            request_policy: Some(cayw_runtime::RequestPolicy {
                permission_policy_id: "mcp-request-policy".into(),
                operations: vec![cayw_requests::OperationScope::exact(
                    "mcp-discover",
                    "mcp.discover",
                    json!({}),
                    "mcp-discover-binding",
                )],
            }),
            ..Default::default()
        };
        let (resources, report) =
            prepare_optional_services(&workspace, Some(&mcp), false, None, false, &mut config)
                .await
                .expect("a valid file with only skipped entries must not fail setup");
        assert_eq!(report.mcp_installed, 0);
        assert!(report.mcp_error.is_none());
        assert!(config.bridge.is_none());
        let runtime = Runtime::open(
            temp.path().join("sessions.db"),
            &workspace,
            Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
            Model::new("fixture", "mcp-all-skipped"),
            config,
        )
        .expect("a request policy for an uninstalled source must not fail open");
        assert!(
            runtime
                .capabilities()
                .unwrap()
                .iter()
                .any(|definition| definition.name == "capability.request")
        );
        resources
            .shutdown()
            .await
            .expect("skipped sources need no cleanup");
    }

    #[cfg(feature = "pi-compat")]
    #[tokio::test]
    async fn mcp_failure_does_not_prevent_pi_install() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let missing = temp.path().join("missing-mcp.json");
        let mut config = RuntimeConfig::default();
        let (resources, report) = prepare_optional_services(
            &workspace,
            Some(&missing),
            true,
            Some(PiCompatConfig {
                enabled: true,
                fixture: true,
                sample_extension: None,
                npm_hello: false,
            }),
            false,
            &mut config,
        )
        .await
        .expect("one source failing must not fail setup");
        assert!(report.mcp_error.is_some());
        assert!(report.pi_installed > 0, "{report:?}");
        assert!(report.pi_error.is_none(), "{report:?}");
        assert!(config.bridge.is_some());
        resources
            .shutdown()
            .await
            .expect("installed Pi source unloads");
    }

    #[cfg(not(feature = "pi-compat"))]
    #[tokio::test]
    async fn pi_without_feature_is_reported_and_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut config = RuntimeConfig::default();
        let (resources, report) =
            prepare_optional_services(&workspace, None, true, None, false, &mut config)
                .await
                .expect("a missing optional feature must not fail setup");
        assert!(report.pi_error.is_some());
        assert!(config.bridge.is_none());
        resources.shutdown().await.expect("nothing to clean up");
    }

    #[tokio::test]
    async fn client_work_router_automatically_starts_and_completes_external_workspace_dispatch() {
        use cayw_ai::{
            AssistantEvent, AssistantMessage, Content, ModelRequest, StopReason, Usage, UsageStatus,
        };

        struct PlainReply(&'static str);
        impl Provider for PlainReply {
            fn stream(
                &self,
                request: ModelRequest,
                _cancel: CancellationToken,
            ) -> cayw_ai::AssistantStream {
                let mut message = AssistantMessage::new(&request.model);
                message.content.push(Content::text(self.0));
                message.stop_reason = StopReason::Stop;
                message.usage = Usage {
                    input: 4,
                    output: 4,
                    total_tokens: 8,
                    ..Default::default()
                };
                message.usage_status = UsageStatus::Reported;
                Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let main_ws = temp.path().join("main-ws");
        let ext_ws = temp.path().join("ext-ws");
        std::fs::create_dir_all(&main_ws).unwrap();
        std::fs::create_dir_all(&ext_ws).unwrap();

        let main_paths = LocalPaths::from_environment(&main_ws).unwrap();
        let main_runtime = Arc::new(
            Runtime::open(
                temp.path().join("main.sqlite"),
                &main_ws,
                Arc::new(PlainReply("main-done")),
                Model::new("fixture", "main"),
                RuntimeConfig {
                    main_session: true,
                    approval_rules: Some(Arc::new(cayw_runtime::FileRules::new(
                        temp.path().join("rules.json"),
                    ))),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let ext_runtime = Arc::new(
            Runtime::open(
                temp.path().join("ext.sqlite"),
                &ext_ws,
                Arc::new(PlainReply("external-task-finished")),
                Model::new("fixture", "ext"),
                RuntimeConfig::default(),
            )
            .unwrap(),
        );

        let main_host = Arc::new(WorkspaceHost::new(
            main_runtime.clone(),
            None,
            None,
            WorkspaceResources::default(),
            true,
            "configured".into(),
            OptionalServicesReport::default(),
        ));
        let ext_host = Arc::new(WorkspaceHost::new(
            ext_runtime.clone(),
            None,
            None,
            WorkspaceResources::default(),
            true,
            "configured".into(),
            OptionalServicesReport::default(),
        ));

        let workspaces = Workspaces::new(
            main_paths,
            None,
            true,
            true,
            main_host.clone(),
            #[cfg(feature = "plugins")]
            crate::cli_plugins::Plugins::new(temp.path().join("plugins.json")),
        );
        let ext_id = workspaces.id_for(&ext_ws.canonicalize().unwrap());
        workspaces
            .inner
            .hosts
            .write()
            .await
            .insert(ext_id.clone(), ext_host);

        workspaces.register_main(&main_host).await.unwrap();
        let router = workspaces.work_router();
        main_runtime.set_work_router(router.clone()).unwrap();
        let owner = main_runtime.create(Some("Main".into())).unwrap();
        main_runtime
            .create_work_item(
                serde_json::from_value(json!({
                    "work_id": "work-ext-auto",
                    "owner_main_session_id": owner.id,
                    "title": "Observe dispatched task",
                    "goal": "Check the original task and its terminal notice",
                    "acceptance_criteria": []
                }))
                .unwrap(),
            )
            .unwrap();
        #[cfg(feature = "plugins")]
        {
            workspaces.start_observer(main_runtime.clone()).unwrap();
            assert!(
                main_runtime
                    .enqueue(Runtime::OBSERVER_SESSION_ID, "run a task")
                    .is_err()
            );
        }
        ext_runtime
            .set_work_owner(Arc::new(ClientWorkOwner {
                inner: Arc::downgrade(&workspaces.inner),
                #[cfg(feature = "plugins")]
                workspace: ext_id.clone(),
            }))
            .unwrap();

        let target_session = router
            .create_session(&ext_id, "External Worker")
            .await
            .unwrap();
        let target_session_id = target_session["id"].as_str().unwrap().to_owned();

        let reply = router
            .dispatch(WorkDispatchRequest {
                work_id: "work-ext-auto".into(),
                workspace: ext_id.clone(),
                session_id: Some(target_session_id.clone()),
                label: None,
                text: "execute external work automatically".into(),
                dispatch_key: "dk-ext-auto-1".into(),
                pause_version: 0,
                stop_version: 0,
                priority: cayw_runtime::TaskPriority::Normal,
                declared_memory_bytes: 0,
            })
            .await
            .unwrap();
        assert!(!reply.reused);
        let claimed = ext_runtime.task(&reply.task_id).unwrap();
        assert!(
            claimed.worker_id.is_some(),
            "external dispatch through ClientWorkRouter must claim and start a worker automatically"
        );

        ext_runtime.wait_idle(&target_session_id).await;
        let finished = ext_runtime.task(&reply.task_id).unwrap();
        assert_eq!(finished.state, cayw_runtime::TaskState::Completed);
        let view = ext_runtime.get(&target_session_id).unwrap();
        assert!(
            view.messages
                .iter()
                .any(|m| m.text().contains("external-task-finished"))
        );

        main_runtime.wait_idle(&owner.id).await;
        let notice_key = format!("status-notice:{}", reply.task_id);
        let notice = main_runtime
            .task_by_dispatch_key(&notice_key)
            .unwrap()
            .unwrap();
        assert_eq!(notice.session_id, owner.id);

        workspaces.shutdown().await.unwrap();
        #[cfg(feature = "plugins")]
        {
            let events = main_runtime
                .events(Runtime::OBSERVER_SESSION_ID, 0, 10)
                .unwrap();
            assert_eq!(events.len(), 2);
            let cayw_core::CoreEvent::Host { kind, data } = &events[1].event else {
                panic!("expected a committed Observer event");
            };
            assert_eq!(kind, "observer.task_status");
            assert_eq!(
                data["decision"],
                json!({
                    "workspace": ext_id,
                    "work_id": reply.work_id,
                    "task_id": reply.task_id,
                    "session_id": target_session_id,
                    "state": "completed",
                    "needs_attention": false
                })
            );
        }
    }
}
