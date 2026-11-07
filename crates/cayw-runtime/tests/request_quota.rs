//! C19 regressions at the Runtime boundary: ordinary zero-request Chat must not
//! consume an irrevocable lifetime allowance, and a trusted host can adjust the
//! persistent operational limits without resetting audit facts.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_requests::{HostActor, OperationScope, RequestConfig};
use cayw_runtime::{RequestPolicy, Runtime, RuntimeConfig};
use serde_json::json;
use std::{sync::Arc, time::Duration};

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let call = format!("request-{}", request.messages.len());
        let message = cayw_runtime::fixture::script_response(&request, &self.0, &call);
        Box::pin(async_stream::stream! { yield AssistantEvent::Done { message }; })
    }
}

fn settings(limits: RequestConfig) -> RuntimeConfig {
    RuntimeConfig {
        max_run_time: Duration::from_secs(15),
        request_policy: Some(RequestPolicy {
            permission_policy_id: "runtime-quota-policy".into(),
            operations: vec![OperationScope::exact(
                "write-result",
                "fs.write",
                json!({"path":"result.txt","content":"approved"}),
                "workspace-policy",
            )],
        }),
        request_limits: limits,
        ..Default::default()
    }
}

fn open(temp: &tempfile::TempDir, settings: RuntimeConfig) -> Runtime {
    Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        Arc::new(Script("return 1".into())),
        Model::new("fixture", "request-quota"),
        settings,
    )
    .unwrap()
}

fn setup() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    temp
}

/// Five completed tasks under `max_tasks = 2` and a two-MiB operational state
/// allowance. A lifetime counter stops at the third; released operational
/// reservations keep admitting ordinary work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finished_zero_request_tasks_release_runtime_operational_quota() {
    let temp = setup();
    let limits = RequestConfig {
        max_tasks: 2,
        max_state_bytes: 2 * 1024 * 1024,
        ..RequestConfig::default()
    };
    let rt = open(&temp, settings(limits));
    let session = rt.create(None).unwrap().id;
    for index in 0..5 {
        let text = format!("ordinary chat {index}");
        let result = rt
            .prompt(&session, &text, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.state, cayw_core::SessionState::Idle);
    }
    assert_eq!(rt.tasks(&session).unwrap().len(), 5);
    assert!(
        rt.tasks(&session)
            .unwrap()
            .iter()
            .all(|task| task.state == cayw_runtime::TaskState::Completed)
    );
}

/// A trusted live adjustment changes the operational allowance, persists it
/// with history, and the next ordinary task uses the new value. The separate
/// offline helper applies the same durable change when no Runtime is open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_runtime_limit_adjustment_persists_without_clearing_audit() {
    let temp = setup();
    let limits = RequestConfig {
        max_tasks: 2,
        max_state_bytes: 2 * 1024 * 1024,
        ..RequestConfig::default()
    };
    let rt = open(&temp, settings(limits.clone()));
    let session = rt.create(None).unwrap().id;
    let _ = rt
        .prompt(&session, "first ordinary chat", CancellationToken::new())
        .await
        .unwrap();
    let adjusted = RequestConfig {
        max_tasks: 1,
        ..limits.clone()
    };
    rt.request_limits_adjust(
        adjusted.clone(),
        "trusted-operator",
        "one active task at a time",
    )
    .unwrap();
    assert_eq!(rt.request_limits(), adjusted);
    let history = rt.request_config_history().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].previous, limits);
    assert_eq!(history[0].next, adjusted);
    // A new task still fits because the finished task released its allowance.
    let result = rt
        .prompt(&session, "second ordinary chat", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.state, cayw_core::SessionState::Idle);
    drop(rt);

    assert_eq!(
        Runtime::persisted_request_limits(temp.path().join("session.db")).unwrap(),
        adjusted
    );
    let offline = RequestConfig {
        max_tasks: 3,
        ..adjusted.clone()
    };
    let applied = Runtime::adjust_request_limits(
        temp.path().join("session.db"),
        offline.clone(),
        &HostActor::new("trusted-operator").unwrap(),
        "raise the operational allowance after review",
    )
    .unwrap();
    assert_eq!(applied, offline);
    // Reopening with the persisted value succeeds and retains both history rows.
    let reopened = open(&temp, settings(offline.clone()));
    assert_eq!(reopened.request_limits(), offline);
    assert_eq!(reopened.request_config_history().unwrap().len(), 2);
}
