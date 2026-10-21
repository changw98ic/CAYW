use cayw_ai::{CancellationToken, Model, Provider, providers::OpenAiProvider};
use cayw_ipc::{ServerConfig, serve};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, fixture::OfflineProvider};
use serde_json::json;
use std::{collections::BTreeMap, error::Error, net::SocketAddr, path::PathBuf, sync::Arc};

const USAGE: &str = "cayw serve --db PATH --workspace DIR --listen 127.0.0.1:PORT --token-env ENV [--offline] [--demo-authorized]\n\nWithout --offline, set CAYW_PROVIDER_BASE_URL, CAYW_PROVIDER_API_KEY and CAYW_MODEL explicitly.\nStarting a server makes no model request. session.prompt starts one.\nOffline mode is a synthetic provider using real bounded Luau + Host file operations.\n--demo-authorized grants only the two exact offline demo file calls. All other calls await approval.\nThe server binds loopback only. Disconnects cancel that connection's in-flight prompts.";

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
            "--db" | "--workspace" | "--listen" | "--token-env" => {
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
    let (provider, model): (Arc<dyn Provider>, Model) = if offline {
        (
            Arc::new(OfflineProvider::default()),
            Model::new("fixture", "offline-runtime"),
        )
    } else {
        let base = std::env::var("CAYW_PROVIDER_BASE_URL").map_err(
            |_| "CAYW_PROVIDER_BASE_URL is required; use --offline for a no-network demo",
        )?;
        let key = std::env::var("CAYW_PROVIDER_API_KEY")
            .map_err(|_| "CAYW_PROVIDER_API_KEY is required")?;
        let id = std::env::var("CAYW_MODEL").map_err(|_| "CAYW_MODEL is required")?;
        let mut model = Model::new("openai-chat", id);
        model.max_output_tokens = Some(4096);
        (Arc::new(OpenAiProvider::new(base, key)?), model)
    };
    let mut config = RuntimeConfig::default();
    if demo_authorized {
        config.permissions = vec![
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"cayw-demo.txt","content":"headless runtime demo"}),
            ),
            ScopedPermission::exact("fs.read", json!({"path":"cayw-demo.txt"})),
        ];
    }
    let runtime = Runtime::open(db, workspace, provider, model, config)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!(
        "{}",
        json!({"listening":listener.local_addr()?.to_string(),"protocol_version":cayw_ipc::PROTOCOL_VERSION,"offline_fixture":offline,"disconnect_policy":"cancel_in_flight","demo_authorized":demo_authorized})
    );
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });
    let mut ipc = ServerConfig::new(token);
    ipc.request_timeout = std::time::Duration::from_secs(360);
    ipc.frame_timeout = std::time::Duration::from_secs(360);
    serve(listener, Arc::new(runtime), ipc, shutdown).await?;
    Ok(())
}
