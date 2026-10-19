//! Explicit application-owned local MCP fixture client. This is intentionally
//! scoped to MCP 2025-11-25 stdio; the bridge library contains no transport.
#![allow(dead_code)]
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::*;
use cayw_codemode::{CapabilityError, CapabilityHost, CodeMode, Limits as CodeLimits};
use cayw_host::{AuditSink, ExternalHost, Host, HostConfig, PermissionGate};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Notify, mpsc, oneshot},
};

pub const PROTOCOL: &str = "2025-11-25";
pub const SKILL: &str = "# Local addition\nUse the add tool with two integer arguments.\n";
const FRAME_LIMIT: usize = 512 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub struct Stats {
    pub spawned: AtomicUsize,
    pub initializations: AtomicUsize,
    pub tool_calls: AtomicUsize,
    pub skill_reads: AtomicUsize,
    pub cancellations: AtomicUsize,
    pub progress: AtomicUsize,
    pub ignored_late: AtomicUsize,
    pub disconnected: AtomicUsize,
}
enum CommandMessage {
    Request {
        id: u64,
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, CapabilityError>>,
    },
    Cancel(u64),
    Notify {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<(), CapabilityError>>,
    },
}
struct Transport {
    commands: mpsc::Sender<CommandMessage>,
    close: CancellationToken,
    finished: Arc<Notify>,
    next: AtomicU64,
    stats: Arc<Stats>,
    owner_key: String,
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.close.cancel();
    }
}
#[derive(Clone)]
pub struct LocalClient {
    transport: Arc<Transport>,
}
impl LocalClient {
    /// Caller supplies the absolute executable and working directory explicitly.
    /// The fixture child inherits no environment and is never launched via shell.
    pub async fn spawn(
        program: &Path,
        directory: &Path,
        stats: Arc<Stats>,
    ) -> Result<Self, CapabilityError> {
        if !program.is_absolute() || !directory.is_absolute() {
            return Err(CapabilityError::new("fixture paths must be absolute"));
        }
        let mut child = Command::new(program)
            .current_dir(directory)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| CapabilityError::new(e.to_string()))?;
        stats.spawned.fetch_add(1, Ordering::Relaxed);
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (commands, receiver) = mpsc::channel(32);
        let close = CancellationToken::new();
        let finished = Arc::new(Notify::new());
        let client = Self {
            transport: Arc::new(Transport {
                commands,
                close: close.clone(),
                finished: finished.clone(),
                next: AtomicU64::new(1),
                stats: stats.clone(),
                owner_key: format!("local-fixture-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
            }),
        };
        // The actor owns this one child and its streams. Closing the handle
        // cancels the actor; the child is killed/reaped rather than abandoned.
        tokio::spawn(async move {
            let mut writer = stdin;
            let mut reader = BufReader::new(stdout);
            let mut pending: BTreeMap<u64, oneshot::Sender<Result<Value, CapabilityError>>> =
                BTreeMap::new();
            let mut receiver = receiver;
            let mut line = Vec::new();
            let outcome: Result<(), CapabilityError> = async {
                loop {
                    let mut bounded_reader = (&mut reader).take((FRAME_LIMIT + 1 - line.len()) as u64);
                    tokio::select! {
                        biased;
                        _ = close.cancelled() => break,
                        message = receiver.recv() => match message {
                            Some(CommandMessage::Request { id, method, params, reply }) => {
                                if reply.is_closed() { continue; }
                                if pending.insert(id, reply).is_some() { return Err(CapabilityError::unknown("duplicate fixture request ID")); }
                                let value = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
                                send(&mut writer, &value, &close).await?;
                            }
                            Some(CommandMessage::Notify { method, params, reply }) => {
                                let result = send(&mut writer, &json!({"jsonrpc":"2.0","method":method,"params":params}), &close).await;
                                let failed = result.is_err();
                                let _ = reply.send(result);
                                if failed { return Err(CapabilityError::unknown("fixture notification send failed")); }
                            }
                            Some(CommandMessage::Cancel(id)) => {
                                if pending.remove(&id).is_some() {
                                    stats.cancellations.fetch_add(1, Ordering::Relaxed);
                                    send(&mut writer, &json!({"jsonrpc":"2.0","method":"notifications/cancelled",
                                        "params":{"requestId":id,"reason":"application cancelled or dropped wait"}}), &close).await?;
                                }
                            }
                            None => break,
                        },
                        bytes = bounded_reader.read_until(b'\n', &mut line) => {
                            let bytes = bytes.map_err(|e| CapabilityError::unknown(e.to_string()))?;
                            if bytes == 0 || line.len() > FRAME_LIMIT { return Err(CapabilityError::unknown("fixture closed or frame bound exceeded")); }
                            if !line.ends_with(b"\n") { continue; }
                            let value: Value = serde_json::from_slice(&line).map_err(|_| CapabilityError::unknown("invalid fixture JSON"))?;
                            line.clear();
                            if value["jsonrpc"] != "2.0" { return Err(CapabilityError::unknown("invalid JSON-RPC version")); }
                            if let Some(id) = value.get("id").and_then(Value::as_u64) {
                                if let Some(reply) = pending.remove(&id) {
                                    let result = match (value.get("result"), value.get("error")) {
                                        (Some(result), None) => Ok(result.clone()),
                                        // This controlled fixture rejects before executing
                                        // on JSON-RPC errors. Tool execution errors would
                                        // need their own effect reconciliation contract.
                                        (None, Some(_)) => Err(CapabilityError::new("fixture rejected request before execution")),
                                        _ => Err(CapabilityError::unknown("invalid fixture response envelope")),
                                    };
                                    let _ = reply.send(result);
                                } else { stats.ignored_late.fetch_add(1, Ordering::Relaxed); }
                            } else if value["method"] == "notifications/progress" {
                                stats.progress.fetch_add(1, Ordering::Relaxed);
                                // Progress is observational; never resolves a request,
                                // changes a registry policy, or authorizes execution.
                            } else if value["method"] != "notifications/initialized" {
                                return Err(CapabilityError::unknown("unsupported fixture message"));
                            }
                        }
                    }
                }
                Ok(())
            }.await;
            close.cancel();
            for (_, reply) in pending {
                let _ = reply.send(Err(CapabilityError::unknown("fixture transport ended")));
            }
            let _ = outcome;
            let _ = child.kill().await;
            let _ = child.wait().await;
            stats.disconnected.fetch_add(1, Ordering::Relaxed);
            finished.notify_waiters();
        });
        let initialized = client
            .rpc(
                "initialize",
                json!({"protocolVersion":PROTOCOL,
            "capabilities":{}, "clientInfo":{"name":"cayw-local-example","version":"1"}}),
                CancellationToken::new(),
            )
            .await?;
        if initialized["protocolVersion"] != PROTOCOL
            || !initialized["capabilities"]["tools"].is_object()
            || !initialized["capabilities"]["resources"].is_object()
        {
            client.close();
            return Err(CapabilityError::new("fixture protocol mismatch"));
        }
        // Notification is sent through the same actor as requests, preserving order.
        client.notify_initialized().await?;
        client
            .transport
            .stats
            .initializations
            .fetch_add(1, Ordering::Relaxed);
        Ok(client)
    }
    async fn notify_initialized(&self) -> Result<(), CapabilityError> {
        let (reply, receiver) = oneshot::channel();
        self.transport
            .commands
            .send(CommandMessage::Notify {
                method: "notifications/initialized".into(),
                params: json!({}),
                reply,
            })
            .await
            .map_err(|_| CapabilityError::new("fixture actor unavailable"))?;
        receiver
            .await
            .map_err(|_| CapabilityError::unknown("fixture initialization notification lost"))?
    }
    pub fn close(&self) {
        self.transport.close.cancel();
    }
    pub async fn wait_closed(&self) {
        let notified = self.transport.finished.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.transport.stats.disconnected.load(Ordering::Acquire) == 0 {
            notified.await;
        }
    }
    pub async fn rpc(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if cancel.is_cancelled() || self.transport.close.is_cancelled() {
            return Err(CapabilityError::new("fixture cancelled before send"));
        }
        let id = self.transport.next.fetch_add(1, Ordering::Relaxed);
        if id == u64::MAX {
            self.close();
            return Err(CapabilityError::new("fixture IDs exhausted"));
        }
        let (reply, receiver) = oneshot::channel();
        let mut guard = Pending {
            id,
            client: self.clone(),
            sent: false,
            done: false,
            initialize: method == "initialize",
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CapabilityError::new("fixture cancelled before enqueue")),
            _ = self.transport.close.cancelled() => return Err(CapabilityError::new("fixture disconnected before enqueue")),
            result = self.transport.commands.send(CommandMessage::Request { id, method: method.into(), params, reply }) => {
                result.map_err(|_| CapabilityError::new("fixture actor unavailable"))?;
                guard.sent = true;
            }
        }
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(CapabilityError::unknown("fixture request cancelled after enqueue")),
            _ = self.transport.close.cancelled() => Err(CapabilityError::unknown("fixture disconnected after enqueue")),
            _ = tokio::time::sleep(Duration::from_secs(5)) => Err(CapabilityError::unknown("fixture request deadline")),
            result = receiver => result.unwrap_or_else(|_| Err(CapabilityError::unknown("fixture response lost"))),
        };
        if result.is_ok() || result.as_ref().is_err_and(|e| !e.outcome_unknown) {
            guard.done = true;
        }
        result
    }
    pub async fn manifest(&self) -> Result<Manifest, CapabilityError> {
        let listed = self
            .rpc("tools/list", json!({}), CancellationToken::new())
            .await?;
        if listed.get("nextCursor").is_some() {
            return Err(CapabilityError::new("fixture pagination unsupported"));
        }
        let tools = listed["tools"]
            .as_array()
            .ok_or_else(|| CapabilityError::new("missing tools"))?
            .iter()
            .map(|tool| {
                Ok(ToolSpec {
                    name: tool["name"]
                        .as_str()
                        .ok_or_else(|| CapabilityError::new("missing tool name"))?
                        .into(),
                    description: tool["description"].as_str().unwrap_or("").into(),
                    input_schema: tool["inputSchema"].clone(),
                    output_schema: tool.get("outputSchema").cloned(),
                })
            })
            .collect::<Result<Vec<_>, CapabilityError>>()?;
        let resources = self
            .rpc("resources/list", json!({}), CancellationToken::new())
            .await?;
        if resources["resources"][0]["uri"] != "fixture://local/addition" {
            return Err(CapabilityError::new("unexpected fixture resource"));
        }
        let resource = self
            .rpc(
                "resources/read",
                json!({"uri":"fixture://local/addition"}),
                CancellationToken::new(),
            )
            .await?;
        if resource["contents"][0]["uri"] != "fixture://local/addition"
            || resource["contents"][0]["text"] != SKILL
        {
            return Err(CapabilityError::new("fixture resource mismatch"));
        }
        Ok(Manifest {
            tools,
            skills: vec![SkillSpec {
                name: "addition".into(),
                description: "Fixed application-supplied local addition instructions".into(),
                content_sha256: content_sha256(SKILL),
            }],
            search: false,
        })
    }
    pub fn handle(&self) -> LocalHandle {
        LocalHandle {
            client: self.clone(),
            owner_key: self.transport.owner_key.clone(),
            skill: Arc::from(SKILL),
        }
    }
}
async fn send(
    writer: &mut tokio::process::ChildStdin,
    value: &Value,
    close: &CancellationToken,
) -> Result<(), CapabilityError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|_| CapabilityError::new("fixture serialization failed"))?;
    if bytes.len() > FRAME_LIMIT {
        return Err(CapabilityError::new("fixture output bound exceeded"));
    }
    bytes.push(b'\n');
    tokio::select! {
        biased;
        _ = close.cancelled() => Err(CapabilityError::unknown("fixture closed while writing")),
        _ = tokio::time::sleep(Duration::from_secs(5)) => Err(CapabilityError::unknown("fixture write deadline")),
        result = async { writer.write_all(&bytes).await?; writer.flush().await } => result.map_err(|_| CapabilityError::unknown("fixture write failed")),
    }
}
struct Pending {
    id: u64,
    client: LocalClient,
    sent: bool,
    done: bool,
    initialize: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if self.sent
            && !self.done
            && (self.initialize
                || self
                    .client
                    .transport
                    .commands
                    .try_send(CommandMessage::Cancel(self.id))
                    .is_err())
        {
            self.client.close();
        }
    }
}

pub struct LocalHandle {
    client: LocalClient,
    owner_key: String,
    skill: Arc<str>,
}
#[async_trait]
impl SourceHandle for LocalHandle {
    fn ownership_key(&self) -> &str {
        &self.owner_key
    }
    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        let value = match request.operation {
            SourceOperation::InvokeTool { name, arguments } => {
                self.client
                    .transport
                    .stats
                    .tool_calls
                    .fetch_add(1, Ordering::Relaxed);
                let result = self
                    .client
                    .rpc(
                        "tools/call",
                        json!({"name":name,"arguments":arguments,
                    "_meta":{"progressToken":format!("bridge-{}",request.key.id)}}),
                        cancel,
                    )
                    .await?;
                if result["isError"] == true {
                    return Err(CapabilityError::unknown(
                        "fixture tool error with unconfirmed effect",
                    ));
                }
                result["structuredContent"].clone()
            }
            SourceOperation::ReadSkill { name } if name == "addition" => {
                if cancel.is_cancelled() {
                    return Err(CapabilityError::new("skill read cancelled"));
                }
                self.client
                    .transport
                    .stats
                    .skill_reads
                    .fetch_add(1, Ordering::Relaxed);
                Value::String(self.skill.to_string())
            }
            _ => return Err(CapabilityError::new("fixture operation unavailable")),
        };
        if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > request.max_result_bytes) {
            return Err(CapabilityError::unknown(
                "fixture reply exceeds requested budget",
            ));
        }
        Ok(SourceReply {
            key: request.key,
            value,
        })
    }
    fn disconnect(&self) {
        self.client.close();
    }
}

pub struct Policy {
    pub hidden: Mutex<BTreeSet<(u8, String)>>,
    pub denied: Mutex<BTreeSet<String>>,
    pub calls: Mutex<Vec<AccessRequest>>,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            hidden: Mutex::new(BTreeSet::new()),
            denied: Mutex::new(BTreeSet::new()),
            calls: Mutex::new(vec![]),
        }
    }
}
#[async_trait]
impl RegistryPolicy for Policy {
    fn visible(&self, kind: Visibility, _: &Provenance, name: &str) -> bool {
        !self
            .hidden
            .lock()
            .unwrap()
            .contains(&(kind as u8, name.into()))
    }
    async fn authorize(
        &self,
        request: &AccessRequest,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.calls.lock().unwrap().push(request.clone());
        if cancel.is_cancelled()
            || self
                .denied
                .lock()
                .unwrap()
                .contains(&format!("{:?}", request.operation))
        {
            Err(CapabilityError::new("registry policy denied"))
        } else {
            Ok(())
        }
    }
}
#[derive(Default)]
pub struct Gate {
    pub denied: Mutex<BTreeSet<String>>,
    pub calls: Mutex<Vec<String>>,
}
#[async_trait]
impl PermissionGate for Gate {
    async fn authorize(
        &self,
        name: &str,
        _: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.calls.lock().unwrap().push(name.into());
        if cancel.is_cancelled() || self.denied.lock().unwrap().contains(name) {
            Err(CapabilityError::new("host gate denied"))
        } else {
            Ok(())
        }
    }
}
#[derive(Default)]
pub struct Audit {
    pub records: Mutex<Vec<String>>,
}
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, capability: &str, _: &Value) -> Result<String, CapabilityError> {
        let mut records = self.records.lock().unwrap();
        let id = records.len().to_string();
        records.push(format!("start:{capability}"));
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        self.records.lock().unwrap().push(format!(
            "settle:{id}:{}",
            match result {
                Ok(_) => "success",
                Err(e) if e.outcome_unknown => "unknown",
                Err(_) => "failure",
            }
        ));
        Ok(())
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        self.records
            .lock()
            .unwrap()
            .push(format!("drop:{id}:unknown"));
        Ok(())
    }
}
/// Fixture-only runtime adapter. Production Runtime must derive these fields
/// from its own durable execution state and provide real DispatchJournal storage.
#[derive(Default)]
pub struct FixtureJournal {
    pub records: Mutex<Vec<Value>>,
}
#[async_trait]
impl DispatchJournal for FixtureJournal {
    async fn started(&self, record: &DispatchRecord) -> Result<(), CapabilityError> {
        self.records
            .lock()
            .unwrap()
            .push(json!({"kind":"started","record":record}));
        Ok(())
    }
    async fn settled(
        &self,
        record: &DispatchRecord,
        outcome: &DispatchOutcome,
    ) -> Result<(), CapabilityError> {
        self.records
            .lock()
            .unwrap()
            .push(json!({"kind":"settled","record":record,"outcome":outcome}));
        Ok(())
    }
    fn dropped(
        &self,
        record: &DispatchRecord,
        state: DispatchDropState,
    ) -> Result<(), CapabilityError> {
        self.records
            .lock()
            .unwrap()
            .push(json!({"kind":"dropped","record":record,"state":state}));
        Ok(())
    }
}
pub fn invocation(sequence: u64) -> InvocationContext {
    InvocationContext {
        session_id: "fixture-session".into(),
        task_id: "fixture-task".into(),
        attempt_id: "fixture-attempt".into(),
        execution_id: "fixture-execution".into(),
        sequence,
    }
}
pub struct FixtureRuntime {
    bridge: Bridge,
    next: AtomicU64,
    pub journal: Arc<FixtureJournal>,
}
impl FixtureRuntime {
    pub fn new(bridge: Bridge) -> Self {
        Self {
            bridge,
            next: AtomicU64::new(1),
            journal: Arc::default(),
        }
    }
    pub async fn call(
        &self,
        capability: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let scope = self.bridge.scoped(
            capability,
            invocation(self.next.fetch_add(1, Ordering::Relaxed)),
            self.journal.clone(),
        )?;
        scope.call(capability, args, cancel).await
    }
}
#[async_trait]
impl ExternalHost for FixtureRuntime {
    async fn call(
        &self,
        capability: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.call(capability, args, cancel).await
    }
}
pub fn consumer(registry: &Registry) -> FixtureRuntime {
    FixtureRuntime::new(registry.consumer())
}
pub fn host(root: &Path, bridge: FixtureRuntime, gate: Arc<Gate>, audit: Arc<Audit>) -> Arc<Host> {
    Arc::new(
        Host::new(
            root,
            gate,
            audit,
            HostConfig {
                external: Some(Arc::new(bridge)),
                ..HostConfig::default()
            },
        )
        .unwrap(),
    )
}
pub fn mode(host: Arc<Host>) -> CodeMode {
    CodeMode::new(host, CodeLimits::default()).unwrap()
}
pub fn identity(version: &str) -> SourceIdentity {
    SourceIdentity {
        id: "local".into(),
        version: version.into(),
        origin: "application:explicit-local-stdio-fixture".into(),
    }
}
pub struct Workspace(pub PathBuf);
impl Workspace {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-bridge-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub async fn host_call(host: &Host, name: &str, args: Value) -> Result<Value, CapabilityError> {
    host.call(name, args, CancellationToken::new()).await
}
