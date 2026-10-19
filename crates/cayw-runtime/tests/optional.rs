use cayw_ai::{CancellationToken, Model};
use cayw_runtime::{Runtime, RuntimeConfig, fixture::OfflineProvider};
use std::sync::Arc;

fn runtime(dir: &tempfile::TempDir) -> Runtime {
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();
    Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "optional"),
        RuntimeConfig::default(),
    )
    .unwrap()
}
#[cfg(feature = "remote")]
#[tokio::test]
async fn remote_device_forwards_versioned_calls_into_the_same_rpc_handler() {
    use cayw_remote::DeviceHandler;
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(&dir);
    let health = rt
        .handle(
            json!({"version":cayw_core::PROTOCOL_VERSION,"id":"health","method":"runtime.health","params":{}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(health["result"]["status"], "ok");
    let created = rt.handle(json!({"version":cayw_core::PROTOCOL_VERSION,"id":"create","method":"session.create","params":{"label":"remote"}}),CancellationToken::new()).await.unwrap();
    let id = created["result"]["id"].as_str().unwrap();
    assert_eq!(rt.get(id).unwrap().label, "remote");
    assert!(
        rt.handle(
            json!({"version":cayw_core::PROTOCOL_VERSION + 1,"id":"wrong","method":"runtime.health","params":{}}),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    let unknown = rt
        .handle(
            json!({"version":cayw_core::PROTOCOL_VERSION,"id":"old","method":"health","params":{}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(unknown["error"]["code"], "method_not_found");
}

#[cfg(feature = "strongflow")]
#[tokio::test]
async fn actual_plugin_binding_freezes_on_unload_and_only_confirmed_fork_runs_chat() {
    use cayw_plugins::{
        PluginContext,
        strongflow::{PlanStep, StrongFlow, WorkflowPlan},
    };
    use cayw_runtime::{RuntimeError, ScopedPermission};
    use serde_json::json;
    use std::collections::BTreeSet;
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(&dir);
    let plugin = Arc::new(StrongFlow::new());
    plugin
        .create_plan(WorkflowPlan {
            id: "plan".into(),
            steps: vec![PlanStep {
                id: "step".into(),
                depends_on: BTreeSet::new(),
                acceptance_checks: BTreeSet::from(["real-tests".into()]),
            }],
        })
        .unwrap();
    rt.load_native(
        plugin,
        PluginContext::new(
            ["strongflow.describe".into(), "strongflow.ready".into()],
            [],
        ),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    rt.bind_strongflow(&id).unwrap();
    use cayw_ipc::RpcHandler;
    let before = rt
        .call(
            "session.get",
            json!({"session_id":id}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(before["frozen"], false);
    assert_eq!(before["binding"]["plugin"], "strongflow");
    rt.unload_plugin("strongflow").unwrap();
    let frozen = rt
        .call(
            "session.get",
            json!({"session_id":id}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(frozen["frozen"], true);
    assert_eq!(frozen["binding"]["frozen"], true);
    let list = rt
        .call("session.list", json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        list.as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == id)
            .unwrap()["frozen"],
        true
    );
    assert!(matches!(
        rt.prompt(&id, "blocked", CancellationToken::new()).await,
        Err(RuntimeError::Frozen(_))
    ));
    rt.load_native(Arc::new(StrongFlow::new()), PluginContext::new([], []))
        .unwrap();
    assert!(matches!(
        rt.prompt(&id, "still blocked", CancellationToken::new())
            .await,
        Err(RuntimeError::Frozen(_))
    ));
    assert!(rt.fork(&id, "unconfirmed", false).is_err());
    let fork = rt.fork(&id, "ordinary", true).unwrap();
    assert_ne!(fork.id, id);
    // A frozen source can still be inspected, and stays frozen after a fork.
    assert_eq!(rt.get(&id).unwrap().label, "Chat");
    assert!(rt.enqueue(&id, "source stays frozen").is_err());
    // Plain chat is available with no plugins loaded in a separate runtime.
    drop(rt);
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
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "optional"),
        config,
    )
    .unwrap();
    let result = rt
        .prompt(&fork.id, "ordinary fork", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.state, cayw_core::SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/cayw-demo.txt")).unwrap(),
        "headless runtime demo"
    );
    assert!(rt.enqueue(&id, "frozen after restart").is_err());
}

#[cfg(not(any(feature = "remote", feature = "strongflow")))]
#[tokio::test]
async fn ordinary_chat_opens_without_optional_plugins_or_relay() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(&dir);
    let id = rt.create(None).unwrap().id;
    assert_eq!(rt.get(&id).unwrap().state, cayw_core::SessionState::Idle);
    let token = CancellationToken::new();
    token.cancel();
    assert!(rt.prompt(&id, "cancel before model", token).await.is_err());
}
