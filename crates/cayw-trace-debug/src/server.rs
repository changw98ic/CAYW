//! Loopback-only HTTP surface for the trace debug interface.
//!
//! The server is read-mostly: it exposes the shared [`crate::TraceStore`]
//! snapshot and forwards edits/undos to the store. It has no runtime or IPC
//! write path and never starts, resumes or retries a Runtime task. A browser
//! credential is required for every `/api/` request and is accepted only in
//! the `Authorization` header, never in a URL.
//!
//! Host, `Origin` and `Sec-Fetch-Site` are checked on every request so that a
//! DNS-rebinding page cannot read a loopback snapshot. Static resources are
//! compiled into the binary; no external resource is referenced.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::{
    Error, TraceStore,
    types::{Document, Edit, Undo},
};
use cayw_codeintel::{DefinitionQuery, SymbolVersionBridge};

/// Requests are rejected before JSON parsing or store access at this size.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// How long `GET /api/state?after=N` waits for a newer snapshot.
const LONG_POLL_TIMEOUT: Duration = Duration::from_secs(25);
/// The store has no notification channel contract, so poll its sequence.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

struct Server {
    store: Arc<TraceStore>,
    web_token: String,
    authorities: Vec<String>,
    cancel: CancellationToken,
    /// Same F04 bridge instance Host/editor hold when the app wires one.
    codeintel: Option<SymbolVersionBridge>,
}

/// Serve the trace debug interface on an already-bound loopback listener.
///
/// `web_token` is independent from any IPC credential and is never embedded in
/// a page or URL. Shutdown is graceful: in-flight long polls observe `cancel`
/// and return without waiting for their timeout.
pub async fn serve(
    listener: TcpListener,
    store: Arc<TraceStore>,
    web_token: String,
    cancel: CancellationToken,
) -> Result<(), Error> {
    serve_with_codeintel(listener, store, web_token, cancel, None).await
}

/// Same as [`serve`], sharing one F04 [`SymbolVersionBridge`] with Host/editor.
pub async fn serve_with_codeintel(
    listener: TcpListener,
    store: Arc<TraceStore>,
    web_token: String,
    cancel: CancellationToken,
    codeintel: Option<SymbolVersionBridge>,
) -> Result<(), Error> {
    let address = listener.local_addr().map_err(|error| {
        Error::Invalid(format!("cannot read trace debug listener address: {error}"))
    })?;
    if !address.ip().is_loopback() {
        return Err(Error::Invalid(
            "trace debug server must bind to a loopback address".into(),
        ));
    }
    if !valid_token(&web_token) {
        return Err(Error::Invalid(
            "trace debug web token must contain 32..4096 ASCII bytes without whitespace".into(),
        ));
    }
    let state = Arc::new(Server {
        store,
        web_token,
        authorities: authorities(address),
        cancel: cancel.clone(),
        codeintel,
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/styles.css", get(styles))
        .route("/favicon.svg", get(favicon))
        .route("/api/state", get(state_route))
        .route(
            "/api/codeintel/definitions",
            post(codeintel_definitions_route),
        )
        .route(
            "/api/codeintel/file-version",
            post(codeintel_file_version_route),
        )
        .route("/api/run-inspector", get(run_inspector_route))
        .route("/api/projects/{project_id}/edit", post(edit_route))
        .route("/api/projects/{project_id}/undo", post(undo_route))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .map_err(|error| Error::Invalid(format!("trace debug server failed: {error}")))
}

/// The exact authority fixed by the bound listener. Request headers never
/// expand this set, so neither a rebinding nor a proxied Host can pass.
fn authorities(address: SocketAddr) -> Vec<String> {
    vec![address.to_string()]
}

/// A host-supplied token must have 32..4096 printable ASCII graphic bytes;
/// whitespace and control bytes would be normalized by a header parser.
fn valid_token(token: &str) -> bool {
    (32..=4096).contains(&token.len()) && token.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

fn same_origin(request: &Request, state: &Server) -> bool {
    let headers = request.headers();
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
    let site = one_header(headers, "sec-fetch-site");
    // A user opening this login page from another local port makes a same-site
    // document navigation. This exception never admits API or asset requests.
    let login_navigation = request.method() == Method::GET
        && request.uri().path() == "/"
        && request.uri().query().is_none()
        && site == Some("same-site")
        && one_header(headers, "sec-fetch-mode") == Some("navigate")
        && one_header(headers, "sec-fetch-dest") == Some("document")
        && one_header(headers, "sec-fetch-user") == Some("?1");
    if headers.contains_key("sec-fetch-site")
        && !matches!(site, Some("same-origin" | "none"))
        && !login_navigation
    {
        return false;
    }
    true
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(received) =
        one_header(headers, "authorization").and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    received.len() == token.len()
        && received
            .as_bytes()
            .iter()
            .zip(token.as_bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

async fn guard(State(state): State<Arc<Server>>, request: Request, next: Next) -> Response {
    secure_headers(guarded_request(&state, request, next).await)
}

async fn guarded_request(state: &Server, request: Request, next: Next) -> Response {
    if !same_origin(&request, state) {
        return error_response(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Host or Origin is not allowed",
        );
    }
    if !request.uri().path().starts_with("/api/") {
        return next.run(request).await;
    }
    if !authorized(request.headers(), &state.web_token) {
        let mut response = error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid web Bearer token is required",
        );
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
    // The state and run-inspector endpoints accept query paging (`after` / `limit`);
    // every other API call carries its input in the body. Credentials never belong in a URL.
    if !matches!(request.uri().path(), "/api/state" | "/api/run-inspector")
        && request.uri().query().is_some()
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "API parameters and credentials must not appear in a URL",
        );
    }
    next.run(request).await
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
        ("cross-origin-resource-policy", "same-origin"),
        ("cross-origin-opener-policy", "same-origin"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// The axum build used here does not enable its `query` feature, so the one
/// accepted parameter is parsed explicitly. Unknown or repeated keys fail.
fn query_after(query: Option<&str>) -> Result<Option<u64>, ()> {
    let Some(query) = query else {
        return Ok(None);
    };
    let mut after = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or(())?;
        if key != "after" || after.is_some() {
            return Err(());
        }
        after = Some(value.parse::<u64>().map_err(|_| ())?);
    }
    Ok(after)
}

/// Return the whole [`Document`]. Without `after` this is a plain snapshot;
/// with `after` it waits (bounded) for a newer store sequence.
async fn state_route(State(state): State<Arc<Server>>, uri: Uri) -> Response {
    let after = match query_after(uri.query()) {
        Ok(after) => after,
        Err(()) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The only accepted query parameter is a non-negative integer `after`",
            );
        }
    };
    let document = match after {
        None => state.store.snapshot(),
        Some(after) => match wait_for_change(&state, after).await {
            Some(document) => document,
            None => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "shutting_down",
                    "Trace debug server is shutting down",
                );
            }
        },
    };
    Json(document).into_response()
}

async fn wait_for_change(state: &Server, after: u64) -> Option<Document> {
    let deadline = tokio::time::Instant::now() + LONG_POLL_TIMEOUT;
    loop {
        let document = state.store.snapshot();
        if document.sequence > after {
            return Some(document);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Some(document);
        }
        tokio::select! {
            _ = state.cancel.cancelled() => return None,
            _ = tokio::time::sleep(POLL_INTERVAL.min(remaining)) => {}
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeintelDefinitionsBody {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    file_version: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeintelPathBody {
    path: String,
}

async fn codeintel_definitions_route(
    State(state): State<Arc<Server>>,
    payload: Result<Json<CodeintelDefinitionsBody>, JsonRejection>,
) -> Response {
    let Some(bridge) = state.codeintel.as_ref() else {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "codeintel bridge not attached",
        );
    };
    let body = match decode_body(payload) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    match bridge.definitions(DefinitionQuery {
        name: body.name,
        path: body.path,
        kind: body.kind,
        file_version: body.file_version,
    }) {
        Ok(definitions) => Json(json!({
            "protocol_version": cayw_codeintel::CODEINTEL_PROTOCOL_VERSION,
            "definitions": definitions,
        }))
        .into_response(),
        Err(error) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &error.to_string(),
        ),
    }
}

async fn codeintel_file_version_route(
    State(state): State<Arc<Server>>,
    payload: Result<Json<CodeintelPathBody>, JsonRejection>,
) -> Response {
    let Some(bridge) = state.codeintel.as_ref() else {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "codeintel bridge not attached",
        );
    };
    let body = match decode_body(payload) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    match bridge.file_version(&body.path) {
        Ok(file) => Json(json!({
            "protocol_version": cayw_codeintel::CODEINTEL_PROTOCOL_VERSION,
            "file": file,
        }))
        .into_response(),
        Err(error) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            &error.to_string(),
        ),
    }
}

/// Optional `after` (exclusive offset into the ingested event list) and `limit`
/// for `GET /api/run-inspector`. Unknown keys fail closed.
fn query_run_inspector(query: Option<&str>) -> Result<(u64, Option<usize>), ()> {
    let Some(query) = query else {
        return Ok((0, None));
    };
    let mut after = None;
    let mut limit = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or(())?;
        match key {
            "after" if after.is_none() => {
                after = Some(value.parse::<u64>().map_err(|_| ())?);
            }
            "limit" if limit.is_none() => {
                let parsed = value.parse::<usize>().map_err(|_| ())?;
                if parsed == 0 {
                    return Err(());
                }
                limit = Some(parsed);
            }
            _ => return Err(()),
        }
    }
    Ok((after.unwrap_or(0), limit))
}

/// Fold ingested Runtime events into run-inspector timelines (F06 product entry).
///
/// Supports `?after=&limit=` over the ingested event list so long captures are
/// not silently truncated. The response always reports page metadata.
async fn run_inspector_route(State(state): State<Arc<Server>>, uri: Uri) -> Response {
    let (after, limit) = match query_run_inspector(uri.query()) {
        Ok(page) => page,
        Err(()) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Accepted query parameters are non-negative `after` and positive `limit`",
            );
        }
    };
    let document = state.store.snapshot();
    let events_total = document.events.len() as u64;
    let start = after.min(events_total) as usize;
    let end = match limit {
        Some(limit) => start.saturating_add(limit).min(document.events.len()),
        None => document.events.len(),
    };
    let page = &document.events[start..end];
    let timelines = cayw_run_inspector::timelines_from_runtime_events(page);
    let next_after = end as u64;
    Json(json!({
        "protocol_version": cayw_run_inspector::RUN_INSPECTOR_PROTOCOL_VERSION,
        "timelines": timelines,
        "events_total": events_total,
        "after": start as u64,
        "events_scanned": (end - start) as u64,
        "has_more": end < document.events.len(),
        "next_after": next_after,
    }))
    .into_response()
}

async fn edit_route(
    State(state): State<Arc<Server>>,
    Path(project_id): Path<String>,
    payload: Result<Json<Edit>, JsonRejection>,
) -> Response {
    let edit = match decode_body(payload) {
        Ok(edit) => edit,
        Err(response) => return *response,
    };
    if !valid_project_id(&project_id) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "Project id must contain 1..256 bytes",
        );
    }
    match state
        .store
        .edit(&project_id, edit, state.cancel.child_token())
        .await
    {
        Ok(document) => Json(document).into_response(),
        Err(error) => store_error(error),
    }
}

async fn undo_route(
    State(state): State<Arc<Server>>,
    Path(project_id): Path<String>,
    payload: Result<Json<Undo>, JsonRejection>,
) -> Response {
    let undo = match decode_body(payload) {
        Ok(undo) => undo,
        Err(response) => return *response,
    };
    if !valid_project_id(&project_id) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "Project id must contain 1..256 bytes",
        );
    }
    match state
        .store
        .undo(&project_id, undo, state.cancel.child_token())
        .await
    {
        Ok(document) => Json(document).into_response(),
        Err(error) => store_error(error),
    }
}

fn valid_project_id(project_id: &str) -> bool {
    !project_id.is_empty() && project_id.len() <= 256 && !project_id.contains('/')
}

fn decode_body<T: DeserializeOwned>(
    payload: Result<Json<T>, JsonRejection>,
) -> Result<T, Box<Response>> {
    match payload {
        Ok(Json(value)) => Ok(value),
        Err(rejection) => {
            let response = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    "Request body exceeds 256 KiB",
                )
            } else {
                error_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid",
                    "Expected a valid JSON object matching the edit contract",
                )
            };
            Err(Box::new(response))
        }
    }
}

/// Map every store error without echoing implementation details. Invalid
/// edits are caller errors and conflicts are version races; only file I/O or
/// corrupt persisted JSON is a server failure. Recorded application outcomes
/// arrive as `Ok(Document)` with a blocked evaluation, so they are never a 500.
fn store_error(error: Error) -> Response {
    match error {
        Error::Invalid(message) => {
            error_response(StatusCode::UNPROCESSABLE_ENTITY, "invalid", &message)
        }
        Error::Conflict(message) => error_response(StatusCode::CONFLICT, "conflict", &message),
        Error::Io(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_io",
            "Trace file could not be read or written",
        ),
        Error::Json(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_json",
            "Trace file contains invalid JSON",
        ),
    }
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", "Unknown endpoint")
}

async fn method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed for this endpoint",
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

async fn favicon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#145c52"/><path d="M7 16h6l4-8v16l4-8h4" fill="none" stroke="#fff" stroke-width="2" stroke-linejoin="round"/></svg>"##,
    )
}

async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/styles.css"),
    )
}
