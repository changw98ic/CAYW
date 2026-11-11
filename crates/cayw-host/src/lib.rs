//! Host-owned, permission-gated capabilities for bounded CodeMode execution.
//!
//! The host supplies permission decisions, durable audit storage, process profiles,
//! and optional external services. Catalog membership never grants permission.

mod catalog;
mod codeintel;
mod filesystem;
mod git;
mod process;
mod read_fence;
pub mod terminal;
pub use cayw_output_filter::{FilterOptions, FilterProfile};
pub use git::GitWritePolicy;
pub use read_fence::{
    FenceRecovery, FrozenReadStamp, FrozenReadTicket, FrozenReadValue, PendingMutation,
    ReadFenceFault, ReadFenceRecovery,
};
pub use terminal::{TerminalConfig, TerminalLimits};

use async_trait::async_trait;
use cap_std::fs::Dir;
use cayw_ai::{ArgumentValidator, CancellationToken, ToolDefinition};
use cayw_codeintel::SymbolVersionBridge;
use cayw_codemode::{CapabilityError, CapabilityHost, CapabilityStop};
pub use cayw_device::{
    DeviceError, DirectoryProcessSpec, ProcessIdentity, ProcessIo, ProcessObserver, ProcessResult,
    ProcessRunner, ProcessSpec, TerminationReason,
};
pub use process::runner;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// A permission decision for the exact capability and validated JSON arguments.
/// Implementations must honor cancellation while awaiting a human decision.
#[async_trait]
pub trait PermissionGate: Send + Sync {
    async fn authorize(
        &self,
        capability: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError>;
}

/// Persistence boundary for capability execution.
///
/// `started` must durably commit a running record before returning its unique ID.
/// `settled` must durably record the complete result before returning success.
/// Neither method may report success for a buffered, uncommitted write. A running
/// record left by a crash is unknown until the application reconciles it.
/// `dropped` must synchronously retain unknown status; its failure is also exposed
/// through `Host::audit_failure`, and permanently stops this host instance.
#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn started(&self, capability: &str, arguments: &Value)
    -> Result<String, CapabilityError>;
    async fn settled(
        &self,
        call_id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError>;
    fn dropped(&self, call_id: &str) -> Result<(), CapabilityError>;
}

/// Existing application services, injected without creating a loader or connection.
///
/// Names are `search.query`, `mcp.discover`, `mcp.describe`, `mcp.call`, `mcp.prompts.*`,
/// `skills.list`, and `skills.read`. Their schemas are in `Host::capabilities`.
/// The child cancellation token is cancelled on timeout, cancellation, or drop.
/// Dropping a future must not silently detach work. If completion cannot be
/// established, return `CapabilityError::unknown` and retain reconciliation data.
#[async_trait]
pub trait ExternalHost: Send + Sync {
    /// Trusted orchestration services may delegate every workspace operation to
    /// another Host call. They do not hold the workspace fence or execution lane
    /// while awaiting those calls; authorization, audit and budgets still apply.
    fn delegates_workspace_calls(&self) -> bool {
        false
    }

    async fn call(
        &self,
        capability: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileKind {
    Build,
    Test,
    Check,
    Lint,
    Format,
}

/// A command selected entirely by the application. No shell is inserted.
/// `allow_filter` permits one restricted, positional filter for a test profile.
#[derive(Debug, Clone)]
pub struct Profile {
    pub kind: ProfileKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// `None` (the default) lets the process run until it exits or the caller
    /// cancels. `Some` enforces an explicit lifetime bound.
    pub timeout: Option<Duration>,
    /// `None` (the default) never stops the process for output volume. Above
    /// the Device inline threshold, output spills to a private file.
    pub max_output_bytes: Option<usize>,
    pub allow_filter: bool,
    pub io: cayw_device::ProcessIo,
}

/// Durable sink for F02 originals that were removed from the agent-facing summary.
///
/// Runtime wires the Device [`cayw_content::ContentStore`]; tests may use an
/// in-memory implementation. When filtering reduces a stream, Host **must** put
/// the original here before returning — absence fails closed.
pub trait FilteredOriginalStore: Send + Sync {
    fn put_original(&self, digest: &str, bytes: &[u8]) -> Result<Value, CapabilityError>;
    fn get_original(&self, digest: &str) -> Result<Option<Vec<u8>>, CapabilityError>;
}

pub fn result_body_key(
    device: &str,
    session: &str,
    task: Option<&str>,
    handle: &str,
) -> Result<cayw_content::Id, CapabilityError> {
    let bytes = serde_json::to_vec(&("cayw-runtime-result-body-v1", device, session, task, handle))
        .map_err(|e| failure(e.to_string()))?;
    cayw_content::Id::new(cayw_content::ContentHash::of(&bytes).to_string())
        .map_err(|e| failure(e.to_string()))
}

pub fn result_meta_key(
    device: &str,
    session: &str,
    task: Option<&str>,
    handle: &str,
) -> Result<cayw_content::Id, CapabilityError> {
    let bytes = serde_json::to_vec(&("cayw-runtime-result-meta-v1", device, session, task, handle))
        .map_err(|e| failure(e.to_string()))?;
    cayw_content::Id::new(cayw_content::ContentHash::of(&bytes).to_string())
        .map_err(|e| failure(e.to_string()))
}

/// ContentStore-backed [`FilteredOriginalStore`] (content-addressed and session-scoped).
pub struct ContentFilteredOriginalStore {
    store: Arc<cayw_content::ContentStore>,
    session_id: String,
}

impl ContentFilteredOriginalStore {
    pub fn new(store: Arc<cayw_content::ContentStore>) -> Self {
        Self {
            store,
            session_id: "default".into(),
        }
    }

    pub fn with_session(
        store: Arc<cayw_content::ContentStore>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            session_id: session_id.into(),
        }
    }

    pub fn store(&self) -> &cayw_content::ContentStore {
        &self.store
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredResultMeta {
    v: u32,
    handle: String,
    content_hash: String,
    bytes: u64,
    encoding: String,
    released: bool,
    scope: String,
    session_id: String,
    task_id: Option<String>,
}

fn result_storage_error(error: cayw_content::Error) -> CapabilityError {
    if matches!(error, cayw_content::Error::CommitOutcomeUnknown(_)) {
        CapabilityError::unknown(error.to_string())
    } else {
        failure(error.to_string())
    }
}

fn fresh_result_handle() -> Result<String, CapabilityError> {
    use std::io::Read;
    let mut bytes = [0; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(io_error)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn produced_result_handle(
    source: &str,
    key: &str,
    encoding: &str,
    bytes: &[u8],
) -> Result<String, CapabilityError> {
    let identity = serde_json::to_vec(&(
        "cayw-produced-result-v1",
        source,
        key,
        encoding,
        cayw_content::ContentHash::of(bytes).to_string(),
    ))
    .map_err(|error| failure(error.to_string()))?;
    Ok(cayw_content::ContentHash::of(&identity).to_string())
}

fn read_result_meta(
    store: &cayw_content::ContentStore,
    session: &str,
    handle: &str,
) -> Result<Option<StoredResultMeta>, CapabilityError> {
    cayw_content::Id::new(handle).map_err(result_storage_error)?;
    let id = result_meta_key(store.device_id().as_str(), session, None, handle)?;
    let Some((_, bytes)) = store.try_read(&id).map_err(result_storage_error)? else {
        return Ok(None);
    };
    let meta: StoredResultMeta =
        serde_json::from_slice(&bytes).map_err(|_| failure("result metadata is corrupt"))?;
    if meta.v != 1
        || meta.handle != handle
        || meta.session_id != session
        || meta.task_id.is_some()
        || meta.scope != "session"
        || !matches!(meta.encoding.as_str(), "utf8" | "json" | "base64")
    {
        return Err(failure(
            "result metadata differs from its owner or encoding",
        ));
    }
    Ok(Some(meta))
}

fn put_session_result(
    store: &cayw_content::ContentStore,
    session: &str,
    initial_handle: &str,
    bytes: &[u8],
    encoding: &str,
) -> Result<Value, CapabilityError> {
    let mut handle = initial_handle.to_owned();
    let hash = cayw_content::ContentHash::of(bytes).to_string();
    loop {
        let meta = StoredResultMeta {
            v: 1,
            handle: handle.clone(),
            content_hash: hash.clone(),
            bytes: bytes.len() as u64,
            encoding: encoding.into(),
            released: false,
            scope: "session".into(),
            session_id: session.into(),
            task_id: None,
        };
        if let Some(existing) = read_result_meta(store, session, &handle)? {
            let mut live = existing.clone();
            live.released = false;
            if live != meta {
                return Err(failure(
                    "result metadata conflicts with the produced content",
                ));
            }
            // Release remains permanent for that reference. A new production
            // creates a new reference instead of silently reviving the old one.
            if existing.released {
                handle = fresh_result_handle()?;
                continue;
            }
        }
        let body = result_body_key(store.device_id().as_str(), session, None, &handle)?;
        match store.try_put(&body, cayw_content::ExpectedRevision::Absent, bytes) {
            Ok(_) => {}
            Err(cayw_content::Error::Conflict { .. }) => {
                let existing = store.try_read(&body).map_err(result_storage_error)?;
                if existing.is_none_or(|(_, existing)| existing != bytes) {
                    return Err(failure("result body conflicts with the produced content"));
                }
            }
            Err(error) => return Err(result_storage_error(error)),
        }
        let id = result_meta_key(store.device_id().as_str(), session, None, &handle)?;
        let metadata = serde_json::to_vec(&meta).map_err(|error| failure(error.to_string()))?;
        match store.try_put(&id, cayw_content::ExpectedRevision::Absent, &metadata) {
            Ok(_) => {}
            Err(cayw_content::Error::Conflict { .. }) => {
                let existing = read_result_meta(store, session, &handle)?
                    .ok_or_else(|| failure("result metadata disappeared after a conflict"))?;
                let mut live = existing.clone();
                live.released = false;
                if live != meta {
                    return Err(failure(
                        "result metadata conflicts with the produced content",
                    ));
                }
                if existing.released {
                    handle = fresh_result_handle()?;
                    continue;
                }
            }
            Err(error) => return Err(result_storage_error(error)),
        }
        let mut view = serde_json::to_value(meta).map_err(|error| failure(error.to_string()))?;
        view.as_object_mut()
            .expect("result metadata is an object")
            .remove("v");
        view["stored"] = Value::Bool(true);
        return Ok(view);
    }
}

fn get_session_result(
    store: &cayw_content::ContentStore,
    session: &str,
    handle: &str,
) -> Result<Option<(StoredResultMeta, Vec<u8>)>, CapabilityError> {
    let Some(meta) = read_result_meta(store, session, handle)? else {
        return Ok(None);
    };
    if meta.released {
        return Err(failure("result handle has been released"));
    }
    let body = result_body_key(store.device_id().as_str(), session, None, handle)?;
    let Some((_, bytes)) = store.try_read(&body).map_err(result_storage_error)? else {
        return Err(failure("result body not found"));
    };
    if cayw_content::ContentHash::of(&bytes).to_string() != meta.content_hash
        || bytes.len() as u64 != meta.bytes
    {
        return Err(failure("result body differs from its metadata"));
    }
    Ok(Some((meta, bytes)))
}

impl FilteredOriginalStore for ContentFilteredOriginalStore {
    fn put_original(&self, digest: &str, bytes: &[u8]) -> Result<Value, CapabilityError> {
        if cayw_content::ContentHash::of(bytes).to_string() != digest
            || std::str::from_utf8(bytes).is_err()
        {
            return Err(failure("filtered original must match its UTF-8 digest"));
        }
        put_session_result(&self.store, &self.session_id, digest, bytes, "utf8")
    }

    fn get_original(&self, handle: &str) -> Result<Option<Vec<u8>>, CapabilityError> {
        get_session_result(&self.store, &self.session_id, handle).and_then(|result| {
            result
                .map(|(meta, bytes)| {
                    if meta.encoding != "utf8" {
                        return Err(failure("filtered original is not UTF-8 text"));
                    }
                    Ok(bytes)
                })
                .transpose()
        })
    }
}

pub struct ContentWebSink {
    store: Arc<cayw_content::ContentStore>,
    session_id: String,
}

impl ContentWebSink {
    pub fn new(store: Arc<cayw_content::ContentStore>, session_id: impl Into<String>) -> Self {
        Self {
            store,
            session_id: session_id.into(),
        }
    }
}

impl cayw_web_access::ContentSink for ContentWebSink {
    fn put(
        &self,
        id: &str,
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> Result<Value, cayw_web_access::WebAccessError> {
        let text_mime = mime_type.is_none_or(|mime| {
            let mime = mime.to_ascii_lowercase();
            mime.starts_with("text/") || mime.contains("json") || mime.contains("xml")
        });
        let (stored, encoding) = if text_mime && std::str::from_utf8(bytes).is_ok() {
            (bytes.to_vec(), "utf8")
        } else {
            use base64::Engine;
            (
                base64::engine::general_purpose::STANDARD
                    .encode(bytes)
                    .into_bytes(),
                "base64",
            )
        };
        produced_result_handle("web", id, encoding, &stored)
            .and_then(|handle| {
                put_session_result(&self.store, &self.session_id, &handle, &stored, encoding)
            })
            .map_err(web_storage_error)
    }

    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, cayw_web_access::WebAccessError> {
        get_session_result(&self.store, &self.session_id, id)
            .and_then(|result| {
                result
                    .map(|(meta, bytes)| decode_result_body(&meta, bytes))
                    .transpose()
            })
            .map_err(web_storage_error)
    }
}

fn web_storage_error(error: CapabilityError) -> cayw_web_access::WebAccessError {
    let mut result = cayw_web_access::WebAccessError::new(
        cayw_web_access::FailureClass::ContentStorage,
        error.message,
    );
    result.outcome_unknown = error.outcome_unknown;
    result
}

fn decode_result_body(meta: &StoredResultMeta, bytes: Vec<u8>) -> Result<Vec<u8>, CapabilityError> {
    if meta.encoding == "base64" {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(bytes)
            .map_err(|_| failure("result body is not valid base64"))
    } else {
        Ok(bytes)
    }
}

pub struct ContentVisionSink {
    store: Arc<cayw_content::ContentStore>,
    session_id: String,
}

impl ContentVisionSink {
    pub fn new(store: Arc<cayw_content::ContentStore>, session_id: impl Into<String>) -> Self {
        Self {
            store,
            session_id: session_id.into(),
        }
    }
}

impl cayw_vision::ImageSink for ContentVisionSink {
    fn put(
        &self,
        key: &str,
        bytes: &[u8],
        _mime_type: Option<&str>,
    ) -> Result<Value, CapabilityError> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(bytes)
            .into_bytes();
        let handle = produced_result_handle("vision", key, "base64", &encoded)?;
        put_session_result(&self.store, &self.session_id, &handle, &encoded, "base64")
    }

    fn get(&self, handle: &str) -> Result<Option<Vec<u8>>, CapabilityError> {
        get_session_result(&self.store, &self.session_id, handle).and_then(|result| {
            result
                .map(|(meta, bytes)| decode_result_body(&meta, bytes))
                .transpose()
        })
    }
}

/// One optional Host service that could not be constructed. The reason is safe
/// to show to the user; it never contains credentials or file contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DisabledService {
    pub name: &'static str,
    pub reason: String,
}

#[derive(Clone)]
pub struct HostConfig {
    /// Trusted opt-in to a cooperating, explicitly frozen fs.read view. Outside
    /// editors ignoring Host fences are not observed automatically.
    pub frozen_reads: bool,
    /// Optional explicit admission limits. `None` (the default) applies no
    /// artificial bound for that dimension; the enclosing run deadline and
    /// explicit caller limits still apply.
    pub max_argument_bytes: Option<usize>,
    pub max_result_bytes: Option<usize>,
    /// Combined serialized argument and result bytes across this host instance.
    pub max_total_bytes: Option<usize>,
    pub max_calls: Option<usize>,
    /// Optional per-file bound for ordinary file reads and for each copied Git
    /// metadata file. Git reads no longer copy object data or the worktree, so
    /// repository size and ignored build data do not gate a read. Directory
    /// listings are bounded only by an explicitly configured
    /// `max_list_entries`.
    pub max_file_bytes: Option<usize>,
    /// Optional directory listing bound and copied Git metadata entry allowance.
    pub max_list_entries: Option<usize>,
    /// Includes permission waiting and execution. `None` (the default) waits
    /// until cancellation or completion; the enclosing run deadline still stops
    /// the call. Durable audit writes are awaited even after cancellation so a
    /// caller cannot mistake an uncommitted result.
    pub call_timeout: Option<Duration>,
    /// Absolute application-selected git executable; absent means no git catalog.
    /// Read commands use a sanitized private metadata directory and the held root
    /// in place. Worktree symlinks are not followed; a changed object-store
    /// identity discards the result. Concurrent in-place source edits do not have
    /// transaction snapshot semantics.
    pub git_program: Option<PathBuf>,
    /// Opt-in for real-repository Git writes. Absent means no write capability.
    /// The identity is host-owned; capabilities are individually policy-gated.
    pub git_writes: Option<GitWritePolicy>,
    /// `None` (the default) lets process profiles and Git commands run until
    /// they exit or the caller cancels.
    pub process_timeout: Option<Duration>,
    /// `None` (the default) never stops a process for output volume; larger
    /// output spills to private files.
    pub max_process_output_bytes: Option<usize>,
    pub profiles: BTreeMap<String, Profile>,
    /// Host-selected interactive profiles. None omits terminal handoffs.
    pub terminal_handoffs: Option<TerminalConfig>,
    /// Pure, bounded data capabilities. None omits them from the catalog.
    pub data: Option<cayw_data::Limits>,
    /// Tree, change-batch and archive capabilities. Opening is deferred until an
    /// authorized, audited execution; catalog inspection never creates a store.
    /// Pending batches fence ordinary writes and profiles even when this is None.
    pub workspace: Option<cayw_workspace::Limits>,
    /// Shared F04 symbol/version bridge for Agent tools, editor, and Trace.
    /// `None` (the default) omits `codeintel.*` from the ordinary Host catalog.
    pub codeintel: Option<SymbolVersionBridge>,
    /// Optional F02 command-output filter applied when presenting process/git
    /// results to agents/UI. `None` (the default) returns stdout/stderr directly.
    /// Runtime can enable filtering when a [`Self::content_store`] is available.
    /// When `Some` and enabled, `stdout`/`stderr` contain filtered summaries; the
    /// full original is stored before return and `original_handle` under
    /// `output_filter` contains its formal session result reference. Structured
    /// git reads (`blame`/`log`/`show`) use Passthrough so porcelain records stay
    /// intact. Task wall/cost are not measured here; callers pass those to
    /// compare APIs.
    pub output_filter: Option<FilterOptions>,
    /// Store used to retain full process/git captures when F02 filtering
    /// reduces them. Required whenever filtering would drop bytes; absence
    /// fails closed instead of silently discarding the only copy. Runtime
    /// injects a ContentStore adapter when available, so filtering can be
    /// configured. Tests that exercise reduction must wire a
    /// [`FilteredOriginalStore`].
    pub content_store: Option<Arc<dyn FilteredOriginalStore>>,
    /// Web search / fetch / extract (`web.*`).
    /// `None` (the default) omits them from the ordinary Host catalog.
    /// When set, DuckDuckGo HTML search (no API key) + live HTTP fetch are available.
    /// Runtime binds bodies to session result handles; standalone services may
    /// retain bodies in their own process-local store.
    pub web_access: Option<cayw_web_access::WebAccessOptions>,
    /// Optional external sink for web content reference bodies (e.g. Device storage).
    pub web_access_sink: Option<Arc<dyn cayw_web_access::ContentSink>>,
    /// Vision tools (crop / pixel_diff / colors / ocr / describe).
    /// `None` (the default) omits them from the ordinary Host catalog.
    pub vision: Option<cayw_vision::Limits>,
    /// Optional external sink for cropped vision images (e.g. Device storage).
    pub vision_sink: Option<Arc<dyn cayw_vision::ImageSink>>,
    pub external: Option<Arc<dyn ExternalHost>>,
}
impl Default for HostConfig {
    fn default() -> Self {
        Self {
            frozen_reads: false,
            max_argument_bytes: None,
            max_result_bytes: None,
            max_total_bytes: None,
            max_calls: None,
            max_file_bytes: None,
            max_list_entries: None,
            call_timeout: None,
            git_program: None,
            git_writes: None,
            process_timeout: None,
            max_process_output_bytes: None,
            profiles: BTreeMap::new(),
            terminal_handoffs: None,
            data: Some(cayw_data::Limits::default()),
            workspace: Some(cayw_workspace::Limits::default()),
            codeintel: None,
            output_filter: None,
            content_store: None,
            web_access: None,
            web_access_sink: None,
            vision: None,
            vision_sink: None,
            external: None,
        }
    }
}
impl HostConfig {
    fn validate(&self) -> Result<(), CapabilityError> {
        if self.max_argument_bytes == Some(0)
            || self.max_result_bytes.is_some_and(|limit| limit < 2)
            || self.max_total_bytes.is_some_and(|limit| limit < 2)
            || self.max_calls == Some(0)
            || self.max_file_bytes == Some(0)
            || self.max_list_entries == Some(0)
            || self.max_process_output_bytes == Some(0)
            || self.call_timeout.is_some_and(|timeout| {
                timeout.is_zero() || Instant::now().checked_add(timeout).is_none()
            })
            || self.process_timeout.is_some_and(|timeout| {
                timeout.is_zero() || Instant::now().checked_add(timeout).is_none()
            })
        {
            return Err(failure(
                "host budgets must be positive and have representable deadlines",
            ));
        }
        if let Some(limits) = self.data {
            limits.validate()?;
        }
        if let Some(limits) = &self.workspace {
            limits.validate()?;
        }
        if let Some(program) = &self.git_program {
            process::validate_program(program)?;
        }
        git::validate_config(self)?;
        if let Some(config) = &self.terminal_handoffs {
            terminal::validate_config(config)?;
            if let (Some(read), Some(result)) =
                (config.limits.max_read_bytes, self.max_result_bytes)
                && read.saturating_mul(4).saturating_add(4096) > result
            {
                return Err(failure("terminal read page exceeds host result budget"));
            }
        }
        for (name, profile) in &self.profiles {
            if !process::valid_name(name) {
                return Err(failure("invalid profile name"));
            }
            process::validate_program(&profile.program)?;
            if let cayw_device::ProcessIo::Pty { columns, rows } = profile.io
                && (columns == 0 || rows == 0)
            {
                return Err(failure("terminal dimensions must be positive"));
            }
            #[cfg(not(target_os = "macos"))]
            if matches!(profile.io, cayw_device::ProcessIo::Pty { .. }) {
                return Err(failure("terminal execution is only supported on macOS"));
            }
            if profile.timeout.is_some_and(|timeout| {
                timeout.is_zero() || Instant::now().checked_add(timeout).is_none()
            }) || profile.max_output_bytes == Some(0)
                || (profile.allow_filter && profile.kind != ProfileKind::Test)
                || profile.args.len() > 256
                || profile
                    .args
                    .iter()
                    .any(|arg| arg.contains('\0') || arg.len() > 8192)
                || profile.env.len() > 256
                || profile.env.iter().any(|(key, value)| {
                    key.is_empty()
                        || key.contains(['=', '\0'])
                        || value.contains('\0')
                        || key.len() > 1024
                        || value.len() > 32768
                })
            {
                return Err(failure("invalid process profile"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Executor {
    Filesystem,
    Git,
    Profile,
    Terminal,
    Data,
    Workspace,
    CodeIntel,
    WebAccess,
    Vision,
    External,
}

struct Capability {
    definition: ToolDefinition,
    validator: ArgumentValidator,
    executor: Executor,
}
#[derive(Default)]
struct Budget {
    calls: usize,
    bytes: usize,
    reserved: usize,
    exhausted: bool,
}
#[derive(Default)]
struct Safety {
    unknown: AtomicBool,
    audit_failure: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct Host {
    root: Arc<Dir>,
    gate: Arc<dyn PermissionGate>,
    audit: Arc<dyn AuditSink>,
    config: Arc<HostConfig>,
    catalog: Arc<BTreeMap<String, Capability>>,
    budget: Arc<Mutex<Budget>>,
    execution: Arc<tokio::sync::Mutex<()>>,
    safety: Arc<Safety>,
    runner: Arc<cayw_device::ProcessRunner>,
    terminal: Option<Arc<terminal::TerminalManager>>,
    web_access: Option<Arc<cayw_web_access::WebAccess>>,
    vision: Option<Arc<cayw_vision::VisionService>>,
    external: Option<Arc<dyn ExternalHost>>,
    disabled_services: Arc<Vec<DisabledService>>,
}
impl Host {
    /// Opens one directory capability. File operations never reopen an ambient
    /// path after validation; each component is opened without following links.
    pub fn new(
        root: impl AsRef<Path>,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
        config: HostConfig,
    ) -> Result<Self, CapabilityError> {
        let root_path = root.as_ref().canonicalize().map_err(io_error)?;
        let root =
            Dir::open_ambient_dir(&root_path, cap_std::ambient_authority()).map_err(io_error)?;
        Self::from_root(root, gate, audit, config)
    }
    /// Creates a separate task's admission and process state over this same held
    /// directory. It never clears this Host's safety, budgets or pending fences.
    pub fn task_host(
        &self,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
    ) -> Result<Self, CapabilityError> {
        let mut host = Self::from_root(
            self.root.try_clone().map_err(io_error)?,
            gate,
            audit,
            (*self.config).clone(),
        )?;
        // The parent already recorded why an optional service is absent. Carry
        // that reason into the task scope instead of silently dropping it.
        host.disabled_services = self.disabled_services.clone();
        Ok(host)
    }
    fn from_root(
        root: Dir,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
        mut config: HostConfig,
    ) -> Result<Self, CapabilityError> {
        config.validate()?;
        // An optional service that cannot be constructed only disables its own
        // tool group. Record a visible reason and keep the Host usable.
        let mut disabled_services = Vec::new();
        let web_access = match config.web_access.take() {
            Some(options) => {
                let options = options.normalized();
                match options.build_with_sink(config.web_access_sink.clone()) {
                    Ok(service) => {
                        config.web_access = Some(options);
                        Some(Arc::new(service))
                    }
                    Err(error) => {
                        disabled_services.push(DisabledService {
                            name: "web_access",
                            reason: cayw_web_access::host_error_message(&error),
                        });
                        None
                    }
                }
            }
            None => None,
        };
        let vision = match config.vision.take() {
            Some(limits) => match cayw_vision::VisionService::from_env(limits) {
                Ok(mut service) => {
                    if let Some(sink) = &config.vision_sink {
                        service = service.with_sink(Arc::clone(sink));
                    }
                    config.vision = Some(limits);
                    Some(Arc::new(service))
                }
                Err(error) => {
                    disabled_services.push(DisabledService {
                        name: "vision",
                        reason: error.to_string(),
                    });
                    None
                }
            },
            None => None,
        };
        let mut catalog = BTreeMap::new();
        for (definition, executor) in catalog::definitions(&config) {
            let validator = ArgumentValidator::new(&definition.parameters)
                .map_err(|error| failure(error.to_string()))?;
            if catalog
                .insert(
                    definition.name.clone(),
                    Capability {
                        definition,
                        validator,
                        executor,
                    },
                )
                .is_some()
            {
                return Err(failure("duplicate host capability"));
            }
        }
        let runner = process::runner(&config)?;
        let terminal = config
            .terminal_handoffs
            .clone()
            .map(terminal::TerminalManager::new)
            .transpose()?
            .map(Arc::new);
        let external = config.external.clone();
        Ok(Self {
            root: Arc::new(root),
            gate,
            audit,
            config: Arc::new(config),
            catalog: Arc::new(catalog),
            budget: Arc::default(),
            execution: Arc::new(tokio::sync::Mutex::new(())),
            safety: Arc::default(),
            runner: Arc::new(runner),
            terminal,
            web_access,
            vision,
            external,
            disabled_services: Arc::new(disabled_services),
        })
    }
    /// Optional services that construction disabled, with a user-safe reason.
    pub fn disabled_services(&self) -> &[DisabledService] {
        &self.disabled_services
    }
    /// Shared F04 bridge handle (same instance as editor/Trace when they clone it).
    pub fn codeintel_bridge(&self) -> Option<SymbolVersionBridge> {
        self.config.codeintel.clone()
    }

    /// Read an externalized original using `original_handle.handle`.
    /// Returns `Ok(None)` when the store is configured but the handle is absent.
    pub fn read_output_original(&self, handle_id: &str) -> Result<Option<String>, CapabilityError> {
        let store = self.config.content_store.as_ref().ok_or_else(|| {
            failure("filtered-original store is not configured; originals cannot be retrieved")
        })?;
        Ok(store
            .get_original(handle_id)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Identity of the directory actually held open by this host. Independent
    /// opens of the same live directory agree; renaming/replacing its pathname
    /// cannot change this identity or redirect the metadata lookup.
    pub fn workspace_identity(&self) -> Result<String, CapabilityError> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            let metadata = self.root.dir_metadata().map_err(io_error)?;
            Ok(format!("unix:{:x}:{:x}", metadata.dev(), metadata.ino()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[repr(C)]
            struct FileIdInfo {
                volume: u64,
                id: [u8; 16],
            }
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetFileInformationByHandleEx(
                    file: *mut std::ffi::c_void,
                    class: i32,
                    info: *mut std::ffi::c_void,
                    size: u32,
                ) -> i32;
            }
            let mut info = FileIdInfo {
                volume: 0,
                id: [0; 16],
            };
            // SAFETY: root owns this live directory handle; FileIdInfo matches
            // FILE_ID_INFO and the API writes only the supplied output buffer.
            let result = unsafe {
                GetFileInformationByHandleEx(
                    self.root.as_raw_handle(),
                    18, // FileIdInfo
                    (&mut info as *mut FileIdInfo).cast(),
                    std::mem::size_of::<FileIdInfo>() as u32,
                )
            };
            if result == 0 {
                return Err(io_error(io::Error::last_os_error()));
            }
            Ok(format!(
                "windows:{:x}:{:032x}",
                info.volume,
                u128::from_le_bytes(info.id)
            ))
        }
        #[cfg(not(any(unix, windows)))]
        Err(failure(
            "opened workspace identity is unsupported on this platform",
        ))
    }

    /// Creates an immutable permission/audit scope over the same open root,
    /// configuration, catalog, budgets, execution lane, safety and process runner.
    /// No filesystem paths are reopened and no state is reset, even if stopped.
    pub fn scoped(
        &self,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
    ) -> Result<Self, CapabilityError> {
        Ok(Self {
            root: self.root.clone(),
            gate,
            audit,
            config: self.config.clone(),
            catalog: self.catalog.clone(),
            budget: self.budget.clone(),
            execution: self.execution.clone(),
            safety: self.safety.clone(),
            runner: self.runner.clone(),
            terminal: self.terminal.clone(),
            web_access: self.web_access.clone(),
            vision: self.vision.clone(),
            external: self.external.clone(),
            disabled_services: self.disabled_services.clone(),
        })
    }

    /// Adds durable OS process observations to this scope. The original runner's
    /// ownership, admission, cancellation and cleanup ledger remain shared.
    pub fn with_process_observer(&self, observer: Arc<dyn ProcessObserver>) -> Self {
        let mut scope = self.clone();
        scope.runner = Arc::new(self.runner.observed(observer));
        scope
    }

    /// Access the underlying managed ProcessRunner for process lifecycle and group supervision.
    pub fn process_runner(&self) -> Arc<cayw_device::ProcessRunner> {
        self.runner.clone()
    }

    /// Injects an active filtered-original store into this host scope.
    pub fn with_content_store(&self, store: Arc<dyn FilteredOriginalStore>) -> Self {
        let mut scope = self.clone();
        let mut config = (*self.config).clone();
        config.content_store = Some(store);
        scope.config = Arc::new(config);
        scope
    }

    /// Injects session-scoped content store, web access sink, and vision sink.
    pub fn with_session_content(
        &self,
        store: Arc<cayw_content::ContentStore>,
        session_id: impl Into<String>,
    ) -> Self {
        let session_id = session_id.into();
        let mut scope = self.clone();
        let mut config = (*self.config).clone();
        let orig_store = Arc::new(ContentFilteredOriginalStore::with_session(
            store.clone(),
            session_id.clone(),
        ));
        let web_sink = Arc::new(ContentWebSink::new(store.clone(), session_id.clone()));
        let vision_sink = Arc::new(ContentVisionSink::new(store.clone(), session_id.clone()));

        config.content_store = Some(orig_store);
        config.web_access_sink = Some(web_sink.clone());
        config.vision_sink = Some(vision_sink.clone());
        scope.config = Arc::new(config);

        if let Some(wa) = &self.web_access {
            scope.web_access = Some(Arc::new(wa.with_sink(web_sink)));
        }
        if let Some(v) = &self.vision {
            scope.vision = Some(Arc::new(v.as_ref().clone().with_sink(vision_sink)));
        }

        scope
    }

    /// Overrides this scope's application service while sharing the original
    /// root, config, catalog, cumulative budgets, execution lane, safety, and
    /// process runner. Ordinary scopes derived from this one inherit the service.
    /// A host without an external catalog cannot enable one through a scope.
    pub fn scoped_external(
        &self,
        gate: Arc<dyn PermissionGate>,
        audit: Arc<dyn AuditSink>,
        external: Arc<dyn ExternalHost>,
    ) -> Result<Self, CapabilityError> {
        if self.config.external.is_none() {
            return Err(failure("external capability catalog is not configured"));
        }
        let mut scope = self.scoped(gate, audit)?;
        scope.external = Some(external);
        Ok(scope)
    }

    /// Calls an application-supplied service through this host's existing
    /// permissions, cumulative budget, ordered execution, safety and audit. The
    /// definition is trusted Rust SDK input and never expands the base catalog.
    /// Services may mutate state; an unretainable response remains unknown.
    pub async fn call_service(
        &self,
        definition: &ToolDefinition,
        arguments: Value,
        cancel: CancellationToken,
        service: &dyn ExternalHost,
    ) -> Result<Value, CapabilityError> {
        let arguments = Arguments(arguments);
        let admitted = (|| {
            if self.catalog.contains_key(&definition.name) {
                return Err(failure("service name already belongs to the host catalog"));
            }
            if !process::valid_name(&definition.name) {
                return Err(failure("invalid service capability name"));
            }
            if self
                .config
                .max_argument_bytes
                .is_some_and(|limit| definition.description.len() > limit)
            {
                return Err(failure("service description exceeds byte limit"));
            }
            json_size(&definition.parameters, self.config.max_argument_bytes)?;
            let validator = ArgumentValidator::new(&definition.parameters)
                .map_err(|error| failure(error.to_string()))?;
            Ok(Capability {
                definition: definition.clone(),
                validator,
                executor: Executor::External,
            })
        })();
        let capability = admitted.map_err(|error| self.diagnostic(error))?;
        self.call_inner(&capability, arguments, cancel, Some(service))
            .await
    }

    /// Permanently closes process admission across this task's host scopes and
    /// requests cancellation. Cells should be closed first by the runtime.
    pub fn close_processes(&self) {
        if let Some(terminal) = &self.terminal {
            terminal.close();
        }
        self.runner.close();
    }

    /// Supervisors still owned by this task, including dropped call futures.
    pub fn active_processes(&self) -> usize {
        self.runner.active_count()
    }

    /// Terminal sessions whose original supervisor has not joined.
    pub fn active_terminal_sessions(&self) -> usize {
        self.terminal
            .as_ref()
            .map_or(0, |terminal| terminal.active_sessions())
    }

    /// Checks terminal completion before cleanup cancels any remaining sessions.
    pub fn terminal_completion_error(&self) -> Option<String> {
        self.terminal
            .as_ref()
            .and_then(|terminal| terminal.completion_error())
    }

    /// Waits for actual process cleanup and supervisor exit across all scopes.
    /// Runtime task reservations must remain held until this returns Ok. Dropping
    /// this future only requests cancellation; call again to establish cleanup.
    pub async fn drain_processes(&self) -> Result<(), cayw_device::DeviceError> {
        if let Some(terminal) = &self.terminal {
            terminal.close();
        }
        let terminal_result = match &self.terminal {
            Some(terminal) => terminal.drain().await,
            None => Ok(()),
        };
        let process_result = self.runner.drain().await;
        match (terminal_result, process_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(terminal), Err(process)) => Err(cayw_device::DeviceError::CleanupUnconfirmed(
                format!("terminal cleanup: {terminal}; process cleanup: {process}"),
            )),
        }
    }

    /// Unknown effects cannot be cleared by resetting budgets or reusing a host.
    /// The application must reconcile its durable records before creating another.
    pub fn outcome_unknown(&self) -> bool {
        self.safety.unknown.load(Ordering::Acquire)
            || self.runner.cleanup_failure().is_some()
            || self
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.unknown().is_some())
    }
    pub fn audit_failure(&self) -> Option<String> {
        lock(&self.safety.audit_failure).clone()
    }
    fn check(
        &self,
        cancel: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<(), CapabilityError> {
        if self.outcome_unknown() {
            let reason = self.audit_failure().map_or_else(
                || "a previous capability requires reconciliation".into(),
                |error| format!("audit persistence failed: {error}"),
            );
            return Err(CapabilityError::unknown(reason));
        }
        check(cancel, deadline)?;
        if lock(&self.budget).exhausted {
            return Err(CapabilityError::stopped(
                CapabilityStop::Budget,
                "host total byte budget exhausted",
            ));
        }
        Ok(())
    }
    fn admit(&self, bytes: usize) -> Result<ResponseBudget, CapabilityError> {
        let mut budget = lock(&self.budget);
        let available = self.config.max_total_bytes.map(|max_total| {
            max_total
                .saturating_sub(budget.bytes)
                .saturating_sub(budget.reserved)
        });
        // Reserve empty JSON messages for both the result sent to audit and a
        // possible settlement failure. Concurrent queued calls cannot consume it.
        if budget.exhausted
            || self
                .config
                .max_calls
                .is_some_and(|max_calls| budget.calls >= max_calls)
            || available
                .is_some_and(|available| bytes > available.saturating_sub(4) || available < 4)
        {
            return Err(CapabilityError::stopped(
                CapabilityStop::Budget,
                "host call or total byte budget exhausted",
            ));
        }
        budget.calls += 1;
        budget.bytes += bytes;
        budget.reserved += 4;
        Ok(ResponseBudget {
            host: self.clone(),
            reserved: 4,
        })
    }
    fn diagnostic(&self, error: CapabilityError) -> CapabilityError {
        ResponseBudget {
            host: self.clone(),
            reserved: 0,
        }
        .error(error)
    }
    async fn execute(
        &self,
        executor: Executor,
        name: &str,
        args: Value,
        cancel: &CancellationToken,
        deadline: Option<Instant>,
        service: Option<&dyn ExternalHost>,
    ) -> Result<Value, CapabilityError> {
        match executor {
            Executor::Filesystem => {
                if matches!(name, "fs.write" | "fs.patch") {
                    let (workspace, workspace_deadline) =
                        self.open_workspace(cancel, deadline).await?;
                    self.ensure_workspace_settled(&workspace, cancel, workspace_deadline)
                        .await?;
                    let _lock = filesystem::write_lock(&self.root, cancel, deadline).await?;
                    filesystem::execute(self, name, &args, cancel, deadline).await
                } else {
                    filesystem::execute(self, name, &args, cancel, deadline).await
                }
            }
            Executor::Git => git::execute(self, name, &args, cancel, deadline).await,
            Executor::Terminal => {
                let manager = self
                    .terminal
                    .as_ref()
                    .ok_or_else(|| failure("terminal handoffs are disabled"))?;
                terminal::execute(
                    manager,
                    &self.root,
                    self.runner.clone(),
                    name,
                    &args,
                    cancel,
                    deadline,
                )
                .await
            }
            Executor::Profile => {
                let (workspace, workspace_deadline) = self.open_workspace(cancel, deadline).await?;
                self.ensure_workspace_settled(&workspace, cancel, workspace_deadline)
                    .await?;
                let _lock = filesystem::write_lock(&self.root, cancel, deadline).await?;
                process::execute(self, name, &args, cancel, deadline).await
            }
            Executor::Data => {
                let limits = self
                    .config
                    .data
                    .ok_or_else(|| failure("data capabilities are disabled"))?;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(CapabilityError::stopped(CapabilityStop::Cancelled, "data capability cancelled")),
                    _ = sleep_deadline(deadline) => Err(CapabilityError::stopped(CapabilityStop::Timeout, "data capability deadline reached")),
                    result = cayw_data::execute(name, args, cancel.clone(), limits) => result,
                }
            }
            Executor::Workspace => {
                let (workspace, workspace_deadline) = self.open_workspace(cancel, deadline).await?;
                workspace
                    .execute_until(
                        name,
                        &args,
                        cancel.clone(),
                        deadline_instant(workspace_deadline),
                    )
                    .await
            }
            Executor::CodeIntel => codeintel::execute(self, name, &args, cancel, deadline).await,
            Executor::WebAccess => {
                let access = self
                    .web_access
                    .as_ref()
                    .ok_or_else(|| failure("web-access capabilities are disabled"))?;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(CapabilityError::stopped(CapabilityStop::Cancelled, "web-access capability cancelled")),
                    _ = sleep_deadline(deadline) => Err(CapabilityError::stopped(CapabilityStop::Timeout, "web-access capability deadline reached")),
                    result = access.call(name, args) => result.map_err(|error| {
                        let message = cayw_web_access::host_error_message(&error);
                        if error.outcome_unknown {
                            CapabilityError::unknown(message)
                        } else {
                            failure(message)
                        }
                    }),
                }
            }
            Executor::Vision => {
                let service = self
                    .vision
                    .as_ref()
                    .ok_or_else(|| failure("vision capabilities are disabled"))?
                    .clone();
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(CapabilityError::stopped(CapabilityStop::Cancelled, "vision capability cancelled")),
                    _ = sleep_deadline(deadline) => Err(CapabilityError::stopped(CapabilityStop::Timeout, "vision capability deadline reached")),
                    result = cayw_vision::execute(service, name, args, cancel.clone()) => result,
                }
            }
            Executor::External => {
                let external = service
                    .or(self.external.as_deref())
                    .ok_or_else(|| failure("external service is not configured"))?;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(CapabilityError::unknown("external capability cancelled after admission")),
                    _ = sleep_deadline(deadline) => Err(CapabilityError::unknown("external capability timed out after admission")),
                    result = external.call(name, args, cancel.clone()) => result,
                }
            }
        }
    }

    pub(crate) async fn open_workspace(
        &self,
        cancel: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<(cayw_workspace::Workspace, Option<Instant>), CapabilityError> {
        check(cancel, deadline)?;
        let mut limits = self.config.workspace.clone().unwrap_or_default();
        limits.max_file_bytes = min_opt(limits.max_file_bytes, self.config.max_file_bytes);
        limits.max_tree_entries = min_opt(limits.max_tree_entries, self.config.max_list_entries);
        let deadline = match limits.timeout {
            Some(timeout) => {
                let local = Instant::now()
                    .checked_add(timeout)
                    .ok_or_else(|| failure("invalid workspace deadline"))?;
                Some(deadline.map_or(local, |deadline| deadline.min(local)))
            }
            None => deadline,
        };
        loop {
            check(cancel, deadline)?;
            match cayw_workspace::Workspace::open_dir_until(
                self.root.try_clone().map_err(io_error)?,
                limits.clone(),
                cancel.clone(),
                deadline_instant(deadline),
            ) {
                Ok(workspace) => return Ok((workspace, deadline)),
                Err(error)
                    if !error.outcome_unknown
                        && error.message.starts_with("workspace is busy:") =>
                {
                    // A concurrent fork briefly inherits unrelated lock fds until
                    // exec. Treat that known, nonmutating refusal like write-lock
                    // admission, without extending the original call deadline.
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(CapabilityError::stopped(CapabilityStop::Cancelled, "cancelled while opening workspace")),
                        _ = sleep_deadline(deadline) => return Err(CapabilityError::stopped(CapabilityStop::Timeout, "workspace opening deadline reached")),
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {},
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Must be called while holding the same OS lock as the subsequent mutation.
    /// Prepared batches that never reached submission are inert and are
    /// discarded here. A pending interrupted journal is a known refusal before
    /// this call has changed anything; it does not clear any earlier unknown
    /// outcome on this host.
    async fn ensure_workspace_settled(
        &self,
        workspace: &cayw_workspace::Workspace,
        cancel: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<(), CapabilityError> {
        check(cancel, deadline)?;
        loop {
            match workspace.discard_prepared_batches() {
                Ok(_) => break,
                Err(error) if !error.outcome_unknown && error.message == "workspace is busy" => {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(CapabilityError::stopped(CapabilityStop::Cancelled, "cancelled while settling prepared workspace batches")),
                        _ = sleep_deadline(deadline) => return Err(CapabilityError::stopped(CapabilityStop::Timeout, "prepared workspace settlement deadline reached")),
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {},
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let status = workspace
            .execute_until(
                "fs.batch_status",
                &serde_json::json!({}),
                cancel.clone(),
                deadline_instant(deadline),
            )
            .await?;
        let batches = status
            .get("batches")
            .and_then(Value::as_array)
            .ok_or_else(|| failure("workspace status evidence is unavailable"))?;
        for batch in batches {
            if batch.get("outcome_unknown").and_then(Value::as_bool) != Some(false) {
                return Err(failure(
                    "workspace batch requires explicit recovery before writes or process profiles",
                ));
            }
        }
        check(cancel, deadline)
    }
}

#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.catalog
            .values()
            .map(|cap| cap.definition.clone())
            .collect()
    }
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let arguments = Arguments(arguments);
        let capability = self
            .catalog
            .get(name)
            .ok_or_else(|| self.diagnostic(failure("capability is not configured")))?;
        self.call_inner(capability, arguments, cancel, None).await
    }
}

impl Host {
    async fn call_inner(
        &self,
        capability: &Capability,
        arguments: Arguments,
        cancel: CancellationToken,
        service: Option<&dyn ExternalHost>,
    ) -> Result<Value, CapabilityError> {
        let deadline =
            call_deadline(self.config.call_timeout).map_err(|error| self.diagnostic(error))?;
        // Wait for any already-started call on this lane before validating or
        // asking permission, but do not hold the lane across an async permission
        // wait so another cell can run approval.pending / approval.decide.
        tokio::select! { biased;
            _ = cancel.cancelled() => return Err(self.diagnostic(CapabilityError::stopped(CapabilityStop::Cancelled, "capability cancelled while queued"))),
            _ = sleep_deadline(deadline) => return Err(self.diagnostic(CapabilityError::stopped(CapabilityStop::Timeout, "capability queue deadline reached"))),
            guard = self.execution.lock() => drop(guard),
        }
        let (mut call, mut lane) = self
            .admit_owned(
                capability,
                arguments,
                cancel,
                service.is_some(),
                true,
                deadline,
            )
            .await?;
        let name = capability.definition.name.as_str();
        let delegated = service.is_some_and(ExternalHost::delegates_workspace_calls);
        let may_mutate = service.is_some()
            || catalog::may_mutate(name)
            || matches!(capability.executor, Executor::External);
        let mutation = if may_mutate && !delegated {
            match self
                .mutation(
                    name,
                    &call.arguments.0,
                    &call.guard.id,
                    service.is_some()
                        || matches!(capability.executor, Executor::External | Executor::Profile),
                    &call.child,
                    call.deadline,
                )
                .await
            {
                Ok(mutation) => Some(mutation),
                Err(error) => return call.settle(Err(error), false).await,
            }
        } else {
            None
        };
        if delegated {
            drop(lane.take());
        }
        let result = match self.check(&call.child, call.deadline) {
            Ok(()) => {
                self.execute(
                    capability.executor,
                    name,
                    std::mem::take(&mut call.arguments.0),
                    &call.child,
                    call.deadline,
                    service,
                )
                .await
            }
            Err(error) => Err(error),
        };
        let result = match self.runner.cleanup_failure() {
            Some(message) => Err(CapabilityError::unknown(format!(
                "process resource cleanup is unconfirmed: {message}"
            ))),
            None => result,
        };
        let result = call.settle(result, may_mutate).await;
        if !result.as_ref().is_err_and(|error| error.outcome_unknown)
            && let Some(mutation) = mutation
        {
            mutation.settle(self)?;
        }
        drop(lane);
        result
    }

    /// The one admission pipeline used by ordinary calls and frozen read tickets.
    async fn admit_owned<'a>(
        &'a self,
        capability: &Capability,
        arguments: Arguments,
        cancel: CancellationToken,
        service: bool,
        lock_execution: bool,
        deadline: Option<Instant>,
    ) -> Result<(OwnedCall, Option<tokio::sync::MutexGuard<'a, ()>>), CapabilityError> {
        let name = capability.definition.name.as_str();
        let child = cancel.child_token();
        let cancel_on_drop = CancelCall(child.clone());
        let admission = (|| {
            self.check(&child, deadline)?;
            let argument_limit = match capability.executor {
                Executor::Data => self.config.data.and_then(|limits| limits.input_bytes),
                Executor::Workspace => self
                    .config
                    .workspace
                    .as_ref()
                    .and_then(|limits| limits.max_argument_bytes),
                Executor::Vision => self.config.vision.map(|limits| limits.input_bytes),
                _ => None,
            };
            let argument_limit = min_opt(argument_limit, self.config.max_argument_bytes);
            let size = json_size(&arguments.0, argument_limit)?;
            capability
                .validator
                .validate(&arguments.0)
                .map_err(|error| failure(error.to_string()))?;
            if !service {
                catalog::validate_arguments(name, &arguments.0, &self.config)?;
            }
            self.admit(size)
        })();
        let mut response = match admission {
            Ok(admitted) => admitted,
            Err(error) => return Err(self.diagnostic(error)),
        };
        // Permission may wait for a user. It holds no lane needed by a reader
        // completing its original Core/Cells acknowledgment.
        self.check(&child, deadline)
            .map_err(|error| response.error(error))?;
        tokio::select! {
            biased;
            _ = child.cancelled() => return Err(response.error(CapabilityError::stopped(CapabilityStop::Cancelled, "capability cancelled before permission"))),
            _ = sleep_deadline(deadline) => return Err(response.error(CapabilityError::stopped(CapabilityStop::Timeout, "capability permission deadline reached"))),
            result = self.gate.authorize(name, &arguments.0, child.clone()) => {
                if let Err(error) = result {
                    if error.outcome_unknown { self.safety.unknown.store(true, Ordering::Release); }
                    return Err(response.error(error));
                }
            },
        }
        let lane = if lock_execution {
            Some(tokio::select! {
                biased;
                _ = child.cancelled() => return Err(response.error(CapabilityError::stopped(CapabilityStop::Cancelled, "capability cancelled while queued"))),
                _ = sleep_deadline(deadline) => return Err(response.error(CapabilityError::stopped(CapabilityStop::Timeout, "capability queue deadline reached"))),
                guard = self.execution.lock() => guard,
            })
        } else {
            None
        };
        self.check(&child, deadline)
            .map_err(|error| response.error(error))?;
        let mut starting = StartingGuard {
            safety: self.safety.clone(),
            cancel: child.clone(),
            max_error_bytes: self.config.max_result_bytes,
            pending: true,
        };
        let started = self.audit.started(name, &arguments.0).await;
        starting.pending = false;
        let id = started.map_err(|error| {
            if error.outcome_unknown {
                self.safety.unknown.store(true, Ordering::Release);
            }
            response.error(error)
        })?;
        let guard = CallGuard {
            id,
            audit: self.audit.clone(),
            safety: self.safety.clone(),
            cancel: child.clone(),
            max_error_bytes: self.config.max_result_bytes,
            settled: false,
        };
        Ok((
            OwnedCall {
                host: self.clone(),
                arguments,
                response,
                child,
                deadline,
                guard,
                _cancel_on_drop: cancel_on_drop,
            },
            lane,
        ))
    }
    /// Independently validates, charges and authorizes this exact original call.
    /// The non-Clone ticket owns its audit and the shared generation lease.
    pub async fn admit_frozen_read(
        &self,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<FrozenReadTicket, CapabilityError> {
        let capability = self.catalog.get("fs.read").expect("filesystem catalog");
        let deadline =
            call_deadline(self.config.call_timeout).map_err(|error| self.diagnostic(error))?;
        let (mut call, _) = self
            .admit_owned(
                capability,
                Arguments(arguments),
                cancel,
                false,
                false,
                deadline,
            )
            .await?;
        match self.read_lease(&call.child, call.deadline).await {
            Ok(lease) => Ok(FrozenReadTicket::new(call, lease)),
            Err(error) => {
                let error = call
                    .settle(Err(error), false)
                    .await
                    .expect_err("read lease failed");
                Err(error)
            }
        }
    }
}

struct OwnedCall {
    host: Host,
    arguments: Arguments,
    response: ResponseBudget,
    child: CancellationToken,
    deadline: Option<Instant>,
    guard: CallGuard,
    _cancel_on_drop: CancelCall,
}
struct CancelCall(CancellationToken);
impl Drop for CancelCall {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
impl OwnedCall {
    async fn settle(
        &mut self,
        result: Result<Value, CapabilityError>,
        may_mutate: bool,
    ) -> Result<Value, CapabilityError> {
        if self.guard.settled {
            return Err(failure("Host call already settled"));
        }
        self.child.cancel();
        let result = self.response.result(result, may_mutate);
        if result.as_ref().is_err_and(|error| error.outcome_unknown) {
            self.host.safety.unknown.store(true, Ordering::Release);
        }
        if let Err(mut error) = self.host.audit.settled(&self.guard.id, &result).await {
            error.message = fit_message(error.message, self.host.config.max_result_bytes);
            *lock(&self.host.safety.audit_failure) = Some(error.message.clone());
            self.host.safety.unknown.store(true, Ordering::Release);
            return Err(self.response.error(CapabilityError::unknown(format!(
                "could not durably settle capability: {}",
                error.message
            ))));
        }
        self.guard.settled = true;
        result
    }
}

// Each admitted call reserves a result plus a possible audit-settlement error.
// Reservations are shared across scopes and released even if its future drops.
struct ResponseBudget {
    host: Host,
    reserved: usize,
}
impl ResponseBudget {
    fn available(&self, budget: &Budget, keep: usize) -> Option<usize> {
        self.host.config.max_total_bytes.map(|max_total| {
            max_total
                .saturating_sub(budget.bytes)
                .saturating_sub(budget.reserved.saturating_sub(self.reserved))
                .saturating_sub(keep)
        })
    }
    fn release(&mut self, budget: &mut Budget, keep: usize) {
        budget.reserved -= self.reserved.saturating_sub(keep);
        self.reserved = keep;
    }
    fn error(&mut self, error: CapabilityError) -> CapabilityError {
        self.error_keeping(error, 0)
    }
    fn error_keeping(&mut self, mut error: CapabilityError, keep: usize) -> CapabilityError {
        let shared = self.host.budget.clone();
        let mut budget = lock(&shared);
        let available = self.available(&budget, keep);
        // An already exhausted caller has no encodable result allowance. Return
        // only the empty terminal control error, without another admission. All
        // encodable diagnostics below are charged at their full serialized size.
        if available.is_some_and(|available| available < 2) {
            budget.exhausted = true;
            error.stop.get_or_insert(CapabilityStop::Budget);
            error.message.clear();
            self.release(&mut budget, keep);
            return error;
        }
        let result_limit = self.host.config.max_result_bytes;
        error.message = fit_message(error.message, result_limit);
        let limit = min_opt(result_limit, available);
        let size = serde_json::to_vec(&error.message)
            .expect("string JSON serialization")
            .len();
        if available.is_some_and(|available| size > available) {
            budget.exhausted = true;
            error.stop.get_or_insert(CapabilityStop::Budget);
            error.message = fit_message(error.message, limit);
        }
        let size = serde_json::to_vec(&error.message)
            .expect("string JSON serialization")
            .len();
        budget.bytes += size;
        self.release(&mut budget, keep);
        error
    }
    fn result(
        &mut self,
        result: Result<Value, CapabilityError>,
        may_mutate: bool,
    ) -> Result<Value, CapabilityError> {
        let value = match result {
            Ok(value) => value,
            Err(error) => return Err(self.error_keeping(error, 2)),
        };
        let charged = {
            let shared = self.host.budget.clone();
            let mut budget = lock(&shared);
            let limit = min_opt(
                self.host.config.max_result_bytes,
                self.available(&budget, 2),
            );
            match json_size(&value, limit) {
                Ok(size) => {
                    budget.bytes += size;
                    self.release(&mut budget, 2);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        };
        match charged {
            Ok(()) => Ok(value),
            Err(error) => Err(self.error_keeping(
                if may_mutate {
                    CapabilityError::unknown(error.message)
                } else {
                    error
                },
                2,
            )),
        }
    }
}
impl Drop for ResponseBudget {
    fn drop(&mut self) {
        lock(&self.host.budget).reserved -= self.reserved;
    }
}

/// Fits the serialized JSON string, including quotes, escapes and UTF-8 bytes.
/// `None` leaves the message unchanged.
fn fit_message(mut message: String, limit: Option<usize>) -> String {
    let Some(limit) = limit else {
        return message;
    };
    let mut used = 2usize;
    let mut end = 0;
    for (at, character) in message.char_indices() {
        let bytes = match character {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            '\u{0}'..='\u{1f}' => 6,
            _ => character.len_utf8(),
        };
        if bytes > limit.saturating_sub(used) {
            break;
        }
        used += bytes;
        end = at + character.len_utf8();
    }
    message.truncate(end);
    message
}

// Rejected direct Rust calls may supply a Value far deeper than the host limit.
// Dispose of those iteratively even on cancellation or an unconfigured name.
struct Arguments(Value);
impl Drop for Arguments {
    fn drop(&mut self) {
        let mut pending = vec![std::mem::take(&mut self.0)];
        while let Some(value) = pending.pop() {
            match value {
                Value::Array(values) => pending.extend(values),
                Value::Object(values) => pending.extend(values.into_values()),
                _ => {}
            }
        }
    }
}

// If starting was interrupted, the sink may already have durably committed a
// running record whose ID was never returned. Preserve that uncertainty too.
struct StartingGuard {
    safety: Arc<Safety>,
    cancel: CancellationToken,
    max_error_bytes: Option<usize>,
    pending: bool,
}
impl Drop for StartingGuard {
    fn drop(&mut self) {
        if self.pending {
            self.cancel.cancel();
            self.safety.unknown.store(true, Ordering::Release);
            *lock(&self.safety.audit_failure) = Some(fit_message(
                "audit start was interrupted before its durable result was confirmed".into(),
                self.max_error_bytes,
            ));
        }
    }
}

struct CallGuard {
    id: String,
    audit: Arc<dyn AuditSink>,
    safety: Arc<Safety>,
    cancel: CancellationToken,
    max_error_bytes: Option<usize>,
    settled: bool,
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        if self.settled {
            return;
        }
        self.safety.unknown.store(true, Ordering::Release);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.audit.dropped(&self.id)
        }));
        let failure = match result {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error.message,
            Err(_) => "audit dropped callback panicked".into(),
        };
        *lock(&self.safety.audit_failure) = Some(fit_message(failure, self.max_error_bytes));
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
pub(crate) fn failure(message: impl Into<String>) -> CapabilityError {
    CapabilityError::new(message)
}
pub(crate) fn io_error(error: io::Error) -> CapabilityError {
    failure(error.to_string())
}
/// Internal executors take an optional absolute deadline. `None` means the
/// wait ends only by cancellation or completion.
pub(crate) fn call_deadline(timeout: Option<Duration>) -> Result<Option<Instant>, CapabilityError> {
    timeout
        .map(|timeout| {
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| failure("invalid deadline"))
        })
        .transpose()
}
pub(crate) fn check(
    cancel: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<(), CapabilityError> {
    if cancel.is_cancelled() {
        return Err(CapabilityError::stopped(
            CapabilityStop::Cancelled,
            "capability cancelled",
        ));
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(CapabilityError::stopped(
            CapabilityStop::Timeout,
            "capability deadline reached",
        ));
    }
    Ok(())
}
/// Waits until the optional deadline; without one it never resolves on its own.
pub(crate) async fn sleep_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}
/// `cayw_workspace` still takes an absolute `Instant`. An absent host deadline
/// becomes a far-future sentinel only at that boundary, never a product limit.
pub(crate) fn deadline_instant(deadline: Option<Instant>) -> Instant {
    deadline.unwrap_or_else(|| Instant::now() + UNBOUNDED_WAIT)
}
/// Combines optional bounds. `None` means unbounded; a `Some` value caps the
/// other side, and two `Some` values keep the smaller one.
pub(crate) fn min_opt<T: Ord>(left: Option<T>, right: Option<T>) -> Option<T> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}
/// An unconfigured call timeout means the wait ends only by cancellation or
/// completion, so the workspace-boundary sentinel is far beyond any real run
/// rather than a product limit.
const UNBOUNDED_WAIT: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

fn json_size(value: &Value, limit: Option<usize>) -> Result<usize, CapabilityError> {
    // Exact canonical JSON byte count, computed with an explicit stack so an
    // arbitrarily deep value cannot overflow the call stack. Only an explicitly
    // configured byte limit rejects the value.
    fn string_size(value: &str) -> Result<usize, CapabilityError> {
        serde_json::to_string(value)
            .map(|value| value.len())
            .map_err(|error| CapabilityError::stopped(CapabilityStop::Budget, error.to_string()))
    }
    fn add(used: &mut usize, bytes: usize, limit: Option<usize>) -> Result<(), CapabilityError> {
        *used = used.checked_add(bytes).ok_or_else(|| {
            CapabilityError::stopped(CapabilityStop::Budget, "JSON byte size overflow")
        })?;
        if limit.is_some_and(|limit| *used > limit) {
            return Err(CapabilityError::stopped(
                CapabilityStop::Budget,
                "JSON byte budget exceeded",
            ));
        }
        Ok(())
    }
    let mut used = 0usize;
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Null => add(&mut used, 4, limit)?,
            Value::Bool(true) => add(&mut used, 4, limit)?,
            Value::Bool(false) => add(&mut used, 5, limit)?,
            Value::Number(number) => add(&mut used, number.to_string().len(), limit)?,
            Value::String(value) => add(&mut used, string_size(value)?, limit)?,
            Value::Array(values) => {
                add(&mut used, 2 + values.len().saturating_sub(1), limit)?;
                pending.extend(values);
            }
            Value::Object(values) => {
                add(&mut used, 2 + values.len().saturating_sub(1), limit)?;
                for (key, value) in values {
                    add(&mut used, string_size(key)? + 1, limit)?;
                    pending.push(value);
                }
            }
        }
    }
    Ok(used)
}
