use cayw_ai::CancellationToken;
use cayw_bridge::{
    Operation, Provenance, RequestKey, SourceHandle, SourceIdentity, SourceOperation, SourceRequest,
};
use cayw_ipc::ClientError;
use cayw_remote::RsaSealedFrame;
use cayw_sources::{StdioConfig, StdioControl, StdioSource};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, oneshot},
    time::timeout,
};

static NEXT_REQ_ID: AtomicU64 = AtomicU64::new(1);

pub struct DeviceChannelHub {
    queues: Mutex<HashMap<String, VecDeque<RsaSealedFrame>>>,
    waiters: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    last_poll_ms: Mutex<HashMap<String, i64>>,
    pub session_devices: Mutex<HashMap<String, String>>,
    pub task_devices: Mutex<HashMap<String, String>>,
    notify: Notify,
}

impl Default for DeviceChannelHub {
    fn default() -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            last_poll_ms: Mutex::new(HashMap::new()),
            session_devices: Mutex::new(HashMap::new()),
            task_devices: Mutex::new(HashMap::new()),
            notify: Notify::new(),
        }
    }
}

impl DeviceChannelHub {
    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    pub fn is_polling(&self, device_id: &str) -> bool {
        let now = Self::now_ms();
        self.last_poll_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(device_id)
            .is_some_and(|&ts| now.saturating_sub(ts) <= 30_000)
    }

    pub fn next_request_id() -> String {
        let seq = NEXT_REQ_ID.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        format!("devreq-{now:x}-{seq:x}")
    }

    pub async fn dispatch(
        &self,
        device_id: &str,
        request_id: String,
        frame: RsaSealedFrame,
        budget: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, ClientError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut waiters = self
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            waiters.insert(request_id.clone(), tx);
        }
        {
            let mut queues = self
                .queues
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queues
                .entry(device_id.to_owned())
                .or_default()
                .push_back(frame);
        }
        self.notify.notify_waiters();

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ClientError {
                code: "cancelled".to_owned(),
                message: "Gateway shutting down".to_owned(),
                outcome_unknown: true,
            }),
            res = timeout(budget, rx) => match res {
                Ok(Ok(val)) => Ok(val),
                Ok(Err(_)) => Err(ClientError {
                    code: "device_disconnected".to_owned(),
                    message: "目标设备响应通道已关闭".to_owned(),
                    outcome_unknown: true,
                }),
                Err(_) => Err(ClientError {
                    code: "device_timeout".to_owned(),
                    message: "等待目标设备执行响应超时，请检查设备在线状态".to_owned(),
                    outcome_unknown: true,
                }),
            }
        };
        if outcome.is_err() {
            let mut waiters = self
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            waiters.remove(&request_id);
        }
        outcome
    }

    pub async fn poll_next(
        &self,
        device_id: &str,
        wait_ms: u64,
        cancel: &CancellationToken,
    ) -> Option<RsaSealedFrame> {
        self.last_poll_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(device_id.to_owned(), Self::now_ms());
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms.min(10_000));
        loop {
            let notified = self.notify.notified();
            {
                let mut queues = self
                    .queues
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(q) = queues.get_mut(device_id)
                    && let Some(frame) = q.pop_front()
                {
                    return Some(frame);
                }
            }
            if wait_ms == 0 || tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                _ = tokio::time::sleep_until(deadline) => return None,
                _ = notified => {},
            }
        }
    }

    pub fn complete(&self, request_id: &str, payload: Value) -> bool {
        let tx = {
            let mut waiters = self
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            waiters.remove(request_id)
        };
        if let Some(sender) = tx {
            let _ = sender.send(payload);
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelItemConfig {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default = "default_context_window")]
    pub context: String,
    #[serde(default = "default_true")]
    pub tools: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning: bool,
}

fn default_context_window() -> String {
    "128K".to_owned()
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderEntryConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_api_protocol")]
    pub api: String,
    pub base_url: String,
    #[serde(default)]
    pub configured: bool,
    #[serde(default)]
    pub api_key_secret: Option<String>,
    pub models: Vec<ModelItemConfig>,
}

fn default_api_protocol() -> String {
    "chat_completions".to_owned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelsConfigState {
    pub selected_model: String,
    pub providers: Vec<ProviderEntryConfig>,
    #[serde(default)]
    pub session_models: HashMap<String, String>,
    #[serde(skip)]
    pub file_path: PathBuf,
}

impl ModelsConfigState {
    pub fn load_or_init(base_dir: &Path) -> Self {
        let file_path = base_dir.join("models-config.json");
        if file_path.exists()
            && let Ok(raw) = std::fs::read_to_string(&file_path)
            && let Ok(mut loaded) = serde_json::from_str::<Self>(&raw)
        {
            loaded.file_path = file_path;
            return loaded;
        }
        let state = Self {
            selected_model: "deepseek-v4.1-flash".to_owned(),
            providers: vec![
                ProviderEntryConfig {
                    id: "magpie".to_owned(),
                    name: "Magpie / DeepSeek 网关".to_owned(),
                    api: "chat_completions".to_owned(),
                    base_url: "http://127.0.0.1:3425/v1".to_owned(),
                    configured: std::env::var("MAGPIE_API_KEY").is_ok(),
                    api_key_secret: std::env::var("MAGPIE_API_KEY").ok(),
                    models: vec![
                        ModelItemConfig {
                            id: "deepseek-v4.1-flash".to_owned(),
                            label: "deepseek-v4.1-flash".to_owned(),
                            desc: "高速低延迟代码编排模型".to_owned(),
                            context: "128K".to_owned(),
                            tools: true,
                            vision: false,
                            reasoning: true,
                        },
                        ModelItemConfig {
                            id: "deepseek-r1".to_owned(),
                            label: "deepseek-r1".to_owned(),
                            desc: "深度推理与架构分析模型".to_owned(),
                            context: "128K".to_owned(),
                            tools: true,
                            vision: false,
                            reasoning: true,
                        },
                    ],
                },
                ProviderEntryConfig {
                    id: "openai".to_owned(),
                    name: "OpenAI 兼容接口".to_owned(),
                    api: "chat_completions".to_owned(),
                    base_url: "https://api.openai.com/v1".to_owned(),
                    configured: false,
                    api_key_secret: None,
                    models: vec![ModelItemConfig {
                        id: "gpt-4.1".to_owned(),
                        label: "gpt-4.1".to_owned(),
                        desc: "通用工具调用与代码生成模型".to_owned(),
                        context: "128K".to_owned(),
                        tools: true,
                        vision: true,
                        reasoning: false,
                    }],
                },
                ProviderEntryConfig {
                    id: "anthropic".to_owned(),
                    name: "Anthropic Messages".to_owned(),
                    api: "messages".to_owned(),
                    base_url: "https://api.anthropic.com/v1".to_owned(),
                    configured: false,
                    api_key_secret: None,
                    models: vec![ModelItemConfig {
                        id: "claude-sonnet-4".to_owned(),
                        label: "claude-sonnet-4".to_owned(),
                        desc: "长上下文代码审查与重构模型".to_owned(),
                        context: "200K".to_owned(),
                        tools: true,
                        vision: true,
                        reasoning: true,
                    }],
                },
            ],
            session_models: HashMap::new(),
            file_path,
        };
        state.save();
        state
    }

    pub fn save(&self) {
        if self.file_path.as_os_str().is_empty() {
            return;
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&self.file_path, json);
        }
    }

    pub fn public_json(&self) -> Value {
        let providers: Vec<Value> = self
            .providers
            .iter()
            .map(|p| {
                let has_key = p
                    .api_key_secret
                    .as_deref()
                    .is_some_and(|k| !k.trim().is_empty())
                    || p.configured;
                json!({
                    "id": p.id,
                    "name": p.name,
                    "api": p.api,
                    "baseUrl": p.base_url,
                    "configured": has_key,
                    "models": p.models,
                })
            })
            .collect();
        json!({
            "selected_model": self.selected_model,
            "providers": providers,
            "session_models": self.session_models,
        })
    }

    pub fn find_provider_and_model(
        &self,
        model_id: &str,
    ) -> (String, String, Option<String>, String) {
        for p in &self.providers {
            if p.models.iter().any(|m| m.id == model_id) {
                return (
                    p.id.clone(),
                    p.base_url.clone(),
                    p.api_key_secret.clone(),
                    p.api.clone(),
                );
            }
        }
        if let Some(first) = self.providers.first() {
            return (
                first.id.clone(),
                first.base_url.clone(),
                first.api_key_secret.clone(),
                first.api.clone(),
            );
        }
        (
            "magpie".to_owned(),
            "http://127.0.0.1:3425/v1".to_owned(),
            None,
            "chat_completions".to_owned(),
        )
    }

    pub fn runtime_configure_payload(&self, model_id: &str, session_id: Option<&str>) -> Value {
        let (provider_id, base_url, api_key, api) = self.find_provider_and_model(model_id);
        // Only activate live HTTP if caller explicitly passed a local/live endpoint with key
        // AND did not request offline fixture fallback. By default, if the base_url is an
        // unreachable default placeholder or offline fixture mode is active, we still update
        // the Runtime's provider/model identity so health & session messages reflect it,
        // and use live HTTP when an explicit reachable endpoint + key is configured.
        let use_live = api_key.as_deref().is_some_and(|k| !k.trim().is_empty())
            && std::env::var("CAYW_ENABLE_LIVE_HTTP").ok().as_deref() == Some("1");
        json!({
            "provider": provider_id,
            "model": model_id,
            "base_url": base_url,
            "api_key": api_key,
            "api": api,
            "max_output_tokens": 4096,
            "session_id": session_id,
            "offline_fallback": !use_live,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct McpToolView {
    pub name: String,
    pub desc: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    pub id: String,
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub tools: Vec<McpToolView>,
}

pub struct LiveMcpConnection {
    pub control: StdioControl,
    pub handle: Arc<tokio::sync::Mutex<Box<dyn SourceHandle>>>,
    pub tools: Vec<McpToolView>,
    pub server_name: String,
    pub server_version: String,
    pub next_call_id: AtomicU64,
}

pub struct McpHostState {
    pub entries: Vec<McpServerEntry>,
    pub live: HashMap<String, Arc<LiveMcpConnection>>,
    pub file_path: PathBuf,
}

pub fn locate_mcp_fixture_bin() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("CAYW_MCP_FIXTURE_BIN")
        && !explicit.trim().is_empty()
    {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidates = [
        "/Volumes/ORICO/CAYW-webui/target/debug/cayw-runtime-mcp-fixture",
        "/Volumes/ORICO/CAYW/target/debug/cayw-runtime-mcp-fixture",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("cayw-runtime-mcp-fixture");
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    None
}

impl McpHostState {
    pub fn load_or_init(base_dir: &Path) -> Self {
        let file_path = base_dir.join("mcp-servers.json");
        if file_path.exists()
            && let Ok(raw) = std::fs::read_to_string(&file_path)
            && let Ok(entries) = serde_json::from_str::<Vec<McpServerEntry>>(&raw)
        {
            return Self {
                entries,
                live: HashMap::new(),
                file_path,
            };
        }
        let fixture_endpoint = locate_mcp_fixture_bin()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| {
                "/Volumes/ORICO/CAYW-webui/target/debug/cayw-runtime-mcp-fixture".to_owned()
            });
        let entries = vec![McpServerEntry {
            id: "local-mcp".to_owned(),
            name: "本机算术与状态校验服务 (Stdio MCP)".to_owned(),
            transport: "本机标准输入输出".to_owned(),
            endpoint: fixture_endpoint,
            builtin: true,
            tools: Vec::new(),
        }];
        let s = Self {
            entries,
            live: HashMap::new(),
            file_path,
        };
        s.save();
        s
    }

    pub fn save(&self) {
        if self.file_path.as_os_str().is_empty() {
            return;
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.entries) {
            let _ = std::fs::write(&self.file_path, json);
        }
    }

    pub fn catalog_json(&mut self) -> Value {
        // Prune dead connections based on real StdioControl snapshot
        let mut dead = Vec::new();
        for (id, conn) in &self.live {
            let snap = conn.control.snapshot();
            if snap.cleanup_complete || snap.disconnected > 0 || snap.reaped {
                dead.push(id.clone());
            }
        }
        for id in dead {
            self.live.remove(&id);
        }

        let items: Vec<Value> = self
            .entries
            .iter()
            .map(|entry| {
                if let Some(conn) = self.live.get(&entry.id) {
                    let snap = conn.control.snapshot();
                    json!({
                        "id": entry.id,
                        "name": entry.name,
                        "transport": entry.transport,
                        "endpoint": entry.endpoint,
                        "status": "active",
                        "statusLabel": "已连接",
                        "enabled": true,
                        "builtin": entry.builtin,
                        "pid": snap.pid,
                        "tool_calls": snap.tool_calls,
                        "server_info": {
                            "name": conn.server_name,
                            "version": conn.server_version,
                        },
                        "tools": conn.tools,
                    })
                } else {
                    json!({
                        "id": entry.id,
                        "name": entry.name,
                        "transport": entry.transport,
                        "endpoint": entry.endpoint,
                        "status": "disconnected",
                        "statusLabel": "未连接",
                        "enabled": false,
                        "builtin": entry.builtin,
                        "pid": null,
                        "tool_calls": 0,
                        "tools": entry.tools,
                    })
                }
            })
            .collect();
        Value::Array(items)
    }
}

pub async fn connect_stdio_mcp(endpoint: &str) -> Result<LiveMcpConnection, String> {
    let parts: Vec<&str> = endpoint.split_whitespace().collect();
    let Some(&prog_raw) = parts.first() else {
        return Err("MCP 启动命令或可执行文件路径不能为空".to_owned());
    };
    let prog_path = if prog_raw == "cayw-runtime-mcp-fixture" || prog_raw == "local-mcp" {
        locate_mcp_fixture_bin()
            .ok_or_else(|| "未找到 cayw-runtime-mcp-fixture 可执行文件".to_owned())?
    } else {
        let candidate = PathBuf::from(prog_raw);
        if candidate.is_absolute() && candidate.is_file() {
            candidate
        } else if let Some(bin) = locate_mcp_fixture_bin()
            && prog_raw.ends_with("cayw-runtime-mcp-fixture")
        {
            bin
        } else {
            return Err(format!(
                "MCP stdio 服务要求存在的本机绝对路径可执行文件：{prog_raw}"
            ));
        }
    };
    let work_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
    let mut cfg = StdioConfig::new(&prog_path, &work_dir);
    cfg.arguments = parts[1..].iter().map(|s| (*s).to_owned()).collect();
    cfg.request_timeout = Duration::from_secs(10);
    let source = StdioSource::connect(cfg, CancellationToken::new())
        .await
        .map_err(|e| format!("连接 MCP 服务失败：{e}"))?;
    let control = source.control();
    let info = source.server_info().clone();
    let (manifest, handle) = source.into_parts();
    let tools: Vec<McpToolView> = manifest
        .tools
        .iter()
        .map(|t| McpToolView {
            name: t.name.clone(),
            desc: if t.description.is_empty() {
                "MCP 工具".to_owned()
            } else {
                t.description.clone()
            },
        })
        .collect();
    Ok(LiveMcpConnection {
        control,
        handle: Arc::new(tokio::sync::Mutex::new(handle)),
        tools,
        server_name: info.name,
        server_version: info.version,
        next_call_id: AtomicU64::new(1),
    })
}

pub async fn invoke_stdio_mcp_tool(
    conn: &Arc<LiveMcpConnection>,
    tool_name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let call_id = conn.next_call_id.fetch_add(1, Ordering::Relaxed);
    let key = RequestKey {
        id: call_id,
        source: Provenance {
            registry_incarnation: "webui-host".to_owned(),
            source: SourceIdentity {
                id: conn.server_name.clone(),
                version: conn.server_version.clone(),
                origin: "stdio".to_owned(),
            },
            generation: 1,
            manifest_sha256: "0".repeat(64),
        },
        operation: Operation::Invoke,
        target: tool_name.to_owned(),
    };
    let req = SourceRequest {
        key,
        operation: SourceOperation::InvokeTool {
            name: tool_name.to_owned(),
            arguments,
        },
        max_result_bytes: 256 * 1024,
    };
    let guard = conn.handle.lock().await;
    match guard.request(req, CancellationToken::new()).await {
        Ok(reply) => Ok(reply.value.clone()),
        Err(e) => Err(format!("调用 MCP 工具失败：{e}")),
    }
}
