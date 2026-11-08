//! Host-only workspace catalog and in-process Runtime routing. Every local
//! workspace shares the one Client endpoint; workspace roots and databases stay isolated.
use crate::{UnconfiguredProvider, WorkspaceHost, cli_settings};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, Model, Provider};
use cayw_bridge::{AccessRequest, Limits, Provenance, Registry, RegistryPolicy, Visibility};
use cayw_codemode::CapabilityError;
use cayw_runtime::{
    HeadlessConfig, PiCompatConfig, Runtime, RuntimeConfig,
    local_cli::{LocalPaths, local_model_setup_hint},
};
use cayw_sources::{InstalledServer, install_mcp_servers_file};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap, error::Error, os::unix::ffi::OsStrExt, path::Path, sync::Arc,
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug)]
pub(crate) struct WorkspaceOpenError {
    message: String,
    code: &'static str,
}

impl WorkspaceOpenError {
    #[cfg(feature = "plugins")]
    fn from_error(error: Box<dyn Error>) -> Self {
        let code = if let Some(error) = error.downcast_ref::<cayw_runtime::RuntimeError>() {
            error.code()
        } else if cli_settings::error_outcome_unknown(error.as_ref()) {
            "workspace_outcome_unknown"
        } else {
            "settings_error"
        };
        Self {
            message: error.to_string(),
            code,
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }
}

impl std::fmt::Display for WorkspaceOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for WorkspaceOpenError {}

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

#[derive(Default)]
pub(crate) struct OptionalServicesReport {
    pub mcp_installed: usize,
    pub mcp_skipped: usize,
    pub pi_installed: usize,
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
                if tokio::time::timeout(Duration::from_secs(10), entry.control.wait_closed())
                    .await
                    .is_err()
                {
                    failures.push(format!("MCP source {} did not confirm shutdown", entry.id));
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

#[cfg(feature = "plugins")]
async fn cleanup_failed_setup_error<T>(
    resources: WorkspaceResources,
    primary: WorkspaceOpenError,
) -> Result<T> {
    match resources.shutdown().await {
        Ok(()) => Err(Box::new(primary)),
        Err(cleanup) => Err(Box::new(WorkspaceOpenError {
            message: format!("{primary}; optional service cleanup also failed: {cleanup}"),
            code: "workspace_outcome_unknown",
        })),
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
    let setup: Result<()> = async {
        if let Some(path) = mcp_servers_file {
            let installed =
                install_mcp_servers_file(path, workspace, &registry, CancellationToken::new())
                    .await
                    .map_err(|error| format!("MCP servers install failed: {error}"))?;
            report.mcp_installed = installed.installed.len();
            report.mcp_skipped = installed.skipped.len();
            resources.mcp = Some((registry.clone(), installed.installed));
        }

        if pi_compat_enabled {
            #[cfg(feature = "pi-compat")]
            {
                let plugin_registry = Arc::new(cayw_plugins::PluginRegistry::new());
                let pi_host = Arc::new(cayw_pi_compat::PiCompatHost::with_registry(
                    Arc::clone(&plugin_registry),
                    true,
                ));
                resources.pi = Some((Arc::clone(&pi_host), Vec::new()));
                let pi = pi_compat.unwrap_or_default();
                if pi.fixture
                    || (pi_fixture_fallback && !pi.npm_hello && pi.sample_extension.is_none())
                {
                    pi_host
                        .install_fixture()
                        .map_err(|error| format!("install pi fixture: {error}"))?;
                    let package = cayw_pi_compat::PiFixturePlugin::package_id();
                    resources
                        .pi
                        .as_mut()
                        .expect("Pi resources were initialized")
                        .1
                        .push(package.clone());
                    pi_host
                        .register_into_bridge(&package, &registry)
                        .map_err(|error| format!("bridge pi fixture: {error}"))?;
                    report.pi_installed += 1;
                }
                if let Some(sample_path) = pi.sample_extension {
                    pi_host
                        .install_sample_extension(&sample_path, CancellationToken::new())
                        .await
                        .map_err(|error| format!("install pi sample extension: {error}"))?;
                    let package = cayw_pi_compat::PiSampleExtension::package_id();
                    resources
                        .pi
                        .as_mut()
                        .expect("Pi resources were initialized")
                        .1
                        .push(package.clone());
                    pi_host
                        .register_into_bridge(&package, &registry)
                        .map_err(|error| format!("bridge pi sample extension: {error}"))?;
                    report.pi_installed += 1;
                }
                if pi.npm_hello {
                    pi_host
                        .install_npm_hello_extension(CancellationToken::new())
                        .await
                        .map_err(|error| format!("install pi npm hello extension: {error}"))?;
                    let package = cayw_pi_compat::PiNpmHelloExtension::package_id();
                    resources
                        .pi
                        .as_mut()
                        .expect("Pi resources were initialized")
                        .1
                        .push(package.clone());
                    pi_host
                        .register_into_bridge(&package, &registry)
                        .map_err(|error| format!("bridge pi npm hello extension: {error}"))?;
                    report.pi_installed += 1;
                }
                config.plugin_registry = Some(plugin_registry);
            }
            #[cfg(not(feature = "pi-compat"))]
            {
                let _ = (pi_compat, pi_fixture_fallback);
                return Err(
                    "pi-compat requires the `pi-compat` feature to be enabled in this binary"
                        .into(),
                );
            }
        }
        Ok(())
    }
    .await;
    if let Err(message) = setup.map_err(|error| error.to_string()) {
        return cleanup_failed_setup(resources, message).await;
    }
    config.bridge = Some(registry.consumer());
    Ok((resources, report))
}

#[derive(Clone)]
pub struct Workspaces {
    inner: Arc<Inner>,
}

struct Inner {
    paths: LocalPaths,
    offline: bool,
    discoverable: bool,
    hosts: tokio::sync::RwLock<BTreeMap<String, Arc<WorkspaceHost>>>,
    opening: tokio::sync::Mutex<()>,
    #[cfg(feature = "plugins")]
    plugins: crate::cli_plugins::Plugins,
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
                offline,
                discoverable,
                hosts: tokio::sync::RwLock::new(hosts),
                opening: tokio::sync::Mutex::new(()),
                #[cfg(feature = "plugins")]
                plugins,
            }),
        }
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
                    rows.push(json!({"id":id,"path":path,"state":state}));
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
                        self.inner.offline,
                        #[cfg(feature = "plugins")]
                        self.inner.plugins.clone(),
                    )
                    .await?,
                );
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
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!("workspace cleanup failed: {}", failures.join("; ")).into())
        }
    }
}

async fn open_workspace(
    paths: LocalPaths,
    offline: bool,
    #[cfg(feature = "plugins")] plugins: crate::cli_plugins::Plugins,
) -> Result<WorkspaceHost> {
    paths.validate_existing()?;
    cli_settings::migrate_legacy_model(&paths, &paths.config_file)?;
    let headless = HeadlessConfig::load(&paths.config_file)?;
    let selected = cli_settings::selected_model(&paths)?;
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
    let mut config = headless.into_runtime_config()?;
    let request_timeout = config
        .max_run_time
        .checked_add(Duration::from_secs(60))
        .ok_or("workspace request timeout calculation overflowed")?;
    let request_timeout_ms = u64::try_from(request_timeout.as_millis())
        .map_err(|_| "workspace request timeout exceeds u64")?;
    if request_timeout.is_zero() || request_timeout_ms == 0 {
        return Err("workspace request timeout must be positive".into());
    }
    let (provider, model): (Arc<dyn Provider>, Model) = if let Some(selected) = selected {
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
    let (resources, _) = prepare_optional_services(
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
    let settings = cli_settings::Settings {
        #[cfg(feature = "plugins")]
        plugins,
        pi_enabled,
        offline,
        available: model_available,
        paths: paths.clone(),
    };
    #[cfg(feature = "plugins")]
    let restore_error = match settings.plugins.restore(&paths, &runtime).await {
        Ok(()) => None,
        Err(error) => Some(WorkspaceOpenError::from_error(error)),
    };
    #[cfg(feature = "plugins")]
    if let Some(error) = restore_error {
        drop(settings);
        drop(runtime);
        return cleanup_failed_setup_error(resources, error).await;
    }
    Ok(WorkspaceHost::new(
        runtime,
        request_timeout_ms,
        Some(settings),
        resources,
        model_available,
        local_model_setup_hint(&paths),
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
