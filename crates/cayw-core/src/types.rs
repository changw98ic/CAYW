use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{Message, ToolCall, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One canonical wire/storage contract. Older storage is migrated atomically.
pub const PROTOCOL_VERSION: u32 = 3;
pub const SCHEMA_VERSION: u32 = 3;
pub const MAX_EVENT_PAGE: usize = 1_000;
pub const MAX_IDENTIFIER_BYTES: usize = 256;
pub const MAX_METADATA_BYTES: usize = 65_536;
pub const MAX_METADATA_DEPTH: usize = 32;
pub const MAX_METADATA_NODES: usize = 4_096;
pub const MAX_EXECUTIONS: usize = 4_096;
pub const MAX_CAPABILITY_CALLS: usize = 65_536;
pub const MAX_APPROVALS: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Running,
    AwaitingApproval,
    NeedsReconciliation,
}

/// Every execution write checks the entire lease in its write transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptLease {
    pub session_id: String,
    pub attempt_id: String,
    pub worker_id: String,
    pub fence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Planned,
    Prepared,
    Started,
    Completed,
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallRecord {
    pub call: ToolCall,
    pub attempt_id: String,
    pub state: CallState,
    /// Only `mark_tool_started` crosses the model tool execution boundary.
    pub execution_started: bool,
    pub result: Option<ToolResultMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionInput {
    pub execution_id: String,
    pub parent_call_id: String,
    pub task_id: String,
    pub source_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Running,
    Yielded,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOutcome {
    Completed {
        result: Value,
    },
    Failed {
        error: String,
    },
    Cancelled {
        reason: String,
    },
    /// The VM is gone. Core never automatically replays its source.
    Interrupted {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecord {
    pub execution_id: String,
    pub parent_call_id: String,
    pub task_id: String,
    pub source_hash: String,
    pub lease: AttemptLease,
    pub state: ExecutionState,
    pub terminal: Option<ExecutionOutcome>,
}

/// Trusted binding for a started execute_code wait to its independently tracked VM.
/// Retains the original complete lease, task and exact arguments for audit/recovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWaitRecord {
    pub call_id: String,
    pub execution_id: String,
    pub task_id: String,
    pub lease: AttemptLease,
    pub arguments: Value,
}

/// Sequence starts at zero and increases contiguously within one execution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityCallKey {
    pub execution_id: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityInput {
    pub key: CapabilityCallKey,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStage {
    Planned,
    Started,
    Settled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityOutcome {
    Completed {
        result: Value,
    },
    /// A known failure; the host knows whether its side effects occurred.
    Failed {
        error: String,
    },
    NotExecuted {
        reason: String,
    },
    OutcomeUnknown {
        error: String,
    },
}

/// Supplied only by the trusted host, after its permission/budget checks.
/// An approval is exact to this child target; model arguments cannot supply it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityAuthorization {
    pub actor: String,
    pub target: CapabilityCallKey,
    pub task_id: String,
    pub capability: String,
    pub scope: String,
    pub decision: ApprovalDecision,
    pub approval_id: Option<String>,
    pub external_provenance: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityCallRecord {
    pub key: CapabilityCallKey,
    pub name: String,
    pub arguments: Value,
    pub parent_call_id: String,
    pub task_id: String,
    pub source_hash: String,
    pub lease: AttemptLease,
    pub stage: CapabilityStage,
    pub authorization: Option<Value>,
    pub outcome: Option<CapabilityOutcome>,
    pub external_provenance: Option<Value>,
}

/// The host must identify the complete original execution, never a replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationOrigin {
    pub lease: AttemptLease,
    pub task_id: String,
    pub execution_id: String,
    pub parent_call_id: String,
    pub source_hash: String,
}

impl From<&CapabilityCallRecord> for Box<ReconciliationOrigin> {
    fn from(call: &CapabilityCallRecord) -> Self {
        Box::new(ReconciliationOrigin::from(call))
    }
}

impl From<&CapabilityCallRecord> for ReconciliationOrigin {
    fn from(call: &CapabilityCallRecord) -> Self {
        Self {
            lease: call.lease.clone(),
            task_id: call.task_id.clone(),
            execution_id: call.key.execution_id.clone(),
            parent_call_id: call.parent_call_id.clone(),
            source_hash: call.source_hash.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityReconciliation {
    pub key: CapabilityCallKey,
    pub expected: Box<ReconciliationOrigin>,
    pub actor: String,
    pub evidence: String,
    /// Must be a known outcome. Reconciliation never edits a model tool reply.
    pub outcome: CapabilityOutcome,
    pub external_provenance: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalTarget {
    ToolCall { call_id: String },
    CapabilityCall { key: CapabilityCallKey },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalInput {
    pub task_id: String,
    pub target: ApprovalTarget,
    pub capability: String,
    pub scope: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolution {
    pub decision: ApprovalDecision,
    pub actor: String,
    pub decided_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    pub id: String,
    pub session_id: String,
    pub attempt_id: String,
    pub input: ApprovalInput,
    pub requested_at: u64,
    pub decision: Option<ApprovalResolution>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReconciledOutcome {
    Completed { result: ToolResult },
    NotExecuted { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolReconciliationOrigin {
    pub lease: AttemptLease,
    pub task_id: String,
    pub call: ToolCall,
    /// Present only when the original host registered an execution or wait.
    pub execution: Option<ExecutionInput>,
}

/// A trusted host must authorize the actor and verify evidence for this exact
/// identity and proposed outcome. A nonempty actor/evidence string grants nothing.
/// Implementations understand their own receipt format; this is not a general
/// detector for arbitrary forged text. No verifier is installed by default.
pub trait ReconciliationVerifier: Send + Sync {
    fn verify(&self, request: &ReconciliationRequest) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReconciliationRequest {
    Tool {
        reconciliation: Reconciliation,
    },
    Capability {
        reconciliation: CapabilityReconciliation,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reconciliation {
    pub call_id: String,
    pub expected: Box<ToolReconciliationOrigin>,
    pub actor: String,
    /// Evidence supplied by the trusted host, retained in the append-only log.
    pub evidence: String,
    pub outcome: ReconciledOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub id: String,
    pub label: String,
    pub messages: Vec<Message>,
    pub cursor: u64,
    pub state: SessionState,
    pub attempt: Option<AttemptLease>,
    pub pending_approvals: Vec<ApprovalRequest>,
    pub unresolved_calls: Vec<CallRecord>,
    pub executions: Vec<ExecutionRecord>,
    pub unresolved_capabilities: Vec<CapabilityCallRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreEvent {
    SessionCreated {
        label: String,
    },
    SessionForked {
        source_session_id: String,
        source_cursor: u64,
        messages: Vec<Message>,
    },
    AttemptStarted {
        lease: AttemptLease,
    },
    Agent {
        event: Box<AgentEvent>,
    },
    ToolOriginBound {
        call_id: String,
        lease: AttemptLease,
        task_id: String,
    },
    ToolStarted {
        call_id: String,
    },
    ExecutionStarted {
        execution: ExecutionRecord,
    },
    ExecutionYielded {
        execution: ExecutionRecord,
    },
    ExecutionWaitRegistered {
        binding: ExecutionWaitRecord,
    },
    ExecutionFinished {
        execution: ExecutionRecord,
    },
    CapabilityPlanned {
        call: CapabilityCallRecord,
    },
    CapabilityStarted {
        call: CapabilityCallRecord,
    },
    CapabilitySettled {
        call: CapabilityCallRecord,
    },
    CapabilityReconciled {
        reconciliation: CapabilityReconciliation,
        call: CapabilityCallRecord,
    },
    AttemptFinished {
        lease: AttemptLease,
    },
    AttemptRecovered {
        lease: Option<AttemptLease>,
        unknown_call_ids: Vec<String>,
        not_executed_call_ids: Vec<String>,
        executions: Vec<ExecutionRecord>,
        capabilities: Vec<CapabilityCallRecord>,
    },
    ApprovalRequested {
        approval: ApprovalRequest,
    },
    ApprovalDecided {
        approval: ApprovalRequest,
    },
    Reconciled {
        reconciliation: Reconciliation,
    },
    Host {
        kind: String,
        data: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventRecord {
    pub protocol_version: u32,
    pub session_id: String,
    /// Monotonic within the session; `events(after, limit)` excludes `after`.
    pub cursor: u64,
    pub timestamp: u64,
    pub attempt: Option<AttemptLease>,
    pub event: CoreEvent,
}
