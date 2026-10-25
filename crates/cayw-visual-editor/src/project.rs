//! Source mapping and deterministic edits for one existing HTML page.
//!
//! The project engine keeps the original page bytes and an in-memory draft. It
//! never writes source until [`Project::save`], and that write goes through the
//! public `cayw-host` SDK with an exact page permission gate and a
//! compare-and-swap digest. Edits are expressed as structured operations; the
//! whole-file content is never accepted from the caller.
//!
//! The HTML tokenizer used here is `html5gum`. Byte spans come from the
//! `usize`-span emitter, so mapping and injection use exact source positions
//! rather than a regular-expression approximation.

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{Host, HostConfig, PermissionGate};
use html5gum::{DefaultEmitter, Token, Tokenizer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

use crate::audit::{AuditOpenError, DurableAudit};

/// Attribute injected into the in-memory response HTML for each mapped element.
pub const NODE_ATTRIBUTE: &str = "data-cayw-node";

/// Response-only marker for a mapped element the engine will not edit.
pub const READONLY_ATTRIBUTE: &str = "data-cayw-readonly";

/// Response-only explanation that accompanies [`READONLY_ATTRIBUTE`].
pub const READONLY_REASON_ATTRIBUTE: &str = "data-cayw-reason";

/// Stable public component names accepted by the `insert` operation.
pub const COMPONENT_NAMES: &[&str] = &[
    "text",
    "image",
    "button",
    "input",
    "card",
    "container",
    "grid",
    "hero",
    "bento",
    "faq",
    "cta",
];

const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 20_000;
const MAX_HISTORY: usize = 100;
const MAX_HISTORY_BYTES: usize = 32 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_ATTRIBUTE_VALUE_BYTES: usize = 64 * 1024;

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "image",
];

/// Tags that are never a direct edit target. Their source boundaries may exist,
/// but changing them would alter document loading or executable content.
const NON_EDITABLE_TAGS: &[&str] = &[
    "html",
    "head",
    "title",
    "base",
    "meta",
    "link",
    "script",
    "style",
    "template",
    "noscript",
    "slot",
    "iframe",
    "frame",
    "object",
    "embed",
    "canvas",
    "video",
    "audio",
    "source",
    "track",
    "map",
    "area",
    "param",
    "col",
    "colgroup",
    "datalist",
    "xmp",
    "noembed",
    "noframes",
    "plaintext",
];

/// Nodes that never receive [`NODE_ATTRIBUTE`]. Injecting into these would be
/// useless or would modify non-visual/executable content.
const NON_INJECT_TAGS: &[&str] = &[
    "html",
    "head",
    "title",
    "base",
    "meta",
    "link",
    "script",
    "style",
    "template",
    "noscript",
    "iframe",
    "xmp",
    "noembed",
    "noframes",
    "plaintext",
];

/// Elements whose content is raw text or RCDATA. Their descendants are never
/// editable even if the tokenizer recovers by emitting element tokens.
const RAW_TEXT_TAGS: &[&str] = &[
    "script",
    "style",
    "title",
    "textarea",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "noframe",
    "noscript",
    "plaintext",
];

/// Containers that can receive inserted or moved block content.
const CONTAINER_TAGS: &[&str] = &[
    "body",
    "main",
    "header",
    "footer",
    "nav",
    "section",
    "article",
    "aside",
    "div",
    "figure",
    "figcaption",
    "form",
    "fieldset",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
    "details",
    "blockquote",
    "pre",
    "address",
];

/// Whole-page structure that may be styled but not moved or removed.
const PROTECTED_STRUCTURE: &[&str] = &["html", "head", "body", "main"];

/// Table containers whose direct children follow a strict content model.
const TABLE_STRUCTURE_TAGS: &[&str] = &["table", "thead", "tbody", "tfoot", "tr"];

/// A failure returned by the source engine.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectError {
    pub code: String,
    pub message: String,
    pub outcome_unknown: bool,
}

impl ProjectError {
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            outcome_unknown: false,
        }
    }

    pub(crate) fn unknown(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            outcome_unknown: true,
        }
    }
}

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProjectError {}

/// One element that has an exact source position.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceNode {
    pub id: String,
    /// Project-relative source file and UTF-8 byte position of the element.
    pub file: String,
    pub start: usize,
    pub tag: String,
    pub parent: Option<String>,
    pub line: usize,
    pub editable: bool,
    pub reason: Option<String>,
}

/// A source file's saved digest and current uninstrumented draft.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceFile {
    pub path: String,
    pub sha256: String,
    pub source: String,
}

/// The current injected draft and its source revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub path: String,
    pub sha256: String,
    pub adapter: String,
    pub revision: String,
    pub files: Vec<SourceFile>,
    /// Response HTML with the mapping attributes injected for the editor.
    pub html: String,
    /// The uninstrumented draft source. Read-only viewers must render this
    /// value so response-only mapping attributes never appear as page content.
    pub source: String,
    pub nodes: Vec<SourceNode>,
    /// Source-mapped element the editor should keep selected after this
    /// snapshot. The engine resolves it from exact source positions; callers
    /// never need to guess by tag, line or insertion order.
    #[serde(default)]
    pub selected_id: Option<String>,
}

/// One open page and its in-memory draft.
pub struct Project {
    page: String,
    root: PathBuf,
    host: Host,
    _audit: Arc<DurableAudit>,
    state: Mutex<State>,
}

struct State {
    /// Digest of the file on disk at open or after the last successful save.
    original_sha256: String,
    /// Source bytes that correspond to `original_sha256`.
    saved: String,
    /// Current draft. May differ from `saved`; never written before `save`.
    draft: String,
    undo: Vec<String>,
    redo: Vec<String>,
    /// Byte offset of the selected element's opening `<` in `draft`.
    selection: Option<usize>,
    unknown: bool,
    /// A failed compare-and-swap proved that the source changed on disk. The
    /// draft now belongs to an older revision, so edits and saves stay locked
    /// until [`Project::reload`] replaces it with the current source.
    conflicted: bool,
}

impl fmt::Debug for Project {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Project")
            .field("page", &self.page)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Project {
    /// Opens an existing page with a permission scope limited to that exact file.
    ///
    /// `state_dir` must be outside the project tree. It holds the durable call
    /// ledger. A retained unknown outcome refuses the open.
    pub async fn open(root: &Path, page: &str, state_dir: &Path) -> Result<Self, ProjectError> {
        let page = normalize_page(page)?;
        let root = root.canonicalize().map_err(|error| {
            ProjectError::new("invalid_project", format!("project root: {error}"))
        })?;
        if !root.is_dir() {
            return Err(ProjectError::new(
                "invalid_project",
                "project root must be a directory",
            ));
        }
        // Resolve the path without creating anything. The nearest existing
        // ancestor is canonicalized, so a symlink alias into the project is
        // rejected before a single directory is created inside the project.
        let state_dir = resolve_state_dir(state_dir)?;
        if state_dir.starts_with(&root) || root.starts_with(&state_dir) {
            return Err(ProjectError::new(
                "state_inside_project",
                "the state directory must be outside the project tree",
            ));
        }
        std::fs::create_dir_all(&state_dir).map_err(|error| {
            ProjectError::new(
                "state_dir_unavailable",
                format!("could not create the state directory: {error}"),
            )
        })?;
        let state_dir = state_dir.canonicalize().map_err(|error| {
            ProjectError::new(
                "state_dir_unavailable",
                format!("could not resolve the state directory: {error}"),
            )
        })?;
        if state_dir.starts_with(&root) || root.starts_with(&state_dir) {
            return Err(ProjectError::new(
                "state_inside_project",
                "the state directory must be outside the project tree",
            ));
        }
        let audit = Arc::new(DurableAudit::open(&state_dir).map_err(audit_error)?);
        let gate = Arc::new(PageGate { page: page.clone() });
        let host = Host::new(&root, gate, audit.clone(), host_config()).map_err(|error| {
            ProjectError::new(
                "host_unavailable",
                format!("could not open the host: {error}"),
            )
        })?;
        let (source, sha256) = read_page(&host, &page).await?;
        if source.len() > MAX_PAGE_BYTES {
            return Err(ProjectError::new(
                "page_too_large",
                format!("page exceeds the {MAX_PAGE_BYTES}-byte editor limit"),
            ));
        }
        parse_document(&source)?;
        Ok(Self {
            page,
            root,
            host,
            _audit: audit,
            state: Mutex::new(State {
                original_sha256: sha256,
                saved: source.clone(),
                draft: source,
                undo: Vec::new(),
                redo: Vec::new(),
                selection: None,
                unknown: false,
                conflicted: false,
            }),
        })
    }

    /// Current injected draft with fresh source mapping.
    pub async fn snapshot(&self) -> Result<Snapshot, ProjectError> {
        let state = self.state.lock().await;
        build_snapshot(
            &self.page,
            &state.original_sha256,
            &state.draft,
            state.selection,
        )
    }

    /// Applies one structured operation and reparses the draft.
    pub async fn apply(&self, operation: Value) -> Result<Snapshot, ProjectError> {
        let mut state = self.state.lock().await;
        if state.unknown {
            return Err(reconciliation("a previous edit has an unknown outcome"));
        }
        if state.conflicted {
            return Err(stale_source());
        }
        let kind = operation
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| ProjectError::new("invalid_operation", "operation kind is required"))?;
        let selection = match kind {
            "undo" => {
                let previous = state.undo.pop().ok_or_else(|| {
                    ProjectError::new("history_empty", "there is no operation to undo")
                })?;
                let current = state.draft.clone();
                push_history(&mut state.redo, &current);
                state.draft = previous;
                None
            }
            "redo" => {
                let next = state.redo.pop().ok_or_else(|| {
                    ProjectError::new("history_empty", "there is no operation to redo")
                })?;
                let current = state.draft.clone();
                push_history(&mut state.undo, &current);
                state.draft = next;
                None
            }
            _ => {
                let document = parse_document(&state.draft)?;
                let result = apply_operation(&document, &state.draft, &operation, kind)?;
                // Reparse before accepting so a splice can never commit a broken
                // mapping or an unterminated script/style region.
                let next_document = parse_document(&result.source)?;
                let selection = resolve_selection(&next_document, result.selection);
                if result.source != state.draft {
                    let previous = std::mem::replace(&mut state.draft, result.source);
                    push_history(&mut state.undo, &previous);
                    state.redo.clear();
                }
                selection
            }
        };
        state.selection = selection;
        build_snapshot(
            &self.page,
            &state.original_sha256,
            &state.draft,
            state.selection,
        )
    }

    /// Writes the draft to the project source with an exact compare-and-swap.
    pub async fn save(&self, expected_sha256: &str) -> Result<Snapshot, ProjectError> {
        if !valid_sha256(expected_sha256) {
            return Err(ProjectError::new(
                "invalid_value",
                "expected_sha256 must be one SHA-256 hex digest",
            ));
        }
        let mut state = self.state.lock().await;
        if state.unknown {
            return Err(reconciliation("a previous save has an unknown outcome"));
        }
        if state.conflicted {
            return Err(stale_source());
        }
        if !expected_sha256.eq_ignore_ascii_case(&state.original_sha256) {
            return Err(stale_source());
        }
        if self.host.outcome_unknown() {
            state.unknown = true;
            return Err(reconciliation(
                "the host requires reconciliation before another write",
            ));
        }
        let draft = state.draft.clone();
        let local_sha256 = sha256_hex(draft.as_bytes());
        let result = self
            .host
            .call(
                "fs.write",
                json!({
                    "path": self.page,
                    "content": draft,
                    "overwrite": true,
                    "expected_sha256": expected_sha256,
                }),
                CancellationToken::new(),
            )
            .await;
        match result {
            Ok(value) => {
                let written = value.get("sha256").and_then(Value::as_str).ok_or_else(|| {
                    state.unknown = true;
                    ProjectError::unknown(
                        "save_outcome_unknown",
                        "the host did not return a source digest",
                    )
                })?;
                if !written.eq_ignore_ascii_case(&local_sha256) {
                    state.unknown = true;
                    return Err(ProjectError::unknown(
                        "save_outcome_unknown",
                        "the saved digest does not match the draft; verify the source",
                    ));
                }
                state.original_sha256 = written.to_ascii_lowercase();
                state.saved = state.draft.clone();
            }
            Err(error) if !error.outcome_unknown && is_stale_error(&error.message) => {
                // The draft belongs to an older revision. Refresh the reported
                // revision for diagnostics, but lock the project: a client that
                // reads the fresh digest from the snapshot must still fail to
                // save this stale draft until reload replaces it.
                state.conflicted = true;
                match read_page(&self.host, &self.page).await {
                    Ok((content, sha256)) => {
                        state.original_sha256 = sha256;
                        state.saved = content;
                    }
                    Err(read_error) if read_error.outcome_unknown => {
                        state.unknown = true;
                        return Err(read_error);
                    }
                    Err(_) => {}
                }
                return Err(stale_source());
            }
            Err(error) if error.outcome_unknown => {
                state.unknown = true;
                return Err(ProjectError::unknown("save_outcome_unknown", error.message));
            }
            Err(error) => {
                return Err(ProjectError::new("save_failed", error.message));
            }
        }
        match build_snapshot(
            &self.page,
            &state.original_sha256,
            &state.draft,
            state.selection,
        ) {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                state.unknown = true;
                Err(ProjectError::unknown(
                    "save_outcome_unknown",
                    format!(
                        "the source was written but its draft could not be mapped: {}",
                        error.message
                    ),
                ))
            }
        }
    }

    /// Discards the draft and reads the current source again.
    pub async fn reload(&self) -> Result<Snapshot, ProjectError> {
        let mut state = self.state.lock().await;
        if state.unknown {
            return Err(reconciliation("a previous write has an unknown outcome"));
        }
        if self.host.outcome_unknown() {
            state.unknown = true;
            return Err(reconciliation(
                "the host requires reconciliation before another read",
            ));
        }
        let (source, sha256) = match read_page(&self.host, &self.page).await {
            Ok(page) => page,
            Err(error) => {
                if error.outcome_unknown {
                    state.unknown = true;
                }
                return Err(error);
            }
        };
        // Build the full response before changing any project state: a source
        // that cannot be mapped must leave the old draft and selection intact.
        let snapshot = build_snapshot(&self.page, &sha256, &source, None)?;
        state.conflicted = false;
        state.original_sha256 = sha256;
        state.saved = source.clone();
        state.draft = source;
        state.undo.clear();
        state.redo.clear();
        state.selection = None;
        Ok(snapshot)
    }

    /// True when the draft differs from the last opened or saved source.
    pub async fn dirty(&self) -> bool {
        let state = self.state.lock().await;
        state.draft != state.saved
    }

    /// `(can_undo, can_redo)` for callers that keep no own counters.
    pub async fn history_flags(&self) -> (bool, bool) {
        let state = self.state.lock().await;
        (!state.undo.is_empty(), !state.redo.is_empty())
    }

    /// Page path relative to the project root that this project may access.
    pub fn page(&self) -> &str {
        &self.page
    }

    /// Project root, canonicalized when the project was opened.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

struct PageGate {
    page: String,
}

#[async_trait]
impl PermissionGate for PageGate {
    async fn authorize(
        &self,
        capability: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        if cancel.is_cancelled() {
            return Err(CapabilityError::new("permission check was cancelled"));
        }
        let path = arguments.get("path").and_then(Value::as_str);
        if path != Some(self.page.as_str()) {
            return Err(CapabilityError::new(
                "the visual editor may only access its configured page",
            ));
        }
        match capability {
            "fs.read" => {
                if arguments.get("content").is_some() || arguments.get("overwrite").is_some() {
                    return Err(CapabilityError::new(
                        "read arguments contain write-only fields",
                    ));
                }
                Ok(())
            }
            "fs.write" => {
                if arguments.get("overwrite").and_then(Value::as_bool) != Some(true) {
                    return Err(CapabilityError::new(
                        "the visual editor rewrites require overwrite=true",
                    ));
                }
                if arguments
                    .get("expected_sha256")
                    .and_then(Value::as_str)
                    .is_none()
                {
                    return Err(CapabilityError::new(
                        "the visual editor rewrites require expected_sha256",
                    ));
                }
                Ok(())
            }
            _ => Err(CapabilityError::new(
                "the visual editor has only page read and write access",
            )),
        }
    }
}

fn host_config() -> HostConfig {
    HostConfig {
        max_argument_bytes: 32 * 1024 * 1024,
        max_result_bytes: 32 * 1024 * 1024,
        max_total_bytes: 128 * 1024 * 1024,
        max_calls: 4096,
        max_file_bytes: MAX_PAGE_BYTES,
        max_list_entries: 1,
        call_timeout: Duration::from_secs(60),
        data: None,
        workspace: None,
        ..HostConfig::default()
    }
}

async fn read_page(host: &Host, page: &str) -> Result<(String, String), ProjectError> {
    if host.outcome_unknown() {
        return Err(reconciliation(
            "the host requires reconciliation before another read",
        ));
    }
    let value = host
        .call("fs.read", json!({ "path": page }), CancellationToken::new())
        .await
        .map_err(|error| host_read_error(&error))?;
    let content = value
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| ProjectError::new("read_failed", "the host did not return page content"))?;
    let reported = value.get("sha256").and_then(Value::as_str).ok_or_else(|| {
        ProjectError::new("read_failed", "the host did not return a source digest")
    })?;
    let actual = sha256_hex(content.as_bytes());
    if !reported.eq_ignore_ascii_case(&actual) {
        return Err(ProjectError::new(
            "read_failed",
            "the host source digest does not match the returned content",
        ));
    }
    Ok((content.to_owned(), actual))
}

fn host_read_error(error: &CapabilityError) -> ProjectError {
    if error.outcome_unknown {
        ProjectError::unknown("read_outcome_unknown", error.message.clone())
    } else {
        ProjectError::new("read_failed", error.message.clone())
    }
}

fn audit_error(error: AuditOpenError) -> ProjectError {
    if error.outcome_unknown {
        ProjectError::unknown(error.code, error.message)
    } else {
        ProjectError::new(error.code, error.message)
    }
}

fn reconciliation(message: impl Into<String>) -> ProjectError {
    ProjectError::unknown("reconciliation_required", message)
}

fn stale_source() -> ProjectError {
    ProjectError::new(
        "stale_source",
        "the project source changed outside the editor; reload before editing or saving",
    )
}

fn is_stale_error(message: &str) -> bool {
    message.contains("compare-and-swap conflict") || message.contains("destination is absent")
}

fn build_snapshot(
    page: &str,
    sha256: &str,
    draft: &str,
    selection: Option<usize>,
) -> Result<Snapshot, ProjectError> {
    let document = parse_document(draft)?;
    let selected_id = selection.and_then(|offset| {
        document
            .nodes
            .iter()
            .find(|node| node.editable && node.start_tag_span.start == offset)
            .map(|node| node.id.clone())
    });
    let html = instrument(draft, &document)?;
    let nodes = document
        .nodes
        .iter()
        .map(|node| SourceNode {
            id: node.id.clone(),
            file: page.to_owned(),
            start: node.start_tag_span.start,
            tag: node.tag.clone(),
            parent: node.parent.map(|parent| document.nodes[parent].id.clone()),
            line: node.line,
            editable: node.editable,
            reason: node.reason.clone(),
        })
        .collect();
    Ok(Snapshot {
        path: page.to_owned(),
        sha256: sha256.to_owned(),
        adapter: "html".to_owned(),
        revision: sha256_hex(draft.as_bytes()),
        files: vec![SourceFile {
            path: page.to_owned(),
            sha256: sha256.to_owned(),
            source: draft.to_owned(),
        }],
        html,
        source: draft.to_owned(),
        nodes,
        selected_id,
    })
}

fn push_history(history: &mut Vec<String>, value: &str) {
    if history.last().is_some_and(|last| last == value) {
        return;
    }
    history.push(value.to_owned());
    while history.len() > MAX_HISTORY {
        history.remove(0);
    }
    let mut bytes: usize = history.iter().map(String::len).sum();
    while bytes > MAX_HISTORY_BYTES && history.len() > 1 {
        bytes = bytes.saturating_sub(history.remove(0).len());
    }
}

fn normalize_page(page: &str) -> Result<String, ProjectError> {
    let page = page.strip_prefix('/').unwrap_or(page);
    let page = page.strip_prefix("./").unwrap_or(page);
    if page.is_empty() || page.len() > 512 || page.chars().any(char::is_control) {
        return Err(ProjectError::new(
            "invalid_page",
            "page must be a relative HTML path",
        ));
    }
    let components: Vec<&str> = page.split('/').collect();
    if components.is_empty() || components.len() > 16 {
        return Err(ProjectError::new(
            "invalid_page",
            "page path has too many components",
        ));
    }
    for component in &components {
        if component.is_empty()
            || *component == "."
            || *component == ".."
            || component.len() > 255
            || component.starts_with('.')
            || component.contains(['\\', ':', '\0'])
        {
            return Err(ProjectError::new(
                "invalid_page",
                "page must be a portable relative HTML path",
            ));
        }
    }
    let last = components.last().expect("nonempty components");
    let extension = last
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    if !matches!(extension.as_deref(), Some("html" | "htm")) {
        return Err(ProjectError::new(
            "invalid_page",
            "the visual editor only edits HTML pages",
        ));
    }
    Ok(page.to_owned())
}

/// Resolves a possibly missing state path against its nearest existing
/// ancestor. This performs no filesystem writes, so the caller can reject a
/// path inside the project before creating it. Lexical `..` components in the
/// missing suffix are normalized after the existing ancestor is canonicalized.
fn resolve_state_dir(path: &Path) -> Result<PathBuf, ProjectError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                ProjectError::new(
                    "state_dir_unavailable",
                    format!("could not resolve the current directory: {error}"),
                )
            })?
            .join(path)
    };
    let mut current = path.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match current.canonicalize() {
            Ok(mut resolved) => {
                for part in tail.iter().rev() {
                    if part.as_os_str() == std::ffi::OsStr::new("..") {
                        resolved.pop();
                    } else if part.as_os_str() != std::ffi::OsStr::new(".") {
                        resolved.push(part);
                    }
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(component) = current.components().next_back() else {
                    break;
                };
                match component {
                    std::path::Component::Normal(name) => tail.push(name.to_owned()),
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => {
                        tail.push(std::ffi::OsString::from(".."));
                    }
                    std::path::Component::RootDir | std::path::Component::Prefix(_) => break,
                }
                let Some(parent) = current.parent() else {
                    break;
                };
                if parent == current {
                    break;
                }
                current = parent;
            }
            Err(error) => {
                return Err(ProjectError::new(
                    "state_dir_unavailable",
                    format!("could not resolve the state directory: {error}"),
                ));
            }
        }
    }
    Err(ProjectError::new(
        "state_dir_unavailable",
        "could not resolve the state directory",
    ))
}

// ---------------------------------------------------------------------------
// Tokenizer mapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Element {
    id: String,
    tag: String,
    parent: Option<usize>,
    line: usize,
    editable: bool,
    reason: Option<String>,
    inject: bool,
    start_tag_span: Range<usize>,
    start_tag_end: usize,
    start_tag_insert: Option<usize>,
    end_tag_start: Option<usize>,
    structural_end: Option<usize>,
    void: bool,
    foreign: bool,
    /// This element opened a new SVG/MathML foreign-content region.
    foreign_root: bool,
    dynamic: bool,
    auto_closed: bool,
    unclosed: bool,
    stray_end: bool,
    invalid_self_close: bool,
    has_child_element: bool,
    has_comment: bool,
    attribute_values: BTreeMap<String, String>,
    existing_node_attribute: bool,
    span_ok: bool,
}

impl Element {
    fn source_range(&self) -> Option<Range<usize>> {
        if self.void
            || (self.foreign && self.structural_end.is_some() && self.end_tag_start.is_none())
        {
            return Some(self.start_tag_span.start..self.start_tag_end);
        }
        self.structural_end
            .map(|end| self.start_tag_span.start..end)
    }
}

#[derive(Debug)]
struct Document {
    nodes: Vec<Element>,
    index: BTreeMap<String, usize>,
}

impl Document {
    fn node(&self, id: &str) -> Result<&Element, ProjectError> {
        self.index
            .get(id)
            .map(|index| &self.nodes[*index])
            .ok_or_else(|| ProjectError::new("not_found", format!("no mapped element has id {id}")))
    }

    fn index_of(&self, id: &str) -> Result<usize, ProjectError> {
        self.index
            .get(id)
            .copied()
            .ok_or_else(|| ProjectError::new("not_found", format!("no mapped element has id {id}")))
    }

    fn is_descendant(&self, mut index: usize, ancestor: usize) -> bool {
        while let Some(parent) = self.nodes[index].parent {
            if parent == ancestor {
                return true;
            }
            index = parent;
        }
        false
    }
}

fn parse_document(source: &str) -> Result<Document, ProjectError> {
    let line_starts = line_starts(source);
    // Bind every mapping id to the complete source revision. A structural edit
    // changes the digest, so an id issued for an older draft can never be
    // reused for a different element that happens to keep the same position.
    let document_hash = sha256_hex(source.as_bytes());
    let mut nodes: Vec<Element> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut foreign_depth = 0usize;
    let mut errors: Vec<Range<usize>> = Vec::new();
    // `html5gum` exposes script/style raw-text switching as an explicit emitter
    // option. Without it, markup-shaped script text would be parsed as elements.
    let mut emitter = DefaultEmitter::<usize>::new_with_span();
    emitter.naively_switch_states(true);
    let tokenizer = Tokenizer::new_with_emitter(source, emitter);
    for item in tokenizer {
        let token = match item {
            Ok(token) => token,
            Err(_) => {
                errors.push(0..source.len());
                break;
            }
        };
        match token {
            Token::StartTag(tag) => {
                if nodes.len() >= MAX_NODES {
                    return Err(ProjectError::new(
                        "page_too_complex",
                        format!("page exceeds the {MAX_NODES}-element editor limit"),
                    ));
                }
                let name = String::from_utf8_lossy(&tag.name).to_ascii_lowercase();
                let span = tag.span.start..tag.span.end;
                let span_ok = span.start < span.end
                    && span.end <= source.len()
                    && source.as_bytes().get(span.start) == Some(&b'<')
                    && source.as_bytes().get(span.end - 1) == Some(&b'>');
                let insert = if span_ok {
                    let mut at = span.end - 1;
                    // Only a tokenizer-recognized self-closing slash belongs to
                    // the tag syntax. A trailing slash inside an unquoted
                    // attribute value such as `href=/docs/` is data and must
                    // stay in the value.
                    if tag.self_closing && at > span.start + 1 && source.as_bytes()[at - 1] == b'/'
                    {
                        at -= 1;
                    }
                    Some(at)
                } else {
                    None
                };
                let mut attributes = BTreeMap::new();
                for (attribute_name, value) in &tag.attributes {
                    let name = String::from_utf8_lossy(attribute_name).to_ascii_lowercase();
                    attributes.insert(name, String::from_utf8_lossy(&value.value).into_owned());
                }
                close_implied(&mut stack, &mut nodes, &name, span.start);
                let parent = stack.last().copied();
                let dynamic = dynamic_attributes(&name, &attributes);
                let foreign_here = foreign_depth > 0;
                let element_foreign = foreign_here || matches!(name.as_str(), "svg" | "math");
                let foreign_root =
                    !foreign_here && !tag.self_closing && matches!(name.as_str(), "svg" | "math");
                let is_void = VOID_TAGS.contains(&name.as_str());
                let invalid_self_close = tag.self_closing && !is_void && !element_foreign;
                let index = nodes.len();
                nodes.push(Element {
                    id: format!("n{index}-{document_hash}"),
                    tag: name.clone(),
                    parent,
                    line: line_at(&line_starts, span.start),
                    editable: false,
                    reason: None,
                    inject: false,
                    start_tag_span: span.clone(),
                    start_tag_end: span.end,
                    start_tag_insert: insert,
                    end_tag_start: None,
                    structural_end: None,
                    void: is_void,
                    foreign: element_foreign,
                    foreign_root,
                    dynamic,
                    auto_closed: false,
                    unclosed: false,
                    stray_end: false,
                    invalid_self_close,
                    has_child_element: false,
                    has_comment: false,
                    attribute_values: attributes,
                    existing_node_attribute: false,
                    span_ok,
                });
                if let Some(parent) = parent {
                    nodes[parent].has_child_element = true;
                }
                if is_void || (element_foreign && tag.self_closing) {
                    nodes[index].structural_end = Some(span.end);
                } else {
                    if foreign_root {
                        foreign_depth += 1;
                    }
                    stack.push(index);
                }
            }
            Token::EndTag(tag) => {
                let name = String::from_utf8_lossy(&tag.name).to_ascii_lowercase();
                let span = tag.span.start..tag.span.end;
                let span_ok = span.start < span.end
                    && span.end <= source.len()
                    && source.as_bytes().get(span.start) == Some(&b'<')
                    && source.as_bytes().get(span.end - 1) == Some(&b'>');
                let Some(position) = stack.iter().rposition(|index| nodes[*index].tag == name)
                else {
                    // Browsers ignore stray end tags. The surrounding open
                    // elements keep their own explicit boundaries.
                    continue;
                };
                while stack.len() > position + 1 {
                    let closed = stack.pop().expect("nonempty stack");
                    if nodes[closed].foreign_root {
                        foreign_depth = foreign_depth.saturating_sub(1);
                    }
                    nodes[closed].auto_closed = true;
                    nodes[closed].structural_end = Some(span.start.min(source.len()));
                }
                let closed = stack.pop().expect("matched stack entry");
                if nodes[closed].foreign_root {
                    foreign_depth = foreign_depth.saturating_sub(1);
                }
                if span_ok {
                    nodes[closed].end_tag_start = Some(span.start);
                    nodes[closed].structural_end = Some(span.end);
                } else {
                    nodes[closed].stray_end = true;
                }
            }
            Token::String(_) => {}
            Token::Comment(_) => {
                if let Some(top) = stack.last() {
                    nodes[*top].has_comment = true;
                }
            }
            Token::Doctype(_) => {}
            Token::Error(error) => {
                errors.push(error.span.start..error.span.end);
            }
        }
    }
    for index in stack {
        nodes[index].unclosed = true;
    }
    let mut document = Document {
        nodes,
        index: BTreeMap::new(),
    };
    for index in 0..document.nodes.len() {
        let parent = document.nodes[index].parent;
        let parent_dynamic = parent.is_some_and(|parent| document.nodes[parent].dynamic);
        let parent_custom = parent.is_some_and(|parent| document.nodes[parent].tag.contains('-'));
        let parent_ambiguous = parent.is_some_and(|parent| {
            let node = &document.nodes[parent];
            node.auto_closed || node.unclosed || node.stray_end || node.invalid_self_close
        });
        let parent_raw_text = parent
            .is_some_and(|parent| RAW_TEXT_TAGS.contains(&document.nodes[parent].tag.as_str()));
        document.nodes[index].dynamic =
            document.nodes[index].dynamic || parent_dynamic || parent_custom || parent_raw_text;
        let tokenizer_error = errors
            .iter()
            .any(|error| overlaps(error, &document.nodes[index].start_tag_span));
        let text_dynamic = template_text_dynamic(source, &document, index);
        if text_dynamic {
            document.nodes[index].dynamic = true;
        }
        let node = &document.nodes[index];
        let tag = node.tag.as_str();
        let reason = if node.attribute_values.contains_key(NODE_ATTRIBUTE) {
            Some("reserved_attribute")
        } else if node.foreign {
            Some(if matches!(tag, "svg" | "math") {
                "vector_content"
            } else {
                "foreign_content"
            })
        } else if NON_EDITABLE_TAGS.contains(&tag) {
            Some("non_editable_element")
        } else if node.dynamic || text_dynamic {
            Some("dynamic_region")
        } else if tag.contains('-') {
            Some("unsupported_custom_element")
        } else if node.auto_closed
            || node.unclosed
            || node.stray_end
            || node.invalid_self_close
            || parent_ambiguous
        {
            Some("ambiguous_structure")
        } else if tokenizer_error {
            Some("tokenizer_error")
        } else if !node.span_ok || node.start_tag_insert.is_none() {
            Some("unmapped_source")
        } else {
            None
        };
        let inject = node.span_ok
            && node.start_tag_insert.is_some()
            && !node.foreign
            && !NON_INJECT_TAGS.contains(&tag)
            && !node.attribute_values.contains_key(NODE_ATTRIBUTE);
        let node = &mut document.nodes[index];
        node.editable = reason.is_none();
        node.reason = reason.map(str::to_owned);
        node.inject = inject;
        node.existing_node_attribute = node.attribute_values.contains_key(NODE_ATTRIBUTE);
        document.index.insert(node.id.clone(), index);
    }
    Ok(document)
}

/// Applies the small set of HTML optional-end-tag rules that affect source
/// boundaries in ordinary pages. Every element closed this way is marked
/// ambiguous and stays read-only, so an inferred boundary is never edited.
fn close_implied(stack: &mut Vec<usize>, nodes: &mut [Element], name: &str, at: usize) {
    if matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "div"
            | "dl"
            | "fieldset"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "ul"
    ) && let Some(position) = stack.iter().rposition(|index| nodes[*index].tag == "p")
    {
        close_through(stack, nodes, position, at);
    }
    if matches!(name, "a" | "button" | "form" | "nobr")
        && let Some(position) = stack.iter().rposition(|index| nodes[*index].tag == name)
    {
        close_through(stack, nodes, position, at);
    }
    let position = match name {
        "li" => stack.iter().rposition(|index| nodes[*index].tag == "li"),
        "dt" | "dd" => stack
            .iter()
            .rposition(|index| matches!(nodes[*index].tag.as_str(), "dt" | "dd")),
        "p" => stack.iter().rposition(|index| nodes[*index].tag == "p"),
        "option" => stack
            .iter()
            .rposition(|index| nodes[*index].tag == "option"),
        "optgroup" => stack
            .iter()
            .rposition(|index| matches!(nodes[*index].tag.as_str(), "option" | "optgroup")),
        "tr" => stack.iter().rposition(|index| nodes[*index].tag == "tr"),
        "td" | "th" => stack
            .iter()
            .rposition(|index| matches!(nodes[*index].tag.as_str(), "td" | "th")),
        "thead" | "tbody" | "tfoot" => stack
            .iter()
            .rposition(|index| matches!(nodes[*index].tag.as_str(), "td" | "th" | "tr")),
        _ => None,
    };
    if let Some(position) = position {
        close_through(stack, nodes, position, at);
    }
}

fn close_through(stack: &mut Vec<usize>, nodes: &mut [Element], position: usize, at: usize) {
    while stack.len() > position {
        let closed = stack.pop().expect("nonempty stack");
        nodes[closed].auto_closed = true;
        nodes[closed].structural_end = Some(at.min(nodes[closed].structural_end.unwrap_or(at)));
    }
}

fn dynamic_attributes(name: &str, attributes: &BTreeMap<String, String>) -> bool {
    if matches!(name, "template" | "slot") {
        return true;
    }
    attributes.iter().any(|(name, value)| {
        let name = name.to_ascii_lowercase();
        name.starts_with("v-")
            || name.starts_with(':')
            || name.starts_with('@')
            || name.starts_with("x-")
            || name.starts_with("*ng")
            || name.starts_with('[')
            || name.starts_with('(')
            || name.starts_with('#')
            || name.starts_with("th:")
            || name.starts_with("bind:")
            || name.starts_with("on:")
            || matches!(
                name.as_str(),
                "data-bind"
                    | "data-if"
                    | "data-for"
                    | "data-each"
                    | "ng-if"
                    | "ng-for"
                    | "ng-repeat"
            )
            || has_template_markers(value)
    })
}

fn has_template_markers(value: &str) -> bool {
    ["{{", "}}", "{%", "%}", "${", "#{", "<%", "%>"]
        .iter()
        .any(|marker| value.contains(marker))
}

/// Checks the element's own text runs, not the text of its descendants. Text
/// after a child (for example `<div><span>Hi</span> {{ user }}</div>`) still
/// belongs to the wrapper and must make the wrapper read-only.
fn template_text_dynamic(source: &str, document: &Document, index: usize) -> bool {
    let node = &document.nodes[index];
    let Some(end) = node.end_tag_start else {
        return false;
    };
    let start = node.start_tag_end;
    if start >= end || end > source.len() {
        return false;
    }
    let mut cursor = start;
    for child in document.nodes.iter().skip(index + 1) {
        let child_start = child.start_tag_span.start;
        if child_start >= end {
            break;
        }
        if child.parent != Some(index) {
            continue;
        }
        let child_end = child.structural_end.unwrap_or(end).min(end);
        if child_start < cursor {
            cursor = cursor.max(child_end);
            continue;
        }
        if has_template_markers(&source[cursor..child_start]) {
            return true;
        }
        cursor = cursor.max(child_end);
    }
    cursor <= end && has_template_markers(&source[cursor..end])
}

fn overlaps(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

fn line_at(starts: &[usize], offset: usize) -> usize {
    starts.partition_point(|start| *start <= offset)
}

enum InstrumentEdit {
    Insert(usize, String),
    Remove(Range<usize>),
}

fn instrument(source: &str, document: &Document) -> Result<String, ProjectError> {
    let mut edits: Vec<InstrumentEdit> = Vec::new();
    for node in &document.nodes {
        let reserved = node.existing_node_attribute
            || node.attribute_values.contains_key(READONLY_ATTRIBUTE)
            || node
                .attribute_values
                .contains_key(READONLY_REASON_ATTRIBUTE);
        if reserved && let Ok(attributes) = scan_attributes(source, node.start_tag_span.clone()) {
            // A page cannot spoof a mapping id or a read-only marker: the
            // reserved attributes are removed from the response only. The
            // draft keeps the original bytes.
            for attribute in attributes.iter().filter(|attribute| {
                attribute.name == NODE_ATTRIBUTE
                    || attribute.name == READONLY_ATTRIBUTE
                    || attribute.name == READONLY_REASON_ATTRIBUTE
            }) {
                let mut start = attribute.range.start;
                if start > 0 && source.as_bytes()[start - 1].is_ascii_whitespace() {
                    start -= 1;
                }
                edits.push(InstrumentEdit::Remove(start..attribute.range.end));
            }
        }
        if node.inject
            && let Some(at) = node.start_tag_insert
        {
            let mut injected = format!(" {NODE_ATTRIBUTE}=\"{}\"", node.id);
            if !node.editable {
                injected.push_str(&format!(" {READONLY_ATTRIBUTE}=\"true\""));
                if let Some(reason) = &node.reason {
                    injected.push_str(&format!(
                        " {READONLY_REASON_ATTRIBUTE}=\"{}\"",
                        escape_attribute(reason)
                    ));
                }
            }
            edits.push(InstrumentEdit::Insert(at, injected));
        }
    }
    edits.sort_by_key(|edit| match edit {
        InstrumentEdit::Insert(at, _) => *at,
        InstrumentEdit::Remove(range) => range.start,
    });
    let mut output = String::with_capacity(source.len() + edits.len() * 16);
    let mut cursor = 0;
    for edit in edits {
        match edit {
            InstrumentEdit::Insert(at, attribute) => {
                if at < cursor || at > source.len() || !source.is_char_boundary(at) {
                    return Err(ProjectError::new(
                        "parse_failed",
                        "a mapped source position is not a valid insertion point",
                    ));
                }
                output.push_str(&source[cursor..at]);
                output.push_str(&attribute);
                cursor = at;
            }
            InstrumentEdit::Remove(range) => {
                if range.start < cursor
                    || range.end > source.len()
                    || !source.is_char_boundary(range.start)
                    || !source.is_char_boundary(range.end)
                {
                    return Err(ProjectError::new(
                        "parse_failed",
                        "a mapped attribute range is not valid",
                    ));
                }
                output.push_str(&source[cursor..range.start]);
                cursor = range.end;
            }
        }
    }
    output.push_str(&source[cursor..]);
    Ok(output)
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

struct OperationResult {
    source: String,
    selection: Option<SelectionHint>,
}

#[derive(Clone, Copy)]
enum SelectionHint {
    /// The selected element keeps its exact source start.
    ExactStart(usize),
    /// Select the first editable element whose start is at or after this byte.
    FirstEditableAtOrAfter(usize),
}

impl OperationResult {
    fn new(source: String, selection: Option<SelectionHint>) -> Self {
        Self { source, selection }
    }
}

/// Resolves a source-position hint against the already parsed result. A stale
/// hint is not an operation failure; it simply yields no selection.
fn resolve_selection(document: &Document, hint: Option<SelectionHint>) -> Option<usize> {
    match hint? {
        SelectionHint::ExactStart(at) => document
            .nodes
            .iter()
            .find(|node| node.editable && node.start_tag_span.start == at)
            .map(|node| node.start_tag_span.start),
        SelectionHint::FirstEditableAtOrAfter(at) => document
            .nodes
            .iter()
            .filter(|node| node.editable && node.start_tag_span.start >= at)
            .min_by_key(|node| node.start_tag_span.start)
            .map(|node| node.start_tag_span.start),
    }
}

fn apply_operation(
    document: &Document,
    source: &str,
    operation: &Value,
    kind: &str,
) -> Result<OperationResult, ProjectError> {
    match kind {
        "style" => {
            let node = document.node(string_field(operation, "id")?)?;
            ensure_editable(node)?;
            let properties = operation
                .get("properties")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    ProjectError::new("invalid_operation", "style properties must be an object")
                })?;
            let updates = validate_style_properties(properties)?;
            let current = node
                .attribute_values
                .get("style")
                .map(String::as_str)
                .unwrap_or("");
            let merged = merge_style(current, &updates)?;
            Ok(OperationResult::new(
                set_attribute(source, node, "style", &merged)?,
                Some(SelectionHint::ExactStart(node.start_tag_span.start)),
            ))
        }
        "text" => {
            let node = document.node(string_field(operation, "id")?)?;
            ensure_editable(node)?;
            let text = string_field(operation, "text")?;
            validate_text(text)?;
            if node.void || node.has_child_element || node.has_comment {
                return Err(ProjectError::new(
                    "unsupported_target",
                    "text editing requires an element whose content is plain text",
                ));
            }
            let end = node.end_tag_start.ok_or_else(|| {
                ProjectError::new(
                    "unsupported_target",
                    "text editing requires an exact closing tag boundary",
                )
            })?;
            if node.start_tag_end > end {
                return Err(ProjectError::new(
                    "unsupported_target",
                    "element content range is not valid",
                ));
            }
            Ok(OperationResult::new(
                splice(source, node.start_tag_end..end, &escape_text(text))?,
                Some(SelectionHint::ExactStart(node.start_tag_span.start)),
            ))
        }
        "attribute" => {
            let node = document.node(string_field(operation, "id")?)?;
            ensure_editable(node)?;
            let name = string_field(operation, "name")?.to_ascii_lowercase();
            let value = string_field(operation, "value")?;
            validate_attribute(document, node, &name, value)?;
            Ok(OperationResult::new(
                set_attribute(source, node, &name, value)?,
                Some(SelectionHint::ExactStart(node.start_tag_span.start)),
            ))
        }
        "move" => {
            let id = string_field(operation, "id")?;
            let target_id = string_field(operation, "target")?;
            let placement = string_field(operation, "placement")?;
            let source_index = document.index_of(id)?;
            let target_index = document.index_of(target_id)?;
            if source_index == target_index {
                return Err(ProjectError::new(
                    "unsupported_target",
                    "an element cannot be moved onto itself",
                ));
            }
            let from = &document.nodes[source_index];
            let target = &document.nodes[target_index];
            ensure_editable(from)?;
            ensure_editable(target)?;
            ensure_not_protected(from)?;
            ensure_no_script(document, source_index, "moved")?;
            if PROTECTED_STRUCTURE.contains(&target.tag.as_str()) && placement != "inside" {
                return Err(ProjectError::new(
                    "unsupported_target",
                    format!(
                        "<{}> is protected page structure and only accepts inside placement",
                        target.tag
                    ),
                ));
            }
            if document.is_descendant(target_index, source_index) {
                return Err(ProjectError::new(
                    "unsupported_target",
                    "an element cannot be moved inside its own subtree",
                ));
            }
            let range = from.source_range().ok_or_else(|| {
                ProjectError::new(
                    "unsupported_target",
                    "the element has no exact source boundary to move",
                )
            })?;
            let insert_at = placement_offset(document, target_index, placement, Some(&from.tag))?;
            let selected_at = if insert_at <= range.start {
                insert_at
            } else {
                insert_at - (range.end - range.start)
            };
            let snippet = source[range.clone()].to_owned();
            Ok(OperationResult::new(
                relocate(source, range, insert_at, &snippet)?,
                Some(SelectionHint::FirstEditableAtOrAfter(selected_at)),
            ))
        }
        "insert" => {
            let target_id = string_field(operation, "target")?;
            let placement = string_field(operation, "placement")?;
            let component = string_field(operation, "component")?.to_ascii_lowercase();
            let snippet = component_html(&component)?;
            let target_index = document.index_of(target_id)?;
            let target = &document.nodes[target_index];
            ensure_editable(target)?;
            let insert_at = placement_offset(document, target_index, placement, None)?;
            Ok(OperationResult::new(
                splice(source, insert_at..insert_at, &snippet)?,
                Some(SelectionHint::FirstEditableAtOrAfter(insert_at)),
            ))
        }
        "duplicate" => {
            let index = document.index_of(string_field(operation, "id")?)?;
            let node = &document.nodes[index];
            ensure_editable(node)?;
            ensure_not_protected(node)?;
            ensure_no_script(document, index, "duplicated")?;
            let range = node.source_range().ok_or_else(|| {
                ProjectError::new(
                    "unsupported_target",
                    "the element has no exact source boundary to duplicate",
                )
            })?;
            if contains_id(document, index) {
                return Err(ProjectError::new(
                    "unsupported_target",
                    "duplicating this element would create duplicate id attributes",
                ));
            }
            let snippet = source[range.clone()].to_owned();
            let insert_at = range.end;
            Ok(OperationResult::new(
                splice(source, insert_at..insert_at, &snippet)?,
                Some(SelectionHint::FirstEditableAtOrAfter(insert_at)),
            ))
        }
        "delete" => {
            let index = document.index_of(string_field(operation, "id")?)?;
            let node = &document.nodes[index];
            ensure_editable(node)?;
            ensure_not_protected(node)?;
            ensure_no_script(document, index, "deleted")?;
            let range = node.source_range().ok_or_else(|| {
                ProjectError::new(
                    "unsupported_target",
                    "the element has no exact source boundary to delete",
                )
            })?;
            Ok(OperationResult::new(splice(source, range, "")?, None))
        }
        _ => Err(ProjectError::new(
            "invalid_operation",
            format!("unsupported operation kind {kind}"),
        )),
    }
}

fn string_field<'a>(value: &'a Value, name: &str) -> Result<&'a str, ProjectError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ProjectError::new("invalid_operation", format!("{name} must be a string")))
}

fn ensure_editable(node: &Element) -> Result<(), ProjectError> {
    if node.editable {
        return Ok(());
    }
    Err(ProjectError::new(
        "not_editable",
        format!(
            "<{}> at line {} is read-only: {}",
            node.tag,
            node.line,
            node.reason.as_deref().unwrap_or("unsupported region")
        ),
    ))
}

fn ensure_not_protected(node: &Element) -> Result<(), ProjectError> {
    if PROTECTED_STRUCTURE.contains(&node.tag.as_str()) {
        return Err(ProjectError::new(
            "unsupported_target",
            format!("<{}> is protected page structure", node.tag),
        ));
    }
    Ok(())
}

fn placement_offset(
    document: &Document,
    target_index: usize,
    placement: &str,
    content_tag: Option<&str>,
) -> Result<usize, ProjectError> {
    let target = &document.nodes[target_index];
    match placement {
        "before" => {
            ensure_container_parent(document, target_index)?;
            let parent = target
                .parent
                .map(|parent| &document.nodes[parent])
                .ok_or_else(|| {
                    ProjectError::new(
                        "unsupported_target",
                        "the page root has no insertion parent",
                    )
                })?;
            ensure_table_child(parent, content_tag)?;
            Ok(target.start_tag_span.start)
        }
        "after" => {
            ensure_container_parent(document, target_index)?;
            let parent = target
                .parent
                .map(|parent| &document.nodes[parent])
                .ok_or_else(|| {
                    ProjectError::new(
                        "unsupported_target",
                        "the page root has no insertion parent",
                    )
                })?;
            ensure_table_child(parent, content_tag)?;
            target.structural_end.ok_or_else(|| {
                ProjectError::new(
                    "unsupported_target",
                    "the insertion target has no exact source boundary",
                )
            })
        }
        "inside" => {
            if !CONTAINER_TAGS.contains(&target.tag.as_str()) {
                return Err(ProjectError::new(
                    "unsupported_target",
                    format!("<{}> is not a supported insertion container", target.tag),
                ));
            }
            if target.void {
                return Err(ProjectError::new(
                    "unsupported_target",
                    "a void element cannot contain inserted content",
                ));
            }
            ensure_table_child(target, content_tag)?;
            let at = target.end_tag_start.ok_or_else(|| {
                ProjectError::new(
                    "unsupported_target",
                    "the target container has no exact closing tag boundary",
                )
            })?;
            if document.nodes.iter().enumerate().any(|(index, node)| {
                index != target_index
                    && document.is_descendant(index, target_index)
                    && node.auto_closed
                    && node.structural_end == Some(at)
            }) {
                return Err(ProjectError::new(
                    "ambiguous_target",
                    "the inside insertion point falls within an implicitly closed descendant; add explicit closing tags or choose another target",
                ));
            }
            Ok(at)
        }
        _ => Err(ProjectError::new(
            "invalid_operation",
            "placement must be before, after or inside",
        )),
    }
}

/// Rejects content that a browser would move out of a strict table container.
/// `None` means a generic inserted component; `Some` is the tag being moved.
fn ensure_table_child(parent: &Element, content_tag: Option<&str>) -> Result<(), ProjectError> {
    if !TABLE_STRUCTURE_TAGS.contains(&parent.tag.as_str()) {
        return Ok(());
    }
    let allowed = match content_tag {
        None => false,
        Some(child) => match parent.tag.as_str() {
            "table" => matches!(
                child,
                "caption" | "colgroup" | "thead" | "tbody" | "tfoot" | "tr"
            ),
            "thead" | "tbody" | "tfoot" => child == "tr",
            "tr" => matches!(child, "td" | "th"),
            _ => true,
        },
    };
    if allowed {
        return Ok(());
    }
    Err(ProjectError::new(
        "unsupported_target",
        match content_tag {
            Some(child) => format!(
                "<{}> cannot directly contain <{}>; select a td, th or another container",
                parent.tag, child
            ),
            None => format!(
                "<{}> cannot directly receive a generic component; select a td or th",
                parent.tag
            ),
        },
    ))
}

fn ensure_container_parent(document: &Document, index: usize) -> Result<(), ProjectError> {
    let node = &document.nodes[index];
    let parent = node.parent.ok_or_else(|| {
        ProjectError::new(
            "unsupported_target",
            "the page root has no insertion parent",
        )
    })?;
    let parent = &document.nodes[parent];
    if !parent.editable {
        return Err(ProjectError::new(
            "not_editable",
            format!(
                "the parent <{}> is read-only: {}",
                parent.tag,
                parent.reason.as_deref().unwrap_or("unsupported region")
            ),
        ));
    }
    if !CONTAINER_TAGS.contains(&parent.tag.as_str()) {
        return Err(ProjectError::new(
            "unsupported_target",
            format!("<{}> cannot receive sibling block content", parent.tag),
        ));
    }
    Ok(())
}

/// A wrapper that contains a script must not be duplicated, moved or removed:
/// each operation would duplicate or reorder executable content.
fn ensure_no_script(
    document: &Document,
    index: usize,
    operation: &str,
) -> Result<(), ProjectError> {
    let contains_script = document.nodes.iter().enumerate().any(|(candidate, node)| {
        (candidate == index || document.is_descendant(candidate, index)) && node.tag == "script"
    });
    if contains_script {
        return Err(ProjectError::new(
            "unsupported_target",
            format!(
                "an element that contains a script cannot be {operation}; edit a neighboring element instead"
            ),
        ));
    }
    Ok(())
}

fn contains_id(document: &Document, index: usize) -> bool {
    document.nodes.iter().enumerate().any(|(candidate, node)| {
        (candidate == index || document.is_descendant(candidate, index))
            && node.attribute_values.contains_key("id")
    })
}

fn relocate(
    source: &str,
    remove: Range<usize>,
    insert_at: usize,
    snippet: &str,
) -> Result<String, ProjectError> {
    if remove.start > remove.end
        || remove.end > source.len()
        || insert_at > source.len()
        || !source.is_char_boundary(remove.start)
        || !source.is_char_boundary(remove.end)
        || !source.is_char_boundary(insert_at)
    {
        return Err(ProjectError::new(
            "unsupported_target",
            "the requested source range is not valid",
        ));
    }
    if insert_at > remove.start && insert_at < remove.end {
        return Err(ProjectError::new(
            "unsupported_target",
            "an element cannot be moved inside its own source range",
        ));
    }
    let mut output = String::with_capacity(source.len() + snippet.len());
    if insert_at <= remove.start {
        output.push_str(&source[..insert_at]);
        output.push_str(snippet);
        output.push_str(&source[insert_at..remove.start]);
        output.push_str(&source[remove.end..]);
    } else {
        output.push_str(&source[..remove.start]);
        output.push_str(&source[remove.end..insert_at]);
        output.push_str(snippet);
        output.push_str(&source[insert_at..]);
    }
    Ok(output)
}

fn splice(source: &str, range: Range<usize>, replacement: &str) -> Result<String, ProjectError> {
    if range.start > range.end
        || range.end > source.len()
        || !source.is_char_boundary(range.start)
        || !source.is_char_boundary(range.end)
    {
        return Err(ProjectError::new(
            "unsupported_target",
            "the requested source range is not valid",
        ));
    }
    let mut output =
        String::with_capacity(source.len() - (range.end - range.start) + replacement.len());
    output.push_str(&source[..range.start]);
    output.push_str(replacement);
    output.push_str(&source[range.end..]);
    Ok(output)
}

// ---------------------------------------------------------------------------
// Text and attribute edits
// ---------------------------------------------------------------------------

fn validate_text(text: &str) -> Result<(), ProjectError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(ProjectError::new(
            "invalid_value",
            format!("text exceeds the {MAX_TEXT_BYTES}-byte limit"),
        ));
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(ProjectError::new(
            "invalid_value",
            "text contains an unsupported control character",
        ));
    }
    Ok(())
}

fn escape_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            _ => output.push(character),
        }
    }
    output
}

fn escape_attribute(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
    output
}

#[derive(Debug, Clone)]
struct AttributeSpan {
    name: String,
    name_range: Range<usize>,
    value_range: Option<Range<usize>>,
    range: Range<usize>,
    quoted: bool,
}

fn scan_attributes(source: &str, tag: Range<usize>) -> Result<Vec<AttributeSpan>, ProjectError> {
    let bytes = source.as_bytes();
    if tag.start >= tag.end || tag.end > bytes.len() || bytes[tag.start] != b'<' {
        return Err(ProjectError::new(
            "parse_failed",
            "start tag span is not valid",
        ));
    }
    let mut index = tag.start + 1;
    while index < tag.end
        && !bytes[index].is_ascii_whitespace()
        && !matches!(bytes[index], b'>' | b'/')
    {
        index += 1;
    }
    let mut attributes = Vec::new();
    loop {
        while index < tag.end && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= tag.end || bytes[index] == b'>' {
            break;
        }
        if bytes[index] == b'/' {
            index += 1;
            continue;
        }
        let name_start = index;
        while index < tag.end
            && !bytes[index].is_ascii_whitespace()
            && !matches!(bytes[index], b'=' | b'>' | b'/')
        {
            index += 1;
        }
        if index == name_start {
            index += 1;
            continue;
        }
        let name_end = index;
        while index < tag.end && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let mut value_range = None;
        let mut quoted = false;
        if index < tag.end && bytes[index] == b'=' {
            index += 1;
            while index < tag.end && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index < tag.end && matches!(bytes[index], b'"' | b'\'') {
                let quote = bytes[index];
                quoted = true;
                index += 1;
                let value_start = index;
                while index < tag.end && bytes[index] != quote {
                    index += 1;
                }
                value_range = Some(value_start..index);
                if index < tag.end {
                    index += 1;
                }
            } else {
                let value_start = index;
                while index < tag.end && !bytes[index].is_ascii_whitespace() && bytes[index] != b'>'
                {
                    index += 1;
                }
                value_range = Some(value_start..index);
            }
        }
        attributes.push(AttributeSpan {
            name: String::from_utf8_lossy(&bytes[name_start..name_end]).to_ascii_lowercase(),
            name_range: name_start..name_end,
            value_range,
            range: name_start..index,
            quoted,
        });
    }
    Ok(attributes)
}

fn set_attribute(
    source: &str,
    node: &Element,
    name: &str,
    value: &str,
) -> Result<String, ProjectError> {
    let escaped = escape_attribute(value);
    let attributes = scan_attributes(source, node.start_tag_span.clone())?;
    if let Some(attribute) = attributes
        .iter()
        .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
    {
        if let Some(range) = &attribute.value_range {
            if attribute.quoted {
                return splice(source, range.clone(), &escaped);
            }
            return splice(
                source,
                attribute.name_range.start..range.end,
                &format!("{name}=\"{escaped}\""),
            );
        }
        return splice(
            source,
            attribute.name_range.end..attribute.name_range.end,
            &format!("=\"{escaped}\""),
        );
    }
    let insert_at = node.start_tag_insert.ok_or_else(|| {
        ProjectError::new(
            "parse_failed",
            "start tag has no valid attribute insertion point",
        )
    })?;
    splice(
        source,
        insert_at..insert_at,
        &format!(" {name}=\"{escaped}\""),
    )
}

const ATTRIBUTE_NAMES: &[&str] = &[
    "href",
    "src",
    "alt",
    "title",
    "aria-label",
    "aria-labelledby",
    "aria-describedby",
    "aria-controls",
    "aria-expanded",
    "id",
    "name",
    "placeholder",
    "type",
    "target",
    "rel",
    "download",
    "width",
    "height",
    "loading",
    "decoding",
    "role",
    "for",
    "value",
    "action",
    "method",
    "autocomplete",
    "inputmode",
    "pattern",
    "min",
    "max",
    "step",
    "disabled",
];

fn validate_attribute(
    document: &Document,
    node: &Element,
    name: &str,
    value: &str,
) -> Result<(), ProjectError> {
    if name.is_empty()
        || name.len() > 64
        || !name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        return Err(ProjectError::new(
            "invalid_attribute",
            "attribute names are limited to lowercase ASCII letters, digits and hyphens",
        ));
    }
    if name.starts_with("on") {
        return Err(ProjectError::new(
            "invalid_attribute",
            "event handler attributes cannot be changed",
        ));
    }
    if name == "style" {
        return Err(ProjectError::new(
            "invalid_attribute",
            "use the style operation for inline styles",
        ));
    }
    if !ATTRIBUTE_NAMES.contains(&name) {
        return Err(ProjectError::new(
            "invalid_attribute",
            format!("{name} is not an editable attribute"),
        ));
    }
    if value.len() > MAX_ATTRIBUTE_VALUE_BYTES
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(ProjectError::new(
            "invalid_value",
            "attribute value exceeds the limit or contains a control character",
        ));
    }
    match name {
        "href" => validate_url(value, false),
        "src" => validate_url(value, true),
        "action" => validate_url(value, false),
        "for" | "aria-controls" | "aria-labelledby" | "aria-describedby" => {
            validate_identifier(value)
        }
        "target" => {
            if value.is_empty()
                || value.starts_with('_')
                    && !matches!(value, "_blank" | "_self" | "_parent" | "_top")
            {
                return Err(ProjectError::new(
                    "invalid_value",
                    "target must be a browsing-context name or _blank, _self, _parent or _top",
                ));
            }
            Ok(())
        }
        "rel" => {
            if value.chars().any(|character| {
                !(character.is_ascii_alphanumeric() || matches!(character, '-' | ' '))
            }) {
                return Err(ProjectError::new(
                    "invalid_value",
                    "rel may contain only link-relation tokens",
                ));
            }
            Ok(())
        }
        "type" => {
            const TYPES: &[&str] = &[
                "text", "button", "submit", "reset", "email", "password", "search", "number",
                "tel", "url", "checkbox", "radio", "range", "date", "time", "color",
            ];
            if !TYPES.contains(&value) {
                return Err(ProjectError::new(
                    "invalid_value",
                    "type must be a supported control type",
                ));
            }
            Ok(())
        }
        "width" | "height" => {
            if !value.is_empty() && !value.chars().all(|character| character.is_ascii_digit()) {
                return Err(ProjectError::new(
                    "invalid_value",
                    "width and height must be nonnegative integers",
                ));
            }
            Ok(())
        }
        "loading" => {
            if !matches!(value, "" | "eager" | "lazy") {
                return Err(ProjectError::new(
                    "invalid_value",
                    "loading must be eager or lazy",
                ));
            }
            Ok(())
        }
        "decoding" => {
            if !matches!(value, "" | "sync" | "async" | "auto") {
                return Err(ProjectError::new(
                    "invalid_value",
                    "decoding must be sync, async or auto",
                ));
            }
            Ok(())
        }
        "role" => {
            if !value
                .chars()
                .all(|character| character.is_ascii_lowercase() || matches!(character, '-' | ' '))
            {
                return Err(ProjectError::new(
                    "invalid_value",
                    "role must contain only role tokens",
                ));
            }
            Ok(())
        }
        "aria-expanded" => {
            if !matches!(value, "" | "true" | "false") {
                return Err(ProjectError::new(
                    "invalid_value",
                    "aria-expanded must be true or false",
                ));
            }
            Ok(())
        }
        "method" => {
            if !matches!(value.to_ascii_lowercase().as_str(), "get" | "post") {
                return Err(ProjectError::new(
                    "invalid_value",
                    "method must be get or post",
                ));
            }
            Ok(())
        }
        "autocomplete" => {
            if !value
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
            {
                return Err(ProjectError::new(
                    "invalid_value",
                    "autocomplete must contain only token characters",
                ));
            }
            Ok(())
        }
        "inputmode" => {
            const MODES: &[&str] = &[
                "none", "text", "decimal", "numeric", "tel", "search", "email", "url",
            ];
            if !MODES.contains(&value) {
                return Err(ProjectError::new(
                    "invalid_value",
                    "inputmode must be a supported input mode",
                ));
            }
            Ok(())
        }
        "min" | "max" | "step" => {
            if !value.chars().all(|character| {
                character.is_ascii_digit() || matches!(character, '.' | '-' | ':' | '%' | 'T')
            }) {
                return Err(ProjectError::new(
                    "invalid_value",
                    "min, max and step must be numeric or date tokens",
                ));
            }
            Ok(())
        }
        "id" => {
            validate_identifier(value)?;
            if document.nodes.iter().any(|candidate| {
                !std::ptr::eq(candidate, node)
                    && candidate
                        .attribute_values
                        .get("id")
                        .is_some_and(|existing| existing == value)
            }) {
                return Err(ProjectError::new(
                    "invalid_value",
                    "the id is already used by another element",
                ));
            }
            Ok(())
        }
        "name" | "download" => {
            if !value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            }) {
                return Err(ProjectError::new(
                    "invalid_value",
                    "name and download must contain only simple token characters",
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_identifier(value: &str) -> Result<(), ProjectError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(ProjectError::new(
            "invalid_value",
            "identifier must contain 1..128 ASCII letters, digits, hyphens or underscores",
        ));
    }
    Ok(())
}

fn validate_url(value: &str, allow_image_data: bool) -> Result<(), ProjectError> {
    if value.chars().any(char::is_control) {
        return Err(ProjectError::new(
            "invalid_value",
            "URL contains a control character",
        ));
    }
    let lower = value.trim().to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("file:")
    {
        return Err(ProjectError::new(
            "invalid_value",
            "this URL scheme is not allowed",
        ));
    }
    if lower.starts_with("data:") {
        let allowed = allow_image_data
            && [
                "data:image/png",
                "data:image/jpeg",
                "data:image/gif",
                "data:image/webp",
                "data:image/avif",
                "data:image/svg+xml",
            ]
            .iter()
            .any(|prefix| lower.starts_with(prefix));
        if !allowed {
            return Err(ProjectError::new(
                "invalid_value",
                "only image data URLs are allowed for src",
            ));
        }
        return Ok(());
    }
    if value.is_empty()
        || value.starts_with('#')
        || value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('?')
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
    {
        return Ok(());
    }
    if value.contains(':') {
        return Err(ProjectError::new(
            "invalid_value",
            "relative URLs must not contain an unknown scheme",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Inline style validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum StyleKind {
    Color,
    Length,
    LengthList,
    FontFamily,
    FontWeight,
    LineHeight,
    TextAlign,
    Display,
    FlexDirection,
    Justify,
    Align,
    Grid,
    BorderStyle,
    Opacity,
    Overflow,
    Position,
    ZIndex,
    ObjectFit,
    AspectRatio,
    Number,
    Order,
    FlexWrap,
    WhiteSpace,
    Visibility,
    TextDecoration,
    TextTransform,
    Border,
    Shadow,
    Cursor,
    Transition,
    /// A case-sensitive `--custom-property` declaration.
    Custom,
}

fn style_kind(name: &str) -> Option<StyleKind> {
    use StyleKind::*;
    Some(match name {
        "color"
        | "background-color"
        | "border-color"
        | "border-top-color"
        | "border-right-color"
        | "border-bottom-color"
        | "border-left-color"
        | "outline-color"
        | "text-decoration-color"
        | "caret-color"
        | "fill"
        | "stroke"
        | "background" => Color,
        "width"
        | "height"
        | "min-width"
        | "max-width"
        | "min-height"
        | "max-height"
        | "top"
        | "right"
        | "bottom"
        | "left"
        | "inset"
        | "padding-top"
        | "padding-right"
        | "padding-bottom"
        | "padding-left"
        | "padding-inline"
        | "padding-block"
        | "margin-top"
        | "margin-right"
        | "margin-bottom"
        | "margin-left"
        | "margin-inline"
        | "margin-block"
        | "border-width"
        | "border-top-width"
        | "border-right-width"
        | "border-bottom-width"
        | "border-left-width"
        | "outline-width"
        | "row-gap"
        | "column-gap"
        | "flex-basis"
        | "letter-spacing"
        | "word-spacing"
        | "font-size" => Length,
        "padding" | "margin" | "gap" | "border-radius" => LengthList,
        "font-family" => FontFamily,
        "font-weight" => FontWeight,
        "line-height" => LineHeight,
        "text-align" => TextAlign,
        "display" => Display,
        "flex-direction" => FlexDirection,
        "justify-content" | "justify-items" | "justify-self" => Justify,
        "align-items" | "align-self" | "align-content" => Align,
        "grid-template-columns"
        | "grid-template-rows"
        | "grid-auto-columns"
        | "grid-auto-rows"
        | "grid-column"
        | "grid-row"
        | "grid-auto-flow" => Grid,
        "border-style"
        | "border-top-style"
        | "border-right-style"
        | "border-bottom-style"
        | "border-left-style" => BorderStyle,
        "opacity" => Opacity,
        "overflow" | "overflow-x" | "overflow-y" => Overflow,
        "position" => Position,
        "z-index" => ZIndex,
        "object-fit" => ObjectFit,
        "aspect-ratio" => AspectRatio,
        "flex-grow" | "flex-shrink" => Number,
        "order" => Order,
        "flex-wrap" => FlexWrap,
        "white-space" => WhiteSpace,
        "visibility" => Visibility,
        "text-decoration" => TextDecoration,
        "text-transform" => TextTransform,
        "border" | "outline" | "font" => Border,
        "box-shadow" => Shadow,
        "cursor" => Cursor,
        "transition" => Transition,
        _ => return None,
    })
}

fn validate_style_properties(
    properties: &serde_json::Map<String, Value>,
) -> Result<Vec<(String, String)>, ProjectError> {
    if properties.is_empty() {
        return Err(ProjectError::new(
            "invalid_style",
            "style operation requires at least one property",
        ));
    }
    if properties.len() > 32 {
        return Err(ProjectError::new(
            "invalid_style",
            "style operation is limited to 32 properties",
        ));
    }
    let mut updates = Vec::with_capacity(properties.len());
    for (raw_name, raw_value) in properties {
        let trimmed = raw_name.trim();
        let custom = trimmed.starts_with("--");
        // Custom property names are case-sensitive and may contain uppercase
        // letters. Lowercasing them would break existing `var(--Name)` uses.
        let name = if custom {
            trimmed.to_owned()
        } else {
            trimmed.to_ascii_lowercase()
        };
        let valid_name = if custom {
            name.len() > 2
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        } else {
            !name.is_empty()
                && name.chars().all(|character| {
                    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
                })
        };
        if name.len() > 64 || !valid_name {
            return Err(ProjectError::new(
                "invalid_style",
                format!("invalid style property name {raw_name}"),
            ));
        }
        let value = raw_value.as_str().ok_or_else(|| {
            ProjectError::new(
                "invalid_style",
                format!("style value for {name} must be a string"),
            )
        })?;
        if value.len() > 512
            || value
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
        {
            return Err(ProjectError::new(
                "invalid_style",
                format!("style value for {name} exceeds the limit"),
            ));
        }
        let (value, important) = strip_important(value.trim());
        if value.is_empty() || !safe_css_value(value) {
            return Err(ProjectError::new(
                "invalid_style",
                format!("style value for {name} contains unsupported characters"),
            ));
        }
        let kind = if custom {
            StyleKind::Custom
        } else {
            style_kind(&name).ok_or_else(|| {
                ProjectError::new(
                    "invalid_style",
                    format!("unsupported style property {name}"),
                )
            })?
        };
        validate_style_value(kind, value)?;
        let value = if important {
            format!("{value} !important")
        } else {
            value.to_owned()
        };
        updates.push((name, value));
    }
    Ok(updates)
}

fn strip_important(value: &str) -> (&str, bool) {
    let trimmed = value.trim_end();
    const SUFFIX: &str = "!important";
    if trimmed.len() >= SUFFIX.len() {
        let cut = trimmed.len() - SUFFIX.len();
        if trimmed.is_char_boundary(cut) && trimmed[cut..].eq_ignore_ascii_case(SUFFIX) {
            return (trimmed[..cut].trim_end(), true);
        }
    }
    (trimmed, false)
}

fn safe_css_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if value.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                ';' | '{' | '}' | '<' | '>' | '\\' | '@' | '!' | '`'
            )
    }) {
        return false;
    }
    ![
        "url(",
        "expression(",
        "javascript:",
        "vbscript:",
        "behavior:",
        "-moz-binding",
        "image-set(",
        "cross-fade(",
        "src(",
        "/*",
        "*/",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        && balanced(value)
}

fn balanced(value: &str) -> bool {
    let mut parentheses = 0i64;
    let mut brackets = 0i64;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for character in value.chars() {
        if let Some(current) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == current {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '(' => parentheses += 1,
            ')' => parentheses -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            _ => {}
        }
        if parentheses < 0 || brackets < 0 {
            return false;
        }
    }
    parentheses == 0 && brackets == 0 && quote.is_none()
}

fn validate_style_value(kind: StyleKind, value: &str) -> Result<(), ProjectError> {
    use StyleKind::*;
    let valid = match kind {
        Color => valid_color(value),
        Length => valid_length(value),
        LengthList => valid_length_list(value),
        FontFamily => valid_font_family(value),
        FontWeight => valid_font_weight(value),
        LineHeight => valid_length(value) || value.parse::<f64>().is_ok_and(|number| number >= 0.0),
        TextAlign => matches!(
            value.to_ascii_lowercase().as_str(),
            "left"
                | "right"
                | "center"
                | "justify"
                | "start"
                | "end"
                | "inherit"
                | "initial"
                | "unset"
        ),
        Display => matches!(
            value.to_ascii_lowercase().as_str(),
            "block"
                | "inline"
                | "inline-block"
                | "flex"
                | "inline-flex"
                | "grid"
                | "inline-grid"
                | "none"
                | "contents"
                | "flow-root"
                | "table"
                | "table-row"
                | "table-cell"
                | "list-item"
                | "inherit"
                | "initial"
                | "unset"
        ),
        FlexDirection => matches!(
            value.to_ascii_lowercase().as_str(),
            "row" | "row-reverse" | "column" | "column-reverse" | "inherit" | "initial" | "unset"
        ),
        Justify => matches!(
            value.to_ascii_lowercase().as_str(),
            "flex-start"
                | "flex-end"
                | "center"
                | "space-between"
                | "space-around"
                | "space-evenly"
                | "start"
                | "end"
                | "stretch"
                | "normal"
                | "inherit"
                | "initial"
                | "unset"
        ),
        Align => matches!(
            value.to_ascii_lowercase().as_str(),
            "stretch"
                | "flex-start"
                | "flex-end"
                | "center"
                | "baseline"
                | "start"
                | "end"
                | "normal"
                | "inherit"
                | "initial"
                | "unset"
        ),
        Grid => valid_grid(value),
        BorderStyle => matches!(
            value.to_ascii_lowercase().as_str(),
            "none"
                | "hidden"
                | "dotted"
                | "dashed"
                | "solid"
                | "double"
                | "groove"
                | "ridge"
                | "inset"
                | "outset"
        ),
        Opacity => value
            .parse::<f64>()
            .is_ok_and(|number| (0.0..=1.0).contains(&number)),
        Overflow => matches!(
            value.to_ascii_lowercase().as_str(),
            "visible" | "hidden" | "clip" | "scroll" | "auto"
        ),
        Position => matches!(
            value.to_ascii_lowercase().as_str(),
            "static" | "relative" | "absolute" | "fixed" | "sticky"
        ),
        ZIndex => {
            value.parse::<i64>().is_ok()
                || matches!(value, "auto" | "inherit" | "initial" | "unset")
        }
        ObjectFit => matches!(
            value.to_ascii_lowercase().as_str(),
            "fill" | "contain" | "cover" | "none" | "scale-down"
        ),
        AspectRatio => valid_aspect_ratio(value),
        Number => value.parse::<f64>().is_ok_and(|number| number >= 0.0),
        Order => value.parse::<i64>().is_ok(),
        FlexWrap => matches!(
            value.to_ascii_lowercase().as_str(),
            "nowrap" | "wrap" | "wrap-reverse"
        ),
        WhiteSpace => matches!(
            value.to_ascii_lowercase().as_str(),
            "normal" | "nowrap" | "pre" | "pre-wrap" | "pre-line" | "break-spaces"
        ),
        Visibility => matches!(
            value.to_ascii_lowercase().as_str(),
            "visible" | "hidden" | "collapse"
        ),
        TextDecoration => value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | ' ')),
        TextTransform => matches!(
            value.to_ascii_lowercase().as_str(),
            "none" | "capitalize" | "uppercase" | "lowercase" | "full-width"
        ),
        Border => valid_shorthand(value),
        Shadow => valid_shorthand(value),
        Cursor => value
            .chars()
            .all(|character| character.is_ascii_lowercase() || matches!(character, '-' | ' ')),
        Transition => valid_shorthand(value),
        Custom => !value.trim().is_empty() && safe_css_value(value),
    };
    if valid {
        Ok(())
    } else {
        Err(ProjectError::new(
            "invalid_style",
            format!("unsupported style value {value}"),
        ))
    }
}

fn valid_color(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if let Some(digits) = lower.strip_prefix('#') {
        return matches!(digits.len(), 3 | 4 | 6 | 8)
            && digits.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    if let Some(inner) = lower.strip_prefix("var(") {
        return inner.starts_with("--") && lower.ends_with(')') && balanced(value);
    }
    for prefix in [
        "rgb(", "rgba(", "hsl(", "hsla(", "hwb(", "lab(", "lch(", "oklab(", "oklch(", "color(",
    ] {
        if lower.starts_with(prefix) {
            return lower.ends_with(')')
                && balanced(value)
                && value.chars().all(|character| {
                    character.is_ascii_alphanumeric()
                        || matches!(
                            character,
                            ' ' | ',' | '.' | '%' | '/' | '(' | ')' | '+' | '-' | '_'
                        )
                });
        }
    }
    lower
        .chars()
        .all(|character| character.is_ascii_lowercase())
        && !lower.is_empty()
        && lower.len() <= 32
}

fn valid_length(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "auto"
            | "none"
            | "inherit"
            | "initial"
            | "unset"
            | "revert"
            | "revert-layer"
            | "fit-content"
            | "min-content"
            | "max-content"
            | "stretch"
            | "normal"
    ) {
        return true;
    }
    if lower.starts_with("calc(")
        || lower.starts_with("clamp(")
        || lower.starts_with("min(")
        || lower.starts_with("max(")
        || lower.starts_with("var(")
    {
        return lower.ends_with(')')
            && balanced(value)
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(
                        character,
                        ' ' | ',' | '.' | '%' | '/' | '(' | ')' | '+' | '-' | '*'
                    )
            });
    }
    if lower == "0" {
        return true;
    }
    let split = lower
        .find(|character: char| {
            !(character.is_ascii_digit() || character == '.' || character == '-')
        })
        .unwrap_or(lower.len());
    let (number, unit) = lower.split_at(split);
    if number.is_empty() || number.parse::<f64>().is_err() {
        return false;
    }
    matches!(
        unit,
        "px" | "%"
            | "em"
            | "rem"
            | "vh"
            | "vw"
            | "vmin"
            | "vmax"
            | "ch"
            | "ex"
            | "lh"
            | "rlh"
            | "vi"
            | "vb"
            | "cm"
            | "mm"
            | "in"
            | "pt"
            | "pc"
            | "svh"
            | "svw"
            | "dvh"
            | "dvw"
            | "lvh"
            | "lvw"
            | "cqw"
            | "cqh"
            | "cqmin"
            | "cqmax"
            | "fr"
    )
}

fn valid_length_list(value: &str) -> bool {
    let mut parts = split_top_level_whitespace(value);
    if parts.is_empty() || parts.len() > 4 && !value.contains('/') {
        return false;
    }
    if value.contains('/') {
        parts = value
            .split('/')
            .flat_map(split_top_level_whitespace)
            .collect();
    }
    !parts.is_empty() && parts.iter().all(|part| valid_length(part))
}

fn split_top_level_whitespace(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i64;
    let mut start = None;
    for (index, character) in value.char_indices() {
        match character {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            _ => {}
        }
        if depth == 0 && character.is_whitespace() {
            if let Some(start) = start.take() {
                parts.push(&value[start..index]);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(start) = start {
        parts.push(&value[start..]);
    }
    parts
}

fn valid_font_family(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, ' ' | ',' | '-' | '_' | '.' | '"' | '\'')
        })
}

fn valid_font_weight(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "normal" | "bold" | "bolder" | "lighter" | "inherit" | "initial" | "unset"
    ) || lower
        .parse::<u32>()
        .is_ok_and(|number| (100..=900).contains(&number) && number % 100 == 0)
}

fn valid_grid(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "none" | "auto" | "subgrid" | "masonry" | "inherit" | "initial" | "unset"
    ) {
        return true;
    }
    value.chars().all(|character| {
        character.is_ascii_alphanumeric()
            || matches!(
                character,
                ' ' | '-' | '_' | '.' | '%' | ',' | '(' | ')' | '[' | ']' | '/'
            )
    }) && balanced(value)
        && !value.trim().is_empty()
}

fn valid_aspect_ratio(value: &str) -> bool {
    if value.eq_ignore_ascii_case("auto") {
        return true;
    }
    let Some((left, right)) = value.split_once('/') else {
        return value.parse::<f64>().is_ok_and(|number| number > 0.0);
    };
    left.trim().parse::<f64>().is_ok_and(|number| number > 0.0)
        && right.trim().parse::<f64>().is_ok_and(|number| number > 0.0)
}

fn valid_shorthand(value: &str) -> bool {
    !value.trim().is_empty() && safe_css_value(value)
}

/// Standard property names are case-insensitive, but custom property names
/// (and therefore the `var(--Name)` references that use them) are not.
fn normalized_style_name(name: &str) -> String {
    if name.starts_with("--") {
        name.to_owned()
    } else {
        name.to_ascii_lowercase()
    }
}

fn merge_style(current: &str, updates: &[(String, String)]) -> Result<String, ProjectError> {
    if current
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(ProjectError::new(
            "invalid_style",
            "the existing style attribute contains an unsupported control character",
        ));
    }
    let mut declarations: Vec<(String, String)> = Vec::new();
    for segment in split_top_level(current, ';')? {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let (name, value) = split_declaration(segment)?;
        declarations.push((normalized_style_name(name), value.trim().to_owned()));
    }
    for (name, value) in updates {
        let mut found = false;
        for declaration in &mut declarations {
            if declaration.0 == *name {
                declaration.1.clone_from(value);
                found = true;
            }
        }
        if !found {
            declarations.push((name.clone(), value.clone()));
        }
    }
    let mut output = String::new();
    for (index, (name, value)) in declarations.iter().enumerate() {
        if index > 0 {
            output.push_str("; ");
        }
        output.push_str(name);
        output.push_str(": ");
        output.push_str(value);
    }
    Ok(output)
}

fn split_top_level(value: &str, separator: char) -> Result<Vec<&str>, ProjectError> {
    let mut parts = Vec::new();
    let mut depth = 0i64;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    for (index, character) in value.char_indices() {
        if let Some(current) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == current {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return Err(ProjectError::new(
                "invalid_style",
                "the existing style attribute has unbalanced parentheses",
            ));
        }
        if depth == 0 && character == separator {
            parts.push(&value[start..index]);
            start = index + character.len_utf8();
        }
    }
    if quote.is_some() || depth != 0 {
        return Err(ProjectError::new(
            "invalid_style",
            "the existing style attribute is not balanced",
        ));
    }
    parts.push(&value[start..]);
    Ok(parts)
}

fn split_declaration(segment: &str) -> Result<(&str, &str), ProjectError> {
    let mut depth = 0i64;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, character) in segment.char_indices() {
        if let Some(current) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == current {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '(' => depth += 1,
            ')' => depth -= 1,
            ':' if depth == 0 => {
                let name = segment[..index].trim();
                if name.is_empty() {
                    break;
                }
                return Ok((name, &segment[index + 1..]));
            }
            _ => {}
        }
    }
    Err(ProjectError::new(
        "invalid_style",
        "the existing style attribute contains a declaration without a property",
    ))
}

// ---------------------------------------------------------------------------
// Component catalog
// ---------------------------------------------------------------------------

const IMAGE_DATA: &str = "data:image/svg+xml;charset=utf-8,%3Csvg%20xmlns%3D%27http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%27%20viewBox%3D%270%200%20640%20360%27%3E%3Cdefs%3E%3ClinearGradient%20id%3D%27g%27%20x1%3D%270%27%20y1%3D%270%27%20x2%3D%271%27%20y2%3D%271%27%3E%3Cstop%20offset%3D%270%27%20stop-color%3D%27%23ff8a3d%27%2F%3E%3Cstop%20offset%3D%271%27%20stop-color%3D%27%2338bdf8%27%2F%3E%3C%2FlinearGradient%3E%3C%2Fdefs%3E%3Crect%20width%3D%27640%27%20height%3D%27360%27%20fill%3D%27%2320242c%27%2F%3E%3Crect%20x%3D%2740%27%20y%3D%2740%27%20width%3D%27560%27%20height%3D%27280%27%20rx%3D%2724%27%20fill%3D%27url(%23g)%27%20opacity%3D%27.85%27%2F%3E%3C%2Fsvg%3E";

const COMPONENT_TEXT: &str = "<p data-cayw-component=\"text\" data-testid=\"cayw-component-text\" style=\"margin:0 0 1em;font:inherit;color:inherit;\">新的文本内容</p>";
const COMPONENT_IMAGE: &str = "<figure data-cayw-component=\"image\" data-testid=\"cayw-component-image\" style=\"margin:0;display:grid;gap:.5rem;font:inherit;color:inherit;\"><img src=\"DATA_IMAGE\" alt=\"示例图片\" width=\"640\" height=\"360\" style=\"display:block;width:100%;height:auto;border-radius:.75rem;\"/><figcaption style=\"font-size:.875em;opacity:.72;\">图片说明</figcaption></figure>";
const COMPONENT_BUTTON: &str = "<button type=\"button\" data-cayw-component=\"button\" data-testid=\"cayw-component-button\" aria-label=\"操作按钮\" style=\"font:inherit;color:inherit;background:var(--cayw-accent,#ff8a3d);border:1px solid transparent;border-radius:.6rem;padding:.6em 1.1em;cursor:pointer;\">操作按钮</button>";
const COMPONENT_INPUT: &str = "<label data-cayw-component=\"input\" data-testid=\"cayw-component-input\" style=\"display:grid;gap:.35rem;font:inherit;color:inherit;\"><span>输入标签</span><input type=\"text\" name=\"cayw-input\" placeholder=\"请输入\" aria-label=\"输入内容\" style=\"font:inherit;color:inherit;background:transparent;border:1px solid currentColor;border-radius:.5rem;padding:.55em .7em;\"/></label>";
const COMPONENT_CARD: &str = "<article data-cayw-component=\"card\" data-testid=\"cayw-component-card\" style=\"display:grid;gap:.6rem;padding:1.25rem;border:1px solid rgba(128,128,128,.35);border-radius:1rem;background:transparent;font:inherit;color:inherit;\"><h3 style=\"margin:0;font-size:1.15rem;\">卡片标题</h3><p style=\"margin:0;opacity:.78;\">卡片说明文字，可继续修改内容。</p><a href=\"#\" style=\"color:inherit;font-weight:600;\">了解更多</a></article>";
const COMPONENT_CONTAINER: &str = "<div data-cayw-component=\"container\" data-testid=\"cayw-component-container\" style=\"display:block;padding:1.5rem;border:1px dashed rgba(128,128,128,.4);border-radius:.9rem;font:inherit;color:inherit;\">容器内容</div>";
const COMPONENT_GRID: &str = "<div data-cayw-component=\"grid\" data-testid=\"cayw-component-grid\" style=\"display:grid;grid-template-columns:repeat(auto-fit,minmax(12rem,1fr));gap:1rem;font:inherit;color:inherit;\"><div style=\"padding:1rem;border:1px solid rgba(128,128,128,.3);border-radius:.75rem;\">网格项一</div><div style=\"padding:1rem;border:1px solid rgba(128,128,128,.3);border-radius:.75rem;\">网格项二</div><div style=\"padding:1rem;border:1px solid rgba(128,128,128,.3);border-radius:.75rem;\">网格项三</div></div>";
const COMPONENT_HERO: &str = "<style data-cayw-component-css=\"motion\">@keyframes cayw-fade-up{from{opacity:0;transform:translateY(10px)}to{opacity:1;transform:none}}[data-cayw-motion]{animation:cayw-fade-up .5s ease both}@media (prefers-reduced-motion: reduce){[data-cayw-motion]{animation:none}}</style><section data-cayw-component=\"hero\" data-testid=\"cayw-component-hero\" data-cayw-motion style=\"display:grid;gap:1rem;padding:clamp(1.5rem,5vw,3.5rem);border-radius:1.25rem;background:linear-gradient(135deg,rgba(255,138,61,.14),rgba(56,189,248,.12));font:inherit;color:inherit;\"><p style=\"margin:0;letter-spacing:.14em;text-transform:uppercase;font-size:.75rem;opacity:.7;\">New section</p><h2 style=\"margin:0;font-size:clamp(1.6rem,4vw,2.6rem);\">新的主视觉标题</h2><p style=\"margin:0;max-width:36rem;opacity:.82;\">说明这段内容的价值，并继续在编辑器中修改文字和样式。</p><div style=\"display:flex;gap:.75rem;flex-wrap:wrap;\"><a href=\"#\" style=\"display:inline-flex;align-items:center;padding:.65em 1.1em;border-radius:.6rem;background:var(--cayw-accent,#ff8a3d);color:#161616;text-decoration:none;font-weight:600;\">开始使用</a><a href=\"#\" style=\"display:inline-flex;align-items:center;padding:.65em 1.1em;border-radius:.6rem;border:1px solid currentColor;color:inherit;text-decoration:none;\">了解更多</a></div></section>";
const COMPONENT_BENTO: &str = "<style data-cayw-component-css=\"motion\">@keyframes cayw-fade-up{from{opacity:0;transform:translateY(10px)}to{opacity:1;transform:none}}[data-cayw-motion]{animation:cayw-fade-up .5s ease both}@media (prefers-reduced-motion: reduce){[data-cayw-motion]{animation:none}}</style><section data-cayw-component=\"bento\" data-testid=\"cayw-component-bento\" style=\"display:grid;grid-template-columns:repeat(auto-fit,minmax(12rem,1fr));gap:1rem;font:inherit;color:inherit;\"><article data-cayw-motion style=\"grid-column:1/-1;padding:1.4rem;border:1px solid rgba(128,128,128,.35);border-radius:1rem;\"><h2 style=\"margin:0 0 .5rem;font-size:1.4rem;\">重点内容</h2><p style=\"margin:0;opacity:.8;\">用一张宽卡片说明主要能力，再配合下面的小卡片。</p></article><article data-cayw-motion style=\"padding:1.1rem;border:1px solid rgba(128,128,128,.3);border-radius:.85rem;\"><h3 style=\"margin:0 0 .35rem;\">能力一</h3><p style=\"margin:0;opacity:.75;\">简短说明。</p></article><article data-cayw-motion style=\"padding:1.1rem;border:1px solid rgba(128,128,128,.3);border-radius:.85rem;\"><h3 style=\"margin:0 0 .35rem;\">能力二</h3><p style=\"margin:0;opacity:.75;\">简短说明。</p></article><article data-cayw-motion style=\"padding:1.1rem;border:1px solid rgba(128,128,128,.3);border-radius:.85rem;\"><h3 style=\"margin:0 0 .35rem;\">能力三</h3><p style=\"margin:0;opacity:.75;\">简短说明。</p></article></section>";
const COMPONENT_FAQ: &str = "<section data-cayw-component=\"faq\" data-testid=\"cayw-component-faq\" style=\"display:grid;gap:.6rem;font:inherit;color:inherit;\"><h2 style=\"margin:0 0 .4rem;font-size:1.4rem;\">常见问题</h2><details style=\"padding:.75rem 1rem;border:1px solid rgba(128,128,128,.35);border-radius:.7rem;\"><summary style=\"cursor:pointer;font-weight:600;\">问题一</summary><p style=\"margin:.6rem 0 0;opacity:.8;\">回答内容可以继续修改。</p></details><details style=\"padding:.75rem 1rem;border:1px solid rgba(128,128,128,.35);border-radius:.7rem;\"><summary style=\"cursor:pointer;font-weight:600;\">问题二</summary><p style=\"margin:.6rem 0 0;opacity:.8;\">回答内容可以继续修改。</p></details></section>";
const COMPONENT_CTA: &str = "<style data-cayw-component-css=\"motion\">@keyframes cayw-fade-up{from{opacity:0;transform:translateY(10px)}to{opacity:1;transform:none}}[data-cayw-motion]{animation:cayw-fade-up .5s ease both}@media (prefers-reduced-motion: reduce){[data-cayw-motion]{animation:none}}</style><section data-cayw-component=\"cta\" data-testid=\"cayw-component-cta\" data-cayw-motion style=\"display:grid;gap:.85rem;justify-items:center;text-align:center;padding:clamp(1.5rem,5vw,3rem);border-radius:1.25rem;border:1px solid rgba(128,128,128,.3);font:inherit;color:inherit;\"><h2 style=\"margin:0;font-size:clamp(1.4rem,3.5vw,2.2rem);\">准备好开始了吗？</h2><p style=\"margin:0;max-width:32rem;opacity:.8;\">用一个明确的行动按钮结束这段内容。</p><a href=\"#\" style=\"display:inline-flex;align-items:center;padding:.7em 1.3em;border-radius:.6rem;background:var(--cayw-accent,#ff8a3d);color:#161616;text-decoration:none;font-weight:600;\">立即开始</a></section>";

fn component_snippet(name: &str) -> Result<&'static str, ProjectError> {
    Ok(match name {
        "text" => COMPONENT_TEXT,
        "image" => COMPONENT_IMAGE,
        "button" => COMPONENT_BUTTON,
        "input" => COMPONENT_INPUT,
        "card" => COMPONENT_CARD,
        "container" => COMPONENT_CONTAINER,
        "grid" => COMPONENT_GRID,
        "hero" => COMPONENT_HERO,
        "bento" => COMPONENT_BENTO,
        "faq" => COMPONENT_FAQ,
        "cta" => COMPONENT_CTA,
        _ => {
            return Err(ProjectError::new(
                "invalid_operation",
                format!("unknown component {name}"),
            ));
        }
    })
}

/// Returns the component snippet with the original image data URL substituted.
///
/// This is exposed for the server's palette so the same local asset is used in
/// both insert operations and catalog previews.
pub fn component_html(name: &str) -> Result<String, ProjectError> {
    Ok(component_snippet(name)?.replace("DATA_IMAGE", IMAGE_DATA))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_spans_are_exact() {
        let source = "<div data-x=\"a>b\" class='c'>Text</div>";
        let document = parse_document(source).unwrap();
        assert_eq!(document.nodes.len(), 1);
        let node = &document.nodes[0];
        assert_eq!(node.tag, "div");
        assert_eq!(
            &source[node.start_tag_span.clone()],
            "<div data-x=\"a>b\" class='c'>"
        );
        assert!(node.editable);
        let attributes = scan_attributes(source, node.start_tag_span.clone()).unwrap();
        assert_eq!(attributes.len(), 2);
        assert_eq!(&source[attributes[0].value_range.clone().unwrap()], "a>b");
    }

    #[test]
    fn script_and_style_are_not_editable() {
        let source = "<html><body><script>var x = '<div>';</script><style>.a{color:red}</style><p>ok</p></body></html>";
        let document = parse_document(source).unwrap();
        let script = document
            .nodes
            .iter()
            .find(|node| node.tag == "script")
            .unwrap();
        assert!(!script.editable);
        assert!(!script.inject);
        let paragraph = document.nodes.iter().find(|node| node.tag == "p").unwrap();
        assert!(paragraph.editable);
    }

    #[test]
    fn template_markers_make_a_region_read_only() {
        let source = "<div><p>static</p><div>{{ item }}</div></div>";
        let document = parse_document(source).unwrap();
        let static_node = document.nodes.iter().find(|node| node.tag == "p").unwrap();
        assert!(static_node.editable);
        let dynamic = document
            .nodes
            .iter()
            .filter(|node| node.tag == "div")
            .find(|node| node.dynamic)
            .unwrap();
        assert!(!dynamic.editable);
        assert_eq!(dynamic.reason.as_deref(), Some("dynamic_region"));
    }

    #[test]
    fn injection_is_not_written_back_into_the_draft() {
        let source = "<main><h1>Hello</h1></main>";
        let document = parse_document(source).unwrap();
        let html = instrument(source, &document).unwrap();
        assert!(html.contains("data-cayw-node=\"n"));
        assert!(!source.contains("data-cayw-node"));
    }

    #[test]
    fn style_merge_preserves_unrelated_declarations() {
        let merged = merge_style(
            "display: grid; color: red; --x: 1",
            &[
                ("color".into(), "blue".into()),
                ("gap".into(), "1rem".into()),
            ],
        )
        .unwrap();
        assert_eq!(merged, "display: grid; color: blue; --x: 1; gap: 1rem");
    }

    #[test]
    fn unsafe_css_values_are_rejected() {
        for value in [
            "url(http://x)",
            "red; background: url(x)",
            "expression(1)",
            "a}",
        ] {
            assert!(!safe_css_value(value), "{value} should be unsafe");
        }
        assert!(safe_css_value("rgb(1 2 3 / 50%)"));
        assert!(safe_css_value("clamp(1rem, 2vw, 3rem)"));
    }
}
