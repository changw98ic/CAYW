//! Multi-file React and Vue projects backed by the framework companion.
//!
//! The Host is authoritative for source bytes and writes. The companion only
//! proposes a bounded file manifest, maps documents and computes structured
//! patches; every read and write in this module goes through the public
//! `cayw-host` SDK with an exact manifest permission gate. Draft state never
//! reaches disk before [`FrameworkProject::save`], and that write is a
//! per-file compare-and-swap.
//!
//! IDs and revisions are opaque. Every source change produces a new global
//! draft revision, so an operation that was computed for an older draft fails
//! before any state is replaced.

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{Host, HostConfig, PermissionGate};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

use crate::audit::{AuditOpenError, DurableAudit};
use crate::companion::Companion;
use crate::project::{ProjectError, Snapshot, SourceFile, SourceNode, component_html};

const MAX_FILES: usize = 256;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 20_000;
const MAX_HISTORY: usize = 100;
const MAX_HISTORY_BYTES: usize = 32 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_PATH_COMPONENTS: usize = 64;

const SOURCE_EXTENSIONS: &[&str] = &["js", "jsx", "ts", "tsx", "vue"];
const PAGE_EXTENSIONS: &[&str] = &["html", "htm"];
const OPERATION_KINDS: &[&str] = &[
    "style",
    "text",
    "attribute",
    "insert",
    "move",
    "duplicate",
    "delete",
];

/// Digest of the saved source of every file since open or the last save.
struct FileState {
    saved: String,
    sha256: String,
}

/// One bounded single-file undo record.
#[derive(Clone)]
struct HistoryEntry {
    path: String,
    before: String,
    after: String,
}

#[derive(Clone, Debug)]
struct Selection {
    path: String,
    start: usize,
}

struct State {
    /// Authoritative saved bytes and their digests, keyed by relative path.
    baseline: BTreeMap<String, FileState>,
    /// Current in-memory draft for every manifest file.
    draft: BTreeMap<String, String>,
    /// Validated source nodes for `draft`.
    nodes: Vec<SourceNode>,
    undo: Vec<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    selection: Option<Selection>,
    unknown: bool,
    conflicted: bool,
}

/// One open framework project and its bounded in-memory draft.
pub struct FrameworkProject {
    page: String,
    root: PathBuf,
    framework: String,
    host: Host,
    companion: Arc<Companion>,
    _audit: Arc<DurableAudit>,
    state: Mutex<State>,
}

impl fmt::Debug for FrameworkProject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrameworkProject")
            .field("page", &self.page)
            .field("root", &self.root)
            .field("framework", &self.framework)
            .finish_non_exhaustive()
    }
}

impl FrameworkProject {
    /// Opens a React or Vue project. The companion proposes the file manifest;
    /// the Host revalidates every path and supplies the actual bytes.
    ///
    /// `root` must be canonical and `state_dir` must already be resolved
    /// outside the project tree; [`crate::start`] is the only caller and owns
    /// that normalization. Crate-visible because [`Companion`] stays
    /// crate-private; external tests exercise this path through
    /// [`crate::start`].
    pub(crate) async fn open(
        root: &Path,
        page: &str,
        state_dir: &Path,
        framework: &str,
        companion: Arc<Companion>,
    ) -> Result<Self, ProjectError> {
        // `crate::start` is the one place that canonicalizes the project root
        // and resolves a state directory outside that tree. This function is
        // crate-private and its only caller is `start`, so it keeps just the
        // checks its own inputs need.
        let framework = framework.to_owned();
        let page = normalize_page(page)?;
        let root_text = root.to_str().ok_or_else(|| {
            ProjectError::new(
                "invalid_project",
                "project root must be a UTF-8 path for the companion",
            )
        })?;
        let audit = Arc::new(DurableAudit::open(state_dir).map_err(audit_error)?);

        let scan = companion
            .call(
                "scan",
                json!({
                    "root": root_text,
                    "page": page.as_str(),
                    "framework": framework.as_str(),
                }),
            )
            .await?;
        let paths = parse_scan(&scan, &page)?;
        let gate = Arc::new(FrameworkGate {
            paths: paths.iter().cloned().collect(),
        });
        let host = Host::new(root, gate, audit.clone(), host_config()).map_err(|error| {
            ProjectError::new(
                "host_unavailable",
                format!("could not open the host: {error}"),
            )
        })?;

        let mut baseline = BTreeMap::new();
        let mut draft = BTreeMap::new();
        let mut total = 0usize;
        for path in paths {
            let (source, sha256) = read_source(&host, &path).await?;
            if source.len() > MAX_FILE_BYTES {
                return Err(ProjectError::new(
                    "source_too_large",
                    format!("{path} exceeds the {MAX_FILE_BYTES}-byte editor limit"),
                ));
            }
            total = total.checked_add(source.len()).ok_or_else(|| {
                ProjectError::new("source_too_large", "project source size overflow")
            })?;
            if total > MAX_TOTAL_BYTES {
                return Err(ProjectError::new(
                    "source_too_large",
                    format!("project source exceeds the {MAX_TOTAL_BYTES}-byte editor limit"),
                ));
            }
            baseline.insert(
                path.clone(),
                FileState {
                    saved: source.clone(),
                    sha256,
                },
            );
            draft.insert(path, source);
        }
        if draft.is_empty() {
            return Err(ProjectError::new(
                "invalid_project",
                "the companion proposed no source files",
            ));
        }
        let revision = draft_revision(&draft);
        let nodes = parse_documents(&companion, &framework, &draft, &revision).await?;

        Ok(Self {
            page,
            root: root.to_path_buf(),
            framework,
            host,
            companion,
            _audit: audit,
            state: Mutex::new(State {
                baseline,
                draft,
                nodes,
                undo: Vec::new(),
                redo: Vec::new(),
                selection: None,
                unknown: false,
                conflicted: false,
            }),
        })
    }

    /// Current flat mapping over every draft file.
    pub async fn snapshot(&self) -> Result<Snapshot, ProjectError> {
        let state = self.state.lock().await;
        build_snapshot(&self.page, &self.framework, &state)
    }

    /// Applies one structured operation through the companion and commits the
    /// candidate only after it parses, validates and the preview acknowledges
    /// the new draft.
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
        if kind == "undo" || kind == "redo" {
            history_step(&mut state, kind == "redo", &self.companion, &self.framework).await?;
            return build_snapshot(&self.page, &self.framework, &state);
        }
        if !OPERATION_KINDS.contains(&kind) {
            return Err(ProjectError::new(
                "unsupported_operation",
                format!("operation kind {kind} is not supported"),
            ));
        }
        let primary = operation.get("id").and_then(Value::as_str);
        if let Some(id) = primary {
            require_node(&state.nodes, id)?;
        }
        if kind == "insert"
            && let Some(target) = operation.get("target").and_then(Value::as_str)
        {
            require_node(&state.nodes, target)?;
        }
        if kind == "move" {
            let target = operation
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProjectError::new("invalid_operation", "move requires a target node")
                })?;
            let target = require_node(&state.nodes, target)?;
            let from_id = primary
                .ok_or_else(|| ProjectError::new("invalid_operation", "move requires a node id"))?;
            let from = require_node(&state.nodes, from_id)?;
            if from.file != target.file {
                return Err(ProjectError::new(
                    "cross_file_move",
                    "move must stay inside one source file",
                ));
            }
        }
        let component = if kind == "insert" {
            let name = operation
                .get("component")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProjectError::new("invalid_operation", "insert requires a component name")
                })?;
            Some(component_html(name)?)
        } else {
            None
        };

        let revision = draft_revision(&state.draft);
        let mut params = json!({
            "framework": self.framework.as_str(),
            "files": files_json(&state.draft),
            "revision": revision,
            "operation": operation,
        });
        if let Some(html) = component {
            params["componentHtml"] = Value::String(html);
        }
        let result = self
            .companion
            .call("edit", params)
            .await
            .map_err(companion_edit_error)?;
        let edit = EditResult::parse(&result, &state.draft)?;

        let mut candidate = state.draft.clone();
        if let Some(path) = &edit.path {
            candidate.insert(path.clone(), edit.source.clone());
        }
        enforce_limits(&candidate)?;
        let next_revision = draft_revision(&candidate);
        let nodes =
            parse_documents(&self.companion, &self.framework, &candidate, &next_revision).await?;
        let selection = if kind == "delete" {
            None
        } else {
            match edit.selection {
                Some(selection) => {
                    validate_selection(&selection, &candidate, &nodes, edit.path.as_deref())?;
                    Some(selection)
                }
                None => None,
            }
        };
        sync_draft(&self.companion, &self.framework, &candidate, &next_revision).await?;

        if let Some(path) = &edit.path {
            let before = state
                .draft
                .get(path)
                .expect("edited path is in the draft")
                .clone();
            if before != edit.source {
                push_history(
                    &mut state.undo,
                    HistoryEntry {
                        path: path.clone(),
                        before,
                        after: edit.source.clone(),
                    },
                );
                state.redo.clear();
            }
        }
        state.draft = candidate;
        state.nodes = nodes;
        state.selection = selection;
        build_snapshot(&self.page, &self.framework, &state)
    }

    /// Writes every dirty file with a per-file compare-and-swap.
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
        if !expected_sha256.eq_ignore_ascii_case(&baseline_revision(&state.baseline)) {
            return Err(stale_source());
        }
        if self.host.outcome_unknown() {
            state.unknown = true;
            return Err(reconciliation(
                "the host requires reconciliation before another write",
            ));
        }

        let paths: Vec<String> = state.baseline.keys().cloned().collect();
        let host = self.operation_host(paths.iter().cloned())?;
        let refreshed = match read_all(&host, paths.iter()).await {
            Ok(files) => files,
            Err(error) => {
                if error.outcome_unknown {
                    state.unknown = true;
                }
                return Err(error);
            }
        };
        if refreshed
            .iter()
            .any(|(path, file)| state.baseline[path].sha256 != file.sha256)
        {
            // Report the current disk digest for diagnostics, but keep the
            // stale draft and lock edits until reload replaces it.
            state.baseline = refreshed;
            state.conflicted = true;
            return Err(stale_source());
        }

        let dirty: Vec<String> = state
            .draft
            .iter()
            .filter(|(path, draft)| {
                state
                    .baseline
                    .get(path.as_str())
                    .is_none_or(|file| file.saved != **draft)
            })
            .map(|(path, _)| path.clone())
            .collect();
        if dirty.is_empty() {
            return build_snapshot(&self.page, &self.framework, &state);
        }

        let mut written = Vec::new();
        for path in dirty {
            let content = state
                .draft
                .get(&path)
                .expect("dirty path is in the draft")
                .clone();
            let expected = state
                .baseline
                .get(&path)
                .expect("manifest path has a baseline")
                .sha256
                .clone();
            match write_source(&host, &path, &content, &expected).await {
                Ok(sha256) => {
                    let local = sha256_hex(content.as_bytes());
                    if !sha256.eq_ignore_ascii_case(&local) {
                        state.unknown = true;
                        if written.is_empty() {
                            return Err(ProjectError::unknown(
                                "save_outcome_unknown",
                                format!(
                                    "the host digest for {path} does not match the draft; verify the source"
                                ),
                            ));
                        }
                        return Err(ProjectError::unknown(
                            "partial_save",
                            format!(
                                "source files written before the failed digest check: {}; verify {}",
                                written.join(", "),
                                path
                            ),
                        ));
                    }
                    let file = state
                        .baseline
                        .get_mut(&path)
                        .expect("manifest path has a baseline");
                    file.saved = content;
                    file.sha256 = sha256.to_ascii_lowercase();
                    written.push(path);
                }
                Err(error) => {
                    if !written.is_empty() {
                        state.unknown = true;
                        return Err(ProjectError::unknown(
                            "partial_save",
                            format!(
                                "source files written: {}; then {path} failed: {}",
                                written.join(", "),
                                error.message
                            ),
                        ));
                    }
                    if error.outcome_unknown {
                        state.unknown = true;
                        return Err(ProjectError::unknown("save_outcome_unknown", error.message));
                    }
                    if is_stale_error(&error.message) {
                        state.conflicted = true;
                        return Err(stale_source());
                    }
                    return Err(ProjectError::new("save_failed", error.message));
                }
            }
        }
        build_snapshot(&self.page, &self.framework, &state)
    }

    /// Rescans the manifest, reads every current source, parses it and
    /// acknowledges the preview. The old draft, manifest and gate stay intact
    /// until all steps succeed, so a failed reload changes no source state.
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
        let root = self.root.to_str().ok_or_else(|| {
            ProjectError::new(
                "invalid_project",
                "project root must be a UTF-8 path for the companion",
            )
        })?;
        let scan = self
            .companion
            .call(
                "scan",
                json!({
                    "root": root,
                    "page": self.page.as_str(),
                    "framework": self.framework.as_str(),
                }),
            )
            .await?;
        let paths = parse_scan(&scan, &self.page)?;

        // Read the new manifest through a fresh gate over the same held root
        // handle. The old gate, baseline and draft stay untouched until every
        // read, parse and sync succeeds.
        let host = self.operation_host(paths.iter().cloned())?;
        let refreshed = match read_all(&host, paths.iter()).await {
            Ok(files) => files,
            Err(error) => {
                if error.outcome_unknown {
                    state.unknown = true;
                }
                return Err(error);
            }
        };
        if host.outcome_unknown() {
            state.unknown = true;
            return Err(reconciliation(
                "the host requires reconciliation before another read",
            ));
        }
        let draft: BTreeMap<String, String> = refreshed
            .iter()
            .map(|(path, file)| (path.clone(), file.saved.clone()))
            .collect();
        let revision = draft_revision(&draft);
        let nodes = parse_documents(&self.companion, &self.framework, &draft, &revision).await?;
        sync_draft(&self.companion, &self.framework, &draft, &revision).await?;

        state.baseline = refreshed;
        state.draft = draft;
        state.nodes = nodes;
        state.undo.clear();
        state.redo.clear();
        state.selection = None;
        state.conflicted = false;
        build_snapshot(&self.page, &self.framework, &state)
    }

    /// True when any draft file differs from its saved bytes.
    pub async fn dirty(&self) -> bool {
        let state = self.state.lock().await;
        let mut dirty = false;
        for (path, draft) in &state.draft {
            if state.baseline[path].saved != *draft {
                dirty = true;
                break;
            }
        }
        dirty
    }

    /// `(can_undo, can_redo)` for callers that keep no own counters.
    pub async fn history_flags(&self) -> (bool, bool) {
        let state = self.state.lock().await;
        (!state.undo.is_empty(), !state.redo.is_empty())
    }

    /// One explicit user operation gets its own Host admission and byte/call
    /// budget over the same held directory handle. Unknown outcomes remain a
    /// project-level sticky flag; a new scope never clears them.
    fn operation_host(&self, paths: impl Iterator<Item = String>) -> Result<Host, ProjectError> {
        operation_scope(&self.host, &self._audit, paths)
    }

    /// Page path relative to the project root.
    pub fn page(&self) -> &str {
        &self.page
    }

    /// Project root, canonicalized when the project was opened.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `react` or `vue`.
    pub fn framework(&self) -> &str {
        &self.framework
    }
}

async fn history_step(
    state: &mut State,
    redo: bool,
    companion: &Companion,
    framework: &str,
) -> Result<(), ProjectError> {
    let entry = if redo {
        state.redo.last().cloned()
    } else {
        state.undo.last().cloned()
    };
    let entry = entry.ok_or_else(|| {
        ProjectError::new("history_empty", "there is no operation to undo or redo")
    })?;
    let mut candidate = state.draft.clone();
    candidate.insert(
        entry.path.clone(),
        if redo {
            entry.after.clone()
        } else {
            entry.before.clone()
        },
    );
    let revision = draft_revision(&candidate);
    let nodes = parse_documents(companion, framework, &candidate, &revision).await?;
    sync_draft(companion, framework, &candidate, &revision).await?;

    if redo {
        state.redo.pop();
        push_history(&mut state.undo, entry);
    } else {
        state.undo.pop();
        push_history(&mut state.redo, entry);
    }
    state.draft = candidate;
    state.nodes = nodes;
    state.selection = None;
    Ok(())
}

/// Fresh Host admission, budget, safety and process lane over the held root.
/// `task_host` duplicates the open directory handle; it never reopens the root
/// by path, so an externally renamed or replaced path cannot redirect a read.
fn operation_scope(
    host: &Host,
    audit: &Arc<DurableAudit>,
    paths: impl Iterator<Item = String>,
) -> Result<Host, ProjectError> {
    let gate = Arc::new(FrameworkGate {
        paths: paths.collect(),
    });
    host.task_host(gate, audit.clone()).map_err(|error| {
        ProjectError::new(
            "host_unavailable",
            format!("could not open an operation scope: {}", error.message),
        )
    })
}

struct FrameworkGate {
    paths: BTreeSet<String>,
}

#[async_trait]
impl PermissionGate for FrameworkGate {
    async fn authorize(
        &self,
        capability: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        if cancel.is_cancelled() {
            return Err(CapabilityError::new("permission check was cancelled"));
        }
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::new("source capabilities require a path"))?;
        if !self.paths.contains(path) {
            return Err(CapabilityError::new(
                "the visual editor may only access scanned source files",
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
                        "visual editor rewrites require overwrite=true",
                    ));
                }
                let expected = arguments
                    .get("expected_sha256")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        CapabilityError::new("visual editor rewrites require expected_sha256")
                    })?;
                if !valid_sha256(expected) {
                    return Err(CapabilityError::new(
                        "expected_sha256 must be one SHA-256 hex digest",
                    ));
                }
                Ok(())
            }
            _ => Err(CapabilityError::new(
                "the visual editor has only source read and write access",
            )),
        }
    }
}

/// Limits for one explicit user operation. Every operation receives a fresh
/// [`Host::task_host`] scope over the same held root handle, so these bounds
/// apply per operation rather than accumulating until the editor restarts.
/// A 16 MiB project needs at most 32 MiB of raw arguments and results for one
/// full read or save; JSON escaping keeps the worst case well below this cap.
fn host_config() -> HostConfig {
    HostConfig {
        max_argument_bytes: 32 * 1024 * 1024,
        max_result_bytes: 32 * 1024 * 1024,
        max_total_bytes: 128 * 1024 * 1024,
        max_calls: 4096,
        max_file_bytes: MAX_FILE_BYTES,
        max_list_entries: 1,
        call_timeout: Duration::from_secs(60),
        data: None,
        workspace: None,
        ..HostConfig::default()
    }
}

async fn read_source(host: &Host, path: &str) -> Result<(String, String), ProjectError> {
    if host.outcome_unknown() {
        return Err(reconciliation(
            "the host requires reconciliation before another read",
        ));
    }
    let value = host
        .call("fs.read", json!({ "path": path }), CancellationToken::new())
        .await
        .map_err(|error| host_read_error(&error))?;
    let content = value
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ProjectError::new(
                "read_failed",
                format!("the host did not return {path} content"),
            )
        })?;
    let reported = value.get("sha256").and_then(Value::as_str).ok_or_else(|| {
        ProjectError::new(
            "read_failed",
            format!("the host did not return a digest for {path}"),
        )
    })?;
    let actual = sha256_hex(content.as_bytes());
    if !reported.eq_ignore_ascii_case(&actual) {
        return Err(ProjectError::new(
            "read_failed",
            format!("the host digest for {path} does not match the returned content"),
        ));
    }
    Ok((content.to_owned(), actual))
}

async fn read_all<'a>(
    host: &Host,
    paths: impl Iterator<Item = &'a String>,
) -> Result<BTreeMap<String, FileState>, ProjectError> {
    let mut files = BTreeMap::new();
    let mut total = 0usize;
    for path in paths {
        let (source, sha256) = read_source(host, path).await?;
        if source.len() > MAX_FILE_BYTES {
            return Err(ProjectError::new(
                "source_too_large",
                format!("{path} exceeds the {MAX_FILE_BYTES}-byte editor limit"),
            ));
        }
        total = total
            .checked_add(source.len())
            .ok_or_else(|| ProjectError::new("source_too_large", "project source size overflow"))?;
        if total > MAX_TOTAL_BYTES {
            return Err(ProjectError::new(
                "source_too_large",
                format!("project source exceeds the {MAX_TOTAL_BYTES}-byte editor limit"),
            ));
        }
        files.insert(
            path.clone(),
            FileState {
                saved: source,
                sha256,
            },
        );
    }
    Ok(files)
}

async fn write_source(
    host: &Host,
    path: &str,
    content: &str,
    expected_sha256: &str,
) -> Result<String, ProjectError> {
    if host.outcome_unknown() {
        return Err(reconciliation(
            "the host requires reconciliation before another write",
        ));
    }
    let value = host
        .call(
            "fs.write",
            json!({
                "path": path,
                "content": content,
                "overwrite": true,
                "expected_sha256": expected_sha256,
            }),
            CancellationToken::new(),
        )
        .await
        .map_err(|error| host_write_error(&error))?;
    value
        .get("sha256")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            ProjectError::unknown(
                "save_outcome_unknown",
                format!("the host did not return a digest for {path}"),
            )
        })
}

fn host_read_error(error: &CapabilityError) -> ProjectError {
    if error.outcome_unknown {
        ProjectError::unknown("read_outcome_unknown", error.message.clone())
    } else {
        ProjectError::new("read_failed", error.message.clone())
    }
}

fn host_write_error(error: &CapabilityError) -> ProjectError {
    if error.outcome_unknown {
        ProjectError::unknown("save_outcome_unknown", error.message.clone())
    } else {
        ProjectError::new("save_failed", error.message.clone())
    }
}

async fn parse_documents(
    companion: &Companion,
    framework: &str,
    files: &BTreeMap<String, String>,
    revision: &str,
) -> Result<Vec<SourceNode>, ProjectError> {
    let value = companion
        .call(
            "parse",
            json!({
                "framework": framework,
                "files": files_json(files),
                "revision": revision,
            }),
        )
        .await
        .map_err(companion_edit_error)?;
    let nodes = parse_nodes(&value, files)?;
    Ok(nodes)
}

async fn sync_draft(
    companion: &Companion,
    framework: &str,
    files: &BTreeMap<String, String>,
    revision: &str,
) -> Result<(), ProjectError> {
    let value = companion
        .call(
            "sync",
            json!({
                "framework": framework,
                "files": files_json(files),
                "revision": revision,
            }),
        )
        .await
        .map_err(companion_sync_error)?;
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(ProjectError::new(
            "sync_failed",
            "the companion did not acknowledge the new draft",
        ));
    }
    Ok(())
}

fn companion_edit_error(error: ProjectError) -> ProjectError {
    if error.outcome_unknown {
        ProjectError::new("companion_unavailable", error.message)
    } else if error.code.is_empty() || error.code == "parse_failed" {
        ProjectError::new("parser_unavailable", error.message)
    } else {
        error
    }
}

fn companion_sync_error(error: ProjectError) -> ProjectError {
    if error.outcome_unknown {
        ProjectError::new("companion_unavailable", error.message)
    } else if error.code.is_empty() {
        ProjectError::new("sync_failed", error.message)
    } else {
        error
    }
}

#[derive(Deserialize)]
struct MappedNode {
    id: String,
    file: String,
    start: usize,
    tag: String,
    #[serde(default)]
    parent: Option<String>,
    line: usize,
    editable: bool,
    #[serde(default)]
    reason: Option<String>,
}

fn parse_nodes(
    value: &Value,
    files: &BTreeMap<String, String>,
) -> Result<Vec<SourceNode>, ProjectError> {
    if let Some(instrumented) = value.get("instrumented") {
        let object = instrumented.as_object().ok_or_else(|| {
            ProjectError::new("invalid_response", "instrumented must be an object")
        })?;
        for (path, source) in object {
            if !files.contains_key(path) || !source.is_string() {
                return Err(ProjectError::new(
                    "invalid_response",
                    "instrumented output does not match the manifest",
                ));
            }
        }
    }
    let array = value
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| ProjectError::new("invalid_response", "the companion returned no nodes"))?;
    if array.len() > MAX_NODES {
        return Err(ProjectError::new(
            "too_many_nodes",
            format!("mapped node count exceeds {MAX_NODES}"),
        ));
    }
    let mut mapped = Vec::with_capacity(array.len());
    for value in array {
        let node: MappedNode = serde_json::from_value(value.clone()).map_err(|_| {
            ProjectError::new(
                "invalid_response",
                "the companion returned a malformed node",
            )
        })?;
        mapped.push(node);
    }
    validate_nodes(&mapped, files)?;
    Ok(mapped
        .into_iter()
        .map(|node| SourceNode {
            id: node.id,
            file: node.file,
            start: node.start,
            tag: node.tag,
            parent: node.parent,
            line: node.line,
            editable: node.editable,
            reason: node.reason,
        })
        .collect())
}

fn validate_nodes(
    nodes: &[MappedNode],
    files: &BTreeMap<String, String>,
) -> Result<(), ProjectError> {
    let mut ids = BTreeSet::new();
    let mut positions = BTreeSet::new();
    for node in nodes {
        if !valid_node_id(&node.id) {
            return Err(ProjectError::new(
                "invalid_response",
                format!("node id {} is not a valid opaque revision id", node.id),
            ));
        }
        if !ids.insert(node.id.as_str()) {
            return Err(ProjectError::new(
                "invalid_response",
                format!("node id {} is duplicated", node.id),
            ));
        }
        if !positions.insert((node.file.as_str(), node.start)) {
            return Err(ProjectError::new(
                "invalid_response",
                format!(
                    "node {} duplicates the source position of another node",
                    node.id
                ),
            ));
        }
        let source = files.get(&node.file).ok_or_else(|| {
            ProjectError::new(
                "invalid_response",
                format!("node {} names a file outside the manifest", node.id),
            )
        })?;
        let bytes = source.as_bytes();
        if node.start >= bytes.len()
            || bytes[node.start] != b'<'
            || !source.is_char_boundary(node.start)
        {
            return Err(ProjectError::new(
                "invalid_response",
                format!("node {} has an invalid source start", node.id),
            ));
        }
        if node.tag.is_empty()
            || node.tag.len() > 128
            || node
                .tag
                .chars()
                .any(|character| character.is_whitespace() || matches!(character, '<' | '>' | '"'))
        {
            return Err(ProjectError::new(
                "invalid_response",
                format!("node {} has an invalid tag", node.id),
            ));
        }
        if node.line == 0 {
            return Err(ProjectError::new(
                "invalid_response",
                format!("node {} has an invalid line", node.id),
            ));
        }
    }
    for node in nodes {
        if let Some(parent) = &node.parent {
            let Some(parent_node) = nodes.iter().find(|candidate| &candidate.id == parent) else {
                return Err(ProjectError::new(
                    "invalid_response",
                    format!("node {} names a missing parent", node.id),
                ));
            };
            if parent_node.file != node.file {
                return Err(ProjectError::new(
                    "invalid_response",
                    format!("node {} names a parent in another file", node.id),
                ));
            }
            if parent_node.start >= node.start {
                return Err(ProjectError::new(
                    "invalid_response",
                    format!("node {} is not nested inside its parent", node.id),
                ));
            }
        }
    }
    Ok(())
}

fn require_node<'a>(nodes: &'a [SourceNode], id: &str) -> Result<&'a SourceNode, ProjectError> {
    let node = nodes.iter().find(|node| node.id == id).ok_or_else(|| {
        ProjectError::new(
            "stale_node",
            "the operation targets a node outside the current revision",
        )
    })?;
    if !node.editable {
        return Err(ProjectError::new(
            "not_editable",
            format!(
                "<{}> at line {} is read-only: {}",
                node.tag,
                node.line,
                node.reason.as_deref().unwrap_or("unsupported region")
            ),
        ));
    }
    Ok(node)
}

fn valid_node_id(id: &str) -> bool {
    if id.len() > 128 {
        return false;
    }
    let Some(rest) = id.strip_prefix('n') else {
        return false;
    };
    let Some((ordinal, digest)) = rest.split_once('-') else {
        return false;
    };
    !ordinal.is_empty()
        && ordinal.len() <= 16
        && ordinal.bytes().all(|byte| byte.is_ascii_hexdigit())
        && digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Deserialize)]
struct Patch {
    path: String,
    start: usize,
    end: usize,
    before: String,
    replacement: String,
}

#[derive(Deserialize)]
struct SelectionResponse {
    path: String,
    start: usize,
}

#[derive(Deserialize)]
struct EditResponse {
    #[serde(default)]
    patches: Vec<Patch>,
    #[serde(default)]
    selection: Option<SelectionResponse>,
}

#[derive(Debug)]
struct EditResult {
    path: Option<String>,
    source: String,
    selection: Option<Selection>,
}

impl EditResult {
    fn parse(value: &Value, draft: &BTreeMap<String, String>) -> Result<Self, ProjectError> {
        let response: EditResponse = serde_json::from_value(value.clone()).map_err(|_| {
            ProjectError::new(
                "invalid_response",
                "the companion returned a malformed edit",
            )
        })?;
        let selection = response.selection.map(|selection| Selection {
            path: selection.path,
            start: selection.start,
        });
        if response.patches.is_empty() {
            return Ok(Self {
                path: None,
                source: String::new(),
                selection,
            });
        }
        let affected = response.patches[0].path.clone();
        if response.patches.iter().any(|patch| patch.path != affected) {
            return Err(ProjectError::new(
                "invalid_patch",
                "one operation may affect exactly one file",
            ));
        }
        let source = draft.get(&affected).ok_or_else(|| {
            ProjectError::new(
                "invalid_patch",
                format!("patch path {affected} is outside the manifest"),
            )
        })?;
        let mut order: Vec<&Patch> = response.patches.iter().collect();
        order.sort_by_key(|patch| (patch.start, patch.end));
        let mut previous_end = 0usize;
        let mut previous_start = None;
        for patch in &order {
            if patch.start > patch.end
                || patch.end > source.len()
                || !source.is_char_boundary(patch.start)
                || !source.is_char_boundary(patch.end)
                || source.get(patch.start..patch.end) != Some(patch.before.as_str())
            {
                return Err(ProjectError::new(
                    "stale_node",
                    "the patch no longer matches the current source",
                ));
            }
            if previous_start.is_some()
                && (patch.start < previous_end
                    || (patch.start == previous_end && patch.start == patch.end))
            {
                return Err(ProjectError::new(
                    "invalid_patch",
                    "patches overlap or stack at one position",
                ));
            }
            previous_end = patch.end;
            previous_start = Some(patch.start);
        }
        let mut result = source.clone();
        for patch in order.iter().rev() {
            result.replace_range(patch.start..patch.end, &patch.replacement);
        }
        if result.len() > MAX_FILE_BYTES {
            return Err(ProjectError::new(
                "source_too_large",
                format!("{affected} exceeds the {MAX_FILE_BYTES}-byte editor limit"),
            ));
        }
        Ok(Self {
            path: Some(affected),
            source: result,
            selection,
        })
    }
}

fn validate_selection(
    selection: &Selection,
    files: &BTreeMap<String, String>,
    nodes: &[SourceNode],
    edited: Option<&str>,
) -> Result<(), ProjectError> {
    if edited.is_some_and(|path| path != selection.path) {
        return Err(ProjectError::new(
            "invalid_response",
            "the selection is outside the edited file",
        ));
    }
    let source = files.get(&selection.path).ok_or_else(|| {
        ProjectError::new(
            "invalid_response",
            "the selection names a file outside the manifest",
        )
    })?;
    if selection.start > source.len()
        || !source.is_char_boundary(selection.start)
        || source.as_bytes().get(selection.start).copied() != Some(b'<')
    {
        return Err(ProjectError::new(
            "invalid_response",
            "the selection has an invalid source start",
        ));
    }
    if !nodes
        .iter()
        .any(|node| node.file == selection.path && node.start == selection.start)
    {
        return Err(ProjectError::new(
            "invalid_response",
            "the selection does not match a mapped node",
        ));
    }
    Ok(())
}

fn parse_scan(value: &Value, page: &str) -> Result<Vec<String>, ProjectError> {
    let paths = value
        .get("paths")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProjectError::new("invalid_response", "the companion returned no source paths")
        })?;
    if paths.len() > MAX_FILES {
        return Err(ProjectError::new(
            "invalid_project",
            format!("the project has more than {MAX_FILES} source files"),
        ));
    }
    let mut unique = BTreeSet::new();
    for path in paths {
        let path = path.as_str().ok_or_else(|| {
            ProjectError::new("invalid_response", "a source path is not a string")
        })?;
        let path = normalize_source_path(path, page)?;
        if !unique.insert(path) {
            return Err(ProjectError::new(
                "invalid_response",
                "the companion proposed one path twice",
            ));
        }
    }
    if !unique.iter().any(|candidate| candidate.as_str() == page) {
        return Err(ProjectError::new(
            "invalid_project",
            "the scanned source paths do not include the page",
        ));
    }
    Ok(unique.into_iter().collect())
}

fn normalize_source_path(path: &str, page: &str) -> Result<String, ProjectError> {
    let normalized = normalize_portable_path(path, "source path")?;
    if normalized == page {
        return Ok(normalized);
    }
    let extension = normalized
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .ok_or_else(|| {
            ProjectError::new(
                "invalid_project",
                format!("source path {normalized} has no supported extension"),
            )
        })?;
    if !SOURCE_EXTENSIONS.contains(&extension.as_str()) {
        return Err(ProjectError::new(
            "invalid_project",
            format!("source path {normalized} has no supported extension"),
        ));
    }
    Ok(normalized)
}

fn normalize_page(page: &str) -> Result<String, ProjectError> {
    let normalized = normalize_portable_path(page, "page")?;
    let extension = normalized
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .ok_or_else(|| {
            ProjectError::new("invalid_page", "page must name a supported entry file")
        })?;
    if !PAGE_EXTENSIONS.contains(&extension.as_str())
        && !SOURCE_EXTENSIONS.contains(&extension.as_str())
    {
        return Err(ProjectError::new(
            "invalid_page",
            "page must name a supported entry file",
        ));
    }
    Ok(normalized)
}

fn normalize_portable_path(path: &str, label: &str) -> Result<String, ProjectError> {
    let path = path.strip_prefix("./").unwrap_or(path);
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.contains(['\\', ':', '\0'])
        || path.chars().any(char::is_control)
    {
        return Err(ProjectError::new(
            "invalid_path",
            format!("{label} must be a portable relative path"),
        ));
    }
    let components: Vec<&str> = path.split('/').collect();
    if components.len() > MAX_PATH_COMPONENTS {
        return Err(ProjectError::new(
            "invalid_path",
            format!("{label} has too many components"),
        ));
    }
    for component in &components {
        if component.is_empty()
            || *component == "."
            || *component == ".."
            || component.len() > 255
            || component.starts_with('.')
        {
            return Err(ProjectError::new(
                "invalid_path",
                format!("{label} must be a portable relative path"),
            ));
        }
    }
    Ok(path.to_owned())
}

fn build_snapshot(page: &str, framework: &str, state: &State) -> Result<Snapshot, ProjectError> {
    let path = snapshot_path(page, state);
    let source = state
        .draft
        .get(&path)
        .ok_or_else(|| ProjectError::new("invalid_project", "the selected file is absent"))?
        .clone();
    let revision = draft_revision(&state.draft);
    let sha256 = baseline_revision(&state.baseline);
    let selected_id = state.selection.as_ref().and_then(|selection| {
        state
            .nodes
            .iter()
            .find(|node| node.file == selection.path && node.start == selection.start)
            .map(|node| node.id.clone())
    });
    let files = state
        .draft
        .iter()
        .map(|(path, source)| SourceFile {
            path: path.clone(),
            sha256: state
                .baseline
                .get(path)
                .expect("draft path has a baseline")
                .sha256
                .clone(),
            source: source.clone(),
        })
        .collect();
    Ok(Snapshot {
        path,
        sha256,
        adapter: framework.to_owned(),
        revision,
        files,
        html: String::new(),
        source,
        nodes: state.nodes.clone(),
        selected_id,
    })
}

fn snapshot_path(page: &str, state: &State) -> String {
    if let Some(selection) = &state.selection {
        return selection.path.clone();
    }
    let mut first_with_nodes = None;
    for path in state.draft.keys() {
        if state.nodes.iter().any(|node| &node.file == path) {
            if path.as_str() != page {
                return path.clone();
            }
            if first_with_nodes.is_none() {
                first_with_nodes = Some(path.clone());
            }
        }
    }
    first_with_nodes.unwrap_or_else(|| {
        state
            .draft
            .keys()
            .find(|path| path.as_str() != page)
            .or_else(|| state.draft.keys().next())
            .expect("a project always has one source file")
            .clone()
    })
}

fn files_json(files: &BTreeMap<String, String>) -> Value {
    Value::Array(
        files
            .iter()
            .map(|(path, source)| json!({ "path": path, "source": source }))
            .collect(),
    )
}

fn enforce_limits(files: &BTreeMap<String, String>) -> Result<(), ProjectError> {
    if files.len() > MAX_FILES {
        return Err(ProjectError::new(
            "source_too_large",
            format!("the project has more than {MAX_FILES} source files"),
        ));
    }
    let mut total = 0usize;
    for (path, source) in files {
        if source.len() > MAX_FILE_BYTES {
            return Err(ProjectError::new(
                "source_too_large",
                format!("{path} exceeds the {MAX_FILE_BYTES}-byte editor limit"),
            ));
        }
        total = total
            .checked_add(source.len())
            .ok_or_else(|| ProjectError::new("source_too_large", "project source size overflow"))?;
    }
    if total > MAX_TOTAL_BYTES {
        return Err(ProjectError::new(
            "source_too_large",
            format!("project source exceeds the {MAX_TOTAL_BYTES}-byte editor limit"),
        ));
    }
    Ok(())
}

fn draft_revision(files: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (path, source) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(sha256_hex(source.as_bytes()).as_bytes());
        hasher.update([b'\n']);
    }
    format!("{:x}", hasher.finalize())
}

fn baseline_revision(files: &BTreeMap<String, FileState>) -> String {
    let mut hasher = Sha256::new();
    for (path, file) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(file.sha256.as_bytes());
        hasher.update([b'\n']);
    }
    format!("{:x}", hasher.finalize())
}

fn push_history(history: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    if history.last().is_some_and(|last| {
        last.path == entry.path && last.before == entry.before && last.after == entry.after
    }) {
        return;
    }
    history.push(entry);
    while history.len() > MAX_HISTORY {
        history.remove(0);
    }
    let mut bytes: usize = history
        .iter()
        .map(|entry| entry.before.len() + entry.after.len())
        .sum();
    while bytes > MAX_HISTORY_BYTES && history.len() > 1 {
        let removed = history.remove(0);
        bytes = bytes.saturating_sub(removed.before.len() + removed.after.len());
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

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, source)| ((*path).to_owned(), (*source).to_owned()))
            .collect()
    }

    #[test]
    fn revisions_sort_paths_and_hash_contents() {
        let forward = files(&[("a.tsx", "A"), ("b.tsx", "B")]);
        let reverse = files(&[("b.tsx", "B"), ("a.tsx", "A")]);
        assert_eq!(draft_revision(&forward), draft_revision(&reverse));
        let changed = files(&[("a.tsx", "A!"), ("b.tsx", "B")]);
        assert_ne!(draft_revision(&forward), draft_revision(&changed));
    }

    #[test]
    fn patch_application_requires_exact_old_text_and_no_overlap() {
        let draft = files(&[("a.tsx", "<div>hello</div>")]);
        let edit = json!({
            "patches": [
                {"path":"a.tsx","start":5,"end":10,"before":"hello","replacement":"world"},
                {"path":"a.tsx","start":10,"end":16,"before":"</div>","replacement":"</div>"}
            ],
            "selection": null
        });
        let result = EditResult::parse(&edit, &draft).expect("valid edit");
        assert_eq!(result.path.as_deref(), Some("a.tsx"));
        assert_eq!(result.source, "<div>world</div>");

        let stale = json!({
            "patches":[{"path":"a.tsx","start":5,"end":10,"before":"olleh","replacement":"x"}],
            "selection": null
        });
        assert_eq!(
            EditResult::parse(&stale, &draft).unwrap_err().code,
            "stale_node"
        );

        let overlap = json!({
            "patches":[
                {"path":"a.tsx","start":0,"end":6,"before":"<div>h","replacement":"x"},
                {"path":"a.tsx","start":5,"end":10,"before":"hello","replacement":"y"}
            ],
            "selection": null
        });
        assert_eq!(
            EditResult::parse(&overlap, &draft).unwrap_err().code,
            "invalid_patch"
        );
    }

    #[test]
    fn patch_offsets_are_utf8_byte_offsets() {
        let draft = files(&[("a.jsx", "<p>héllo 😀</p>")]);
        let source = &draft["a.jsx"];
        let start = source.find("héllo").unwrap();
        let end = start + "héllo".len();
        let edit = json!({
            "patches":[{"path":"a.jsx","start":start,"end":end,"before":"héllo","replacement":"bye"}],
            "selection": null
        });
        let result = EditResult::parse(&edit, &draft).expect("valid utf8 edit");
        assert_eq!(result.source, "<p>bye 😀</p>");
    }

    #[test]
    fn cross_file_patches_are_rejected() {
        let draft = files(&[("a.jsx", "<p>a</p>"), ("b.jsx", "<p>b</p>")]);
        let edit = json!({
            "patches":[
                {"path":"a.jsx","start":0,"end":3,"before":"<p>","replacement":"<div>"},
                {"path":"b.jsx","start":0,"end":3,"before":"<p>","replacement":"<div>"}
            ],
            "selection": null
        });
        assert_eq!(
            EditResult::parse(&edit, &draft).unwrap_err().code,
            "invalid_patch"
        );
    }

    #[test]
    fn node_validation_enforces_ids_files_and_positions() {
        let draft = files(&[("a.tsx", "<div><span/></div>")]);
        let good = json!({
            "nodes":[
                {"id":format!("n1-{}", "a".repeat(64)),"file":"a.tsx","start":0,"tag":"div","parent":null,"line":1,"editable":true,"reason":null},
                {"id":format!("n2-{}", "b".repeat(64)),"file":"a.tsx","start":5,"tag":"span","parent":format!("n1-{}", "a".repeat(64)),"line":1,"editable":true,"reason":null}
            ],
            "instrumented":{"a.tsx":"<div><span/></div>"}
        });
        assert!(parse_nodes(&good, &draft).is_ok());

        let duplicate = json!({
            "nodes":[
                {"id":format!("n1-{}", "a".repeat(64)),"file":"a.tsx","start":0,"tag":"div","parent":null,"line":1,"editable":true,"reason":null},
                {"id":format!("n1-{}", "a".repeat(64)),"file":"a.tsx","start":5,"tag":"span","parent":null,"line":1,"editable":true,"reason":null}
            ]
        });
        assert_eq!(
            parse_nodes(&duplicate, &draft).unwrap_err().code,
            "invalid_response"
        );

        let duplicate_position = json!({
            "nodes":[
                {"id":format!("n1-{}", "a".repeat(64)),"file":"a.tsx","start":0,"tag":"div","parent":null,"line":1,"editable":true,"reason":null},
                {"id":format!("n2-{}", "b".repeat(64)),"file":"a.tsx","start":0,"tag":"span","parent":null,"line":1,"editable":true,"reason":null}
            ]
        });
        assert_eq!(
            parse_nodes(&duplicate_position, &draft).unwrap_err().code,
            "invalid_response"
        );

        let outside = json!({
            "nodes":[{"id":format!("n1-{}", "a".repeat(64)),"file":"b.tsx","start":0,"tag":"div","parent":null,"line":1,"editable":true,"reason":null}]
        });
        assert_eq!(
            parse_nodes(&outside, &draft).unwrap_err().code,
            "invalid_response"
        );

        let not_tag = json!({
            "nodes":[{"id":format!("n1-{}", "a".repeat(64)),"file":"a.tsx","start":1,"tag":"div","parent":null,"line":1,"editable":true,"reason":null}]
        });
        assert_eq!(
            parse_nodes(&not_tag, &draft).unwrap_err().code,
            "invalid_response"
        );
    }

    #[test]
    fn source_paths_and_pages_are_bounded_and_portable() {
        assert_eq!(
            normalize_source_path("src/App.tsx", "index.html").unwrap(),
            "src/App.tsx"
        );
        assert_eq!(normalize_page("index.html").unwrap(), "index.html");
        for bad in ["../App.tsx", "/App.tsx", "src/.hidden.tsx", "src\\App.tsx"] {
            assert!(normalize_source_path(bad, "index.html").is_err(), "{bad}");
        }
        assert!(normalize_source_path("src/App.exe", "index.html").is_err());
    }

    #[test]
    fn history_is_bounded_and_moves_entries_between_stacks() {
        let mut undo = Vec::new();
        for index in 0..(MAX_HISTORY + 5) {
            push_history(
                &mut undo,
                HistoryEntry {
                    path: "a.tsx".into(),
                    before: format!("{index}"),
                    after: format!("{}", index + 1),
                },
            );
        }
        assert_eq!(undo.len(), MAX_HISTORY);
        assert_eq!(undo[0].before, "5");
    }

    #[test]
    fn snapshot_prefers_a_non_page_file_with_nodes() {
        let mut state = State {
            baseline: BTreeMap::new(),
            draft: files(&[("index.html", "<html></html>"), ("src/App.tsx", "<div/>")]),
            nodes: vec![SourceNode {
                id: format!("n1-{}", "a".repeat(64)),
                file: "src/App.tsx".into(),
                start: 0,
                tag: "div".into(),
                parent: None,
                line: 1,
                editable: true,
                reason: None,
            }],
            undo: Vec::new(),
            redo: Vec::new(),
            selection: None,
            unknown: false,
            conflicted: false,
        };
        for (path, source) in &state.draft {
            state.baseline.insert(
                path.clone(),
                FileState {
                    saved: source.clone(),
                    sha256: sha256_hex(source.as_bytes()),
                },
            );
        }
        assert_eq!(snapshot_path("index.html", &state), "src/App.tsx");
    }

    #[tokio::test]
    async fn operation_scopes_share_the_root_but_restart_the_budget() {
        let project = tempfile::tempdir().expect("project");
        std::fs::write(project.path().join("a.tsx"), "<div/>").expect("source");
        let state = tempfile::tempdir().expect("state");
        let audit = Arc::new(DurableAudit::open(state.path()).expect("audit"));
        let mut config = host_config();
        config.max_calls = 2;
        config.max_total_bytes = 4096;
        let paths = || ["a.tsx".to_owned()].into_iter();
        let gate = Arc::new(FrameworkGate {
            paths: paths().collect(),
        });
        let root = Host::new(project.path(), gate, audit.clone(), config).expect("root host");
        let first = operation_scope(&root, &audit, paths()).expect("first scope");
        assert_eq!(
            first.workspace_identity().expect("identity"),
            root.workspace_identity().expect("identity"),
            "an operation scope keeps the same held root"
        );
        for _ in 0..2 {
            first
                .call(
                    "fs.read",
                    json!({"path": "a.tsx"}),
                    CancellationToken::new(),
                )
                .await
                .expect("call within the scope budget");
        }
        let exhausted = first
            .call(
                "fs.read",
                json!({"path": "a.tsx"}),
                CancellationToken::new(),
            )
            .await
            .expect_err("the scope budget is enforced");
        assert!(!exhausted.outcome_unknown);

        let second = operation_scope(&root, &audit, paths()).expect("second scope");
        second
            .call(
                "fs.read",
                json!({"path": "a.tsx"}),
                CancellationToken::new(),
            )
            .await
            .expect("the next user operation gets a fresh budget");
    }
}
