use async_trait::async_trait;
use cayw_ai::{CancellationToken, Model, Provider};
use cayw_bridge::{AccessRequest, Limits, Provenance, Registry, RegistryPolicy, Visibility};
use cayw_codemode::CapabilityError;
use cayw_ipc::{ServerConfig, serve};
use cayw_runtime::{HeadlessConfig, Runtime, ScopedPermission, fixture::OfflineProvider};
use cayw_sources::{InstalledServer, install_mcp_servers_file};
use serde_json::json;
use std::{collections::BTreeMap, error::Error, net::SocketAddr, path::PathBuf, sync::Arc};

const USAGE: &str = "cayw serve --db PATH --workspace DIR --listen 127.0.0.1:PORT --token-env ENV [--config PATH] [--offline] [--demo-authorized] [--pi-compat]\n\nWithout --offline, --config must contain one explicit model object: service, api, id, base_url, api_version, max_output_tokens and api_key_env. Only that credential environment variable is read.\nServices: openai/chat_completions, magpie/chat_completions, anthropic/messages, gemini/stream_generate_content or gemini/generate_content.\nCAYW_PROVIDER_BASE_URL, CAYW_PROVIDER_API_KEY and CAYW_MODEL no longer select the server model.\nStarting a server makes no model request. session.prompt starts one.\n--config also reads host-owned JSON budgets, fixed pipe-based process profiles, exact permissions, optional task request policy/limits, optional web_access / vision options, optional pi_compat options, and optional mcp_servers_file (absolute path to external mcpServers JSON; absolute local stdio only → ManagedStdio prepare → Registry → Runtime Bridge).\nProfiles and request scopes alone grant no permission.\nOffline mode requires model to be omitted and uses a synthetic provider with real bounded Luau + Host file operations.\n--demo-authorized grants only the two exact offline demo file calls. All other calls await approval.\nThe server binds loopback only. Disconnects cancel that connection's in-flight prompts.";

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
    if command != "serve" {
        return Err("expected `serve`; use --help".into());
    }
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
    let db = PathBuf::from(values.remove("--db").ok_or("--db is required")?);
    let workspace = PathBuf::from(
        values
            .remove("--workspace")
            .ok_or("--workspace is required")?,
    );
    if !workspace.is_absolute() || !workspace.is_dir() {
        return Err("--workspace must be an absolute existing directory".into());
    }
    let address: SocketAddr = values
        .remove("--listen")
        .unwrap_or_else(|| "127.0.0.1:0".into())
        .parse()?;
    if !address.ip().is_loopback() {
        return Err("local runtime IPC must bind a loopback address".into());
    }
    let token_env = values
        .remove("--token-env")
        .ok_or("--token-env is required")?;
    let token =
        std::env::var(&token_env).map_err(|_| "IPC token environment variable is missing")?;
    if token.len() < 16 {
        return Err("IPC token must contain at least 16 bytes".into());
    }
    if demo_authorized && !offline {
        return Err("--demo-authorized requires --offline".into());
    }
    let mut headless = values
        .remove("--config")
        .map(HeadlessConfig::load)
        .transpose()?
        .unwrap_or_default();
    if pi_compat {
        headless.enable_pi_compat();
    }
    let selected = headless.model().cloned();
    if offline && selected.is_some() {
        return Err(
            "model configuration conflicts with --offline; omit model for a fixture run".into(),
        );
    }
    if !offline && selected.is_none() {
        return Err(
            "model configuration is required in --config; use --offline for a no-network demo"
                .into(),
        );
    }
    let mcp_servers_file = headless.mcp_servers_file().map(PathBuf::from);
    let pi_compat_cfg = headless.pi_compat().cloned();
    let mut config = headless.into_runtime_config()?;
    let model_configuration = selected.as_ref().map(serde_json::to_value).transpose()?;
    let (provider, model): (Arc<dyn Provider>, Model) = if let Some(selected) = selected {
        let configured = selected.build()?;
        (configured.provider, configured.model)
    } else {
        (
            Arc::new(OfflineProvider::default()),
            Model::new("fixture", "offline-runtime"),
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
    let provider_name = model.provider.clone();
    let model_name = model.id.clone();
    let runtime = Runtime::open(db, workspace, provider, model, config)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!(
        "{}",
        json!({"listening":listener.local_addr()?.to_string(),"protocol_version":cayw_ipc::PROTOCOL_VERSION,"offline_fixture":offline,"disconnect_policy":"cancel_in_flight","demo_authorized":demo_authorized,"provider":provider_name,"model":model_name,"model_configuration":model_configuration,"configuration":configuration})
    );
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });
    let mut ipc = ServerConfig::new(token);
    ipc.request_timeout = std::time::Duration::from_secs(360);
    serve(listener, Arc::new(runtime), ipc, shutdown).await?;

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
