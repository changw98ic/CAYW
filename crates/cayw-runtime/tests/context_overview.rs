//! Live path: OfflineProvider → durable model_requests ledger → F05 overview.
use cayw_ai::{CancellationToken, Model};
use cayw_context::CostStatus;
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{
    Runtime, RuntimeConfig, ScopedPermission, TaskState, fixture::OfflineProvider,
    session_ledger_snapshot_from_model_requests,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

fn open_runtime(root: &std::path::Path) -> Runtime {
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    Runtime::open(
        root.join("session.db"),
        &workspace,
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "context-overview"),
        RuntimeConfig {
            permissions: vec![
                ScopedPermission::exact(
                    "fs.write",
                    json!({"path":"cayw-demo.txt","content":"headless runtime demo"}),
                ),
                ScopedPermission::exact("fs.read", json!({"path":"cayw-demo.txt"})),
            ],
            ..Default::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn runtime_session_context_overview_reads_live_model_request_ledger() {
    let root = tempfile::tempdir().unwrap();
    let runtime = open_runtime(root.path());
    let session = runtime.create(Some("overview fixture".into())).unwrap();

    tokio::time::timeout(
        Duration::from_secs(20),
        runtime.prompt(
            &session.id,
            "run the offline demo",
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        runtime.tasks(&session.id).unwrap().last().unwrap().state,
        TaskState::Completed
    );

    let ledger = runtime.model_requests(&session.id).unwrap();
    assert!(!ledger.requests.is_empty());
    assert_eq!(ledger.cost_status, cayw_runtime::ModelCostStatus::Unknown);

    let overview = runtime.session_context_overview(&session.id).unwrap();
    assert_eq!(overview.session_id, session.id);
    assert_eq!(overview.budget_session_id, ledger.budget_session_id);
    assert_eq!(overview.label.as_deref(), Some("overview fixture"));
    assert!(overview.request_count >= 1);
    assert_eq!(
        overview.tokens.input,
        ledger
            .requests
            .iter()
            .map(|r| r
                .observations
                .last()
                .map(|o| o.observation.usage.input)
                .unwrap_or(0))
            .sum::<u64>()
    );
    assert_eq!(
        overview.tokens.total_tokens,
        overview.tokens.input
            + overview.tokens.output
            + overview.tokens.cache_read
            + overview.tokens.cache_write
    );
    assert_eq!(overview.ledger_settled_tokens, ledger.settled_tokens);
    assert_eq!(overview.ledger_reserved_tokens, ledger.reserved_tokens);
    assert_eq!(overview.ledger_remaining_tokens, ledger.remaining_tokens);
    assert_eq!(overview.budget_unknown, ledger.budget_unknown);
    assert_eq!(overview.cost.status, CostStatus::Unknown);
    assert!(overview.cost.amount.is_none());
    assert!(overview.cost.currency.is_none());

    let adapted = session_ledger_snapshot_from_model_requests(
        &ledger,
        &session.id,
        None,
        Some("overview fixture".into()),
    );
    assert_eq!(
        runtime
            .session_ledger_snapshot(&session.id)
            .unwrap()
            .requests
            .len(),
        adapted.requests.len()
    );
}

#[tokio::test]
async fn ipc_session_context_overview_returns_live_overview() {
    let root = tempfile::tempdir().unwrap();
    let runtime = open_runtime(root.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(runtime.clone()),
        ServerConfig::new("context-overview-ipc-secret-unique"),
        shutdown.clone(),
    ));

    let client = Client::connect(address, "context-overview-ipc-secret-unique")
        .await
        .unwrap();
    let session = client
        .request("session.create", json!({"label":"ipc overview"}))
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap();
    client
        .request(
            "session.prompt",
            json!({"session_id":id,"text":"run the offline demo"}),
        )
        .await
        .unwrap();

    let overview = client
        .request("session.context_overview", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(overview["session_id"], id);
    assert_eq!(overview["label"], "ipc overview");
    assert!(overview["request_count"].as_u64().unwrap() >= 1);
    assert_eq!(overview["cost"]["status"], "unknown");
    assert!(overview["cost"].get("amount").is_none());
    assert!(overview["tokens"]["total_tokens"].as_u64().unwrap() >= 8);

    let direct = runtime.session_context_overview(id).unwrap();
    assert_eq!(
        direct.tokens.total_tokens,
        overview["tokens"]["total_tokens"].as_u64().unwrap()
    );

    shutdown.cancel();
    let _ = server.await;
}
