//! Optional loopback browser gateway. The runtime owns every session, event,
//! tool invocation and approval decision. This gateway never retries an RPC.

mod access;
mod ownership;
mod workbench;

pub use access::{LanAccount, WebAccess};

/// Host-selected account path and optional paired loopback management listener.
pub struct AccountSettings {
    pub path: std::path::PathBuf,
    pub local_listener: Option<tokio::net::TcpListener>,
}
pub use ownership::OwnershipStore;

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, DefaultBodyLimit, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use cayw_ipc::{Client, ClientConfig, ClientError};
use cayw_panel::{OpenPanelRequest, PanelZone};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fmt, io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore, time::timeout};
use tokio_util::sync::CancellationToken;
use workbench::{Workbench, panel_error_response};

/// Requests are bounded before JSON parsing or IPC dispatch.
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// The browser can only call methods from the shared runtime RPC contract.
pub const PUBLIC_METHODS: &[&str] = &[
    "settings.workspace",
    "settings.workspaces.list",
    "settings.workspaces.open",
    "settings.tools",
    "settings.models.list",
    "settings.models.put",
    "settings.models.use",
    "settings.models.remove",
    "settings.plugins.list",
    "settings.plugins.bind",
    "settings.plugins.install",
    "settings.plugins.update",
    "settings.plugins.enable",
    "settings.plugins.disable",
    "settings.plugins.remove",
    "runtime.health",
    "runtime.resources",
    "runtime.fleet",
    "runtime.fleet.stop",
    "runtime.fleet.steer",
    "runtime.background.list",
    "runtime.background.inspect",
    "runtime.background.logs",
    "runtime.background.kill",
    "runtime.background.result",
    "runtime.background.recover",
    "session.create",
    "session.list",
    "session.get",
    "session.events",
    "session.prompt",
    "task.enqueue",
    "task.resume",
    "task.get",
    "session.tasks",
    "session.cancel",
    "approval.decide",
    "session.fork",
    "capabilities.list",
    "session.plan",
    "plannotator.submit",
    "plannotator.list",
    "artifacts.put",
    "artifacts.update",
    "artifacts.preview",
    "artifacts.list",
    "artifacts.action",
    "artifacts.action_list",
    "artifacts.action_get",
    "artifacts.action_cancel",
    "artifacts.recovery_save",
    "artifacts.recovery_load",
    "session.context_overview",
    "session.run_inspector",
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

/// Live `device_id` → IPC connect targets.
///
/// [`OwnershipStore`] restores which device owns a Session/Task after a gateway
/// restart; this router turns that device id into the IPC endpoint used for the
/// RPC. Fallback to the default/local endpoint happens **only** when the store
/// has no entry (or the bound id is empty/`local`). A non-local owned device
/// that is not registered is an error — never a silent fallback to default.
pub struct DeviceRouter {
    default: IpcEndpoint,
    by_device: std::sync::Mutex<std::collections::BTreeMap<String, IpcEndpoint>>,
}

impl DeviceRouter {
    pub fn new(default: IpcEndpoint) -> Self {
        Self {
            default,
            by_device: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Register (or replace) the IPC endpoint for `device_id`.
    pub fn register(&self, device_id: &str, endpoint: IpcEndpoint) -> io::Result<()> {
        if device_id.is_empty()
            || device_id == "local"
            || device_id.len() > 256
            || device_id
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid device id for endpoint registration",
            ));
        }
        self.by_device
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device_id.to_owned(), endpoint);
        Ok(())
    }

    /// Choose the IPC endpoint for an OwnershipStore-resolved device id.
    ///
    /// - `None` / `""` / `"local"` → default (local) endpoint
    /// - registered non-local device → that device's endpoint
    /// - owned but unregistered non-local device → `Err("unknown_device")`
    ///   (never a silent fallback to default)
    pub fn endpoint_for(&self, owned_device: Option<&str>) -> Result<IpcEndpoint, &'static str> {
        match owned_device.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("local") => Ok(self.default.clone()),
            Some(device_id) => self
                .by_device
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(device_id)
                .cloned()
                .ok_or("unknown_device"),
        }
    }

    fn contains_ipc_token(&self, text: &str) -> bool {
        if text.contains(&self.default.token) {
            return true;
        }
        self.by_device
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|ep| text.contains(&ep.token))
    }

    fn redact_ipc_tokens(&self, text: &str) -> String {
        let mut out = text.replace(&self.default.token, "[redacted]");
        for ep in self
            .by_device
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            out = out.replace(&ep.token, "[redacted]");
        }
        out
    }
}

struct Gateway {
    /// Default local IPC plus any registered device endpoints (F13 routing).
    devices: Arc<DeviceRouter>,
    web_token: String,
    browser: access::BrowserAccess,
    authorities: Vec<String>,
    management_authorities: Vec<String>,
    limits: GatewayLimits,
    pending: Semaphore,
    pending_prompts: Semaphore,
    in_flight: Semaphore,
    prompts: Semaphore,
    cancel: CancellationToken,
    workbench: Arc<Workbench>,
    /// Durable Session/Task → device maps (F13). Present when served via
    /// [`serve_with_ownership`]; create/bind writes and route/restore reads it.
    ownership: Option<Arc<OwnershipStore>>,
    workspace_ids: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

/// Serve the embedded interface on an already-bound loopback listener.
/// The web token is explicit, independent from IPC, and never embedded in HTML.
pub async fn serve(
    listener: TcpListener,
    ipc_client: IpcEndpoint,
    web_token: impl Into<String>,
    cancel: CancellationToken,
) -> io::Result<()> {
    serve_with_limits(
        listener,
        ipc_client,
        web_token,
        GatewayLimits::default(),
        cancel,
    )
    .await
}

/// Serve with explicit resource budgets. Dropping a timed-out IPC request
/// cancels its transport request; the runtime retains responsibility for real
/// effect settlement. A failed side-effecting request is never safe to replay
/// merely because this HTTP connection ended.
pub async fn serve_with_limits(
    listener: TcpListener,
    ipc_client: IpcEndpoint,
    web_token: impl Into<String>,
    limits: GatewayLimits,
    cancel: CancellationToken,
) -> io::Result<()> {
    serve_inner(
        listener,
        Arc::new(DeviceRouter::new(ipc_client)),
        WebAccess::LegacyBearer(web_token.into()),
        limits,
        None,
        None,
        cancel,
    )
    .await
}

/// Same as [`serve_with_limits`], with a durable [`OwnershipStore`] injected into
/// the live Gateway so create/bind persists Session/Task → device affinity and
/// RPC connect selection restores that device after a gateway restart (F13).
///
/// Pass a shared [`DeviceRouter`] so callers can [`DeviceRouter::register`]
/// non-local device IPC endpoints; OwnershipStore device ids select among them.
pub async fn serve_with_ownership(
    listener: TcpListener,
    devices: Arc<DeviceRouter>,
    web_token: impl Into<String>,
    limits: GatewayLimits,
    ownership: Arc<OwnershipStore>,
    cancel: CancellationToken,
) -> io::Result<()> {
    serve_inner(
        listener,
        devices,
        WebAccess::LegacyBearer(web_token.into()),
        limits,
        Some(ownership),
        None,
        cancel,
    )
    .await
}

/// Serve an explicit local or LAN browser mode. Local mode requires loopback;
/// LAN mode allows access without an account and enables password sessions when
/// an account is explicitly configured.
pub async fn serve_configured(
    listener: TcpListener,
    devices: Arc<DeviceRouter>,
    access: WebAccess,
    limits: GatewayLimits,
    ownership: Option<Arc<OwnershipStore>>,
    cancel: CancellationToken,
) -> io::Result<()> {
    serve_inner(listener, devices, access, limits, ownership, None, cancel).await
}

/// Serve a browser with local-only account settings. LAN listeners can share
/// the same gateway with a paired loopback listener, without exposing management.
pub async fn serve_with_account_settings(
    listener: TcpListener,
    devices: Arc<DeviceRouter>,
    access: WebAccess,
    limits: GatewayLimits,
    ownership: Option<Arc<OwnershipStore>>,
    settings: AccountSettings,
    cancel: CancellationToken,
) -> io::Result<()> {
    serve_inner(
        listener,
        devices,
        access,
        limits,
        ownership,
        Some(settings),
        cancel,
    )
    .await
}

async fn serve_inner(
    listener: TcpListener,
    devices: Arc<DeviceRouter>,
    access: WebAccess,
    limits: GatewayLimits,
    ownership: Option<Arc<OwnershipStore>>,
    settings: Option<AccountSettings>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() && !matches!(access, WebAccess::Lan(_)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Non-loopback WebUI requires explicit LAN access mode",
        ));
    }
    let web_token = match &access {
        WebAccess::LegacyBearer(token) => {
            if !valid_token(token) || token == &devices.default.token {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid or shared legacy web credential",
                ));
            }
            token.clone()
        }
        _ => String::new(),
    };
    limits.validate()?;
    let workbench = Arc::new(Workbench::with_builtin_panels().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("failed to seed WebUI panel workbench: {error}"),
        )
    })?);
    let mut authorities = vec![address.to_string()];
    if address.ip().is_loopback() {
        authorities.push(format!("localhost:{}", address.port()));
    }
    let mut management_authorities = Vec::new();
    let (account_path, local_listener) = match settings {
        Some(settings) => (Some(settings.path), settings.local_listener),
        None => (None, None),
    };
    if account_path.is_some() && matches!(access, WebAccess::Local) {
        management_authorities = authorities.clone();
    }
    if let Some(listener) = &local_listener {
        let address = listener.local_addr()?;
        if !address.ip().is_loopback() {
            return Err(io::Error::other(
                "Account management listener must bind loopback",
            ));
        }
        management_authorities = vec![address.to_string(), format!("localhost:{}", address.port())];
        authorities.extend(management_authorities.clone());
    }
    let state = Arc::new(Gateway {
        devices,
        web_token,
        browser: access::BrowserAccess::new(access, address.port(), account_path),
        authorities,
        management_authorities,
        pending: Semaphore::new(limits.max_pending_requests),
        // Reserve a pending slot as well as execution slots for control RPCs.
        pending_prompts: Semaphore::new(limits.max_pending_requests.saturating_sub(1).max(1)),
        in_flight: Semaphore::new(limits.max_in_flight),
        prompts: Semaphore::new(limits.max_prompt_in_flight),
        limits,
        cancel: cancel.clone(),
        workbench,
        ownership,
        workspace_ids: Default::default(),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/styles.css", get(styles))
        .route("/api/rpc", post(rpc))
        .route("/api/panels", post(panels))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state);
    // Cancellation also stops the paired listener when this serving future is
    // dropped; an admin request can never outlive the gateway's owned lifecycle.
    let _stop_on_drop = cancel.clone().drop_guard();
    let admin = local_listener.map(|listener| {
        let app = app.clone();
        let stop = cancel.clone();
        let shutdown = cancel.clone();
        tokio::spawn(async move {
            let result = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await;
            if result.is_err() {
                stop.cancel();
            }
            result
        })
    });
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(cancel.clone().cancelled_owned())
    .await;
    cancel.cancel();
    if let Some(admin) = admin {
        admin.await.map_err(io::Error::other)??;
    }
    result
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

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(received) =
        one_header(headers, "authorization").and_then(|s| s.strip_prefix("Bearer "))
    else {
        return false;
    };
    received.len() == token.len()
        && received
            .as_bytes()
            .iter()
            .zip(token.as_bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

async fn guard(State(state): State<Arc<Gateway>>, request: Request, next: Next) -> Response {
    secure_headers(guarded_request(&state, request, next).await)
}

async fn guarded_request(state: &Gateway, mut request: Request, next: Next) -> Response {
    if !same_origin(request.headers(), state) {
        return error_response(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Host or Origin is not allowed",
            false,
        );
    }
    if request.uri().query().is_some() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Credentials and RPC parameters must not appear in URLs",
            false,
        );
    }
    // Only the accepted socket peer establishes locality; forwarded headers
    // and client-supplied Host alone never grant account administration.
    let manager = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| peer.0.ip().is_loopback())
        && one_header(request.headers(), "host").is_some_and(|host| {
            state
                .management_authorities
                .iter()
                .any(|allowed| allowed == host)
        });
    if let Some(response) = state.browser.handle(&mut request, manager).await {
        return response;
    }
    if request.uri().path() != "/api/rpc" && request.uri().path() != "/api/panels" {
        return next.run(request).await;
    }
    if !state.browser.authenticated(request.headers(), manager) {
        return error_response(
            if state.web_token.is_empty() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            },
            "forbidden",
            "请使用账号密码登录",
            false,
        );
    }
    if request.uri().query().is_some() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "RPC parameters and credentials must not appear in a URL",
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
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'",
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
    workspace_id: Option<String>,
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
    let budget = if prompt {
        state.limits.prompt_timeout
    } else {
        state.limits.request_timeout
    };
    // OwnershipStore restores Session/Task → device; DeviceRouter turns that
    // into the IPC endpoint. Fallback to default only when the store has no entry
    // (or the bound id is local/empty) — never when a non-local owner is missing.
    let restored_device = resolve_owned_device(&state, &request.method, &request.params);
    let mut owned_device =
        match route_device(&state, &request.method, &request.params, restored_device) {
            Ok(device) => device,
            Err((status, body)) => {
                return (
                    StatusCode::from_u16(status).expect("valid status"),
                    Json(body),
                )
                    .into_response();
            }
        };
    if request.method.starts_with("settings.workspaces.") {
        // Catalog/startup is always controlled by this gateway's original host.
        owned_device = None;
    } else if let Some(scope) = request.workspace_id.as_deref() {
        // Existing remote-device sessions remain in the primary gateway's
        // scope. Registered local workspaces use a separate reserved namespace.
        let primary_remote = scope == "local"
            && owned_device.as_deref().is_some_and(|owner| {
                owner != "local"
                    && !state
                        .workspace_ids
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(owner)
            });
        if scope.is_empty()
            || scope.len() > 128
            || (!primary_remote && owned_device.as_deref().is_some_and(|owner| owner != scope))
        {
            return error_response(
                StatusCode::CONFLICT,
                "workspace_mismatch",
                "Request workspace does not match the recorded session/task owner",
                false,
            );
        }
        if !primary_remote {
            owned_device = Some(scope.to_owned());
        }
    }

    let endpoint = match state.devices.endpoint_for(owned_device.as_deref()) {
        Ok(endpoint) => endpoint,
        Err(code) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                code,
                "Owned device has no registered IPC endpoint; register the device or clear ownership",
                false,
            );
        }
    };
    let method = request.method.clone();
    let params = request.params.clone();
    let mut runtime_params = params.clone();
    if matches!(method.as_str(), "session.create" | "session.fork") {
        runtime_params
            .as_object_mut()
            .expect("validated object")
            .remove("device_id");
    }
    let client = tokio::select! {
        _ = state.cancel.cancelled() => return shutting_down(false),
        result = endpoint.connect(budget) => match result {
            Ok(client) => client,
            Err(error) => return upstream_error(&state, error, false),
        }
    };
    // The outer deadline drops this connection's request guard; no other HTTP
    // request shares the connection or can lose its event response as a result.
    tokio::select! {
        biased;
        _ = state.cancel.cancelled() => shutting_down(side_effecting(&request.method)),
        result = timeout(budget, client.request(&method, runtime_params)) => match result {
            Ok(Ok(mut result)) => {
                if method == "settings.workspaces.open"
                    && let Err(message) = register_workspace(&state, &mut result) {
                    return error_response(StatusCode::BAD_GATEWAY, "workspace_receipt_outcome_unknown", message, true);
                }
                // Successful RPC data is a protocol value, never diagnostic
                // text. Reject an accidental credential disclosure as a whole
                // rather than rewriting keys, ids, statuses or user content.
                if contains_credential(&result, &state) {
                    error_response(StatusCode::BAD_GATEWAY, "credential_in_result", "Runtime result contained a gateway credential and was withheld; inspect session events before taking further action", side_effecting(&method))
                } else {
                    match persist_ownership_bind(
                        &state,
                        &method,
                        &result,
                        owned_device.as_deref(),
                    ) {
                        Ok(()) => Json(json!({"result":result})).into_response(),
                        Err(_) => error_response(StatusCode::BAD_GATEWAY, "ownership_persist_outcome_unknown", "Runtime accepted the request but its device ownership could not be saved; inspect the owning device before retrying", true),
                    }
                }
            },
            Ok(Err(error)) => upstream_error(&state, error, side_effecting(&request.method)),
            Err(_) => error_response(StatusCode::GATEWAY_TIMEOUT, "request_timeout", "Runtime request timed out; check session events before taking further action", side_effecting(&request.method)),
        }
    }
}

// IPC addresses and credentials stay on the gateway. The browser only receives
// an opaque registered workspace ID and display metadata.
fn register_workspace(state: &Gateway, result: &mut Value) -> Result<(), &'static str> {
    let id = result["id"].as_str().ok_or("Workspace receipt has no id")?;
    let address: SocketAddr = result["ipc_addr"]
        .as_str()
        .ok_or("Workspace receipt has no endpoint")?
        .parse()
        .map_err(|_| "Workspace endpoint is invalid")?;
    if id == "local" {
        if address != state.devices.default.address {
            return Err("Primary workspace endpoint changed");
        }
    } else {
        if !id.starts_with("workspace-") {
            return Err("Unexpected workspace id");
        }
        let mut known = state
            .workspace_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !known.contains(id) && state.devices.endpoint_for(Some(id)).is_ok() {
            return Err("Workspace ID conflicts with a registered device endpoint");
        }
        let endpoint = IpcEndpoint::new(address, state.devices.default.token.clone())
            .map_err(|_| "Workspace endpoint must use loopback")?;
        state
            .devices
            .register(id, endpoint)
            .map_err(|_| "Workspace endpoint could not be registered")?;
        known.insert(id.to_owned());
    }
    result
        .as_object_mut()
        .ok_or("Workspace receipt must be an object")?
        .remove("ipc_addr");
    Ok(())
}

/// Restore Session/Task → device affinity from the durable store (the maps
/// `DeviceChannelHub` kept only in memory on feat/tui-webui).
fn resolve_owned_device(state: &Gateway, method: &str, params: &Value) -> Option<String> {
    let ownership = state.ownership.as_ref()?;
    if let Some(sid) = params.get("session_id").and_then(Value::as_str)
        && let Some(device) = ownership.session_device(sid)
    {
        return Some(device);
    }
    if let Some(tid) = params.get("task_id").and_then(Value::as_str)
        && let Some(device) = ownership.task_device(tid)
    {
        return Some(device);
    }
    let _ = method;
    None
}

fn route_device(
    state: &Gateway,
    method: &str,
    params: &Value,
    restored: Option<String>,
) -> Result<Option<String>, (u16, Value)> {
    if !params
        .as_object()
        .expect("validated object")
        .contains_key("device_id")
    {
        return Ok(restored);
    }
    if !matches!(method, "session.create" | "session.fork") {
        return Err((
            400,
            json!({"error":{"code":"invalid_params","message":"device_id is only supported for gateway session.create/session.fork routing","outcome_unknown":false}}),
        ));
    }
    let requested = required_string(params, "device_id")?;
    if requested.len() > 256
        || requested
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err((
            400,
            json!({"error":{"code":"invalid_params","message":"invalid device_id","outcome_unknown":false}}),
        ));
    }
    if state.ownership.is_none() && requested != "local" {
        return Err((
            400,
            json!({"error":{"code":"invalid_params","message":"device routing requires a persistent ownership store","outcome_unknown":false}}),
        ));
    }
    if method == "session.fork" && requested != restored.as_deref().unwrap_or("local") {
        return Err((
            400,
            json!({"error":{"code":"invalid_params","message":"this fork API operates on the source device and does not migrate sessions across devices","outcome_unknown":false}}),
        ));
    }
    Ok(Some(requested))
}

/// Persist affinity after create/fork/enqueue — same bind points as the
/// in-memory `DeviceChannelHub` on feat/tui-webui.
fn persist_ownership_bind(
    state: &Gateway,
    method: &str,
    result: &Value,
    restored: Option<&str>,
) -> io::Result<()> {
    let Some(ownership) = state.ownership.as_ref() else {
        return Ok(());
    };
    let device_id = restored.unwrap_or("local");
    if matches!(method, "session.create" | "session.fork") {
        let sid = result.get("id").and_then(Value::as_str).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "session receipt missing id")
        })?;
        ownership.bind_session(sid, device_id)?;
    } else if method == "task.enqueue" {
        let tid = result
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "task receipt missing id"))?;
        ownership.bind_task(tid, device_id)?;
    }
    Ok(())
}

fn side_effecting(method: &str) -> bool {
    (method.starts_with("settings.")
        && !method.ends_with(".list")
        && method != "settings.workspace"
        && method != "settings.tools")
        || matches!(
            method,
            "session.create"
                | "session.prompt"
                | "task.enqueue"
                | "task.resume"
                | "session.cancel"
                | "approval.decide"
                | "session.fork"
                | "runtime.fleet.stop"
                | "runtime.fleet.steer"
                | "runtime.background.kill"
                | "runtime.background.recover"
                | "plannotator.submit"
                | "artifacts.put"
                | "artifacts.update"
                | "artifacts.action"
                | "artifacts.action_cancel"
                | "artifacts.recovery_save"
                | "artifacts.recovery_load"
        )
}

fn redact(text: &str, state: &Gateway) -> String {
    let text = state.devices.redact_ipc_tokens(text);
    if state.web_token.is_empty() {
        text
    } else {
        text.replace(&state.web_token, "[redacted]")
    }
}

fn contains_token(text: &str, state: &Gateway) -> bool {
    state.devices.contains_ipc_token(text)
        || (!state.web_token.is_empty() && text.contains(&state.web_token))
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

fn upstream_outcome_unknown(error: &ClientError, side_effecting: bool) -> bool {
    let transport_failure = matches!(
        error.code.as_str(),
        "connection_lost"
            | "connection_closed"
            | "request_dropped"
            | "request_timeout"
            | "cancel_failed"
            | "write_failed"
            | "protocol_error"
    );
    // IPC distinguishes rejection before admission from an admitted call whose
    // receipt was lost. Preserve that distinction even when codes are shared.
    error.outcome_unknown && (!transport_failure || side_effecting)
}

fn upstream_error(state: &Gateway, error: ClientError, side_effecting: bool) -> Response {
    let outcome_unknown = upstream_outcome_unknown(&error, side_effecting);
    error_response(
        StatusCode::BAD_GATEWAY,
        if contains_token(&error.code, state) {
            "upstream_error"
        } else {
            &error.code
        },
        &redact(&error.message, state),
        outcome_unknown,
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

const PANEL_METHODS: &[&str] = &[
    "panels.catalog",
    "panels.open",
    "panels.activate",
    "panels.close",
    "panels.list_open",
    "panels.interact",
];

async fn panels(
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
    if !PANEL_METHODS.contains(&request.method.as_str()) {
        return error_response(
            StatusCode::FORBIDDEN,
            "method_not_allowed",
            "This method is not part of the panel workbench contract",
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
    match dispatch_panels(&state, &request.method, request.params) {
        Ok(result) => Json(json!({"result": result})).into_response(),
        Err((status, body)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            Json(body),
        )
            .into_response(),
    }
}

fn dispatch_panels(state: &Gateway, method: &str, params: Value) -> Result<Value, (u16, Value)> {
    let workbench = &state.workbench;
    match method {
        "panels.catalog" => {
            Ok(serde_json::to_value(workbench.catalog()).expect("catalog serializes"))
        }
        "panels.open" => {
            let session_id = required_string(&params, "session_id")?;
            let panel_id = required_string(&params, "panel_id")?;
            let zone = optional_zone(&params)?;
            let path = optional_string(&params, "path")?;
            let activate = params
                .get("activate")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let opened = workbench
                .open(
                    &session_id,
                    OpenPanelRequest {
                        panel_id,
                        zone,
                        path,
                        activate,
                    },
                )
                .map_err(panel_error_response)?;
            Ok(serde_json::to_value(opened).expect("opened serializes"))
        }
        "panels.activate" => {
            let session_id = required_string(&params, "session_id")?;
            let instance_id = required_string(&params, "instance_id")?;
            let opened = workbench
                .activate(&session_id, &instance_id)
                .map_err(panel_error_response)?;
            Ok(serde_json::to_value(opened).expect("opened serializes"))
        }
        "panels.close" => {
            let session_id = required_string(&params, "session_id")?;
            let instance_id = required_string(&params, "instance_id")?;
            let opened = workbench
                .close(&session_id, &instance_id)
                .map_err(panel_error_response)?;
            Ok(serde_json::to_value(opened).expect("opened serializes"))
        }
        "panels.list_open" => {
            let session_id = required_string(&params, "session_id")?;
            let zone = optional_zone(&params)?;
            let opened = workbench
                .list_open(&session_id, zone)
                .map_err(panel_error_response)?;
            Ok(serde_json::to_value(opened).expect("opened serializes"))
        }
        "panels.interact" => {
            let session_id = required_string(&params, "session_id")?;
            let instance_id = required_string(&params, "instance_id")?;
            let kind = required_string(&params, "kind")?;
            let path = optional_string(&params, "path")?;
            let opened = workbench
                .interact(&session_id, &instance_id, &kind, path)
                .map_err(panel_error_response)?;
            Ok(serde_json::to_value(opened).expect("opened serializes"))
        }
        _ => Err((
            403,
            json!({"error":{"code":"method_not_allowed","message":"This method is not part of the panel workbench contract","outcome_unknown":false}}),
        )),
    }
}

fn required_string(params: &Value, key: &str) -> Result<String, (u16, Value)> {
    match params.get(key).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => Ok(value.to_owned()),
        _ => Err((
            400,
            json!({"error":{"code":"invalid_params","message":format!("params.{key} must be a non-empty string"),"outcome_unknown":false}}),
        )),
    }
}

fn optional_string(params: &Value, key: &str) -> Result<Option<String>, (u16, Value)> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err((
            400,
            json!({"error":{"code":"invalid_params","message":format!("params.{key} must be a string when set"),"outcome_unknown":false}}),
        )),
    }
}

fn optional_zone(params: &Value) -> Result<Option<PanelZone>, (u16, Value)> {
    match params.get("zone") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value == "sidebar" => Ok(Some(PanelZone::Sidebar)),
        Some(Value::String(value)) if value == "bottom" => Ok(Some(PanelZone::Bottom)),
        Some(_) => Err((
            400,
            json!({"error":{"code":"invalid_params","message":"params.zone must be sidebar or bottom","outcome_unknown":false}}),
        )),
    }
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

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn admission_failures_do_not_become_unknown_for_writes() {
        for code in ["connection_closed", "protocol_error", "invalid_params"] {
            let error = ClientError {
                code: code.into(),
                message: "request was not sent".into(),
                outcome_unknown: false,
            };
            assert!(!upstream_outcome_unknown(&error, true), "{code}");
        }
    }

    #[test]
    fn lost_receipts_follow_the_method_effect_classification() {
        for code in [
            "connection_lost",
            "connection_closed",
            "request_dropped",
            "request_timeout",
            "cancel_failed",
            "write_failed",
            "protocol_error",
        ] {
            let error = ClientError {
                code: code.into(),
                message: "admitted request lost its receipt".into(),
                outcome_unknown: true,
            };
            assert!(upstream_outcome_unknown(&error, true), "{code}");
            assert!(!upstream_outcome_unknown(&error, false), "{code}");
        }
    }

    #[test]
    fn runtime_reported_unknown_is_preserved_for_reads_and_writes() {
        let error = ClientError {
            code: "tool_outcome_unknown".into(),
            message: "inspect the original execution".into(),
            outcome_unknown: true,
        };
        assert!(upstream_outcome_unknown(&error, true));
        assert!(upstream_outcome_unknown(&error, false));
    }
}

#[cfg(test)]
mod device_router_tests {
    use super::*;

    fn ep(port: u16, token: &str) -> IpcEndpoint {
        IpcEndpoint::new(format!("127.0.0.1:{port}").parse().unwrap(), token).unwrap()
    }

    #[test]
    fn endpoint_for_falls_back_only_without_store_entry() {
        let router = DeviceRouter::new(ep(4100, "fedcba9876543210fedcba9876543210"));
        router
            .register("device-alpha", ep(4101, "aabbccddeeff00112233445566778899"))
            .unwrap();

        assert!(router.endpoint_for(None).is_ok());
        assert!(router.endpoint_for(Some("local")).is_ok());
        assert!(router.endpoint_for(Some("")).is_ok());
        assert!(router.endpoint_for(Some("device-alpha")).is_ok());
        assert_eq!(
            router.endpoint_for(Some("device-missing")).unwrap_err(),
            "unknown_device"
        );
    }
}
