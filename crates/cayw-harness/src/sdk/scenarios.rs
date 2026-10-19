use super::{DEADLINE, Result, Scenario, ScenarioContext, children, provider::*, report::*};
use crate::measurement::{elapsed_ns, sha256};
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_core::{CapabilityOutcome, CoreEvent, EventRecord, SessionState};
use cayw_ipc::{Client, ServerConfig};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, SessionView, TaskState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc, time::Instant};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuiltinScenario {
    ColdSdk,
    SdkLifecycle,
    IpcLifecycle,
    IpcReconnect,
    IpcCancel,
    HostRestartBeforeDispatch,
    HostRestartUnknown,
}
impl BuiltinScenario {
    pub const ALL: [Self; 7] = [
        Self::ColdSdk,
        Self::SdkLifecycle,
        Self::IpcLifecycle,
        Self::IpcReconnect,
        Self::IpcCancel,
        Self::HostRestartBeforeDispatch,
        Self::HostRestartUnknown,
    ];
    pub fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|s| s.name() == name)
            .ok_or_else(|| format!("unknown scenario {name}").into())
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NegativeControl {
    #[default]
    None,
    WrongScript,
    SkipCursor,
}
impl NegativeControl {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "wrong-script" => Ok(Self::WrongScript),
            "skip-cursor" => Ok(Self::SkipCursor),
            _ => Err(format!("unknown negative control {value}").into()),
        }
    }
    pub fn applies(self, scenario: BuiltinScenario) -> bool {
        match self {
            Self::None => true,
            Self::WrongScript => matches!(
                scenario,
                BuiltinScenario::ColdSdk | BuiltinScenario::SdkLifecycle
            ),
            Self::SkipCursor => scenario == BuiltinScenario::IpcReconnect,
        }
    }
}
#[async_trait]
impl Scenario for BuiltinScenario {
    fn name(&self) -> &str {
        match self {
            Self::ColdSdk => "cold-sdk",
            Self::SdkLifecycle => "sdk-lifecycle",
            Self::IpcLifecycle => "ipc-lifecycle",
            Self::IpcReconnect => "ipc-reconnect",
            Self::IpcCancel => "ipc-cancel",
            Self::HostRestartBeforeDispatch => "host-restart-before-dispatch",
            Self::HostRestartUnknown => "host-restart-unknown",
        }
    }
    fn specification(&self, control: NegativeControl) -> Value {
        let effect = *self == Self::HostRestartUnknown;
        let script = if effect { EFFECT_SCRIPT } else { PIPELINE };
        let warm = warm_pipeline();
        json!({"input":{"source":"offline synthetic","prompt":PROMPT,"script":script,"script_sha256":sha256(script.as_bytes()),
                "warm_script":if effect {None} else {Some(&warm)},
                "warm_script_sha256":if effect {None} else {Some(sha256(warm.as_bytes()))},
                "script_selection":"first user turn uses script; later user turns use warm_script when the pipeline is selected",
                "expected_content":CONTENT,"expected_content_sha256":sha256(CONTENT.as_bytes()),"provider":model()},
            "configuration":configuration_json(effect),
            "permissions":{"source":"explicit offline fixture host configuration","product_user_approval":false,
                "policy":"exact capability and canonical arguments; no wildcard or ambient credential",
                "grants":if effect { json!([{ "capability":"search.query", "arguments":external_arguments()}]) } else { json!([
                    {"capability":"fs.write","arguments":write_arguments(false)},
                    {"capability":"fs.write","arguments":write_arguments(true)},
                    {"capability":"fs.read","arguments":{"path":"result.txt"}}]) }},
            "fault_points":match self { Self::ColdSdk=>json!(["fresh independent process"]),Self::SdkLifecycle|Self::IpcLifecycle=>json!(["none"]),
                Self::IpcReconnect=>json!(["Provider entered before first response; client socket explicitly closed; new independent client reads persisted cursor"]),
                Self::IpcCancel=>json!(["Provider entered before first response; authenticated rpc.cancel"]),
                Self::HostRestartBeforeDispatch=>json!(["Provider entered before emitting script; terminate original host process; reopen and explicitly recover without dispatch or replay"]),
                Self::HostRestartUnknown=>json!(["Core capability started and local marker fsynced; terminate original host process; explicit Runtime::recover"]),},
            "negative_control":control,"performance_thresholds":null})
    }
    async fn execute(&self, context: &ScenarioContext, report: &mut ScenarioReport) -> Result<()> {
        match self {
            Self::ColdSdk => children::cold(context, report).await,
            Self::SdkLifecycle => sdk_lifecycle(context, report).await,
            Self::IpcLifecycle => ipc_lifecycle(context, report).await,
            Self::IpcReconnect | Self::IpcCancel => ipc_fault(*self, context, report).await,
            Self::HostRestartBeforeDispatch | Self::HostRestartUnknown => {
                children::restart(context, report, *self == Self::HostRestartUnknown).await
            }
        }
    }
}
pub fn config(effect: bool, root: &Path) -> RuntimeConfig {
    let permissions = if effect {
        vec![ScopedPermission::exact(
            "search.query",
            external_arguments(),
        )]
    } else {
        vec![
            ScopedPermission::exact("fs.write", write_arguments(false)),
            ScopedPermission::exact("fs.write", write_arguments(true)),
            ScopedPermission::exact("fs.read", json!({"path":"result.txt"})),
        ]
    };
    let mut config = RuntimeConfig {
        permissions,
        max_run_time: DEADLINE,
        max_queue_time: DEADLINE,
        ..RuntimeConfig::default()
    };
    if effect {
        config.host.external = Some(Arc::new(EffectFixture(root.join("effect.json"))));
        config.codemode.timeout = DEADLINE;
    }
    config
}
fn configuration_json(effect: bool) -> Value {
    let c = config(effect, Path::new("<sample-root>"));
    let ns = |d: std::time::Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    // The Host defaults are shared with the existing Luau measurement config.
    json!({"runtime":{"max_concurrent_runs":c.max_concurrent_runs,"max_queued_runs":c.max_queued_runs,
        "max_prompt_bytes":c.max_prompt_bytes,"max_history_bytes":c.max_history_bytes,"max_event_bytes":c.max_event_bytes,
        "max_total_tokens":c.max_total_tokens,"max_turns":c.max_turns,"max_run_time_ns":ns(c.max_run_time),"max_queue_time_ns":ns(c.max_queue_time),
        "max_capability_calls":c.max_capability_calls,"max_capability_bytes":c.max_capability_bytes,"grant_lifetime_ns":ns(c.grant_lifetime),
        "codemode":{"memory_bytes":c.codemode.memory_bytes,"instruction_budget":c.codemode.instruction_budget,
            "timeout_ns":ns(c.codemode.timeout),"shutdown_grace_ns":ns(c.codemode.shutdown_grace),"host_calls":c.codemode.host_calls,"output_bytes":c.codemode.output_bytes},
        "cells":{"max_cells":c.cells.max_cells,"max_running":c.cells.max_running,"script_bytes":c.cells.script_bytes,
            "output_bytes":c.cells.output_bytes,"output_items":c.cells.output_items,"item_bytes":c.cells.item_bytes,
            "media_bytes":c.cells.media_bytes,"max_output_tokens":c.cells.max_output_tokens,"max_yield_time_ns":ns(c.cells.max_yield_time),
            "store_bytes":c.cells.store_bytes,"store_keys":c.cells.store_keys,"json_depth":c.cells.json_depth,"cache_entries":c.cells.cache_entries},
        "requests":c.request_limits,"request_policy":null,"optional_features":[]},
        "host":{"max_argument_bytes":c.host.max_argument_bytes,"max_result_bytes":c.host.max_result_bytes,
            "max_total_bytes":c.host.max_total_bytes,"max_calls":c.host.max_calls,"max_file_bytes":c.host.max_file_bytes,
            "max_list_entries":c.host.max_list_entries,"call_timeout_ns":ns(c.host.call_timeout),"process_timeout_ns":ns(c.host.process_timeout),
            "max_process_output_bytes":c.host.max_process_output_bytes,"git_program":null,"profiles":{},
            "data":c.host.data.map(|limits|json!({"input_bytes":limits.input_bytes,"output_bytes":limits.output_bytes,
                "max_depth":limits.max_depth,"max_nodes":limits.max_nodes,"max_rows":limits.max_rows,"max_diff_work":limits.max_diff_work})),
            "workspace":c.host.workspace.as_ref().map(|w|json!({"max_file_bytes":w.max_file_bytes,
                "max_batch_bytes":w.max_batch_bytes,"max_batch_operations":w.max_batch_operations,"max_batches":w.max_batches,
                "max_tree_entries":w.max_tree_entries,"max_depth":w.max_depth,"max_argument_bytes":w.max_argument_bytes,"max_output_bytes":w.max_output_bytes,
                "max_archive_bytes":w.max_archive_bytes,"max_archive_entries":w.max_archive_entries,"max_archive_expanded_bytes":w.max_archive_expanded_bytes,
                "max_compression_ratio":w.max_compression_ratio,"timeout_ns":ns(w.timeout),"syntax_validator":"cayw_workspace::JsonTomlValidator"})),
            "external":if effect {Some("search.query fixture performs one durable local marker then waits")} else {None}},
        "ipc":ipc_config_json(),"executor":"tokio current_thread","deadline_ns":ns(DEADLINE)})
}
pub fn ipc_config() -> ServerConfig {
    let mut c = ServerConfig::new("offline-harness-loopback-fixture-token");
    c.cancellation_grace = std::time::Duration::from_secs(5);
    c
}
fn ipc_config_json() -> Value {
    let c = ipc_config();
    let client = cayw_ipc::ClientConfig::default();
    let ns = |d: std::time::Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    json!({"transport":"loopback TCP ephemeral port","auth":"fixed public offline fixture token, not a credential",
        "server":{"max_frame_bytes":c.max_frame_bytes,"max_in_flight":c.max_in_flight,"request_timeout_ns":ns(c.request_timeout),
            "max_connections":c.max_connections,"max_requests_per_connection":c.max_requests_per_connection,"max_id_bytes":c.max_id_bytes,
            "response_queue_capacity":c.response_queue_capacity,"handshake_timeout_ns":ns(c.handshake_timeout),"frame_timeout_ns":ns(c.frame_timeout),
            "write_timeout_ns":ns(c.write_timeout),"cancellation_grace_ns":ns(c.cancellation_grace)},
        "client":{"max_frame_bytes":client.max_frame_bytes,"max_in_flight":client.max_in_flight,
            "max_requests_per_connection":client.max_requests_per_connection,"max_id_bytes":client.max_id_bytes,
            "write_queue_capacity":client.write_queue_capacity,"connect_timeout_ns":ns(client.connect_timeout),
            "request_timeout_ns":ns(client.request_timeout),"write_timeout_ns":ns(client.write_timeout)}})
}
pub fn open(root: &Path, provider: Arc<FixedProvider>, effect: bool) -> Result<Runtime> {
    Ok(Runtime::open(
        root.join("session.sqlite"),
        root.join("workspace"),
        provider,
        model(),
        config(effect, root),
    )?)
}
pub fn initialize(root: &Path) -> Result<()> {
    fs::create_dir(root.join("workspace"))?;
    Ok(())
}
pub fn all_events(runtime: &Runtime, id: &str) -> Result<Vec<EventRecord>> {
    let mut events = Vec::new();
    let mut cursor = 0;
    loop {
        let page = runtime.events(id, cursor, 2)?;
        if page.is_empty() {
            break;
        }
        let next = page.last().expect("nonempty").cursor;
        if next <= cursor {
            return Err("event page did not advance its cursor".into());
        }
        cursor = next;
        events.extend(page);
    }
    Ok(events)
}
pub fn capture(
    runtime: &Runtime,
    id: &str,
    stage: &str,
    report: &mut ScenarioReport,
) -> Result<Vec<EventRecord>> {
    let events = all_events(runtime, id)?;
    report.event_page(EventPage {
        stage: stage.into(),
        session_id: id.into(),
        after: 0,
        records: events.clone(),
    })?;
    report.observe(&format!("{stage}_session"), runtime.get(id)?)?;
    report.observe(&format!("{stage}_tasks"), runtime.tasks(id)?)?;
    report.observe(
        &format!("{stage}_audit"),
        runtime.audit(id, 0, cayw_core::MAX_EVENT_PAGE)?,
    )?;
    report.observe(&format!("{stage}_grants"), runtime.grants(id)?)?;
    Ok(events)
}
pub fn clean(view: &SessionView) -> bool {
    view.state == SessionState::Idle
        && view.attempt.is_none()
        && view.pending_approvals.is_empty()
        && view.unresolved_calls.is_empty()
        && view.unresolved_capabilities.is_empty()
        && view.executions.iter().all(|execution| {
            execution.terminal.is_some()
                && !matches!(
                    execution.state,
                    cayw_core::ExecutionState::Running | cayw_core::ExecutionState::Yielded
                )
        })
}
pub async fn sdk_lifecycle(context: &ScenarioContext, report: &mut ScenarioReport) -> Result<()> {
    sdk_executions(context, report, 2).await
}
pub async fn sdk_executions(
    context: &ScenarioContext,
    report: &mut ScenarioReport,
    runs: usize,
) -> Result<()> {
    initialize(&context.root)?;
    let actual = if context.negative_control == NegativeControl::WrongScript {
        "return {incorrect_fixture=true}"
    } else {
        PIPELINE
    };
    report.observe(
        "actual_provider_script",
        json!({"script":actual,"sha256":sha256(actual.as_bytes()),
            "warm_script":if actual==PIPELINE {Some(warm_pipeline())} else {None}}),
    )?;
    let provider = Arc::new(FixedProvider::new(
        actual,
        false,
        Some(context.root.join("provider.jsonl")),
    ));
    let start = Instant::now();
    let runtime = open(&context.root, provider.clone(), false)?;
    report
        .timing_ns
        .insert("runtime_open".into(), elapsed_ns(start));
    let session = runtime.create(Some("Offline SDK fixture".into()))?;
    report.observe("sdk_session_created", &session)?;
    for index in 1..=runs {
        let start = Instant::now();
        let view = tokio::time::timeout(
            DEADLINE,
            runtime.prompt(&session.id, PROMPT, CancellationToken::new()),
        )
        .await??;
        report.timing_ns.insert(
            if index == 1 {
                "first_execution"
            } else {
                "warm_execution"
            }
            .into(),
            elapsed_ns(start),
        );
        report.observe(&format!("sdk_prompt_{index}"), &view)?;
        report.check(
            &format!("sdk_prompt_{index}_converged"),
            clean(&view),
            "SDK prompt returned a durable idle session without unresolved work",
        );
    }
    let before = runtime.get(&session.id)?;
    let events = capture(&runtime, &session.id, "after_warm", report)?;
    report.check(
        "ordered_complete_events",
        contiguous_events(&session.id, 0, &events, before.cursor),
        "Every durable cursor is present exactly once",
    );
    let bytes = fs::read(context.root.join("workspace/result.txt")).ok();
    report.observe(
        "observed_file",
        json!({"bytes":bytes,"sha256":bytes.as_ref().map(|b|sha256(b))}),
    )?;
    report.check(
        "actual_pipeline_effect",
        bytes.as_deref() == Some(CONTENT.as_bytes()),
        "Real Host must write and read the exact expected UTF-8 input",
    );
    validate_pipeline(&events, runs, report);
    report.check(
        "distinct_durable_completed_tasks",
        runtime.tasks(&session.id)?.len() == runs
            && runtime
                .tasks(&session.id)?
                .iter()
                .all(|t| t.state == TaskState::Completed),
        "Every measured prompt must have its own completed task",
    );
    let calls_before = provider.calls();
    drop(runtime);
    let start = Instant::now();
    let reopened = open(&context.root, provider.clone(), false)?;
    report.timing_ns.insert("reopen".into(), elapsed_ns(start));
    let after = reopened.get(&session.id)?;
    report.observe("reopened_session", &after)?;
    report.check(
        "reopen_preserves_history_without_provider_replay",
        before == after && provider.calls() == calls_before,
        "Reopen validates durable state and does not request another Provider stream",
    );
    Ok(())
}
fn validate_pipeline(events: &[EventRecord], runs: usize, report: &mut ScenarioReport) {
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let CoreEvent::CapabilityStarted { call } = &e.event {
                Some(call)
            } else {
                None
            }
        })
        .collect();
    let settled: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let CoreEvent::CapabilitySettled { call } = &e.event {
                Some(call)
            } else {
                None
            }
        })
        .collect();
    let expected: Vec<_> = (0..runs)
        .flat_map(|run| {
            [
                ("fs.write", write_arguments(run > 0)),
                ("fs.read", json!({"path":"result.txt"})),
            ]
        })
        .collect();
    let mut keys = std::collections::BTreeSet::new();
    let ordered=starts.len()==runs*2&&settled.len()==runs*2&&starts.iter().zip(&settled).enumerate().all(|(i,(a,b))| {
        let (name,args)=&expected[i];
        let source = if i < 2 { PIPELINE.to_owned() } else { warm_pipeline() };
        let source_hash = format!("sha256:{}", sha256(source.as_bytes()));
        let authority=a.authorization.as_ref().and_then(|value|serde_json::from_value::<cayw_core::CapabilityAuthorization>(value.clone()).ok());
        let exact_authority=authority.as_ref().is_some_and(|auth|auth.target==a.key&&auth.task_id==a.task_id
            &&auth.capability==a.name&&auth.decision==cayw_core::ApprovalDecision::Approve&&auth.actor=="runtime-config"
            &&auth.scope==serde_json::to_string(args).expect("fixed arguments serialize"));
        keys.insert(a.key.clone())&&a.key==b.key&&a.name==*name&&a.arguments==*args&&b.name==a.name&&b.arguments==a.arguments
            &&a.lease==b.lease&&a.task_id==b.task_id&&a.parent_call_id==b.parent_call_id&&a.source_hash==b.source_hash
            &&a.source_hash==source_hash&&!a.parent_call_id.is_empty()&&!a.task_id.is_empty()
            &&exact_authority&&b.authorization==a.authorization&&a.outcome.is_none()
            && matches!(&b.outcome,Some(CapabilityOutcome::Completed { result }) if result["sha256"]==sha256(CONTENT.as_bytes()))
    });
    report.check("capability_origins_permissions_and_settlements",ordered,"Expected write/read order, exact arguments, original keys and persisted authority with observed content hashes");
    report.check("provider_completed_after_real_tool_results",events.iter().filter(|e| matches!(&e.event,CoreEvent::Agent{event} if serde_json::to_value(event).is_ok_and(|v|v.to_string().contains(FINAL)))).count()>=runs,"Fixed Provider only reports done after actual tool results; raw event and Provider inputs are retained");
}

struct Server {
    stop: CancellationToken,
    task: Option<JoinHandle<std::io::Result<()>>>,
    addr: std::net::SocketAddr,
}
impl Server {
    async fn start(runtime: Runtime) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let stop = CancellationToken::new();
        let task = tokio::spawn(cayw_ipc::serve(
            listener,
            Arc::new(runtime),
            ipc_config(),
            stop.clone(),
        ));
        Ok(Self {
            stop,
            task: Some(task),
            addr,
        })
    }
    async fn close(&mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            tokio::time::timeout(DEADLINE, task).await???;
        }
        Ok(())
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
async fn request(
    client: &Client,
    method: &str,
    params: Value,
    report: &mut ScenarioReport,
) -> Result<Value> {
    report.observe("rpc_request", json!({"method":method,"params":params}))?;
    let result = client.request(method, params).await;
    report.observe("rpc_terminal", json!({"method":method,"result":result}))?;
    Ok(result?)
}
pub async fn ipc_pages(
    client: &Client,
    id: &str,
    after: u64,
    stage: &str,
    report: &mut ScenarioReport,
) -> Result<Vec<EventRecord>> {
    let mut events = Vec::new();
    let mut cursor = after;
    loop {
        let value = request(
            client,
            "session.events",
            json!({"session_id":id,"after":cursor,"limit":2,"wait_ms":0}),
            report,
        )
        .await?;
        let page: Vec<EventRecord> = serde_json::from_value(value)?;
        report.event_page(EventPage {
            stage: stage.into(),
            session_id: id.into(),
            after: cursor,
            records: page.clone(),
        })?;
        if page.is_empty() {
            break;
        }
        let next = page.last().expect("nonempty").cursor;
        if next <= cursor {
            return Err("event page did not advance its cursor".into());
        }
        cursor = next;
        events.extend(page);
    }
    Ok(events)
}
async fn ipc_lifecycle(context: &ScenarioContext, report: &mut ScenarioReport) -> Result<()> {
    initialize(&context.root)?;
    let provider = Arc::new(FixedProvider::new(
        PIPELINE,
        false,
        Some(context.root.join("provider.jsonl")),
    ));
    let runtime = open(&context.root, provider.clone(), false)?;
    let mut server = Server::start(runtime.clone()).await?;
    let wire_path = context.root.join("ipc-frames.jsonl");
    let mut proxy = super::transport::RecordingProxy::start(server.addr, &wire_path).await?;
    report.observe(
        "ipc_listener",
        json!({"server":server.addr,"recording_relay":proxy.address}),
    )?;
    let client = Client::connect(proxy.address, ipc_config().token).await?;
    let value = request(
        &client,
        "session.create",
        json!({"label":"IPC successful pipeline"}),
        report,
    )
    .await?;
    let created: SessionView = serde_json::from_value(value)?;
    let mut last = created;
    for index in 1..=2 {
        let start = Instant::now();
        let value = request(
            &client,
            "session.prompt",
            json!({"session_id":last.id,"text":PROMPT}),
            report,
        )
        .await?;
        report.timing_ns.insert(
            if index == 1 {
                "ipc_first_execution"
            } else {
                "ipc_warm_execution"
            }
            .into(),
            elapsed_ns(start),
        );
        last = serde_json::from_value(value)?;
        report.check(
            &format!("ipc_prompt_{index}_converged"),
            clean(&last),
            "Authenticated IPC prompt returns one durable idle session",
        );
    }
    let events = ipc_pages(&client, &last.id, 0, "successful_ipc_pipeline", report).await?;
    report.check(
        "ipc_pipeline_contiguous",
        contiguous_events(&last.id, 0, &events, last.cursor),
        "Successful remote event pages include every original durable cursor",
    );
    validate_pipeline(&events, 2, report);
    let bytes = fs::read(context.root.join("workspace/result.txt"))?;
    report.observe(
        "observed_file",
        json!({"content":String::from_utf8_lossy(&bytes),"sha256":sha256(&bytes)}),
    )?;
    report.check(
        "ipc_actual_pipeline_effect",
        bytes == CONTENT.as_bytes(),
        "Actual IPC executes the expected Luau Host write/read and returns its durable results",
    );
    let local = runtime.get(&last.id)?;
    report.check(
        "ipc_result_equals_sdk",
        local == last,
        "Public SDK and IPC expose the identical completed state",
    );
    capture(&runtime, &last.id, "completed_sdk", report)?;
    client.close().await;
    proxy.close().await?;
    server.close().await?;
    let wire = fs::read(&wire_path)?;
    let frames: Vec<Value> = String::from_utf8(wire.clone())?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    report.observe("actual_ipc_frames", &frames)?;
    report.artifacts["ipc_frames"] = json!({"path":"ipc-frames.jsonl","sha256":sha256(&wire)});
    let prompts: Vec<_> = frames
        .iter()
        .filter(|r| {
            r["direction"] == "client_to_server" && r["frame"]["method"] == "session.prompt"
        })
        .collect();
    report.check(
        "ipc_success_replies_match_original_ids",
        prompts.len() == 2
            && prompts.iter().all(|p| {
                frames.iter().any(|r| {
                    r["direction"] == "server_to_client"
                        && r["connection"] == p["connection"]
                        && r["frame"]["id"] == p["frame"]["id"]
                        && r["frame"].get("result").is_some()
                })
            }),
        "Both successful prompts retain their actual wire request and terminal response identity",
    );
    Ok(())
}

async fn ipc_fault(
    scenario: BuiltinScenario,
    context: &ScenarioContext,
    report: &mut ScenarioReport,
) -> Result<()> {
    initialize(&context.root)?;
    let provider = Arc::new(FixedProvider::new(
        PIPELINE,
        true,
        Some(context.root.join("provider.jsonl")),
    ));
    let runtime = open(&context.root, provider.clone(), false)?;
    let mut server = Server::start(runtime.clone()).await?;
    report.observe(
        "ipc_listener",
        json!({"address":server.addr,"process_id":std::process::id()}),
    )?;
    let wire_path = context.root.join("ipc-frames.jsonl");
    let mut proxy = super::transport::RecordingProxy::start(server.addr, &wire_path).await?;
    report.observe(
        "recording_relay",
        json!({"address":proxy.address,"upstream":server.addr,"frames":wire_path,
        "token":"public offline fixture token only; never a user credential"}),
    )?;
    let client = Client::connect(proxy.address, ipc_config().token).await?;
    let denied = Client::connect(proxy.address, "wrong-public-fixture-token").await;
    report.observe("wrong_token_control", denied.as_ref().err())?;
    report.check(
        "wrong_token_rejected",
        denied.is_err(),
        "An independent client with the wrong fixture token must fail authentication",
    );
    if let Ok(bad) = denied {
        bad.close().await;
    }
    let value = request(
        &client,
        "session.create",
        json!({"label":"Offline IPC fixture"}),
        report,
    )
    .await?;
    let created: SessionView = serde_json::from_value(value)?;
    let id = created.id.clone();
    let cancel = CancellationToken::new();
    let request_client = client.clone();
    let request_id = id.clone();
    let request_cancel = cancel.clone();
    let mut task = tokio::spawn(async move {
        request_client
            .request_with_cancel(
                "session.prompt",
                json!({"session_id":request_id,"text":PROMPT}),
                request_cancel,
            )
            .await
    });
    tokio::select! {
        ()=provider.entered.cancelled()=>{},
        result=&mut task=>return Err(format!("prompt ended before deterministic fault point: {result:?}").into()),
        ()=tokio::time::sleep(DEADLINE)=>return Err("Provider fault point was not reached".into()),
    }
    report.observe(
        "fault_point_provider_entered",
        json!({"provider_calls":provider.calls(),"session":runtime.get(&id)?}),
    )?;
    // Save a deliberately partial page so reconnect has real remaining events.
    let first = request(
        &client,
        "session.events",
        json!({"session_id":id,"after":0,"limit":1,"wait_ms":0}),
        report,
    )
    .await?;
    let first: Vec<EventRecord> = serde_json::from_value(first)?;
    let cursor = first.last().ok_or("created session has no event")?.cursor;
    report.check(
        "initial_saved_cursor_is_exact",
        first.len() == 1 && contiguous_events(&id, 0, &first, cursor),
        "Persisted cursor originates in the observed first event of this exact session",
    );
    report.event_page(EventPage {
        stage: "before_fault".into(),
        session_id: id.clone(),
        after: 0,
        records: first,
    })?;
    write_new(
        &context.root.join("cursor.json"),
        &json!({"session_id":id,"cursor":cursor}),
    )?;
    report.observe(
        "cursor_saved",
        json!({"path":context.root.join("cursor.json"),"cursor":cursor}),
    )?;
    let start = Instant::now();
    if scenario == BuiltinScenario::IpcReconnect {
        report.observe(
            "fault_injected",
            json!({"operation":"Client::close","point":"after Provider entry and cursor fsync"}),
        )?;
        client.close().await;
    } else {
        report.observe("fault_injected",json!({"operation":"authenticated rpc.cancel via request_with_cancel","point":"after Provider entry"}))?;
        cancel.cancel();
    }
    let terminal = tokio::time::timeout(DEADLINE, task).await??;
    report.observe("prompt_transport_terminal", &terminal)?;
    tokio::time::timeout(DEADLINE, runtime.wait_idle(&id)).await?;
    report
        .timing_ns
        .insert("fault_to_final_idle".into(), elapsed_ns(start));
    let view = runtime.get(&id)?;
    let tasks = runtime.tasks(&id)?;
    report.observe("converged_session", &view)?;
    report.observe("converged_tasks", &tasks)?;
    report.check("cancelled_task_reached_durable_idle",clean(&view)&&tasks.len()==1&&tasks[0].state==TaskState::Cancelled,"Cancellation converged to one durable cancelled task, no live attempt/approval/unknown effect");
    report.check(
        "gate_prevented_tool_effects",
        !context.root.join("workspace/result.txt").exists() && view.executions.is_empty(),
        "Fault happened in pending Provider before any script or Host effect",
    );
    if scenario == BuiltinScenario::IpcReconnect {
        report.check(
            "disconnected_call_is_conservatively_unknown",
            terminal.as_ref().is_err_and(|e| e.outcome_unknown),
            "An admitted call lost with the socket remains unknown to that client",
        );
        let closed = client
            .request("session.get", json!({"session_id":id}))
            .await;
        report.observe("closed_client_negative_control", &closed)?;
        report.check(
            "old_client_did_not_reconnect",
            closed.is_err(),
            "Original Client remains closed; reconnect is explicit",
        );
    } else {
        report.observe("cancel_reply_scope", json!({
            "meaning":"The original transport reply is retained exactly, including outcome_unknown. Durable task, session and effect facts are checked independently.",
            "outcome_unknown":terminal.as_ref().err().map(|error|error.outcome_unknown)
        }))?;
        let health = request(&client, "runtime.health", json!({}), report).await?;
        report.check(
            "cancel_preserved_connection",
            health["status"] == "ok",
            "The same authenticated connection is usable after rpc.cancel",
        );
        client.close().await;
    }
    let fresh = Client::connect(proxy.address, ipc_config().token).await?;
    let saved: Value = serde_json::from_slice(&fs::read(context.root.join("cursor.json"))?)?;
    let saved_cursor = saved["cursor"].as_u64().ok_or("saved cursor is invalid")?;
    let saved_session = saved["session_id"]
        .as_str()
        .ok_or("saved session is invalid")?;
    report.check(
        "saved_cursor_belongs_to_original_session",
        saved_session == id,
        "The synced disk cursor is bound to the original session identity",
    );
    report.observe(
        "independent_reconnect",
        json!({"new_connection":true,"saved_cursor":saved_cursor,"saved_session":saved_session,"source":"fs::read(cursor.json)"}),
    )?;
    let after = if context.negative_control == NegativeControl::SkipCursor {
        saved_cursor + 1
    } else {
        saved_cursor
    };
    report.observe("reconnect_requested_after", after)?;
    let events = ipc_pages(&fresh, saved_session, after, "after_reconnect", report).await?;
    report.check("saved_cursor_resume_is_exact",!events.is_empty()&&contiguous_events(&id,saved_cursor,&events,view.cursor),"New independent Client must retrieve every cursor strictly after the saved value, without gaps, duplicates or replay");
    let value = request(&fresh, "session.get", json!({"session_id":id}), report).await?;
    let remote: SessionView = serde_json::from_value(value)?;
    report.check(
        "ipc_and_sdk_agree_on_final_state",
        remote == view,
        "Final session through actual TCP equals local SDK state",
    );
    let unchanged = provider.calls();
    provider.release.cancel();
    tokio::task::yield_now().await;
    report.check(
        "reconnect_does_not_resume_provider",
        provider.calls() == unchanged && unchanged == 1,
        "Disconnect/cancel does not issue a replacement model request",
    );
    capture(&runtime, &id, "final_sdk", report)?;
    fresh.close().await;
    client.close().await;
    proxy.close().await?;
    server.close().await?;
    let wire = fs::read(&wire_path)?;
    let frames: Vec<Value> = String::from_utf8(wire.clone())?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    report.observe("actual_ipc_frames", &frames)?;
    report.artifacts["ipc_frames"] = json!({"path":"ipc-frames.jsonl","sha256":sha256(&wire)});
    validate_wire(&frames, scenario, report);
    if scenario == BuiltinScenario::IpcCancel {
        validate_cancel_terminal(&frames, &terminal, report);
    }
    Ok(())
}

fn validate_cancel_terminal(
    frames: &[Value],
    terminal: &std::result::Result<Value, cayw_ipc::ClientError>,
    report: &mut ScenarioReport,
) {
    let prompts: Vec<_> = frames
        .iter()
        .filter(|frame| {
            frame["direction"] == "client_to_server" && frame["frame"]["method"] == "session.prompt"
        })
        .collect();
    let replies: Vec<_> = prompts.first().map_or_else(Vec::new, |prompt| {
        frames
            .iter()
            .filter(|frame| {
                frame["direction"] == "server_to_client"
                    && frame["connection"] == prompt["connection"]
                    && frame["frame"]["id"] == prompt["frame"]["id"]
            })
            .collect()
    });
    let matches = prompts.len() == 1
        && replies.len() == 1
        && match terminal {
            Ok(value) => {
                replies[0]["frame"].get("result") == Some(value)
                    && replies[0]["frame"].get("error").is_none()
            }
            Err(error) => {
                serde_json::from_value::<cayw_ipc::RpcError>(replies[0]["frame"]["error"].clone())
                    .map(cayw_ipc::ClientError::from)
                    .is_ok_and(|wire_error| {
                        wire_error.code == error.code
                            && wire_error.message == error.message
                            && wire_error.outcome_unknown == error.outcome_unknown
                    })
            }
        };
    report.check("cancel_has_original_terminal_reply", matches,
        "Exactly one original wire response matches the consumed result or error, including its unchanged outcome_unknown classification");
}

fn validate_wire(frames: &[Value], scenario: BuiltinScenario, report: &mut ScenarioReport) {
    let requests: Vec<_> = frames
        .iter()
        .filter(|r| r["direction"] == "client_to_server")
        .collect();
    let prompt = requests
        .iter()
        .find(|r| r["frame"]["method"] == "session.prompt");
    let paired = prompt.is_some_and(|prompt| {
        frames.iter().any(|r| {
            r["direction"] == "server_to_client"
                && r["connection"] == prompt["connection"]
                && r["frame"]["id"] == prompt["frame"]["id"]
        })
    });
    let cancel = prompt.is_some_and(|prompt| {
        requests.iter().any(|r| {
            r["frame"]["method"] == "rpc.cancel"
                && r["connection"] == prompt["connection"]
                && r["frame"]["params"]["request_id"] == prompt["frame"]["id"]
        })
    });
    report.check(
        "wire_retains_original_request_identity",
        prompt.is_some(),
        "Raw relayed IPC frames include the actual prompt request ID and connection identity",
    );
    if scenario == BuiltinScenario::IpcCancel {
        report.check("wire_cancel_targets_original_request",cancel&&paired,"Actual rpc.cancel targets the original prompt ID on the same connection and its original reply is retained");
    } else {
        report.check("wire_has_independent_reconnect",requests.iter().filter(|r|r["frame"]["method"]=="hello").map(|r|r["connection"].as_u64()).collect::<std::collections::BTreeSet<_>>().len()>=3,
            "Actual hello frames cover the original authenticated connection, wrong-token control and fresh authenticated reconnect");
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    fn frames() -> Vec<Value> {
        vec![
            json!({"direction":"client_to_server","connection":1,"frame":{"method":"session.prompt","id":"original"}}),
            json!({"direction":"server_to_client","connection":1,"frame":{"id":"original","error":{"code":"cancelled_outcome_unknown","message":"request cancelled; side effects may have happened"}}}),
        ]
    }

    fn accepts(frames: &[Value]) -> bool {
        let error = cayw_ipc::ClientError::from(cayw_ipc::RpcError::new(
            "cancelled_outcome_unknown",
            "request cancelled; side effects may have happened",
        ));
        let mut report = ScenarioReport::new("reply-detector", 1, json!({}), json!({}));
        validate_cancel_terminal(frames, &Err(error), &mut report);
        report.checks[0].passed
    }

    #[test]
    fn cancellation_preserves_unknown_reply_and_rejects_missing_duplicate_or_changed_identity() {
        let good = frames();
        assert!(accepts(&good));
        assert!(!accepts(&good[..1]));
        let mut duplicate = good.clone();
        duplicate.push(good[1].clone());
        assert!(!accepts(&duplicate));
        for (field, value) in [("connection", json!(2)), ("id", json!("another"))] {
            let mut bad = good.clone();
            if field == "connection" {
                bad[1][field] = value;
            } else {
                bad[1]["frame"][field] = value;
            }
            assert!(!accepts(&bad));
        }
        let mut downgraded = good;
        downgraded[1]["frame"]["error"]["code"] = json!("cancelled");
        assert!(!accepts(&downgraded));
    }

    #[test]
    fn warm_fixture_script_and_exact_grant_include_expected_existing_content_digest() {
        let digest = sha256(CONTENT.as_bytes());
        assert!(warm_pipeline().contains(&format!("expected_sha256='{digest}'")));
        assert!(warm_pipeline().contains("overwrite=true"));
        assert_eq!(write_arguments(true)["expected_sha256"], digest);
        assert_eq!(write_arguments(true)["overwrite"], true);
        assert!(write_arguments(false).get("expected_sha256").is_none());
    }
}
