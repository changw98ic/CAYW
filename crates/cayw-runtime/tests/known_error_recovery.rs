use async_trait::async_trait;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_codemode::CapabilityError;
use cayw_core::SessionState;
use cayw_host::ExternalHost;
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission};
use futures::stream;
use serde_json::{Value, json};
use std::sync::Arc;

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(stream::iter(vec![AssistantEvent::Done {
            message: cayw_runtime::fixture::script_response(&request, self.0, "known-error"),
        }]))
    }
}
struct EmptyFailure;
#[async_trait]
impl ExternalHost for EmptyFailure {
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Err(CapabilityError::new(""))
    }
}

async fn check(settings: RuntimeConfig, script: &'static str) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let open = || {
        Runtime::open(
            temp.path().join("session.db"),
            &workspace,
            Arc::new(Script(script)),
            Model::new("fixture", "known-error"),
            settings.clone(),
        )
        .unwrap()
    };
    let rt = open();
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "known failure", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_capabilities.is_empty());
    let calls = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            cayw_core::CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    match calls[0].outcome.as_ref().unwrap() {
        cayw_core::CapabilityOutcome::Failed { error } => assert_eq!(error, "capability failed"),
        cayw_core::CapabilityOutcome::NotExecuted { reason } => assert!(!reason.is_empty()),
        outcome => panic!("expected a known failure: {outcome:?}"),
    }
    assert_eq!(rt.recover(&id).unwrap().state, SessionState::Idle);
    let cursor = rt.get(&id).unwrap().cursor;
    assert_eq!(rt.recover(&id).unwrap().state, SessionState::Idle);
    assert!(rt.get(&id).unwrap().cursor >= cursor);
    drop(rt);
    let reopened = open();
    assert_eq!(reopened.recover(&id).unwrap().state, SessionState::Idle);
    assert!(reopened.enqueue(&id, "explicit next task").is_ok());
}

#[tokio::test]
async fn empty_external_error_recovers_without_freezing_known_result() {
    let mut settings = RuntimeConfig::default();
    settings.host.external = Some(Arc::new(EmptyFailure));
    settings.permissions.push(ScopedPermission::exact(
        "search.query",
        json!({"query":"fixture"}),
    ));
    check(
        settings,
        "return cayw.call('search.query', {query='fixture'})",
    )
    .await;
}

#[tokio::test]
async fn minimal_host_error_budget_recovers_without_freezing_known_nonexecution() {
    let mut settings = RuntimeConfig::default();
    settings.host.max_total_bytes = 2;
    settings.permissions.push(ScopedPermission::exact(
        "fs.read",
        json!({"path":"missing"}),
    ));
    check(settings, "return cayw.call('fs.read', {path='missing'})").await;
}
