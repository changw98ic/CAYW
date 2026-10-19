use cayw_ipc::Client;
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(1)
        .ok_or("usage: ipc_client 127.0.0.1:PORT; set CAYW_IPC_TOKEN")?
        .parse()?;
    let token = std::env::var("CAYW_IPC_TOKEN")?;
    let client = Client::connect(addr, token).await?;
    let result = client
        .request(
            "fixtureEcho",
            json!({"message":"hello from another process"}),
        )
        .await?;
    println!("{}", serde_json::to_string(&result)?);
    client.close().await;
    Ok(())
}
