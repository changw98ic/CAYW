use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{
    Manifest, PromptSpec, ResourceSpec, SourceHandle, SourceOperation, SourceReply, SourceRequest,
    ToolSpec,
};
use cayw_codemode::CapabilityError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Notify, mpsc, oneshot},
    time::Instant,
};

pub const MCP_PROTOCOL: &str = "2026-07-28";
const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
const CLIENT_KEY: &str = "io.modelcontextprotocol/clientInfo";
const SERVER_KEY: &str = "io.modelcontextprotocol/serverInfo";

/// Trusted application launch configuration. It is never deserialized from model input.
#[derive(Debug, Clone)]
pub struct StdioConfig {
    pub program: PathBuf,
    pub directory: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub max_frame_bytes: usize,
    pub max_pending: usize,
    pub max_observations: usize,
    pub max_received_frames: usize,
    pub max_stderr_bytes: usize,
    pub request_timeout: Duration,
    pub cleanup_timeout: Duration,
}
impl StdioConfig {
    pub fn new(program: impl AsRef<Path>, directory: impl AsRef<Path>) -> Self {
        Self {
            program: program.as_ref().into(),
            directory: directory.as_ref().into(),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            max_frame_bytes: 512 * 1024,
            max_pending: 8,
            max_observations: 256,
            max_received_frames: 4096,
            max_stderr_bytes: 16 * 1024,
            request_timeout: Duration::from_secs(30),
            cleanup_timeout: Duration::from_secs(5),
        }
    }
    fn validate(&self) -> Result<(), CapabilityError> {
        if !self.program.is_absolute()
            || !self.directory.is_absolute()
            || !self.directory.is_dir()
            || !self.program.is_file()
            || !(256..=4 * 1024 * 1024).contains(&self.max_frame_bytes)
            || !(1..=128).contains(&self.max_pending)
            || !(1..=4096).contains(&self.max_observations)
            || !(1..=1_000_000).contains(&self.max_received_frames)
            || self.max_stderr_bytes > 256 * 1024
            || self.request_timeout.is_zero()
            || self.request_timeout > Duration::from_secs(300)
            || self.cleanup_timeout.is_zero()
            || self.cleanup_timeout > Duration::from_secs(30)
            || self.arguments.len() > 128
            || self.environment.len() > 128
            || self
                .arguments
                .iter()
                .any(|arg| arg.len() > 8192 || arg.contains('\0'))
            || self.environment.iter().any(|(key, value)| {
                key.is_empty()
                    || key.len() > 256
                    || key.contains(['=', '\0'])
                    || value.len() > 8192
                    || value.contains('\0')
            })
        {
            return Err(failure(
                "invalid explicit stdio paths, configuration or bounds",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct StdioEvent {
    pub kind: String,
    pub request_id: Option<u64>,
    pub method: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct StdioSnapshot {
    pub instance_id: String,
    pub pid: u32,
    pub spawned: u64,
    pub methods: Vec<String>,
    pub tool_calls: u64,
    pub resource_reads: u64,
    pub progress: u64,
    pub cancellations: u64,
    pub ignored_late: u64,
    pub received_frames: u64,
    pub observed_frame_bytes: usize,
    pub disconnected: u64,
    pub reaped: bool,
    pub cleanup_complete: bool,
    pub stderr: Vec<u8>,
    pub stderr_discarded_bytes: u64,
    pub events: Vec<StdioEvent>,
    pub observations_omitted: u64,
    pub failure: Option<String>,
}
struct Shared {
    close: CancellationToken,
    finished: Notify,
    snapshot: Mutex<StdioSnapshot>,
    limit: usize,
}
impl Shared {
    fn update(&self, change: impl FnOnce(&mut StdioSnapshot)) {
        change(
            &mut self
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
    fn event(&self, kind: &str, id: Option<u64>, method: Option<&str>) {
        self.update(|state| {
            if state.events.len() < self.limit {
                state.events.push(StdioEvent {
                    kind: kind.into(),
                    request_id: id,
                    method: method.map(str::to_owned),
                });
            } else {
                state.observations_omitted = state.observations_omitted.saturating_add(1);
            }
        });
    }
}
/// Observations and shutdown only. This object cannot issue a tool/resource request.
#[derive(Clone)]
pub struct StdioControl {
    shared: Arc<Shared>,
}
impl StdioControl {
    pub fn snapshot(&self) -> StdioSnapshot {
        self.shared
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn close(&self) {
        self.shared.close.cancel();
    }
    pub async fn wait_closed(&self) {
        loop {
            let notified = self.shared.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.snapshot().cleanup_complete {
                return;
            }
            notified.await;
        }
    }
}
enum CommandMessage {
    Request {
        id: u64,
        method: &'static str,
        params: Value,
        effect: bool,
        reply: oneshot::Sender<Result<Value, CapabilityError>>,
    },
    Cancel(u64),
}
struct Transport {
    commands: mpsc::Sender<CommandMessage>,
    shared: Arc<Shared>,
    next: AtomicU64,
    config: StdioConfig,
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.shared.close.cancel();
    }
}
#[derive(Clone)]
struct Client {
    transport: Arc<Transport>,
}
struct Pending {
    reply: oneshot::Sender<Result<Value, CapabilityError>>,
    effect: bool,
}
struct CancelPending {
    id: u64,
    client: Client,
    queued: bool,
    done: bool,
}
impl Drop for CancelPending {
    fn drop(&mut self) {
        if self.queued
            && !self.done
            && self
                .client
                .transport
                .commands
                .try_send(CommandMessage::Cancel(self.id))
                .is_err()
        {
            self.client.transport.shared.close.cancel();
        }
    }
}
impl Client {
    async fn request(
        &self,
        method: &'static str,
        mut params: Value,
        cancel: CancellationToken,
        effect: bool,
    ) -> Result<Value, CapabilityError> {
        let transport = &self.transport;
        if cancel.is_cancelled() || transport.shared.close.is_cancelled() {
            return Err(failure("stdio cancelled or disconnected before dispatch"));
        }
        let id = transport
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| failure("stdio request IDs exhausted"))?;
        let object = params
            .as_object_mut()
            .ok_or_else(|| failure("stdio params must be an object"))?;
        let meta = object
            .entry("_meta")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| failure("stdio metadata must be an object"))?;
        meta.insert(VERSION_KEY.into(), json!(MCP_PROTOCOL));
        meta.insert(CAPABILITIES_KEY.into(), json!({}));
        meta.insert(
            CLIENT_KEY.into(),
            json!({"name":"cayw-host","version":env!("CARGO_PKG_VERSION")}),
        );
        let (reply, receiver) = oneshot::channel();
        let mut guard = CancelPending {
            id,
            client: self.clone(),
            queued: false,
            done: false,
        };
        let deadline = Instant::now() + transport.config.request_timeout;
        tokio::select! { biased;
            _ = cancel.cancelled() => return Err(failure("stdio cancelled before enqueue")),
            _ = transport.shared.close.cancelled() => return Err(failure("stdio disconnected before enqueue")),
            _ = tokio::time::sleep_until(deadline) => return Err(failure("stdio enqueue deadline")),
            sent = transport.commands.send(CommandMessage::Request { id, method, params, effect, reply }) => {
                sent.map_err(|_| failure("stdio connection unavailable"))?;
                guard.queued = true;
            }
        }
        let result = tokio::select! { biased;
            response = receiver => {
                guard.done = true;
                response.unwrap_or_else(|_| Err(uncertain(effect,"stdio response lost")))
            },
            _ = cancel.cancelled() => Err(uncertain(effect,"stdio cancelled after enqueue")),
            _ = transport.shared.close.cancelled() => Err(uncertain(effect,"stdio disconnected after enqueue")),
            _ = tokio::time::sleep_until(deadline) => Err(uncertain(effect,"stdio response deadline")),
        };
        result
    }
}

/// A prepared single connection. Consuming it transfers its only execution handle.
pub struct StdioSource {
    client: Client,
    manifest: Manifest,
    server_info: ServerInfo,
}
impl StdioSource {
    pub async fn connect(
        config: StdioConfig,
        cancel: CancellationToken,
    ) -> Result<Self, CapabilityError> {
        config.validate()?;
        if cancel.is_cancelled() {
            return Err(failure("stdio connection cancelled before launch"));
        }
        let mut command = Command::new(&config.program);
        command
            .args(&config.arguments)
            .current_dir(&config.directory)
            .env_clear()
            .envs(&config.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut child = command
            .spawn()
            .map_err(|_| failure("explicit stdio server failed to launch"))?;
        let pid = child
            .id()
            .ok_or_else(|| failure("stdio server has no process identity"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let shared = Arc::new(Shared {
            close: CancellationToken::new(),
            finished: Notify::new(),
            limit: config.max_observations,
            snapshot: Mutex::new(StdioSnapshot {
                instance_id: format!("stdio-{}", uuid::Uuid::new_v4()),
                pid,
                spawned: 1,
                methods: Vec::new(),
                tool_calls: 0,
                resource_reads: 0,
                progress: 0,
                cancellations: 0,
                ignored_late: 0,
                received_frames: 0,
                observed_frame_bytes: 0,
                disconnected: 0,
                reaped: false,
                cleanup_complete: false,
                stderr: Vec::new(),
                stderr_discarded_bytes: 0,
                events: Vec::new(),
                observations_omitted: 0,
                failure: None,
            }),
        });
        let (commands, receiver) = mpsc::channel(config.max_pending);
        let client = Client {
            transport: Arc::new(Transport {
                commands,
                shared: shared.clone(),
                next: AtomicU64::new(1),
                config: config.clone(),
            }),
        };
        tokio::spawn(run(child, stdin, stdout, stderr, receiver, shared, config));
        let result = async {
            let tools = client
                .request("tools/list", json!({}), cancel.clone(), false)
                .await?;
            let server_info = server_info(&tools)?;
            if tools["resultType"] != "complete" || tools.get("nextCursor").is_some() {
                return Err(failure("stdio tools require a complete bounded catalog"));
            }
            let tools = tools["tools"]
                .as_array()
                .filter(|tools| tools.len() <= 128)
                .ok_or_else(|| failure("stdio tools catalog missing or too large"))?
                .iter()
                .map(|tool| {
                    Ok(ToolSpec {
                        name: text(tool, "name")?,
                        description: optional_text(tool, "description")?,
                        input_schema: tool
                            .get("inputSchema")
                            .filter(|schema| schema.is_object())
                            .cloned()
                            .ok_or_else(|| failure("stdio input schema missing"))?,
                        output_schema: tool.get("outputSchema").cloned(),
                    })
                })
                .collect::<Result<Vec<_>, CapabilityError>>()?;
            let resources = client
                .request("resources/list", json!({}), cancel.clone(), false)
                .await?;
            if server_info != self::server_info(&resources)?
                || resources.get("nextCursor").is_some()
                || resources
                    .get("resultType")
                    .is_some_and(|value| value != "complete")
            {
                return Err(failure(
                    "stdio server identity changed or resource pagination unsupported",
                ));
            }
            let resources = resources["resources"]
                .as_array()
                .filter(|resources| resources.len() <= 128)
                .ok_or_else(|| failure("stdio resource catalog missing or too large"))?
                .iter()
                .map(|resource| {
                    Ok(ResourceSpec {
                        uri: text(resource, "uri")?,
                        description: optional_text(resource, "description")?,
                        mime_type: text(resource, "mimeType")?,
                    })
                })
                .collect::<Result<Vec<_>, CapabilityError>>()?;
            let prompts = client
                .request("prompts/list", json!({}), cancel, false)
                .await?;
            if server_info != self::server_info(&prompts)?
                || prompts.get("nextCursor").is_some()
                || prompts
                    .get("resultType")
                    .is_some_and(|value| value != "complete")
            {
                return Err(failure(
                    "stdio server identity changed or prompt pagination unsupported",
                ));
            }
            let prompts = prompts["prompts"]
                .as_array()
                .filter(|prompts| prompts.len() <= 128)
                .ok_or_else(|| failure("stdio prompt catalog missing or too large"))?
                .iter()
                .map(|prompt| {
                    Ok(PromptSpec {
                        name: text(prompt, "name")?,
                        description: optional_text(prompt, "description")?,
                        arguments_schema: prompt.get("argumentsSchema").cloned().or_else(|| {
                            prompt.get("arguments").and_then(|value| {
                                if value.is_object() || value.is_array() {
                                    Some(value.clone())
                                } else {
                                    None
                                }
                            })
                        }),
                    })
                })
                .collect::<Result<Vec<_>, CapabilityError>>()?;
            Ok((
                Manifest {
                    tools,
                    resources,
                    prompts,
                    ..Manifest::default()
                },
                server_info,
            ))
        }
        .await;
        match result {
            Ok((manifest, server_info)) => Ok(Self {
                client,
                manifest,
                server_info,
            }),
            Err(error) => {
                let control = StdioControl {
                    shared: client.transport.shared.clone(),
                };
                control.close();
                control.wait_closed().await;
                Err(error)
            }
        }
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }
    pub fn control(&self) -> StdioControl {
        StdioControl {
            shared: self.client.transport.shared.clone(),
        }
    }
    pub fn into_parts(self) -> (Manifest, Box<dyn SourceHandle>) {
        let owner_key = self
            .client
            .transport
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .instance_id
            .clone();
        (
            self.manifest,
            Box::new(StdioHandle {
                client: self.client,
                owner_key,
                server_info: self.server_info,
            }),
        )
    }
}
struct StdioHandle {
    client: Client,
    owner_key: String,
    server_info: ServerInfo,
}
#[async_trait]
impl SourceHandle for StdioHandle {
    fn ownership_key(&self) -> &str {
        &self.owner_key
    }
    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        let (method, params, effect) = match &request.operation {
            SourceOperation::InvokeTool { name, arguments } => (
                "tools/call",
                json!({"name":name,"arguments":arguments,
                "_meta":{"progressToken":format!("{}-{}",self.owner_key,request.key.id)}}),
                true,
            ),
            SourceOperation::ReadResource { uri } => ("resources/read", json!({"uri":uri}), false),
            SourceOperation::GetPrompt { name, arguments } => (
                "prompts/get",
                json!({"name":name,"arguments":arguments}),
                false,
            ),
            _ => return Err(failure("stdio source operation unavailable")),
        };
        let result = self.client.request(method, params, cancel, effect).await?;
        if self.server_info
            != server_info(&result)
                .map_err(|_| uncertain(effect, "stdio result server identity missing"))?
        {
            self.client.transport.shared.close.cancel();
            return Err(uncertain(
                effect,
                "stdio result server identity changed; reconnect explicitly",
            ));
        }
        let value = if matches!(request.operation, SourceOperation::GetPrompt { .. }) {
            if !result
                .get("description")
                .is_none_or(|value| value.is_string())
                || !result["messages"].is_array()
            {
                return Err(failure("stdio prompt result requires description/messages"));
            }
            let mut value = result;
            if let Some(object) = value.as_object_mut() {
                object.remove("_meta");
            }
            value
        } else if effect {
            if result
                .get("isError")
                .is_some_and(|value| !value.is_boolean())
                || result["isError"] == true
            {
                return Err(CapabilityError::unknown(
                    "stdio tool reported an error; effects require reconciliation",
                ));
            }
            if !result["content"].is_array() {
                return Err(CapabilityError::unknown("stdio tool content missing"));
            }
            if let Some(structured) = result.get("structuredContent") {
                if !structured.is_object() {
                    return Err(CapabilityError::unknown(
                        "stdio structured content must be an object",
                    ));
                }
                structured.clone()
            } else {
                let mut result = result;
                result
                    .as_object_mut()
                    .expect("validated result")
                    .remove("_meta");
                result
            }
        } else {
            let contents = result["contents"]
                .as_array()
                .filter(|contents| contents.len() == 1)
                .ok_or_else(|| failure("stdio resource must return exactly one content item"))?;
            let content = &contents[0];
            let mut value = json!({"uri":content["uri"],"mime_type":content["mimeType"]});
            match (content.get("text"), content.get("blob")) {
                (Some(text), None) if text.is_string() => value["text"] = text.clone(),
                (None, Some(blob)) if blob.is_string() => value["blob_base64"] = blob.clone(),
                _ => return Err(failure("stdio resource requires one text or blob payload")),
            }
            value
        };
        if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > request.max_result_bytes) {
            return Err(uncertain(
                effect,
                "stdio reply exceeds the original result budget",
            ));
        }
        Ok(SourceReply {
            key: request.key,
            value,
        })
    }
    fn disconnect(&self) {
        self.client.transport.shared.close.cancel();
    }
}
fn text(value: &Value, key: &str) -> Result<String, CapabilityError> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 8192)
        .map(str::to_owned)
        .ok_or_else(|| failure("stdio required metadata string invalid"))
}
fn optional_text(value: &Value, key: &str) -> Result<String, CapabilityError> {
    match value.get(key) {
        None => Ok(String::new()),
        Some(Value::String(text)) if text.len() <= 8192 => Ok(text.clone()),
        _ => Err(failure("stdio optional metadata string invalid")),
    }
}
fn server_info(value: &Value) -> Result<ServerInfo, CapabilityError> {
    let info = &value["_meta"][SERVER_KEY];
    Ok(ServerInfo {
        name: text(info, "name")?,
        version: text(info, "version")?,
    })
}
fn failure(message: &str) -> CapabilityError {
    CapabilityError::new(message)
}
fn uncertain(effect: bool, message: &str) -> CapabilityError {
    if effect {
        CapabilityError::unknown(message)
    } else {
        failure(message)
    }
}
async fn send(
    writer: &mut ChildStdin,
    value: &Value,
    shared: &Shared,
    config: &StdioConfig,
) -> Result<(), CapabilityError> {
    let mut bytes =
        serde_json::to_vec(value).map_err(|_| failure("stdio message serialization failed"))?;
    if bytes.len() + 1 > config.max_frame_bytes {
        return Err(failure("stdio outgoing frame bound exceeded"));
    }
    bytes.push(b'\n');
    tokio::select! { biased;
        _=shared.close.cancelled()=>Err(CapabilityError::unknown("stdio closed during write")),
        result=tokio::time::timeout(config.request_timeout,async {writer.write_all(&bytes).await?;writer.flush().await})=>
            match result { Ok(Ok(()))=>Ok(()),_=>Err(CapabilityError::unknown("stdio write failed or timed out")) }
    }
}
async fn run(
    mut child: Child,
    mut writer: ChildStdin,
    stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
    mut receiver: mpsc::Receiver<CommandMessage>,
    shared: Arc<Shared>,
    config: StdioConfig,
) {
    let mut reader = BufReader::new(stdout);
    let mut pending: BTreeMap<u64, Pending> = BTreeMap::new();
    let mut line = Vec::new();
    let mut stderr_buffer = [0_u8; 4096];
    let mut stderr_done = false;
    let outcome:Result<(),CapabilityError>=async {
        loop {
            let mut bounded=(&mut reader).take((config.max_frame_bytes+1-line.len()) as u64);
            tokio::select! { biased;
                _=shared.close.cancelled()=>break,
                bytes=bounded.read_until(b'\n',&mut line)=> {
                    let bytes=bytes.map_err(|_| CapabilityError::unknown("stdio read failed"))?;
                    if bytes==0 || line.len()>config.max_frame_bytes {return Err(CapabilityError::unknown("stdio EOF or frame bound exceeded"));}
                    if !line.ends_with(b"\n") {continue;}
                    let value:Value=serde_json::from_slice(&line).map_err(|_| CapabilityError::unknown("stdio invalid JSON"))?;
                    let frame_bytes=line.len();line.clear();
                    let mut exhausted=false;
                    shared.update(|state|{state.received_frames+=1;state.observed_frame_bytes=state.observed_frame_bytes.max(frame_bytes);
                        exhausted=state.received_frames>config.max_received_frames as u64;});
                    if exhausted {return Err(CapabilityError::unknown("stdio received-message budget exhausted"));}
                    if value["jsonrpc"]!="2.0" || !value.is_object() {return Err(CapabilityError::unknown("stdio invalid JSON-RPC envelope"));}
                    if let Some(id)=value.get("id") {
                        if value.get("method").is_some() {return Err(CapabilityError::unknown("stdio server requests are unsupported"));}
                        let id=id.as_u64().ok_or_else(||CapabilityError::unknown("stdio invalid response ID"))?;
                        if let Some(waiter)=pending.remove(&id) {
                            let response=match(value.get("result"),value.get("error")) {
                                (Some(result),None) if result.is_object()=>Ok(result.clone()),
                                (None,Some(error))=> {
                                    let code=error["code"].as_i64().ok_or_else(||CapabilityError::unknown("stdio invalid error code"))?;
                                    if !error["message"].is_string() {return Err(CapabilityError::unknown("stdio invalid error message"));}
                                    let known=matches!(code,-32600|-32601|-32602|-32022);
                                    Err(uncertain(waiter.effect && !known,&format!("MCP error {code} for original request {id}")))
                                }
                                _=>Err(uncertain(waiter.effect,"stdio invalid response result/error")),
                            };
                            shared.event("response",Some(id),None);
                            let _=waiter.reply.send(response);
                        } else {shared.update(|state|state.ignored_late+=1);shared.event("ignored_late",Some(id),None);}
                    } else if value["method"]=="notifications/progress" {
                        shared.update(|state|state.progress+=1);shared.event("progress",None,None);
                    } else if !value["method"].as_str().is_some_and(|method|method.starts_with("notifications/")) {
                        return Err(CapabilityError::unknown("stdio unsupported server message"));
                    }
                },
                message=receiver.recv()=>match message {
                    Some(CommandMessage::Request{id,method,params,effect,reply})=>{
                        if reply.is_closed() {continue;}
                        if pending.len()>=config.max_pending {
                            let _=reply.send(Err(failure("stdio pending-call budget exhausted")));continue;
                        }
                        let value=json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
                        if serde_json::to_vec(&value).map_or(true,|bytes|bytes.len()+1>config.max_frame_bytes) {
                            let _=reply.send(Err(failure("stdio outgoing frame bound exceeded")));continue;
                        }
                        pending.insert(id,Pending{reply,effect});
                        send(&mut writer,&value,&shared,&config).await?;
                        shared.update(|state| {
                            if state.methods.len()<config.max_observations {state.methods.push(method.into());}
                            else {state.observations_omitted=state.observations_omitted.saturating_add(1);}
                            if method=="tools/call" {state.tool_calls+=1;}
                            if method=="resources/read" {state.resource_reads+=1;}
                            // prompts/list and prompts/get appear in methods only
                        });
                        shared.event("sent",Some(id),Some(method));
                    }
                    Some(CommandMessage::Cancel(id))=>{
                        if pending.remove(&id).is_some() {
                            send(&mut writer,&json!({"jsonrpc":"2.0","method":"notifications/cancelled",
                                "params":{"requestId":id,"reason":"original host call cancelled or dropped"}}),&shared,&config).await?;
                            shared.update(|state|state.cancellations+=1);shared.event("cancelled",Some(id),None);
                        }
                    }
                    None=>break,
                },
                bytes=stderr.read(&mut stderr_buffer),if !stderr_done=> {
                    match bytes {
                        Ok(0)|Err(_)=>stderr_done=true,
                        Ok(bytes)=>shared.update(|state| {
                            let keep=bytes.min(config.max_stderr_bytes.saturating_sub(state.stderr.len()));
                            state.stderr.extend_from_slice(&stderr_buffer[..keep]);
                            state.stderr_discarded_bytes=state.stderr_discarded_bytes.saturating_add((bytes-keep) as u64);
                        }),
                    }
                }
            }
        }
        Ok(())
    }.await;
    shared.close.cancel();
    if let Err(error) = outcome {
        shared.update(|state| state.failure = Some(error.message));
    }
    for (_, waiter) in pending {
        let _ = waiter.reply.send(Err(uncertain(
            waiter.effect,
            "stdio transport ended; no replay",
        )));
    }
    drop(writer);
    drop(reader);
    drop(stderr);
    // The child owns a dedicated process group. Signal it while the original
    // child identity is still unreaped; this is cleanup, not a sandbox/rollback.
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let reaped = matches!(
        tokio::time::timeout(config.cleanup_timeout, child.wait()).await,
        Ok(Ok(_))
    );
    shared.update(|state| {
        state.disconnected += 1;
        state.reaped = reaped;
        state.cleanup_complete = true;
        if !reaped {
            state.failure = Some("stdio child cleanup unconfirmed".into());
        }
    });
    shared.finished.notify_waiters();
}
