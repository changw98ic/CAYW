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
use cayw_ipc::{Client, ClientConfig, ClientError};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fmt, io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore, time::timeout};
use tokio_util::sync::CancellationToken;

/// Requests are bounded before JSON parsing or IPC dispatch.
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// The browser can only call methods from the shared runtime RPC contract.
pub const PUBLIC_METHODS: &[&str] = &[
    "runtime.health",
    "session.create",
    "session.list",
    "session.get",
    "session.events",
    "session.prompt",
    "task.enqueue",
    "task.resume",
    "task.get",
    "session.cancel",
    "approval.decide",
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

struct Gateway {
    ipc: IpcEndpoint,
    web_token: String,
    authorities: Vec<String>,
    limits: GatewayLimits,
    pending: Semaphore,
    pending_prompts: Semaphore,
    in_flight: Semaphore,
    prompts: Semaphore,
    cancel: CancellationToken,
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
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WebUI must bind to a loopback address",
        ));
    }
    let web_token = web_token.into();
    if !valid_token(&web_token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Web token must contain 32..4096 ASCII bytes without whitespace",
        ));
    }
    if web_token == ipc_client.token {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Web and IPC access tokens must differ",
        ));
    }
    limits.validate()?;
    let state = Arc::new(Gateway {
        ipc: ipc_client,
        web_token,
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
    if !authorized(request.headers(), &state.web_token) {
        return error_response(
            StatusCode::FORBIDDEN,
            "forbidden",
            "A valid web Bearer token is required",
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
    let client = tokio::select! {
        _ = state.cancel.cancelled() => return shutting_down(false),
        result = state.ipc.connect(budget) => match result {
            Ok(client) => client,
            Err(error) => return upstream_error(&state, error),
        }
    };
    // The outer deadline drops this connection's request guard; no other HTTP
    // request shares the connection or can lose its event response as a result.
    tokio::select! {
        biased;
        _ = state.cancel.cancelled() => shutting_down(side_effecting(&request.method)),
        result = timeout(budget, client.request(&request.method, request.params)) => match result {
            Ok(Ok(result)) => {
                // Successful RPC data is a protocol value, never diagnostic
                // text. Reject an accidental credential disclosure as a whole
                // rather than rewriting keys, ids, statuses or user content.
                if contains_credential(&result, &state) {
                    error_response(StatusCode::BAD_GATEWAY, "credential_in_result", "Runtime result contained a gateway credential and was withheld; inspect session events before taking further action", side_effecting(&request.method))
                } else {
                    Json(json!({"result":result})).into_response()
                }
            },
            Ok(Err(error)) => upstream_error(&state, error),
            Err(_) => error_response(StatusCode::GATEWAY_TIMEOUT, "request_timeout", "Runtime request timed out; check session events before taking further action", side_effecting(&request.method)),
        }
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
            | "session.fork"
    )
}

fn redact(text: &str, state: &Gateway) -> String {
    text.replace(&state.ipc.token, "[redacted]")
        .replace(&state.web_token, "[redacted]")
}

fn contains_token(text: &str, state: &Gateway) -> bool {
    text.contains(&state.ipc.token) || text.contains(&state.web_token)
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
