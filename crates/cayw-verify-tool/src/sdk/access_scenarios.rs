//! Public Runtime/IPC approval and rejection evidence with an offline Provider.
use super::{
    DEADLINE, EventPage, NegativeControl, Result, Scenario, ScenarioContext, ScenarioReport,
    contiguous_events,
    provider::{CONTENT, FixedProvider, PIPELINE, PROMPT, model, write_arguments},
    scenarios::{BuiltinScenario, all_events, clean, config, initialize},
    transport::RecordingProxy,
};
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_core::{
    ApprovalDecision, ApprovalRequest, ApprovalTarget, CapabilityAuthorization, CapabilityOutcome,
    CoreEvent, EventRecord,
};
use cayw_ipc::{Client, RpcError, RpcHandler, ServerConfig};
use cayw_runtime::{Runtime, TaskState};
use serde_json::{Value, json};
use std::{
    fs, io,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

const TOKEN: &str = "offline-verify-tool-loopback-fixture-token";
const REJECTION_FRAME_BYTES: usize = 1024;

pub(super) fn specification(name: &str) -> Value {
    let mut value = BuiltinScenario::IpcLifecycle.specification(NegativeControl::None);
    value["permissions"] = json!({
        "source":"Runtime approval.decide over authenticated local IPC",
        "product_user_approval":true,
        "policy":"no configured grants; allow_once for each original exact capability target",
        "grants":[],"actor":"rpc-user",
        "fixture_decisions":"automated offline consumer, not a human acceptance record"
    });
    value["configuration"]["runtime"]["codemode"]["timeout_ns"] =
        json!(u64::try_from(DEADLINE.as_nanos()).unwrap_or(u64::MAX));
    value["fault_points"] = match name {
        "ipc-approval" => json!([
            "write and read each pause before their own CapabilityStarted; separate prompt and event/approval connections"
        ]),
        "ipc-rejections" => {
            value["configuration"]["ipc"]["server"]["max_frame_bytes"] =
                json!(REJECTION_FRAME_BYTES);
            value["permissions"]["product_user_approval"] = json!(false);
            json!([
                "direct socket wrong token",
                "direct socket wrong IPC version",
                "direct socket oversized frame without newline after valid hello",
                "Client prompt unknown field",
                "Client approval forged actor",
                "Client events zero limit"
            ])
        }
        _ => json!(["unknown access scenario"]),
    };
    value["evidence"] = json!({"snapshots":"complete Session, Tasks, durable events, audit, grants, stored approvals, Provider requests and workspace contents",
        "wire":"actual LF JSON frames; raw rejection bytes bypass RecordingProxy",
        "scope":"macOS local offline fixture only; not Issue acceptance"});
    value
}

struct CountedRuntime {
    runtime: Runtime,
    calls: Arc<AtomicU64>,
}
#[async_trait]
impl RpcHandler for CountedRuntime {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, RpcError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.runtime.call(method, params, cancel).await
    }
}
struct Server {
    address: SocketAddr,
    stop: CancellationToken,
    task: Option<JoinHandle<io::Result<()>>>,
    calls: Arc<AtomicU64>,
}
impl Server {
    async fn start(runtime: Runtime, configuration: ServerConfig) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let stop = CancellationToken::new();
        let calls = Arc::new(AtomicU64::new(0));
        let handler = Arc::new(CountedRuntime {
            runtime,
            calls: calls.clone(),
        });
        let task = tokio::spawn(cayw_ipc::serve(
            listener,
            handler,
            configuration,
            stop.clone(),
        ));
        Ok(Self {
            address,
            stop,
            task: Some(task),
            calls,
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

fn require(report: &mut ScenarioReport, name: &str, passed: bool, detail: &str) -> Result<()> {
    report.check(name, passed, detail);
    if !passed {
        return Err(format!("{name}: {detail}").into());
    }
    Ok(())
}
fn file(path: &Path) -> Result<Value> {
    match fs::read(path) {
        Ok(bytes) => Ok(
            json!({"bytes":bytes,"sha256":crate::measurement::sha256(&bytes),
            "utf8":String::from_utf8(bytes.clone()).ok()}),
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Value::Null),
        Err(error) => Err(error.into()),
    }
}
// Only the fresh, generated fixture workspace is inspected, never source trees.
fn workspace_files(root: &Path, directory: &Path) -> Result<Vec<Value>> {
    let mut entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut result = Vec::new();
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            result.push(json!({"path":relative,"kind":"symlink","target":fs::read_link(path)?}));
        } else if kind.is_dir() {
            result.push(json!({"path":relative,"kind":"directory"}));
            result.extend(workspace_files(root, &path)?);
        } else {
            result.push(json!({"path":relative,"kind":"file","content":file(&path)?}));
        }
    }
    Ok(result)
}
fn snapshot(runtime: &Runtime, provider: &FixedProvider, root: &Path, id: &str) -> Result<Value> {
    let events = all_events(runtime, id)?;
    let approvals = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::ApprovalRequested { approval } => {
                Some(runtime.store().approval(id, &approval.id))
            }
            _ => None,
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut audit = Vec::new();
    let mut cursor = 0;
    loop {
        let page = runtime.audit(id, cursor, cayw_core::MAX_EVENT_PAGE)?;
        if page.is_empty() {
            break;
        }
        let next = page.last().expect("nonempty audit page").cursor;
        if next <= cursor {
            return Err("audit cursor did not advance".into());
        }
        cursor = next;
        audit.extend(page);
    }
    let requests = fs::read_to_string(root.join("provider.jsonl"))?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(
        json!({"session":runtime.get(id)?,"sessions":runtime.list()?,"tasks":runtime.tasks(id)?,
        "events":events,"audit":audit,"grants":runtime.grants(id)?,"approvals":approvals,
        "health":runtime.health()?,"provider_calls":provider.calls(),"provider_requests":requests,
        "file":file(&root.join("workspace/result.txt"))?,
        "workspace":workspace_files(&root.join("workspace"), &root.join("workspace"))?}),
    )
}

pub(super) async fn execute(
    name: &str,
    context: &ScenarioContext,
    report: &mut ScenarioReport,
) -> Result<()> {
    if !matches!(name, "ipc-approval" | "ipc-rejections") {
        return Err(format!("unknown access scenario {name}").into());
    }
    initialize(&context.root)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(context.root.join("provider.jsonl"))?
        .sync_all()?;
    let provider = Arc::new(FixedProvider::new(
        PIPELINE,
        false,
        Some(context.root.join("provider.jsonl")),
    ));
    let mut configuration = config(false, &context.root);
    configuration.permissions.clear();
    configuration.codemode.timeout = DEADLINE;
    let runtime = Runtime::open(
        context.root.join("session.sqlite"),
        context.root.join("workspace"),
        provider.clone(),
        model(),
        configuration,
    )?;
    let mut ipc = super::scenarios::ipc_config();
    if name == "ipc-rejections" {
        ipc.max_frame_bytes = REJECTION_FRAME_BYTES;
    }
    let mut server = Server::start(runtime.clone(), ipc).await?;
    let result = if name == "ipc-approval" {
        approval(context, report, &runtime, &provider, &server).await
    } else {
        rejections(context, report, &runtime, &provider, &server).await
    };
    let cleanup = server.close().await;
    report.check(
        "access_server_stopped",
        cleanup.is_ok(),
        "The actual IPC server and its connections have joined",
    );
    result?;
    cleanup?;
    report.bind_attachment(
        "access_ipc_frames",
        &context.root,
        "access-ipc-frames.jsonl",
    )?;
    report.bind_attachment("access_provider_requests", &context.root, "provider.jsonl")?;
    Ok(())
}

async fn next_approval(
    client: &Client,
    id: &str,
    after: &mut u64,
    report: &mut ScenarioReport,
) -> Result<ApprovalRequest> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let request = json!({"session_id":id,"after":*after,"limit":100,"wait_ms":1000});
            report.observe("access_events_request", &request)?;
            let records: Vec<EventRecord> =
                serde_json::from_value(client.request("session.events", request).await?)?;
            let next = records.last().map_or(*after, |record| record.cursor);
            require(
                report,
                "access_live_page_contiguous",
                contiguous_events(id, *after, &records, next),
                "Every live IPC event page keeps the original session and exact cursor order",
            )?;
            report.event_page(EventPage {
                stage: "access_live".into(),
                session_id: id.into(),
                after: *after,
                records: records.clone(),
            })?;
            *after = next;
            if let Some(approval) = records.into_iter().find_map(|record| match record.event {
                CoreEvent::ApprovalRequested { approval } => Some(approval),
                _ => None,
            }) {
                return Ok(approval);
            }
        }
    })
    .await?
}

async fn approval(
    context: &ScenarioContext,
    report: &mut ScenarioReport,
    runtime: &Runtime,
    provider: &FixedProvider,
    server: &Server,
) -> Result<()> {
    let mut proxy = RecordingProxy::start(
        server.address,
        &context.root.join("access-ipc-frames.jsonl"),
    )
    .await?;
    let execution = Client::connect(proxy.address, TOKEN).await?;
    let control = Client::connect(proxy.address, TOKEN).await?;
    let result: Result<()> = async {
        let created = control.request("session.create", json!({"label":"Offline IPC approvals"})).await?;
        report.observe("access_session_created", &created)?;
        let id = created["id"].as_str().ok_or("created session has no id")?.to_owned();
        let prompt = json!({"session_id":id,"text":PROMPT});
        report.observe("access_prompt_request", &prompt)?;
        let mut originals = Vec::new();
        let mut cursor = 0;
        let (response, ()) = tokio::try_join!(
            async { Ok::<_, Box<dyn std::error::Error + Send + Sync>>(execution.request("session.prompt", prompt).await?) },
            async {
                for (index, capability) in ["fs.write", "fs.read"].into_iter().enumerate() {
                    let original = next_approval(&control, &id, &mut cursor, report).await?;
                    let before = snapshot(runtime, provider, &context.root, &id)?;
                    report.observe("access_approval_waiting", json!({"approval":original,"snapshot":before}))?;
                    let expected_arguments = if index == 0 { write_arguments(false) } else { json!({"path":"result.txt"}) };
                    let planned = before["events"].as_array().ok_or("snapshot events are missing")?.iter().filter(|event| {
                        event["event"]["type"] == "capability_planned"
                            && event["event"]["call"]["key"] == serde_json::to_value(&original.input.target).unwrap_or(Value::Null)["key"]
                    }).collect::<Vec<_>>();
                    let starts = before["events"].as_array().expect("checked events").iter().filter(|event| event["event"]["type"] == "capability_started").collect::<Vec<_>>();
                    let exact_target = matches!(&original.input.target, ApprovalTarget::CapabilityCall { key } if key.sequence == index as u64)
                        && original.session_id == id && original.decision.is_none()
                        && original.input.capability == capability
                        && original.input.scope == serde_json::to_string(&expected_arguments)?
                        && before["session"]["attempt"]["attempt_id"] == original.attempt_id
                        && before["tasks"].as_array().is_some_and(|tasks| tasks.len() == 1 && tasks[0]["id"] == original.input.task_id)
                        && planned.len() == 1 && planned[0]["event"]["call"]["arguments"] == expected_arguments;
                    require(report, &format!("access_original_target_{index}"), exact_target,
                        "The actual pending approval matches its original task, attempt, capability, sequence and exact arguments")?;
                    require(report, &format!("access_no_start_before_approval_{index}"), starts.len() == index
                        && starts.iter().all(|event| event["event"]["call"]["key"] != planned[0]["event"]["call"]["key"])
                        && if index == 0 { before["file"].is_null() } else { before["file"]["bytes"] == json!(CONTENT.as_bytes()) },
                        "The pending child has no start; write has no file and read waits after the approved write")?;
                    for method in ["session.get", "session.tasks", "session.grants"] {
                        let response = control.request(method, json!({"session_id":id})).await?;
                        report.observe("access_pending_query", json!({"method":method,"response":response}))?;
                    }
                    let request = json!({"session_id":id,"approval_id":original.id,"decision":"allow_once"});
                    report.observe("access_approval_request", &request)?;
                    let decided: ApprovalRequest = serde_json::from_value(control.request("approval.decide", request).await?)?;
                    report.observe("access_approval_decided", json!({"original":original,"response":decided}))?;
                    let mut expected = original.clone();
                    expected.decision = decided.decision.clone();
                    require(report, &format!("access_rpc_actor_and_original_{index}"), decided == expected
                        && decided.decision.as_ref().is_some_and(|decision| decision.actor == "rpc-user" && decision.decision == ApprovalDecision::Approve),
                        "IPC supplies rpc-user and decides the original unchanged target")?;
                    originals.push(original);
                }
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            }
        )?;
        report.observe("access_prompt_response", &response)?;
        tokio::time::timeout(DEADLINE, runtime.wait_idle(&id)).await?;
        let final_state = snapshot(runtime, provider, &context.root, &id)?;
        report.observe("access_approval_final", json!({"snapshot":final_state}))?;
        let events = all_events(runtime, &id)?;
        report.event_page(EventPage {stage:"access_final".into(),session_id:id.clone(),after:0,records:events.clone()})?;
        require(report, "access_approval_durable_terminal", clean(&runtime.get(&id)?) && response == final_state["session"]
            && runtime.tasks(&id)?.len() == 1 && runtime.tasks(&id)?[0].state == TaskState::Completed,
            "One prompt has one completed task and an idle fully paired durable session")?;
        let starts = events.iter().filter_map(|event| match &event.event { CoreEvent::CapabilityStarted { call } => Some((event.cursor, call)), _ => None }).collect::<Vec<_>>();
        let settled = events.iter().filter_map(|event| match &event.event { CoreEvent::CapabilitySettled { call } => Some(call), _ => None }).collect::<Vec<_>>();
        let exact = starts.len() == 2 && settled.len() == 2 && starts.iter().zip(&settled).zip(&originals).all(|(((started_at, start), terminal), original)| {
            let authorization = start.authorization.as_ref().and_then(|value| serde_json::from_value::<CapabilityAuthorization>(value.clone()).ok());
            let decided_at = events.iter().find_map(|event| match &event.event { CoreEvent::ApprovalDecided { approval } if approval.id == original.id => Some(event.cursor), _ => None });
            start.key == terminal.key && start.arguments == terminal.arguments && start.lease == terminal.lease
                && start.task_id == terminal.task_id && start.source_hash == terminal.source_hash && start.parent_call_id == terminal.parent_call_id
                && terminal.authorization == start.authorization && decided_at.is_some_and(|cursor| cursor < *started_at)
                && authorization.is_some_and(|authorization| authorization.actor == "rpc-user" && authorization.target == start.key
                    && authorization.task_id == original.input.task_id && authorization.capability == original.input.capability
                    && authorization.scope == original.input.scope && authorization.approval_id.as_deref() == Some(original.id.as_str())
                    && authorization.decision == ApprovalDecision::Approve)
                && matches!(&terminal.outcome, Some(CapabilityOutcome::Completed { result }) if result["sha256"] == crate::measurement::sha256(CONTENT.as_bytes()))
        });
        require(report, "access_approval_exact_started_authority", exact,
            "Each actual start follows its own original decision and retains exact rpc-user authority through settlement")?;
        report.task_check("access_approved_write_and_read_result", fs::read(context.root.join("workspace/result.txt"))? == CONTENT.as_bytes()
            && final_state["workspace"].as_array().is_some_and(|files| files.iter().any(|file| file["path"] == "result.txt" && file["content"]["bytes"] == json!(CONTENT.as_bytes())))
            && provider.calls() == 2 && exact,
            "Approved Host write and read produced the exact UTF-8 bytes and the Provider consumed the real tool results");
        Ok(())
    }.await;
    execution.close().await;
    control.close().await;
    let cleanup = proxy.close().await;
    report.check(
        "access_approval_connections_stopped",
        cleanup.is_ok(),
        "Both Clients and the recording relay were closed",
    );
    result?;
    cleanup
}

fn bytes_record(bytes: &[u8]) -> Value {
    json!({"bytes":bytes,"length":bytes.len(),"sha256":crate::measurement::sha256(bytes),"utf8":String::from_utf8(bytes.to_vec()).ok()})
}
fn frame(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}
async fn read_closed(reader: &mut BufReader<TcpStream>) -> Result<(Vec<u8>, String)> {
    let mut received = Vec::new();
    loop {
        let mut bytes = [0; 4096];
        match reader.read(&mut bytes).await {
            Ok(0) => return Ok((received, "eof".into())),
            Ok(count) => {
                received.extend_from_slice(&bytes[..count]);
                if received.len() > 16 * 1024 {
                    return Err("raw rejection response exceeded fixture bound".into());
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                ) =>
            {
                return Ok((received, error.kind().to_string()));
            }
            Err(error) => return Err(error.into()),
        }
    }
}
async fn raw_rejection(server: &Server, id: &str, case: &str) -> Result<Value> {
    tokio::time::timeout(DEADLINE, async {
        let mut reader = BufReader::new(TcpStream::connect(server.address).await?);
        let hello = frame(&json!({"version":if case == "wrong-version" { 2 } else { cayw_ipc::PROTOCOL_VERSION },"id":"access-hello","method":"hello",
            "params":{"token":if case == "wrong-token" { "deliberately-wrong-offline-token" } else { TOKEN }}}))?;
        let mut sent = vec![bytes_record(&hello)];
        let mut handshake = Vec::new();
        let prompt = json!({"version":cayw_ipc::PROTOCOL_VERSION,"id":"access-unauthorized-prompt","method":"session.prompt","params":{"session_id":id,"text":PROMPT}});
        if case == "oversized-no-newline" {
            reader.get_mut().write_all(&hello).await?;
            reader.read_until(b'\n', &mut handshake).await?;
            let accepted: Value = serde_json::from_slice(&handshake)?;
            if accepted["result"]["version"] != cayw_ipc::PROTOCOL_VERSION { return Err("oversize case failed to authenticate".into()); }
            let mut oversized = prompt;
            oversized["params"]["text"] = json!("x".repeat(REJECTION_FRAME_BYTES + 1));
            let bytes = serde_json::to_vec(&oversized)?;
            if bytes.len() <= REJECTION_FRAME_BYTES || bytes.contains(&b'\n') { return Err("invalid oversize fixture".into()); }
            reader.get_mut().write_all(&bytes).await?;
            sent.push(bytes_record(&bytes));
        } else {
            let bytes = frame(&prompt)?;
            sent.push(bytes_record(&bytes));
            let mut pipelined = hello;
            pipelined.extend(bytes);
            reader.get_mut().write_all(&pipelined).await?;
        }
        let (received, terminal) = read_closed(&mut reader).await?;
        let response = received.split(|byte| *byte == b'\n').find(|line| !line.is_empty())
            .map(serde_json::from_slice::<Value>).transpose()?;
        Ok(json!({"transport":"direct_tcp","address":server.address,"sent":sent,
            "hello_response_bytes":bytes_record(&handshake),"received":bytes_record(&received),
            "response":response,"closed":true,"closure":terminal}))
    }).await?
}

async fn rejections(
    context: &ScenarioContext,
    report: &mut ScenarioReport,
    runtime: &Runtime,
    provider: &FixedProvider,
    server: &Server,
) -> Result<()> {
    let session = runtime.create(Some("Offline IPC rejection baseline".into()))?;
    report.observe("access_rejection_created", &session)?;
    let initial = snapshot(runtime, provider, &context.root, &session.id)?;
    for (case, code) in [
        ("wrong-token", "unauthorized"),
        ("wrong-version", "unsupported_version"),
        ("oversized-no-newline", ""),
    ] {
        let before = snapshot(runtime, provider, &context.root, &session.id)?;
        let calls_before = server.calls.load(Ordering::Acquire);
        let wire = raw_rejection(server, &session.id, case).await?;
        let calls_after = server.calls.load(Ordering::Acquire);
        let after = snapshot(runtime, provider, &context.root, &session.id)?;
        report.observe(
            "access_rejection",
            json!({"case":case,"before":before,"after":after,"wire":wire,
            "handler_calls_before":calls_before,"handler_calls_after":calls_after}),
        )?;
        require(
            report,
            &format!("access_reject_{case}"),
            wire["closed"] == true
                && if case == "oversized-no-newline" {
                    wire["response"].is_null() && wire["received"]["length"] == 0
                } else {
                    wire["response"]["error"]["code"] == code
                },
            "Actual server rejected the exact direct-socket input and closed its connection",
        )?;
        require(
            report,
            &format!("access_unchanged_{case}"),
            before == initial && before == after && calls_before == calls_after,
            "Rejected authentication/version/frame bytes never reached Runtime and changed no observed business facts",
        )?;
    }
    let mut proxy = RecordingProxy::start(
        server.address,
        &context.root.join("access-ipc-frames.jsonl"),
    )
    .await?;
    let client = Client::connect(proxy.address, TOKEN).await?;
    let result: Result<()> = async {
        for (case, method, params, code) in [
            ("prompt-extra-field", "session.prompt", json!({"session_id":session.id,"text":PROMPT,"extra":true}), "invalid_params"),
            ("approval-forged-actor", "approval.decide", json!({"session_id":session.id,"approval_id":"unissued-approval","decision":"allow_once","actor":"forged-model"}), "invalid_params"),
            ("events-zero-limit", "session.events", json!({"session_id":session.id,"after":0,"limit":0,"wait_ms":0}), "session_error"),
        ] {
            let before = snapshot(runtime, provider, &context.root, &session.id)?;
            let calls_before = server.calls.load(Ordering::Acquire);
            let response = client.request(method, params.clone()).await;
            let calls_after = server.calls.load(Ordering::Acquire);
            let after = snapshot(runtime, provider, &context.root, &session.id)?;
            let error = response.as_ref().err().map(serde_json::to_value).transpose()?;
            report.observe("access_rejection", json!({"case":case,"request":{"method":method,"params":params},
                "before":before,"after":after,"error":error,"unexpected_response":response.as_ref().ok(),
                "wire":null,"frames":"access-ipc-frames.jsonl","handler_calls_before":calls_before,"handler_calls_after":calls_after}))?;
            require(report, &format!("access_reject_{case}"), response.is_err_and(|error| error.code == code && !error.outcome_unknown)
                && calls_after == calls_before + 1,
                "An authenticated request reached actual Runtime parameter validation and returned the expected known error")?;
            require(report, &format!("access_unchanged_{case}"), before == initial && before == after,
                "Invalid Runtime parameters created no task, event, permission, Provider call or file effect")?;
        }
        let final_state = snapshot(runtime, provider, &context.root, &session.id)?;
        report.observe("access_rejection_final", json!({"snapshot":final_state}))?;
        report.task_check("access_all_rejections_without_business_effects", final_state == initial && provider.calls() == 0
            && runtime.tasks(&session.id)?.is_empty() && final_state["workspace"].as_array().is_some_and(Vec::is_empty),
            "All six rejected inputs left the complete original Session/Tasks/events/permissions/Provider/files unchanged");
        Ok(())
    }.await;
    client.close().await;
    let cleanup = proxy.close().await;
    report.check(
        "access_rejection_connections_stopped",
        cleanup.is_ok(),
        "Client and relay were closed after all rejection cases",
    );
    result?;
    cleanup
}
