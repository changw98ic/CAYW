use cayw_ai::{CancellationToken, Model, Provider};
use cayw_ipc::{ServerConfig, serve};
use cayw_runtime::{HeadlessConfig, Runtime, ScopedPermission, fixture::OfflineProvider};
use serde_json::json;
use std::{collections::BTreeMap, error::Error, net::SocketAddr, path::PathBuf, sync::Arc};

const USAGE: &str = "cayw serve --db PATH --workspace DIR [--listen 127.0.0.1:PORT] [--token-env ENV] [--config PATH] [--offline] [--demo-authorized]\ncayw bind --server http://127.0.0.1:PORT --bind-id ID [--label NAME] [--workspace DIR] [--db PATH] [--daemon]\ncayw device --server http://127.0.0.1:PORT [--workspace DIR] [--db PATH]\n\nWithout --offline, --config must contain one explicit model object: service, api, id, base_url, api_version, max_output_tokens and api_key_env. Only that credential environment variable is read.\nServices: openai/chat_completions, magpie/chat_completions, anthropic/messages, gemini/stream_generate_content or gemini/generate_content.\nCAYW_PROVIDER_BASE_URL, CAYW_PROVIDER_API_KEY and CAYW_MODEL no longer select the server model.\nStarting a server makes no model request. session.prompt starts one.\n--config also reads host-owned JSON budgets, fixed pipe-based process profiles, exact permissions and optional task request policy/limits. Profiles and request scopes alone grant no permission.\nOffline mode requires model to be omitted and uses a synthetic provider with real bounded Luau + Host file operations.\n--demo-authorized grants only the two exact offline demo file calls. All other calls await approval.\nThe server binds loopback only. Disconnects cancel that connection's in-flight prompts.";

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
    if command == "device" {
        return run_device(arguments).await;
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
    if demo_authorized {
        runtime.grant_extra_permission(ScopedPermission::exact(
            "fs.read",
            json!({"path":"device-marker.txt"}),
        ));
        runtime.grant_extra_permission(ScopedPermission::exact(
            "plan.update",
            json!({
                "expected_version": 0,
                "steps": [
                    {"id": "step-1", "text": "分析任务需求与工作区上下文", "status": "completed"},
                    {"id": "step-2", "text": "调用受控 CodeMode 能力执行文件读写", "status": "in_progress"},
                    {"id": "step-3", "text": "核验输出并完成任务总结", "status": "pending"}
                ]
            }),
        ));
        runtime.grant_extra_permission(ScopedPermission::exact(
            "plan.update",
            json!({
                "expected_version": 0,
                "steps": [
                    {"id": "step-1", "text": "分析任务需求与工作区上下文", "status": "completed"},
                    {"id": "step-2", "text": "调用受控 CodeMode 能力读取目标设备标识文件", "status": "in_progress"},
                    {"id": "step-3", "text": "核验输出并完成任务总结", "status": "pending"}
                ]
            }),
        ));
    }
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
    ipc.request_timeout = std::time::Duration::from_secs(360);
    ipc.frame_timeout = std::time::Duration::from_secs(360);
    serve(listener, Arc::new(runtime), ipc, shutdown).await?;
    Ok(())
}

fn parse_http_server_url(raw_server: &str) -> Result<String, Box<dyn Error>> {
    let trimmed = raw_server.trim();
    if trimmed.is_empty() {
        return Err("--server address cannot be empty".into());
    }
    if trimmed.starts_with("https://") || trimmed.starts_with("wss://") {
        return Err(
            "unsupported TLS scheme (https:// or wss://) on plain TCP transport; refusing to downgrade to plaintext HTTP"
                .into(),
        );
    }
    let without_scheme = if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        rest
    } else if trimmed.contains("://") {
        return Err(format!("unsupported URL scheme in {trimmed}").into());
    } else {
        trimmed
    };
    let host_port = without_scheme.trim_end_matches('/');
    if host_port.is_empty()
        || host_port.contains('/')
        || host_port.contains('@')
        || host_port.contains('?')
        || host_port.contains('#')
        || !host_port.contains(':')
    {
        return Err(format!("invalid server host:port in {trimmed}").into());
    }
    Ok(host_port.to_owned())
}

fn default_identity_path(custom: Option<String>) -> PathBuf {
    if let Some(p) = custom.filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(p);
    }
    if let Ok(env_p) = std::env::var("CAYW_DEVICE_IDENTITY")
        && !env_p.trim().is_empty()
    {
        return PathBuf::from(env_p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(home)
        .join(".cayw")
        .join("device-rsa-binding.json")
}

async fn run_bind(mut arguments: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    use cayw_remote::{PersistentRsaDeviceBinding, RsaKeyPair, RsaPublicKey};

    let mut server = None;
    let mut bind_id_opt = None;
    let mut label = None;
    let mut workspace_opt = None;
    let mut db_opt = None;
    let mut identity_opt = None;
    let mut no_daemon = false;
    let mut foreground = false;
    while let Some(flag) = arguments.next() {
        match flag.as_str() {
            "--server" => server = Some(arguments.next().ok_or("missing --server value")?),
            "--bind-id" => bind_id_opt = Some(arguments.next().ok_or("missing --bind-id value")?),
            "--label" => label = Some(arguments.next().ok_or("missing --label value")?),
            "--workspace" => {
                workspace_opt = Some(arguments.next().ok_or("missing --workspace value")?)
            }
            "--db" => db_opt = Some(arguments.next().ok_or("missing --db value")?),
            "--identity" => {
                identity_opt = Some(arguments.next().ok_or("missing --identity value")?)
            }
            "--no-daemon" => no_daemon = true,
            "--foreground" => foreground = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    let raw_server = server.ok_or("--server is required")?;
    let host_port = parse_http_server_url(&raw_server)?;
    let bind_id_req = bind_id_opt
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or(
            "--bind-id is required; generate a one-time pairing invitation in the WebUI first",
        )?;
    let default_host = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "macOS-Device".to_owned());
    let device_label = label.unwrap_or_else(|| format!("{default_host} (macOS)"));
    let identity_path = default_identity_path(identity_opt);

    // 1. Load existing local RSA keypair if re-binding same identity, or generate a fresh one locally.
    // The private key never leaves this Device process.
    let device_keypair = if identity_path.exists()
        && let Ok(raw) = std::fs::read_to_string(&identity_path)
        && let Ok(existing) = serde_json::from_str::<PersistentRsaDeviceBinding>(&raw)
    {
        existing.device_keypair
    } else {
        RsaKeyPair::generate()?
    };
    let device_pub = device_keypair.public_key();
    let resolved_ws = workspace_opt
        .or_else(|| std::env::var("CAYW_WORKSPACE").ok())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| ".".to_owned())
        });

    // 2. Phase 1: Send only `device_public_key` to `/api/bind/hello` and receive an RSA-encrypted challenge.
    let hello_payload = json!({
        "bind_id": bind_id_req,
        "label": device_label,
        "device_address": format!("{default_host} · {resolved_ws}"),
        "device_public_key": device_pub,
    });
    let hello_res = http_post_json(&host_port, "/api/bind/hello", &hello_payload).await?;
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
    let confirm_res = http_post_json(&host_port, "/api/bind/confirm", &confirm_payload).await?;
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
    local_binding.save(&identity_path)?;

    // 6. Send an initial signed heartbeat with fresh timestamp and monotonic sequence 1.
    let ts_str = now_ms.to_string();
    let seq = 1u64;
    let hb_sig =
        device_keypair.sign_sha256(format!("heartbeat:{device_id}:{ts_str}:{seq}").as_bytes())?;
    let _ = http_post_json(
        &host_port,
        "/api/device/heartbeat",
        &json!({
            "device_id": device_id,
            "timestamp": ts_str,
            "seq": seq,
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
    println!(
        "  私钥仅保存在本设备 {}，未传输给 Server。",
        identity_path.display()
    );

    let db_path = db_opt.unwrap_or_else(|| {
        let parent = identity_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        parent
            .join(format!("device-runtime-{device_id}.db"))
            .display()
            .to_string()
    });
    if foreground {
        return run_device_daemon(
            local_binding,
            PathBuf::from(&resolved_ws),
            PathBuf::from(&db_path),
        )
        .await;
    }
    if !no_daemon && let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .arg("device")
            .arg("--identity")
            .arg(&identity_path)
            .arg("--workspace")
            .arg(&resolved_ws)
            .arg("--db")
            .arg(&db_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        // Give the background resident device daemon a brief moment to attach.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    Ok(())
}

async fn run_device(mut arguments: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    use cayw_remote::PersistentRsaDeviceBinding;

    let mut identity_opt = None;
    let mut server_opt = None;
    let mut workspace_opt = None;
    let mut db_opt = None;
    while let Some(flag) = arguments.next() {
        match flag.as_str() {
            "--identity" => {
                identity_opt = Some(arguments.next().ok_or("missing --identity value")?)
            }
            "--server" => server_opt = Some(arguments.next().ok_or("missing --server value")?),
            "--workspace" => {
                workspace_opt = Some(arguments.next().ok_or("missing --workspace value")?)
            }
            "--db" => db_opt = Some(arguments.next().ok_or("missing --db value")?),
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    let identity_path = default_identity_path(identity_opt);
    let raw = std::fs::read_to_string(&identity_path).map_err(|_| {
        format!(
            "missing saved device RSA binding at {}; run `cayw bind` first",
            identity_path.display()
        )
    })?;
    let mut binding: PersistentRsaDeviceBinding = serde_json::from_str(&raw)?;
    if let Some(srv) = server_opt {
        let hp = parse_http_server_url(&srv)?;
        binding.server_url = format!("http://{hp}");
    }
    let workspace = PathBuf::from(
        workspace_opt
            .or_else(|| std::env::var("CAYW_WORKSPACE").ok())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| ".".to_owned())
            }),
    );
    let db = PathBuf::from(db_opt.unwrap_or_else(|| {
        let parent = identity_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        parent
            .join(format!("device-runtime-{}.db", binding.device_id))
            .display()
            .to_string()
    }));
    run_device_daemon(binding, workspace, db).await
}

async fn run_device_daemon(
    binding: cayw_remote::PersistentRsaDeviceBinding,
    workspace: PathBuf,
    db: PathBuf,
) -> Result<(), Box<dyn Error>> {
    use cayw_ipc::RpcHandler;
    use cayw_remote::RsaSealedFrame;
    use std::sync::atomic::{AtomicU64, Ordering};

    let host_port = parse_http_server_url(&binding.server_url)?;
    let _ = std::fs::create_dir_all(&workspace);
    if let Some(parent) = db.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut config = cayw_runtime::RuntimeConfig::default();
    config.permissions.extend([
        ScopedPermission::exact(
            "fs.write",
            json!({"path":"cayw-demo.txt","content":"headless runtime demo"}),
        ),
        ScopedPermission::exact("fs.read", json!({"path":"cayw-demo.txt"})),
    ]);
    let runtime = Arc::new(Runtime::open(
        &db,
        &workspace,
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "offline-runtime"),
        config,
    )?);
    runtime.grant_extra_permission(ScopedPermission::exact(
        "fs.read",
        json!({"path":"device-marker.txt"}),
    ));
    runtime.grant_extra_permission(ScopedPermission::exact(
        "plan.update",
        json!({
            "expected_version": 0,
            "steps": [
                {"id": "step-1", "text": "分析任务需求与工作区上下文", "status": "completed"},
                {"id": "step-2", "text": "调用受控 CodeMode 能力执行文件读写", "status": "in_progress"},
                {"id": "step-3", "text": "核验输出并完成任务总结", "status": "pending"}
            ]
        }),
    ));
    runtime.grant_extra_permission(ScopedPermission::exact(
        "plan.update",
        json!({
            "expected_version": 0,
            "steps": [
                {"id": "step-1", "text": "分析任务需求与工作区上下文", "status": "completed"},
                {"id": "step-2", "text": "调用受控 CodeMode 能力读取目标设备标识文件", "status": "in_progress"},
                {"id": "step-3", "text": "核验输出并完成任务总结", "status": "pending"}
            ]
        }),
    ));

    let seq = Arc::new(AtomicU64::new(10));
    let hb_binding = binding.clone();
    let hb_host = host_port.clone();
    let hb_seq = seq.clone();
    tokio::spawn(async move {
        loop {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let next_seq = hb_seq.fetch_add(1, Ordering::SeqCst);
            let ts_str = now_ms.to_string();
            if let Ok(sig) = hb_binding.device_keypair.sign_sha256(
                format!("heartbeat:{}:{ts_str}:{next_seq}", hb_binding.device_id).as_bytes(),
            ) {
                let _ = http_post_json(
                    &hb_host,
                    "/api/device/heartbeat",
                    &json!({
                        "device_id": hb_binding.device_id,
                        "timestamp": ts_str,
                        "seq": next_seq,
                        "signature": sig,
                    }),
                )
                .await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        }
    });

    loop {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let next_seq = seq.fetch_add(1, Ordering::SeqCst);
        let ts_str = now_ms.to_string();
        let Ok(poll_sig) = binding
            .device_keypair
            .sign_sha256(format!("poll:{}:{ts_str}:{next_seq}", binding.device_id).as_bytes())
        else {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            continue;
        };
        let poll_res = http_post_json(
            &host_port,
            "/api/device/poll",
            &json!({
                "device_id": binding.device_id,
                "timestamp": ts_str,
                "seq": next_seq,
                "signature": poll_sig,
                "wait_ms": 5000,
            }),
        )
        .await;
        let Ok(resp) = poll_res else {
            // Automatic reconnect backoff on network or server restart
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            continue;
        };
        let Some(frame_val) = resp
            .get("result")
            .and_then(|r| r.get("frame"))
            .filter(|v| !v.is_null())
        else {
            continue;
        };
        let Ok(frame) = serde_json::from_value::<RsaSealedFrame>(frame_val.clone()) else {
            continue;
        };
        let Ok(plain) = binding.open_from_server(&frame) else {
            continue;
        };
        let Ok(req_json) = serde_json::from_slice::<serde_json::Value>(&plain) else {
            continue;
        };
        let Some(request_id) = req_json
            .get("request_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(method) = req_json
            .get("method")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let params = req_json.get("params").cloned().unwrap_or_else(|| json!({}));

        let rt = runtime.clone();
        let resp_binding = binding.clone();
        let resp_host = host_port.clone();
        tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let call_res = RpcHandler::call(rt.as_ref(), &method, params, cancel).await;
            let reply_payload = match call_res {
                Ok(val) => json!({
                    "request_id": request_id,
                    "result": val,
                }),
                Err(err) => json!({
                    "request_id": request_id,
                    "error": {
                        "code": err.code,
                        "message": err.message,
                        "outcome_unknown": err.code == "needs_reconciliation" || err.code.ends_with("outcome_unknown"),
                    }
                }),
            };
            if let Ok(bytes) = serde_json::to_vec(&reply_payload)
                && let Ok(sealed) = resp_binding.seal_for_server(&bytes)
            {
                let _ = http_post_json(
                    &resp_host,
                    "/api/device/respond",
                    &json!({ "frame": sealed }),
                )
                .await;
            }
        });
    }
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
