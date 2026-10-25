#[path = "../examples/support/mod.rs"]
mod support;
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::*;
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct Policy;
#[async_trait]
impl RegistryPolicy for Policy {
    fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
        true
    }
    async fn authorize(
        &self,
        _: &AccessRequest,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}
fn identity(version: &str) -> SourceIdentity {
    SourceIdentity {
        id: "source".into(),
        version: version.into(),
        origin: "application:test-handle".into(),
    }
}
fn manifest() -> Manifest {
    Manifest {
        tools: vec![ToolSpec {
            name: "echo".into(),
            description: "Strict echo".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            output_schema: Some(
                json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            ),
        }],
        skills: vec![SkillSpec {
            name: "fixed".into(),
            description: "Fixed skill".into(),
            content_sha256: content_sha256("fixed content"),
        }],
        resources: Vec::new(),
        prompts: Vec::new(),
        search: true,
    }
}
#[derive(Default)]
struct State {
    calls: AtomicUsize,
    closed: AtomicUsize,
    entered: Notify,
    release: Notify,
    wait: AtomicBool,
    mismatch: AtomicBool,
    bad_skill: AtomicBool,
    oversize: AtomicBool,
    observed_cancel: Mutex<Option<CancellationToken>>,
}
struct Handle {
    key: String,
    state: Arc<State>,
}
#[async_trait]
impl SourceHandle for Handle {
    fn ownership_key(&self) -> &str {
        &self.key
    }
    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        self.state.calls.fetch_add(1, Ordering::Relaxed);
        *self.state.observed_cancel.lock().unwrap() = Some(cancel);
        self.state.entered.notify_one();
        if self.state.wait.load(Ordering::Acquire) {
            self.state.release.notified().await;
        }
        let mut key = request.key;
        if self.state.mismatch.load(Ordering::Relaxed) {
            key.id += 1;
        }
        let value = match request.operation {
            SourceOperation::InvokeTool { arguments, .. } => {
                if self.state.oversize.load(Ordering::Relaxed) {
                    json!({"text":"x".repeat(request.max_result_bytes+1)})
                } else {
                    arguments
                }
            }
            SourceOperation::ReadSkill { .. } => {
                json!(if self.state.bad_skill.load(Ordering::Relaxed) {
                    "altered content"
                } else {
                    "fixed content"
                })
            }
            SourceOperation::Search { arguments } => json!({"matches":[arguments["query"]]}),
            SourceOperation::GetPrompt { .. } => Value::Null,
            SourceOperation::ReadResource { .. } => {
                return Err(CapabilityError::new("unexpected resource read"));
            }
        };
        Ok(SourceReply { key, value })
    }
    fn disconnect(&self) {
        self.state.closed.fetch_add(1, Ordering::Relaxed);
    }
}
fn handle(key: &str, state: Arc<State>) -> Box<dyn SourceHandle> {
    Box::new(Handle {
        key: key.into(),
        state,
    })
}
fn registry(limits: Limits) -> Registry {
    Registry::new(limits, Arc::new(Policy)).unwrap()
}
async fn call(bridge: &support::FixtureRuntime) -> Result<Value, CapabilityError> {
    bridge
        .call(
            "mcp.call",
            json!({"server":"source","tool":"echo","arguments":{"text":"hello"}}),
            CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn immutable_metadata_strict_schemas_digest_matching_and_search_injection() {
    let registry = registry(Limits::default());
    let state = Arc::new(State::default());
    let mut source_manifest = manifest();
    registry
        .register(
            identity("1"),
            source_manifest.clone(),
            handle("connection-a", state.clone()),
        )
        .unwrap();
    source_manifest.tools[0].input_schema = json!({"type":"object"});
    let bridge = support::consumer(&registry);
    let described = bridge
        .call(
            "mcp.describe",
            json!({"server":"source","tool":"echo"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        described["tool"]["input_schema"]["additionalProperties"],
        false
    );
    assert_eq!(call(&bridge).await.unwrap()["value"]["text"], "hello");
    for args in [json!({"text":7}), json!({"text":"x","extra":1}), json!({})] {
        assert!(
            bridge
                .call(
                    "mcp.call",
                    json!({"server":"source","tool":"echo","arguments":args}),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
    }
    assert!(
        bridge
            .call("mcp.discover", json!({"extra":1}), CancellationToken::new())
            .await
            .is_err()
    );
    let skill = bridge
        .call(
            "skills.read",
            json!({"name":"source/fixed"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(skill["value"], "fixed content");
    state.bad_skill.store(true, Ordering::Relaxed);
    assert!(
        !bridge
            .call(
                "skills.read",
                json!({"name":"source/fixed"}),
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .outcome_unknown
    );
    let search = bridge
        .call(
            "search.query",
            json!({"query":"needle","limit":2}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(search["value"]["matches"][0], "needle");
    registry.disconnect("source").unwrap();
    registry.disconnect("source").unwrap();
    assert_eq!(state.closed.load(Ordering::Relaxed), 1);
}

#[test]
fn duplicate_source_ownership_and_untrusted_schema_fail_closed() {
    let registry = registry(Limits::default());
    let state = Arc::new(State::default());
    registry
        .register(
            identity("1"),
            manifest(),
            handle("connection-a", state.clone()),
        )
        .unwrap();
    let rejected = Arc::new(State::default());
    assert!(
        registry
            .register(
                identity("2"),
                manifest(),
                handle("connection-b", rejected.clone())
            )
            .is_err()
    );
    assert_eq!(rejected.closed.load(Ordering::Relaxed), 1);
    assert_eq!(state.closed.load(Ordering::Relaxed), 0);
    let mut another = identity("1");
    another.id = "other".into();
    let mut no_search = manifest();
    no_search.search = false;
    let duplicate_wrapper = Arc::new(State::default());
    assert!(
        registry
            .register(
                another,
                no_search,
                handle("connection-a", duplicate_wrapper.clone())
            )
            .is_err()
    );
    assert_eq!(duplicate_wrapper.closed.load(Ordering::Relaxed), 0);
    assert_eq!(state.closed.load(Ordering::Relaxed), 0);
    for bad_schema in [
        json!({"type":"object","properties":{}}),
        json!({"type":"object","additionalProperties":false,"properties":{"x":{"$ref":"file:///private/schema"}}}),
        json!({"type":"object","additionalProperties":false,"properties":{"x":{"type":"object"}}}),
        json!({"type":"object","additionalProperties":false,"properties":{"x":{"type":["object","null"]}}}),
        json!({"type":"object","additionalProperties":false,"properties":{"x":{"properties":{"y":{"type":"string"}}}}}),
    ] {
        let isolated = super_registry();
        let mut bad = manifest();
        bad.tools[0].input_schema = bad_schema;
        assert!(
            isolated
                .register(
                    identity("1"),
                    bad,
                    handle("new", Arc::new(State::default()))
                )
                .is_err()
        );
    }
    let keyword_names = super_registry();
    let mut keyword_manifest = manifest();
    keyword_manifest.tools[0].input_schema = json!({
        "type":"object", "properties":{"properties":{"type":"string"},"type":{"type":"string"}},
        "required":["properties"], "additionalProperties":false
    });
    keyword_names
        .register(
            identity("1"),
            keyword_manifest,
            handle("keywords", Arc::new(State::default())),
        )
        .unwrap();
    registry.revoke("source").unwrap();
    assert!(
        registry
            .reconnect(
                identity("2"),
                manifest(),
                handle("connection-c", Arc::new(State::default()))
            )
            .is_err()
    );
    assert_eq!(state.closed.load(Ordering::Relaxed), 1);
}
fn super_registry() -> Registry {
    registry(Limits::default())
}

#[tokio::test]
async fn dropping_started_effect_cancels_token_and_retains_unknown_across_reconnect() {
    let registry = registry(Limits::default());
    let state = Arc::new(State::default());
    state.wait.store(true, Ordering::Relaxed);
    registry
        .register(identity("1"), manifest(), handle("first", state.clone()))
        .unwrap();
    let bridge = support::consumer(&registry);
    let mut future = Box::pin(call(&bridge));
    tokio::select! {result=&mut future=>panic!("unexpected result {result:?}"),_=state.entered.notified()=>{}}
    drop(future);
    assert!(
        state
            .observed_cancel
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    let unknown = registry.snapshot("source").unwrap();
    assert!(unknown.outcome_unknown);
    assert_eq!(unknown.unresolved.len(), 1);
    assert_eq!(unknown.unresolved[0].arguments_sha256.len(), 64);
    registry.disconnect("source").unwrap();
    let replacement = Arc::new(State::default());
    registry
        .reconnect(
            identity("2"),
            manifest(),
            handle("second", replacement.clone()),
        )
        .unwrap();
    let fresh = support::consumer(&registry);
    assert!(call(&fresh).await.unwrap_err().outcome_unknown);
    assert_eq!(replacement.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        registry.snapshot("source").unwrap().unresolved[0]
            .key
            .source
            .source
            .version,
        "1"
    );
}

#[tokio::test]
async fn reconnect_while_response_is_pending_cannot_accept_old_result() {
    let registry = registry(Limits::default());
    let state = Arc::new(State::default());
    state.wait.store(true, Ordering::Relaxed);
    registry
        .register(identity("1"), manifest(), handle("first", state.clone()))
        .unwrap();
    let old = support::consumer(&registry);
    let mut pending = Box::pin(call(&old));
    tokio::select! {result=&mut pending=>panic!("unexpected {result:?}"),_=state.entered.notified()=>{}}
    registry.disconnect("source").unwrap();
    registry
        .reconnect(
            identity("2"),
            manifest(),
            handle("second", Arc::new(State::default())),
        )
        .unwrap();
    state.release.notify_one();
    assert!(pending.await.unwrap_err().outcome_unknown);
    assert!(registry.snapshot("source").unwrap().outcome_unknown);
    assert!(
        call(&support::consumer(&registry))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(state.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn mismatched_ids_and_result_budgets_keep_executed_effects_unknown() {
    for mismatch in [true, false] {
        let registry = registry(Limits::default());
        let state = Arc::new(State::default());
        state.mismatch.store(mismatch, Ordering::Relaxed);
        state.oversize.store(!mismatch, Ordering::Relaxed);
        registry
            .register(identity("1"), manifest(), handle("first", state.clone()))
            .unwrap();
        assert!(
            call(&support::consumer(&registry))
                .await
                .unwrap_err()
                .outcome_unknown
        );
        let snapshot = registry.snapshot("source").unwrap();
        assert!(snapshot.outcome_unknown);
        assert_eq!(snapshot.unresolved.len(), 1);
        assert_eq!(state.calls.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn source_budgets_apply_to_metadata_operations_and_never_reset_on_reconnect() {
    let registry = registry(Limits {
        max_source_operations: 3,
        ..Limits::default()
    });
    let state = Arc::new(State::default());
    registry
        .register(identity("1"), manifest(), handle("first", state.clone()))
        .unwrap();
    let bridge = support::consumer(&registry);
    bridge
        .call("mcp.discover", json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert!(call(&bridge).await.is_ok());
    registry.disconnect("source").unwrap();
    registry
        .reconnect(
            identity("2"),
            manifest(),
            handle("second", Arc::new(State::default())),
        )
        .unwrap();
    assert!(call(&support::consumer(&registry)).await.is_err());
    assert_eq!(registry.snapshot("source").unwrap().operations, 3);
    assert_eq!(state.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn concurrent_calls_and_json_depth_are_bounded_before_execution() {
    let registry = registry(Limits {
        max_in_flight: 1,
        ..Limits::default()
    });
    let state = Arc::new(State::default());
    state.wait.store(true, Ordering::Relaxed);
    registry
        .register(identity("1"), manifest(), handle("first", state.clone()))
        .unwrap();
    let bridge = support::consumer(&registry);
    let mut pending = Box::pin(call(&bridge));
    tokio::select! { result = &mut pending => panic!("unexpected {result:?}"), _ = state.entered.notified() => {} }
    assert!(!call(&bridge).await.unwrap_err().outcome_unknown);
    assert_eq!(state.calls.load(Ordering::Relaxed), 1);
    state.release.notify_one();
    pending.await.unwrap();
    let mut nested = json!("leaf");
    for _ in 0..60 {
        nested = json!({"nested":nested});
    }
    assert!(bridge.call("mcp.call", json!({"server":"source","tool":"echo","arguments":{"text":"hello","nested":nested}}), CancellationToken::new()).await.is_err());
    assert_eq!(state.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn cancelled_before_admission_and_timeouts_do_not_replay() {
    let registry = registry(Limits {
        operation_timeout: Duration::from_millis(20),
        ..Limits::default()
    });
    let state = Arc::new(State::default());
    state.wait.store(true, Ordering::Relaxed);
    registry
        .register(identity("1"), manifest(), handle("first", state.clone()))
        .unwrap();
    let bridge = support::consumer(&registry);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        !bridge
            .call(
                "mcp.call",
                json!({"server":"source","tool":"echo","arguments":{"text":"hello"}}),
                cancel
            )
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(state.calls.load(Ordering::Relaxed), 0);
    assert!(call(&bridge).await.unwrap_err().outcome_unknown);
    assert_eq!(state.calls.load(Ordering::Relaxed), 1);
    assert!(
        state
            .observed_cancel
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
}
