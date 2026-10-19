//! Real Runtime + Core + Host over loopback IPC with deterministic offline model input.
//! Temporary database/workspace only; no model network or persistent user session.
#[path = "../tests/support/mod.rs"]
mod support;

use cayw_ipc::{ServerConfig, serve};
use serde_json::json;
use std::{error::Error, sync::Arc};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // A controlled Provider gate lets terminal verification attach before the
    // assistant finishes. Releasing it changes model input only, never Core state.
    let gate = std::env::var_os("CAYW_FIXTURE_STREAM_RELEASE").map(|path| {
        let gate = CancellationToken::new();
        let release = gate.clone();
        let task = tokio::spawn(async move {
            while !std::path::Path::new(&path).exists() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            release.cancel();
        });
        (gate, task)
    });
    let fixture = match &gate {
        Some((gate, _)) => support::OfflineRuntime::with_stream_gate(Some(gate.clone())),
        None => support::OfflineRuntime::new(),
    };
    let listener =
        TcpListener::bind(std::env::var("CAYW_IPC_ADDR").unwrap_or_else(|_| "127.0.0.1:0".into()))
            .await?;
    let token = std::env::var("CAYW_IPC_TOKEN")?;
    if let Ok(path) = std::env::var("CAYW_FIXTURE_INFO") {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&json!({
                "kind":"real Runtime/Core/Host, offline Provider",
                "database":fixture.directory.path().join("runtime.db"),
                "workspace":fixture.directory.path(),
                "frozen_id":fixture.frozen_id,
                "frozen_view":fixture.runtime.get(&fixture.frozen_id)?,
            }))?,
        )?;
    }
    println!("{}", listener.local_addr()?);
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let signal_task = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });
    let result = serve(
        listener,
        Arc::new(fixture.runtime),
        ServerConfig::new(token),
        stop,
    )
    .await;
    signal_task.abort();
    let _ = signal_task.await;
    if let Some((_, task)) = gate {
        task.abort();
        let _ = task.await;
    }
    result.map_err(Into::into)
}
