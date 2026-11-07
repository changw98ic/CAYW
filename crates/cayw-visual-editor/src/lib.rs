//! Optional visual editor for an existing project page.
//!
//! The crate is a consumer of the public `cayw-host` SDK. It never becomes a
//! dependency of Core, the Runtime or an Agent. Two loopback origins are kept
//! separate: the editor origin owns the bearer token and the JSON API, while the
//! preview origin serves the project page and its assets. Project HTML never
//! receives the editor token.

pub mod audit;
mod companion;
pub mod framework;
pub mod preview;
pub mod project;

pub use audit::DurableAudit;
pub use project::{NODE_ATTRIBUTE, Project, ProjectError, Snapshot, SourceFile, SourceNode};

use cayw_codeintel::SymbolVersionBridge;
use companion::Companion;
use framework::FrameworkProject;

/// Default location of the optional framework compiler shipped with this crate.
pub fn default_companion() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("framework/companion.mjs")
}

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, BufRead, BufReader, Write},
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, sync::Mutex as AsyncMutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use preview::PreviewState;

const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_AUDIT_LINE_BYTES: usize = 1024 * 1024;
const MAX_PENDING_AUDIT: usize = 100_000;
const EVENT_LOG: &str = "server-events.jsonl";
const UNKNOWN_MARKER: &str = "unknown.json";

/// Startup inputs for the editor service.
///
/// `token` is supplied by the caller, normally from an environment variable.
/// It is never written to a log or to a URL. `state_dir` must be outside the
/// project root and is used for the durable operation audit.
#[derive(Clone)]
pub struct EditorConfig {
    pub project_root: PathBuf,
    pub page: String,
    pub state_dir: PathBuf,
    pub editor_addr: SocketAddr,
    pub preview_addr: SocketAddr,
    pub token: String,
    /// None uses the native HTML adapter; framework projects select react or vue.
    pub framework: Option<String>,
    pub node: PathBuf,
    pub companion: PathBuf,
    /// Shared F04 symbol/version bridge (same instance as Host/Trace when wired).
    pub codeintel: Option<SymbolVersionBridge>,
}

impl std::fmt::Debug for EditorConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EditorConfig")
            .field("project_root", &self.project_root)
            .field("page", &self.page)
            .field("state_dir", &self.state_dir)
            .field("editor_addr", &self.editor_addr)
            .field("preview_addr", &self.preview_addr)
            .field("framework", &self.framework)
            .field("node", &self.node)
            .field("companion", &self.companion)
            .field("codeintel", &self.codeintel.is_some())
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A failure before both origins are serving.
#[derive(Debug, thiserror::Error)]
pub enum EditorError {
    #[error("editor token must be at least 32 visible ASCII characters")]
    InvalidToken,
    #[error("editor and preview addresses must be loopback addresses")]
    NonLoopback,
    #[error("editor and preview must not share one fixed address")]
    SameAddress,
    #[error("project root is not a directory")]
    ProjectRoot,
    #[error("state directory must be outside the project")]
    StateInside,
    #[error("unresolved previous edit: {0}")]
    Unresolved(String),
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error(transparent)]
    Preview(#[from] preview::PreviewError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// URLs of a running editor. They contain no token.
pub struct EditorHandle {
    pub editor_url: String,
    pub preview_url: String,
    pub editor_origin: String,
    pub preview_origin: String,
    pub editor_addr: SocketAddr,
    pub preview_addr: SocketAddr,
    pub normal_preview_url: String,
    pub normal_preview_origin: String,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<io::Result<()>>>,
    companion: Option<Arc<Companion>>,
}

impl EditorHandle {
    /// Requests shutdown without waiting.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Stops both listeners and waits for their tasks with a bounded grace.
    /// `drain` keeps the handle movable even though it also implements `Drop`.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(companion) = self.companion.take() {
            companion.shutdown().await;
        }
        for mut task in self.tasks.drain(..) {
            match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
                Ok(_) => {}
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                }
            }
        }
    }
}

impl Drop for EditorHandle {
    fn drop(&mut self) {
        // Dropping the handle must never leave either loopback listener serving.
        self.cancel.cancel();
    }
}

/// Binds the two loopback origins and starts serving. The returned handle is
/// ready to accept connections. No token is printed or embedded in the URLs.
pub async fn start(config: EditorConfig) -> Result<EditorHandle, EditorError> {
    if !valid_token(&config.token) {
        return Err(EditorError::InvalidToken);
    }
    if !config.editor_addr.ip().is_loopback() || !config.preview_addr.ip().is_loopback() {
        return Err(EditorError::NonLoopback);
    }
    if config.editor_addr.port() != 0 && config.editor_addr == config.preview_addr {
        return Err(EditorError::SameAddress);
    }

    let root = config.project_root.canonicalize()?;
    if !root.is_dir() {
        return Err(EditorError::ProjectRoot);
    }
    let requested_state = std::path::absolute(&config.state_dir)?;
    if requested_state
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(EditorError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state directory must not contain '..'",
        )));
    }
    // Resolve the nearest existing ancestor before creating anything. On macOS
    // an alias such as /var vs /private/var could otherwise hide that the new
    // directory is inside the project, and the directory would already exist
    // by the time the canonical check rejected it.
    let resolved_state = resolve_existing_ancestor(&requested_state)?;
    if resolved_state.starts_with(&root) || root.starts_with(&resolved_state) {
        return Err(EditorError::StateInside);
    }
    std::fs::create_dir_all(&resolved_state)?;
    let state_dir = resolved_state.canonicalize()?;
    if state_dir.starts_with(&root) || root.starts_with(&state_dir) {
        return Err(EditorError::StateInside);
    }

    let audit = StateAudit::open(&state_dir)?;
    let assets = UiAssets::embedded();
    let editor_listener = TcpListener::bind(config.editor_addr).await?;
    let editor_addr = editor_listener.local_addr()?;
    let editor_origin = origin_for(editor_addr);
    let editor_authority = authority_for(editor_addr);
    let cancel = CancellationToken::new();
    let startup_guard = cancel.clone().drop_guard();
    let mut tasks = Vec::new();

    let (
        project,
        preview_state,
        companion,
        preview_url,
        preview_addr,
        normal_preview_url,
        normal_preview_origin,
    ) = if let Some(framework) = config.framework.as_deref() {
        if !matches!(framework, "react" | "vue") {
            return Err(
                ProjectError::new("invalid_framework", "framework must be react or vue").into(),
            );
        }
        let companion =
            Arc::new(Companion::spawn(&config.node, &config.companion, &state_dir).await?);
        let project = FrameworkProject::open_with_codeintel(
            &root,
            &config.page,
            &state_dir,
            framework,
            companion.clone(),
            config.codeintel.clone(),
        )
        .await?;
        let snapshot = project.snapshot().await?;
        let ready = companion
            .call(
                "preview.start",
                json!({
                    "root": root,
                    "state_dir": state_dir,
                    "page": config.page,
                    "framework": framework,
                    "editor_origin": editor_origin,
                    "preview_addr": config.preview_addr.to_string(),
                    "bridge_js": String::from_utf8_lossy(&assets.bridge_js),
                    "files": snapshot.files,
                    "revision": snapshot.revision,
                }),
            )
            .await?;
        let ready_string = |key: &str| -> Result<String, ProjectError> {
            ready
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    ProjectError::new(
                        "companion_protocol",
                        format!("preview response is missing {key}"),
                    )
                })
        };
        let preview_addr: SocketAddr = ready_string("preview_addr")?
            .parse()
            .map_err(|_| ProjectError::new("companion_protocol", "preview address is invalid"))?;
        let preview_origin = origin_for(preview_addr);
        let preview_url = ready_string("preview_url")?;
        let normal_preview_url = ready_string("normal_preview_url")?;
        let normal_preview_origin = ready_string("normal_preview_origin")?;
        if !preview_addr.ip().is_loopback()
            || preview_addr.port() == 0
            || preview_addr == editor_addr
            || ready_string("preview_origin")? != preview_origin
            || !preview_url.starts_with(&format!("{preview_origin}/"))
        {
            return Err(ProjectError::new(
                "companion_protocol",
                "preview response has an invalid origin",
            )
            .into());
        }
        let normal_addr: SocketAddr = normal_preview_origin
            .strip_prefix("http://")
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                ProjectError::new("companion_protocol", "ordinary preview origin is invalid")
            })?;
        if !normal_addr.ip().is_loopback()
            || normal_addr.port() == 0
            || normal_addr == editor_addr
            || normal_addr == preview_addr
            || !normal_preview_url.starts_with(&format!("{normal_preview_origin}/"))
        {
            return Err(ProjectError::new(
                "companion_protocol",
                "ordinary preview address is invalid",
            )
            .into());
        }
        let child = companion.clone();
        let stop = cancel.clone();
        tasks.push(tokio::spawn(async move {
            stop.cancelled().await;
            child.shutdown().await;
            Ok(())
        }));
        (
            ProjectBackend::Framework(project),
            None,
            Some(companion),
            preview_url,
            preview_addr,
            normal_preview_url,
            normal_preview_origin,
        )
    } else {
        let project =
            Project::open_with_codeintel(&root, &config.page, &state_dir, config.codeintel.clone())
                .await?;
        let snapshot = project.snapshot().await?;
        let listener = TcpListener::bind(config.preview_addr).await?;
        let addr = listener.local_addr()?;
        let origin = origin_for(addr);
        let preview = Arc::new(PreviewState::open(
            &root,
            &config.page,
            editor_origin.clone(),
            authority_for(addr),
            assets.bridge_js.clone(),
        )?);
        preview.set_edit_html(Some(snapshot.html));
        let url = format!("{origin}/{}", preview.page());
        tasks.push(tokio::spawn(serve(
            listener,
            preview::router(preview.clone()),
            cancel.clone(),
        )));
        (
            ProjectBackend::Html(project),
            Some(preview),
            None,
            url.clone(),
            addr,
            url,
            origin,
        )
    };
    let snapshot = project.snapshot().await?;
    let preview_origin = origin_for(preview_addr);
    let state = Arc::new(ServerState {
        project: AsyncMutex::new(project),
        mutation: AsyncMutex::new(()),
        snapshot: RwLock::new(snapshot),
        token: config.token,
        editor_origin: editor_origin.clone(),
        editor_authority,
        preview_origin: preview_origin.clone(),
        preview_url: preview_url.clone(),
        normal_preview_url: normal_preview_url.clone(),
        normal_preview_origin: normal_preview_origin.clone(),
        preview: preview_state,
        companion: companion.clone(),
        audit,
        backend_unknown: AtomicBool::new(false),
        assets,
        csp: editor_csp(&preview_origin),
        codeintel: config.codeintel,
    });
    tasks.push(tokio::spawn(serve(
        editor_listener,
        editor_router(state),
        cancel.clone(),
    )));
    startup_guard.disarm();

    Ok(EditorHandle {
        editor_url: format!("{editor_origin}/"),
        preview_url,
        editor_origin,
        preview_origin,
        normal_preview_url,
        normal_preview_origin,
        editor_addr,
        preview_addr,
        cancel,
        tasks,
        companion,
    })
}

async fn serve(listener: TcpListener, router: Router, cancel: CancellationToken) -> io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { cancel.cancelled_owned().await })
        .await
}

fn valid_token(token: &str) -> bool {
    token.len() >= 32 && token.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

/// Canonical HTTP authority. Browsers omit the default port, so an editor or
/// preview bound to port 80 must compare Host and Origin without `:80`.
fn authority_for(addr: SocketAddr) -> String {
    let mut authority = addr.to_string();
    if addr.port() == 80 {
        authority.truncate(authority.len() - 3);
    }
    authority
}

fn origin_for(addr: SocketAddr) -> String {
    format!("http://{}", authority_for(addr))
}

/// Resolves the longest existing prefix of `path` and reattaches the missing
/// suffix. The result is suitable for an inside-the-project comparison before
/// any directory is created.
fn resolve_existing_ancestor(path: &Path) -> io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match current.canonicalize() {
            Ok(base) => {
                let mut resolved = base;
                for name in missing.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = current.file_name() else {
                    return Err(error);
                };
                missing.push(name.to_os_string());
                if !current.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

enum ProjectBackend {
    Html(Project),
    Framework(FrameworkProject),
}

impl ProjectBackend {
    async fn snapshot(&self) -> Result<Snapshot, ProjectError> {
        match self {
            Self::Html(project) => project.snapshot().await,
            Self::Framework(project) => project.snapshot().await,
        }
    }

    async fn apply(&self, operation: Value) -> Result<Snapshot, ProjectError> {
        match self {
            Self::Html(project) => project.apply(operation).await,
            Self::Framework(project) => project.apply(operation).await,
        }
    }

    async fn save(&self, expected: &str) -> Result<Snapshot, ProjectError> {
        match self {
            Self::Html(project) => project.save(expected).await,
            Self::Framework(project) => project.save(expected).await,
        }
    }

    async fn reload(&self) -> Result<Snapshot, ProjectError> {
        match self {
            Self::Html(project) => project.reload().await,
            Self::Framework(project) => project.reload().await,
        }
    }

    async fn dirty(&self) -> bool {
        match self {
            Self::Html(project) => project.dirty().await,
            Self::Framework(project) => project.dirty().await,
        }
    }

    async fn history_flags(&self) -> (bool, bool) {
        match self {
            Self::Html(project) => project.history_flags().await,
            Self::Framework(project) => project.history_flags().await,
        }
    }
}

struct ServerState {
    project: AsyncMutex<ProjectBackend>,
    /// One gate for every state-changing request and for document reads. It
    /// spans the unknown check, the audit start, the project change, audit
    /// settlement, publication and response generation, so an older request
    /// can never publish after a newer one. The project mutex may remain, but
    /// this gate is the single serialization point.
    mutation: AsyncMutex<()>,
    snapshot: RwLock<Snapshot>,
    token: String,
    editor_origin: String,
    editor_authority: String,
    preview_origin: String,
    preview_url: String,
    normal_preview_url: String,
    normal_preview_origin: String,
    preview: Option<Arc<PreviewState>>,
    companion: Option<Arc<Companion>>,
    audit: StateAudit,
    backend_unknown: AtomicBool,
    assets: UiAssets,
    csp: HeaderValue,
    codeintel: Option<SymbolVersionBridge>,
}

impl ServerState {
    fn publish_codeintel(&self, bridge: &SymbolVersionBridge, snapshot: &Snapshot) {
        if snapshot.files.is_empty() {
            let language = cayw_codeintel::language_for_path(&snapshot.path);
            let _ = cayw_codeintel::publish_bytes(
                bridge,
                &snapshot.path,
                snapshot.source.as_bytes(),
                language,
            );
        } else {
            for file in &snapshot.files {
                let language = cayw_codeintel::language_for_path(&file.path);
                let _ = cayw_codeintel::publish_bytes(
                    bridge,
                    &file.path,
                    file.source.as_bytes(),
                    language,
                );
            }
        }
    }

    fn is_unknown(&self) -> bool {
        self.backend_unknown.load(Ordering::Acquire) || self.audit.unknown()
    }

    fn available(&self) -> bool {
        self.companion
            .as_ref()
            .is_none_or(|child| child.available())
    }

    fn cached_snapshot(&self) -> Snapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Builds the document response from an already captured snapshot and the
    /// authoritative history flags. Callers take both under the mutation gate
    /// and never while still holding the project mutex.
    fn document_response(
        &self,
        snapshot: Snapshot,
        dirty: bool,
        can_undo: bool,
        can_redo: bool,
    ) -> Response {
        json_response(
            StatusCode::OK,
            &json!({
                "snapshot": snapshot,
                "preview_origin": self.preview_origin.as_str(),
                "preview_url": self.preview_url.as_str(),
                "normal_preview_url": self.normal_preview_url.as_str(),
                "normal_preview_origin": self.normal_preview_origin.as_str(),
                "backend_available": self.companion.as_ref().is_none_or(|child| child.available()),
                "dirty": dirty,
                "can_undo": can_undo,
                "can_redo": can_redo,
            }),
        )
    }

    /// Consistent read boundary for `GET /api/document`. The caller holds the
    /// mutation gate; the project mutex is acquired once and released before
    /// any response is built, so there is no nested re-acquisition.
    async fn read_document(&self) -> (Snapshot, bool, bool, bool) {
        let snapshot = self.cached_snapshot();
        let project = self.project.lock().await;
        let dirty = project.dirty().await;
        let (can_undo, can_redo) = project.history_flags().await;
        (snapshot, dirty, can_undo, can_redo)
    }

    /// Some failed mutations still refresh source-engine state. A stale
    /// compare-and-swap, for example, updates the reported source digest while
    /// keeping the draft. Rebuild the cached snapshot so a later document read
    /// never pairs the old source digest with the new history flags. The caller
    /// holds the mutation gate.
    async fn refresh_after_error(&self) {
        let project = self.project.lock().await;
        let Ok(snapshot) = project.snapshot().await else {
            return;
        };
        drop(project);
        self.publish(snapshot);
    }

    fn publish(&self, snapshot: Snapshot) {
        *self
            .snapshot
            .write()
            .unwrap_or_else(|error| error.into_inner()) = snapshot.clone();
        if let Some(preview) = &self.preview {
            preview.set_edit_html(Some(snapshot.html));
        }
    }

    fn note_unknown(&self, request: &str, reason: &str) {
        self.backend_unknown.store(true, Ordering::Release);
        if let Err(error) = self.audit.mark_unknown(request, reason) {
            eprintln!("cayw-visual-editor: failed to persist unknown outcome: {error}");
        }
    }
}

/// Authoritative draft and history state while the caller holds the project
/// mutex. Kept as one helper so every mutation captures the same values.
async fn project_status(project: &ProjectBackend) -> (bool, bool, bool) {
    let dirty = project.dirty().await;
    let (can_undo, can_redo) = project.history_flags().await;
    (dirty, can_undo, can_redo)
}

fn editor_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/document", get(document))
        .route("/api/operation", post(operation))
        .route("/api/save", post(save))
        .route("/api/reload", post(reload))
        .route("/", get(ui_index))
        .route("/editor.js", get(ui_script))
        .route("/editor.css", get(ui_style))
        .route("/favicon.ico", get(favicon))
        .fallback(fallback)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn health() -> Response {
    json_response(StatusCode::OK, &json!({"status": "ok"}))
}

async fn document(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard(&state, &headers, &Method::GET) {
        return *response;
    }
    let _gate = state.mutation.lock().await;
    let (snapshot, dirty, can_undo, can_redo) = state.read_document().await;
    state.document_response(snapshot, dirty, can_undo, can_redo)
}

async fn operation(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = guard(&state, &headers, &Method::POST) {
        return *response;
    }
    let _gate = state.mutation.lock().await;
    if state.is_unknown() {
        return unknown_response();
    }
    if !state.available() {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "companion_unavailable",
            "the framework compiler stopped; restart the editor before editing or saving",
            false,
        );
    }
    let value = match parse_body(&body, false) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let kind = match validate_operation(&value) {
        Ok(kind) => kind,
        Err(message) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "unsupported_operation",
                message,
                false,
            );
        }
    };
    let request = state.audit.next_id();
    if let Err(error) = state.audit.started(&request, kind, &summary(&value)) {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit_failed",
            format!("could not commit the operation audit record: {error}"),
            false,
        );
    }
    let (outcome, dirty, can_undo, can_redo) = {
        let project = state.project.lock().await;
        let outcome = project.apply(value).await;
        let (dirty, can_undo, can_redo) = project_status(&project).await;
        (outcome, dirty, can_undo, can_redo)
    };
    match outcome {
        Ok(snapshot) => {
            let settlement = state.audit.settled(&request, "ok", false);
            // The in-memory draft did change. Publish it even when the audit
            // settlement is not durable so a later document read cannot pair
            // the previous HTML with the new history flags.
            state.publish(snapshot.clone());
            if let Err(error) = settlement {
                state.note_unknown(&request, &format!("audit settlement failed: {error}"));
                return api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "audit_failed",
                    format!("operation applied but its audit record was not durable: {error}"),
                    true,
                );
            }
            state.document_response(snapshot, dirty, can_undo, can_redo)
        }
        Err(error) => {
            let unknown = error.outcome_unknown;
            if let Err(audit_error) = state.audit.settled(&request, &error.code, unknown) {
                state.note_unknown(
                    &request,
                    &format!("audit settlement failed after a project error: {audit_error}"),
                );
                if !unknown {
                    state.refresh_after_error().await;
                }
                return api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "audit_failed",
                    format!("operation outcome could not be settled durably: {audit_error}"),
                    true,
                );
            }
            if unknown {
                state.note_unknown(&request, &error.message);
            } else {
                state.refresh_after_error().await;
            }
            project_error_response(error)
        }
    }
}

async fn save(State(state): State<Arc<ServerState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = guard(&state, &headers, &Method::POST) {
        return *response;
    }
    let _gate = state.mutation.lock().await;
    if state.is_unknown() {
        return unknown_response();
    }
    if !state.available() {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "companion_unavailable",
            "the framework compiler stopped; restart the editor before editing or saving",
            false,
        );
    }
    let value = match parse_body(&body, false) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let Some(expected) = value.get("expected_sha256").and_then(Value::as_str) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "expected_sha256 is required",
            false,
        );
    };
    if !valid_sha256(expected) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "expected_sha256 must be one lowercase or uppercase SHA-256 hex digest",
            false,
        );
    }
    let request = state.audit.next_id();
    if let Err(error) = state.audit.started(&request, "save", &json!({})) {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit_failed",
            format!("could not commit the save audit record: {error}"),
            false,
        );
    }
    let (outcome, dirty, can_undo, can_redo) = {
        let project = state.project.lock().await;
        let outcome = project.save(expected).await;
        let (dirty, can_undo, can_redo) = project_status(&project).await;
        (outcome, dirty, can_undo, can_redo)
    };
    match outcome {
        Ok(snapshot) => {
            let settlement = state.audit.settled(&request, "saved", false);
            // The source was written. Publish its snapshot even if the audit
            // settlement failed, otherwise the cached digest and draft would
            // lag behind the source that is already on disk.
            state.publish(snapshot.clone());
            if let Some(bridge) = &state.codeintel {
                state.publish_codeintel(bridge, &snapshot);
            }
            if let Err(error) = settlement {
                state.note_unknown(&request, &format!("save audit settlement failed: {error}"));
                return api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "audit_failed",
                    format!("source was saved but its audit record was not durable: {error}"),
                    true,
                );
            }
            state.document_response(snapshot, dirty, can_undo, can_redo)
        }
        Err(error) => {
            let unknown = error.outcome_unknown;
            if let Err(audit_error) = state.audit.settled(&request, &error.code, unknown) {
                state.note_unknown(
                    &request,
                    &format!("save audit settlement failed: {audit_error}"),
                );
                if !unknown {
                    state.refresh_after_error().await;
                }
                return api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "audit_failed",
                    format!("save outcome could not be settled durably: {audit_error}"),
                    true,
                );
            }
            if unknown {
                state.note_unknown(&request, &error.message);
            } else {
                state.refresh_after_error().await;
            }
            project_error_response(error)
        }
    }
}

async fn reload(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = guard(&state, &headers, &Method::POST) {
        return *response;
    }
    let _gate = state.mutation.lock().await;
    if state.is_unknown() {
        return unknown_response();
    }
    if !state.available() {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "companion_unavailable",
            "the framework compiler stopped; restart the editor before editing or saving",
            false,
        );
    }
    let value = match parse_body(&body, true) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    if !value.is_object() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "the reload body must be an empty JSON object",
            false,
        );
    }
    let request = state.audit.next_id();
    if let Err(error) = state.audit.started(&request, "reload", &json!({})) {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit_failed",
            format!("could not commit the reload audit record: {error}"),
            false,
        );
    }
    let (outcome, dirty, can_undo, can_redo) = {
        let project = state.project.lock().await;
        let outcome = project.reload().await;
        let (dirty, can_undo, can_redo) = project_status(&project).await;
        (outcome, dirty, can_undo, can_redo)
    };
    match outcome {
        Ok(snapshot) => {
            let settlement = state.audit.settled(&request, "reloaded", false);
            // Reload replaced the draft. Publish it even when settlement is not
            // durable so the cached snapshot cannot disagree with the source
            // that was just read.
            state.publish(snapshot.clone());
            if let Err(error) = settlement {
                state.note_unknown(
                    &request,
                    &format!("reload audit settlement failed: {error}"),
                );
                return api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "audit_failed",
                    format!("reload applied but its audit record was not durable: {error}"),
                    true,
                );
            }
            state.document_response(snapshot, dirty, can_undo, can_redo)
        }
        Err(error) => {
            let unknown = error.outcome_unknown;
            if let Err(audit_error) = state.audit.settled(&request, &error.code, unknown) {
                state.note_unknown(
                    &request,
                    &format!("reload audit settlement failed: {audit_error}"),
                );
                if !unknown {
                    state.refresh_after_error().await;
                }
                return api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "audit_failed",
                    format!("reload outcome could not be settled durably: {audit_error}"),
                    true,
                );
            }
            if unknown {
                state.note_unknown(&request, &error.message);
            } else {
                state.refresh_after_error().await;
            }
            project_error_response(error)
        }
    }
}

async fn ui_index(State(state): State<Arc<ServerState>>) -> Response {
    static_response(&state, &state.assets.index, "text/html; charset=utf-8")
}

async fn ui_script(State(state): State<Arc<ServerState>>) -> Response {
    static_response(
        &state,
        &state.assets.editor_js,
        "text/javascript; charset=utf-8",
    )
}

async fn ui_style(State(state): State<Arc<ServerState>>) -> Response {
    static_response(&state, &state.assets.editor_css, "text/css; charset=utf-8")
}

async fn favicon() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response
}

async fn fallback() -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "no such editor resource",
        false,
    )
}

fn guard(state: &ServerState, headers: &HeaderMap, method: &Method) -> Result<(), Box<Response>> {
    if !host_matches(headers, &state.editor_authority) {
        return Err(Box::new(api_error(
            StatusCode::FORBIDDEN,
            "forbidden_host",
            "request host does not match the editor origin",
            false,
        )));
    }
    let origin = headers
        .get(header::ORIGIN)
        .map(|value| value.to_str().map(|value| value.to_owned()).map_err(|_| ()))
        .transpose()
        .map_err(|_| {
            Box::new(api_error(
                StatusCode::FORBIDDEN,
                "forbidden_origin",
                "the request Origin header is not valid text",
                false,
            ))
        })?;
    match origin {
        Some(origin) if origin != state.editor_origin => {
            return Err(Box::new(api_error(
                StatusCode::FORBIDDEN,
                "forbidden_origin",
                "the request Origin is not the editor origin",
                false,
            )));
        }
        None if method != Method::GET && method != Method::HEAD => {
            return Err(Box::new(api_error(
                StatusCode::FORBIDDEN,
                "forbidden_origin",
                "state-changing requests must carry the editor Origin header",
                false,
            )));
        }
        _ => {}
    }
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| Box::new(unauthorized()))?;
    if !constant_time_eq(authorization.as_bytes(), state.token.as_bytes()) {
        return Err(Box::new(unauthorized()));
    }
    Ok(())
}

fn host_matches(headers: &HeaderMap, authority: &str) -> bool {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(authority))
}

fn unauthorized() -> Response {
    let mut response = api_error(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "a valid editor bearer token is required",
        false,
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"cayw-visual-editor\""),
    );
    response
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn parse_body(body: &Bytes, allow_empty: bool) -> Result<Value, Box<Response>> {
    if body.is_empty() {
        if allow_empty {
            return Ok(json!({}));
        }
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "a JSON request body is required",
            false,
        )));
    }
    serde_json::from_slice(body).map_err(|_| {
        Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "the request body must be one JSON value",
            false,
        ))
    })
}

const OPERATION_KINDS: &[&str] = &[
    "style",
    "text",
    "attribute",
    "move",
    "insert",
    "duplicate",
    "delete",
    "undo",
    "redo",
];

fn operation_kind(value: &Value) -> Option<&'static str> {
    let kind = value.get("kind")?.as_str()?;
    OPERATION_KINDS
        .iter()
        .copied()
        .find(|candidate| *candidate == kind)
}

/// The server is a forwarder, never a whole-file writer. Only the documented
/// operation shape is accepted; unknown top-level fields are refused so a
/// caller cannot smuggle source or markup through a recognised kind.
fn validate_operation(value: &Value) -> Result<&'static str, &'static str> {
    let object = value
        .as_object()
        .ok_or("the operation must be one JSON object")?;
    let kind = operation_kind(value).ok_or("the operation kind is not supported")?;
    let allowed: &[&str] = match kind {
        "style" => &["kind", "id", "properties"],
        "text" => &["kind", "id", "text"],
        "attribute" => &["kind", "id", "name", "value"],
        "move" => &["kind", "id", "target", "placement"],
        "insert" => &["kind", "target", "placement", "component"],
        "duplicate" | "delete" => &["kind", "id"],
        "undo" | "redo" => &["kind"],
        _ => return Err("the operation kind is not supported"),
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("the operation contains fields that are not forwarded");
    }
    match kind {
        "style" => {
            require_string(object, "id", 256)?;
            let properties = object
                .get("properties")
                .and_then(Value::as_object)
                .ok_or("style requires an object properties map")?;
            if properties.is_empty() || properties.len() > 64 {
                return Err("style property count is outside the allowed range");
            }
            for (name, value) in properties {
                if name.is_empty()
                    || name.len() > 64
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                {
                    return Err("style property names must be plain CSS property names");
                }
                let value = value
                    .as_str()
                    .ok_or("style property values must be strings")?;
                if value.len() > 4096
                    || value.contains([';', '<', '>', '{', '}', '\\'])
                    || value.chars().any(char::is_control)
                {
                    return Err("style property value is not allowed");
                }
            }
        }
        "text" => {
            require_string(object, "id", 256)?;
            // Empty text is a valid request: it clears the element's content.
            string_with_limit(object, "text", 100_000)?;
        }
        "attribute" => {
            require_string(object, "id", 256)?;
            let name = require_string(object, "name", 64)?;
            if !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                return Err("attribute name is not allowed");
            }
            // Empty values clear attributes such as alt, title or href.
            string_with_limit(object, "value", 4096)?;
        }
        "move" => {
            require_string(object, "id", 256)?;
            require_string(object, "target", 256)?;
            require_placement(object)?;
        }
        "insert" => {
            require_string(object, "target", 256)?;
            require_placement(object)?;
            require_string(object, "component", 64)?;
        }
        "duplicate" | "delete" => {
            require_string(object, "id", 256)?;
        }
        _ => {}
    }
    Ok(kind)
}

fn require_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    limit: usize,
) -> Result<&'a str, &'static str> {
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .ok_or("the operation field must be a string")?;
    if value.is_empty() || value.len() > limit {
        return Err("the operation field length is outside the allowed range");
    }
    Ok(value)
}

fn string_with_limit<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    limit: usize,
) -> Result<&'a str, &'static str> {
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .ok_or("the operation field must be a string")?;
    if value.len() > limit {
        return Err("the operation field length is outside the allowed range");
    }
    Ok(value)
}

fn require_placement(object: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    match object.get("placement").and_then(Value::as_str) {
        Some("before" | "after" | "inside") => Ok(()),
        _ => Err("placement must be before, after or inside"),
    }
}

fn summary(value: &Value) -> Value {
    json!({
        "kind": value.get("kind").and_then(Value::as_str),
        "id": value.get("id").and_then(Value::as_str),
        "target": value.get("target").and_then(Value::as_str),
    })
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn project_error_response(error: ProjectError) -> Response {
    let status = if error.outcome_unknown
        || error.code.starts_with("companion_")
        || matches!(
            error.code.as_str(),
            "parser_unavailable" | "preview_unavailable"
        ) {
        StatusCode::SERVICE_UNAVAILABLE
    } else if error.code == "stale_source" {
        StatusCode::CONFLICT
    } else if error.code == "not_found" {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    let code = if error.code.is_empty() {
        "project_error".to_owned()
    } else {
        error.code
    };
    api_error(status, &code, error.message, error.outcome_unknown)
}

fn unknown_response() -> Response {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "outcome_unknown",
        "a previous edit has an unknown outcome; resolve the state directory audit before editing again",
        true,
    )
}

fn api_error(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
    outcome_unknown: bool,
) -> Response {
    json_response(
        status,
        &json!({
            "error": {
                "code": code,
                "message": message.into(),
                "outcome_unknown": outcome_unknown,
            }
        }),
    )
}

fn json_response(status: StatusCode, value: &Value) -> Response {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| {
        br#"{"error":{"code":"serialize","message":"response serialization failed","outcome_unknown":false}}"#.to_vec()
    });
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn static_response(state: &ServerState, bytes: &[u8], content_type: &'static str) -> Response {
    let mut response = Response::new(Body::from(bytes.to_vec()));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, state.csp.clone());
    response
}

fn editor_csp(preview_origin: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; frame-src {preview_origin}; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"))
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<usize> {
    line.clear();
    let mut total_read = 0;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let to_take = pos + 1;
            let room = (max_bytes + 1).saturating_sub(line.len());
            let copy_len = to_take.min(room);
            line.extend_from_slice(&available[..copy_len]);
            reader.consume(to_take);
            total_read += to_take;
            break;
        } else {
            let to_take = available.len();
            let room = (max_bytes + 1).saturating_sub(line.len());
            let copy_len = to_take.min(room);
            line.extend_from_slice(&available[..copy_len]);
            reader.consume(to_take);
            total_read += to_take;
            if line.len() > max_bytes {
                break;
            }
        }
    }
    Ok(total_read)
}

/// Durable operation audit. A start without a settlement, an unreadable log or
/// a retained unknown marker refuses a new run. There is deliberately no
/// automatic or in-process way to clear that state.
struct StateAudit {
    dir: PathBuf,
    unknown: AtomicBool,
    counter: AtomicU64,
    write_lock: Mutex<()>,
}

impl StateAudit {
    fn open(dir: &Path) -> Result<Self, EditorError> {
        if dir.join(UNKNOWN_MARKER).exists() {
            return Err(EditorError::Unresolved(format!(
                "{} is present in the state directory",
                UNKNOWN_MARKER
            )));
        }
        let log = dir.join(EVENT_LOG);
        let file = match std::fs::File::open(&log) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(EditorError::Io(error)),
        };
        if let Some(file) = file {
            // Replay the whole log line by line. The log has no total size
            // limit; only one line is held at a time. An unresolved unknown,
            // an unpaired start or a corrupt line still refuses startup.
            let mut reader = BufReader::new(file);
            let mut pending = BTreeSet::new();
            let mut line = Vec::new();
            let mut index = 0usize;
            loop {
                let read = read_bounded_line(&mut reader, &mut line, MAX_AUDIT_LINE_BYTES)
                    .map_err(EditorError::Io)?;
                if read == 0 {
                    break;
                }
                index += 1;
                if line.len() > MAX_AUDIT_LINE_BYTES {
                    return Err(EditorError::Unresolved(format!(
                        "{EVENT_LOG} line {index} exceeds the {MAX_AUDIT_LINE_BYTES}-byte audit line limit"
                    )));
                }
                let text = std::str::from_utf8(&line).map_err(|_| {
                    EditorError::Unresolved(format!("{EVENT_LOG} line {index} is not valid UTF-8"))
                })?;
                if text.trim().is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(text).map_err(|_| {
                    EditorError::Unresolved(format!(
                        "{EVENT_LOG} line {index} is not valid audit JSON"
                    ))
                })?;
                let event = value.get("event").and_then(Value::as_str).ok_or_else(|| {
                    EditorError::Unresolved(format!("{EVENT_LOG} line {index} has no event name"))
                })?;
                let request = value
                    .get("request")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        EditorError::Unresolved(format!(
                            "{EVENT_LOG} line {index} has no request identifier"
                        ))
                    })?;
                match event {
                    "started" => {
                        pending.insert(request.to_owned());
                        if pending.len() > MAX_PENDING_AUDIT {
                            return Err(EditorError::Unresolved(format!(
                                "{EVENT_LOG} retains more than {MAX_PENDING_AUDIT} unsettled requests"
                            )));
                        }
                    }
                    "settled" => {
                        if value.get("outcome_unknown").and_then(Value::as_bool) == Some(true) {
                            return Err(EditorError::Unresolved(format!(
                                "request {request} was settled with an unknown outcome"
                            )));
                        }
                        pending.remove(request);
                    }
                    _ => {}
                }
            }
            if let Some(request) = pending.into_iter().next() {
                return Err(EditorError::Unresolved(format!(
                    "request {request} was started but never settled"
                )));
            }
        }
        Ok(Self {
            dir: dir.to_owned(),
            unknown: AtomicBool::new(false),
            counter: AtomicU64::new(0),
            write_lock: Mutex::new(()),
        })
    }

    fn unknown(&self) -> bool {
        self.unknown.load(Ordering::Acquire)
    }

    fn next_id(&self) -> String {
        format!(
            "{}-{}-{}",
            std::process::id(),
            unix_ms(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn started(&self, request: &str, kind: &str, detail: &Value) -> Result<(), EditorError> {
        self.append(&json!({
            "at_unix_ms": unix_ms(),
            "event": "started",
            "request": request,
            "kind": kind,
            "detail": detail,
        }))
    }

    fn settled(&self, request: &str, outcome: &str, unknown: bool) -> Result<(), EditorError> {
        self.append(&json!({
            "at_unix_ms": unix_ms(),
            "event": "settled",
            "request": request,
            "outcome": outcome,
            "outcome_unknown": unknown,
        }))
    }

    fn append(&self, value: &Value) -> Result<(), EditorError> {
        let mut line = serde_json::to_vec(value).map_err(|error| {
            EditorError::Io(io::Error::other(format!(
                "audit serialization failed: {error}"
            )))
        })?;
        line.push(b'\n');
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = self.dir.join(EVENT_LOG);
        let existed = path.exists();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(&line)?;
        file.sync_all()?;
        if !existed {
            // The first record creates the log. Persist the directory entry so
            // a crash cannot leave a durable file that the directory does not
            // reference.
            sync_directory(&self.dir)?;
        }
        Ok(())
    }

    fn mark_unknown(&self, request: &str, reason: &str) -> Result<(), EditorError> {
        self.unknown.store(true, Ordering::Release);
        let path = self.dir.join(UNKNOWN_MARKER);
        let payload = serde_json::to_vec(&json!({
            "at_unix_ms": unix_ms(),
            "request": request,
            "reason": reason,
        }))
        .map_err(|error| {
            EditorError::Io(io::Error::other(format!(
                "marker serialization failed: {error}"
            )))
        })?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(&payload)?;
                file.sync_all()?;
                sync_directory(&self.dir)?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(EditorError::Io(error)),
        }
        Ok(())
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u64::MAX as u128) as u64
        })
}

fn sync_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// The editor UI and bridge are compiled into the binary. There is exactly one
/// asset set: `assets/index.html`, `assets/editor.js`, `assets/editor.css` and
/// `assets/bridge.js` in this crate. A missing file is a build error, not a
/// runtime fallback.
pub struct UiAssets {
    pub index: Vec<u8>,
    pub editor_js: Vec<u8>,
    pub editor_css: Vec<u8>,
    pub bridge_js: Vec<u8>,
}

impl UiAssets {
    pub fn embedded() -> Self {
        Self {
            index: include_bytes!("../assets/index.html").to_vec(),
            editor_js: include_bytes!("../assets/editor.js").to_vec(),
            editor_css: include_bytes!("../assets/editor.css").to_vec(),
            bridge_js: include_bytes!("../assets/bridge.js").to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validation_is_strict() {
        assert!(valid_token("0123456789abcdef0123456789abcdef"));
        assert!(!valid_token("short"));
        assert!(!valid_token("0123456789abcdef0123456789abcde "));
        assert!(!valid_token("0123456789abcdef0123456789abcdeé"));
    }

    #[test]
    fn operation_allowlist_rejects_whole_file_writes() {
        assert_eq!(operation_kind(&json!({"kind": "style"})), Some("style"));
        assert_eq!(operation_kind(&json!({"kind": "undo"})), Some("undo"));
        assert_eq!(
            operation_kind(&json!({"kind": "replace", "html": "<p>x"})),
            None
        );
    }

    #[test]
    fn sha256_shape_is_checked() {
        assert!(valid_sha256(&"a".repeat(64)));
        assert!(!valid_sha256(&"a".repeat(63)));
        assert!(!valid_sha256(&"z".repeat(64)));
    }

    #[test]
    fn state_audit_refuses_unsettled_and_unknown() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(EVENT_LOG),
            "{\"event\":\"started\",\"request\":\"r1\"}\n",
        )
        .unwrap();
        assert!(matches!(
            StateAudit::open(directory.path()),
            Err(EditorError::Unresolved(_))
        ));
        std::fs::write(
            directory.path().join(EVENT_LOG),
            "{\"event\":\"started\",\"request\":\"r1\"}\n{\"event\":\"settled\",\"request\":\"r1\"}\n",
        )
        .unwrap();
        assert!(StateAudit::open(directory.path()).is_ok());
        std::fs::write(directory.path().join(UNKNOWN_MARKER), "{}").unwrap();
        assert!(matches!(
            StateAudit::open(directory.path()),
            Err(EditorError::Unresolved(_))
        ));
    }

    #[test]
    fn bridge_injection_ignores_mentions_in_text_and_comments() {
        let source = "<html><body><!-- /__cayw_bridge.js --><p>/__cayw_bridge.js</p></body></html>";
        let injected = preview::inject_bridge(source, "http://127.0.0.1:1");
        assert!(injected.starts_with("<script src=\"/__cayw_bridge.js?"));
        assert!(injected.ends_with(source));
        assert_eq!(
            injected.matches("<script src=\"/__cayw_bridge.js").count(),
            1
        );
        assert!(injected.contains("editor_origin=http%3A%2F%2F127.0.0.1%3A1"));
    }

    #[test]
    fn bridge_follows_the_head_opener_and_precedes_project_scripts() {
        let source = "<!doctype html>\n<html><head><script src=\"/first.js\"></script></head><body><script src=\"/second.js\"></script></body></html>";
        let injected = preview::inject_bridge(source, "http://127.0.0.1:1");
        assert!(injected.starts_with("<!doctype html>"));
        assert_eq!(injected.matches("<!doctype").count(), 1);
        let head = injected.find("<head>").expect("head opener");
        let bridge = injected
            .find("<script src=\"/__cayw_bridge.js?")
            .expect("bridge script");
        let first = injected
            .find("<script src=\"/first.js\"")
            .expect("first project script");
        assert!(head < bridge, "bridge must follow the original head opener");
        assert!(
            bridge < first,
            "bridge must run before the first project script"
        );
        assert_eq!(
            injected.replace("<script src=\"/__cayw_bridge.js?editor_origin=http%3A%2F%2F127.0.0.1%3A1\"></script>\n", ""),
            source
        );
    }

    #[test]
    fn bridge_keeps_head_attributes_and_id() {
        let source = "<html><head id=\"app-head\" data-theme=\"dark\"><script src=\"/first.js\"></script></head><body></body></html>";
        let injected = preview::inject_bridge(source, "http://127.0.0.1:1");
        assert!(injected.contains("<head id=\"app-head\" data-theme=\"dark\">"));
        let head = injected
            .find("<head id=\"app-head\" data-theme=\"dark\">")
            .expect("head opener");
        let bridge = injected
            .find("<script src=\"/__cayw_bridge.js?")
            .expect("bridge script");
        let first = injected
            .find("<script src=\"/first.js\"")
            .expect("first project script");
        assert!(head < bridge && bridge < first);
    }

    #[test]
    fn bridge_is_inserted_after_the_head_opener_without_a_script() {
        let source = "<!doctype html><html><head id=\"only-head\"></head><body></body></html>";
        let injected = preview::inject_bridge(source, "http://127.0.0.1:1");
        let head = injected
            .find("<head id=\"only-head\">")
            .expect("head opener");
        let bridge = injected
            .find("<script src=\"/__cayw_bridge.js?")
            .expect("bridge");
        assert!(head < bridge);
        assert!(bridge < injected.find("</head>").expect("head close"));
        assert!(injected.contains("<body></body>"));
    }

    #[test]
    fn bridge_precedes_the_first_script_when_there_is_no_head() {
        let source = "<!doctype html><html><body><script src=\"/app.js\"></script></body></html>";
        let injected = preview::inject_bridge(source, "http://127.0.0.1:1");
        assert!(injected.starts_with("<!doctype html>"));
        let bridge = injected
            .find("<script src=\"/__cayw_bridge.js?")
            .expect("bridge");
        let app = injected.find("<script src=\"/app.js\"").expect("app");
        assert!(bridge < app);
        assert!(injected.ends_with("<script src=\"/app.js\"></script></body></html>"));
    }

    #[test]
    fn bridge_precedes_an_early_script_even_when_head_follows() {
        let source = "<html><script src=\"/early.js\"></script><head id=\"late\"></head></html>";
        let injected = preview::inject_bridge(source, "http://127.0.0.1:1");
        let bridge = injected
            .find("<script src=\"/__cayw_bridge.js?")
            .expect("bridge");
        let early = injected
            .find("<script src=\"/early.js\"")
            .expect("early script");
        assert!(bridge < early);
        assert!(injected.contains("<head id=\"late\"></head>"));
    }

    #[test]
    fn bridge_follows_a_doctype_when_there_is_no_head_or_script() {
        let source = "<!doctype html><html><body><p>text</p></body></html>";
        let injected = preview::inject_bridge(source, "http://127.0.0.1:1");
        assert!(injected.starts_with("<!doctype html>"));
        let doctype = injected.find("<!doctype html>").expect("doctype");
        let bridge = injected
            .find("<script src=\"/__cayw_bridge.js?")
            .expect("bridge");
        assert!(doctype < bridge);
        assert_eq!(
            injected.strip_prefix("<!doctype html>").unwrap().strip_prefix("<script src=\"/__cayw_bridge.js?editor_origin=http%3A%2F%2F127.0.0.1%3A1\"></script>\n").unwrap(),
            &source["<!doctype html>".len()..]
        );
    }

    #[test]
    fn leading_comments_do_not_make_a_doctype_non_canonical() {
        let source =
            "\n<!-- note -->\n<!doctype html><html><script src=\"/app.js\"></script></html>";
        let injected = preview::inject_bridge(source, "http://127.0.0.1:1");
        let doctype = injected.find("<!doctype html>").expect("doctype");
        let bridge = injected
            .find("<script src=\"/__cayw_bridge.js?")
            .expect("bridge");
        let app = injected.find("<script src=\"/app.js\"").expect("app");
        assert!(doctype < bridge && bridge < app);
    }

    #[test]
    fn settled_unknown_outcome_is_refused_without_a_marker() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(EVENT_LOG),
            "{\"event\":\"started\",\"request\":\"r1\"}\n{\"event\":\"settled\",\"request\":\"r1\",\"outcome_unknown\":true}\n",
        )
        .unwrap();
        assert!(!directory.path().join(UNKNOWN_MARKER).exists());
        assert!(matches!(
            StateAudit::open(directory.path()),
            Err(EditorError::Unresolved(_))
        ));
    }

    #[test]
    fn long_complete_audit_logs_are_replayed_without_a_total_size_limit() {
        let directory = tempfile::tempdir().unwrap();
        {
            let file = std::fs::File::create(directory.path().join(EVENT_LOG)).unwrap();
            let mut writer = std::io::BufWriter::new(file);
            let mut written = 0usize;
            let mut index = 0u64;
            while written < 9 * 1024 * 1024 {
                let line = format!(
                    "{{\"event\":\"started\",\"request\":\"r{index}\"}}\n{{\"event\":\"settled\",\"request\":\"r{index}\",\"outcome_unknown\":false}}\n"
                );
                std::io::Write::write_all(&mut writer, line.as_bytes()).unwrap();
                written += line.len();
                index += 1;
            }
            std::io::Write::flush(&mut writer).unwrap();
        }
        assert!(StateAudit::open(directory.path()).is_ok());
    }

    #[test]
    fn dropping_the_handle_cancels_both_services() {
        let cancel = CancellationToken::new();
        let handle = EditorHandle {
            editor_url: "http://127.0.0.1:1/".to_owned(),
            preview_url: "http://127.0.0.1:2/index.html".to_owned(),
            editor_origin: "http://127.0.0.1:1".to_owned(),
            preview_origin: "http://127.0.0.1:2".to_owned(),
            editor_addr: "127.0.0.1:1".parse().unwrap(),
            preview_addr: "127.0.0.1:2".parse().unwrap(),
            normal_preview_url: "http://127.0.0.1:2/index.html".to_owned(),
            normal_preview_origin: "http://127.0.0.1:2".to_owned(),
            cancel: cancel.clone(),
            tasks: Vec::new(),
            companion: None,
        };
        drop(handle);
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn default_ports_are_omitted_from_origin_and_authority() {
        let http: SocketAddr = "127.0.0.1:80".parse().unwrap();
        assert_eq!(authority_for(http), "127.0.0.1");
        assert_eq!(origin_for(http), "http://127.0.0.1");
        let ipv6: SocketAddr = "[::1]:80".parse().unwrap();
        assert_eq!(authority_for(ipv6), "[::1]");
        assert_eq!(origin_for(ipv6), "http://[::1]");
        let other: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(authority_for(other), "127.0.0.1:8080");
        assert_eq!(origin_for(other), "http://127.0.0.1:8080");
    }
}
