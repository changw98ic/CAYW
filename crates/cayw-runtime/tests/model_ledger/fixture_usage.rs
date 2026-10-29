use cayw_ai::{CancellationToken, Message, Model, UsageStatus};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState, fixture::OfflineProvider};
use serde_json::json;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn offline_demo_reports_explicit_synthetic_usage_and_completes_two_user_rounds() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let runtime = Runtime::open(
        root.path().join("session.db"),
        &workspace,
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "offline-runtime"),
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
    .unwrap();
    let session = runtime.create(None).unwrap();
    for prompt in ["run the offline demo", "run another explicit offline demo"] {
        let view = tokio::time::timeout(
            Duration::from_secs(20),
            runtime.prompt(&session.id, prompt, CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            runtime.tasks(&session.id).unwrap().last().unwrap().state,
            TaskState::Completed,
            "fixed offline usage must not be recorded as an unknown live request"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("cayw-demo.txt")).unwrap(),
            "headless runtime demo"
        );
        assert!(
            view.messages
                .iter()
                .filter_map(|message| match message {
                    Message::Assistant(message) => Some(message),
                    _ => None,
                })
                .all(|message| message.usage_status == UsageStatus::Reported
                    && message.usage.input == 4
                    && message.usage.output == 4
                    && message.usage.total_tokens == 8)
        );
        let ledger = runtime.model_requests(&session.id).unwrap();
        assert!(!ledger.budget_unknown);
        assert_eq!(ledger.reserved_tokens, 0);
        assert!(ledger.cost_amount.is_none());
        assert!(
            ledger
                .requests
                .iter()
                .all(|record| record.settled_tokens == Some(8))
        );
    }
}
