use cayw_remote::{
    CancellationToken, OperatorClient, RemoteError, Secret, ServerConfig, ServerState, ServerUrl,
    serve,
};
use serde_json::Value;
use std::{
    env,
    fs::{File, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    path::Path,
    time::Duration,
};
use tokio::net::TcpListener;

fn setting(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.into())
}
fn token() -> Result<Secret, RemoteError> {
    Secret::new(env::var("CAYW_REMOTE_OPERATOR_TOKEN").map_err(|_| RemoteError::Configuration)?)
}
fn read_bounded_payload(path: impl AsRef<Path>, limit: u64) -> Result<Vec<u8>, RemoteError> {
    let file = File::open(path).map_err(|_| RemoteError::Configuration)?;
    let metadata = file.metadata().map_err(|_| RemoteError::Configuration)?;
    if !metadata.is_file() {
        return Err(RemoteError::Configuration);
    }
    if metadata.len() > limit {
        return Err(RemoteError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| RemoteError::Configuration)?;
    if bytes.len() as u64 > limit {
        return Err(RemoteError::TooLarge);
    }
    Ok(bytes)
}
fn output(value: impl serde::Serialize) -> Result<(), RemoteError> {
    println!(
        "{}",
        serde_json::to_string(&value).map_err(|_| RemoteError::Configuration)?
    );
    Ok(())
}
fn save_code(path: String, code: &str) -> Result<(), RemoteError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| RemoteError::Storage)?;
    file.write_all(code.as_bytes())
        .map_err(|_| RemoteError::Storage)?;
    file.sync_all().map_err(|_| RemoteError::Storage)
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), RemoteError> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let command = args.first().map(String::as_str).unwrap_or("serve");
    let tls_terminated = env::var("CAYW_REMOTE_TLS_TERMINATED").ok().as_deref() == Some("1");
    if command == "serve" {
        let listen: SocketAddr = setting("CAYW_REMOTE_LISTEN", "127.0.0.1:8787")
            .parse()
            .map_err(|_| RemoteError::Configuration)?;
        if !listen.ip().is_loopback() && !tls_terminated {
            return Err(RemoteError::TlsRequired);
        }
        // Validate the advertised URL independently of the local listener. It
        // does not open ports, discover addresses, or configure a TLS proxy.
        let public = ServerUrl::new(
            &setting("CAYW_REMOTE_PUBLIC_URL", "ws://127.0.0.1:8787"),
            tls_terminated,
        )?;
        let state = ServerState::open(
            setting("CAYW_REMOTE_DATABASE", "remote.sqlite"),
            token()?,
            ServerConfig {
                tls_terminated,
                allowed_origin: env::var("CAYW_REMOTE_ALLOWED_ORIGIN").ok(),
                ..Default::default()
            },
        )?;
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|_| RemoteError::Configuration)?;
        println!(
            "relay listening at {}; advertised {}",
            listener
                .local_addr()
                .map_err(|_| RemoteError::Configuration)?,
            public.as_str()
        );
        let stop = CancellationToken::new();
        let signal = stop.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            signal.cancel();
        });
        return serve(listener, state, stop).await;
    }
    let url = ServerUrl::new(
        &setting("CAYW_REMOTE_SERVER_URL", "ws://127.0.0.1:8787"),
        tls_terminated,
    )?;
    let operator = OperatorClient::new(url, token()?);
    let device = || {
        args.get(1)
            .map(String::as_str)
            .ok_or(RemoteError::Configuration)
    };
    let id = || {
        args.get(2)
            .map(String::as_str)
            .ok_or(RemoteError::Configuration)
    };
    match command {
        "pair" => {
            let path = env::var("CAYW_REMOTE_PAIR_FILE").map_err(|_| RemoteError::Configuration)?;
            let code = operator.pair(Duration::from_secs(300)).await?;
            save_code(path, code.code.expose())?;
            output(
                serde_json::json!({"pairing_code_saved": true, "expires_at_ms": code.expires_at_ms}),
            )
        }
        "device-status" => output(operator.device_status(device()?).await?),
        "invoke" => {
            let path = args.get(3).ok_or(RemoteError::Configuration)?;
            let bytes = read_bounded_payload(path, 64 * 1024)?;
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| RemoteError::InvalidRequest)?;
            output(operator.invoke(device()?, id()?, value).await?)
        }
        "status" => output(operator.status(device()?, id()?).await?),
        "cancel" => output(operator.cancel(device()?, id()?).await?),
        "revoke" => {
            operator.revoke(device()?).await?;
            output(serde_json::json!({"revoked":true}))
        }
        _ => Err(RemoteError::Configuration),
    }
}
