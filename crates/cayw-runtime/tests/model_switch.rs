//! Synthetic providers, real Runtime/Agent/Cells/accounting. No network or keys.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_runtime::{ConfiguredProvider, Runtime, RuntimeConfig};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

struct Script {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    pause: AtomicBool,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "return 'model switch fixture'",
            "switch-call",
        );
        self.requests.lock().unwrap().push(request);
        let pause = self.pause.swap(false, Ordering::AcqRel);
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async_stream::stream! {
            if pause { entered.notify_one(); release.notified().await; }
            yield AssistantEvent::Done { message };
        })
    }
}
fn provider(pause: bool) -> Arc<Script> {
    Arc::new(Script {
        requests: Default::default(),
        entered: Default::default(),
        release: Default::default(),
        pause: AtomicBool::new(pause),
    })
}
#[tokio::test]
async fn switching_keeps_active_attempt_provider_model_limits_and_ledger_identity() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let old = provider(true);
    let new = provider(false);
    let mut old_model = Model::new("fixture-old", "old");
    old_model.max_output_tokens = Some(128);
    let runtime = Runtime::open(
        temp.path().join("sessions.db"),
        workspace,
        old.clone(),
        old_model,
        RuntimeConfig::default(),
    )
    .unwrap();
    let first = runtime.create(None).unwrap().id;
    let second = runtime.create(None).unwrap().id;
    let rt = runtime.clone();
    let id = first.clone();
    let running =
        tokio::spawn(async move { rt.prompt(&id, "run old", CancellationToken::new()).await });
    tokio::time::timeout(std::time::Duration::from_secs(30), old.entered.notified())
        .await
        .unwrap();
    let mut new_model = Model::new("fixture-new", "new");
    new_model.max_output_tokens = Some(256);
    runtime
        .select_model(ConfiguredProvider {
            provider: new.clone(),
            model: new_model,
        })
        .unwrap();
    assert_eq!(runtime.health().unwrap().model, "new");
    old.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(30), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        old.requests.lock().unwrap().len() >= 2,
        "both tool turn and completion use the original provider"
    );
    assert!(new.requests.lock().unwrap().is_empty());
    for request in old.requests.lock().unwrap().iter() {
        assert_eq!(request.model.id, "old");
        assert_eq!(request.model.max_output_tokens, Some(128));
    }
    let ledger = runtime.model_requests(&first).unwrap();
    assert!(!ledger.requests.is_empty());
    for request in ledger.requests {
        assert_eq!(request.identity.model, "old");
        assert_eq!(request.identity.provider, "fixture-old");
    }
    runtime
        .prompt(&second, "run new", CancellationToken::new())
        .await
        .unwrap();
    assert!(new.requests.lock().unwrap().len() >= 2);
    for request in new.requests.lock().unwrap().iter() {
        assert_eq!(request.model.id, "new");
        assert_eq!(request.model.max_output_tokens, Some(256));
    }
    for request in runtime.model_requests(&second).unwrap().requests {
        assert_eq!(request.identity.model, "new");
    }
}
