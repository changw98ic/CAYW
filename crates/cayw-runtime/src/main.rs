mod cli_client_plugins;
mod cli_doctor;
#[cfg(feature = "plugins")]
mod cli_plugins;
mod cli_settings;
mod cli_setup;
mod cli_start;
mod cli_tui;
mod cli_workspaces;

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Model, ModelRequest,
    Provider, StopReason,
};
use cayw_ipc::{RpcError, RpcHandler, ServerConfig, serve};
use cayw_runtime::{
    HeadlessConfig, Runtime, ScopedPermission,
    fixture::OfflineProvider,
    local_cli::{LocalPaths, headless_model_setup_hint, local_model_setup_hint},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    error::Error,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

const USAGE: &str = "cayw [--workspace DIR] [--offline]                 # defaults to TUI\ncayw --ui web [--workspace DIR] [--no-open]       # optional local WebUI\ncayw --ui web --lan IP:PORT [--account-file FILE] # LAN access; account optional\ncayw account --username NAME [--workspace DIR]\ncayw start [--workspace DIR] [--ui web|tui|none] [--offline]\ncayw models --help\ncayw plugins --help\ncayw init [--workspace DIR]\ncayw conf [--workspace DIR] [--config PATH]\ncayw doctor [--workspace DIR] [--config PATH]\ncayw tui [--workspace DIR]\ncayw serve [--workspace DIR] [--db PATH] [--listen 127.0.0.1:PORT] [--token-env ENV] [--config PATH] [--offline] [--demo-authorized] [--pi-compat]\n\nUsing default locations automatically initializes ~/.cayw (override with an absolute CAYW_HOME).\nOne local Client owns the shared IPC endpoint, reconnect state, workspace routing and UI/plugin process supervision. Local discovery uses <CAYW_HOME>/client.json.\nThe workspace defaults to the current directory. Each workspace keeps its own SQLite database, model selection, Runtime, Sessions, MCP/Pi instances and architecture plugin activation state. Model profiles are one device list at <CAYW_HOME>/models.json. The discoverable Client also opens the main-session workspace at $HOME/CodeAsYouWish. An absolute CAYW_MAIN_SESSION wins; otherwise an absolute main_session_dir in the device config is used.\ninit creates private Client/workspace state and the workspace database without starting a service. Existing files are kept.\nserve also initializes automatically when it uses default locations and joins the same device-wide Client state.\nA serve with explicit --db, --token-env and --config is a separate Headless boundary: it does not read or create HOME/CAYW_HOME state, live settings are unavailable and --config is authoritative.\nRun `cayw` to start or reuse the unique Client, open the current workspace and show TUI. `cayw tui` remains an attach-only command. WebUI starts only with --ui web. Local browser access has no login; explicit --lan allows network access without login unless an optional password account is configured.\nRun `cayw conf` to set up the device model list and select it for the current workspace. It is the only interactive first-time model setup command. Use `cayw conf --config PATH` only for a fully explicit Headless configuration; that path does not read the device model list.\nStartup never opens a model setup wizard. Without a model, Client, TUI and WebUI still start and Sessions can be created or queried; prompt, enqueue and resume return model_not_configured before task admission.\nA model object specifies service, api, id, base_url, api_version, max_output_tokens, context_window, an optional tokenizer id and an optional plain-text api_key. Omitting api_key selects a service that needs no authentication. context_window is the model context window in tokens; omitting it leaves the field absent.\nStarting the Client or a UI and creating Sessions make no model request.\n--config also reads host-owned budgets, fixed process profiles, exact permissions, request policy, optional web_access / vision / pi_compat options and an absolute mcp_servers_file. Profiles and request scopes alone grant no permission.\nOffline mode is explicit and uses a synthetic provider with real bounded Luau and Host file operations.\n--demo-authorized requires --offline and grants only the two exact demo file calls.\nClient and Headless IPC bind loopback only. Disconnects cancel that connection's in-flight prompts.";

pub(crate) struct WorkspaceHost {
    runtime: Arc<Runtime>,
    request_timeout_ms: Option<u64>,
    settings: Option<tokio::sync::Mutex<cli_settings::Settings>>,
    resources: tokio::sync::Mutex<Option<cli_workspaces::WorkspaceResources>>,
    model_available: bool,
    model_setup_hint: String,
    disabled_services: Vec<cayw_runtime::DisabledService>,
}

impl WorkspaceHost {
    fn new(
        runtime: Arc<Runtime>,
        request_timeout_ms: Option<u64>,
        settings: Option<cli_settings::Settings>,
        resources: cli_workspaces::WorkspaceResources,
        model_available: bool,
        model_setup_hint: String,
        optional_services: cli_workspaces::OptionalServicesReport,
    ) -> Self {
        Self {
            runtime,
            request_timeout_ms,
            settings: settings.map(tokio::sync::Mutex::new),
            resources: tokio::sync::Mutex::new(Some(resources)),
            model_available,
            model_setup_hint,
            disabled_services: optional_services.disabled_services(),
        }
    }

    async fn shutdown(&self) -> Result<(), Box<dyn Error>> {
        let mut failures = Vec::new();
        #[cfg(feature = "plugins")]
        if let Some(settings) = self.settings.as_ref() {
            let settings = settings.lock().await;
            if let Err(error) = settings
                .plugins
                .shutdown_workspace(&settings.paths, &self.runtime)
                .await
            {
                failures.push(error.to_string());
            }
        } else {
            for plugin in self.runtime.list_plugins() {
                if let Err(error) = self.runtime.unload_plugin(&plugin.manifest.name).await {
                    failures.push(error.to_string());
                }
            }
        }
        if let Some(resources) = self.resources.lock().await.take()
            && let Err(error) = resources.shutdown().await
        {
            failures.push(error.to_string());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; ").into())
        }
    }

    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        if method.starts_with("settings.") {
            let Some(settings) = self.settings.as_ref() else {
                return Err(RpcError::new(
                    "settings_unavailable",
                    "settings require a local workspace; this serve uses explicit paths",
                ));
            };
            return settings
                .lock()
                .await
                .call(&self.runtime, method, params, cancel)
                .await;
        }
        if matches!(
            method,
            "session.prompt"
                | "task.enqueue"
                | "task.enqueue_with_intent"
                | "task.enqueue_scheduled"
                | "task.enqueue_with_resources"
                | "task.resume"
        ) {
            let available = match self.settings.as_ref() {
                Some(settings) => settings.lock().await.available,
                None => self.model_available,
            };
            if !available {
                return Err(RpcError::new(
                    "model_not_configured",
                    self.model_setup_hint.clone(),
                ));
            }
        }
        let mut response = self.runtime.call(method, params, cancel).await?;
        if method == "runtime.health"
            && let Value::Object(ref mut map) = response
        {
            map.insert(
                "request_timeout_ms".to_string(),
                json!(self.request_timeout_ms),
            );
            let mut disabled = map
                .get("disabled_services")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for service in &self.disabled_services {
                disabled.push(json!(service));
            }
            map.insert("disabled_services".into(), Value::Array(disabled));
        }
        Ok(response)
    }

    fn read_only(&self, method: &str) -> bool {
        self.runtime.read_only(method)
    }
}

/// Print one startup warning per disabled optional service. Health and doctor
/// carry the same reasons for later inspection.
pub(crate) fn warn_disabled_services(services: &[cayw_runtime::DisabledService]) {
    for service in services {
        eprintln!(
            "warning: optional service disabled: {}: {}",
            service.name, service.reason
        );
    }
}

struct CliRpcHandler {
    primary: Arc<WorkspaceHost>,
    workspaces: Option<cli_workspaces::Workspaces>,
    client_plugins: Option<cli_client_plugins::ClientPlugins>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceCall {
    workspace_id: String,
    rpc: RoutedRpc,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutedRpc {
    method: String,
    params: Value,
}

fn workspace_error(error: Box<dyn Error>) -> RpcError {
    let code = if let Some(error) = error.downcast_ref::<cayw_runtime::RuntimeError>() {
        error.code()
    } else if cli_settings::error_outcome_unknown(error.as_ref()) {
        "workspace_outcome_unknown"
    } else {
        "settings_error"
    };
    RpcError::new(code, error.to_string())
}

fn client_workspace_error(error: Box<dyn Error>) -> RpcError {
    let error = workspace_error(error);
    if error.code.ends_with("_outcome_unknown") {
        error
    } else {
        RpcError::new("workspace_unavailable", error.message)
    }
}

#[cfg(all(test, feature = "plugins"))]
#[test]
fn workspace_route_preserves_plugin_unknown_code() {
    let error = workspace_error(Box::new(cayw_runtime::RuntimeError::Plugin(
        cayw_plugins::PluginError::OutcomeUnknown("synthetic restore uncertainty".into()),
    )));
    assert_eq!(error.code, "plugin_outcome_unknown");
    assert!(error.message.contains("synthetic restore uncertainty"));
}

struct UnconfiguredProvider;
impl Provider for UnconfiguredProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = AssistantMessage::new(&request.model).failure(
            StopReason::Error,
            "model is not configured; run `cayw conf` or open WebUI model settings",
        );
        Box::pin(futures::stream::iter([AssistantEvent::Error { message }]))
    }
}

#[async_trait]
impl RpcHandler for CliRpcHandler {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        if matches!(
            method,
            "settings.workspaces.list"
                | "settings.workspaces.open"
                | "client.workspaces.list"
                | "client.workspaces.open"
        ) {
            let Some(workspaces) = self.workspaces.as_ref() else {
                return Err(RpcError::new(
                    "settings_unavailable",
                    "workspace routing requires the default local Client",
                ));
            };
            let routed = match method {
                "client.workspaces.list" => "settings.workspaces.list",
                "client.workspaces.open" => "settings.workspaces.open",
                _ => method,
            };
            return workspaces
                .call(routed, params, cancel)
                .await
                .map_err(workspace_error);
        }
        if method.starts_with("client.plugins.") {
            let Some(plugins) = self.client_plugins.as_ref() else {
                return Err(RpcError::new(
                    "client_plugins_unavailable",
                    "Client plugins require the default local Client",
                ));
            };
            if method == "client.plugins.start" {
                let workspace_id = plugins.start_workspace(&params)?;
                let Some(workspaces) = self.workspaces.as_ref() else {
                    return Err(RpcError::new(
                        "workspace_unavailable",
                        "Client plugins require the default local workspace catalog",
                    ));
                };
                workspaces
                    .call(
                        "settings.workspaces.open",
                        json!({"id":workspace_id}),
                        cancel.clone(),
                    )
                    .await
                    .map_err(client_workspace_error)?;
            }
            return plugins.call(method, params, cancel).await;
        }
        if method == "client.health" {
            let _: cli_workspaces::Empty = serde_json::from_value(params).map_err(|error| {
                RpcError::new(
                    "invalid_params",
                    format!("invalid Client health request: {error}"),
                )
            })?;
            let mut health = self
                .primary
                .call("runtime.health", json!({}), cancel)
                .await?;
            if let Value::Object(ref mut map) = health {
                map.insert("client".into(), json!(true));
                map.insert(
                    "open_workspaces".into(),
                    json!(match self.workspaces.as_ref() {
                        Some(workspaces) => workspaces.open_count().await,
                        None => 1,
                    }),
                );
                map.insert(
                    "client_plugins".into(),
                    match self.client_plugins.as_ref() {
                        Some(plugins) => plugins.read_only_summary().await,
                        None => json!({"schema":"cayw.client-plugins.v1","entries":[]}),
                    },
                );
            }
            return Ok(health);
        }
        if method == "client.workspace.call" {
            let routed: WorkspaceCall = serde_json::from_value(params).map_err(|error| {
                RpcError::new(
                    "invalid_params",
                    format!("invalid workspace route: {error}"),
                )
            })?;
            if routed.rpc.method.starts_with("client.")
                || routed.rpc.method.starts_with("settings.workspaces.")
                || matches!(routed.rpc.method.as_str(), "hello" | "rpc.cancel")
            {
                return Err(RpcError::new(
                    "invalid_params",
                    "workspace routes cannot call Client or transport methods",
                ));
            }
            let Some(workspaces) = self.workspaces.as_ref() else {
                return Err(RpcError::new(
                    "settings_unavailable",
                    "workspace routing requires the default local Client",
                ));
            };
            let host = workspaces
                .host(&routed.workspace_id)
                .await
                .map_err(|error| RpcError::new("workspace_unavailable", error.to_string()))?;
            return host
                .call(&routed.rpc.method, routed.rpc.params, cancel)
                .await;
        }
        self.primary.call(method, params, cancel).await
    }
    fn read_only(&self, method: &str) -> bool {
        if matches!(method, "client.health" | "client.workspaces.list") {
            true
        } else if method == "client.workspace.call"
            || method.starts_with("client.plugins.")
            || method.starts_with("settings.")
        {
            // Routed calls and settings mutations need the actual request to
            // decide; keep the conservative unknown classification.
            false
        } else {
            self.primary.read_only(method)
        }
    }
}

/// Open the main-session workspace and remember its one session before IPC accepts traffic.
/// Fully explicit Headless never reaches this; `init` and `conf` return earlier.
async fn open_main_session(
    handler: &CliRpcHandler,
    configured: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let directory = cayw_runtime::local_cli::prepare_main_session_directory(configured)?;
    let paths = LocalPaths::from_environment(&directory)?;
    paths.initialize()?;
    let workspaces = handler
        .workspaces
        .as_ref()
        .expect("discoverable Client owns workspace routing");
    let id = workspaces.id_for(&paths.workspace);
    workspaces
        .call(
            "settings.workspaces.open",
            json!({"id": id}),
            CancellationToken::new(),
        )
        .await?;
    let host = workspaces.host(&id).await?;
    cli_settings::ensure_main_session(&host.runtime, &paths)?;
    // The main session can dispatch, pause, stop and observe Tasks in the
    // Client's registered workspaces. The router owns only a weak reference to
    // the Client catalog, so shutdown is not held open by the Runtime.
    host.runtime.set_work_router(workspaces.work_router())?;
    Ok(())
}

async fn shutdown_handler(handler: &CliRpcHandler) -> Result<(), Box<dyn Error>> {
    let client_cleanup = match handler.client_plugins.as_ref() {
        Some(plugins) => plugins.shutdown().await.map_err(|error| error.to_string()),
        None => Ok(()),
    };
    let workspace_cleanup = match handler.workspaces.as_ref() {
        Some(workspaces) => workspaces.shutdown().await,
        None => handler.primary.shutdown().await,
    }
    .map_err(|error| error.to_string());
    match (client_cleanup, workspace_cleanup) {
        (Err(client), Err(workspace)) => Err(format!(
            "Client plugin cleanup failed: {client}; workspace cleanup also failed: {workspace}"
        )
        .into()),
        (Err(client), _) => Err(client.into()),
        (_, Err(workspace)) => Err(workspace.into()),
        _ => Ok(()),
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("cayw: {error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        return cli_start::run(std::iter::empty()).await;
    };
    if command == "--help" || command == "-h" || command == "help" {
        println!("{USAGE}");
        return Ok(());
    }
    if command.starts_with("--") {
        return cli_start::run(std::iter::once(command).chain(arguments)).await;
    }
    if command == "account" {
        return cli_start::account(arguments).await;
    }
    if command == "start" {
        return cli_start::run(arguments).await;
    }
    if command == "models" || command == "plugins" {
        return cli_settings::run(arguments, command == "plugins").await;
    }
    if command == "tui" {
        return cli_tui::run(arguments).await;
    }
    if !matches!(command.as_str(), "serve" | "init" | "conf" | "doctor") {
        return Err("expected `start`, `models`, `plugins`, `init`, `conf`, `doctor`, `serve` or `tui`; use --help".into());
    }
    let initialize_only = command == "init";
    let configure_only = command == "conf";
    let doctor_only = command == "doctor";
    let mut values = BTreeMap::new();
    let mut offline = false;
    let mut demo_authorized = false;
    let mut pi_compat = false;
    while let Some(flag) = arguments.next() {
        match flag.as_str() {
            "--offline" => offline = true,
            "--demo-authorized" => demo_authorized = true,
            "--pi-compat" => pi_compat = true,
            "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--db" | "--workspace" | "--listen" | "--token-env" | "--config" => {
                let value = arguments.next().ok_or("missing flag value")?;
                if values.insert(flag, value).is_some() {
                    return Err("duplicate option".into());
                }
            }
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    if initialize_only
        && (offline
            || demo_authorized
            || pi_compat
            || values.keys().any(|key| key != "--workspace"))
    {
        return Err("init accepts only --workspace; use --help".into());
    }
    if configure_only
        && (offline
            || demo_authorized
            || pi_compat
            || values
                .keys()
                .any(|key| !matches!(key.as_str(), "--workspace" | "--config")))
    {
        return Err("conf accepts only --workspace and --config; use --help".into());
    }
    if doctor_only
        && (offline
            || demo_authorized
            || pi_compat
            || values
                .keys()
                .any(|key| !matches!(key.as_str(), "--workspace" | "--config")))
    {
        return Err("doctor accepts only --workspace and --config; use --help".into());
    }
    if demo_authorized && !offline {
        return Err("--demo-authorized requires --offline".into());
    }
    let workspace = values
        .remove("--workspace")
        .map(PathBuf::from)
        .map_or_else(std::env::current_dir, Ok)?
        .canonicalize()?;
    if !workspace.is_dir() {
        return Err("--workspace must be an existing directory".into());
    }
    let address: SocketAddr = values
        .remove("--listen")
        .unwrap_or_else(|| "127.0.0.1:0".into())
        .parse()?;
    if !address.ip().is_loopback() {
        return Err("local runtime IPC must bind a loopback address".into());
    }
    let explicit_db = values.remove("--db").map(PathBuf::from);
    let explicit_config = values.remove("--config").map(PathBuf::from);
    let explicit_token = values.remove("--token-env");
    if doctor_only {
        return cli_doctor::run(&workspace, explicit_config.as_deref()).await;
    }
    let default_database = explicit_db.is_none();
    let local_token = explicit_token.is_none();
    let discoverable = default_database && local_token;
    let fully_explicit =
        explicit_db.is_some() && explicit_config.is_some() && explicit_token.is_some();
    let local =
        if (!configure_only || explicit_config.is_none()) && (initialize_only || !fully_explicit) {
            let paths = LocalPaths::from_environment(&workspace)?;
            paths.initialize()?;
            Some(paths)
        } else {
            None
        };
    let config_file =
        explicit_config.or_else(|| local.as_ref().map(|paths| paths.config_file.clone()));
    let settings_paths = local.as_ref().cloned();
    let mut headless = match config_file.as_ref() {
        Some(path) if !configure_only || path.try_exists()? => HeadlessConfig::load(path)?,
        _ => HeadlessConfig::default(),
    };
    if configure_only {
        let mut activation = Value::Null;
        let (configured, model_configured, output_path, path_kind) =
            if let Some(paths) = settings_paths.as_ref() {
                let configured = cli_setup::configure_local(paths)?;
                if configured {
                    activation = cli_settings::reload_running_model(paths).await;
                }
                (
                    configured,
                    cli_settings::selected_model(paths)?.is_some(),
                    paths.data_dir.join("models.json"),
                    "models_file",
                )
            } else {
                let path = config_file.as_ref().expect("explicit configuration path");
                let configured = cli_setup::configure(path)?;
                let model_configured = if path.try_exists()? {
                    HeadlessConfig::load(path)?.model().is_some()
                } else {
                    false
                };
                (configured, model_configured, path.clone(), "config_file")
            };
        let mut report = json!({
            "configured":configured,
            "model_configured":model_configured,
            "activation":activation
        });
        report[path_kind] = json!(output_path);
        println!("{report}");
        if activation["error"].is_string() {
            return Err(format!("模型方案已保存，但运行服务的应用结果未确认：{}。请核对设备模型列表和 Client 状态，再决定是否重试或重启。", activation["error"].as_str().unwrap_or("unknown activation error")).into());
        }
        return Ok(());
    }
    let stored_model_configured = match settings_paths.as_ref() {
        Some(paths) => cli_settings::selected_model(paths)?.is_some(),
        None => false,
    };
    let db = explicit_db
        .or_else(|| local.as_ref().map(|paths| paths.database.clone()))
        .expect("explicit or local database");
    if initialize_only {
        let mut config = headless.clone().into_runtime_config()?;
        let paths = local.as_ref().expect("local initialization paths");
        config.recovery = Some(cayw_runtime::recovery_points::RecoveryConfig {
            data_dir: Some(paths.data_dir.clone()),
            ..Default::default()
        });
        if let Some(warning) =
            cayw_runtime::recovery_points::availability_warning(&paths.workspace, paths.state_dir())
        {
            eprintln!("{warning}");
        }
        let runtime = Runtime::open(
            &db,
            &workspace,
            Arc::new(UnconfiguredProvider),
            Model::new("unconfigured", "unconfigured"),
            config,
        )?;
        if let Err(error) = runtime.create_recovery_point() {
            eprintln!("首次恢复点未能建立：{error}；工作继续");
        }
        drop(runtime);
        if headless.model().is_none() && !stored_model_configured {
            eprintln!("{}", local_model_setup_hint(paths));
        }
        println!(
            "{}",
            json!({
                "initialized":true,"workspace":workspace,"database":db,
                "data_dir":paths.data_dir,"config_file":paths.config_file,
                "token_file":paths.token_file,"model_configured":stored_model_configured || headless.model().is_some()
            })
        );
        return Ok(());
    }
    let token = match explicit_token {
        Some(name) => {
            std::env::var(&name).map_err(|_| "IPC token environment variable is missing")?
        }
        None => local.as_ref().expect("local token paths").token()?,
    };
    if token.len() < 16 {
        return Err("IPC token must contain at least 16 bytes".into());
    }
    if pi_compat {
        headless.enable_pi_compat();
    }
    let main_session_override = headless.main_session_dir().map(PathBuf::from);
    let selected = match settings_paths.as_ref() {
        Some(paths) => {
            let choice = cli_settings::model_choice(paths)?;
            if let Some(message) = choice.fallback {
                eprintln!("{message}");
            }
            choice.model
        }
        None => headless.model().cloned(),
    };
    if offline && selected.is_some() {
        return Err(
            "model configuration conflicts with --offline; omit model for a fixture run".into(),
        );
    }
    let model_configured = selected.is_some();
    let model_available = offline || model_configured;
    #[cfg(feature = "plugins")]
    let client_plugins = settings_paths
        .as_ref()
        .map(|paths| cli_plugins::Plugins::new(paths.data_dir.join("plugins.json")));
    let mut settings = settings_paths.map(|paths| cli_settings::Settings {
        #[cfg(feature = "plugins")]
        plugins: client_plugins
            .as_ref()
            .expect("local Client plugin supervisor")
            .clone(),
        pi_enabled: false,
        paths,
        offline,
        available: model_available,
    });
    let mcp_servers_file = headless.mcp_servers_file().map(PathBuf::from);
    let pi_compat_cfg = headless.pi_compat().cloned();
    let mut config = headless.into_runtime_config()?;
    if let Some(paths) = local.as_ref() {
        // The local Client enables CodeMode model decisions. Explicit
        // Headless/SDK callers retain their configured approval mode.
        if default_database {
            config.approval_rules = Some(cayw_runtime::device_rules(&paths.data_dir));
            config.approval_mode = cayw_runtime::ApprovalMode::ModelAuto;
        }
        config.main_session =
            cayw_runtime::local_cli::main_session_directory(main_session_override.as_deref())
                .ok()
                .and_then(|directory| std::fs::canonicalize(directory).ok())
                .is_some_and(|directory| directory == paths.workspace);
        // The discoverable local Client enables private Git recovery by
        // default. Fully explicit Headless keeps its SDK behavior.
        config.recovery = Some(cayw_runtime::recovery_points::RecoveryConfig {
            data_dir: Some(paths.data_dir.clone()),
            ..Default::default()
        });
        if let Some(warning) =
            cayw_runtime::recovery_points::availability_warning(&paths.workspace, paths.state_dir())
        {
            eprintln!("{warning}");
        }
        // Record recovery coverage independently of operation approvals.
        if default_database
            && cayw_runtime::recovery_points::workspace_enabled(&paths.workspace, paths.state_dir())
        {
            config.recovery_scope =
                Some(cayw_runtime::RecoveryScope::new([paths.workspace.clone()]));
        }
    }
    let model_configuration = selected.as_ref().map(|model| model.public_view());
    let (provider, model): (Arc<dyn Provider>, Model) = if let Some(selected) = selected {
        let configured = selected.build()?;
        (configured.provider, configured.model)
    } else if offline {
        (
            Arc::new(OfflineProvider::default()),
            Model::new("fixture", "offline-runtime"),
        )
    } else {
        (
            Arc::new(UnconfiguredProvider),
            Model::new("unconfigured", "unconfigured"),
        )
    };
    if demo_authorized {
        config.permissions.extend([
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"cayw-demo.txt","content":"headless runtime demo"}),
            ),
            ScopedPermission::exact("fs.read", json!({"path":"cayw-demo.txt"})),
        ]);
    }

    let pi_compat_enabled = pi_compat || pi_compat_cfg.as_ref().is_some_and(|p| p.enabled);
    if let Some(settings) = settings.as_mut() {
        settings.pi_enabled = pi_compat_enabled;
    }
    let (workspace_resources, optional_services) = cli_workspaces::prepare_optional_services(
        &workspace,
        mcp_servers_file.as_deref(),
        pi_compat_enabled,
        pi_compat_cfg,
        pi_compat,
        &mut config,
    )
    .await?;
    let web_access_configured = config.host.web_access.is_some();
    let vision_configured = config.host.vision.is_some();
    let mut configuration = json!({
        "max_total_tokens":config.max_total_tokens,
        "max_turns":config.max_turns,
        "max_run_time_ms":config.max_run_time.map(|time| time.as_millis()),
        "max_capability_calls":config.max_capability_calls,
        "max_capability_bytes":config.max_capability_bytes,
        "host_wait_ms":config.codemode.host_wait.map(|time| time.as_millis()),
        "host_call_timeout_ms":config.host.call_timeout.map(|time| time.as_millis()),
        "profiles":config.host.profiles.keys().collect::<Vec<_>>(),
        "profile_io":"pipes",
        "exact_permission_count":config.permissions.len(),
        "mcp_installed":optional_services.mcp_installed,
        "mcp_skipped":optional_services.mcp_skipped,
        "web_access_enabled":web_access_configured,
        "vision_enabled":vision_configured,
        "pi_compat_enabled":pi_compat_enabled,
        "pi_installed":optional_services.pi_installed,
        "mcp_error":optional_services.mcp_error,
        "pi_error":optional_services.pi_error,
        "cleanup_error":optional_services.cleanup_error,
        "disabled_services":optional_services.disabled_services(),
    });
    let request_timeout = match config.max_run_time {
        Some(max_run_time) => match max_run_time.checked_add(std::time::Duration::from_secs(60)) {
            Some(timeout) => Some(timeout),
            None => {
                return cli_workspaces::cleanup_failed_setup(
                    workspace_resources,
                    "server request timeout calculation overflowed".into(),
                )
                .await;
            }
        },
        None => None,
    };
    let request_timeout_ms: Option<u64> = match request_timeout {
        Some(request_timeout) => {
            if request_timeout.is_zero()
                || std::time::Instant::now()
                    .checked_add(request_timeout)
                    .is_none()
            {
                return cli_workspaces::cleanup_failed_setup(
                    workspace_resources,
                    "server request timeout is invalid or non-representable".into(),
                )
                .await;
            }
            let timeout_ms = match u64::try_from(request_timeout.as_millis()) {
                Ok(timeout) => timeout,
                Err(_) => {
                    return cli_workspaces::cleanup_failed_setup(
                        workspace_resources,
                        "server request timeout in milliseconds exceeds u64".into(),
                    )
                    .await;
                }
            };
            if timeout_ms == 0 {
                return cli_workspaces::cleanup_failed_setup(
                    workspace_resources,
                    "server request timeout in milliseconds must be non-zero".into(),
                )
                .await;
            }
            Some(timeout_ms)
        }
        None => None,
    };
    let provider_name = model.provider.clone();
    let model_name = model.id.clone();
    let mut local_server = if default_database {
        match local
            .as_ref()
            .expect("local discovery paths")
            .acquire_server()
        {
            Ok(server) => Some(server),
            Err(error) => {
                return cli_workspaces::cleanup_failed_setup(
                    workspace_resources,
                    error.to_string(),
                )
                .await;
            }
        }
    } else {
        None
    };
    let runtime = match Runtime::open(&db, &workspace, provider, model, config) {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => {
            return cli_workspaces::cleanup_failed_setup(workspace_resources, error.to_string())
                .await;
        }
    };
    let mut disabled_services = runtime.disabled_services().to_vec();
    disabled_services.extend(optional_services.disabled_services());
    warn_disabled_services(&disabled_services);
    let disabled = |name: &str| disabled_services.iter().any(|service| service.name == name);
    configuration["web_access_enabled"] = json!(web_access_configured && !disabled("web_access"));
    configuration["vision_enabled"] = json!(vision_configured && !disabled("vision"));
    configuration["disabled_services"] = json!(disabled_services);
    if let Err(error) = runtime.create_recovery_point() {
        eprintln!("首次恢复点未能建立：{error}；工作继续");
    }
    #[cfg(feature = "plugins")]
    if let Some(settings) = settings.as_ref()
        && let Err(error) = settings.plugins.restore(&settings.paths, &runtime).await
    {
        // Plugin restore only degrades the plugin subsystem. The Client keeps
        // starting, and doctor plus the settings RPC report the same reason.
        eprintln!(
            "cayw: plugin restore failed; the Client continues without plugin supervision: {error}"
        );
    }
    let listener = match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            drop(runtime);
            return cli_workspaces::cleanup_failed_setup(workspace_resources, error.to_string())
                .await;
        }
    };
    let listen_address = match listener.local_addr() {
        Ok(address) => address,
        Err(error) => {
            drop(runtime);
            return cli_workspaces::cleanup_failed_setup(workspace_resources, error.to_string())
                .await;
        }
    };
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    let mut signals = match cli_start::ShutdownSignals::new() {
        Ok(signals) => signals,
        Err(error) => {
            drop(runtime);
            return cli_workspaces::cleanup_failed_setup(workspace_resources, error.to_string())
                .await;
        }
    };
    tokio::spawn(async move {
        signals.recv().await;
        signal.cancel();
    });
    let setup_hint = match local.as_ref() {
        Some(paths) => local_model_setup_hint(paths),
        None => headless_model_setup_hint(config_file.as_deref()),
    };
    let primary = Arc::new(WorkspaceHost::new(
        runtime,
        request_timeout_ms,
        settings,
        workspace_resources,
        model_available,
        setup_hint,
        optional_services,
    ));
    let client_role_plugins = if discoverable {
        match cli_client_plugins::ClientPlugins::new(
            listen_address,
            token.clone(),
            local.as_ref().expect("local Client paths").data_dir.clone(),
        ) {
            Ok(plugins) => Some(plugins),
            Err(error) => {
                let primary_error = error.to_string();
                return match primary.shutdown().await {
                    Ok(()) => Err(primary_error.into()),
                    Err(cleanup) => Err(format!(
                        "{primary_error}; workspace cleanup also failed: {cleanup}"
                    )
                    .into()),
                };
            }
        }
    } else {
        None
    };
    let workspaces = if discoverable {
        #[cfg(feature = "plugins")]
        {
            Some(cli_workspaces::Workspaces::new(
                local.as_ref().expect("local Client paths").clone(),
                offline,
                discoverable,
                primary.clone(),
                client_plugins.expect("local Client plugin supervisor"),
            ))
        }
        #[cfg(not(feature = "plugins"))]
        {
            Some(cli_workspaces::Workspaces::new(
                local.as_ref().expect("local Client paths").clone(),
                offline,
                discoverable,
                primary.clone(),
            ))
        }
    } else {
        None
    };
    let mut ipc = ServerConfig::new(token);
    ipc.request_timeout = request_timeout;
    let handler = Arc::new(CliRpcHandler {
        primary,
        workspaces,
        client_plugins: client_role_plugins,
    });
    if discoverable
        && let Err(error) = open_main_session(&handler, main_session_override.as_deref()).await
    {
        let primary_error = error.to_string();
        return match shutdown_handler(&handler).await {
            Ok(()) => Err(primary_error.into()),
            Err(cleanup) => {
                Err(format!("{primary_error}; Client cleanup also failed: {cleanup}").into())
            }
        };
    }
    if discoverable
        && let Err(error) = local_server
            .as_mut()
            .expect("local discovery server")
            .publish(listen_address)
    {
        let primary_error = error.to_string();
        return match shutdown_handler(&handler).await {
            Ok(()) => Err(primary_error.into()),
            Err(cleanup) => {
                Err(format!("{primary_error}; Client cleanup also failed: {cleanup}").into())
            }
        };
    }
    println!(
        "{}",
        json!({"listening":listen_address.to_string(),"protocol_version":cayw_ipc::PROTOCOL_VERSION,"offline_fixture":offline,"disconnect_policy":"cancel_in_flight","demo_authorized":demo_authorized,"provider":provider_name,"model":model_name,"model_configured":model_configured,"model_configuration":model_configuration,"configuration":configuration,"workspace":workspace,"database":db,"config_file":config_file,"token_file":local.as_ref().filter(|_| local_token).map(|paths| &paths.token_file),"client_file":local.as_ref().filter(|_| discoverable).map(|paths| &paths.client_file)})
    );
    if !model_available {
        eprintln!("{}", handler.primary.model_setup_hint);
        eprintln!("服务可创建和查询会话。配置模型后才能执行任务。");
    }
    let serving = serve(listener, handler.clone(), ipc, shutdown).await;
    let cleanup = shutdown_handler(&handler).await;
    match (serving, cleanup) {
        (Err(service), Err(cleanup)) => {
            Err(format!("{service}; cleanup also failed: {cleanup}").into())
        }
        (Err(service), _) => Err(service.into()),
        (_, Err(cleanup)) => Err(cleanup),
        _ => Ok(()),
    }
}
