use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// Application-supplied identity, never inferred from environment or filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    pub id: String,
    pub version: String,
    pub origin: String,
}

/// The digest covers the complete immutable manifest, including every skill digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Opaque identity of this exact registry lifetime, including across restarts.
    pub registry_incarnation: String,
    pub source: SourceIdentity,
    pub generation: u64,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
}
impl Drop for ToolSpec {
    fn drop(&mut self) {
        crate::bounds::drop_json(self.input_schema.take());
        if let Some(value) = self.output_schema.take() {
            crate::bounds::drop_json(value);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSpec {
    /// A local name. Public names are qualified as `source_id/name`.
    pub name: String,
    pub description: String,
    pub content_sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub tools: Vec<ToolSpec>,
    pub skills: Vec<SkillSpec>,
    /// At most one source can own the host's unqualified `search.query` entry.
    pub search: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    ToolName,
    ToolSchema,
    SkillName,
    SkillContent,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Discover,
    Describe,
    Invoke,
    ListSkills,
    ReadSkill,
    Search,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccessRequest {
    pub source: Provenance,
    pub operation: Operation,
    /// Local tool/skill name (empty for listing); this is not an execution grant.
    pub target: String,
    pub arguments: Value,
}
impl Drop for AccessRequest {
    fn drop(&mut self) {
        crate::bounds::drop_json(self.arguments.take());
    }
}

/// The registry filters each kind of metadata separately. Authorize is awaited
/// for every operation, including every execute; a discovery response is data.
/// The application's Host PermissionGate remains a separate mandatory boundary.
#[async_trait]
pub trait RegistryPolicy: Send + Sync {
    fn visible(&self, kind: Visibility, source: &Provenance, name: &str) -> bool;
    async fn authorize(
        &self,
        request: &AccessRequest,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestKey {
    pub id: u64,
    pub source: Provenance,
    pub operation: Operation,
    pub target: String,
}

/// Trusted runtime identity for one capability invocation, never parsed from
/// model-controlled arguments. Sequence identifies a call inside an execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationContext {
    pub session_id: String,
    pub task_id: String,
    pub attempt_id: String,
    pub execution_id: String,
    pub sequence: u64,
}

/// Exact immutable catalog pinned by a consumer, not a live source status view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingSnapshot {
    pub registry_incarnation: String,
    pub sources: std::collections::BTreeMap<String, Provenance>,
}

/// Persist before polling the source. Request identity includes registry lifetime,
/// source version/origin, generation, manifest digest, operation and local target.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchRecord {
    pub context: InvocationContext,
    pub capability: String,
    pub key: RequestKey,
    pub arguments: Value,
    pub arguments_sha256: String,
}
impl Drop for DispatchRecord {
    fn drop(&mut self) {
        crate::bounds::drop_json(self.arguments.take());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum DispatchOutcome {
    Succeeded { value: Value },
    Failed { message: String },
    NotExecuted { message: String },
    Unknown { message: String },
}
impl Drop for DispatchOutcome {
    fn drop(&mut self) {
        if let Self::Succeeded { value } = self {
            crate::bounds::drop_json(value.take());
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchDropState {
    NotExecuted,
    Unknown,
}

/// Runtime-owned durable storage. All callbacks run without registry locks.
/// `started` must atomically persist the full record before returning success.
/// `settled` commits the terminal outcome before returning. Cancellation of either
/// future must not leave detached writers that can overwrite a later `dropped`.
/// Calls use the complete context + RequestKey as idempotent identity; terminal
/// records must never be overwritten by an older prepare. `dropped` synchronously
/// commits NotExecuted before dispatch, or Unknown once dispatch has started.
/// Failure of the hook stops this registry and is exposed by `journal_failure()`.
#[async_trait]
pub trait DispatchJournal: Send + Sync {
    async fn started(&self, record: &DispatchRecord) -> Result<(), CapabilityError>;
    async fn settled(
        &self,
        record: &DispatchRecord,
        outcome: &DispatchOutcome,
    ) -> Result<(), CapabilityError>;
    fn dropped(
        &self,
        record: &DispatchRecord,
        state: DispatchDropState,
    ) -> Result<(), CapabilityError>;
}

#[derive(Debug, Clone)]
pub enum SourceOperation {
    InvokeTool { name: String, arguments: Value },
    ReadSkill { name: String },
    Search { arguments: Value },
}

#[derive(Debug, Clone)]
pub struct SourceRequest {
    pub key: RequestKey,
    pub operation: SourceOperation,
    /// A source must bound allocation/IO before constructing its reply too.
    pub max_result_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct SourceReply {
    pub key: RequestKey,
    pub value: Value,
}
impl Drop for SourceReply {
    fn drop(&mut self) {
        crate::bounds::drop_json(self.value.take());
    }
}

/// An already-created application handle, moved into exactly one registry slot.
/// This crate creates no connection, loader, subprocess, or filesystem handle.
/// A handle must echo the full RequestKey, stop waiting on cancellation/drop,
/// and never silently detach effects. Ordinary Err asserts a known failure;
/// uncertain effects must return CapabilityError::unknown. `disconnect` must
/// synchronously initiate transport shutdown and is called at most once.
#[async_trait]
pub trait SourceHandle: Send + Sync {
    /// Unique physical connection/loader ownership key, supplied by its creator.
    /// All wrappers around the same connection must return the same key. Moving
    /// this handle transfers use/shutdown authority; the creator must not keep
    /// making independent calls through another wrapper. Rust cannot verify an
    /// application's physical connection identity. A rejected duplicate key is
    /// discarded without calling disconnect, protecting the existing owner.
    /// Keys are consumed before metadata validation and never released, including
    /// failed installs. A rejected wrapper's own Drop must not shut down a shared
    /// connection; shutdown authority belongs to this explicit disconnect hook.
    fn ownership_key(&self) -> &str;
    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError>;
    fn disconnect(&self);
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_sources: usize,
    pub max_tools_per_source: usize,
    pub max_skills_per_source: usize,
    pub max_metadata_bytes: usize,
    pub max_all_metadata_bytes: usize,
    pub max_argument_bytes: usize,
    pub max_result_bytes: usize,
    pub max_skill_bytes: usize,
    /// Includes metadata operations, denied policy operations, and reconnections.
    pub max_source_operations: usize,
    /// Arguments and replies over all generations of one source.
    pub max_source_bytes: usize,
    pub max_in_flight: usize,
    /// Validation, policy callbacks, journal waits and handle execution share one
    /// absolute deadline, starting when the scoped ExternalHost::call is created.
    /// User synchronous callbacks cannot be preempted; time is checked again
    /// before dispatch and after settlement. The synchronous dropped hook must
    /// persist its state even if that deadline has already elapsed.
    pub operation_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sources: 32,
            max_tools_per_source: 128,
            max_skills_per_source: 128,
            max_metadata_bytes: 256 * 1024,
            max_all_metadata_bytes: 2 * 1024 * 1024,
            max_argument_bytes: 64 * 1024,
            max_result_bytes: 256 * 1024,
            max_skill_bytes: 64 * 1024,
            max_source_operations: 1024,
            max_source_bytes: 8 * 1024 * 1024,
            max_in_flight: 8,
            operation_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    Connected,
    Disconnected,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnresolvedCall {
    pub key: RequestKey,
    pub arguments_sha256: String,
    /// True even while awaiting a response: a crash would require reconciliation.
    pub outcome_unknown: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceSnapshot {
    pub provenance: Provenance,
    pub status: SourceStatus,
    pub operations: usize,
    pub bytes: usize,
    pub outcome_unknown: bool,
    pub unresolved: Vec<UnresolvedCall>,
}
