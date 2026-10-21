//! Host integration with `cayw-runtime` (feature `plugins`): the real plugin
//! process is loaded, bound, called through CodeMode, crashed and unloaded,
//! while ordinary sessions keep working. Model replies are offline scripts.

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider,
};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_fusion_plugin::{COLLECT_METHOD, manifest};
use cayw_plugins::{ExternalConfig, PluginContext};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, ScopedPermission};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Offline model: runs the current Luau script, or answers in plain text when
/// the script is empty (the ordinary chat path).
struct Script(Arc<Mutex<String>>);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let script = self.0.lock().unwrap().clone();
        let message = if script.is_empty() {
            let mut message = AssistantMessage::new(&request.model);
            message
                .content
                .push(Content::text("ordinary offline reply"));
            message
        } else {
            cayw_runtime::fixture::script_response(
                &request,
                &script,
                &format!("fusion-{}", request.messages.len()),
            )
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

const ANSWERS: &str = "{provider_id='p0',kind='offline',behavior={mode='answer',value={n=1}}},\
{provider_id='p1',kind='offline',behavior={mode='answer',value={n=2}}},\
{provider_id='p2',kind='offline',behavior={mode='fail',message='offline failure'}}";
const CRASH: &str = "{provider_id='p0',kind='offline',behavior={mode='answer',value={n=1}}},\
{provider_id='p1',kind='offline',behavior={mode='crash'}},\
{provider_id='p2',kind='offline',behavior={mode='answer',value={n=1}}}";

fn luau(providers: &str) -> String {
    format!(
        "return cayw.call('fusion.collect',{{input={{question='offline question',\
canonical_context={{topic='fusion'}},constraints={{'no network'}},\
answer_schema={{type='object',properties={{n={{type='integer'}}}},required={{'n'}}}}}},\
providers={{{providers}}},per_provider_timeout_ms=20000}})"
    )
}

fn grant(providers: Value) -> ScopedPermission {
    ScopedPermission::exact(
        COLLECT_METHOD,
        json!({
            "input": {
                "question": "offline question",
                "canonical_context": {"topic": "fusion"},
                "constraints": ["no network"],
                "answer_schema": {"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]}
            },
            "providers": providers,
            "per_provider_timeout_ms": 20000
        }),
    )
}

fn answers() -> Value {
    json!([
        {"provider_id": "p0", "kind": "offline", "behavior": {"mode": "answer", "value": {"n": 1}}},
        {"provider_id": "p1", "kind": "offline", "behavior": {"mode": "answer", "value": {"n": 2}}},
        {"provider_id": "p2", "kind": "offline", "behavior": {"mode": "fail", "message": "offline failure"}}
    ])
}

fn crash() -> Value {
    json!([
        {"provider_id": "p0", "kind": "offline", "behavior": {"mode": "answer", "value": {"n": 1}}},
        {"provider_id": "p1", "kind": "offline", "behavior": {"mode": "crash"}},
        {"provider_id": "p2", "kind": "offline", "behavior": {"mode": "answer", "value": {"n": 1}}}
    ])
}

fn runtime(dir: &tempfile::TempDir, script: &Arc<Mutex<String>>) -> Runtime {
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();
    let config = RuntimeConfig {
        permissions: vec![grant(answers()), grant(crash())],
        ..Default::default()
    };
    Runtime::open(
        dir.path().join("sessions.db"),
        dir.path().join("workspace"),
        Arc::new(Script(script.clone())),
        Model::new("fixture", "fusion-plugin"),
        config,
    )
    .unwrap()
}

fn external(argv: &[&str]) -> ExternalConfig {
    let mut config = ExternalConfig::new(
        PathBuf::from(env!("CARGO_BIN_EXE_cayw-fusion-plugin")),
        argv.iter().map(|argument| (*argument).into()).collect(),
        manifest(),
    );
    config.call_timeout = Duration::from_secs(60);
    config
}

fn context() -> PluginContext {
    PluginContext::new([COLLECT_METHOD.into()], [])
}

fn settled(rt: &Runtime, session: &str) -> Vec<CapabilityOutcome> {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            CoreEvent::CapabilitySettled { call } if call.name == COLLECT_METHOD => call.outcome,
            _ => None,
        })
        .collect()
}

async fn ordinary_chat_still_works(rt: &Runtime, script: &Arc<Mutex<String>>) {
    script.lock().unwrap().clear();
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "ordinary chat", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert!(!view.frozen);
    assert!(view.binding.is_none());
    assert!(
        view.messages
            .iter()
            .any(|message| format!("{message:?}").contains("ordinary offline reply"))
    );
}

#[tokio::test]
async fn runtime_calls_external_fusion_then_unload_leaves_ordinary_chat_working() {
    let dir = tempfile::tempdir().unwrap();
    let script = Arc::new(Mutex::new(luau(ANSWERS)));
    let rt = runtime(&dir, &script);

    // The plugin's capability names do not collide with the host catalog.
    let catalog = rt.capabilities().unwrap();
    for capability in &manifest().capabilities {
        assert!(
            catalog
                .iter()
                .all(|existing| existing.name != capability.name)
        );
    }
    let description = rt
        .load_external(external(&[]), context(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(description.manifest, manifest());
    // A second instance under the same name is refused.
    assert!(
        rt.load_external(external(&[]), context(), CancellationToken::new())
            .await
            .is_err()
    );

    let id = rt.create(None).unwrap().id;
    rt.bind_plugin(&id, "fusion").unwrap();
    let view = rt
        .prompt(&id, "collect fusion", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:?}");
    let outcomes = settled(&rt, &id);
    let [CapabilityOutcome::Completed { result }] = outcomes.as_slice() else {
        panic!("expected one completed fusion.collect: {outcomes:?}");
    };
    let candidates = result["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 3);
    assert_eq!(candidates[0]["outcome"]["value"], json!({"n": 1}));
    assert_eq!(candidates[1]["outcome"]["value"], json!({"n": 2}));
    assert_eq!(candidates[2]["outcome"]["status"], "failed");
    assert_eq!(result["disagreements"], json!([["p0", "p1"]]));
    let digest = result["input_digest"].as_str().unwrap();
    assert_eq!(digest.len(), 64);
    assert!(candidates.iter().all(|c| c["input_digest"] == digest));

    rt.unload_plugin("fusion").unwrap();
    assert!(rt.list_plugins().is_empty());
    // The bound session freezes by design; it is not silently unbound.
    assert!(matches!(
        rt.prompt(&id, "again", CancellationToken::new()).await,
        Err(RuntimeError::Frozen(_))
    ));
    ordinary_chat_still_works(&rt, &script).await;
}

#[tokio::test]
async fn plugin_crash_mid_call_is_reported_and_host_keeps_working() {
    let dir = tempfile::tempdir().unwrap();
    let script = Arc::new(Mutex::new(luau(CRASH)));
    let rt = runtime(&dir, &script);
    rt.load_external(
        external(&["--allow-fault-injection"]),
        context(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let id = rt.create(None).unwrap().id;
    rt.bind_plugin(&id, "fusion").unwrap();
    let view = rt
        .prompt(&id, "crash mid call", CancellationToken::new())
        .await
        .unwrap();
    // The process died before replying: the outcome is unknown, never a report.
    assert_eq!(view.state, SessionState::NeedsReconciliation);
    assert!(view.frozen);
    let outcomes = settled(&rt, &id);
    assert!(
        matches!(
            outcomes.as_slice(),
            [CapabilityOutcome::OutcomeUnknown { .. }]
        ),
        "{outcomes:?}"
    );

    // The host process and other sessions are unaffected.
    ordinary_chat_still_works(&rt, &script).await;
    // The dead instance can be unloaded and a fresh one loaded and used.
    rt.unload_plugin("fusion").unwrap();
    rt.load_external(external(&[]), context(), CancellationToken::new())
        .await
        .unwrap();
    *script.lock().unwrap() = luau(ANSWERS);
    let fresh = rt.create(None).unwrap().id;
    rt.bind_plugin(&fresh, "fusion").unwrap();
    let view = rt
        .prompt(&fresh, "collect again", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:?}");
    assert!(matches!(
        settled(&rt, &fresh).as_slice(),
        [CapabilityOutcome::Completed { .. }]
    ));
}
