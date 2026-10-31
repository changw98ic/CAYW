mod cli_setup;
mod cli_tui;

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Model, ModelRequest,
    Provider, StopReason,
};
use cayw_bridge::{AccessRequest, Limits, Provenance, Registry, RegistryPolicy, Visibility};
use cayw_codemode::CapabilityError;
use cayw_ipc::{RpcError, RpcHandler, ServerConfig, serve};
use cayw_runtime::{
    HeadlessConfig, Runtime, ScopedPermission,
    fixture::OfflineProvider,
    local_cli::{LocalPaths, model_setup_hint},
};
use cayw_sources::{InstalledServer, install_mcp_servers_file};
use serde_json::{Value, json};
use std::{collections::BTreeMap, error::Error, net::SocketAddr, path::PathBuf, sync::Arc};

const USAGE: &str = "cayw init [--workspace DIR]\ncayw configure [--workspace DIR] [--config PATH]\ncayw tui [--workspace DIR]\ncayw serve [--workspace DIR] [--db PATH] [--listen 127.0.0.1:PORT] [--token-env ENV] [--config PATH] [--offline] [--demo-authorized] [--pi-compat]\n\nFirst use automatically initializes ~/.cayw (override with an absolute CAYW_HOME).\nThe workspace defaults to the current directory. Each workspace has its own SQLite database.\ninit creates private configuration, an IPC token and the database without starting a service. Existing files are kept.\nserve also initializes automatically; cayw-client discovers its local connection from the same workspace.\nRun `cayw tui` in another terminal in that workspace for the interactive terminal interface.\nRun `cayw configure` to set up a model in ~/.cayw/config.json (or use --config PATH).\nWithout a model, a terminal serve offers guided setup; noninteractive use prints the next step.\nA model object specifies service, api, id, base_url, api_version, max_output_tokens and api_key_env. Only that credential environment variable is read.\nWithout a model, serve accepts queries but rejects task execution with model_not_configured before admission.\nStarting a server and creating sessions make no model request.\n--config also reads host-owned budgets, fixed process profiles, exact permissions, request policy, optional web_access / vision / pi_compat options and an absolute mcp_servers_file. Profiles and request scopes alone grant no permission.\nOffline mode is explicit and uses a synthetic provider with real bounded Luau and Host file operations.\n--demo-authorized requires --offline and grants only the two exact demo file calls.\nThe server binds loopback only. Disconnects cancel that connection's in-flight prompts.";

struct ServeBridgePolicy;
#[async_trait]
impl RegistryPolicy for ServeBridgePolicy {
    fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
        true
    }
    async fn authorize(
        &self,
        _: &AccessRequest,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

struct CliRpcHandler {
    runtime: Arc<Runtime>,
    request_timeout_ms: u64,
    model_available: bool,
    model_setup_hint: String,
}

struct UnconfiguredProvider;
impl Provider for UnconfiguredProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = AssistantMessage::new(&request.model).failure(
            StopReason::Error,
            "model is not configured; configure the CLI model and restart serve",
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
        if !self.model_available
            && matches!(
                method,
                "session.prompt"
                    | "task.enqueue"
                    | "task.enqueue_with_intent"
                    | "task.enqueue_scheduled"
                    | "task.enqueue_with_resources"
                    | "task.resume"
            )
        {
            return Err(RpcError::new(
                "model_not_configured",
                self.model_setup_hint.clone(),
            ));
        }
        let mut response = self.runtime.call(method, params, cancel).await?;
        if method == "runtime.health"
            && let Value::Object(ref mut map) = response
        {
            map.insert(
                "request_timeout_ms".to_string(),
                json!(self.request_timeout_ms),
            );
        }
        Ok(response)
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
        println!("{USAGE}");
        return Ok(());
    };
    if command == "--help" || command == "help" {
        println!("{USAGE}");
        return Ok(());
    }
    if command == "tui" {
        return cli_tui::run(arguments);
    }
    if !matches!(command.as_str(), "serve" | "init" | "configure") {
        return Err("expected `init`, `configure`, `serve` or `tui`; use --help".into());
    }
    let initialize_only = command == "init";
    let configure_only = command == "configure";
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
        return Err("configure accepts only --workspace and --config; use --help".into());
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
    let default_database = explicit_db.is_none();
    let local_token = explicit_token.is_none();
    let discoverable = default_database && local_token;
    let local = if (!configure_only || explicit_config.is_none())
        && (initialize_only || explicit_db.is_none() || explicit_token.is_none())
    {
        let paths = LocalPaths::from_environment(&workspace)?;
        paths.initialize()?;
        Some(paths)
    } else {
        None
    };
    let config_file =
        explicit_config.or_else(|| local.as_ref().map(|paths| paths.config_file.clone()));
    let mut headless = match config_file.as_ref() {
        Some(path) if !configure_only || path.try_exists()? => HeadlessConfig::load(path)?,
        _ => HeadlessConfig::default(),
    };
    if configure_only {
        let path = config_file.as_ref().expect("configuration path");
        let configured = if headless.model().is_some() {
            eprintln!("模型已配置：{}。如需更改，请编辑此文件。", path.display());
            true
        } else {
            cli_setup::configure(path)?
        };
        println!(
            "{}",
            json!({"configured":configured,"model_configured":configured,"config_file":path})
        );
        return Ok(());
    }
    let db = explicit_db
        .or_else(|| local.as_ref().map(|paths| paths.database.clone()))
        .expect("explicit or local database");
    if !initialize_only
        && !offline
        && headless.model().is_none()
        && cli_setup::interactive()
        && let Some(path) = config_file.as_ref()
        && cli_setup::offer(path)?
    {
        headless = HeadlessConfig::load(path)?;
    }
    if initialize_only {
        let config = headless.clone().into_runtime_config()?;
        let paths = local.as_ref().expect("local initialization paths");
        let runtime = Runtime::open(
            &db,
            &workspace,
            Arc::new(UnconfiguredProvider),
            Model::new("unconfigured", "unconfigured"),
            config,
        )?;
        drop(runtime);
        if headless.model().is_none() {
            eprintln!("{}", model_setup_hint(config_file.as_deref()));
        }
        println!(
            "{}",
            json!({
                "initialized":true,"workspace":workspace,"database":db,
                "data_dir":paths.data_dir,"config_file":paths.config_file,
                "token_file":paths.token_file,"model_configured":headless.model().is_some()
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
    let selected = headless.model().cloned();
    if offline && selected.is_some() {
        return Err(
            "model configuration conflicts with --offline; omit model for a fixture run".into(),
        );
    }
    let model_configured = selected.is_some();
    let model_available = offline || model_configured;
    let mcp_servers_file = headless.mcp_servers_file().map(PathBuf::from);
    let pi_compat_cfg = headless.pi_compat().cloned();
    let mut config = headless.into_runtime_config()?;
    let model_configuration = selected.as_ref().map(serde_json::to_value).transpose()?;
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
    #[allow(unused_mut)]
    let mut pi_installed = 0usize;
    #[cfg(feature = "pi-compat")]
    let mut pi_keepalive: Option<(
        Arc<cayw_pi_compat::PiCompatHost>,
        Vec<cayw_pi_compat::PiPackageId>,
    )> = None;

    // Keep Registry (and ManagedControl handles) alive for the server lifetime.
    let mut mcp_keepalive: Option<(Registry, Vec<InstalledServer>)> = None;
    let mut mcp_installed = 0usize;
    let mut mcp_skipped = 0usize;

    if mcp_servers_file.is_some() || pi_compat_enabled {
        let registry = Registry::new(Limits::default(), Arc::new(ServeBridgePolicy))?;

        if let Some(path) = mcp_servers_file {
            let report =
                install_mcp_servers_file(&path, &workspace, &registry, CancellationToken::new())
                    .await
                    .map_err(|error| format!("MCP servers install failed: {error}"))?;
            mcp_installed = report.installed.len();
            mcp_skipped = report.skipped.len();
            mcp_keepalive = Some((registry.clone(), report.installed));
        }

        if pi_compat_enabled {
            #[cfg(feature = "pi-compat")]
            {
                let plugin_registry = Arc::new(cayw_plugins::PluginRegistry::new());
                let pi_host = Arc::new(cayw_pi_compat::PiCompatHost::with_registry(
                    Arc::clone(&plugin_registry),
                    true,
                ));
                let mut loaded = Vec::new();
                let pi_cfg = pi_compat_cfg.unwrap_or_default();
                if pi_cfg.fixture
                    || (pi_compat && !pi_cfg.npm_hello && pi_cfg.sample_extension.is_none())
                {
                    let _desc = pi_host
                        .install_fixture()
                        .map_err(|e| format!("install pi fixture: {e}"))?;
                    let pkg = cayw_pi_compat::PiFixturePlugin::package_id();
                    pi_host
                        .register_into_bridge(&pkg, &registry)
                        .map_err(|e| format!("bridge pi fixture: {e}"))?;
                    pi_installed += 1;
                    loaded.push(pkg);
                }
                if let Some(sample_path) = pi_cfg.sample_extension {
                    let _desc = pi_host
                        .install_sample_extension(&sample_path, CancellationToken::new())
                        .await
                        .map_err(|e| format!("install pi sample extension: {e}"))?;
                    let pkg = cayw_pi_compat::PiSampleExtension::package_id();
                    pi_host
                        .register_into_bridge(&pkg, &registry)
                        .map_err(|e| format!("bridge pi sample extension: {e}"))?;
                    pi_installed += 1;
                    loaded.push(pkg);
                }
                if pi_cfg.npm_hello {
                    let _desc = pi_host
                        .install_npm_hello_extension(CancellationToken::new())
                        .await
                        .map_err(|e| format!("install pi npm hello extension: {e}"))?;
                    let pkg = cayw_pi_compat::PiNpmHelloExtension::package_id();
                    pi_host
                        .register_into_bridge(&pkg, &registry)
                        .map_err(|e| format!("bridge pi npm hello extension: {e}"))?;
                    pi_installed += 1;
                    loaded.push(pkg);
                }
                config.plugin_registry = Some(plugin_registry);
                pi_keepalive = Some((pi_host, loaded));
            }
            #[cfg(not(feature = "pi-compat"))]
            {
                return Err(
                    "pi-compat requires the `pi-compat` feature to be enabled in this binary"
                        .into(),
                );
            }
        }

        config.bridge = Some(registry.consumer());
    }

    let configuration = json!({
        "max_total_tokens":config.max_total_tokens,
        "max_turns":config.max_turns,
        "max_run_time_ms":config.max_run_time.as_millis(),
        "max_capability_calls":config.max_capability_calls,
        "max_capability_bytes":config.max_capability_bytes,
        "host_wait_ms":config.codemode.host_wait.as_millis(),
        "host_call_timeout_ms":config.host.call_timeout.as_millis(),
        "profiles":config.host.profiles.keys().collect::<Vec<_>>(),
        "profile_io":"pipes",
        "exact_permission_count":config.permissions.len(),
        "mcp_installed":mcp_installed,
        "mcp_skipped":mcp_skipped,
        "web_access_enabled":config.host.web_access.is_some(),
        "vision_enabled":config.host.vision.is_some(),
        "pi_compat_enabled":pi_compat_enabled,
        "pi_installed":pi_installed,
    });
    let request_timeout = config
        .max_run_time
        .checked_add(std::time::Duration::from_secs(60))
        .ok_or("server request timeout calculation overflowed")?;
    if request_timeout.is_zero()
        || std::time::Instant::now()
            .checked_add(request_timeout)
            .is_none()
    {
        return Err("server request timeout is invalid or non-representable".into());
    }
    let request_timeout_ms: u64 = u64::try_from(request_timeout.as_millis())
        .map_err(|_| "server request timeout in milliseconds exceeds u64")?;
    if request_timeout_ms == 0 {
        return Err("server request timeout in milliseconds must be non-zero".into());
    }
    let provider_name = model.provider.clone();
    let model_name = model.id.clone();
    let mut local_server = if default_database {
        Some(
            local
                .as_ref()
                .expect("local discovery paths")
                .acquire_server()?,
        )
    } else {
        None
    };
    let runtime = Runtime::open(&db, &workspace, provider, model, config)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::spawn(async move {
        let _ = interrupt.recv().await;
        signal.cancel();
    });
    if discoverable {
        local_server
            .as_mut()
            .expect("local discovery server")
            .publish(listener.local_addr()?)?;
    }
    println!(
        "{}",
        json!({"listening":listener.local_addr()?.to_string(),"protocol_version":cayw_ipc::PROTOCOL_VERSION,"offline_fixture":offline,"disconnect_policy":"cancel_in_flight","demo_authorized":demo_authorized,"provider":provider_name,"model":model_name,"model_configured":model_configured,"model_configuration":model_configuration,"configuration":configuration,"workspace":workspace,"database":db,"config_file":config_file,"token_file":local.as_ref().filter(|_| local_token).map(|paths| &paths.token_file),"connection_file":local.as_ref().filter(|_| discoverable).map(|paths| &paths.connection_file)})
    );
    let setup_hint = model_setup_hint(config_file.as_deref());
    if !model_available {
        eprintln!("{setup_hint}");
        eprintln!("服务可创建和查询会话。配置模型后才能执行任务。");
    }
    let mut ipc = ServerConfig::new(token);
    ipc.request_timeout = request_timeout;
    let handler = Arc::new(CliRpcHandler {
        runtime: Arc::new(runtime),
        request_timeout_ms,
        model_available,
        model_setup_hint: setup_hint,
    });
    serve(listener, handler, ipc, shutdown).await?;

    #[cfg(feature = "pi-compat")]
    if let Some((pi_host, loaded)) = pi_keepalive {
        for pkg in loaded {
            let _ = pi_host.unload(&pkg);
        }
    }

    if let Some((registry, installed)) = mcp_keepalive {
        for entry in installed {
            let _ = registry.disconnect(&entry.id);
            entry.control.close();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                entry.control.wait_closed(),
            )
            .await;
        }
    }
    Ok(())
}
