//! Actual Runtime -> Cells/Luau -> Host -> Bridge -> injected local source tests.
use async_trait::async_trait;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_bridge::{
    AccessRequest, DispatchOutcome, Limits, Manifest, Provenance, Registry, RegistryPolicy,
    SkillSpec, SourceHandle, SourceIdentity, SourceOperation, SourceReply, SourceRequest, ToolSpec,
    Visibility, content_sha256,
};
use cayw_codemode::CapabilityError;
use cayw_core::{CapabilityOutcome, CapabilityReconciliation, CoreEvent, SessionState};
use cayw_runtime::{
    ApprovalChoice, BridgeDispatchEvent, RequestPolicy, Runtime, RuntimeConfig, ScopedPermission,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

struct Script {
    script: String,
    calls: AtomicUsize,
    second: Option<Arc<Semaphore>>,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let id = format!("bridge-{}", request.messages.len());
        let message = cayw_runtime::fixture::script_response(&request, &self.script, &id);
        let pause = (self.calls.fetch_add(1, Ordering::AcqRel) == 1)
            .then(|| self.second.clone())
            .flatten();
        Box::pin(async_stream::stream! {
            if let Some(pause) = pause { pause.acquire_owned().await.unwrap().forget(); }
            yield AssistantEvent::Done { message };
        })
    }
}
struct Policy(bool);
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
        if self.0 {
            Ok(())
        } else {
            Err(CapabilityError::new("fixture registry denial"))
        }
    }
}
struct SourceState {
    requests: Mutex<Vec<SourceRequest>>,
    disconnected: AtomicUsize,
    entered: Notify,
    release: Semaphore,
}
// Semaphore has no Default; this fixture uses an explicitly initialized owner.
impl SourceState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(vec![]),
            disconnected: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Semaphore::new(0),
        })
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
enum Behavior {
    Reply,
    Block,
    Large(usize),
}
struct Source {
    key: String,
    state: Arc<SourceState>,
    behavior: Behavior,
    db: Option<std::path::PathBuf>,
}
#[async_trait]
impl SourceHandle for Source {
    fn ownership_key(&self) -> &str {
        &self.key
    }
    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        // The preparation is a real committed Core event before the source is
        // polled; an independent SQLite connection can read it immediately.
        if let Some(path) = &self.db {
            let db = rusqlite::Connection::open(path).unwrap();
            let n: i64 = db.query_row("SELECT count(*) FROM core_events WHERE json_extract(record,'$.event.kind')='runtime_bridge_prepared' AND json_extract(record,'$.event.data.record.key.id')=?1", [i64::try_from(request.key.id).unwrap()], |row| row.get(0)).unwrap();
            assert_eq!(n, 1, "source polled without committed preparation");
        }
        self.state.requests.lock().unwrap().push(request.clone());
        self.state.entered.notify_one();
        if matches!(self.behavior, Behavior::Block) {
            tokio::select! { biased;
                _ = cancel.cancelled() => return Err(CapabilityError::unknown("fixture source interrupted after dispatch")),
                permit = self.state.release.acquire() => { permit.unwrap().forget(); },
            }
        }
        let value = match (&request.operation, &self.behavior) {
            (_, Behavior::Large(bytes)) => json!("x".repeat(*bytes)),
            (SourceOperation::InvokeTool { arguments, .. }, _) => json!({"echo":arguments["text"]}),
            (SourceOperation::ReadSkill { .. }, _) => json!("fixture skill body"),
            (SourceOperation::ReadResource { uri }, _) => {
                json!({"uri":uri,"mime_type":"text/markdown","text":"fixture resource body"})
            }
            (SourceOperation::Search { arguments }, _) => json!({"matches":[arguments["query"]]}),
            (SourceOperation::GetPrompt { name, arguments }, _) => {
                json!({"name": name, "description": "fixture prompt", "arguments": arguments, "messages": []})
            }
        };
        Ok(SourceReply {
            key: request.key,
            value,
        })
    }
    fn disconnect(&self) {
        self.state.disconnected.fetch_add(1, Ordering::AcqRel);
    }
}
fn identity() -> SourceIdentity {
    SourceIdentity {
        id: "local".into(),
        version: "1.0".into(),
        origin: "fixed-local-source".into(),
    }
}
fn manifest() -> Manifest {
    Manifest {
        tools: vec![ToolSpec {
            name: "echo".into(),
            description: "Fixed local echo".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            output_schema: None,
        }],
        skills: vec![SkillSpec {
            name: "guide".into(),
            description: "Fixed local skill".into(),
            content_sha256: content_sha256("fixture skill body"),
        }],
        resources: Vec::new(),
        prompts: Vec::new(),
        search: true,
    }
}
fn registry(
    state: Arc<SourceState>,
    behavior: Behavior,
    policy: bool,
    limits: Limits,
    db: Option<std::path::PathBuf>,
) -> Registry {
    let registry = Registry::new(limits, Arc::new(Policy(policy))).unwrap();
    registry
        .register(
            identity(),
            manifest(),
            Box::new(Source {
                key: "physical-handle-1".into(),
                state,
                behavior,
                db,
            }),
        )
        .unwrap();
    registry
}
fn setup() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    temp
}
fn config(registry: &Registry, calls: &[(&str, Value)]) -> RuntimeConfig {
    RuntimeConfig {
        bridge: Some(registry.consumer()),
        max_run_time: Duration::from_secs(15),
        permissions: calls
            .iter()
            .map(|(name, arguments)| ScopedPermission::exact(*name, arguments.clone()))
            .collect(),
        ..Default::default()
    }
}
fn open(
    temp: &tempfile::TempDir,
    settings: RuntimeConfig,
    script: &str,
    pause: Option<Arc<Semaphore>>,
) -> (Runtime, Arc<Script>) {
    let provider = Arc::new(Script {
        script: script.into(),
        calls: AtomicUsize::new(0),
        second: pause,
    });
    let rt = Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        provider.clone(),
        Model::new("fixture", "runtime-bridge"),
        settings,
    )
    .unwrap();
    (rt, provider)
}
fn calls() -> Vec<(&'static str, Value)> {
    vec![
        ("mcp.discover", json!({})),
        ("mcp.describe", json!({"server":"local","tool":"echo"})),
        ("skills.list", json!({})),
        ("skills.read", json!({"name":"local/guide"})),
        (
            "mcp.call",
            json!({"server":"local","tool":"echo","arguments":{"text":"fixture"}}),
        ),
        ("search.query", json!({"query":"fixture"})),
    ]
}
fn history(rt: &Runtime, session: &str) -> Vec<BridgeDispatchEvent> {
    let mut cursor = 0;
    let mut output = vec![];
    loop {
        let page = rt.bridge_dispatches(session, cursor, 7).unwrap();
        if page.next_cursor == cursor {
            break;
        }
        cursor = page.next_cursor;
        output.extend(page.events);
    }
    output
}
fn core_calls(rt: &Runtime, id: &str) -> Vec<cayw_core::CapabilityCallRecord> {
    rt.events(id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call }
            | CoreEvent::CapabilityReconciled { call, .. } => Some(call),
            _ => None,
        })
        .collect()
}
async fn pending(rt: &Runtime, session: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(approval) = rt.get(session).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn luau_uses_existing_source_for_catalog_skill_tool_and_search_with_full_origins() {
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Reply,
        true,
        Limits::default(),
        Some(temp.path().join("session.db")),
    );
    let settings = config(&registry, &calls());
    let pins = settings.bridge.as_ref().unwrap().binding_snapshot();
    let script = "local a=cayw.call('mcp.discover', {})\nlocal b=cayw.call('mcp.describe',{server='local',tool='echo'})\nlocal c=cayw.call('skills.list',{})\nlocal d=cayw.call('skills.read',{name='local/guide'})\nlocal e=cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})\nlocal f=cayw.call('search.query',{query='fixture'})\nreturn {catalog=a,description=b,skills=c,skill=d,tool=e,search=f}";
    let (rt, _) = open(&temp, settings.clone(), script, None);
    assert!(
        rt.capabilities()
            .unwrap()
            .iter()
            .any(|definition| definition.name == "mcp.call")
    );
    let id = rt.create(None).unwrap().id;
    assert_eq!(
        rt.prompt(&id, "run fixed local sources", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    assert_eq!(state.count(), 3);
    let events = history(&rt, &id);
    assert_eq!(events.len(), 6);
    let calls = core_calls(&rt, &id);
    assert_eq!(calls.len(), 6);
    assert!(
        calls
            .iter()
            .all(|call| call.external_provenance == Some(serde_json::to_value(&pins).unwrap()))
    );
    for event in &events {
        let call = calls
            .iter()
            .find(|call| call.key == event.origin.key)
            .unwrap();
        assert_eq!(event.origin.lease, call.lease);
        assert_eq!(event.origin.task_id, call.task_id);
        assert_eq!(event.origin.parent_call_id, call.parent_call_id);
        assert_eq!(event.origin.source_hash, call.source_hash);
        assert_eq!(event.record.context.sequence, call.key.sequence + 1);
        assert_eq!(event.record.context.execution_id, call.key.execution_id);
        assert_eq!(event.record.key.source, pins.sources["local"]);
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.outcome, Some(DispatchOutcome::Succeeded { .. })))
            .count(),
        3
    );
    assert_eq!(rt.recover(&id).unwrap().state, SessionState::Idle);
    drop(rt);
    let (reopened, _) = open(&temp, settings, "return 1", None);
    assert_eq!(history(&reopened, &id).len(), 6);
    assert_eq!(state.count(), 3);
    assert_eq!(state.disconnected.load(Ordering::Acquire), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interleaved_cells_retain_distinct_execution_parent_and_source_hash_on_one_handle() {
    struct CellsProvider {
        stage: AtomicUsize,
    }
    impl Provider for CellsProvider {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let stage = self.stage.fetch_add(1, Ordering::AcqRel);
            let mut message = cayw_ai::AssistantMessage::new(&request.model);
            // Fixed synthetic counters exercise accounting, not tokenization or billing.
            message.usage.input = 4;
            message.usage.output = 4;
            message.usage.update_total();
            message.usage_status = cayw_ai::UsageStatus::Reported;
            if stage < 2 {
                message.stop_reason = cayw_ai::StopReason::ToolUse;
                let text = if stage == 0 { "first" } else { "second" };
                message.content.push(cayw_ai::Content::ToolCall(cayw_ai::ToolCall::new(format!("parent-{stage}"), "execute_code", json!({"action":"exec","script":format!("return cayw.call('mcp.call',{{server='local',tool='echo',arguments={{text='{text}'}}}})"),"yield_time_ms":1,"max_output_tokens":1024}))));
            } else {
                // Both execs are outstanding: one cell started first and the
                // other started before the first was waited. Track every real
                // cell page independently so each VM is resumed until all of
                // its output is read and it is no longer alive.
                let mut pages = std::collections::BTreeMap::new();
                for prior in &request.messages {
                    let cayw_ai::Message::ToolResult(result) = prior else {
                        continue;
                    };
                    if result.tool_name != "execute_code" {
                        continue;
                    }
                    let Some(page) = result.structured_content.as_ref().and_then(|value| {
                        serde_json::from_value::<cayw_cells::CellPage>(value.clone()).ok()
                    }) else {
                        continue;
                    };
                    pages.insert(page.cell_id.clone(), page);
                }
                let pending = pages
                    .values()
                    .find(|page| page.vm_alive || page.has_more || !page.status.is_terminal());
                match pending {
                    Some(page) => {
                        message.stop_reason = cayw_ai::StopReason::ToolUse;
                        message
                            .content
                            .push(cayw_ai::Content::ToolCall(cayw_ai::ToolCall::new(
                                format!("wait-{}-{}-{}", page.cell_id, page.next_cursor, stage),
                                "execute_code",
                                json!({
                                    "action":"wait",
                                    "cell_id":page.cell_id,
                                    "cursor":page.next_cursor,
                                    "yield_time_ms":1000,
                                    "max_output_tokens":1024,
                                    "continue_execution":true,
                                }),
                            )));
                    }
                    None => {
                        message
                            .content
                            .push(cayw_ai::Content::text("both cells completed"));
                        message.stop_reason = cayw_ai::StopReason::Stop;
                    }
                }
            }
            Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
        }
    }
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Block,
        true,
        Limits::default(),
        Some(temp.path().join("session.db")),
    );
    let settings = config(
        &registry,
        &[
            (
                "mcp.call",
                json!({"server":"local","tool":"echo","arguments":{"text":"first"}}),
            ),
            (
                "mcp.call",
                json!({"server":"local","tool":"echo","arguments":{"text":"second"}}),
            ),
        ],
    );
    let rt = Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        Arc::new(CellsProvider {
            stage: AtomicUsize::new(0),
        }),
        Model::new("fixture", "two-cells"),
        settings,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let run_rt = rt.clone();
    let run_id = id.clone();
    let run = tokio::spawn(async move {
        run_rt
            .prompt(&run_id, "two local cells", CancellationToken::new())
            .await
    });
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let events = rt.events(&id, 0, 1000).unwrap();
            let executions = events
                .iter()
                .filter(|event| matches!(event.event, CoreEvent::ExecutionStarted { .. }))
                .count();
            let planned = events
                .iter()
                .filter(|event| matches!(event.event, CoreEvent::CapabilityPlanned { .. }))
                .count();
            if executions == 2 && planned == 2 && state.count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state.release.add_permits(2);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(8), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .state,
        SessionState::Idle
    );
    assert_eq!(state.count(), 2);
    let events = history(&rt, &id);
    let settled = events
        .iter()
        .filter(|event| event.outcome.is_some())
        .collect::<Vec<_>>();
    assert_eq!(settled.len(), 2);
    assert_ne!(
        settled[0].origin.key.execution_id,
        settled[1].origin.key.execution_id
    );
    assert_ne!(
        settled[0].origin.parent_call_id,
        settled[1].origin.parent_call_id
    );
    assert_ne!(settled[0].origin.source_hash, settled[1].origin.source_hash);
    assert_eq!(settled[0].origin.task_id, settled[1].origin.task_id);
    assert_eq!(settled[0].origin.lease, settled[1].origin.lease);
    assert!(
        settled
            .iter()
            .all(|event| event.record.context.sequence == 1)
    );
    assert_eq!(state.disconnected.load(Ordering::Acquire), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_and_registry_denial_dispatch_zero_sources_and_have_no_source_journal() {
    for host_denies in [true, false] {
        let temp = setup();
        let state = SourceState::new();
        let registry = registry(
            state.clone(),
            Behavior::Reply,
            host_denies,
            Limits::default(),
            None,
        );
        let allowed = if host_denies { vec![] } else { calls() };
        let settings = config(&registry, &allowed);
        let (rt, _) = open(
            &temp,
            settings,
            "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
            None,
        );
        let id = rt.create(None).unwrap().id;
        let run_rt = rt.clone();
        let run_id = id.clone();
        let run = tokio::spawn(async move {
            run_rt
                .prompt(&run_id, "denied", CancellationToken::new())
                .await
        });
        if host_denies {
            let approval = pending(&rt, &id).await;
            rt.approve(&id, &approval.id, ApprovalChoice::Deny, "fixture-host")
                .unwrap();
        }
        let _ = run.await.unwrap();
        assert_eq!(state.count(), 0);
        assert!(history(&rt, &id).is_empty());
        assert!(!matches!(
            core_calls(&rt, &id)[0].outcome,
            Some(CapabilityOutcome::Completed { .. })
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_never_routes_old_pending_task_to_replacement_and_revoke_is_permanent() {
    let temp = setup();
    let old = SourceState::new();
    let registry = registry(old.clone(), Behavior::Reply, true, Limits::default(), None);
    let settings = config(&registry, &[]);
    let (rt, _) = open(
        &temp,
        settings.clone(),
        "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        None,
    );
    let id = rt.create(None).unwrap().id;
    let run_rt = rt.clone();
    let run_id = id.clone();
    let run = tokio::spawn(async move {
        run_rt
            .prompt(&run_id, "pending old generation", CancellationToken::new())
            .await
    });
    let approval = pending(&rt, &id).await;
    registry.disconnect("local").unwrap();
    let replacement = SourceState::new();
    registry
        .reconnect(
            identity(),
            manifest(),
            Box::new(Source {
                key: "physical-handle-2".into(),
                state: replacement.clone(),
                behavior: Behavior::Reply,
                db: None,
            }),
        )
        .unwrap();
    rt.approve(&id, &approval.id, ApprovalChoice::AllowOnce, "fixture-host")
        .unwrap();
    let _ = run.await.unwrap();
    assert_eq!(old.count(), 0);
    assert_eq!(replacement.count(), 0);
    assert_eq!(old.disconnected.load(Ordering::Acquire), 1);
    assert!(history(&rt, &id).is_empty());
    registry.revoke("local").unwrap();
    assert!(
        registry
            .reconnect(
                identity(),
                manifest(),
                Box::new(Source {
                    key: "physical-handle-3".into(),
                    state: SourceState::new(),
                    behavior: Behavior::Reply,
                    db: None
                })
            )
            .is_err()
    );
    let next = rt.create(None).unwrap().id;
    let mut granted = settings;
    granted.permissions = config(&registry, &calls()).permissions;
    let (other, _) = open(
        &temp,
        granted,
        "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        None,
    );
    let _ = other
        .prompt(&next, "revoked old consumer", CancellationToken::new())
        .await;
    assert_eq!(replacement.count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_cancellation_retains_unknown_across_reopen_and_rejects_wrong_provenance() {
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Block,
        true,
        Limits::default(),
        None,
    );
    let settings = config(&registry, &calls());
    let (rt, provider) = open(
        &temp,
        settings.clone(),
        "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        None,
    );
    let id = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&id, "cancel source").unwrap();
    let run_rt = rt.clone();
    let task_id = admitted.id.clone();
    let run = tokio::spawn(async move { run_rt.resume(&task_id, CancellationToken::new()).await });
    tokio::time::timeout(Duration::from_secs(8), state.entered.notified())
        .await
        .unwrap();
    rt.cancel(&rt.task(&admitted.id).unwrap().cancel_target())
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(8), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.count(), 1);
    let view = rt.get(&id).unwrap();
    assert_eq!(view.state, SessionState::NeedsReconciliation);
    let events = history(&rt, &id);
    assert!(matches!(
        events.last().unwrap().outcome,
        Some(DispatchOutcome::Unknown { .. })
    ));
    let call = &view.unresolved_capabilities[0];
    let mut wrong = call.external_provenance.clone().unwrap();
    wrong["registry_incarnation"] = json!("replacement");
    assert!(
        rt.reconcile_capability(
            &id,
            CapabilityReconciliation {
                key: call.key.clone(),
                expected: Box::new(cayw_core::ReconciliationOrigin::from(call)),
                actor: "fixture-host".into(),
                evidence: "wrong catalog cannot reconcile".into(),
                outcome: CapabilityOutcome::NotExecuted {
                    reason: "unverified".into()
                },
                external_provenance: Some(wrong)
            }
        )
        .is_err()
    );
    assert!(rt.enqueue(&id, "retry").is_err());
    assert!(registry.snapshot("local").unwrap().outcome_unknown);
    registry.disconnect("local").unwrap();
    let replacement = SourceState::new();
    registry
        .reconnect(
            identity(),
            manifest(),
            Box::new(Source {
                key: "uncertain-replacement".into(),
                state: replacement.clone(),
                behavior: Behavior::Reply,
                db: None,
            }),
        )
        .unwrap();
    assert!(registry.snapshot("local").unwrap().outcome_unknown);
    let fresh = registry.consumer();
    assert_ne!(
        fresh.binding_snapshot(),
        settings.bridge.as_ref().unwrap().binding_snapshot()
    );
    assert_eq!(replacement.count(), 0);
    let model_calls = provider.calls.load(Ordering::Acquire);
    drop(rt);
    let (reopened, second) = open(&temp, settings, "return 1", None);
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(reopened.enqueue(&id, "retry").is_err());
    assert_eq!(state.count(), 1);
    assert_eq!(second.calls.load(Ordering::Acquire), 0);
    assert!(model_calls > 0);
    assert_eq!(history(&reopened, &id).len(), events.len());
    let fresh_settings = RuntimeConfig {
        bridge: Some(fresh),
        permissions: config(&registry, &calls()).permissions,
        ..Default::default()
    };
    let (fresh_rt, _) = open(
        &temp,
        fresh_settings,
        "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        None,
    );
    let fresh_id = fresh_rt.create(None).unwrap().id;
    let _ = fresh_rt
        .prompt(
            &fresh_id,
            "fresh consumer cannot clear source unknown",
            CancellationToken::new(),
        )
        .await;
    assert_eq!(replacement.count(), 0);
    assert_eq!(state.count(), 1);
}

#[tokio::test]
async fn preparation_persistence_failure_dispatches_zero_and_holds_admission() {
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Reply,
        true,
        Limits::default(),
        None,
    );
    let mut settings = config(&registry, &calls());
    settings.max_concurrent_runs = 1;
    settings.max_queued_runs = 1;
    let (rt, _) = open(
        &temp,
        settings,
        "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        None,
    );
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_bridge_prepare BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='runtime_bridge_prepared' BEGIN SELECT RAISE(ABORT,'injected Bridge prepare failure'); END;").unwrap();
    let error = rt
        .prompt(&id, "prepare fails", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected Bridge prepare failure"),
        "{error}"
    );
    assert_eq!(state.count(), 0);
    assert_eq!(rt.health().unwrap().running, 1);
    assert!(
        rt.tasks(&id)
            .unwrap()
            .iter()
            .all(|task| task.state == cayw_runtime::TaskState::Draining)
    );
    let other = rt.create(None).unwrap().id;
    assert!(matches!(
        rt.enqueue(&other, "no slot"),
        Err(cayw_runtime::RuntimeError::QueueFull)
    ));
    // The prepared guard can write atomic first NotExecuted evidence even when
    // the started transaction was rejected. It may never invent Unknown here.
    let events = history(&rt, &id);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].outcome,
        Some(DispatchOutcome::NotExecuted { .. })
    ));
}

#[tokio::test]
async fn committed_source_terminal_repairs_missing_core_terminal_without_source_or_model_replay() {
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Reply,
        true,
        Limits::default(),
        None,
    );
    let settings = config(&registry, &calls());
    let (rt, _) = open(
        &temp,
        settings.clone(),
        "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        None,
    );
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_core_terminal BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='capability_settled' BEGIN SELECT RAISE(ABORT,'injected Core terminal failure'); END;").unwrap();
    assert!(
        rt.prompt(&id, "lost final audit", CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(state.count(), 1);
    let events = history(&rt, &id);
    assert!(matches!(
        events.last().unwrap().outcome,
        Some(DispatchOutcome::Succeeded { .. })
    ));
    db.execute_batch("DROP TRIGGER fail_core_terminal;")
        .unwrap();
    drop(rt);
    let (reopened, provider) = open(&temp, settings, "return 1", None);
    let view = reopened.recover(&id).unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_capabilities.is_empty());
    assert_eq!(state.count(), 1);
    assert_eq!(provider.calls.load(Ordering::Acquire), 0);
    assert!(reopened.events(&id, 0, 1000).unwrap().iter().any(|event| matches!(&event.event, CoreEvent::CapabilityReconciled { reconciliation, .. } if reconciliation.actor == "runtime-bridge-recovery")));
    assert_eq!(reopened.recover(&id).unwrap().state, SessionState::Idle);
}

#[tokio::test]
async fn host_result_bound_retains_explicit_unknown_even_with_known_bridge_terminal() {
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Large(1024),
        true,
        Limits::default(),
        None,
    );
    let mut settings = config(&registry, &calls());
    settings.host.max_result_bytes = 512;
    let (rt, _) = open(
        &temp,
        settings,
        "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        None,
    );
    let id = rt.create(None).unwrap().id;
    let _ = rt
        .prompt(&id, "bounded source result", CancellationToken::new())
        .await;
    assert_eq!(state.count(), 1);
    assert!(matches!(
        history(&rt, &id).last().unwrap().outcome,
        Some(DispatchOutcome::Succeeded { .. })
    ));
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(
        rt.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
}

#[tokio::test]
async fn bridge_result_bound_is_known_for_skill_and_unknown_for_effectful_tool() {
    for skill in [true, false] {
        let temp = setup();
        let state = SourceState::new();
        let registry = registry(
            state.clone(),
            Behavior::Large(3000),
            true,
            Limits {
                max_result_bytes: 2048,
                ..Default::default()
            },
            None,
        );
        let settings = config(&registry, &calls());
        let script = if skill {
            "return cayw.call('skills.read',{name='local/guide'})"
        } else {
            "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})"
        };
        let (rt, _) = open(&temp, settings, script, None);
        let id = rt.create(None).unwrap().id;
        let _ = rt
            .prompt(&id, "bound source reply", CancellationToken::new())
            .await;
        assert_eq!(state.count(), 1);
        let events = history(&rt, &id);
        if skill {
            assert!(matches!(
                events.last().unwrap().outcome,
                Some(DispatchOutcome::Failed { .. })
            ));
            assert_eq!(rt.recover(&id).unwrap().state, SessionState::Idle);
        } else {
            assert!(matches!(
                events.last().unwrap().outcome,
                Some(DispatchOutcome::Unknown { .. })
            ));
            assert_eq!(
                rt.recover(&id).unwrap().state,
                SessionState::NeedsReconciliation
            );
        }
    }
}

#[test]
fn ordinary_chat_has_no_bridge_catalog_and_conflicting_external_configuration_is_rejected() {
    let temp = setup();
    let (rt, _) = open(&temp, RuntimeConfig::default(), "return 1", None);
    assert!(
        !rt.capabilities()
            .unwrap()
            .iter()
            .any(|definition| definition.name == "mcp.call")
    );
    struct Raw;
    #[async_trait]
    impl cayw_host::ExternalHost for Raw {
        async fn call(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            Ok(Value::Null)
        }
    }
    let registry = registry(
        SourceState::new(),
        Behavior::Reply,
        true,
        Limits::default(),
        None,
    );
    let mut settings = config(&registry, &calls());
    settings.host.external = Some(Arc::new(Raw));
    assert!(matches!(
        Runtime::open(
            temp.path().join("session.db"),
            temp.path().join("workspace"),
            Arc::new(Script {
                script: "return 1".into(),
                calls: AtomicUsize::new(0),
                second: None
            }),
            Model::new("fixture", "ambiguity"),
            settings
        ),
        Err(cayw_runtime::RuntimeError::InvalidInput(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_call_budget_is_shared_across_fresh_bridge_scopes() {
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Reply,
        true,
        Limits::default(),
        None,
    );
    let mut settings = config(&registry, &calls());
    settings.host.max_calls = 1;
    let (rt, _) = open(
        &temp,
        settings,
        "cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})\nreturn cayw.call('search.query',{query='fixture'})",
        None,
    );
    let id = rt.create(None).unwrap().id;
    let _ = rt
        .prompt(&id, "shared Host call budget", CancellationToken::new())
        .await;
    assert_eq!(state.count(), 1);
    let calls = core_calls(&rt, &id);
    assert_eq!(calls.len(), 2);
    assert!(matches!(
        calls[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(matches!(
        calls[1].outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
    assert_eq!(history(&rt, &id).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_bind_original_bridge_incarnation_and_scope_actual_call() {
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Reply,
        true,
        Limits::default(),
        None,
    );
    let mut settings = config(&registry, &calls());
    settings.request_policy = Some(RequestPolicy {
        permission_policy_id: "bridge-policy".into(),
        operations: vec![cayw_requests::OperationScope::exact(
            "echo-exact",
            "mcp.call",
            calls()[4].1.clone(),
            "local-policy",
        )],
    });
    let pause = Arc::new(Semaphore::new(0));
    let (rt, _) = open(
        &temp,
        settings.clone(),
        "local input={purpose='echo fixed input',gap='source call needs approval',operation_id='echo-exact',capability='mcp.call',arguments={server='local',tool='echo',arguments={text='fixture'}}}\ncayw.call('capability.request',input)\ncayw.emit(cayw.call('capability.request',input))\nyield_control()\nreturn cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        Some(pause.clone()),
    );
    assert_eq!(rt.request_operations().unwrap().len(), 1);
    let id = rt.create(None).unwrap().id;
    let task = rt.enqueue(&id, "request fixed source").unwrap();
    let run_rt = rt.clone();
    let task_id = task.id.clone();
    let run = tokio::spawn(async move { run_rt.resume(&task_id, CancellationToken::new()).await });
    let record = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(events) = rt.request_events(&task.id, 0, 64) {
                for event in events.into_iter().rev() {
                    if let Some(id) = event.request_id {
                        let record = rt.request_status(&task.id, &id).unwrap();
                        if record.state == cayw_requests::RequestState::AwaitingEvaluation {
                            return record;
                        }
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("Bridge request did not reach evaluation: {error}; task={:?}; view={:?}; events={:?}; requests={:?}", rt.task(&task.id), rt.get(&id), rt.events(&id,0,1000), rt.request_events(&task.id,0,100)));
    assert_eq!(state.count(), 0);
    let ticket = rt
        .request_evaluate(&task.id, &record.id, "fixture-host")
        .unwrap();
    // A new physical registry with identical public metadata must not inherit the
    // old request's catalog fingerprint or approval receipt.
    let replacement = self::registry(
        SourceState::new(),
        Behavior::Reply,
        true,
        Limits::default(),
        None,
    );
    let mut alternate = settings;
    alternate.bridge = Some(replacement.consumer());
    let (other, _) = open(&temp, alternate, "return 1", None);
    assert!(
        other
            .request_approve(&task.id, &ticket, Duration::from_secs(5), "wrong registry")
            .is_err()
    );
    rt.request_approve(
        &task.id,
        &ticket,
        Duration::from_secs(5),
        "verified original registry",
    )
    .unwrap();
    pause.add_permits(1);
    assert_eq!(run.await.unwrap().unwrap().state, SessionState::Idle);
    assert_eq!(state.count(), 1);
    let journal = history(&rt, &id);
    assert_eq!(journal.len(), 2);
    let call = core_calls(&rt, &id)
        .into_iter()
        .find(|call| call.name == "mcp.call")
        .unwrap();
    assert!(rt.events(&id, 0, 1000).unwrap().iter().any(|event| matches!(&event.event, CoreEvent::Host { kind, data } if kind == "task_request_admitted" && data["key"] == serde_json::to_value(&call.key).unwrap())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_prompt_durably_records_source_unknown_and_fences_original_registry() {
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Block,
        true,
        Limits::default(),
        None,
    );
    let settings = config(&registry, &calls());
    let (rt, _) = open(
        &temp,
        settings.clone(),
        "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        None,
    );
    let id = rt.create(None).unwrap().id;
    let run_rt = rt.clone();
    let run_id = id.clone();
    let run = tokio::spawn(async move {
        run_rt
            .prompt(&run_id, "dropped caller", CancellationToken::new())
            .await
    });
    tokio::time::timeout(Duration::from_secs(8), state.entered.notified())
        .await
        .unwrap();
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(8), rt.wait_idle(&id))
        .await
        .unwrap();
    assert_eq!(state.count(), 1);
    assert!(matches!(
        history(&rt, &id).last().unwrap().outcome,
        Some(DispatchOutcome::Unknown { .. })
    ));
    assert!(registry.snapshot("local").unwrap().outcome_unknown);
    assert!(rt.enqueue(&id, "retry dropped effects").is_err());
    drop(rt);
    let (reopened, provider) = open(&temp, settings, "return 1", None);
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(provider.calls.load(Ordering::Acquire), 0);
    assert_eq!(state.count(), 1);
}

#[tokio::test]
async fn failed_bridge_terminal_commit_keeps_prepare_unknown_and_holds_capacity() {
    let temp = setup();
    let state = SourceState::new();
    let registry = registry(
        state.clone(),
        Behavior::Reply,
        true,
        Limits::default(),
        None,
    );
    let mut settings = config(&registry, &calls());
    settings.max_concurrent_runs = 1;
    settings.max_queued_runs = 1;
    let (rt, _) = open(
        &temp,
        settings.clone(),
        "return cayw.call('mcp.call',{server='local',tool='echo',arguments={text='fixture'}})",
        None,
    );
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_bridge_terminal BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='runtime_bridge_settled' BEGIN SELECT RAISE(ABORT,'injected Bridge terminal failure'); END;").unwrap();
    let error = rt
        .prompt(&id, "failed terminal persistence", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected Bridge terminal failure"),
        "{error}"
    );
    assert_eq!(state.count(), 1);
    assert_eq!(rt.health().unwrap().running, 1);
    assert!(registry.journal_failure().is_some());
    assert_eq!(history(&rt, &id).len(), 1);
    assert!(history(&rt, &id)[0].outcome.is_none());
    db.execute_batch("DROP TRIGGER fail_bridge_terminal;")
        .unwrap();
    drop(rt);
    let (reopened, _) = open(&temp, settings, "return 1", None);
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(state.count(), 1);
    assert!(reopened.enqueue(&id, "retry missing terminal").is_err());
}

#[test]
fn oversized_bindings_are_rejected_before_task_or_source_admission() {
    let temp = setup();
    let state = SourceState::new();
    let registry = Registry::new(Limits::default(), Arc::new(Policy(true))).unwrap();
    for index in 0..12 {
        let mut identity = identity();
        identity.id = format!("source-{index}");
        identity.origin = "x".repeat(2048);
        let mut manifest = manifest();
        manifest.search = index == 0;
        registry
            .register(
                identity,
                manifest,
                Box::new(Source {
                    key: format!("handle-{index}"),
                    state: state.clone(),
                    behavior: Behavior::Reply,
                    db: None,
                }),
            )
            .unwrap();
    }
    let settings = config(&registry, &calls());
    let provider = Arc::new(Script {
        script: "return 1".into(),
        calls: AtomicUsize::new(0),
        second: None,
    });
    let result = Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        provider.clone(),
        Model::new("fixture", "bounded-pins"),
        settings,
    );
    assert!(matches!(
        result,
        Err(cayw_runtime::RuntimeError::InvalidInput(_))
    ));
    assert_eq!(state.count(), 0);
    assert_eq!(provider.calls.load(Ordering::Acquire), 0);
    assert!(!temp.path().join("session.db").exists());
}
