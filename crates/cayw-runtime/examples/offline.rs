use cayw_ai::{CancellationToken, Model};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, fixture::OfflineProvider};
use serde_json::json;
use std::{error::Error, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let directory = std::env::args()
        .nth(1)
        .ok_or("usage: offline ABSOLUTE_DEMO_DIRECTORY")?;
    let root = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(root.join("workspace"))?;
    let settings = RuntimeConfig {
        permissions: vec![
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"cayw-demo.txt","content":"headless runtime demo"}),
            ),
            ScopedPermission::exact("fs.read", json!({"path":"cayw-demo.txt"})),
        ],
        ..Default::default()
    };
    let provider = Arc::new(OfflineProvider::default());
    let rt = Runtime::open(
        root.join("sessions.db"),
        root.join("workspace"),
        provider.clone(),
        Model::new("fixture", "offline-runtime"),
        settings.clone(),
    )?;
    let session = rt.create(Some("Explicitly authorized offline demo".into()))?;
    let answer = rt
        .prompt(
            &session.id,
            "Write the demo file and read it back",
            CancellationToken::new(),
        )
        .await?;
    println!(
        "{}",
        json!({"provider":"offline fixture","session_id":session.id,"snapshot":answer,"events":rt.events(&session.id,0,1000)?,"audit":rt.audit(&session.id,0,1000)?})
    );
    assert_eq!(
        std::fs::read_to_string(root.join("workspace/cayw-demo.txt"))?,
        "headless runtime demo"
    );
    drop(rt);
    let reopened = Runtime::open(
        root.join("sessions.db"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "offline-runtime"),
        settings,
    )?;
    println!(
        "reopened session {} at cursor {}",
        session.id,
        reopened.get(&session.id)?.cursor
    );
    Ok(())
}
