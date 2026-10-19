use cayw_ipc::Client;
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(1)
        .ok_or("missing loopback address")?
        .parse()?;
    let token = std::env::var("CAYW_IPC_TOKEN")?;
    let client = Client::connect(addr, token).await?;
    let result = client
        .request("fixtureEcho", json!({"process":"separate","value":23}))
        .await?;
    client.close().await;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
