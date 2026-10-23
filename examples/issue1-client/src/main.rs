use cayw_ai::{CancellationToken, Message};
use cayw_core::{
    ApprovalRequest, CapabilityOutcome, CapabilityReconciliation, CoreEvent, EventRecord,
    ReconciliationOrigin, ReconciliationRequest, ReconciliationVerifier, SessionState,
};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{
    ApprovalChoice, CancelTarget, Runtime, RuntimeConfig, SessionView, TaskRecord, TaskState,
};
use issue1_client::{
    FILE_SCRIPT, FixedProvider, InterruptedService, Result, SavedCursor, TOKEN, UNKNOWN_SCRIPT,
    WATCHDOG, check_origin, open, require, save,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStdout, Command},
    task::JoinHandle,
};

#[derive(Clone)]
enum Api {
    Sdk(Runtime),
    Ipc(Client),
}
impl Api {
    async fn create(&self, label: &str) -> Result<SessionView> {
        match self {
            Self::Sdk(rt) => Ok(rt.create(Some(label.into()))?),
            Self::Ipc(c) => rpc(c, "session.create", json!({"label":label})).await,
        }
    }
    async fn get(&self, id: &str) -> Result<SessionView> {
        match self {
            Self::Sdk(rt) => Ok(rt.get(id)?),
            Self::Ipc(c) => rpc(c, "session.get", json!({"session_id":id})).await,
        }
    }
    async fn prompt(&self, id: &str) -> Result<SessionView> {
        match self {
            Self::Sdk(rt) => Ok(rt
                .prompt(
                    id,
                    "Issue 1 fixed offline request",
                    CancellationToken::new(),
                )
                .await?),
            Self::Ipc(c) => {
                rpc(
                    c,
                    "session.prompt",
                    json!({"session_id":id,"text":"Issue 1 fixed offline request"}),
                )
                .await
            }
        }
    }
    async fn events(&self, id: &str, after: u64, wait_ms: u64) -> Result<Vec<EventRecord>> {
        match self {
            Self::Sdk(rt) => Ok(rt
                .wait_events(
                    id,
                    after,
                    1000,
                    std::time::Duration::from_millis(wait_ms),
                    CancellationToken::new(),
                )
                .await?),
            Self::Ipc(c) => {
                rpc(
                    c,
                    "session.events",
                    json!({"session_id":id,"after":after,"limit":1000,"wait_ms":wait_ms}),
                )
                .await
            }
        }
    }
    async fn approve(
        &self,
        id: &str,
        approval: &str,
        choice: ApprovalChoice,
    ) -> Result<ApprovalRequest> {
        match self {
            Self::Sdk(rt) => Ok(rt.approve(id, approval, choice, "synthetic-sdk-reviewer")?),
            Self::Ipc(c) => {
                rpc(
                    c,
                    "approval.decide",
                    json!({"session_id":id,"approval_id":approval,"decision":choice}),
                )
                .await
            }
        }
    }
    async fn cancel(&self, target: &CancelTarget) -> Result<bool> {
        match self {
            Self::Sdk(rt) => Ok(rt.cancel(target)?),
            Self::Ipc(c) => Ok(c
                .request("session.cancel", serde_json::to_value(target)?)
                .await?["cancelled"]
                == true),
        }
    }
    async fn enqueue(&self, id: &str) -> Result<TaskRecord> {
        match self {
            Self::Sdk(rt) => Ok(rt.enqueue(id, "Issue 1 fixed offline request")?),
            Self::Ipc(c) => {
                rpc(
                    c,
                    "task.enqueue",
                    json!({"session_id":id,"text":"Issue 1 fixed offline request"}),
                )
                .await
            }
        }
    }
    async fn resume(&self, task_id: &str) -> Result<SessionView> {
        match self {
            Self::Sdk(rt) => Ok(rt.resume(task_id, CancellationToken::new()).await?),
            Self::Ipc(c) => rpc(c, "task.resume", json!({"task_id":task_id})).await,
        }
    }
    async fn task(&self, task_id: &str) -> Result<TaskRecord> {
        match self {
            Self::Sdk(rt) => Ok(rt.task(task_id)?),
            Self::Ipc(c) => rpc(c, "task.get", json!({"task_id":task_id})).await,
        }
    }
    async fn tasks(&self, id: &str) -> Result<Vec<TaskRecord>> {
        match self {
            Self::Sdk(rt) => Ok(rt.tasks(id)?),
            Self::Ipc(c) => rpc(c, "session.tasks", json!({"session_id":id})).await,
        }
    }
}
async fn rpc<T: DeserializeOwned>(client: &Client, method: &str, params: Value) -> Result<T> {
    Ok(serde_json::from_value(
        client.request(method, params).await?,
    )?)
}
async fn pending(api: &Api, id: &str, after: &mut u64) -> Result<ApprovalRequest> {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let events = api.events(id, *after, 1000).await?;
            for record in events {
                *after = record.cursor;
                if let CoreEvent::ApprovalRequested { approval } = record.event {
                    return Ok(approval);
                }
            }
        }
    })
    .await?
}
async fn joined<T>(task: JoinHandle<Result<T>>) -> Result<T> {
    tokio::time::timeout(WATCHDOG, task).await??
}
struct Server {
    address: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
}
impl Server {
    async fn start(rt: Runtime, config: ServerConfig) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(serve(listener, Arc::new(rt), config, shutdown.clone()));
        Ok(Self {
            address,
            shutdown,
            task,
        })
    }
    async fn stop(self) -> Result<()> {
        self.shutdown.cancel();
        tokio::time::timeout(WATCHDOG, self.task).await???;
        Ok(())
    }
}

async fn lifecycle(root: &Path, ipc: bool) -> Result<Value> {
    let provider = Arc::new(FixedProvider::new(FILE_SCRIPT, true));
    let rt = open(root, provider.clone(), RuntimeConfig::default())?;
    // Separate open: cancellation cannot rely on the owning Runtime's token map.
    let peer = open(root, provider.clone(), RuntimeConfig::default())?;
    let peer_server = if ipc {
        Some(Server::start(peer.clone(), ServerConfig::new(TOKEN)).await?)
    } else {
        None
    };
    let cancel_api = match &peer_server {
        Some(s) => Api::Ipc(Client::connect(s.address, TOKEN).await?),
        None => Api::Sdk(peer.clone()),
    };
    let server = if ipc {
        Some(Server::start(rt.clone(), ServerConfig::new(TOKEN)).await?)
    } else {
        None
    };
    let api = match &server {
        Some(s) => Api::Ipc(Client::connect(s.address, TOKEN).await?),
        None => Api::Sdk(rt.clone()),
    };
    let mut results = vec![];
    for decision in ["deny", "cancel", "allow"] {
        let session = api.create(decision).await?;
        let admitted = api.enqueue(&session.id).await?;
        let run_api = api.clone();
        let task_id = admitted.id.clone();
        let run = tokio::spawn(async move { run_api.resume(&task_id).await });
        let mut cursor = 0;
        if decision == "deny" {
            tokio::time::timeout(WATCHDOG, provider.first_started.acquire())
                .await??
                .forget();
            let prefix = api.events(&session.id, 0, 0).await?;
            cursor = prefix.last().ok_or("missing active events")?.cursor;
            require(
                api.get(&session.id).await?.attempt.is_some(),
                "request ended before subscription",
            )?;
            let subscription = api.events(&session.id, cursor, 30_000);
            tokio::pin!(subscription);
            require(
                futures::poll!(&mut subscription).is_pending(),
                "active subscription was not waiting",
            )?;
            provider.release_first.add_permits(1);
            let active_events = tokio::time::timeout(WATCHDOG, &mut subscription).await??;
            require(
                !active_events.is_empty(),
                "active subscription returned no progress",
            )?;
            save(&root.join("active-subscription.json"), &active_events)?;
        }
        let approval = pending(&api, &session.id, &mut cursor).await?;
        let before = api.get(&session.id).await?;
        require(
            before.state == SessionState::AwaitingApproval,
            "approval state missing",
        )?;
        require(
            !root.join("workspace/result.txt").exists(),
            "file effect bypassed approval",
        )?;
        require(
            api.approve(&session.id, "forged-approval", ApprovalChoice::AllowTask)
                .await
                .is_err(),
            "forged approval accepted",
        )?;
        let cancel_target = api.task(&admitted.id).await?.cancel_target();
        match decision {
            "deny" => {
                api.approve(&session.id, &approval.id, ApprovalChoice::Deny)
                    .await?;
            }
            "cancel" => {
                require(
                    cancel_api.cancel(&cancel_target).await?,
                    "peer cancel did not target active task",
                )?;
                require(
                    api.approve(&session.id, &approval.id, ApprovalChoice::AllowTask)
                        .await
                        .is_err(),
                    "approval after accepted peer cancellation was allowed",
                )?;
            }
            _ => {
                api.approve(&session.id, &approval.id, ApprovalChoice::AllowOnce)
                    .await?;
                let read = pending(&api, &session.id, &mut cursor).await?;
                require(
                    read.id != approval.id && read.input.capability == "fs.read",
                    "allow_once escaped exact capability",
                )?;
                api.approve(&session.id, &read.id, ApprovalChoice::AllowOnce)
                    .await?;
            }
        }
        let result = joined(run).await?;
        require(
            result.id == session.id && result.attempt.is_none(),
            "session identity or terminal lost",
        )?;
        require(
            result.pending_approvals.is_empty(),
            "approval remains pending after terminal",
        )?;
        require(
            api.approve(&session.id, &approval.id, ApprovalChoice::AllowTask)
                .await
                .is_err(),
            "late approval revived terminal",
        )?;
        let task = api.tasks(&session.id).await?;
        require(task.len() == 1, "request was replayed")?;
        if decision == "cancel" {
            require(
                task[0].state == TaskState::Cancelled && task[0].cancel_requested,
                "cancellation lost",
            )?;
            require(
                !api.cancel(&cancel_target).await?,
                "repeat cancellation changed terminal task",
            )?;
        }
        let terminal = api.get(&session.id).await?;
        let again = api.tasks(&session.id).await?;
        require(again == task, "late operation changed task terminal")?;
        require(
            api.get(&session.id).await? == terminal,
            "late operation changed session terminal",
        )?;
        let events = api.events(&session.id, 0, 0).await?;
        SavedCursor {
            session_id: session.id.clone(),
            after: 0,
        }
        .advance(&events)?;
        let origin = check_origin(&events)?;
        save(&root.join(format!("{decision}-events.json")), &events)?;
        save(&root.join(format!("{decision}-session.json")), &terminal)?;
        save(&root.join(format!("{decision}-tasks.json")), &task)?;
        if decision == "allow" {
            require(
                std::fs::read(root.join("workspace/result.txt"))? == b"Issue 1 local effect",
                "real Host file round trip failed",
            )?;
            require(
                origin["sequences"] == json!([0, 1]),
                "Core capability sequence is not contiguous",
            )?;
        } else {
            require(
                !root.join("workspace/result.txt").exists(),
                "denial/cancel performed file effect",
            )?;
        }
        if decision == "cancel" {
            let audit = peer.audit(&session.id, 0, 1000)?;
            let terminals = audit
                .iter()
                .filter(|event| {
                    event.task_id.as_ref() == Some(&task[0].id) && event.kind.starts_with("run_")
                })
                .collect::<Vec<_>>();
            require(
                terminals.len() == 1 && terminals[0].kind == "run_cancelled",
                "peer cancellation audit contradicts task",
            )?;
            save(&root.join("cancel-audit.json"), &audit)?;
            let original = before
                .attempt
                .as_ref()
                .ok_or("missing original cancellation lease")?;
            let run_api = api.clone();
            let id = session.id.clone();
            let replacement = tokio::spawn(async move { run_api.prompt(&id).await });
            let fresh = pending(&api, &session.id, &mut cursor).await?;
            let current = api
                .get(&session.id)
                .await?
                .attempt
                .ok_or("missing replacement lease")?;
            require(
                fresh.id != approval.id
                    && fresh.attempt_id != approval.attempt_id
                    && current.worker_id != original.worker_id
                    && current.fence > original.fence,
                "replacement reused cancelled authority",
            )?;
            require(
                !root.join("workspace/result.txt").exists(),
                "replacement skipped fresh permission",
            )?;
            require(
                api.approve(&session.id, &approval.id, ApprovalChoice::AllowOnce)
                    .await
                    .is_err(),
                "stale approval authorized replacement",
            )?;
            api.approve(&session.id, &fresh.id, ApprovalChoice::Deny)
                .await?;
            joined(replacement).await?;
            require(
                !root.join("workspace/result.txt").exists(),
                "replacement denial performed effect",
            )?;
            let after = api.tasks(&session.id).await?;
            require(
                after.len() == 2 && after[0] == task[0],
                "replacement changed original terminal",
            )?;
            save(&root.join("cancel-replacement-tasks.json"), &after)?;
            save(
                &root.join("cancel-replacement-events.json"),
                &api.events(&session.id, 0, 0).await?,
            )?;
            save(
                &root.join("cancel-peer.json"),
                &json!({"separately_opened_runtime":true,"authenticated_ipc":ipc,"accepted":true,"late_allow_rejected":true,"no_file_effect":true,"fresh_replacement_approval":fresh,"original_lease":original,"replacement_lease":current,"single_cancel_terminal":true}),
            )?;
        }
        results.push(json!({"decision":decision,"origin":origin,"session_id":session.id,"task_state":task[0].state,"peer_runtime_cancel":decision=="cancel","passed":true}));
    }
    if let Api::Ipc(client) = cancel_api {
        client.close().await;
    }
    if let Some(server) = peer_server {
        server.stop().await?;
    }
    if let Api::Ipc(client) = api {
        client.close().await;
    }
    if let Some(server) = server {
        server.stop().await?;
    }
    require(
        provider.requests.load(Ordering::SeqCst) >= 4,
        "provider was not called",
    )?;
    let result =
        json!({"mode":if ipc {"ipc"} else {"sdk"},"synthetic_decisions":true,"cases":results});
    save(&root.join("result.json"), &result)?;
    Ok(result)
}

async fn reconnect(root: &Path) -> Result<Value> {
    let provider = Arc::new(FixedProvider::new(FILE_SCRIPT, false));
    let rt = open(root, provider.clone(), RuntimeConfig::default())?;
    let server = Server::start(rt.clone(), ServerConfig::new(TOKEN)).await?;
    let control = Client::connect(server.address, TOKEN).await?;
    let event_client = Client::connect(server.address, TOKEN).await?;
    let api = Api::Ipc(control.clone());
    let session = api.create("saved cursor").await?;
    let run_api = api.clone();
    let id = session.id.clone();
    let run = tokio::spawn(async move { run_api.prompt(&id).await });
    let approval = pending(&api, &session.id, &mut 0).await?;
    let prefix: Vec<EventRecord> = rpc(
        &event_client,
        "session.events",
        json!({"session_id":session.id,"after":0,"limit":3}),
    )
    .await?;
    require(prefix.len() == 3, "prefix did not use bounded page")?;
    let checkpoint = SavedCursor {
        session_id: session.id.clone(),
        after: 0,
    }
    .advance(&prefix)?;
    save(&root.join("processed-prefix.json"), &prefix)?;
    save(&root.join("saved-cursor.json"), &checkpoint)?; // sync file and parent BEFORE close
    event_client.close().await;
    save(
        &root.join("disconnected.json"),
        &json!({"saved_cursor_was_synced_before_close":true,"after":checkpoint.after}),
    )?;
    api.approve(&session.id, &approval.id, ApprovalChoice::Deny)
        .await?;
    joined(run).await?;
    let replay = Client::connect(server.address, TOKEN).await?;
    let from_disk: SavedCursor =
        serde_json::from_slice(&std::fs::read(root.join("saved-cursor.json"))?)?;
    let mut current = SavedCursor {
        session_id: from_disk.session_id.clone(),
        after: from_disk.after,
    };
    let mut suffix = vec![];
    loop {
        let page: Vec<EventRecord> = rpc(
            &replay,
            "session.events",
            json!({"session_id":current.session_id,"after":current.after,"limit":3}),
        )
        .await?;
        if page.is_empty() {
            break;
        }
        current = current.advance(&page)?;
        suffix.extend(page);
    }
    let full = rt.events(&session.id, 0, 1000)?;
    let expected = full
        .iter()
        .filter(|event| event.cursor > from_disk.after)
        .cloned()
        .collect::<Vec<_>>();
    require(
        suffix == expected,
        "resumed events differ from exact durable suffix",
    )?;
    let mut combined = prefix;
    combined.extend(suffix.clone());
    require(
        combined == full,
        "saved prefix plus suffix has gap or duplicate",
    )?;
    let view = api.get(&session.id).await?;
    let users = view
        .messages
        .iter()
        .filter(|m| matches!(m, Message::User { .. }))
        .count();
    require(
        users == 1 && api.tasks(&session.id).await?.len() == 1,
        "reconnect replayed prompt",
    )?;
    save(&root.join("resumed-suffix.json"), &suffix)?;
    save(&root.join("durable-full-log.json"), &full)?;
    let result = json!({"passed":true,"saved_cursor":from_disk,"final_cursor":current.after,"suffix_count":suffix.len(),"prompt_count":users,"exact_suffix":true,"gap_or_duplicate":false});
    save(&root.join("result.json"), &result)?;
    replay.close().await;
    control.close().await;
    server.stop().await?;
    Ok(result)
}

async fn wire_reply(stream: &mut BufReader<TcpStream>) -> Result<Value> {
    let mut line = String::new();
    let bytes = tokio::time::timeout(WATCHDOG, stream.read_line(&mut line)).await??;
    require(bytes != 0, "Runtime closed before paired response")?;
    Ok(serde_json::from_str(&line)?)
}
async fn wire_send(
    stream: &mut BufReader<TcpStream>,
    id: &str,
    method: &str,
    params: Value,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(
        &json!({"version":cayw_ipc::PROTOCOL_VERSION,"id":id,"method":method,"params":params}),
    )?;
    bytes.push(b'\n');
    stream.get_mut().write_all(&bytes).await?;
    Ok(())
}
async fn runtime_wire_pairing(root: &Path, address: SocketAddr, control: &Api) -> Result<Value> {
    let session = control.create("real Runtime RPC pairing").await?;
    let mut stream = BufReader::new(TcpStream::connect(address).await?);
    wire_send(&mut stream, "hello-raw", "hello", json!({"token":TOKEN})).await?;
    let hello = wire_reply(&mut stream).await?;
    require(
        hello["id"] == "hello-raw" && hello["result"]["version"] == cayw_ipc::PROTOCOL_VERSION,
        "raw hello pairing failed",
    )?;
    wire_send(
        &mut stream,
        "original-prompt",
        "session.prompt",
        json!({"session_id":session.id,"text":"cancel this exact Runtime RPC"}),
    )
    .await?;
    let approval = pending(control, &session.id, &mut 0).await?;
    // This connection's only handler slot is occupied. A raw caller still sees
    // server busy, whereas the public Client rejects before sending.
    wire_send(&mut stream, "overflow", "runtime.health", json!({})).await?;
    let busy = wire_reply(&mut stream).await?;
    require(
        busy["id"] == "overflow" && busy["error"]["code"] == "busy",
        "server did not enforce its handler capacity",
    )?;
    wire_send(
        &mut stream,
        "cancel-ack",
        "rpc.cancel",
        json!({"request_id":"original-prompt"}),
    )
    .await?;
    let replies = vec![
        wire_reply(&mut stream).await?,
        wire_reply(&mut stream).await?,
    ];
    let original = replies
        .iter()
        .find(|r| r["id"] == "original-prompt")
        .ok_or("original Runtime response missing")?;
    let ack = replies
        .iter()
        .find(|r| r["id"] == "cancel-ack")
        .ok_or("cancellation acknowledgment missing")?;
    require(
        original["error"]["code"] == "cancelled_outcome_unknown"
            && ack["result"]["cancelled"] == true,
        "Runtime RPC cancellation responses were mispaired",
    )?;
    let mut after = 0;
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if control.get(&session.id).await?.attempt.is_none() {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            let events = control.events(&session.id, after, 1000).await?;
            if let Some(last) = events.last() {
                after = last.cursor;
            }
        }
    })
    .await??;
    let tasks = control.tasks(&session.id).await?;
    require(
        tasks[0].state == TaskState::Cancelled,
        "wire cancel did not stop Runtime task",
    )?;
    require(
        control
            .approve(&session.id, &approval.id, ApprovalChoice::AllowOnce)
            .await
            .is_err(),
        "wire cancelled approval was revived",
    )?;
    require(
        !root.join("workspace/result.txt").exists(),
        "wire cancellation dispatched pending file effect",
    )?;
    // Already settled request IDs cannot produce a second original terminal.
    wire_send(
        &mut stream,
        "late-cancel",
        "rpc.cancel",
        json!({"request_id":"original-prompt"}),
    )
    .await?;
    let late = wire_reply(&mut stream).await?;
    require(
        late["id"] == "late-cancel" && late["result"]["cancelled"] == false,
        "late cancellation revived original request",
    )?;
    wire_send(&mut stream, "after-cancel", "runtime.health", json!({})).await?;
    let health = wire_reply(&mut stream).await?;
    require(
        health["id"] == "after-cancel" && health["result"]["status"] == "ok",
        "duplicate original terminal or broken request pairing",
    )?;
    let result = json!({"hello":hello,"server_busy":busy,"cancel_responses":replies,"late_cancel":late,"subsequent_response":health,"task":tasks[0],"passed":true});
    save(&root.join("runtime-wire-pairing.json"), &result)?;
    stream.get_mut().shutdown().await?;
    Ok(result)
}

async fn transport(root: &Path) -> Result<Value> {
    let provider = Arc::new(FixedProvider::new(FILE_SCRIPT, true));
    let rt = open(root, provider.clone(), RuntimeConfig::default())?;
    let mut config = ServerConfig::new(TOKEN);
    config.max_in_flight = 1;
    let server = Server::start(rt.clone(), config).await?;
    let mut raw = BufReader::new(TcpStream::connect(server.address).await?);
    raw.get_mut()
        .write_all(b"{\"version\":2,\"id\":\"incompatible\",\"method\":\"hello\",\"params\":{}}\n")
        .await?;
    let mut version_line = String::new();
    tokio::time::timeout(WATCHDOG, raw.read_line(&mut version_line)).await??;
    let version: Value = serde_json::from_str(&version_line)?;
    require(
        version["id"] == "incompatible" && version["error"]["code"] == "unsupported_version",
        "wire mismatch did not return explicit error",
    )?;
    let mut eof = [0u8; 1];
    require(
        tokio::time::timeout(WATCHDOG, raw.read(&mut eof)).await?? == 0,
        "incompatible connection left open",
    )?;
    save(&root.join("incompatible-wire-version.json"), &version)?;
    let auth_state = || -> Result<Value> {
        Ok(json!({"sessions":rt.list()?,"health":rt.health()?,
            "provider_requests":provider.requests.load(Ordering::SeqCst),
            "file_exists":root.join("workspace/result.txt").exists()}))
    };
    let before = auth_state()?;
    let rejected = Client::connect(server.address, "wrong-public-fixture-token").await;
    let after = auth_state()?;
    save(
        &root.join("authentication-rejection.json"),
        &json!({"error":rejected.as_ref().err(),"before":before,"after":after,
            "provider_requests":provider.requests.load(Ordering::SeqCst),"sessions":rt.list()?}),
    )?;
    require(
        matches!(rejected,Err(ref error) if error.code == "unauthorized" && !error.outcome_unknown),
        "authentication was bypassed",
    )?;
    require(
        before == after && rt.list()?.is_empty() && provider.requests.load(Ordering::SeqCst) == 0,
        "rejected authentication changed business state",
    )?;
    let client = Client::connect(server.address, TOKEN).await?;
    let control = Client::connect(server.address, TOKEN).await?;
    let api = Api::Ipc(control.clone());
    let session = api.create("backpressure and disconnect").await?;
    let state = || -> Result<Value> {
        Ok(json!({"session":rt.get(&session.id)?,"sessions":rt.list()?,
            "events":rt.events(&session.id,0,1000)?,"tasks":rt.tasks(&session.id)?,
            "grants":rt.grants(&session.id)?,"audit":rt.audit(&session.id,0,1000)?,
            "health":rt.health()?,"provider_requests":provider.requests.load(Ordering::SeqCst),
            "file_exists":root.join("workspace/result.txt").exists()}))
    };
    let initial = state()?;
    let mut rejections = Vec::new();
    for (case, method, params, code) in [
        (
            "prompt-extra-field",
            "session.prompt",
            json!({"session_id":session.id,"text":"must not execute","extra":true}),
            "invalid_params",
        ),
        (
            "prompt-missing-text",
            "session.prompt",
            json!({"session_id":session.id}),
            "invalid_params",
        ),
        (
            "events-zero-limit",
            "session.events",
            json!({"session_id":session.id,"after":0,"limit":0,"wait_ms":0}),
            "session_error",
        ),
    ] {
        let before = state()?;
        let response = control.request(method, params.clone()).await;
        let after = state()?;
        rejections.push(
            json!({"case":case,"request":{"method":method,"params":params},
            "error":response.as_ref().err(),"unexpected_response":response.as_ref().ok(),
            "before":before,"after":after}),
        );
        save(
            &root.join(format!("parameter-rejection-{case}.json")),
            rejections.last().ok_or("rejection observation missing")?,
        )?;
        require(
            response.is_err_and(|error| error.code == code && !error.outcome_unknown),
            "invalid parameters were not explicitly rejected",
        )?;
        require(
            before == initial && before == after,
            "invalid parameters changed session, tasks, events, permissions or effects",
        )?;
    }
    save(&root.join("parameter-rejections.json"), &rejections)?;
    let run_client = client.clone();
    let id = session.id.clone();
    let run = tokio::spawn(async move {
        run_client
            .request(
                "session.prompt",
                json!({"session_id":id,"text":"hold until connection closes"}),
            )
            .await
    });
    tokio::time::timeout(WATCHDOG, provider.first_started.acquire())
        .await??
        .forget();
    let busy = client
        .request("runtime.health", json!({}))
        .await
        .unwrap_err();
    require(
        busy.code == "client_busy" && !busy.outcome_unknown,
        "backpressure not rejected before dispatch",
    )?;
    let over_frame = control
        .request(
            "session.prompt",
            json!({"session_id":session.id,"text":"x".repeat(1024*1024+1)}),
        )
        .await
        .unwrap_err();
    require(
        !over_frame.outcome_unknown,
        "oversize local frame was dispatched",
    )?;
    let during = api.get(&session.id).await?;
    require(during.attempt.is_some(), "held request not active")?;
    client.close().await;
    let original = tokio::time::timeout(WATCHDOG, run).await??.unwrap_err();
    require(
        original.outcome_unknown,
        "disconnect erased original response uncertainty",
    )?;
    tokio::time::timeout(WATCHDOG, rt.wait_idle(&session.id)).await?;
    let terminal = api.get(&session.id).await?;
    let tasks = api.tasks(&session.id).await?;
    require(
        terminal.attempt.is_none() && tasks[0].state == TaskState::Cancelled,
        "disconnect did not persist cancellation",
    )?;
    require(
        !root.join("workspace/result.txt").exists(),
        "disconnect caused file effect",
    )?;
    require(
        provider.requests.load(Ordering::SeqCst) == 1,
        "disconnect retried provider",
    )?;
    let wire_pairing = runtime_wire_pairing(root, server.address, &api).await?;
    let result = json!({"passed":true,"wire_version":cayw_ipc::PROTOCOL_VERSION,"core_version":cayw_core::PROTOCOL_VERSION,
        "real_runtime_wire_pairing":wire_pairing,"backpressure":busy,"oversize":over_frame,"original_disconnect_error":original,"task":tasks[0],"no_automatic_replay":true});
    save(&root.join("result.json"), &result)?;
    save(&root.join("events.json"), &rt.events(&session.id, 0, 1000)?)?;
    control.close().await;
    server.stop().await?;
    Ok(result)
}

struct HostProcess {
    child: Child,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
    client: Client,
    pid: u32,
}
impl HostProcess {
    async fn start(root: &Path, unknown: bool, label: &str) -> Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("host")
            .arg(root)
            .arg(if unknown { "unknown" } else { "before" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(root.join(format!("{label}-stderr.log")))?,
            )
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().ok_or("child has no pid")?;
        let mut lines = BufReader::new(child.stdout.take().ok_or("child has no stdout")?).lines();
        let ready = tokio::time::timeout(WATCHDOG, lines.next_line())
            .await??
            .ok_or("child exited before ready")?;
        let ready: Value = serde_json::from_str(&ready)?;
        require(
            ready["pid"] == pid && ready["offline_fixture"] == true,
            "bad child readiness",
        )?;
        save(&root.join(format!("{label}-ready.json")), &ready)?;
        let client = Client::connect(
            ready["address"]
                .as_str()
                .ok_or("missing child address")?
                .parse()?,
            TOKEN,
        )
        .await?;
        Ok(Self {
            child,
            lines,
            client,
            pid,
        })
    }
    async fn effect_marker(&mut self) -> Result<Value> {
        let line = tokio::time::timeout(WATCHDOG, self.lines.next_line())
            .await??
            .ok_or("child exited before effect")?;
        let marker: Value = serde_json::from_str(&line)?;
        require(marker["marker"] == "effect-synced", "effect marker missing")?;
        Ok(marker)
    }
    async fn kill_and_confirm(&mut self, root: &Path, label: &str) -> Result<()> {
        self.child.start_kill()?;
        let status = tokio::time::timeout(WATCHDOG, self.child.wait()).await??;
        require(
            self.child.id().is_none() && self.child.try_wait()?.is_some(),
            "original child exit unconfirmed",
        )?;
        save(
            &root.join(format!("{label}-exit.json")),
            &json!({"pid":self.pid,"wait_returned":true,"try_wait_confirmed":true,"success":status.success(),"status":status.to_string()}),
        )?;
        self.client.close().await;
        Ok(())
    }
    async fn stop(mut self, root: &Path) -> Result<Value> {
        self.client.close().await;
        self.child
            .stdin
            .take()
            .ok_or("child stdin missing")?
            .write_all(b"stop\n")
            .await?;
        let stopped = tokio::time::timeout(WATCHDOG, self.lines.next_line())
            .await??
            .ok_or("reopened host did not report stopped counters")?;
        let stopped: Value = serde_json::from_str(&stopped)?;
        require(
            stopped["marker"] == "host-stopped",
            "host stop marker missing",
        )?;
        save(&root.join("reopened-stopped.json"), &stopped)?;
        let status = tokio::time::timeout(WATCHDOG, self.child.wait()).await??;
        require(status.success(), "reopened host failed to stop")?;
        save(
            &root.join("reopened-exit.json"),
            &json!({"pid":self.pid,"wait_returned":true,"status":status.to_string()}),
        )?;
        Ok(stopped)
    }
}

async fn process_recovery(root: &Path, unknown: bool) -> Result<Value> {
    std::fs::create_dir_all(root)?;
    let mut original = HostProcess::start(root, unknown, "original").await?;
    let api = Api::Ipc(original.client.clone());
    let session = api.create("same logical session across host exit").await?;
    let run_api = api.clone();
    let id = session.id.clone();
    let run = tokio::spawn(async move { run_api.prompt(&id).await });
    let mut cursor = 0;
    let approval = pending(&api, &session.id, &mut cursor).await?;
    let active_recover = original
        .client
        .request("session.recover", json!({"session_id":session.id}))
        .await
        .unwrap_err();
    require(
        matches!(active_recover.code.as_str(), "busy" | "runtime_error"),
        "live owner recovery returned unexpected error",
    )?;
    save(
        &root.join("live-owner-recovery-rejected.json"),
        &active_recover,
    )?;
    require(
        api.get(&session.id).await?.state == SessionState::AwaitingApproval,
        "child not blocked at approval",
    )?;
    if unknown {
        api.approve(&session.id, &approval.id, ApprovalChoice::AllowOnce)
            .await?;
        let marker = original.effect_marker().await?;
        save(&root.join("observed-effect-marker.json"), &marker)?;
    }
    let before = api.get(&session.id).await?;
    let old_lease = before.attempt.clone().ok_or("missing original worker")?;
    let old_task = api.tasks(&session.id).await?.remove(0);
    let prefix = api.events(&session.id, 0, 0).await?;
    let checkpoint = SavedCursor {
        session_id: session.id.clone(),
        after: 0,
    }
    .advance(&prefix)?;
    save(&root.join("saved-cursor.json"), &checkpoint)?;
    save(&root.join("before-session.json"), &before)?;
    save(&root.join("before-events.json"), &prefix)?;
    original.kill_and_confirm(root, "original").await?; // Reopen occurs strictly after wait().
    let lost = joined(run).await.unwrap_err().to_string();
    save(
        &root.join("lost-request.json"),
        &json!({"error":lost,"not_retried":true}),
    )?;
    let reopened = HostProcess::start(root, unknown, "reopened").await?;
    let api = Api::Ipc(reopened.client.clone());
    let retained = api.get(&session.id).await?;
    require(
        retained.id == session.id && retained.attempt == Some(old_lease.clone()),
        "reopen replaced logical session or silently changed worker",
    )?;
    require(
        api.tasks(&session.id).await? == vec![old_task.clone()],
        "reopen silently replayed task",
    )?;
    let tasks_before_recovery = api.tasks(&session.id).await?;
    require(
        reopened
            .client
            .request("task.resume", json!({"task_id":old_task.id}))
            .await
            .is_err(),
        "running task automatically resumed",
    )?;
    let recovered: SessionView = rpc(
        &reopened.client,
        "session.recover",
        json!({"session_id":session.id}),
    )
    .await?;
    require(
        recovered.id == session.id && recovered.attempt.is_none(),
        "recovery lost session or kept original lease",
    )?;
    require(
        api.approve(&session.id, &approval.id, ApprovalChoice::AllowTask)
            .await
            .is_err(),
        "recovery revived old approval",
    )?;
    let mut replacement = Value::Null;
    if unknown {
        require(
            recovered.state == SessionState::NeedsReconciliation
                && recovered.unresolved_capabilities.len() == 1,
            "started effect not retained as blocking unknown",
        )?;
        let call = &recovered.unresolved_capabilities[0];
        require(
            call.lease == old_lease && call.task_id == old_task.id && call.key.sequence == 0,
            "unknown lost original owner",
        )?;
        require(
            matches!(call.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. })),
            "incomplete started effect was made known",
        )?;
        let tasks_before_rejection = api.tasks(&session.id).await?;
        let error = reopened
            .client
            .request(
                "session.prompt",
                json!({"session_id":session.id,"text":"must not replay unknown"}),
            )
            .await
            .unwrap_err();
        save(&root.join("blocked-new-work.json"), &error)?;
        let blocked_session = api.get(&session.id).await?;
        let blocked_tasks = api.tasks(&session.id).await?;
        save(
            &root.join("blocked-new-work-state.json"),
            &json!({"session":blocked_session,"tasks":blocked_tasks}),
        )?;
        require(
            error.code == "needs_reconciliation_outcome_unknown"
                && error.outcome_unknown
                && blocked_session == recovered
                && blocked_tasks == tasks_before_rejection,
            &format!(
                "unknown prompt returned an unexpected rejection or changed original records: {}",
                error.code
            ),
        )?;
        require(
            reopened
                .client
                .request(
                    "session.fork",
                    json!({"session_id":session.id,"label":"bypass","confirm":true,"mode":"chat"}),
                )
                .await
                .is_err(),
            "fork bypassed unknown",
        )?;
        require(
            !root.join("synthetic-effect-1.json").exists(),
            "recovery replayed synthetic effect",
        )?;
        let original_effect: Value =
            serde_json::from_slice(&std::fs::read(root.join("synthetic-effect-0.json"))?)?;
        require(
            original_effect["pid"] == original.pid
                && original_effect["capability"] == call.name
                && original_effect["arguments"] == call.arguments,
            "effect receipt does not match original dispatch",
        )?;
        let second: SessionView = rpc(
            &reopened.client,
            "session.recover",
            json!({"session_id":session.id}),
        )
        .await?;
        require(
            second.unresolved_capabilities == recovered.unresolved_capabilities,
            "repeat recovery cleared unknown",
        )?;
        save(
            &root.join("reopened-unknown-before-reconciliation.json"),
            &second,
        )?;
        // Reconciliation uses the original public API and actual synced receipt.
        let repaired: SessionView = rpc(&reopened.client,"capability.reconcile",json!({"session_id":session.id,
            "reconciliation":CapabilityReconciliation { key:call.key.clone(),expected:Box::new(ReconciliationOrigin::from(call)),actor:"rpc-user".into(),
                evidence:RECEIPT_EVIDENCE.into(),
                outcome:CapabilityOutcome::Completed { result:original_effect },external_provenance:call.external_provenance.clone() }
        })).await?;
        require(
            repaired.state == SessionState::Idle && repaired.unresolved_capabilities.is_empty(),
            "explicit reconciliation did not repair Core and Cells",
        )?;
        save(&root.join("explicitly-reconciled.json"), &repaired)?;
    } else {
        require(
            recovered.state == SessionState::Idle
                && recovered.unresolved_calls.is_empty()
                && recovered.unresolved_capabilities.is_empty(),
            "unstarted work not recovered as not executed",
        )?;
        require(
            !root.join("workspace/result.txt").exists(),
            "unapproved work replayed on recovery",
        )?;
        // Explicit new work gets a new task, attempt and worker in the same session.
        let run_api = api.clone();
        let id = session.id.clone();
        let next = tokio::spawn(async move { run_api.prompt(&id).await });
        let mut after = recovered.cursor;
        let pending = pending(&api, &session.id, &mut after).await?;
        let running = api.get(&session.id).await?;
        let new_lease = running
            .attempt
            .as_ref()
            .ok_or("replacement attempt missing")?;
        require(
            new_lease.session_id == old_lease.session_id
                && new_lease.attempt_id != old_lease.attempt_id
                && new_lease.worker_id != old_lease.worker_id
                && new_lease.fence > old_lease.fence,
            "replacement did not receive a new execution identity",
        )?;
        api.approve(&session.id, &pending.id, ApprovalChoice::Deny)
            .await?;
        joined(next).await?;
        replacement = serde_json::to_value(new_lease)?;
    }
    let disk: SavedCursor =
        serde_json::from_slice(&std::fs::read(root.join("saved-cursor.json"))?)?;
    let suffix = api.events(&session.id, disk.after, 0).await?;
    disk.advance(&suffix)?;
    let full = api.events(&session.id, 0, 0).await?;
    let mut combined = prefix;
    combined.extend(suffix.clone());
    require(
        combined == full,
        "process restart event resume differs from durable suffix",
    )?;
    let after_tasks = api.tasks(&session.id).await?;
    require(
        after_tasks[0].state == TaskState::Interrupted
            && after_tasks[0].id == old_task.id
            && after_tasks[0].worker_id == old_task.worker_id
            && after_tasks[0].attempt_id == old_task.attempt_id,
        "recovery rewrote original task ownership",
    )?;
    if !unknown {
        require(
            after_tasks.len() == 2 && after_tasks[1].id != old_task.id,
            "replacement reused original task",
        )?;
    }
    save(&root.join("after-tasks.json"), &after_tasks)?;
    save(&root.join("after-recovery.json"), &recovered)?;
    save(&root.join("after-events.json"), &full)?;
    save(&root.join("resumed-suffix.json"), &suffix)?;
    let reopened_pid = reopened.pid;
    let stopped = reopened.stop(root).await?;
    require(
        stopped["service_calls"] == 0,
        "replacement host replayed service effect",
    )?;
    if unknown {
        require(
            stopped["provider_requests"] == 0,
            "unknown recovery called the Provider",
        )?;
    } else {
        require(
            stopped["provider_requests"].as_u64().is_some_and(|n| n > 0),
            "explicit replacement task did not call Provider",
        )?;
    }
    let result = json!({"passed":true,"unknown_scenario":unknown,"same_session_id":session.id,"old_lease":old_lease,
        "replacement_lease":replacement,"original_pid":original.pid,"reopened_pid":reopened_pid,
        "original_exit_confirmed_before_reopen":true,"tasks_before_recovery":tasks_before_recovery,
        "replacement_host_stopped":stopped,"automatic_replay":false,"saved_cursor":disk,"synthetic":true});
    save(&root.join("result.json"), &result)?;
    Ok(result)
}

const RECEIPT_EVIDENCE: &str = "Original child exit was awaited; synthetic-effect-0.json was synced before exit and records this exact capability/arguments";

/// This fixture accepts only its original synced receipt and confirmed child exit.
struct ReceiptVerifier(PathBuf);
impl ReconciliationVerifier for ReceiptVerifier {
    fn verify(&self, request: &ReconciliationRequest) -> std::result::Result<(), String> {
        self.verify_receipt(request)
            .map_err(|error| error.to_string())
    }
}
impl ReceiptVerifier {
    fn verify_receipt(&self, request: &ReconciliationRequest) -> Result<()> {
        let ReconciliationRequest::Capability { reconciliation } = request else {
            return Err("fixture has no model-tool receipt verifier".into());
        };
        require(
            reconciliation.actor == "rpc-user" && reconciliation.evidence == RECEIPT_EVIDENCE,
            "fixture reconciliation actor or evidence is not authorized",
        )?;
        let read = |name: &str| -> Result<Value> {
            Ok(serde_json::from_slice(&std::fs::read(self.0.join(name))?)?)
        };
        let before: SessionView = serde_json::from_value(read("before-session.json")?)?;
        let call = before
            .unresolved_capabilities
            .iter()
            .find(|call| call.key == reconciliation.key)
            .ok_or("receipt has no original capability")?;
        let exit = read("original-exit.json")?;
        let ready = read("original-ready.json")?;
        let receipt = read("synthetic-effect-0.json")?;
        require(
            exit["wait_returned"] == true
                && exit["try_wait_confirmed"] == true
                && exit["pid"] == ready["pid"]
                && receipt["pid"] == exit["pid"]
                && receipt["pid"]
                    .as_u64()
                    .is_some_and(|pid| pid != u64::from(std::process::id()))
                && receipt["synthetic"] == true
                && receipt["ordinal"] == 0,
            "original fixture child exit or synced receipt is unconfirmed",
        )?;
        require(
            *reconciliation.expected == ReconciliationOrigin::from(call)
                && before.attempt.as_ref() == Some(&call.lease)
                && receipt["capability"] == call.name
                && receipt["arguments"] == call.arguments
                && reconciliation.external_provenance == call.external_provenance
                && reconciliation.outcome == (CapabilityOutcome::Completed { result: receipt }),
            "reconciliation differs from the original fixture identity or receipt",
        )
    }
}

async fn host(root: &Path, unknown: bool) -> Result<()> {
    let provider = Arc::new(FixedProvider::new(
        if unknown { UNKNOWN_SCRIPT } else { FILE_SCRIPT },
        false,
    ));
    let service = Arc::new(InterruptedService {
        root: root.into(),
        calls: AtomicUsize::new(0),
    });
    let mut config = RuntimeConfig::default();
    if unknown {
        config.host.external = Some(service.clone());
        config.reconciliation_verifier = Some(Arc::new(ReceiptVerifier(root.into())));
    }
    let rt = open(root, provider.clone(), config)?;
    let server = Server::start(rt, ServerConfig::new(TOKEN)).await?;
    println!(
        "{}",
        json!({"address":server.address,"pid":std::process::id(),"offline_fixture":true})
    );
    std::io::stdout().flush()?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let line = lines.next_line().await?;
    require(
        line.as_deref() == Some("stop"),
        "host expected stop command",
    )?;
    server.stop().await?;
    println!(
        "{}",
        json!({"marker":"host-stopped","pid":std::process::id(),
        "provider_requests":provider.requests.load(Ordering::SeqCst),
        "service_calls":service.calls.load(Ordering::SeqCst)})
    );
    std::io::stdout().flush()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    require(
        args.len() >= 2,
        "usage: issue1-client <all|sdk|ipc|host> <new-output-directory> [before|unknown]",
    )?;
    let mode = &args[0];
    require(
        matches!(mode.as_str(), "all" | "sdk" | "ipc" | "host"),
        "unknown mode",
    )?;
    let root = PathBuf::from(&args[1]);
    require(root.is_absolute(), "output directory must be absolute")?;
    if mode == "host" {
        return host(&root, args.get(2).is_some_and(|v| v == "unknown")).await;
    }
    std::fs::create_dir(&root)?;
    let mut results = vec![];
    if mode == "all" || mode == "sdk" {
        results.push(lifecycle(&root.join("sdk"), false).await?);
    }
    if mode == "all" || mode == "ipc" {
        results.push(lifecycle(&root.join("ipc"), true).await?);
        results.push(reconnect(&root.join("reconnect")).await?);
        results.push(transport(&root.join("transport")).await?);
        results.push(process_recovery(&root.join("restart-before"), false).await?);
        results.push(process_recovery(&root.join("restart-unknown"), true).await?);
    }
    require(!results.is_empty(), "unknown mode")?;
    let result = json!({"passed":true,"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"offline_fixture":true,"scenarios":results});
    save(&root.join("acceptance.json"), &result)?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transport_rejections_retain_requests_errors_and_unchanged_state() {
        let root = std::env::temp_dir().join(format!(
            "cw-client-rejections-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        transport(&root).await.unwrap();
        let read = |name: &str| -> Value {
            serde_json::from_slice(&std::fs::read(root.join(name)).unwrap()).unwrap()
        };
        let auth = read("authentication-rejection.json");
        assert_eq!(auth["error"]["code"], "unauthorized");
        assert_eq!(auth["error"]["outcome_unknown"], false);
        assert_eq!(auth["provider_requests"], 0);
        assert_eq!(auth["sessions"], json!([]));
        assert_eq!(auth["before"], auth["after"]);
        let rejected = read("parameter-rejections.json");
        let cases = rejected.as_array().unwrap();
        assert_eq!(cases.len(), 3);
        for (case, (name, method, code)) in cases.iter().zip([
            ("prompt-extra-field", "session.prompt", "invalid_params"),
            ("prompt-missing-text", "session.prompt", "invalid_params"),
            ("events-zero-limit", "session.events", "session_error"),
        ]) {
            assert_eq!(case["case"], name);
            assert_eq!(case["request"]["method"], method);
            assert_eq!(
                case["request"]["params"]["session_id"],
                case["before"]["session"]["id"]
            );
            assert_eq!(case["error"]["code"], code);
            assert_eq!(case["error"]["outcome_unknown"], false);
            assert_eq!(case["before"], case["after"]);
            assert_eq!(case["after"]["provider_requests"], 0);
            assert_eq!(case["after"]["file_exists"], false);
            assert_eq!(case["after"]["tasks"], json!([]));
            assert!(case["unexpected_response"].is_null());
        }
        assert_eq!(cases[0]["request"]["params"]["extra"], true);
        assert!(cases[1]["request"]["params"].get("text").is_none());
        assert_eq!(cases[2]["request"]["params"]["limit"], 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
