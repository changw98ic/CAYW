//! Standalone outbound Device demo. Its only operation is JSON echo/wait.
use async_trait::async_trait;
use cayw_remote::{
    CancellationToken, DeviceCredential, DeviceHandler, PendingBind, RemoteDevice, RemoteError,
    Secret, ServerUrl,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{env, fs::OpenOptions, io::Write, path::Path, sync::Arc};

#[derive(Serialize, Deserialize)]
struct SavedBind {
    bind_id: String,
    code: String,
    secret: String,
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), RemoteError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| RemoteError::Storage)?;
    file.write_all(bytes).map_err(|_| RemoteError::Storage)?;
    file.sync_all().map_err(|_| RemoteError::Storage)
}
struct Echo;
#[async_trait]
impl DeviceHandler for Echo {
    async fn handle(
        &self,
        request: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RemoteError> {
        if request.get("wait_for_cancel").and_then(Value::as_bool) == Some(true) {
            cancel.cancelled().await;
            return Err(RemoteError::Cancelled);
        }
        Ok(json!({"device_reply":request}))
    }
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), RemoteError> {
    let tls_terminated = env::var("CAYW_REMOTE_TLS_TERMINATED").ok().as_deref() == Some("1");
    let url = ServerUrl::new(
        &env::var("CAYW_REMOTE_SERVER_URL").unwrap_or_else(|_| "ws://127.0.0.1:8787".into()),
        tls_terminated,
    )?;
    let pending_path =
        env::var("CAYW_REMOTE_PENDING_FILE").unwrap_or_else(|_| "device-pending.json".into());
    let pending_path = Path::new(&pending_path);
    let pending = if pending_path.exists() {
        let saved: SavedBind =
            serde_json::from_slice(&std::fs::read(pending_path).map_err(|_| RemoteError::Storage)?)
                .map_err(|_| RemoteError::Storage)?;
        PendingBind {
            bind_id: saved.bind_id,
            code: Secret::new(saved.code)?,
            secret: Secret::new(saved.secret)?,
        }
    } else {
        let code = Secret::new(
            env::var("CAYW_REMOTE_PAIR_CODE").map_err(|_| RemoteError::Configuration)?,
        )?;
        let pending = PendingBind::new(code)?;
        let saved = SavedBind {
            bind_id: pending.bind_id.clone(),
            code: pending.code.expose().into(),
            secret: pending.secret.expose().into(),
        };
        write_private(
            pending_path,
            &serde_json::to_vec(&saved).map_err(|_| RemoteError::Storage)?,
        )?;
        pending
    };
    let id_path = pending_path.with_extension("id");
    let device_id = match env::var("CAYW_REMOTE_DEVICE_ID") {
        Ok(id) => Some(id),
        Err(_) => match std::fs::read_to_string(&id_path) {
            Ok(id) => Some(id),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(RemoteError::Storage),
        },
    };
    let credential: DeviceCredential = match device_id {
        Some(device_id) => DeviceCredential {
            device_id,
            secret: pending.secret.clone(),
        },
        None => pending.complete(&url).await?,
    };
    println!("device_id={}", credential.device_id);
    // Retain pending state for response-loss recovery; later starts use the
    // confirmed non-secret identity without repeating the expired handshake.
    match std::fs::read_to_string(&id_path) {
        Ok(old) if old == credential.device_id => {}
        Ok(_) => return Err(RemoteError::Conflict),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_private(&id_path, credential.device_id.as_bytes())?
        }
        Err(_) => return Err(RemoteError::Storage),
    }
    let stop = CancellationToken::new();
    let signal = stop.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });
    RemoteDevice::run(&url, &credential, Arc::new(Echo), stop).await
}
