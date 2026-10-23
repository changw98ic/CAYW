//! Optional loopback browser gateway. The runtime owns every session, event,
//! tool invocation and approval decision. This gateway never retries an RPC.

mod host_services;

use axum::http::Uri;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{DefaultBodyLimit, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use cayw_ipc::{Client, ClientConfig, ClientError, LOOPBACK_DIRECT_HANDSHAKE};
use cayw_remote::{
    DEFAULT_SERVER_PASSWORD, DEFAULT_SERVER_USERNAME, RsaKeyPair, RsaPublicKey, ServerAccountRecord,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fmt, io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore, time::timeout};
use tokio_util::sync::CancellationToken;

/// Requests are bounded before JSON parsing or IPC dispatch.
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// The browser can only call methods from the shared runtime RPC contract.
pub const PUBLIC_METHODS: &[&str] = &[
    "runtime.health",
    "runtime.model.get",
    "runtime.model.configure",
    "devices.list",
    "devices.prepare_bind",
    "devices.bind_status",
    "devices.bind_hello",
    "devices.bind_confirm",
    "devices.heartbeat",
    "devices.poll",
    "devices.respond",
    "devices.select",
    "devices.remove",
    "device.binding.status",
    "server.auth.status",
    "server.auth.set_mode",
    "server.auth.login",
    "server.auth.logout",
    "server.auth.update_credentials",
    "models.config",
    "models.save_provider",
    "models.add_model",
    "models.select",
    "projects.list",
    "projects.browse",
    "projects.create",
    "projects.remove",
    "skills.catalog",
    "skills.save",
    "mcp.catalog",
    "mcp.connect",
    "mcp.disconnect",
    "mcp.call",
    "session.create",
    "session.list",
    "session.get",
    "session.events",
    "session.grants",
    "session.audit",
    "session.plan",
    "session.questions",
    "session.pending_questions",
    "question.answer",
    "session.prompt",
    "task.enqueue",
    "task.resume",
    "task.get",
    "session.cancel",
    "approval.decide",
    "approval.revoke",
    "session.fork",
    "capabilities.list",
];

/// Resource budgets apply independently from the IPC transport's own budgets.
/// Prompts have a separate lane so they cannot occupy every control slot.
#[derive(Clone, Debug)]
pub struct GatewayLimits {
    pub max_in_flight: usize,
    pub max_prompt_in_flight: usize,
    /// Includes reading bodies, queued requests and executing requests.
    pub max_pending_requests: usize,
    pub queue_timeout: Duration,
    pub body_timeout: Duration,
    pub request_timeout: Duration,
    pub prompt_timeout: Duration,
}

impl Default for GatewayLimits {
    fn default() -> Self {
        Self {
            max_in_flight: 32,
            max_prompt_in_flight: 8,
            max_pending_requests: 64,
            queue_timeout: Duration::from_secs(2),
            body_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            prompt_timeout: Duration::from_secs(360),
        }
    }
}

impl GatewayLimits {
    fn validate(&self) -> io::Result<()> {
        let counts = [
            self.max_in_flight,
            self.max_prompt_in_flight,
            self.max_pending_requests,
        ];
        let durations = [
            self.queue_timeout,
            self.body_timeout,
            self.request_timeout,
            self.prompt_timeout,
        ];
        if self.max_in_flight < 2
            || self.max_prompt_in_flight >= self.max_in_flight
            || counts.iter().any(|&n| n == 0 || n > 4096)
            || durations
                .iter()
                .any(|d| d.is_zero() || std::time::Instant::now().checked_add(*d).is_none())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid WebUI limits",
            ));
        }
        Ok(())
    }
}

/// Explicit host credentials stay exclusively on the gateway. Each HTTP call
/// gets its own IPC connection, so a dropped prompt cannot cancel event reads.
/// Opening a connection never repeats a previous call.
#[derive(Clone)]
pub struct IpcEndpoint {
    address: SocketAddr,
    token: String,
}

impl IpcEndpoint {
    /// Connects directly to a local loopback Device without any token.
    pub fn loopback(address: SocketAddr) -> io::Result<Self> {
        if !address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC requires a loopback address",
            ));
        }
        Ok(Self {
            address,
            token: LOOPBACK_DIRECT_HANDSHAKE.to_owned(),
        })
    }

    pub fn new(address: SocketAddr, token: impl Into<String>) -> io::Result<Self> {
        let token = token.into();
        if !address.ip().is_loopback() || !valid_token(&token) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC requires a loopback address and explicit valid token",
            ));
        }
        Ok(Self { address, token })
    }

    async fn connect(&self, budget: Duration) -> Result<Client, ClientError> {
        Client::connect_with_config(
            self.address,
            self.token.clone(),
            ClientConfig {
                max_in_flight: 1,
                request_timeout: budget.saturating_add(Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
    }
}

impl fmt::Debug for IpcEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpcEndpoint")
            .field("address", &self.address)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// Named local runtime project served by the loopback gateway.
#[derive(Clone)]
pub struct ProjectEndpoint {
    id: String,
    label: String,
    workspace: Option<String>,
    ipc: IpcEndpoint,
}

impl ProjectEndpoint {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        ipc: IpcEndpoint,
    ) -> io::Result<Self> {
        let id = id.into();
        let label = label.into();
        let trimmed_label = label.trim();
        if id.is_empty()
            || id.len() > 64
            || !id.is_ascii()
            || id
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
            || trimmed_label.is_empty()
            || trimmed_label.len() > 160
            || trimmed_label.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "project requires a 1..64 ASCII id and 1..160 label without control characters",
            ));
        }
        Ok(Self {
            id,
            label: trimmed_label.to_owned(),
            workspace: None,
            ipc,
        })
    }

    pub fn with_workspace(mut self, workspace: impl Into<String>) -> io::Result<Self> {
        let ws = workspace.into();
        let trimmed = ws.trim();
        if trimmed.is_empty() || trimmed.len() > 1024 || trimmed.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace requires 1..1024 characters without control characters",
            ));
        }
        self.workspace = Some(trimmed.to_owned());
        Ok(self)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }

    pub fn address(&self) -> SocketAddr {
        self.ipc.address
    }
}

impl fmt::Debug for ProjectEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectEndpoint")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("workspace", &self.workspace)
            .field("ipc", &self.ipc)
            .finish()
    }
}

struct Gateway {
    state_dir: PathBuf,
    projects: RwLock<Vec<ProjectEndpoint>>,
    session_projects: RwLock<HashMap<String, String>>,
    server_auth: RwLock<GatewayServerAuth>,
    device_channels: Arc<host_services::DeviceChannelHub>,
    models_config: RwLock<host_services::ModelsConfigState>,
    mcp_host: RwLock<host_services::McpHostState>,
    authorities: Vec<String>,
    limits: GatewayLimits,
    pending: Semaphore,
    pending_prompts: Semaphore,
    in_flight: Semaphore,
    prompts: Semaphore,
    cancel: CancellationToken,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct BoundDeviceEntry {
    device_id: String,
    label: String,
    #[serde(default)]
    device_address: String,
    device_public_key: RsaPublicKey,
    server_fingerprint: String,
    handshake_bind_id: String,
    bound_at_ms: i64,
    last_heartbeat_ms: i64,
    #[serde(default)]
    last_heartbeat_signed_ts_ms: i64,
    #[serde(default)]
    last_heartbeat_seq: u64,
}

#[derive(Clone)]
enum PendingBindPhase {
    WaitingForHello,
    ChallengeIssued {
        label: String,
        device_address: String,
        device_public_key: RsaPublicKey,
        challenge_nonce: String,
    },
    Confirmed {
        device: BoundDeviceEntry,
    },
}

#[derive(Clone)]
struct PendingBindSession {
    created_at_ms: i64,
    phase: PendingBindPhase,
}

struct GatewayServerAuth {
    connection_mode: String,
    logged_in: bool,
    account: ServerAccountRecord,
    account_path: PathBuf,
    server_keypair: RsaKeyPair,
    active_device_id: String,
    devices: Vec<BoundDeviceEntry>,
    devices_path: PathBuf,
    next_device_seq: u64,
    pending_binds: HashMap<String, PendingBindSession>,
}

use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};

static NEXT_GATEWAY_ID: AtomicU64 = AtomicU64::new(1);
static DEFAULT_SERVER_RSA_KEY: OnceLock<RsaKeyPair> = OnceLock::new();

fn cached_server_rsa_key() -> &'static RsaKeyPair {
    DEFAULT_SERVER_RSA_KEY.get_or_init(|| RsaKeyPair::generate().expect("server RSA keypair"))
}

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn fresh_bind_nonce() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    current_time_ms().hash(&mut hasher);
    NEXT_GATEWAY_ID
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    let h1 = hasher.finish();
    current_time_ms()
        .wrapping_add(0x9e3779b97f4a7c15_u64 as i64)
        .hash(&mut hasher);
    let h2 = hasher.finish();
    format!("{h1:016x}{h2:016x}")
}

fn resolve_state_dir(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = explicit {
        let _ = std::fs::create_dir_all(&dir);
        return dir;
    }
    let dir = std::env::var("CAYW_STATE_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let id = NEXT_GATEWAY_ID.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!("cayw-webui-state-{}-{id}", std::process::id()))
        });
    let _ = std::fs::create_dir_all(&dir);
    dir
}

impl GatewayServerAuth {
    fn load_or_init_at(base_dir: &std::path::Path) -> Self {
        let _ = std::fs::create_dir_all(base_dir);
        let account_path = base_dir.join("server-account.json");
        let server_kp_path = base_dir.join("server-rsa-keypair.json");
        let devices_path = base_dir.join("devices-rsa-bindings.json");

        let account = ServerAccountRecord::load_or_default(&account_path)
            .unwrap_or_else(|_| ServerAccountRecord::default_account().expect("default account"));

        let default_srv_kp = cached_server_rsa_key();
        let server_keypair = if server_kp_path.exists()
            && let Ok(raw) = std::fs::read_to_string(&server_kp_path)
            && let Ok(kp) = serde_json::from_str::<RsaKeyPair>(&raw)
        {
            kp
        } else {
            let kp = default_srv_kp.clone();
            if let Ok(json) = serde_json::to_string_pretty(&kp) {
                let _ = std::fs::write(&server_kp_path, json);
            }
            kp
        };
        let devices = if devices_path.exists()
            && let Ok(raw) = std::fs::read_to_string(&devices_path)
            && let Ok(list) = serde_json::from_str::<Vec<BoundDeviceEntry>>(&raw)
        {
            list
        } else {
            Vec::new()
        };
        let active_device_id = devices
            .first()
            .map(|d| d.device_id.clone())
            .unwrap_or_default();
        let next_device_seq = devices.len() as u64 + 1;

        Self {
            connection_mode: "local_device".to_owned(),
            logged_in: false,
            account,
            account_path,
            server_keypair,
            active_device_id,
            devices,
            devices_path,
            next_device_seq,
            pending_binds: HashMap::new(),
        }
    }

    fn save_devices(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.devices) {
            let _ = std::fs::write(&self.devices_path, json);
        }
    }

    fn entry_json(&self, entry: &BoundDeviceEntry) -> Value {
        let now = current_time_ms();
        let connected = now.saturating_sub(entry.last_heartbeat_ms) <= 30_000;
        json!({
            "device_id": entry.device_id,
            "label": entry.label,
            "device_address": if entry.device_address.is_empty() { "自动出站连接".to_owned() } else { entry.device_address.clone() },
            "active": entry.device_id == self.active_device_id,
            "bound": true,
            "connected": connected,
            "algorithm": "RSA-OAEP-SHA256 + RSA-SHA256",
            "encrypted_channel_verified": true,
            "handshake_bind_id": entry.handshake_bind_id,
            "device_fingerprint": entry.device_public_key.fingerprint(),
            "server_fingerprint": entry.server_fingerprint,
            "device_public_key_pem": entry.device_public_key.to_pem(),
            "bound_at_ms": entry.bound_at_ms,
            "last_heartbeat_ms": entry.last_heartbeat_ms,
        })
    }

    fn devices_list_json(&self) -> Value {
        let items: Vec<Value> = self.devices.iter().map(|e| self.entry_json(e)).collect();
        json!({
            "active_device_id": self.active_device_id,
            "devices": items,
        })
    }

    fn binding_json(&self) -> Value {
        let srv_pub = self.server_keypair.public_key();
        if let Some(active) = self
            .devices
            .iter()
            .find(|d| d.device_id == self.active_device_id)
            .or_else(|| self.devices.first())
        {
            return self.entry_json(active);
        }
        json!({
            "bound": false,
            "connected": false,
            "encrypted_channel_verified": false,
            "device_id": null,
            "algorithm": "RSA-OAEP-SHA256 + RSA-SHA256",
            "server_fingerprint": srv_pub.fingerprint(),
            "server_public_key_pem": srv_pub.to_pem(),
        })
    }

    fn status_json(&self) -> Value {
        let acct = self.account.status();
        json!({
            "connection_mode": self.connection_mode,
            "web_device_tokenless": true,
            "logged_in": self.logged_in,
            "username": acct.username,
            "is_default_credentials": acct.is_default_credentials,
            "default_username_hint": DEFAULT_SERVER_USERNAME,
            "default_password_hint": DEFAULT_SERVER_PASSWORD,
            "updated_at_ms": acct.updated_at_ms,
            "device_binding": self.binding_json(),
            "active_device_id": self.active_device_id,
            "devices": self.devices.iter().map(|e| self.entry_json(e)).collect::<Vec<_>>(),
        })
    }
}

/// Serve the embedded interface on an already-bound loopback listener.
pub async fn serve(
    listener: TcpListener,
    ipc_client: IpcEndpoint,
    cancel: CancellationToken,
) -> io::Result<()> {
    serve_with_limits(listener, ipc_client, GatewayLimits::default(), cancel).await
}

/// Serve with explicit resource budgets. Dropping a timed-out IPC request
/// cancels its transport request; the runtime retains responsibility for real
/// effect settlement. A failed side-effecting request is never safe to replay
/// merely because this HTTP connection ended.
pub async fn serve_with_limits(
    listener: TcpListener,
    ipc_client: IpcEndpoint,
    limits: GatewayLimits,
    cancel: CancellationToken,
) -> io::Result<()> {
    let project = ProjectEndpoint::new("default", "默认项目", ipc_client)?;
    serve_projects_with_limits(listener, vec![project], limits, cancel).await
}

/// Serve one or more local runtime projects on an already-bound loopback listener.
pub async fn serve_projects_with_limits(
    listener: TcpListener,
    projects: Vec<ProjectEndpoint>,
    limits: GatewayLimits,
    cancel: CancellationToken,
) -> io::Result<()> {
    serve_projects_with_state_dir(listener, projects, limits, None, cancel).await
}

/// Serve one or more local runtime projects with an explicit state directory.
pub async fn serve_projects_with_state_dir(
    listener: TcpListener,
    projects: Vec<ProjectEndpoint>,
    limits: GatewayLimits,
    state_dir: Option<PathBuf>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WebUI must bind to a loopback address",
        ));
    }
    if projects.is_empty() || projects.len() > 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WebUI requires 1..32 configured local projects",
        ));
    }
    for (index, project) in projects.iter().enumerate() {
        if projects[..index]
            .iter()
            .any(|earlier| earlier.id == project.id)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Project ids must be unique",
            ));
        }
    }
    limits.validate()?;
    let resolved_dir = resolve_state_dir(state_dir);
    let state = Arc::new(Gateway {
        state_dir: resolved_dir.clone(),
        projects: RwLock::new(projects),
        session_projects: RwLock::new(HashMap::new()),
        server_auth: RwLock::new(GatewayServerAuth::load_or_init_at(&resolved_dir)),
        device_channels: Arc::new(host_services::DeviceChannelHub::default()),
        models_config: RwLock::new(host_services::ModelsConfigState::load_or_init(
            &resolved_dir,
        )),
        mcp_host: RwLock::new(host_services::McpHostState::load_or_init(&resolved_dir)),
        authorities: vec![address.to_string(), format!("localhost:{}", address.port())],
        pending: Semaphore::new(limits.max_pending_requests),
        // Reserve a pending slot as well as execution slots for control RPCs.
        pending_prompts: Semaphore::new(limits.max_pending_requests.saturating_sub(1).max(1)),
        in_flight: Semaphore::new(limits.max_in_flight),
        prompts: Semaphore::new(limits.max_prompt_in_flight),
        limits,
        cancel: cancel.clone(),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/styles.css", get(styles))
        .route("/favicon.svg", get(favicon))
        .route("/api/bind", get(bind_script))
        .route("/api/bind/hello", post(bind_hello_http))
        .route("/api/bind/confirm", post(bind_confirm_http))
        .route("/api/device/heartbeat", post(device_heartbeat_http))
        .route("/api/device/poll", post(device_poll_http))
        .route("/api/device/respond", post(device_respond_http))
        .route("/api/rpc", post(rpc))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
}

/// One policy for IPC and web credentials. The host must generate independent
/// random tokens with at least 128 bits of entropy, encoded as 32+ ASCII bytes.
pub fn valid_token(token: &str) -> bool {
    token.len() >= 32
        && token.len() <= 4096
        && token.is_ascii()
        && !token
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

fn same_origin(headers: &HeaderMap, state: &Gateway) -> bool {
    let Some(host) = one_header(headers, "host") else {
        return false;
    };
    if !state.authorities.iter().any(|allowed| allowed == host) {
        return false;
    }
    if headers.contains_key("origin")
        && one_header(headers, "origin") != Some(format!("http://{host}").as_str())
    {
        return false;
    }
    if headers.contains_key("sec-fetch-site")
        && !matches!(
            one_header(headers, "sec-fetch-site"),
            Some("same-origin" | "none")
        )
    {
        return false;
    }
    true
}

async fn guard(State(state): State<Arc<Gateway>>, request: Request, next: Next) -> Response {
    secure_headers(guarded_request(&state, request, next).await)
}

async fn guarded_request(state: &Gateway, request: Request, next: Next) -> Response {
    if !same_origin(request.headers(), state) {
        return error_response(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Host or Origin is not allowed",
            false,
        );
    }
    if request.uri().path() != "/api/rpc" {
        return next.run(request).await;
    }
    if request.uri().query().is_some() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "RPC parameters must not appear in a URL",
            false,
        );
    }
    let Ok(_pending) = state.pending.try_acquire() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "queue_full",
            "Gateway request queue is full; request was not dispatched",
            false,
        );
    };
    let (parts, body) = request.into_parts();
    let body = tokio::select! {
        _ = state.cancel.cancelled() => return shutting_down(false),
        result = timeout(state.limits.body_timeout, to_bytes(body, MAX_REQUEST_BYTES)) => match result {
            Ok(Ok(body)) => body,
            Ok(Err(_)) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "invalid_body", "Request body could not be read within its byte limit", false),
            Err(_) => return error_response(StatusCode::REQUEST_TIMEOUT, "body_timeout", "Request body timed out; request was not dispatched", false),
        }
    };
    next.run(Request::from_parts(parts, Body::from(body))).await
}

fn secure_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=()",
        ),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserRequest {
    method: String,
    params: Value,
    #[serde(default)]
    project: Option<String>,
}

async fn rpc(
    State(state): State<Arc<Gateway>>,
    payload: Result<Json<BrowserRequest>, JsonRejection>,
) -> Response {
    let request = match payload {
        Ok(Json(request)) => request,
        Err(rejection) => {
            let status = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return error_response(
                status,
                "invalid_json",
                "Expected a bounded JSON object with method and params",
                false,
            );
        }
    };
    if !PUBLIC_METHODS.contains(&request.method.as_str()) {
        return error_response(
            StatusCode::FORBIDDEN,
            "method_not_allowed",
            "This method is not part of the public runtime contract",
            false,
        );
    }
    if !request.params.is_object() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "params must be a JSON object",
            false,
        );
    }
    let prompt = matches!(request.method.as_str(), "session.prompt" | "task.resume");
    let _prompt_pending = if prompt {
        match state.pending_prompts.try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "queue_full",
                    "Prompt queue is full; request was not dispatched",
                    false,
                );
            }
        }
    } else {
        None
    };
    let admission = async {
        let prompt_permit = if prompt {
            Some(state.prompts.acquire().await)
        } else {
            None
        };
        let permit = state.in_flight.acquire().await;
        (prompt_permit, permit)
    };
    let _permits = tokio::select! {
        _ = state.cancel.cancelled() => return shutting_down(false),
        result = timeout(state.limits.queue_timeout, admission) => match result {
            Ok(permits) => permits,
            Err(_) => return error_response(StatusCode::SERVICE_UNAVAILABLE, "queue_timeout", "Gateway queue wait timed out; request was not dispatched", false),
        }
    };
    if request.method == "devices.bind_hello" {
        return handle_devices_bind_hello(&state, &request.params);
    }
    if request.method == "devices.bind_confirm" {
        return handle_devices_bind_confirm(&state, &request.params);
    }
    if request.method == "devices.heartbeat" {
        return handle_devices_heartbeat(&state, &request.params);
    }
    if request.method == "devices.poll" {
        return handle_devices_poll(&state, &request.params).await;
    }
    if request.method == "devices.respond" {
        return handle_devices_respond(&state, &request.params);
    }
    if request.method == "server.auth.status" {
        let auth = state
            .server_auth
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        return Json(json!({ "result": auth.status_json() })).into_response();
    }
    if request.method == "server.auth.set_mode" {
        return handle_server_set_mode(&state, &request.params);
    }
    if request.method == "server.auth.login" {
        return handle_server_login(&state, &request.params);
    }
    if request.method == "server.auth.logout" {
        let mut auth = state
            .server_auth
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        auth.logged_in = false;
        return Json(json!({ "result": auth.status_json() })).into_response();
    }
    if request.method == "server.auth.update_credentials" {
        return handle_server_update_credentials(&state, &request.params);
    }
    {
        let auth = state
            .server_auth
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if auth.connection_mode == "remote_server" && !auth.logged_in {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "server_login_required",
                "当前处于 Web ↔ Server 连接模式，请先使用账号密码登录（默认账号：admin / 默认密码：admin）",
                false,
            );
        }
    }
    if request.method == "device.binding.status" {
        let auth = state
            .server_auth
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        return Json(json!({ "result": auth.binding_json() })).into_response();
    }
    if request.method == "devices.list" {
        let auth = state
            .server_auth
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        return Json(json!({ "result": auth.devices_list_json() })).into_response();
    }
    if request.method == "devices.prepare_bind" {
        return handle_devices_prepare_bind(&state, &request.params);
    }
    if request.method == "devices.bind_status" {
        return handle_devices_bind_status(&state, &request.params);
    }
    if request.method == "devices.select" {
        return handle_devices_select(&state, &request.params);
    }
    if request.method == "devices.remove" {
        return handle_devices_remove(&state, &request.params);
    }
    if request.method == "models.config" {
        let cfg = state
            .models_config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        return Json(json!({ "result": cfg.public_json() })).into_response();
    }
    if request.method == "models.save_provider" {
        return handle_models_save_provider(&state, &request.params).await;
    }
    if request.method == "models.add_model" {
        return handle_models_add_model(&state, &request.params).await;
    }
    if request.method == "models.select" {
        return handle_models_select(&state, &request.params).await;
    }
    if request.method == "projects.list" {
        if !request
            .params
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
        {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                "projects.list takes an empty params object",
                false,
            );
        }
        let list = project_list_json(&state);
        return Json(json!({ "result": list })).into_response();
    }
    if request.method == "projects.browse" {
        return handle_projects_browse(&request.params);
    }
    if request.method == "projects.create" {
        return handle_projects_create(&state, &request.params);
    }
    if request.method == "projects.remove" {
        return handle_projects_remove(&state, &request.params);
    }
    if request.method == "skills.catalog" {
        return handle_skills_catalog(&state, &request.params);
    }
    if request.method == "skills.save" {
        return handle_skills_save(&state, &request.params);
    }
    if request.method == "mcp.catalog" {
        return handle_mcp_catalog(&state, &request.params);
    }
    if request.method == "mcp.connect" {
        return handle_mcp_connect(&state, &request.params).await;
    }
    if request.method == "mcp.disconnect" {
        return handle_mcp_disconnect(&state, &request.params).await;
    }
    if request.method == "mcp.call" {
        return handle_mcp_call(&state, &request.params).await;
    }

    let mut rpc_params = request.params.clone();
    if matches!(request.method.as_str(), "task.enqueue" | "session.prompt")
        && let Some(obj) = rpc_params.as_object_mut()
    {
        let plan_mode = obj
            .remove("plan_mode")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let skills: Vec<String> = obj
            .remove("skills")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| {
                v.as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .collect();
        if (plan_mode || !skills.is_empty())
            && let Some(orig_text) = obj.get("text").and_then(Value::as_str).map(str::to_owned)
        {
            let mut prefix = String::new();
            if plan_mode {
                prefix.push_str(
                    "[规划模式] 请先调用 plan.update 制定分步执行计划，再执行具体操作。\n",
                );
            }
            if !skills.is_empty() {
                let skill_prefix = resolve_skills_prompt_prefix(&state.state_dir, &skills);
                if !skill_prefix.is_empty() {
                    prefix.push_str(&skill_prefix);
                }
            }
            obj.insert(
                "text".to_owned(),
                Value::String(format!("{prefix}{orig_text}")),
            );
        }
    }

    let (project, shares_earlier_ipc) = {
        let projects = state
            .projects
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = match request.project.as_deref() {
            None => 0,
            Some(id) => {
                let Some(pos) = projects.iter().position(|project| project.id == id) else {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_params",
                        "Unknown project id",
                        false,
                    );
                };
                pos
            }
        };
        let item = projects[index].clone();
        let shares_earlier = projects[..index]
            .iter()
            .any(|earlier| earlier.ipc.address == item.ipc.address);
        (item, shares_earlier)
    };
    let budget = if prompt {
        state.limits.prompt_timeout
    } else {
        state.limits.request_timeout
    };

    if let Some(device_id) = resolve_target_device(&state, &request.method, &rpc_params) {
        return dispatch_to_bound_device(&state, &device_id, &request.method, rpc_params, budget)
            .await;
    }

    let client = tokio::select! {
        _ = state.cancel.cancelled() => return shutting_down(false),
        result = project.ipc.connect(budget) => match result {
            Ok(client) => client,
            Err(error) => return upstream_error(&state, error),
        }
    };
    let method_name = request.method.clone();
    // The outer deadline drops this connection's request guard; no other HTTP
    // request shares the connection or can lose its event response as a result.
    tokio::select! {
        biased;
        _ = state.cancel.cancelled() => shutting_down(side_effecting(&method_name)),
        result = timeout(budget, client.request(&request.method, rpc_params)) => match result {
            Ok(Ok(mut result)) => {
                // Successful RPC data is a protocol value, never diagnostic
                // text. Reject an accidental credential disclosure as a whole
                // rather than rewriting keys, ids, statuses or user content.
                if contains_credential(&result, &state) {
                    error_response(StatusCode::BAD_GATEWAY, "credential_in_result", "Runtime result contained a gateway credential and was withheld; inspect session events before taking further action", side_effecting(&method_name))
                } else {
                    if matches!(method_name.as_str(), "session.create" | "session.fork")
                        && let Some(session_id) = result.get("id").and_then(Value::as_str)
                    {
                        state
                            .session_projects
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(session_id.to_owned(), project.id.clone());
                    } else if method_name == "session.list"
                        && let Some(items) = result.as_array_mut()
                    {
                        let map = state
                            .session_projects
                            .read()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if !map.is_empty() || shares_earlier_ipc {
                            items.retain(|session| {
                                let Some(sid) = session.get("id").and_then(Value::as_str) else {
                                    return true;
                                };
                                match map.get(sid) {
                                    Some(owner) => owner == &project.id,
                                    None => !shares_earlier_ipc,
                                }
                            });
                        }
                    }
                    Json(json!({"result":result})).into_response()
                }
            },
            Ok(Err(error)) => upstream_error(&state, error),
            Err(_) => error_response(StatusCode::GATEWAY_TIMEOUT, "request_timeout", "Runtime request timed out; check session events before taking further action", side_effecting(&method_name)),
        }
    }
}

fn project_list_json(state: &Gateway) -> Vec<Value> {
    let projects = state
        .projects
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    projects
        .iter()
        .enumerate()
        .map(|(index, item)| {
            json!({
                "id": item.id,
                "label": item.label,
                "workspace": item.workspace,
                "address": item.ipc.address.to_string(),
                "default": index == 0,
            })
        })
        .collect()
}

fn derive_project_id(raw: &str, existing: &[ProjectEndpoint]) -> String {
    let mut slug = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if (ch == '-' || ch == '_') && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let base = slug.trim_matches('-');
    let base = if base.is_empty() || base == "default" {
        "workspace"
    } else {
        base
    };
    if !existing.iter().any(|p| p.id == base) {
        return base.to_owned();
    }
    for n in 2..=64 {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|p| p.id == candidate) {
            return candidate;
        }
    }
    format!("workspace-{}", existing.len() + 1)
}

fn handle_projects_create(state: &Gateway, params: &Value) -> Response {
    let Some(workspace_raw) = params.get("workspace").and_then(Value::as_str) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "projects.create requires a workspace path string",
            false,
        );
    };
    let workspace = workspace_raw.trim();
    if workspace.is_empty() || workspace.len() > 1024 || workspace.chars().any(char::is_control) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "workspace path must be 1..1024 characters without control characters",
            false,
        );
    }
    let inferred_label = workspace
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(workspace);
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(inferred_label);
    let mut projects = state
        .projects
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if projects.len() >= 32 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "Maximum of 32 local project workspaces reached",
            false,
        );
    }
    if projects
        .iter()
        .any(|p| p.workspace.as_deref() == Some(workspace))
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "该本地项目路径已存在于项目空间列表中",
            false,
        );
    }
    let requested_id = params
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let id = match requested_id {
        Some(explicit) => {
            if projects.iter().any(|p| p.id == explicit) {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_params",
                    "Project id already exists",
                    false,
                );
            }
            explicit.to_owned()
        }
        None => derive_project_id(inferred_label, &projects),
    };
    let default_ipc = projects[0].ipc.clone();
    let ipc = match try_spawn_project_runtime(&id, workspace, &default_ipc.token) {
        Ok(Some(spawned)) => spawned,
        Ok(None) => default_ipc,
        Err(msg) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_params", &msg, false);
        }
    };
    let endpoint = match ProjectEndpoint::new(id, label, ipc)
        .and_then(|p| p.with_workspace(workspace.to_owned()))
    {
        Ok(endpoint) => endpoint,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                "Invalid project id, label or workspace path",
                false,
            );
        }
    };
    let created = json!({
        "id": endpoint.id,
        "label": endpoint.label,
        "workspace": endpoint.workspace,
        "address": endpoint.ipc.address.to_string(),
        "default": false,
    });
    projects.push(endpoint);
    Json(json!({ "result": created })).into_response()
}

fn handle_projects_browse(params: &Value) -> Response {
    let raw_path = params
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let default_dir = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "/Volumes/ORICO".to_owned());
    let base_str = raw_path.unwrap_or(&default_dir);
    let mut target = std::path::PathBuf::from(base_str);
    if !target.is_absolute() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "浏览路径必须是绝对路径",
            false,
        );
    }
    if let Some(create_name) = params
        .get("create_dir")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if create_name == "."
            || create_name == ".."
            || create_name.contains('/')
            || create_name.contains('\\')
            || create_name.chars().any(char::is_control)
        {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                "新建文件夹名称不能包含路径分隔符或控制字符",
                false,
            );
        }
        let new_dir = target.join(create_name);
        if let Err(e) = std::fs::create_dir_all(&new_dir) {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                &format!("无法创建文件夹：{e}"),
                false,
            );
        }
        target = new_dir;
    }
    let read_dir = match std::fs::read_dir(&target) {
        Ok(rd) => rd,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                &format!("无法读取目录 {}：{e}", target.display()),
                false,
            );
        }
    };
    let include_files = params
        .get("include_files")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut dirs: Vec<(bool, bool, String, String)> = Vec::new();
    for entry in read_dir.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let is_dir = ft.is_dir();
        if !(is_dir || include_files && ft.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.is_empty() {
            continue;
        }
        let hidden = name.starts_with('.');
        let full = entry.path().to_string_lossy().into_owned();
        dirs.push((hidden, !is_dir, name, full));
    }
    dirs.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.to_lowercase().cmp(&b.2.to_lowercase()))
    });
    let entries: Vec<Value> = dirs
        .into_iter()
        .take(200)
        .map(|(hidden, is_file, name, path)| {
            json!({
                "name": name,
                "path": path,
                "hidden": hidden,
                "is_file": is_file,
            })
        })
        .collect();
    let parent = target
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Json(json!({
        "result": {
            "path": target.to_string_lossy(),
            "parent": parent,
            "entries": entries,
        }
    }))
    .into_response()
}

fn try_spawn_project_runtime(
    id: &str,
    workspace: &str,
    token: &str,
) -> Result<Option<IpcEndpoint>, String> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    let Ok(runtime_bin) = std::env::var("CAYW_RUNTIME_BIN") else {
        return Ok(None);
    };
    if runtime_bin.trim().is_empty() {
        return Ok(None);
    }
    let ws_path = std::path::Path::new(workspace);
    if !ws_path.is_absolute() {
        return Err("新开项目请输入绝对目录路径（例如 /Volumes/... 或 /tmp/...）".into());
    }
    std::fs::create_dir_all(ws_path)
        .map_err(|e| format!("无法创建或访问项目目录 {workspace}：{e}"))?;
    let db_dir = std::env::var("CAYW_RUNTIME_DB_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    std::fs::create_dir_all(&db_dir).map_err(|e| format!("无法创建运行时数据目录：{e}"))?;
    let db_path = std::path::Path::new(&db_dir).join(format!("cayw-project-{id}.db"));

    let mut cmd = Command::new(runtime_bin.trim());
    cmd.arg("serve")
        .arg("--db")
        .arg(&db_path)
        .arg("--workspace")
        .arg(ws_path)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if token != LOOPBACK_DIRECT_HANDSHAKE {
        cmd.arg("--token-env")
            .arg("CAYW_IPC_TOKEN")
            .env("CAYW_IPC_TOKEN", token);
    }
    if std::env::var("CAYW_PROVIDER_BASE_URL").is_err() {
        cmd.arg("--offline");
    } else if std::env::var("CAYW_PROVIDER_API_KEY").is_err() {
        let fallback_key = std::env::var("MAGPIE_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "magpie-local".to_owned());
        cmd.env("CAYW_PROVIDER_API_KEY", fallback_key);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动项目专属 Runtime：{e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取项目 Runtime 启动端口".to_owned())?;
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("读取项目 Runtime 端口失败：{e}"))?;
    let parsed: Value = serde_json::from_str(line.trim())
        .map_err(|_| "项目 Runtime 启动输出格式无效".to_owned())?;
    let addr_str = parsed
        .get("listening")
        .and_then(Value::as_str)
        .ok_or_else(|| "项目 Runtime 未返回监听地址".to_owned())?;
    let addr: SocketAddr = addr_str
        .parse()
        .map_err(|_| "项目 Runtime 监听地址无效".to_owned())?;
    let endpoint = if token == LOOPBACK_DIRECT_HANDSHAKE {
        IpcEndpoint::loopback(addr).map_err(|e| format!("项目 IPC 配置无效：{e}"))?
    } else {
        IpcEndpoint::new(addr, token.to_owned()).map_err(|e| format!("项目 IPC 配置无效：{e}"))?
    };
    std::mem::forget(child);
    Ok(Some(endpoint))
}

fn handle_projects_remove(state: &Gateway, params: &Value) -> Response {
    let Some(id) = params
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "projects.remove requires a project id",
            false,
        );
    };
    let mut projects = state
        .projects
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(index) = projects.iter().position(|p| p.id == id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "Unknown project id",
            false,
        );
    };
    if index == 0 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "默认项目空间不能移除",
            false,
        );
    }
    projects.remove(index);
    drop(projects);
    Json(json!({ "result": project_list_json(state) })).into_response()
}

fn parse_skill_frontmatter(raw: &str, fallback_name: &str) -> (String, String) {
    let mut name = fallback_name.to_owned();
    let mut description = String::new();
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---")
        && let Some(end_idx) = rest.find("\n---")
    {
        let fm = &rest[..end_idx];
        for line in fm.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("name:") {
                let v = v.trim().trim_matches(['"', '\'']);
                if !v.is_empty() {
                    name = v.to_owned();
                }
            } else if let Some(v) = line.strip_prefix("description:") {
                let v = v.trim().trim_matches(['"', '\'']);
                if !v.is_empty() && v != "|" && v != ">" {
                    description = v.chars().take(240).collect();
                }
            }
        }
    }
    if description.is_empty() {
        for line in trimmed.lines() {
            let l = line.trim();
            if !l.is_empty() && !l.starts_with("---") && !l.starts_with('#') {
                description = l.chars().take(240).collect();
                break;
            }
        }
    }
    (name, description)
}

fn skill_roots(state_dir: &std::path::Path) -> Vec<(&'static str, String)> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/chengwen".to_owned());
    vec![
        (
            "自定义保存技能",
            state_dir.join("skills").display().to_string(),
        ),
        ("用户技能库", format!("{home}/.codex/skills")),
        ("通用技能库", format!("{home}/.agents/skills")),
        ("系统内置技能", format!("{home}/.codex/skills/.system")),
    ]
}

fn collect_skills_catalog(state_dir: &std::path::Path, detail_name: Option<&str>) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (source_label, root_path) in skill_roots(state_dir) {
        let Ok(entries) = std::fs::read_dir(&root_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            if dir_name.starts_with('.') {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            let Ok(raw) = std::fs::read_to_string(&skill_md) else {
                continue;
            };
            let (name, desc) = parse_skill_frontmatter(&raw, &dir_name);
            if !seen.insert(name.clone()) {
                continue;
            }
            let include_body = detail_name.is_some_and(|target| target == name);
            items.push(json!({
                "name": name,
                "description": if desc.is_empty() { "本地工作流与领域技能说明".to_owned() } else { desc },
                "source": source_label,
                "path": skill_md.to_string_lossy(),
                "content": if include_body { Some(raw.chars().take(6000).collect::<String>()) } else { None },
            }));
            if items.len() >= 64 {
                break;
            }
        }
    }
    items.sort_by(|a, b| {
        let an = a.get("name").and_then(Value::as_str).unwrap_or("");
        let bn = b.get("name").and_then(Value::as_str).unwrap_or("");
        an.cmp(bn)
    });
    items
}

fn resolve_skills_prompt_prefix(state_dir: &std::path::Path, skill_names: &[String]) -> String {
    let mut out = String::new();
    for target in skill_names {
        let items = collect_skills_catalog(state_dir, Some(target));
        if let Some(found) = items
            .into_iter()
            .find(|i| i.get("name").and_then(Value::as_str) == Some(target.as_str()))
        {
            let path = found.get("path").and_then(Value::as_str).unwrap_or("");
            let content = found
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(1500)
                .collect::<String>();
            out.push_str(&format!("[已启用技能: {target} ({path})]\n{content}\n\n"));
        } else {
            out.push_str(&format!("[已启用技能: {target}]\n"));
        }
    }
    out
}

fn handle_skills_catalog(state: &Gateway, params: &Value) -> Response {
    let detail_name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let items = collect_skills_catalog(&state.state_dir, detail_name);
    Json(json!({ "result": items })).into_response()
}

fn handle_skills_save(state: &Gateway, params: &Value) -> Response {
    let Some(raw_name) = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "技能名称不能为空",
            false,
        );
    };
    let slug: String = raw_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    if slug.is_empty() || slug.len() > 64 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "技能标识必须包含 1..64 个字母、数字或连字符",
            false,
        );
    }
    let desc = params
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("自定义工作流与规范说明");
    let body = params
        .get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let skill_dir = state.state_dir.join("skills").join(&slug);
    if std::fs::create_dir_all(&skill_dir).is_err() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_error",
            "无法创建本地技能存储目录",
            false,
        );
    }
    let skill_md = skill_dir.join("SKILL.md");
    let markdown = if body.starts_with("---") {
        body.to_owned()
    } else {
        format!("---\nname: {slug}\ndescription: {desc}\n---\n\n# {slug}\n\n{body}\n")
    };
    if std::fs::write(&skill_md, &markdown).is_err() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_error",
            "写入 SKILL.md 失败",
            false,
        );
    }
    let catalog = collect_skills_catalog(&state.state_dir, Some(&slug));
    let saved = catalog
        .iter()
        .find(|i| i.get("name").and_then(Value::as_str) == Some(slug.as_str()))
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "name": slug,
                "description": desc,
                "source": "自定义保存技能",
                "path": skill_md.to_string_lossy(),
                "content": markdown,
            })
        });
    Json(json!({
        "result": {
            "saved": saved,
            "catalog": collect_skills_catalog(&state.state_dir, None),
        }
    }))
    .into_response()
}

fn handle_mcp_catalog(state: &Gateway, _params: &Value) -> Response {
    let mut mcp = state
        .mcp_host
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Json(json!({ "result": mcp.catalog_json() })).into_response()
}

async fn handle_mcp_connect(state: &Gateway, params: &Value) -> Response {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("本地 MCP 服务")
        .to_owned();
    let transport = params
        .get("transport")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("本机标准输入输出")
        .to_owned();
    let endpoint = params
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let id_opt = params.get("id").and_then(Value::as_str);
            let mcp = state
                .mcp_host
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            id_opt.and_then(|id| {
                mcp.entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.endpoint.clone())
            })
        });
    let Some(endpoint) = endpoint else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "连接 MCP 服务需要提供启动命令或可执行文件路径 (endpoint)",
            false,
        );
    };
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let slug: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches('-')
                .to_owned();
            if slug.is_empty() {
                format!("mcp-{}", &fresh_bind_nonce()[..8])
            } else {
                slug
            }
        });
    let conn = match host_services::connect_stdio_mcp(&endpoint).await {
        Ok(c) => Arc::new(c),
        Err(err) => {
            return error_response(StatusCode::BAD_REQUEST, "mcp_connect_failed", &err, false);
        }
    };
    let mut mcp = state
        .mcp_host
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(old) = mcp.live.remove(&id) {
        old.control.close();
    }
    let entry = host_services::McpServerEntry {
        id: id.clone(),
        name,
        transport,
        endpoint,
        builtin: id == "local-mcp",
        tools: conn.tools.clone(),
    };
    if let Some(pos) = mcp.entries.iter().position(|e| e.id == id) {
        mcp.entries[pos] = entry;
    } else {
        mcp.entries.push(entry);
    }
    mcp.live.insert(id.clone(), conn);
    mcp.save();
    let catalog = mcp.catalog_json();
    Json(json!({
        "result": {
            "connected_id": id,
            "servers": catalog,
        }
    }))
    .into_response()
}

async fn handle_mcp_disconnect(state: &Gateway, params: &Value) -> Response {
    let Some(id) = params
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "mcp.disconnect requires server id",
            false,
        );
    };
    let removed = {
        let mut mcp = state
            .mcp_host
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mcp.live.remove(id)
    };
    if let Some(conn) = removed {
        conn.control.close();
        conn.control.wait_closed().await;
    }
    let mut mcp = state
        .mcp_host
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Json(json!({
        "result": {
            "disconnected_id": id,
            "servers": mcp.catalog_json(),
        }
    }))
    .into_response()
}

async fn handle_mcp_call(state: &Gateway, params: &Value) -> Response {
    let Some(id) = params
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "mcp.call requires server id",
            false,
        );
    };
    let Some(tool_name) = params
        .get("tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "mcp.call requires tool name",
            false,
        );
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let conn = {
        let mcp = state
            .mcp_host
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mcp.live.get(id).cloned()
    };
    let Some(conn) = conn else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "mcp_not_connected",
            "该 MCP 服务当前未连接，请先建立连接",
            false,
        );
    };
    match host_services::invoke_stdio_mcp_tool(&conn, tool_name, arguments).await {
        Ok(output) => {
            let mut mcp = state
                .mcp_host
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Json(json!({
                "result": {
                    "output": output,
                    "servers": mcp.catalog_json(),
                }
            }))
            .into_response()
        }
        Err(err) => error_response(StatusCode::BAD_GATEWAY, "mcp_call_failed", &err, false),
    }
}

async fn sync_model_to_active_runtime(state: &Gateway, model_id: &str, session_id: Option<&str>) {
    let payload = {
        let cfg = state
            .models_config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cfg.runtime_configure_payload(model_id, session_id)
    };
    if let Some(dev_id) = resolve_target_device(state, "runtime.model.configure", &payload)
        && state.device_channels.is_polling(&dev_id)
    {
        let _ = dispatch_to_bound_device(
            state,
            &dev_id,
            "runtime.model.configure",
            payload.clone(),
            Duration::from_secs(2),
        )
        .await;
    }
    let project_opt = {
        let projects = state
            .projects
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        projects.first().cloned()
    };
    if let Some(project) = project_opt
        && let Ok(client) = project.ipc.connect(state.limits.request_timeout).await
    {
        let _ = timeout(
            state.limits.request_timeout,
            client.request("runtime.model.configure", payload),
        )
        .await;
    }
}

async fn handle_models_save_provider(state: &Gateway, params: &Value) -> Response {
    let Some(id_raw) = params
        .get("id")
        .or_else(|| params.get("provider_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "缺少模型服务商标识 (id)",
            false,
        );
    };
    let base_url = params
        .get("base_url")
        .or_else(|| params.get("baseUrl"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let api = params
        .get("api")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let api_key = params
        .get("api_key")
        .or_else(|| params.get("apiKey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let selected_model = {
        let mut cfg = state
            .models_config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(prov) = cfg.providers.iter_mut().find(|p| p.id == id_raw) {
            if let Some(u) = base_url {
                prov.base_url = u;
            }
            if let Some(n) = name {
                prov.name = n;
            }
            if let Some(a) = api {
                prov.api = a;
            }
            if let Some(k) = api_key {
                prov.api_key_secret = Some(k);
                prov.configured = true;
            }
        } else {
            let default_model_id = format!("{id_raw}-default");
            cfg.providers.push(host_services::ProviderEntryConfig {
                id: id_raw.to_owned(),
                name: name.unwrap_or_else(|| id_raw.to_owned()),
                api: api.unwrap_or_else(|| "chat_completions".to_owned()),
                base_url: base_url.unwrap_or_else(|| "http://127.0.0.1:3425/v1".to_owned()),
                configured: api_key.is_some(),
                api_key_secret: api_key,
                models: vec![host_services::ModelItemConfig {
                    id: default_model_id.clone(),
                    label: default_model_id,
                    desc: "自定义接入模型".to_owned(),
                    context: "128K".to_owned(),
                    tools: true,
                    vision: false,
                    reasoning: false,
                }],
            });
        }
        cfg.save();
        cfg.selected_model.clone()
    };
    sync_model_to_active_runtime(state, &selected_model, None).await;
    let cfg = state
        .models_config
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Json(json!({ "result": cfg.public_json() })).into_response()
}

async fn handle_models_add_model(state: &Gateway, params: &Value) -> Response {
    let Some(provider_id) = params
        .get("provider_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "缺少 provider_id",
            false,
        );
    };
    let Some(model_id) = params
        .get("model_id")
        .or_else(|| params.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "缺少 model_id",
            false,
        );
    };
    let desc = params
        .get("desc")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("自定义推理与工具编排模型")
        .to_owned();
    {
        let mut cfg = state
            .models_config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(prov) = cfg.providers.iter_mut().find(|p| p.id == provider_id) else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                "未找到对应模型服务商",
                false,
            );
        };
        if !prov.models.iter().any(|m| m.id == model_id) {
            prov.models.push(host_services::ModelItemConfig {
                id: model_id.to_owned(),
                label: model_id.to_owned(),
                desc,
                context: "128K".to_owned(),
                tools: true,
                vision: false,
                reasoning: false,
            });
        }
        cfg.selected_model = model_id.to_owned();
        cfg.save();
    }
    sync_model_to_active_runtime(state, model_id, None).await;
    let cfg = state
        .models_config
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Json(json!({ "result": cfg.public_json() })).into_response()
}

async fn handle_models_select(state: &Gateway, params: &Value) -> Response {
    let Some(model_id) = params
        .get("model_id")
        .or_else(|| params.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "缺少要切换的 model_id",
            false,
        );
    };
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    {
        let mut cfg = state
            .models_config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cfg.selected_model = model_id.to_owned();
        if let Some(sid) = &session_id {
            cfg.session_models.insert(sid.clone(), model_id.to_owned());
        }
        cfg.save();
    }
    sync_model_to_active_runtime(state, model_id, session_id.as_deref()).await;
    let cfg = state
        .models_config
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Json(json!({ "result": cfg.public_json() })).into_response()
}

fn resolve_target_device(state: &Gateway, method: &str, params: &Value) -> Option<String> {
    if let Some(sid) = params.get("session_id").and_then(Value::as_str)
        && let Some(dev_id) = state
            .device_channels
            .session_devices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(sid)
            .cloned()
    {
        return if dev_id.is_empty() || dev_id == "local" {
            None
        } else {
            Some(dev_id)
        };
    }
    if let Some(tid) = params.get("task_id").and_then(Value::as_str)
        && let Some(dev_id) = state
            .device_channels
            .task_devices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tid)
            .cloned()
    {
        return if dev_id.is_empty() || dev_id == "local" {
            None
        } else {
            Some(dev_id)
        };
    }
    let _ = method;
    let auth = state
        .server_auth
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let active = auth.active_device_id.trim();
    if active.is_empty() || active == "local" {
        return None;
    }
    if auth.devices.iter().any(|d| d.device_id == active) {
        Some(active.to_owned())
    } else {
        None
    }
}

async fn dispatch_to_bound_device(
    state: &Gateway,
    device_id: &str,
    method: &str,
    params: Value,
    budget: Duration,
) -> Response {
    let (device_pub, server_kp) = {
        let auth = state
            .server_auth
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = auth.devices.iter().find(|d| d.device_id == device_id) else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "unknown_device",
                "当前选中的设备不存在或已解绑",
                false,
            );
        };
        if current_time_ms().saturating_sub(entry.last_heartbeat_ms) > 30_000
            || !state.device_channels.is_polling(device_id)
        {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "device_offline",
                "当前选中的绑定设备离线，请确认设备端常驻进程正在运行",
                false,
            );
        }
        (entry.device_public_key.clone(), auth.server_keypair.clone())
    };
    let request_id = host_services::DeviceChannelHub::next_request_id();
    let req_payload = json!({
        "request_id": request_id,
        "method": method,
        "params": params,
    });
    let Ok(bytes) = serde_json::to_vec(&req_payload) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "serialization_error",
            "Failed to serialize device RPC request",
            false,
        );
    };
    let Ok(ciphertext) = device_pub.encrypt(&bytes) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "encryption_error",
            "Failed to encrypt RPC request with Device RSA public key",
            false,
        );
    };
    let Ok(signature) = server_kp.sign_sha256(format!("{device_id}:{ciphertext}").as_bytes())
    else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "signature_error",
            "Failed to sign RPC request with Server RSA private key",
            false,
        );
    };
    let frame = cayw_remote::RsaSealedFrame {
        device_id: device_id.to_owned(),
        ciphertext,
        signature,
    };
    let reply = state
        .device_channels
        .dispatch(device_id, request_id, frame, budget, &state.cancel)
        .await;
    match reply {
        Ok(payload) => {
            if let Some(err) = payload.get("error") {
                let code = err
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("device_rpc_error");
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Device RPC failed");
                let unknown = err
                    .get("outcome_unknown")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                return error_response(StatusCode::BAD_GATEWAY, code, message, unknown);
            }
            let result = payload.get("result").cloned().unwrap_or(Value::Null);
            if matches!(method, "session.create" | "session.fork")
                && let Some(sid) = result.get("id").and_then(Value::as_str)
            {
                state
                    .device_channels
                    .session_devices
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(sid.to_owned(), device_id.to_owned());
            } else if method == "task.enqueue"
                && let Some(tid) = result.get("id").and_then(Value::as_str)
            {
                state
                    .device_channels
                    .task_devices
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(tid.to_owned(), device_id.to_owned());
            }
            Json(json!({ "result": result })).into_response()
        }
        Err(err) => upstream_error(state, err),
    }
}

fn handle_devices_prepare_bind(state: &Gateway, _params: &Value) -> Response {
    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = current_time_ms();
    auth.pending_binds
        .retain(|_, s| now.saturating_sub(s.created_at_ms) <= 600_000);
    let bind_id = format!("bind-{}", &fresh_bind_nonce()[..16]);
    auth.pending_binds.insert(
        bind_id.clone(),
        PendingBindSession {
            created_at_ms: now,
            phase: PendingBindPhase::WaitingForHello,
        },
    );
    Json(json!({
        "result": {
            "bind_id": bind_id,
            "phase": "waiting_for_hello",
            "bound": false,
            "encrypted_channel_verified": false,
            "connected": false,
            "server_fingerprint": auth.server_keypair.public_key().fingerprint(),
        }
    }))
    .into_response()
}

fn handle_devices_bind_status(state: &Gateway, params: &Value) -> Response {
    let Some(bind_id) = params
        .get("bind_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "devices.bind_status requires bind_id",
            false,
        );
    };
    let auth = state
        .server_auth
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(session) = auth.pending_binds.get(bind_id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "未找到该绑定会话或已过期",
            false,
        );
    };
    let body = match &session.phase {
        PendingBindPhase::WaitingForHello => json!({
            "bind_id": bind_id,
            "phase": "waiting_for_hello",
            "bound": false,
            "encrypted_channel_verified": false,
            "connected": false,
        }),
        PendingBindPhase::ChallengeIssued {
            label,
            device_public_key,
            ..
        } => json!({
            "bind_id": bind_id,
            "phase": "challenge_issued",
            "label": label,
            "device_fingerprint": device_public_key.fingerprint(),
            "bound": false,
            "encrypted_channel_verified": false,
            "connected": false,
        }),
        PendingBindPhase::Confirmed { device } => json!({
            "bind_id": bind_id,
            "phase": "confirmed",
            "bound": true,
            "encrypted_channel_verified": true,
            "connected": current_time_ms().saturating_sub(device.last_heartbeat_ms) <= 30_000,
            "device": auth.entry_json(device),
        }),
    };
    Json(json!({ "result": body })).into_response()
}

fn handle_devices_bind_hello(state: &Gateway, params: &Value) -> Response {
    let Some(resolved_bind_id) = params
        .get("bind_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_or_expired_bind_id",
            "握手必须携带由已授权网页会话预先创建的一次性配对邀请 bind_id",
            false,
        );
    };
    let Some(pub_val) = params.get("device_public_key") else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "握手必须提供设备端生成的 RSA 公钥 (device_public_key)",
            false,
        );
    };
    let Ok(device_public_key) = serde_json::from_value::<RsaPublicKey>(pub_val.clone()) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "设备端 RSA 公钥格式无效",
            false,
        );
    };
    if device_public_key.modulus_bytes().is_err() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "设备端 RSA 公钥模数长度不足",
            false,
        );
    }
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("远程 Device")
        .to_owned();
    let device_address = params
        .get("device_address")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("跨进程 RSA 握手接入")
        .to_owned();

    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = current_time_ms();
    let Some(existing_session) = auth.pending_binds.get(&resolved_bind_id) else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_or_expired_bind_id",
            "未找到该配对邀请 bind_id 或已过期，请先在网页界面点击“添加设备”生成新绑定命令",
            false,
        );
    };
    if now.saturating_sub(existing_session.created_at_ms) > 600_000
        || !matches!(existing_session.phase, PendingBindPhase::WaitingForHello)
    {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_or_expired_bind_id",
            "该配对邀请 bind_id 已使用或已过期，请重新生成一次性绑定命令",
            false,
        );
    }
    let challenge_nonce = fresh_bind_nonce();
    let Ok(encrypted_challenge) = device_public_key.encrypt(challenge_nonce.as_bytes()) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "rsa_challenge_failed",
            "无法使用设备端 RSA 公钥加密挑战随机数",
            false,
        );
    };
    let Ok(server_signature) = auth
        .server_keypair
        .sign_sha256(format!("{resolved_bind_id}:{encrypted_challenge}").as_bytes())
    else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "rsa_sign_failed",
            "服务端签名挑战失败",
            false,
        );
    };
    let server_pub = auth.server_keypair.public_key();
    if let Some(session) = auth.pending_binds.get_mut(&resolved_bind_id) {
        session.phase = PendingBindPhase::ChallengeIssued {
            label,
            device_address,
            device_public_key,
            challenge_nonce,
        };
    }
    Json(json!({
        "result": {
            "bind_id": resolved_bind_id,
            "phase": "challenge_issued",
            "bound": false,
            "encrypted_channel_verified": false,
            "server_public_key": server_pub,
            "encrypted_challenge": encrypted_challenge,
            "server_signature": server_signature,
        }
    }))
    .into_response()
}

fn handle_devices_bind_confirm(state: &Gateway, params: &Value) -> Response {
    let Some(bind_id) = params
        .get("bind_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "确认握手缺少 bind_id",
            false,
        );
    };
    let Some(encrypted_response) = params.get("encrypted_response").and_then(Value::as_str) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "确认握手缺少设备端加密响应 (encrypted_response)",
            false,
        );
    };
    let Some(device_signature) = params.get("device_signature").and_then(Value::as_str) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "确认握手缺少设备端 RSA 签名 (device_signature)",
            false,
        );
    };

    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(session) = auth.pending_binds.get(bind_id).cloned() else {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "未找到待确认的绑定会话",
            false,
        );
    };
    let PendingBindPhase::ChallengeIssued {
        label,
        device_address,
        device_public_key,
        challenge_nonce,
    } = session.phase
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_phase",
            "该绑定会话尚未发起 hello 挑战或已完成确认",
            false,
        );
    };

    // 1. Decrypt the Device's response using the Server's private key.
    let Ok(decrypted_bytes) = auth.server_keypair.decrypt(encrypted_response) else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "handshake_verification_failed",
            "服务端无法解密设备端响应，RSA 加密通道校验失败",
            false,
        );
    };
    let expected_proof = format!("confirm:{bind_id}:{challenge_nonce}");
    if decrypted_bytes != expected_proof.as_bytes() {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "handshake_verification_failed",
            "设备端解密后的挑战随机数不匹配，拒绝绑定",
            false,
        );
    }

    // 2. Verify the Device's RSA-SHA256 signature over the proof using only the Device's public key.
    if !device_public_key.verify_sha256(expected_proof.as_bytes(), device_signature) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "handshake_verification_failed",
            "设备端 RSA 签名核验未通过，拒绝绑定",
            false,
        );
    }

    let now = current_time_ms();
    let fp = device_public_key.fingerprint();
    let existing_id = auth
        .devices
        .iter()
        .find(|d| d.device_public_key.fingerprint() == fp)
        .map(|d| d.device_id.clone());
    let device_id = existing_id.unwrap_or_else(|| {
        let short_fp: String = fp.trim_start_matches("SHA256:").chars().take(12).collect();
        let seq = auth.next_device_seq;
        auth.next_device_seq = auth.next_device_seq.saturating_add(1);
        let candidate = format!("device-rsa-{short_fp}");
        if auth.devices.iter().any(|d| d.device_id == candidate) {
            format!("device-rsa-{short_fp}-{seq}")
        } else {
            candidate
        }
    });
    let entry = BoundDeviceEntry {
        device_id: device_id.clone(),
        label,
        device_address,
        device_public_key,
        server_fingerprint: auth.server_keypair.public_key().fingerprint(),
        handshake_bind_id: bind_id.to_owned(),
        bound_at_ms: now,
        last_heartbeat_ms: now,
        last_heartbeat_signed_ts_ms: 0,
        last_heartbeat_seq: 0,
    };
    if let Some(pos) = auth.devices.iter().position(|d| d.device_id == device_id) {
        auth.devices[pos] = entry.clone();
    } else {
        auth.devices.push(entry.clone());
    }
    auth.active_device_id = device_id;
    if let Some(s) = auth.pending_binds.get_mut(bind_id) {
        s.phase = PendingBindPhase::Confirmed {
            device: entry.clone(),
        };
    }
    auth.save_devices();
    let created = auth.entry_json(&entry);
    let list = auth.devices_list_json();
    Json(json!({
        "result": {
            "created": created,
            "active_device_id": list["active_device_id"],
            "devices": list["devices"],
        }
    }))
    .into_response()
}

#[allow(clippy::result_large_err)]
fn verify_device_signed_liveness(
    state: &Gateway,
    prefix: &str,
    params: &Value,
) -> Result<(String, Value), Response> {
    let Some(device_id) = params.get("device_id").and_then(Value::as_str) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "requires device_id",
            false,
        ));
    };
    let Some(timestamp_str) = params.get("timestamp").and_then(Value::as_str) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "requires timestamp",
            false,
        ));
    };
    let Ok(ts_ms) = timestamp_str.trim().parse::<i64>() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "timestamp must be integer milliseconds",
            false,
        ));
    };
    let seq_opt = params.get("seq").and_then(Value::as_u64);
    let Some(signature) = params.get("signature").and_then(Value::as_str) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "requires RSA signature",
            false,
        ));
    };
    let now = current_time_ms();
    if now.abs_diff(ts_ms) > 30_000 {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "stale_heartbeat",
            "设备心跳时间戳已过期或超出允许的新鲜度窗口 (30s)",
            false,
        ));
    }
    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = auth.devices.iter_mut().find(|d| d.device_id == device_id) else {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "unknown_device",
            "未知设备标识",
            false,
        ));
    };
    let (payload, next_seq) = match seq_opt {
        Some(seq) => {
            if (ts_ms, seq) <= (entry.last_heartbeat_signed_ts_ms, entry.last_heartbeat_seq) {
                return Err(error_response(
                    StatusCode::UNAUTHORIZED,
                    "replayed_heartbeat",
                    "拒绝重放或旧序号的设备心跳报文",
                    false,
                ));
            }
            (format!("{prefix}:{device_id}:{timestamp_str}:{seq}"), seq)
        }
        None => {
            if ts_ms <= entry.last_heartbeat_signed_ts_ms {
                return Err(error_response(
                    StatusCode::UNAUTHORIZED,
                    "replayed_heartbeat",
                    "拒绝重放或旧时间戳的设备心跳报文",
                    false,
                ));
            }
            (
                format!("{prefix}:{device_id}:{timestamp_str}"),
                entry.last_heartbeat_seq.saturating_add(1),
            )
        }
    };
    if !entry
        .device_public_key
        .verify_sha256(payload.as_bytes(), signature)
    {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            "设备心跳 RSA 签名验证失败",
            false,
        ));
    }
    entry.last_heartbeat_ms = now;
    entry.last_heartbeat_signed_ts_ms = ts_ms;
    entry.last_heartbeat_seq = next_seq;
    let entry_clone = entry.clone();
    let updated = auth.entry_json(&entry_clone);
    auth.save_devices();
    Ok((device_id.to_owned(), updated))
}

fn handle_devices_heartbeat(state: &Gateway, params: &Value) -> Response {
    match verify_device_signed_liveness(state, "heartbeat", params) {
        Ok((_, updated)) => Json(json!({ "result": updated })).into_response(),
        Err(resp) => resp,
    }
}

async fn handle_devices_poll(state: &Gateway, params: &Value) -> Response {
    let (device_id, _) = match verify_device_signed_liveness(state, "poll", params) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let wait_ms = params.get("wait_ms").and_then(Value::as_u64).unwrap_or(0);
    let frame = state
        .device_channels
        .poll_next(&device_id, wait_ms, &state.cancel)
        .await;
    Json(json!({
        "result": {
            "device_id": device_id,
            "frame": frame,
        }
    }))
    .into_response()
}

fn handle_devices_respond(state: &Gateway, params: &Value) -> Response {
    let Some(frame_val) = params.get("frame") else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "devices.respond requires frame",
            false,
        );
    };
    let Ok(frame) = serde_json::from_value::<cayw_remote::RsaSealedFrame>(frame_val.clone()) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "invalid RsaSealedFrame",
            false,
        );
    };
    let plain = {
        let mut auth = state
            .server_auth
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = auth
            .devices
            .iter_mut()
            .find(|d| d.device_id == frame.device_id)
        else {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unknown_device",
                "未知设备标识",
                false,
            );
        };
        if !entry.device_public_key.verify_sha256(
            format!("{}:{}", frame.device_id, frame.ciphertext).as_bytes(),
            &frame.signature,
        ) {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "invalid_signature",
                "设备响应 RSA 签名验证未通过",
                false,
            );
        }
        entry.last_heartbeat_ms = current_time_ms();
        let Ok(decrypted) = auth.server_keypair.decrypt(&frame.ciphertext) else {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "decryption_failed",
                "服务端无法解密设备响应报文",
                false,
            );
        };
        decrypted
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&plain) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            "设备响应报文不是有效 JSON",
            false,
        );
    };
    let Some(request_id) = payload
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            "设备响应缺少 request_id",
            false,
        );
    };
    let delivered = state.device_channels.complete(&request_id, payload);
    Json(json!({ "result": { "delivered": delivered } })).into_response()
}

fn handle_devices_select(state: &Gateway, params: &Value) -> Response {
    let device_id = params
        .get("device_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if device_id.is_empty() || device_id == "local" {
        auth.active_device_id.clear();
        return Json(json!({ "result": auth.devices_list_json() })).into_response();
    }
    let Some(found) = auth
        .devices
        .iter()
        .find(|d| d.device_id == device_id)
        .cloned()
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "未找到指定设备",
            false,
        );
    };
    auth.active_device_id = found.device_id.clone();
    Json(json!({ "result": auth.devices_list_json() })).into_response()
}

fn handle_devices_remove(state: &Gateway, params: &Value) -> Response {
    let Some(device_id) = params
        .get("device_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "缺少要移除的 device_id",
            false,
        );
    };
    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(pos) = auth.devices.iter().position(|d| d.device_id == device_id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "未找到指定设备",
            false,
        );
    };
    auth.devices.remove(pos);
    if auth.active_device_id == device_id {
        auth.active_device_id = auth
            .devices
            .first()
            .map(|d| d.device_id.clone())
            .unwrap_or_default();
    }
    auth.save_devices();
    Json(json!({ "result": auth.devices_list_json() })).into_response()
}

fn handle_server_set_mode(state: &Gateway, params: &Value) -> Response {
    let Some(mode) = params.get("mode").and_then(Value::as_str) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "mode must be 'local_device' or 'remote_server'",
            false,
        );
    };
    if !matches!(mode, "local_device" | "remote_server") {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "mode must be 'local_device' or 'remote_server'",
            false,
        );
    }
    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    auth.connection_mode = mode.to_owned();
    Json(json!({ "result": auth.status_json() })).into_response()
}

fn handle_server_login(state: &Gateway, params: &Value) -> Response {
    let username = params.get("username").and_then(Value::as_str).unwrap_or("");
    let password = params.get("password").and_then(Value::as_str).unwrap_or("");
    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !auth.account.verify(username, password) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "Server 账号或密码不正确（初始默认账号：admin，默认密码：admin）",
            false,
        );
    }
    auth.logged_in = true;
    Json(json!({ "result": auth.status_json() })).into_response()
}

fn handle_server_update_credentials(state: &Gateway, params: &Value) -> Response {
    let current_password = params
        .get("current_password")
        .and_then(Value::as_str)
        .unwrap_or("");
    let new_username = params
        .get("new_username")
        .and_then(Value::as_str)
        .unwrap_or("");
    let new_password = params
        .get("new_password")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !auth.logged_in {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "server_login_required",
            "请先登录 Server 账号后再修改账号密码",
            false,
        );
    }
    match auth
        .account
        .update_credentials(current_password, new_username, new_password)
    {
        Ok(_) => {
            let account_path = auth.account_path.clone();
            let _ = auth.account.save(&account_path);
            Json(json!({ "result": auth.status_json() })).into_response()
        }
        Err(cayw_remote::RemoteError::Unauthorized) => error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "当前密码验证失败，请核对后重试",
            false,
        ),
        Err(_) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "新登录账号与新密码不能为空，且不能包含控制字符",
            false,
        ),
    }
}

fn side_effecting(method: &str) -> bool {
    matches!(
        method,
        "session.create"
            | "session.prompt"
            | "task.enqueue"
            | "task.resume"
            | "session.cancel"
            | "approval.decide"
            | "approval.revoke"
            | "session.fork"
    )
}

fn redact(text: &str, state: &Gateway) -> String {
    let mut out = text.to_owned();
    let projects = state
        .projects
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for project in projects.iter() {
        out = out.replace(&project.ipc.token, "[redacted]");
    }
    out
}

fn contains_token(text: &str, state: &Gateway) -> bool {
    state
        .projects
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|project| text.contains(&project.ipc.token))
}

fn contains_credential(value: &Value, state: &Gateway) -> bool {
    match value {
        Value::String(text) => contains_token(text, state),
        Value::Array(values) => values.iter().any(|value| contains_credential(value, state)),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| contains_token(key, state) || contains_credential(value, state)),
        _ => false,
    }
}

fn upstream_error(state: &Gateway, error: ClientError) -> Response {
    error_response(
        StatusCode::BAD_GATEWAY,
        if contains_token(&error.code, state) {
            "upstream_error"
        } else {
            &error.code
        },
        &redact(&error.message, state),
        error.outcome_unknown,
    )
}

fn shutting_down(outcome_unknown: bool) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "shutting_down",
        "Gateway is shutting down; check session state before retrying",
        outcome_unknown,
    )
}

fn error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    outcome_unknown: bool,
) -> Response {
    (
        status,
        Json(json!({"error":{"code":code,"message":message,"outcome_unknown":outcome_unknown}})),
    )
        .into_response()
}

async fn not_found() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "not_found",
        "Unknown endpoint",
        false,
    )
}

async fn method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed for this endpoint",
        false,
    )
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../assets/index.html"),
    )
}

async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/app.js"),
    )
}

async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/styles.css"),
    )
}

async fn favicon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")],
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="8" fill="#0b0d10" stroke="#ff7a33" stroke-width="1.5"/><text x="16" y="22" text-anchor="middle" font-family="system-ui,sans-serif" font-size="18" font-weight="700" fill="#ff7a33">C</text></svg>"##,
    )
}

async fn bind_script(State(state): State<Arc<Gateway>>, headers: HeaderMap, uri: Uri) -> Response {
    let host = one_header(&headers, "host").unwrap_or("127.0.0.1:4321");
    let Some(bind_id) = uri
        .query()
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("bind_id=")))
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_or_expired_bind_id",
            "绑定脚本请求必须携带有效的 bind_id",
            false,
        );
    };
    {
        let auth = state
            .server_auth
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = current_time_ms();
        let valid = auth.pending_binds.get(bind_id).is_some_and(|s| {
            now.saturating_sub(s.created_at_ms) <= 600_000
                && matches!(s.phase, PendingBindPhase::WaitingForHello)
        });
        if !valid {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "invalid_or_expired_bind_id",
                "该绑定会话不存在、已过期或已使用",
                false,
            );
        }
    }
    let bind_id_arg = format!(" --bind-id {bind_id}");
    let cayw_bin = std::env::var("CAYW_RUNTIME_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/Volumes/ORICO/CAYW-webui/target/debug/cayw".to_owned());
    let script = format!(
        r#"#!/bin/sh
set -e
if command -v cayw >/dev/null 2>&1; then
  exec cayw bind --server "http://{host}"{bind_id_arg}
elif [ -x "{cayw_bin}" ]; then
  exec "{cayw_bin}" bind --server "http://{host}"{bind_id_arg}
elif [ -x "/Volumes/ORICO/CAYW-webui/target/debug/cayw" ]; then
  exec "/Volumes/ORICO/CAYW-webui/target/debug/cayw" bind --server "http://{host}"{bind_id_arg}
else
  echo "未找到本机 cayw 客户端可执行文件，请运行: cayw bind --server http://{host}{bind_id_arg}" >&2
  exit 1
fi
"#
    );
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        script,
    )
        .into_response()
}

async fn bind_hello_http(
    State(state): State<Arc<Gateway>>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(params)) = payload else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "Expected JSON object",
            false,
        );
    };
    handle_devices_bind_hello(&state, &params)
}

async fn bind_confirm_http(
    State(state): State<Arc<Gateway>>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(params)) = payload else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "Expected JSON object",
            false,
        );
    };
    handle_devices_bind_confirm(&state, &params)
}

async fn device_heartbeat_http(
    State(state): State<Arc<Gateway>>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(params)) = payload else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "Expected JSON object",
            false,
        );
    };
    handle_devices_heartbeat(&state, &params)
}

async fn device_poll_http(
    State(state): State<Arc<Gateway>>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(params)) = payload else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "Expected JSON object",
            false,
        );
    };
    handle_devices_poll(&state, &params).await
}

async fn device_respond_http(
    State(state): State<Arc<Gateway>>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(params)) = payload else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "Expected JSON object",
            false,
        );
    };
    handle_devices_respond(&state, &params)
}
