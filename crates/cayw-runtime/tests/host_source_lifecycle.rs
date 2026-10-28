#![cfg(target_os = "macos")]
use async_trait::async_trait;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_bridge::{
    AccessRequest, DispatchOutcome, Limits, Provenance, Registry, RegistryPolicy, SourceIdentity,
    Visibility,
};
use cayw_codemode::CapabilityError;
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityReconciliation, CoreEvent,
    ReconciliationOrigin, ReconciliationRequest, ReconciliationVerifier, SessionState,
};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{ApprovalChoice, BridgeDispatchEvent, Runtime, RuntimeConfig, ScopedPermission};
use cayw_sources::{StdioConfig, StdioControl, StdioSource};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const WATCHDOG: Duration = Duration::from_secs(15);
const TOKEN: &str = "issue9-lifecycle-local-fixture-token";
const WAIT: &str = "return cayw.call('mcp.call',{server='local',tool='wait',arguments={}})";
const ADD: &str = "return cayw.call('mcp.call',{server='local',tool='add',arguments={a=17,b=25}})";
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
struct Fixed {
    script: Mutex<String>,
    requests: Mutex<Vec<Value>>,
}
impl Fixed {
    fn new(script: &str) -> Self {
        Self {
            script: Mutex::new(script.into()),
            requests: Mutex::default(),
        }
    }
}
impl Provider for Fixed {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_value(&request).unwrap());
        let message = cayw_runtime::fixture::script_response(
            &request,
            &self.script.lock().unwrap(),
            &format!("lifecycle-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}
struct Case {
    _temp: Option<tempfile::TempDir>,
    root: PathBuf,
}
impl Case {
    fn new(label: &str) -> Self {
        let (root, temp) = match std::env::var_os("CAYW_ISSUE9_ARTIFACTS_DIR") {
            Some(parent) => {
                let root = PathBuf::from(parent).join(label);
                std::fs::create_dir(&root).unwrap();
                (root, None)
            }
            None => {
                let temp = tempfile::tempdir().unwrap();
                (temp.path().into(), Some(temp))
            }
        };
        std::fs::create_dir(root.join("workspace")).unwrap();
        Self { _temp: temp, root }
    }
    fn save(&self, name: &str, value: &impl serde::Serialize) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join(format!("{name}.json")))
            .unwrap();
        serde_json::to_writer_pretty(&mut file, value).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
    }
    fn open(&self, settings: RuntimeConfig, model: Arc<Fixed>) -> Runtime {
        self.save(&format!("config-{}",uuid::Uuid::new_v4()),&json!({
            "model":{"provider":"fixture","id":"issue9-lifecycle"},"real_model":false,
            "database":self.root.join("session.db"),"workspace":self.root.join("workspace"),
            "permissions":settings.permissions,"bridge":settings.bridge.as_ref().map(|b|b.binding_snapshot()),
            "runtime":{"max_run_time_ms":settings.max_run_time.as_millis(),"max_turns":settings.max_turns,
                "max_total_tokens":settings.max_total_tokens,"max_capability_calls":settings.max_capability_calls,
                "max_capability_bytes":settings.max_capability_bytes,"max_history_bytes":settings.max_history_bytes,"max_event_bytes":settings.max_event_bytes},
            "host":{"max_calls":settings.host.max_calls,"max_argument_bytes":settings.host.max_argument_bytes,
                "max_result_bytes":settings.host.max_result_bytes,"max_total_bytes":settings.host.max_total_bytes,
                "call_timeout_ms":settings.host.call_timeout.as_millis()},
            "codemode":{"host_calls":settings.codemode.host_calls,"output_bytes":settings.codemode.output_bytes,
                "timeout_ms":settings.codemode.timeout.as_millis()},"requests":settings.request_limits
        }));
        Runtime::open(
            self.root.join("session.db"),
            self.root.join("workspace"),
            model,
            Model::new("fixture", "issue9-lifecycle"),
            settings,
        )
        .unwrap()
    }
    fn capture(&self, label: &str, rt: &Runtime, id: &str, control: &StdioControl, model: &Fixed) {
        self.save(label,&json!({"session":rt.get(id).unwrap(),"core":rt.events(id,0,1000).unwrap(),
            "dispatches":records(rt,id),"audit":rt.audit(id,0,1000).unwrap(),"tasks":rt.tasks(id).unwrap(),
            "source":control.snapshot(),"provider_requests":model.requests.lock().unwrap().clone(),
            "cells":rt.get(id).unwrap().messages.iter().filter_map(|message|match message{cayw_ai::Message::ToolResult(result)=>result.structured_content.clone(),_=>None}).collect::<Vec<_>>()}));
    }
}
fn identity(version: &str) -> SourceIdentity {
    SourceIdentity {
        id: "local".into(),
        version: version.into(),
        origin: "host:issue9-local-stdio".into(),
    }
}
fn source_configuration(config: &StdioConfig) -> Value {
    json!({"program":config.program,"directory":config.directory,"arguments":config.arguments,"environment":config.environment,
        "max_frame_bytes":config.max_frame_bytes,"max_pending":config.max_pending,"max_observations":config.max_observations,
        "max_received_frames":config.max_received_frames,"max_stderr_bytes":config.max_stderr_bytes,
        "request_timeout_ms":config.request_timeout.as_millis(),"cleanup_timeout_ms":config.cleanup_timeout.as_millis()})
}
async fn prepared(root: &Path) -> StdioSource {
    let config = StdioConfig::new(env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture"), root);
    let source = StdioSource::connect(config.clone(), CancellationToken::new())
        .await
        .unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(format!(
            "stdio-config-{}.json",
            source.control().snapshot().instance_id
        )))
        .unwrap();
    serde_json::to_writer_pretty(&mut file, &source_configuration(&config)).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
    source
}
struct LifecycleReconciliationVerifier;
impl ReconciliationVerifier for LifecycleReconciliationVerifier {
    fn verify(&self, request: &ReconciliationRequest) -> Result<(), String> {
        match request {
            ReconciliationRequest::Capability { reconciliation } => {
                if reconciliation.actor.is_empty() || reconciliation.evidence.is_empty() {
                    return Err("actor and evidence required".into());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
fn settings(registry: &Registry) -> RuntimeConfig {
    RuntimeConfig {
        bridge: Some(registry.consumer()),
        max_run_time: Duration::from_secs(30),
        permissions: vec![
            ScopedPermission::exact(
                "mcp.call",
                json!({"server":"local","tool":"wait","arguments":{}}),
            ),
            ScopedPermission::exact(
                "mcp.call",
                json!({"server":"local","tool":"add","arguments":{"a":17,"b":25}}),
            ),
        ],
        reconciliation_verifier: Some(Arc::new(LifecycleReconciliationVerifier)),
        ..Default::default()
    }
}
fn records(rt: &Runtime, id: &str) -> Vec<BridgeDispatchEvent> {
    let mut after = 0;
    let mut records = Vec::new();
    loop {
        let page = rt.bridge_dispatches(id, after, 16).unwrap();
        if page.next_cursor == after {
            break;
        }
        after = page.next_cursor;
        records.extend(page.events);
    }
    records
}
fn calls(rt: &Runtime, id: &str) -> Vec<CapabilityCallRecord> {
    rt.events(id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect()
}
async fn progress(control: &StdioControl) {
    tokio::time::timeout(WATCHDOG, async {
        while control.snapshot().progress == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn closed(control: &StdioControl) {
    tokio::time::timeout(WATCHDOG, control.wait_closed())
        .await
        .unwrap();
    assert!(control.snapshot().reaped);
}
async fn pending(rt: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if let Some(p) = rt.get(id).unwrap().pending_approvals.first() {
                return p.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_runtime_cancel_disconnect_and_revoke_preserve_original_unknown_without_replay() {
    for action in ["cancel", "disconnect", "revoke"] {
        let case = Case::new(&format!("lifecycle-{action}"));
        let source = prepared(&case.root).await;
        let control = source.control();
        let (manifest, handle) = source.into_parts();
        let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
        let original = registry.register(identity("2"), manifest, handle).unwrap();
        let config = settings(&registry);
        let model = Arc::new(Fixed::new(WAIT));
        let rt = case.open(config.clone(), model.clone());
        let id = rt.create(None).unwrap().id;
        let task = rt.enqueue(&id, action).unwrap();
        case.save("input",&json!({"action":action,"task":task,"source":original,"script":WAIT,"permissions":config.permissions.iter().map(|p|json!({"capability":p.capability,"scope":p.scope})).collect::<Vec<_>>()}));
        let worker_rt = rt.clone();
        let task_id = task.id.clone();
        let worker =
            tokio::spawn(async move { worker_rt.resume(&task_id, CancellationToken::new()).await });
        progress(&control).await;
        case.capture("before-stop", &rt, &id, &control, &model);
        match action {
            "cancel" => {
                assert!(
                    rt.cancel(&rt.task(&task.id).unwrap().cancel_target())
                        .unwrap()
                );
            }
            "disconnect" => registry.disconnect("local").unwrap(),
            "revoke" => registry.revoke("local").unwrap(),
            _ => unreachable!(),
        }
        let _ = tokio::time::timeout(WATCHDOG, worker)
            .await
            .unwrap()
            .unwrap();
        rt.wait_idle(&id).await;
        assert_eq!(
            rt.get(&id).unwrap().state,
            SessionState::NeedsReconciliation
        );
        let terminals = records(&rt, &id);
        let terminal = terminals.last().unwrap();
        assert!(matches!(
            terminal.outcome,
            Some(DispatchOutcome::Unknown { .. })
        ));
        assert_eq!(terminal.record.context.session_id, id);
        assert_eq!(terminal.record.context.task_id, task.id);
        assert_eq!(
            terminal.record.context.attempt_id,
            terminal.origin.lease.attempt_id
        );
        assert_eq!(
            terminal.record.context.execution_id,
            terminal.origin.key.execution_id
        );
        assert_eq!(
            terminal.record.context.sequence,
            terminal.origin.key.sequence + 1
        );
        assert_eq!(terminal.record.key.source, original);
        assert!(registry.snapshot("local").unwrap().outcome_unknown);
        assert!(rt.enqueue(&id, "must not replay").is_err());
        assert_eq!(control.snapshot().tool_calls, 1);
        if action == "cancel" {
            tokio::time::timeout(WATCHDOG, async {
                while control.snapshot().ignored_late == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(control.snapshot().cancellations, 1);
            assert!(rt.get(&id).unwrap().unresolved_capabilities.iter().all(|c|!matches!(&c.outcome,Some(CapabilityOutcome::Completed{result}) if result["value"]["sum"]==999)));
            registry.disconnect("local").unwrap();
        }
        closed(&control).await;
        case.capture("after-stop", &rt, &id, &control, &model);
        let count = model.requests.lock().unwrap().len();
        drop(rt);
        let reopened = case.open(config, model.clone());
        assert_eq!(
            reopened.get(&id).unwrap().state,
            SessionState::NeedsReconciliation
        );
        assert_eq!(
            reopened.recover(&id).unwrap().state,
            SessionState::NeedsReconciliation
        );
        assert!(reopened.enqueue(&id, "reopen cannot replay").is_err());
        assert_eq!(model.requests.lock().unwrap().len(), count);
        assert_eq!(control.snapshot().tool_calls, 1);
        case.capture("reopened", &reopened, &id, &control, &model);
        // A new physical child cannot reset unknown or permanent revocation.
        let replacement = prepared(&case.root).await;
        let new_control = replacement.control();
        let (manifest, handle) = replacement.into_parts();
        if action == "revoke" {
            assert!(registry.reconnect(identity("3"), manifest, handle).is_err());
        } else {
            let provenance = registry.reconnect(identity("3"), manifest, handle).unwrap();
            assert!(provenance.generation > original.generation);
            let replacement_model = Arc::new(Fixed::new(ADD));
            let new_rt = case.open(settings(&registry), replacement_model.clone());
            let other = new_rt.create(None).unwrap().id;
            let _ = new_rt
                .prompt(
                    &other,
                    "unknown source cannot run",
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(new_control.snapshot().tool_calls, 0);
            assert!(registry.snapshot("local").unwrap().outcome_unknown);
            case.capture(
                "replacement-refused",
                &new_rt,
                &other,
                &new_control,
                &replacement_model,
            );
            registry.disconnect("local").unwrap();
        }
        closed(&new_control).await;
        case.save("replacement-source", &new_control.snapshot());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_ipc_cancel_targets_original_runtime_task_and_ignores_late_reply() {
    let case = Case::new("lifecycle-ipc-cancel");
    let source = prepared(&case.root).await;
    let control = source.control();
    let (manifest, handle) = source.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry.register(identity("2"), manifest, handle).unwrap();
    let model = Arc::new(Fixed::new(WAIT));
    let rt = case.open(settings(&registry), model.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt.clone()),
        ServerConfig::new(TOKEN),
        stop.clone(),
    ));
    let client = Client::connect(address, TOKEN).await.unwrap();
    let id = client.request("session.create", json!({})).await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let prompt_client = client.clone();
    let prompt_id = id.clone();
    let worker = tokio::spawn(async move {
        prompt_client
            .request(
                "session.prompt",
                json!({"session_id":prompt_id,"text":"cancel actual local server"}),
            )
            .await
    });
    progress(&control).await;
    let task = rt.tasks(&id).unwrap().pop().unwrap();
    let target = task.cancel_target();
    let mut forged = serde_json::to_value(&target).unwrap();
    forged["task_id"] = json!("other-task");
    assert!(client.request("session.cancel", forged).await.is_err());
    assert_eq!(control.snapshot().cancellations, 0);
    client
        .request("session.cancel", serde_json::to_value(target).unwrap())
        .await
        .unwrap();
    let _ = tokio::time::timeout(WATCHDOG, worker)
        .await
        .unwrap()
        .unwrap();
    rt.wait_idle(&id).await;
    assert_eq!(
        client
            .request("session.get", json!({"session_id":id}))
            .await
            .unwrap()["state"],
        "needs_reconciliation"
    );
    tokio::time::timeout(WATCHDOG, async {
        while control.snapshot().ignored_late == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let page = client
        .request(
            "session.events",
            json!({"session_id":id,"after":0,"limit":1000}),
        )
        .await
        .unwrap();
    let through_ipc: Vec<cayw_core::EventRecord> = serde_json::from_value(page.clone()).unwrap();
    assert_eq!(through_ipc, rt.events(&id, 0, 1000).unwrap());
    assert!(
        records(&rt, &id)
            .iter()
            .any(|event| matches!(event.outcome, Some(DispatchOutcome::Unknown { .. })))
    );
    case.save("ipc-dispatches", &page);
    case.capture("cancelled", &rt, &id, &control, &model);
    registry.disconnect("local").unwrap();
    closed(&control).await;
    case.save("cleanup", &control.snapshot());
    client.close().await;
    stop.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_before_execution_and_replacement_do_not_reroute_old_approval() {
    let case = Case::new("lifecycle-replacement");
    let source = prepared(&case.root).await;
    let first = source.control();
    let (manifest, handle) = source.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    let old = registry.register(identity("2"), manifest, handle).unwrap();
    let mut config = settings(&registry);
    config.permissions.clear();
    let model = Arc::new(Fixed::new(ADD));
    let rt = case.open(config, model.clone());
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(
                &worker_id,
                "wait for exact old-source approval",
                CancellationToken::new(),
            )
            .await
    });
    let approval = pending(&rt, &id).await;
    registry.disconnect("local").unwrap();
    closed(&first).await;
    let source = prepared(&case.root).await;
    let second = source.control();
    let (manifest, handle) = source.into_parts();
    let new = registry.reconnect(identity("3"), manifest, handle).unwrap();
    assert_eq!(new.generation, old.generation + 2);
    assert_ne!(first.snapshot().instance_id, second.snapshot().instance_id);
    rt.approve(&id, &approval.id, ApprovalChoice::AllowOnce, "fixture-host")
        .unwrap();
    assert_eq!(
        tokio::time::timeout(WATCHDOG, worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .state,
        SessionState::Idle
    );
    assert_eq!(first.snapshot().tool_calls, 0);
    assert_eq!(second.snapshot().tool_calls, 0);
    assert!(records(&rt, &id).is_empty());
    assert!(
        calls(&rt, &id)
            .iter()
            .any(|c| matches!(c.outcome, Some(CapabilityOutcome::Failed { .. })))
    );
    case.capture("old-approval", &rt, &id, &second, &model);
    let model2 = Arc::new(Fixed::new(ADD));
    let fresh = case.open(settings(&registry), model2.clone());
    let fresh_id = fresh.create(None).unwrap().id;
    assert_eq!(
        fresh
            .prompt(
                &fresh_id,
                "fresh consumer and permission",
                CancellationToken::new()
            )
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    let terminal = records(&fresh, &fresh_id).pop().unwrap();
    assert_eq!(terminal.record.key.source, new);
    assert!(
        matches!(&terminal.outcome,Some(DispatchOutcome::Succeeded{value}) if value["value"]["sum"]==42),
        "{terminal:?}; calls={:?}",
        calls(&fresh, &fresh_id)
    );
    assert_eq!(second.snapshot().tool_calls, 1);
    case.capture("fresh-consumer", &fresh, &fresh_id, &second, &model2);
    registry.revoke("local").unwrap();
    closed(&second).await;
    *model2.script.lock().unwrap() = ADD.into();
    let denied = fresh.create(None).unwrap().id;
    let _ = fresh
        .prompt(&denied, "revoked new call", CancellationToken::new())
        .await;
    assert_eq!(second.snapshot().tool_calls, 1);
    case.capture("revoked", &fresh, &denied, &second, &model2);
}

#[tokio::test]
async fn actual_host_call_output_and_runtime_budgets_do_not_reset_between_source_scopes() {
    for kind in ["host-calls", "runtime-calls", "result-bytes"] {
        let case = Case::new(&format!("budget-{kind}"));
        let source = prepared(&case.root).await;
        let control = source.control();
        let (manifest, handle) = source.into_parts();
        let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
        registry.register(identity("2"), manifest, handle).unwrap();
        let mut config = settings(&registry);
        let script = if kind == "result-bytes" {
            config.host.max_result_bytes = 32;
            ADD.to_owned()
        } else {
            if kind == "host-calls" {
                config.host.max_calls = 1;
            } else {
                config.max_capability_calls = 1;
            }
            config.permissions.push(ScopedPermission::exact(
                "mcp.discover",
                json!({"server":"local"}),
            ));
            format!("cayw.call('mcp.discover',{{server='local'}})\n{ADD}")
        };
        let model = Arc::new(Fixed::new(&script));
        let rt = case.open(config, model.clone());
        let id = rt.create(None).unwrap().id;
        let _ = rt.prompt(&id, kind, CancellationToken::new()).await;
        if kind == "result-bytes" {
            assert_eq!(control.snapshot().tool_calls, 1);
            assert_eq!(
                rt.get(&id).unwrap().state,
                SessionState::NeedsReconciliation
            );
            assert!(matches!(
                records(&rt, &id).last().unwrap().outcome,
                Some(DispatchOutcome::Succeeded { .. })
            ));
        } else {
            assert_eq!(control.snapshot().tool_calls, 0);
            assert!(calls(&rt, &id).iter().any(|c| c.name == "mcp.discover"
                && matches!(c.outcome, Some(CapabilityOutcome::Completed { .. }))));
        }
        case.capture("result", &rt, &id, &control, &model);
        registry.disconnect("local").unwrap();
        closed(&control).await;
        case.save("cleanup", &control.snapshot());
    }
}

#[tokio::test]
async fn unrepresentable_tool_integer_is_rejected_before_mcp_dispatch() {
    let case = Case::new("large-integer-before-dispatch");
    let source = prepared(&case.root).await;
    let control = source.control();
    let (manifest, handle) = source.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry.register(identity("2"), manifest, handle).unwrap();
    let mut config = settings(&registry);
    config.permissions.push(ScopedPermission::exact(
        "mcp.call",
        json!({"server":"local","tool":"add","arguments":{"a":i64::MAX,"b":1}}),
    ));
    let script = format!(
        "return cayw.call('mcp.call',{{server='local',tool='add',arguments={{a={},b=1}}}})",
        i64::MAX
    );
    let model = Arc::new(Fixed::new(&script));
    let rt = case.open(config, model.clone());
    let id = rt.create(None).unwrap().id;
    rt.prompt(
        &id,
        "integer conversion negative control",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    case.capture("result", &rt, &id, &control, &model);
    assert!(calls(&rt, &id).is_empty());
    assert!(records(&rt, &id).is_empty());
    assert_eq!(control.snapshot().tool_calls, 0);
    assert!(
        serde_json::to_string(&rt.get(&id).unwrap().executions)
            .unwrap()
            .contains("Integer exceeds Luau's exact JSON integer range")
    );
    registry.disconnect("local").unwrap();
    closed(&control).await;
    case.save("cleanup", &control.snapshot());
}

#[tokio::test]
async fn actual_service_error_is_bound_to_original_call_and_host_deadline_retains_unknown() {
    for kind in ["service-error", "host-deadline"] {
        let case = Case::new(kind);
        let mut source_config =
            StdioConfig::new(env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture"), &case.root);
        if kind == "service-error" {
            source_config
                .environment
                .insert("CAYW_MCP_FIXTURE_FAULT".into(), "tool-rejected".into());
        }
        case.save("stdio-config", &source_configuration(&source_config));
        let source = StdioSource::connect(source_config, CancellationToken::new())
            .await
            .unwrap();
        let control = source.control();
        let (manifest, handle) = source.into_parts();
        let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
        registry.register(identity("2"), manifest, handle).unwrap();
        let mut config = settings(&registry);
        let script = if kind == "service-error" {
            ADD.to_owned()
        } else {
            config.host.call_timeout = Duration::from_millis(300);
            WAIT.to_owned()
        };
        let model = Arc::new(Fixed::new(&script));
        let rt = case.open(config, model.clone());
        let id = rt.create(None).unwrap().id;
        let _ = tokio::time::timeout(WATCHDOG, rt.prompt(&id, kind, CancellationToken::new()))
            .await
            .unwrap();
        case.capture("observed-before-check", &rt, &id, &control, &model);
        let source_records = records(&rt, &id);
        assert!(
            !source_records.is_empty(),
            "{kind}; calls={:?}",
            calls(&rt, &id)
        );
        let last = source_records.last().unwrap();
        assert_eq!(last.record.context.session_id, id);
        assert_eq!(control.snapshot().tool_calls, 1);
        if kind == "service-error" {
            assert!(
                matches!(&last.outcome,Some(DispatchOutcome::Failed{message}) if message.contains("MCP error -32602"))
            );
        } else {
            assert!(matches!(
                last.outcome,
                Some(DispatchOutcome::Unknown { .. })
            ));
            assert_eq!(
                rt.get(&id).unwrap().state,
                SessionState::NeedsReconciliation
            );
        }
        case.capture("result", &rt, &id, &control, &model);
        registry.disconnect("local").unwrap();
        closed(&control).await;
        case.save("cleanup", &control.snapshot());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_stdio_mcp_disconnect_before_response_retains_unknown_and_reconciles_without_replay()
{
    let case = Case::new("lifecycle-stdio-disconnect-before-response");
    let mut source_config =
        StdioConfig::new(env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture"), &case.root);
    // CAYW_MCP_FIXTURE_FAULT="tool-exit" causes the stdio server process to exit immediately on tools/call,
    // simulating abrupt crash/disconnection before any response is transmitted.
    source_config
        .environment
        .insert("CAYW_MCP_FIXTURE_FAULT".into(), "tool-exit".into());
    case.save("stdio-config", &source_configuration(&source_config));
    let source = StdioSource::connect(source_config, CancellationToken::new())
        .await
        .unwrap();
    let control = source.control();
    let (manifest, handle) = source.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    let original_prov = registry.register(identity("2"), manifest, handle).unwrap();
    let config = settings(&registry);
    let model = Arc::new(Fixed::new(ADD));
    let rt = case.open(config.clone(), model.clone());
    let id = rt.create(None).unwrap().id;

    // Run prompt. Stdio child will crash on tools/call before responding.
    let _ = tokio::time::timeout(
        WATCHDOG,
        rt.prompt(&id, "call-crash", CancellationToken::new()),
    )
    .await
    .unwrap();

    // 1. Session must halt defensively with NeedsReconciliation, retaining unknown.
    let session = rt.get(&id).unwrap();
    assert_eq!(session.state, SessionState::NeedsReconciliation);
    assert_eq!(session.unresolved_capabilities.len(), 1);
    let unresolved = &session.unresolved_capabilities[0];
    assert!(matches!(
        &unresolved.outcome,
        Some(CapabilityOutcome::OutcomeUnknown { error }) if error.contains("stdio transport ended; no replay")
    ));

    // 2. Dispatch journal in core must retain unknown terminal and call identity.
    let dispatches = records(&rt, &id);
    assert!(!dispatches.is_empty());
    let last = dispatches.last().unwrap();
    assert!(matches!(
        last.outcome,
        Some(DispatchOutcome::Unknown { .. })
    ));
    assert_eq!(last.record.context.session_id, id);
    assert_eq!(last.record.key.target, "add");
    assert_eq!(last.record.key.source, original_prov);

    // 3. Child process cleanup: the child process must be reaped.
    registry.disconnect("local").unwrap();
    closed(&control).await;
    assert!(control.snapshot().reaped);

    // 4. Independent reopen must preserve NeedsReconciliation and reject new work without replay.
    drop(rt);
    let reopened = case.open(config.clone(), model.clone());
    assert_eq!(
        reopened.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(
        reopened
            .enqueue(&id, "must not replay old unknown")
            .is_err()
    );
    assert_eq!(control.snapshot().tool_calls, 1);

    // 5. Reconcile capability and verify session recovers to Idle without repeating side effects.
    let target_unresolved = reopened
        .get(&id)
        .unwrap()
        .unresolved_capabilities
        .pop()
        .unwrap();
    let reconciliation = CapabilityReconciliation {
        key: target_unresolved.key.clone(),
        expected: Box::new(ReconciliationOrigin::from(&target_unresolved)),
        actor: "reconciliation-operator".into(),
        evidence: "operator verified child crashed before completing side effect".into(),
        outcome: CapabilityOutcome::NotExecuted {
            reason: "child crashed before execution".into(),
        },
        external_provenance: target_unresolved.external_provenance.clone(),
    };
    let reconciled_view = reopened.reconcile_capability(&id, reconciliation).unwrap();
    assert_eq!(reconciled_view.state, SessionState::Idle);
    assert!(reconciled_view.unresolved_capabilities.is_empty());

    // Tool calls count on original process must still be 1 (no second invocation / no replay).
    assert_eq!(control.snapshot().tool_calls, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_stdio_mcp_terminal_persistence_failure_retains_chain_identity_response_and_reconciles()
 {
    let case = Case::new("lifecycle-stdio-terminal-persistence-failure");
    let source = prepared(&case.root).await;
    let control = source.control();
    let (manifest, handle) = source.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    let original_prov = registry.register(identity("2"), manifest, handle).unwrap();
    let config = settings(&registry);
    let model = Arc::new(Fixed::new(ADD));
    let rt = case.open(config.clone(), model.clone());
    let id = rt.create(None).unwrap().id;

    // Inject fault: simulate SQLite commitment failure ONLY for runtime_bridge_settled when outcome is Succeeded.
    // (DispatchOutcome::Succeeded serializes with "state": "succeeded", whereas fallback dropped() has "state": "unknown").
    // This allows prepare and fallback dropped() to succeed while settled() fails in a controlled synthetic harness.
    // Note: Historical reg_15 observed an unknown terminal write without an underlying SQLite error, so this synthetic
    // test verifies the durability and error-chain invariants under simulated terminal commitment failure.
    let connection = rusqlite::Connection::open(case.root.join("session.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_terminal_settle BEFORE INSERT ON core_events \
         WHEN json_extract(NEW.record,'$.event.type')='host' \
         AND json_extract(NEW.record,'$.event.kind')='runtime_bridge_settled' \
         AND json_extract(NEW.record,'$.event.data.outcome.state')='succeeded' \
         BEGIN SELECT RAISE(ABORT,'injected sqlite disk failure during terminal write'); END;",
        )
        .unwrap();
    drop(connection);

    // Prompt runs: Luau calls mcp.call add. The real stdio MCP executes and returns {"sum": 42, ...}.
    // Then settled() fails to commit, fallback dropped() writes cursor with outcome_unknown,
    // and registry returns the enhanced error preserving error chain, call identity, and response summary.
    let _ = tokio::time::timeout(
        WATCHDOG,
        rt.prompt(
            &id,
            "call-add-persistence-failure",
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap();

    // 1. Session must halt defensively with NeedsReconciliation.
    let session = rt.get(&id).unwrap();
    assert_eq!(session.state, SessionState::NeedsReconciliation);
    assert_eq!(session.unresolved_capabilities.len(), 1);

    // 2. Check capability_settled outcome: it must retain error chain, call identity, response summary, and unknown state.
    let unresolved = &session.unresolved_capabilities[0];
    match &unresolved.outcome {
        Some(CapabilityOutcome::OutcomeUnknown { error }) => {
            assert!(
                error
                    .starts_with("dispatch journal terminal write failed; reconciliation required"),
                "error message was: {error}"
            );
            assert!(
                error.contains("underlying_error="),
                "error message was: {error}"
            );
            assert!(
                error.contains("injected sqlite disk failure during terminal write"),
                "error message was: {error}"
            );
            assert!(
                error.contains("call_identity="),
                "error message was: {error}"
            );
            assert!(error.contains("target=add"), "error message was: {error}");
            assert!(error.contains("source=local"), "error message was: {error}");
            assert!(
                error.contains("response_summary="),
                "error message was: {error}"
            );
            assert!(error.contains("succeeded:"), "error message was: {error}");
            assert!(error.contains("sha256="), "error message was: {error}");
            assert!(error.contains("sum"), "error message was: {error}");
            assert!(error.contains("42"), "error message was: {error}");
        }
        other => panic!("expected OutcomeUnknown, got: {other:?}"),
    }

    // 3. Dispatch journal in core has runtime_bridge_settled with unknown outcome (written by fallback).
    let dispatches = records(&rt, &id);
    let last = dispatches.last().unwrap();
    assert!(matches!(
        last.outcome,
        Some(DispatchOutcome::Unknown { .. })
    ));
    assert_eq!(last.record.key.target, "add");
    assert_eq!(last.record.key.source, original_prov);

    // 4. Source cleanup.
    registry.disconnect("local").unwrap();
    closed(&control).await;
    assert!(control.snapshot().reaped);

    // 5. Independent reopen must preserve NeedsReconciliation and reject uncoordinated replay.
    drop(rt);
    let reopened = case.open(config.clone(), model.clone());
    assert_eq!(
        reopened.get(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert_eq!(
        reopened.recover(&id).unwrap().state,
        SessionState::NeedsReconciliation
    );
    assert!(reopened.enqueue(&id, "cannot replay").is_err());
    assert_eq!(control.snapshot().tool_calls, 1);

    // 6. Explicit reconciliation removes the trigger and settles the capability cleanly.
    let connection = rusqlite::Connection::open(case.root.join("session.db")).unwrap();
    connection
        .execute_batch("DROP TRIGGER fail_terminal_settle;")
        .unwrap();
    drop(connection);

    let target_unresolved = reopened
        .get(&id)
        .unwrap()
        .unresolved_capabilities
        .pop()
        .unwrap();
    let reconciliation = CapabilityReconciliation {
        key: target_unresolved.key.clone(),
        expected: Box::new(ReconciliationOrigin::from(&target_unresolved)),
        actor: "reconciliation-operator".into(),
        evidence: "operator verified stdio execution succeeded and confirmed sum=42".into(),
        outcome: CapabilityOutcome::Completed {
            result: json!({"sum": 42}),
        },
        external_provenance: target_unresolved.external_provenance.clone(),
    };
    let reconciled_view = reopened.reconcile_capability(&id, reconciliation).unwrap();
    assert_eq!(reconciled_view.state, SessionState::Idle);
    assert!(reconciled_view.unresolved_capabilities.is_empty());

    // Tool calls count must strictly remain 1 (no duplicate side effect / no replay).
    assert_eq!(control.snapshot().tool_calls, 1);
}
