//! Offline Provider responses; persistent Runtime and authenticated TCP are real.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Model, ModelRequest,
    Provider,
};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{Runtime, RuntimeConfig};
use serde_json::json;
use std::sync::Arc;
use tokio::net::TcpListener;

const TOKEN: &str = "synthetic-recovery-control-token";

struct Offline;
impl Provider for Offline {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: AssistantMessage::new(&request.model),
        }]))
    }
}

#[tokio::test]
async fn ipc_cancel_without_original_task_identity_cannot_cancel_replacement() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let runtime = Runtime::open(
        root.path().join("session.sqlite"),
        workspace,
        Arc::new(Offline),
        Model::new("fixture", "recovery-controls"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let session = runtime.create(None).unwrap().id;
    let original = runtime.enqueue(&session, "original explicit task").unwrap();
    runtime
        .resume(&original.id, CancellationToken::new())
        .await
        .unwrap();
    let original = runtime.task(&original.id).unwrap();
    let replacement = runtime.enqueue(&session, "replacement task").unwrap();
    let before = runtime.get(&session).unwrap();
    let audit_before = runtime.audit(&session, 0, 1000).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(runtime.clone()),
        ServerConfig::new(TOKEN),
        stop.clone(),
    ));
    let client = Client::connect(address, TOKEN).await.unwrap();

    // This delayed command names only the Session that both tasks share.
    let response = client
        .request("session.cancel", json!({"session_id":session}))
        .await;
    client.close().await;
    stop.cancel();
    server.await.unwrap().unwrap();

    let error = response.expect_err("cancellation must require the original execution identity");
    assert_eq!(error.code, "invalid_params");
    assert_eq!(runtime.task(&replacement.id).unwrap(), replacement);
    assert_eq!(runtime.task(&original.id).unwrap(), original);
    assert_eq!(runtime.get(&session).unwrap(), before);
    assert_eq!(
        serde_json::to_value(runtime.audit(&session, 0, 1000).unwrap()).unwrap(),
        serde_json::to_value(audit_before).unwrap()
    );
}
