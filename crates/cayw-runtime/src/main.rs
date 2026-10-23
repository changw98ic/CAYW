use cayw_ai::{CancellationToken, Model, Provider, providers::OpenAiProvider};
use cayw_ipc::{ServerConfig, serve};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, fixture::OfflineProvider};
use serde_json::json;
use std::{collections::BTreeMap, error::Error, net::SocketAddr, path::PathBuf, sync::Arc};

const USAGE: &str = "cayw serve --db PATH --workspace DIR [--listen 127.0.0.1:PORT] [--offline] [--demo-authorized]\ncayw bind --server http://127.0.0.1:PORT [--bind-id ID] [--label NAME]\n\nWithout --offline, set CAYW_PROVIDER_BASE_URL, CAYW_PROVIDER_API_KEY and CAYW_MODEL explicitly.\nStarting a server makes no model request. session.prompt starts one.\nOffline mode is a synthetic provider using real bounded Luau + Host file operations.\n--demo-authorized grants only the two exact offline demo file calls. All other calls await approval.\nThe server binds loopback only. Disconnects cancel that connection's in-flight prompts.";

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
    if command == "bind" {
        return run_bind(arguments).await;
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
    let mut ipc = if let Some(token_env) = values.remove("--token-env") {
        let token =
            std::env::var(&token_env).map_err(|_| "IPC token environment variable is missing")?;
        if token.len() < 16 {
            return Err("IPC token must contain at least 16 bytes".into());
        }
        ServerConfig::new(token)
    } else {
        ServerConfig::loopback()
    };
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
    ipc.request_timeout = std::time::Duration::from_secs(360);
    ipc.frame_timeout = std::time::Duration::from_secs(360);
    serve(listener, Arc::new(runtime), ipc, shutdown).await?;
    Ok(())
}

async fn run_bind(mut arguments: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    use cayw_remote::{PersistentRsaDeviceBinding, RsaKeyPair, RsaPublicKey};

    let mut server = None;
    let mut bind_id_opt = None;
    let mut label = None;
    while let Some(flag) = arguments.next() {
        match flag.as_str() {
            "--server" => server = Some(arguments.next().ok_or("missing --server value")?),
            "--bind-id" => bind_id_opt = Some(arguments.next().ok_or("missing --bind-id value")?),
            "--label" => label = Some(arguments.next().ok_or("missing --label value")?),
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    let raw_server = server.ok_or("--server is required")?;
    let host_port = raw_server
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_end_matches('/');
    let default_host = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "macOS-Device".to_owned());
    let device_label = label.unwrap_or_else(|| format!("{default_host} (macOS)"));

    // 1. Generate the Device's own RSA keypair locally on the Device.
    // The private key never leaves this Device process.
    let device_keypair = RsaKeyPair::generate()?;
    let device_pub = device_keypair.public_key();

    // 2. Phase 1: Send only `device_public_key` to `/api/bind/hello` and receive an RSA-encrypted challenge.
    let hello_payload = json!({
        "bind_id": bind_id_opt,
        "label": device_label,
        "device_address": format!("{default_host} · 独立进程 RSA 挑战握手"),
        "device_public_key": device_pub,
    });
    let hello_res = http_post_json(host_port, "/api/bind/hello", &hello_payload).await?;
    let hello_obj = hello_res
        .get("result")
        .ok_or("Server /api/bind/hello did not return a result")?;
    let bind_id = hello_obj
        .get("bind_id")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing bind_id in hello response")?;
    let server_pub: RsaPublicKey = serde_json::from_value(
        hello_obj
            .get("server_public_key")
            .cloned()
            .ok_or("missing server_public_key in hello response")?,
    )?;
    let encrypted_challenge = hello_obj
        .get("encrypted_challenge")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing encrypted_challenge in hello response")?;
    let server_signature = hello_obj
        .get("server_signature")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing server_signature in hello response")?;

    // 3. Verify Server's RSA signature and decrypt the challenge using Device's local RSA private key.
    if !server_pub.verify_sha256(
        format!("{bind_id}:{encrypted_challenge}").as_bytes(),
        server_signature,
    ) {
        return Err("Server RSA signature verification failed during bind hello".into());
    }
    let challenge_bytes = device_keypair.decrypt(encrypted_challenge)?;
    let challenge_nonce = String::from_utf8(challenge_bytes)?;

    // 4. Phase 2: Encrypt the challenge response to the Server's public key and sign with Device's private key.
    let proof = format!("confirm:{bind_id}:{challenge_nonce}");
    let encrypted_response = server_pub.encrypt(proof.as_bytes())?;
    let device_signature = device_keypair.sign_sha256(proof.as_bytes())?;

    let confirm_payload = json!({
        "bind_id": bind_id,
        "encrypted_response": encrypted_response,
        "device_signature": device_signature,
    });
    let confirm_res = http_post_json(host_port, "/api/bind/confirm", &confirm_payload).await?;
    let created = confirm_res
        .get("result")
        .and_then(|r| r.get("created"))
        .ok_or("Server /api/bind/confirm rejected RSA handshake")?;
    let device_id = created
        .get("device_id")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing device_id in confirm response")?;

    // 5. Persist the local Device credential (private key stays strictly on the Device).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let local_binding = PersistentRsaDeviceBinding {
        device_id: device_id.to_owned(),
        server_url: format!("http://{host_port}"),
        device_keypair: device_keypair.clone(),
        server_public_key: server_pub.clone(),
        bound_at_ms: now_ms,
    };
    if let Ok(home) = std::env::var("HOME") {
        let path = PathBuf::from(home)
            .join(".cayw")
            .join("device-rsa-binding.json");
        let _ = local_binding.save(&path);
    }

    // 6. Send an initial signed heartbeat proving active connection.
    let ts_str = now_ms.to_string();
    let hb_sig =
        device_keypair.sign_sha256(format!("heartbeat:{device_id}:{ts_str}").as_bytes())?;
    let _ = http_post_json(
        host_port,
        "/api/device/heartbeat",
        &json!({
            "device_id": device_id,
            "timestamp": ts_str,
            "signature": hb_sig,
        }),
    )
    .await;

    println!(
        "✓ 设备 [{device_label}] 已完成跨进程双向 RSA 挑战握手并绑定到 Server (http://{host_port})"
    );
    println!(
        "  设备 ID: {device_id} | 设备公钥指纹: {} | 服务端公钥指纹: {}",
        device_pub.fingerprint(),
        server_pub.fingerprint()
    );
    println!("  私钥仅保存在本设备 ~/.cayw/device-rsa-binding.json，未传输给 Server。");
    Ok(())
}

async fn http_post_json(
    host_port: &str,
    path: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn Error>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = payload.to_string();
    let mut stream = tokio::net::TcpStream::connect(host_port).await?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await?;
    let json_body = resp.split("\r\n\r\n").nth(1).unwrap_or(&resp);
    let parsed: serde_json::Value = serde_json::from_str(json_body)?;
    if let Some(err) = parsed.get("error") {
        return Err(format!("Server error: {err}").into());
    }
    Ok(parsed)
}
