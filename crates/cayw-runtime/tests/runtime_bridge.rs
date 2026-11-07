use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall,
};
use cayw_bridge::{
    AccessRequest, DispatchOutcome, Limits, Manifest, Provenance, Registry, RegistryPolicy,
    SkillSpec, SourceHandle, SourceIdentity, SourceOperation, SourceReply, SourceRequest, ToolSpec,
    Visibility,
};
use cayw_codemode::CapabilityError;
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityReconciliation, CapabilityStage, CoreEvent,
    SessionState,
};
use cayw_runtime::{
    ApprovalChoice, BridgeDispatchEvent, RequestPolicy, Runtime, RuntimeConfig, RuntimeError,
    ScopedPermission, TaskState,
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

const SKILL: &str = "fixture guidance from the one existing host loader";
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
struct SourceState {
    calls: Mutex<Vec<SourceRequest>>,
    disconnected: AtomicUsize,
    entered: Notify,
    release: Semaphore,
}
impl Default for SourceState {
    fn default() -> Self {
        Self {
            calls: Mutex::default(),
            disconnected: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Semaphore::new(0),
        }
    }
}
#[derive(Clone, Copy)]
enum Behavior {
    Reply,
    Block,
    Fail,
    Oversize,
}
struct Source {
    state: Arc<SourceState>,
    behavior: Behavior,
    owner: String,
}
#[async_trait]
impl SourceHandle for Source {
    fn ownership_key(&self) -> &str {
        &self.owner
    }
    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        self.state.calls.lock().unwrap().push(request.clone());
        self.state.entered.notify_one();
        if matches!(self.behavior, Behavior::Block) {
            tokio::select! {
                _ = cancel.cancelled() => return Err(CapabilityError::unknown("fixture source stopped without confirmed effect outcome")),
                permit = self.state.release.acquire() => permit.unwrap().forget(),
            }
        }
        if matches!(self.behavior, Behavior::Fail) {
            return Err(CapabilityError::new("fixture source confirmed failure"));
        }
        let value = if matches!(self.behavior, Behavior::Oversize) {
            json!("x".repeat(32 * 1024))
        } else {
            match &request.operation {
                SourceOperation::InvokeTool { arguments, .. } => json!({"echo":arguments}),
                SourceOperation::ReadSkill { .. } => json!(SKILL),
                SourceOperation::ReadResource { uri } => {
                    json!({"uri":uri,"mime_type":"text/markdown","text":"fixture resource body"})
                }
                SourceOperation::Search { arguments } => json!({"hits":[arguments["query"]]}),
                SourceOperation::GetPrompt { name, arguments } => {
                    json!({"name": name, "description": "fixture prompt", "arguments": arguments, "messages": []})
                }
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
fn manifest() -> Manifest {
    Manifest {
        tools: vec![ToolSpec {
            name: "echo".into(),
            description: "local fixture".into(),
            input_schema: json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}),
            output_schema: None,
        }],
        skills: vec![SkillSpec {
            name: "guide".into(),
            description: "fixture skill".into(),
            content_sha256: cayw_bridge::content_sha256(SKILL),
        }],
        resources: Vec::new(),
        prompts: Vec::new(),
        search: true,
    }
}
fn identity(version: &str) -> SourceIdentity {
    SourceIdentity {
        id: "local".into(),
        version: version.into(),
        origin: "injected local fixture handle".into(),
    }
}
fn registry_with(behavior: Behavior, limits: Limits) -> (Registry, Arc<SourceState>) {
    let registry = Registry::new(limits, Arc::new(Policy)).unwrap();
    let state = Arc::new(SourceState {
        release: Semaphore::new(0),
        ..Default::default()
    });
    registry
        .register(
            identity("1"),
            manifest(),
            Box::new(Source {
                state: state.clone(),
                behavior,
                owner: "physical-fixture-one".into(),
            }),
        )
        .unwrap();
    (registry, state)
}
fn registry(behavior: Behavior) -> (Registry, Arc<SourceState>) {
    registry_with(behavior, Limits::default())
}

struct ConfirmedSourceReceipt(CapabilityReconciliation);
impl cayw_core::ReconciliationVerifier for ConfirmedSourceReceipt {
    fn verify(&self, request: &cayw_core::ReconciliationRequest) -> Result<(), String> {
        match request {
            cayw_core::ReconciliationRequest::Capability { reconciliation }
                if reconciliation == &self.0 && reconciliation.actor == "fixture-host" =>
            {
                Ok(())
            }
            _ => Err("request differs from the inspected original fixture source receipt".into()),
        }
    }
}

struct Script {
    text: String,
    requests: AtomicUsize,
    pause: Option<Arc<Semaphore>>,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            &self.text,
            &format!("fixture-{}", request.messages.len()),
        );
        let pause = if self.requests.fetch_add(1, Ordering::AcqRel) == 1 {
            self.pause.clone()
        } else {
            None
        };
        Box::pin(async_stream::stream! {
            if let Some(pause) = pause { pause.acquire_owned().await.unwrap().forget(); }
            yield AssistantEvent::Done { message };
        })
    }
}
fn provider(script: &str) -> Arc<Script> {
    Arc::new(Script {
        text: script.into(),
        requests: AtomicUsize::new(0),
        pause: None,
    })
}
fn setup() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    temp
}
fn config(registry: &Registry) -> RuntimeConfig {
    RuntimeConfig {
        bridge: Some(registry.consumer()),
        max_run_time: Duration::from_secs(15),
        ..Default::default()
    }
}
fn open(temp: &tempfile::TempDir, config: RuntimeConfig, provider: Arc<dyn Provider>) -> Runtime {
    Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        provider,
        Model::new("fixture", "runtime-bridge"),
        config,
    )
    .unwrap()
}
fn allow(config: &mut RuntimeConfig, name: &str, args: Value) {
    config.permissions.push(ScopedPermission::exact(name, args));
}
fn call_args(value: u64) -> Value {
    json!({"server":"local","tool":"echo","arguments":{"value":value}})
}
fn dispatches(rt: &Runtime, id: &str) -> Vec<BridgeDispatchEvent> {
    let mut cursor = 0;
    let mut events = Vec::new();
    loop {
        let page = rt.bridge_dispatches(id, cursor, 3).unwrap();
        if cursor == page.next_cursor {
            break;
        }
        cursor = page.next_cursor;
        events.extend(page.events);
    }
    events
}
fn settled(rt: &Runtime, id: &str) -> Vec<CapabilityCallRecord> {
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
async fn pending(rt: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(approval) = rt.get(id).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[test]
fn bridge_is_optional_and_conflicting_raw_service_is_rejected() {
    let temp = setup();
    let rt = open(&temp, RuntimeConfig::default(), provider("return 1"));
    assert!(
        !rt.capabilities()
            .unwrap()
            .iter()
            .any(|tool| tool.name == "mcp.call")
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
            Ok(json!(null))
        }
    }
    let (registry, _) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    settings.host.external = Some(Arc::new(Raw));
    let error = Runtime::open(
        temp.path().join("conflict.db"),
        temp.path().join("workspace"),
        provider("return 1"),
        Model::new("fixture", "conflict"),
        settings,
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("ambiguous"));
    assert!(!temp.path().join("conflict.db").exists());
}

#[tokio::test]
async fn all_six_external_operations_use_existing_consumer_through_luau() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    for (name, args) in [
        ("mcp.discover", json!({})),
        ("mcp.describe", json!({"server":"local","tool":"echo"})),
        ("skills.list", json!({})),
        ("skills.read", json!({"name":"local/guide"})),
        ("mcp.call", call_args(7)),
        ("search.query", json!({"query":"needle"})),
    ] {
        allow(&mut settings, name, args);
    }
    let pins = registry.consumer().binding_snapshot();
    let rt = open(
        &temp,
        settings.clone(),
        provider(
            "local d=cayw.call('mcp.discover', {})\nlocal t=cayw.call('mcp.describe', {server='local',tool='echo'})\nlocal l=cayw.call('skills.list', {})\nlocal s=cayw.call('skills.read', {name='local/guide'})\nlocal v=cayw.call('mcp.call', {server='local',tool='echo',arguments={value=7}})\nlocal q=cayw.call('search.query', {query='needle'})\nreturn {discovered=d,described=t,skills=l,skill=s,invoked=v,search=q}",
        ),
    );
    assert!(
        rt.capabilities()
            .unwrap()
            .iter()
            .any(|tool| tool.name == "mcp.call")
    );
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "run fixture", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert_eq!(state.calls.lock().unwrap().len(), 3);
    assert_eq!(state.disconnected.load(Ordering::Acquire), 0);
    let calls = settled(&rt, &id);
    assert_eq!(calls.len(), 6);
    assert!(
        calls
            .iter()
            .all(|call| matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })))
    );
    for call in &calls {
        assert_eq!(
            call.external_provenance,
            Some(serde_json::to_value(&pins).unwrap())
        );
    }
    let entries = dispatches(&rt, &id);
    assert_eq!(entries.len(), 6);
    assert_eq!(
        entries
            .iter()
            .filter(|event| event.outcome.is_some())
            .count(),
        3
    );
    for event in &entries {
        let call = calls
            .iter()
            .find(|call| call.key == event.origin.key)
            .unwrap();
        assert_eq!(event.origin.lease, call.lease);
        assert_eq!(event.origin.task_id, call.task_id);
        assert_eq!(event.origin.parent_call_id, call.parent_call_id);
        assert_eq!(event.origin.source_hash, call.source_hash);
        assert_eq!(event.origin.bindings, pins);
        assert_eq!(event.record.context.sequence, call.key.sequence + 1);
        assert_eq!(event.record.key.source, pins.sources["local"]);
    }
    assert_eq!(rt.recover(&id).unwrap().state, SessionState::Idle);
    drop(rt);
    let reopened = open(&temp, settings, provider("return 1"));
    assert_eq!(dispatches(&reopened, &id).len(), 6);
    assert_eq!(state.calls.lock().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denial_reaches_no_source_and_does_not_create_a_dispatch() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let rt = open(
        &temp,
        config(&registry),
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(&worker_id, "denied fixture", CancellationToken::new())
            .await
    });
    let approval = pending(&rt, &id).await;
    rt.approve(&id, &approval.id, ApprovalChoice::Deny, "fixture-host")
        .unwrap();
    assert_eq!(worker.await.unwrap().unwrap().state, SessionState::Idle);
    assert!(state.calls.lock().unwrap().is_empty());
    assert!(dispatches(&rt, &id).is_empty());
    assert!(matches!(
        settled(&rt, &id)[0].outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
}

#[tokio::test]
async fn all_scopes_share_host_call_budget() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    settings.host.max_calls = 1;
    allow(&mut settings, "mcp.discover", json!({}));
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings,
        provider(
            "cayw.call('mcp.discover',{})\nreturn cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})",
        ),
    );
    let id = rt.create(None).unwrap().id;
    assert!(matches!(
        rt.prompt(&id, "bounded fixture", CancellationToken::new()).await,
        Err(RuntimeError::Worker(ref reason)) if reason == "host call or total byte budget exhausted"
    ));
    let view = rt.get(&id).unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_calls.is_empty());
    assert!(view.unresolved_capabilities.is_empty());
    let tasks = rt.tasks(&id).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::Failed);
    assert!(!tasks[0].cancel_requested);
    let terminals = rt
        .audit(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.task_id.as_deref() == Some(tasks[0].id.as_str()) && event.kind.starts_with("run_")
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].kind, "run_failed");
    assert!(state.calls.lock().unwrap().is_empty());
    let calls = settled(&rt, &id);
    assert_eq!(calls.len(), 2);
    assert!(matches!(
        calls[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(matches!(
        calls[1].outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
    assert!(dispatches(&rt, &id).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_cannot_reroute_a_task_waiting_on_its_original_permission() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let settings = config(&registry);
    let rt = open(
        &temp,
        settings,
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(&worker_id, "old generation", CancellationToken::new())
            .await
    });
    let approval = pending(&rt, &id).await;
    registry.disconnect("local").unwrap();
    registry
        .reconnect(
            identity("2"),
            manifest(),
            Box::new(Source {
                state: state.clone(),
                behavior: Behavior::Reply,
                owner: "physical-fixture-two".into(),
            }),
        )
        .unwrap();
    rt.approve(&id, &approval.id, ApprovalChoice::AllowOnce, "fixture-host")
        .unwrap();
    assert_eq!(worker.await.unwrap().unwrap().state, SessionState::Idle);
    assert!(state.calls.lock().unwrap().is_empty());
    assert!(dispatches(&rt, &id).is_empty());
    assert_eq!(state.disconnected.load(Ordering::Acquire), 1);
    assert!(matches!(
        settled(&rt, &id)[0].outcome,
        Some(CapabilityOutcome::Failed { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_preserves_unknown_dispatch_across_reopen_and_original_registry_stays_fenced()
{
    let temp = setup();
    let (registry, state) = registry(Behavior::Block);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings.clone(),
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let id = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&id, "cancel fixture").unwrap();
    let worker_rt = rt.clone();
    let task_id = admitted.id.clone();
    let worker =
        tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
    tokio::time::timeout(Duration::from_secs(8), state.entered.notified())
        .await
        .unwrap();
    rt.cancel(&rt.task(&admitted.id).unwrap().cancel_target())
        .unwrap();
    let _ = worker.await.unwrap();
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    let records = dispatches(&rt, &id);
    assert!(matches!(
        records.last().unwrap().outcome,
        Some(DispatchOutcome::Unknown { .. })
    ));
    assert!(registry.snapshot("local").unwrap().outcome_unknown);
    assert!(rt.enqueue(&id, "retry").is_err());
    let call = rt.get(&id).unwrap().unresolved_capabilities[0].clone();
    let pins = call.external_provenance.clone();
    // The original source future has joined. Its fixture performs no effect
    // before release, and this run never received a release permit.
    {
        let requests = state.calls.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].key, records.last().unwrap().record.key);
        assert!(matches!(&requests[0].operation,
            SourceOperation::InvokeTool { name, arguments }
                if name == "echo" && arguments == &json!({"value":1})));
        assert_eq!(state.release.available_permits(), 0);
    }
    let receipt = CapabilityReconciliation {
        key: call.key.clone(),
        expected: (&call).into(),
        actor: "fixture-host".into(),
        evidence: "original source future joined; blocked fixture had no release or effect".into(),
        outcome: CapabilityOutcome::NotExecuted {
            reason: "fixture confirmation".into(),
        },
        external_provenance: pins.clone(),
    };
    settings.reconciliation_verifier = Some(Arc::new(ConfirmedSourceReceipt(receipt.clone())));
    drop(rt);
    let reopened = open(&temp, settings, provider("return 1"));
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(reopened.enqueue(&id, "retry").is_err());
    let mut bad = pins.clone().unwrap();
    bad["sources"]["local"]["source"]["origin"] = json!("altered origin");
    let wrong = CapabilityReconciliation {
        key: call.key.clone(),
        expected: (&call).into(),
        actor: "fixture-host".into(),
        evidence: "wrong provenance must fail".into(),
        outcome: CapabilityOutcome::NotExecuted {
            reason: "fixture verification".into(),
        },
        external_provenance: Some(bad),
    };
    assert!(reopened.reconcile_capability(&id, wrong).is_err());
    let mut forged = receipt.clone();
    forged.expected.task_id = "replacement-task".into();
    assert!(reopened.reconcile_capability(&id, forged).is_err());
    reopened.reconcile_capability(&id, receipt).unwrap();
    assert_eq!(state.calls.lock().unwrap().len(), 1);
    assert!(registry.snapshot("local").unwrap().outcome_unknown);
    assert!(matches!(
        dispatches(&reopened, &id).last().unwrap().outcome,
        Some(DispatchOutcome::Unknown { .. })
    ));
}

#[tokio::test]
async fn preparation_commit_failure_dispatches_zero_calls_and_holds_admission() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    settings.max_concurrent_runs = 1;
    settings.max_queued_runs = 1;
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings,
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_bridge_prepare BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='runtime_bridge_prepared' BEGIN SELECT RAISE(ABORT,'injected bridge preparation failure'); END;").unwrap();
    let id = rt.create(None).unwrap().id;
    let error = rt
        .prompt(&id, "fail preparation", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected bridge preparation failure"),
        "{error}"
    );
    assert!(state.calls.lock().unwrap().is_empty());
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Draining);
    assert_eq!(rt.health().unwrap().running, 1);
    let events = dispatches(&rt, &id);
    // The prepared guard may persist an atomic first NotExecuted after the
    // failed preparation. It must never claim Unknown without a preparation.
    assert_eq!(events.len(), 1);
    assert!(
        events
            .iter()
            .all(|event| matches!(event.outcome, Some(DispatchOutcome::NotExecuted { .. })))
    );
    db.execute_batch("DROP TRIGGER fail_bridge_prepare;")
        .unwrap();
}

#[tokio::test]
async fn durable_source_terminal_repairs_missing_core_terminal_without_replay() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    let model =
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})");
    let rt = open(&temp, settings.clone(), model.clone());
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_bridge_core_terminal BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='capability_settled' BEGIN SELECT RAISE(ABORT,'injected Core terminal failure'); END;").unwrap();
    let id = rt.create(None).unwrap().id;
    let error = rt
        .prompt(&id, "lose final audit", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("injected Core terminal failure"),
        "{error}"
    );
    assert_eq!(state.calls.lock().unwrap().len(), 1);
    assert!(matches!(
        dispatches(&rt, &id).last().unwrap().outcome,
        Some(DispatchOutcome::Succeeded { .. })
    ));
    assert_eq!(
        rt.get(&id).unwrap().unresolved_capabilities[0].stage,
        CapabilityStage::Started
    );
    let model_count = model.requests.load(Ordering::Acquire);
    db.execute_batch("DROP TRIGGER fail_bridge_core_terminal;")
        .unwrap();
    drop(rt);
    let reopened = open(&temp, settings, model.clone());
    assert_eq!(reopened.recover(&id).unwrap().state, SessionState::Idle);
    assert!(
        reopened
            .get(&id)
            .unwrap()
            .unresolved_capabilities
            .is_empty()
    );
    assert!(
        settled(&reopened, &id)
            .iter()
            .any(|call| matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })))
    );
    assert_eq!(reopened.recover(&id).unwrap().state, SessionState::Idle);
    assert_eq!(state.calls.lock().unwrap().len(), 1);
    assert_eq!(model.requests.load(Ordering::Acquire), model_count);
}

#[tokio::test]
async fn unverifiable_bridge_child_cannot_hide_the_unknown_parent_after_a_recording_failure() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings,
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    // Deliberately bypass the database append-only guard in this corruption
    // fixture. Production writes cannot rewrite a committed source origin.
    db.execute_batch(
        "DROP TRIGGER core_events_no_update;
        CREATE TRIGGER corrupt_bridge_origin AFTER INSERT ON core_events
        WHEN json_extract(NEW.record,'$.event.kind')='runtime_bridge_settled'
        BEGIN
            UPDATE core_events SET record=json_set(record,'$.event.data.origin.source_hash','forged-source-hash')
            WHERE json_extract(record,'$.event.kind')='runtime_bridge_prepared';
        END;
        CREATE TRIGGER reject_core_terminal BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record,'$.event.type')='capability_settled'
        BEGIN SELECT RAISE(ABORT,'injected Core terminal failure'); END;",
    ).unwrap();
    let id = rt.create(None).unwrap().id;
    assert!(
        rt.prompt(&id, "retain unverifiable parent", CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(state.calls.lock().unwrap().len(), 1);
    let snapshot = rt.get(&id).unwrap();
    assert_eq!(snapshot.state, SessionState::NeedsReconciliation);
    assert_eq!(snapshot.unresolved_capabilities.len(), 1);
    let child = &snapshot.unresolved_capabilities[0];
    assert_eq!(child.stage, CapabilityStage::Started);
    let bridge_events = rt.bridge_dispatches(&id, 0, 1000);
    assert!(
        bridge_events.is_err(),
        "corrupted Bridge events accepted: {bridge_events:#?}"
    );
    assert!(
        snapshot
            .unresolved_calls
            .iter()
            .any(|parent| parent.call.id == child.parent_call_id),
        "original parent lost from snapshot: {snapshot:#?}"
    );
    db.execute_batch("DROP TRIGGER reject_core_terminal;")
        .unwrap();
    assert!(rt.recover(&id).is_err());
    assert_eq!(state.calls.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_bind_bridge_incarnation_and_revalidate_the_original_host_catalog() {
    use cayw_requests::{OperationScope, RequestState};
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    settings.request_policy = Some(RequestPolicy {
        permission_policy_id: "fixed-host-policy".into(),
        operations: vec![OperationScope::exact(
            "echo-once",
            "mcp.call",
            call_args(1),
            "host-fixture",
        )],
    });
    let pause = Arc::new(Semaphore::new(0));
    let model = Arc::new(Script {
        text: "local request={purpose='echo one fixture value',gap='exact echo unavailable',operation_id='echo-once',capability='mcp.call',arguments={server='local',tool='echo',arguments={value=1}}}\ncayw.call('capability.request',request)\ncayw.call('capability.request',request)\nyield_control()\nreturn cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})".into(),
        requests: AtomicUsize::new(0), pause: Some(pause.clone()),
    });
    let rt = open(&temp, settings.clone(), model);
    assert_eq!(rt.request_operations().unwrap().len(), 1);
    let id = rt.create(None).unwrap().id;
    let task = rt.enqueue(&id, "request echo").unwrap();
    let worker_rt = rt.clone();
    let task_id = task.id.clone();
    let worker =
        tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
    let request = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(events) = rt.request_events(&task.id, 0, 64) {
                for event in events.into_iter().rev() {
                    if let Some(request_id) = event.request_id {
                        let record = rt.request_status(&task.id, &request_id).unwrap();
                        if record.state == RequestState::AwaitingEvaluation {
                            return record;
                        }
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(state.calls.lock().unwrap().is_empty());
    let ticket = rt
        .request_evaluate(&task.id, &request.id, "fixture-host")
        .unwrap();
    // Same workspace, budgets, raw policy ID, schemas and arguments; only the
    // host-created Registry lifetime differs. Its catalog cannot approve this.
    let (other_registry, other_state) = registry_with(Behavior::Reply, Limits::default());
    let mut replacement = settings.clone();
    replacement.bridge = Some(other_registry.consumer());
    let other = open(&temp, replacement, provider("return 1"));
    assert!(
        other
            .request_approve(&task.id, &ticket, Duration::from_secs(5), "new incarnation")
            .is_err()
    );
    assert!(other_state.calls.lock().unwrap().is_empty());
    rt.request_approve(
        &task.id,
        &ticket,
        Duration::from_secs(5),
        "verified original consumer",
    )
    .unwrap();
    pause.add_permits(1);
    assert_eq!(worker.await.unwrap().unwrap().state, SessionState::Idle);
    assert_eq!(state.calls.lock().unwrap().len(), 1);
    let calls = settled(&rt, &id);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.name == "capability.request")
            .count(),
        2
    );
    assert!(calls.iter().any(|call| call.name == "mcp.call"
        && matches!(call.outcome, Some(CapabilityOutcome::Completed { .. }))));
    assert!(rt.events(&id, 0, 1000).unwrap().iter().any(|event| matches!(&event.event, CoreEvent::Host {kind,data} if kind == "task_request_admitted" && data["receipt"]["request_id"] == request.id)));
}

struct InterleavedProvider {
    step: AtomicUsize,
    cells: Mutex<Vec<(String, u64)>>,
}
impl Provider for InterleavedProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let step = self.step.fetch_add(1, Ordering::AcqRel);
        if let Some(Message::ToolResult(result)) = request.messages.last()
            && let Some(value) = &result.structured_content
            && let Ok(page) = serde_json::from_value::<cayw_cells::CellPage>(value.clone())
        {
            let mut cells = self.cells.lock().unwrap();
            if let Some(entry) = cells.iter_mut().find(|(id, _)| id == &page.cell_id) {
                entry.1 = page.next_cursor;
            } else {
                cells.push((page.cell_id, page.next_cursor));
            }
        }
        let args = match step {
            0 => Some(
                json!({"action":"exec","script":"cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})\nyield_control()\nreturn cayw.call('mcp.call',{server='local',tool='echo',arguments={value=3}})","yield_time_ms":30000,"max_output_tokens":131072}),
            ),
            1 => Some(
                json!({"action":"exec","script":"cayw.call('mcp.call',{server='local',tool='echo',arguments={value=2}})\nyield_control()\nreturn cayw.call('mcp.call',{server='local',tool='echo',arguments={value=4}})","yield_time_ms":30000,"max_output_tokens":131072}),
            ),
            2 | 3 => {
                let cells = self.cells.lock().unwrap();
                let (cell_id, cursor) = &cells[step - 2];
                Some(
                    json!({"action":"wait","cell_id":cell_id,"cursor":cursor,"yield_time_ms":30000,"max_output_tokens":131072}),
                )
            }
            _ => None,
        };
        let mut message = AssistantMessage::new(&request.model);
        // Fixed synthetic counters exercise accounting, not tokenization or billing.
        message.usage.input = 4;
        message.usage.output = 4;
        message.usage.update_total();
        message.usage_status = cayw_ai::UsageStatus::Reported;
        if let Some(args) = args {
            message.content.push(Content::ToolCall(ToolCall::new(
                format!("interleaved-{step}"),
                "execute_code",
                args,
            )));
            message.stop_reason = StopReason::ToolUse;
        } else {
            message
                .content
                .push(Content::text("both original cells complete"));
            message.stop_reason = StopReason::Stop;
        }
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_interleaved_cells_retain_distinct_origins_on_one_physical_handle() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    for value in 1..=4 {
        allow(&mut settings, "mcp.call", call_args(value));
    }
    let model = Arc::new(InterleavedProvider {
        step: AtomicUsize::new(0),
        cells: Mutex::default(),
    });
    let rt = open(&temp, settings, model.clone());
    let id = rt.create(None).unwrap().id;
    assert_eq!(
        rt.prompt(&id, "interleave cells", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    let events = dispatches(&rt, &id)
        .into_iter()
        .filter(|event| event.outcome.is_some())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 4);
    let cells = model
        .cells
        .lock()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    assert_eq!(cells.len(), 2);
    assert_eq!(events[0].origin.key.execution_id, cells[0]);
    assert_eq!(events[1].origin.key.execution_id, cells[1]);
    assert_eq!(events[2].origin.key.execution_id, cells[0]);
    assert_eq!(events[3].origin.key.execution_id, cells[1]);
    assert_ne!(
        events[0].origin.parent_call_id,
        events[1].origin.parent_call_id
    );
    assert_ne!(events[0].origin.source_hash, events[1].origin.source_hash);
    assert_eq!(events[0].record.context.sequence, 1);
    assert_eq!(events[1].record.context.sequence, 1);
    assert_eq!(events[2].record.context.sequence, 2);
    assert_eq!(events[3].record.context.sequence, 2);
    for event in &events {
        assert_eq!(event.origin.lease, events[0].origin.lease);
        assert_eq!(event.origin.task_id, events[0].origin.task_id);
        assert_eq!(event.origin.bindings, events[0].origin.bindings);
    }
    assert_eq!(state.calls.lock().unwrap().len(), 4);
    assert_eq!(state.disconnected.load(Ordering::Acquire), 0);
    assert_eq!(rt.recover(&id).unwrap().state, SessionState::Idle);
}

#[tokio::test]
async fn known_source_success_does_not_replace_explicit_core_unknown_from_host_result_bound() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    settings.host.max_result_bytes = 32;
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings.clone(),
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "small Host result", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::NeedsReconciliation);
    assert!(matches!(
        dispatches(&rt, &id).last().unwrap().outcome,
        Some(DispatchOutcome::Succeeded { .. })
    ));
    assert!(matches!(
        view.unresolved_capabilities[0].outcome,
        Some(CapabilityOutcome::OutcomeUnknown { .. })
    ));
    assert_eq!(
        rt.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    drop(rt);
    let reopened = open(&temp, settings, provider("return 1"));
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(state.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_source_terminal_write_retains_preparation_and_unknown_without_replay() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings.clone(),
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_bridge_settlement BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='runtime_bridge_settled' BEGIN SELECT RAISE(ABORT,'injected source settlement failure'); END;").unwrap();
    let id = rt.create(None).unwrap().id;
    let error = rt
        .prompt(&id, "lose source terminal", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected source settlement failure"),
        "{error}"
    );
    let evidence = dispatches(&rt, &id);
    assert_eq!(evidence.len(), 1);
    assert!(evidence[0].outcome.is_none());
    assert_eq!(state.calls.lock().unwrap().len(), 1);
    assert!(registry.journal_failure().is_some());
    assert!(registry.snapshot("local").unwrap().outcome_unknown);
    db.execute_batch("DROP TRIGGER fail_bridge_settlement;")
        .unwrap();
    assert_eq!(
        rt.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    drop(rt);
    let reopened = open(&temp, settings, provider("return 1"));
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(reopened.enqueue(&id, "retry").is_err());
    assert_eq!(state.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn changed_execution_projection_is_rejected_before_known_dispatch_recovery() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings,
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_core_terminal_for_drift BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='capability_settled' BEGIN SELECT RAISE(ABORT,'injected lost terminal for drift'); END;").unwrap();
    let id = rt.create(None).unwrap().id;
    assert!(
        rt.prompt(&id, "projection drift", CancellationToken::new())
            .await
            .is_err()
    );
    db.execute_batch("DROP TRIGGER fail_core_terminal_for_drift;")
        .unwrap();
    db.execute("UPDATE core_sessions SET projection=json_set(projection,'$.executions[0].source_hash','altered-source-hash') WHERE id=?1",[&id]).unwrap();
    assert!(rt.bridge_dispatches(&id, 0, 1000).is_err());
    assert!(rt.recover(&id).is_err());
    assert_eq!(state.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn recovery_retries_exact_known_source_terminal_after_reconciliation_commit_failure() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Fail);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings.clone(),
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_core_terminal_retry BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='capability_settled' BEGIN SELECT RAISE(ABORT,'injected lost known failure'); END;").unwrap();
    let id = rt.create(None).unwrap().id;
    assert!(
        rt.prompt(&id, "known failure recovery", CancellationToken::new())
            .await
            .is_err()
    );
    assert!(matches!(
        dispatches(&rt, &id).last().unwrap().outcome,
        Some(DispatchOutcome::Failed { .. })
    ));
    db.execute_batch("DROP TRIGGER fail_core_terminal_retry; CREATE TRIGGER fail_reconcile_retry BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='capability_reconciled' BEGIN SELECT RAISE(ABORT,'injected reconciliation commit failure'); END;").unwrap();
    assert!(
        rt.recover(&id)
            .unwrap_err()
            .to_string()
            .contains("injected reconciliation commit failure")
    );
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(state.calls.lock().unwrap().len(), 1);
    db.execute_batch("DROP TRIGGER fail_reconcile_retry;")
        .unwrap();
    drop(rt);
    let reopened = open(&temp, settings, provider("return 1"));
    assert_eq!(reopened.recover(&id).unwrap().state, SessionState::Idle);
    assert!(settled(&reopened,&id).iter().any(|call|matches!(&call.outcome,Some(CapabilityOutcome::Failed { error }) if error == "fixture source confirmed failure")));
    assert_eq!(state.calls.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_cancels_original_source_and_keeps_unknown_after_reopen() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Block);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings.clone(),
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(&worker_id, "revoke source", CancellationToken::new())
            .await
    });
    tokio::time::timeout(Duration::from_secs(8), state.entered.notified())
        .await
        .unwrap();
    registry.revoke("local").unwrap();
    let _ = worker.await.unwrap();
    assert_eq!(
        rt.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(matches!(
        dispatches(&rt, &id).last().unwrap().outcome,
        Some(DispatchOutcome::Unknown { .. })
    ));
    assert!(registry.snapshot("local").unwrap().outcome_unknown);
    assert_eq!(state.disconnected.load(Ordering::Acquire), 1);
    drop(rt);
    let reopened = open(&temp, settings, provider("return 1"));
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(state.calls.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_sessions_keep_full_task_attempt_worker_origins_on_one_registry() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings,
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let left = rt.create(None).unwrap().id;
    let right = rt.create(None).unwrap().id;
    let (a, b) = tokio::join!(
        rt.prompt(&left, "left original task", CancellationToken::new()),
        rt.prompt(&right, "right original task", CancellationToken::new())
    );
    assert_eq!(a.unwrap().state, SessionState::Idle);
    assert_eq!(b.unwrap().state, SessionState::Idle);
    let a = dispatches(&rt, &left);
    let b = dispatches(&rt, &right);
    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 2);
    assert_eq!(a[0].record.context.session_id, left);
    assert_eq!(b[0].record.context.session_id, right);
    assert_ne!(a[0].origin.lease, b[0].origin.lease);
    assert_ne!(a[0].origin.task_id, b[0].origin.task_id);
    assert_ne!(a[0].origin.key, b[0].origin.key);
    // Parent tool IDs are session-local; the full origin is still distinct
    // even when this fixed Provider uses the same ID in both conversations.
    assert_ne!(
        (&a[0].origin.lease.session_id, &a[0].origin.parent_call_id),
        (&b[0].origin.lease.session_id, &b[0].origin.parent_call_id)
    );
    assert_ne!(a[0].record.key.id, b[0].record.key.id);
    assert_eq!(a[0].origin.bindings, b[0].origin.bindings);
    assert_eq!(state.calls.lock().unwrap().len(), 2);
    assert_eq!(state.disconnected.load(Ordering::Acquire), 0);
}

#[test]
fn oversized_pinned_binding_catalog_is_rejected_before_opening_or_dispatching() {
    let temp = setup();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    let state = Arc::new(SourceState::default());
    for index in 0..10 {
        registry
            .register(
                SourceIdentity {
                    id: format!("source-{index}"),
                    version: "1".into(),
                    origin: "x".repeat(2000),
                },
                Manifest::default(),
                Box::new(Source {
                    state: state.clone(),
                    behavior: Behavior::Reply,
                    owner: format!("physical-{index}"),
                }),
            )
            .unwrap();
    }
    let error = Runtime::open(
        temp.path().join("large.db"),
        temp.path().join("workspace"),
        provider("return 1"),
        Model::new("fixture", "large-pins"),
        config(&registry),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("metadata budget"), "{error}");
    assert!(!temp.path().join("large.db").exists());
    assert!(state.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn fabricated_host_event_title_cannot_be_used_as_bridge_evidence() {
    struct Forgery {
        db: std::path::PathBuf,
        written: std::sync::atomic::AtomicBool,
    }
    impl Provider for Forgery {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            if !self.written.swap(true, Ordering::AcqRel) {
                let db = rusqlite::Connection::open(&self.db).unwrap();
                let projection: String = db
                    .query_row("SELECT projection FROM core_sessions LIMIT 1", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                let projection: Value = serde_json::from_str(&projection).unwrap();
                let lease = serde_json::from_value::<cayw_core::AttemptLease>(
                    projection["attempt"].clone(),
                )
                .unwrap();
                cayw_core::SessionStore::open(&self.db)
                    .unwrap()
                    .append_host_event(
                        &lease,
                        "runtime_bridge_settled",
                        json!({"plausible_title":true}),
                    )
                    .unwrap();
            }
            let mut message = AssistantMessage::new(&request.model);
            // The event corruption is deliberate; the model reply itself succeeds.
            message.usage.input = 4;
            message.usage.output = 4;
            message.usage.update_total();
            message.usage_status = cayw_ai::UsageStatus::Reported;
            message
                .content
                .push(Content::text("fixture forge inserted"));
            message.stop_reason = StopReason::Stop;
            Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
                message,
            }]))
        }
    }
    let temp = setup();
    let (registry, state) = registry(Behavior::Reply);
    let rt = open(
        &temp,
        config(&registry),
        Arc::new(Forgery {
            db: temp.path().join("session.db"),
            written: std::sync::atomic::AtomicBool::new(false),
        }),
    );
    let id = rt.create(None).unwrap().id;
    rt.prompt(
        &id,
        "host title is not dispatch proof",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(rt.bridge_dispatches(&id, 0, 1000).is_err());
    assert!(rt.recover(&id).is_err());
    assert!(state.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_dispatch_journal_cannot_write_after_its_full_lease_is_fenced() {
    let temp = setup();
    let (registry, state) = registry(Behavior::Block);
    let mut settings = config(&registry);
    allow(&mut settings, "mcp.call", call_args(1));
    let rt = open(
        &temp,
        settings.clone(),
        provider("return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})"),
    );
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(
                &worker_id,
                "fence old source owner",
                CancellationToken::new(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(8), state.entered.notified())
        .await
        .unwrap();
    // Deliberately call the lower-level Core API to simulate an external owner
    // fence. Runtime's normal recovery requires the original worker to stop.
    rt.store().recover(&id).unwrap();
    state.release.add_permits(1);
    assert!(worker.await.unwrap().is_err());
    let events = dispatches(&rt, &id);
    assert_eq!(events.len(), 1);
    assert!(events[0].outcome.is_none());
    assert_eq!(state.calls.lock().unwrap().len(), 1);
    assert!(registry.journal_failure().is_some());
    drop(rt);
    let reopened = open(&temp, settings, provider("return 1"));
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(reopened.enqueue(&id, "retry old task").is_err());
}

#[tokio::test]
async fn source_failure_and_source_result_bounds_keep_their_known_or_unknown_outcomes() {
    for (behavior, unknown) in [(Behavior::Fail, false), (Behavior::Oversize, true)] {
        let temp = setup();
        let (registry, state) = registry_with(
            behavior,
            Limits {
                max_result_bytes: 4096,
                ..Default::default()
            },
        );
        let mut settings = config(&registry);
        allow(&mut settings, "mcp.call", call_args(1));
        let rt = open(
            &temp,
            settings,
            provider(
                "return cayw.call('mcp.call',{server='local',tool='echo',arguments={value=1}})",
            ),
        );
        let id = rt.create(None).unwrap().id;
        let result = rt
            .prompt(&id, "source result bound", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            result.state,
            if unknown {
                SessionState::NeedsReconciliation
            } else {
                SessionState::Idle
            }
        );
        let entries = dispatches(&rt, &id);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            matches!(
                entries.last().unwrap().outcome,
                Some(DispatchOutcome::Unknown { .. })
            ),
            unknown
        );
        if !unknown {
            assert!(matches!(
                entries.last().unwrap().outcome,
                Some(DispatchOutcome::Failed { .. })
            ));
        }
        assert_eq!(state.calls.lock().unwrap().len(), 1);
    }
}
