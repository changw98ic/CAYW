use cayw_ai::{CancellationToken, Model};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, fixture::OfflineProvider};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn authenticated_ipc_and_sdk_observe_the_same_persistent_run() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config = RuntimeConfig {
        permissions: vec![
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"cayw-demo.txt","content":"headless runtime demo"}),
            ),
            ScopedPermission::exact("fs.read", json!({"path":"cayw-demo.txt"})),
        ],
        ..Default::default()
    };
    let open = || {
        Runtime::open(
            directory.path().join("sessions.sqlite"),
            &workspace,
            Arc::new(OfflineProvider::default()),
            Model::new("fixture", "integrated-ipc"),
            config.clone(),
        )
        .unwrap()
    };
    let runtime = open();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(runtime.clone()),
        ServerConfig::new("integrated-ipc-fixture-token"),
        shutdown.clone(),
    ));
    let client = Client::connect(address, "integrated-ipc-fixture-token")
        .await
        .unwrap();
    let session = client.request("session.create", json!({})).await.unwrap();
    let id = session["id"].as_str().unwrap();
    let finished = client
        .request(
            "session.prompt",
            json!({"session_id":id,"text":"write, read, and inspect this run"}),
        )
        .await
        .unwrap();
    assert_eq!(
        finished,
        serde_json::to_value(runtime.get(id).unwrap()).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("cayw-demo.txt")).unwrap(),
        "headless runtime demo"
    );
    let tasks = serde_json::to_value(runtime.tasks(id).unwrap()).unwrap();
    assert_eq!(
        client
            .request("session.tasks", json!({"session_id":id}))
            .await
            .unwrap(),
        tasks
    );
    assert_eq!(tasks.as_array().unwrap().len(), 1);
    assert_eq!(tasks[0]["state"], "completed");
    assert!(tasks[0]["worker_id"].is_string());
    assert!(tasks[0]["attempt_id"].is_string());
    assert!(tasks[0]["fence"].as_u64().unwrap() > 0);
    let overview = client
        .request("session.context_overview", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(
        overview,
        serde_json::to_value(runtime.session_context_overview(id).unwrap()).unwrap()
    );
    assert!(overview["tokens"]["total_tokens"].as_u64().unwrap() > 0);
    assert_eq!(overview["cost"]["status"], "unknown");
    assert!(overview["cost"]["amount"].is_null());
    let inspector = client
        .request("session.run_inspector", json!({"session_id":id}))
        .await
        .unwrap();
    assert!(
        !inspector["snapshot"]["steps"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(inspector["has_more"], false);

    let events = runtime.events(id, 0, 1000).unwrap();
    let prefix = client
        .request(
            "session.events",
            json!({"session_id":id,"after":0,"limit":3}),
        )
        .await
        .unwrap();
    let cursor = prefix[2]["cursor"].as_u64().unwrap();
    let session_id = id.to_owned();
    client.close().await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(runtime);

    let runtime = open();
    assert_eq!(
        serde_json::to_value(runtime.get(&session_id).unwrap()).unwrap(),
        finished
    );
    assert_eq!(
        serde_json::to_value(runtime.tasks(&session_id).unwrap()).unwrap(),
        tasks
    );
    assert_eq!(runtime.events(&session_id, 0, 1000).unwrap(), events);
    assert_eq!(
        serde_json::to_value(runtime.session_context_overview(&session_id).unwrap()).unwrap(),
        overview
    );
    let mut resumed: Vec<Value> = prefix.as_array().unwrap().clone();
    resumed.extend(
        runtime
            .events(&session_id, cursor, 1000)
            .unwrap()
            .iter()
            .map(|event| serde_json::to_value(event).unwrap()),
    );
    assert_eq!(serde_json::to_value(events).unwrap(), json!(resumed));
}
