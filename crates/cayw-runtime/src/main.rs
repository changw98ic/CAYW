use cayw_ai::{CancellationToken, Model, Provider};
use cayw_ipc::{ServerConfig, serve};
use cayw_runtime::{HeadlessConfig, Runtime, ScopedPermission, fixture::OfflineProvider};
use serde_json::json;
use std::{collections::BTreeMap, error::Error, net::SocketAddr, path::PathBuf, sync::Arc};

const USAGE: &str = "cayw serve --db PATH --workspace DIR --listen 127.0.0.1:PORT --token-env ENV [--config PATH] [--offline] [--demo-authorized]\n\nWithout --offline, --config must contain one explicit model object: service, api, id, base_url, api_version, max_output_tokens and api_key_env. Only that credential environment variable is read.\nServices: openai/chat_completions, magpie/chat_completions, anthropic/messages, gemini/stream_generate_content or gemini/generate_content.\nCAYW_PROVIDER_BASE_URL, CAYW_PROVIDER_API_KEY and CAYW_MODEL no longer select the server model.\nStarting a server makes no model request. session.prompt starts one.\n--config also reads host-owned JSON budgets, fixed pipe-based process profiles, exact permissions and optional task request policy/limits. Profiles and request scopes alone grant no permission.\nOffline mode requires model to be omitted and uses a synthetic provider with real bounded Luau + Host file operations.\n--demo-authorized grants only the two exact offline demo file calls. All other calls await approval.\nThe server binds loopback only. Disconnects cancel that connection's in-flight prompts.";

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
    while let Some(flag) = arguments.next() {
        match flag.as_str() {
            "--offline" => offline = true,
            "--demo-authorized" => demo_authorized = true,
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
    let headless = values
        .remove("--config")
        .map(HeadlessConfig::load)
        .transpose()?
        .unwrap_or_default();
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
    Ok(())
}
