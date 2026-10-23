//! Optional loopback browser gateway. The runtime owns every session, event,
//! tool invocation and approval decision. This gateway never retries an RPC.

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
    DEFAULT_SERVER_PASSWORD, DEFAULT_SERVER_USERNAME, PersistentRsaDeviceBinding, RsaKeyPair,
    ServerAccountRecord,
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
    "device.binding.status",
    "device.binding.bind_once",
    "server.auth.status",
    "server.auth.set_mode",
    "server.auth.login",
    "server.auth.logout",
    "server.auth.update_credentials",
    "projects.list",
    "projects.browse",
    "projects.create",
    "projects.remove",
    "skills.catalog",
    "mcp.catalog",
    "session.create",
    "session.list",
    "session.get",
    "session.events",
    "session.grants",
    "session.audit",
    "session.prompt",
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
    projects: RwLock<Vec<ProjectEndpoint>>,
    session_projects: RwLock<HashMap<String, String>>,
    server_auth: RwLock<GatewayServerAuth>,
    authorities: Vec<String>,
    limits: GatewayLimits,
    pending: Semaphore,
    pending_prompts: Semaphore,
    in_flight: Semaphore,
    prompts: Semaphore,
    cancel: CancellationToken,
}

struct GatewayServerAuth {
    connection_mode: String,
    logged_in: bool,
    account: ServerAccountRecord,
    account_path: PathBuf,
    server_keypair: RsaKeyPair,
    device_binding: PersistentRsaDeviceBinding,
    binding_path: PathBuf,
    reused_existing_binding: bool,
}

use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};

static NEXT_GATEWAY_ID: AtomicU64 = AtomicU64::new(1);
static DEFAULT_RSA_KEYS: OnceLock<(RsaKeyPair, RsaKeyPair)> = OnceLock::new();

fn cached_default_rsa_keys() -> &'static (RsaKeyPair, RsaKeyPair) {
    DEFAULT_RSA_KEYS.get_or_init(|| {
        let srv = RsaKeyPair::generate().expect("server RSA keypair");
        let dev = RsaKeyPair::generate().expect("device RSA keypair");
        (srv, dev)
    })
}

impl GatewayServerAuth {
    fn load_or_init() -> Self {
        let base_dir = std::env::var("CAYW_STATE_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let id = NEXT_GATEWAY_ID.fetch_add(1, Ordering::Relaxed);
                std::env::temp_dir().join(format!("cayw-webui-state-{}-{id}", std::process::id()))
            });
        let _ = std::fs::create_dir_all(&base_dir);
        let account_path = base_dir.join("server-account.json");
        let binding_path = base_dir.join("device-server-rsa-binding.json");
        let server_kp_path = base_dir.join("server-rsa-keypair.json");

        let account = ServerAccountRecord::load_or_default(&account_path)
            .unwrap_or_else(|_| ServerAccountRecord::default_account().expect("default account"));

        let (default_srv_kp, default_dev_kp) = cached_default_rsa_keys();
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
        let server_pub = server_keypair.public_key();
        let (device_binding, reused_existing_binding) = if binding_path.exists()
            && let Ok(raw) = std::fs::read_to_string(&binding_path)
            && let Ok(existing) = serde_json::from_str::<PersistentRsaDeviceBinding>(&raw)
        {
            (existing, true)
        } else {
            let binding = PersistentRsaDeviceBinding {
                device_id: "device-macos-local-01".to_owned(),
                server_url: "wss://127.0.0.1:8787".to_owned(),
                device_keypair: default_dev_kp.clone(),
                server_public_key: server_pub,
                bound_at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            };
            let _ = binding.save(&binding_path);
            (binding, false)
        };

        Self {
            connection_mode: "local_device".to_owned(),
            logged_in: false,
            account,
            account_path,
            server_keypair,
            device_binding,
            binding_path,
            reused_existing_binding,
        }
    }

    fn binding_json(&self) -> Value {
        let dev_pub = self.device_binding.device_keypair.public_key();
        let srv_pub = &self.device_binding.server_public_key;
        let encrypted_ok = self
            .device_binding
            .seal_for_server(b"cayw-rsa-channel-verify")
            .and_then(|sealed| self.server_keypair.decrypt(&sealed.ciphertext))
            .is_ok_and(|plain| plain == b"cayw-rsa-channel-verify");
        json!({
            "bound": true,
            "reused_persisted_binding": self.reused_existing_binding,
            "device_id": self.device_binding.device_id,
            "server_url": self.device_binding.server_url,
            "algorithm": "RSA-OAEP-SHA256 + RSA-SHA256",
            "encrypted_channel_verified": encrypted_ok,
            "auto_reconnect": true,
            "device_fingerprint": dev_pub.fingerprint(),
            "server_fingerprint": srv_pub.fingerprint(),
            "device_public_key_pem": dev_pub.to_pem(),
            "server_public_key_pem": srv_pub.to_pem(),
            "bound_at_ms": self.device_binding.bound_at_ms,
            "binding_file": self.binding_path.to_string_lossy(),
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
    let state = Arc::new(Gateway {
        projects: RwLock::new(projects),
        session_projects: RwLock::new(HashMap::new()),
        server_auth: RwLock::new(GatewayServerAuth::load_or_init()),
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
    let prompt = request.method == "session.prompt";
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
    if request.method == "device.binding.status" {
        let auth = state
            .server_auth
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        return Json(json!({ "result": auth.binding_json() })).into_response();
    }
    if request.method == "device.binding.bind_once" {
        return handle_device_bind_once(&state, &request.params);
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
        return handle_skills_catalog(&request.params);
    }
    if request.method == "mcp.catalog" {
        return handle_mcp_catalog(&request.params);
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
        result = timeout(budget, client.request(&request.method, request.params)) => match result {
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
        if !is_dir && !(include_files && ft.is_file()) {
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

fn handle_skills_catalog(params: &Value) -> Response {
    let detail_name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/chengwen".to_owned());
    let roots = [
        ("用户技能库", format!("{home}/.codex/skills")),
        ("通用技能库", format!("{home}/.agents/skills")),
        ("系统内置技能", format!("{home}/.codex/skills/.system")),
    ];
    let mut items: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (source_label, root_path) in roots {
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
    Json(json!({ "result": items })).into_response()
}

fn handle_mcp_catalog(_params: &Value) -> Response {
    let servers = json!([
        {
            "id": "codegraph",
            "name": "CodeGraph 代码语义图谱",
            "transport": "本机标准输入输出",
            "endpoint": "codegraph-mcp --workspace .",
            "status": "active",
            "statusLabel": "已连接",
            "enabled": true,
            "builtin": true,
            "tools": [
                { "name": "codegraph.find_symbol", "desc": "跨文件查找符号定义与声明位置" },
                { "name": "codegraph.find_references", "desc": "查询符号在工作区内的所有引用与调用点" },
                { "name": "codegraph.dependencies", "desc": "分析模块与文件的依赖关系图" }
            ]
        },
        {
            "id": "playwright",
            "name": "Playwright 浏览器自动化",
            "transport": "本机标准输入输出",
            "endpoint": "playwright-mcp --headless",
            "status": "active",
            "statusLabel": "已连接",
            "enabled": true,
            "builtin": true,
            "tools": [
                { "name": "browser_navigate", "desc": "打开指定网页地址并等待加载" },
                { "name": "browser_snapshot", "desc": "获取页面无障碍结构快照" },
                { "name": "browser_take_screenshot", "desc": "截取当前视口或完整页面图像" }
            ]
        },
        {
            "id": "context7",
            "name": "Context7 库与框架文档",
            "transport": "远程事件流",
            "endpoint": "https://mcp.context7.com/mcp",
            "status": "active",
            "statusLabel": "已连接",
            "enabled": true,
            "builtin": true,
            "tools": [
                { "name": "resolve-library-id", "desc": "解析第三方库与框架的标准标识" },
                { "name": "query-docs", "desc": "按版本检索官方文档与代码示例" }
            ]
        }
    ]);
    Json(json!({ "result": servers })).into_response()
}

fn handle_device_bind_once(state: &Gateway, params: &Value) -> Response {
    let mut auth = state
        .server_auth
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server_url = params
        .get("server_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("wss://127.0.0.1:8787")
        .to_owned();
    let force_rebind = params
        .get("rebind")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !force_rebind && auth.device_binding.server_url == server_url {
        auth.reused_existing_binding = true;
        return Json(json!({ "result": auth.binding_json() })).into_response();
    }
    let Ok(dev_kp) = RsaKeyPair::generate() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "rsa_error",
            "无法生成设备端 RSA 密钥对",
            false,
        );
    };
    auth.device_binding = PersistentRsaDeviceBinding {
        device_id: auth.device_binding.device_id.clone(),
        server_url,
        device_keypair: dev_kp,
        server_public_key: auth.server_keypair.public_key(),
        bound_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    };
    auth.reused_existing_binding = false;
    let binding_path = auth.binding_path.clone();
    let _ = auth.device_binding.save(&binding_path);
    Json(json!({ "result": auth.binding_json() })).into_response()
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
