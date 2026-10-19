use async_trait::async_trait;
use cayw_ipc::{RpcError, RpcHandler, ServerConfig, serve};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct FixtureEcho;
#[async_trait]
impl RpcHandler for FixtureEcho {
    async fn call(
        &self,
        method: &str,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        if method == "fixtureEcho" {
            Ok(params)
        } else {
            Err(RpcError::new("method_not_found", "unknown fixture method"))
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("CAYW_IPC_TOKEN")?;
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:0".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("{}", listener.local_addr()?);
    serve(
        listener,
        Arc::new(FixtureEcho),
        ServerConfig::new(token),
        CancellationToken::new(),
    )
    .await?;
    Ok(())
}
