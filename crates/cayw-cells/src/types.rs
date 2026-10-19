use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

pub type CellId = String;

/// These values come from Runtime, never from model arguments or Luau.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellIdentity {
    pub session_id: String,
    pub task_id: String,
    pub attempt_id: String,
    pub permission_context: String,
}
impl CellIdentity {
    pub(crate) fn validate(&self) -> Result<(), CellError> {
        for value in [
            &self.session_id,
            &self.task_id,
            &self.attempt_id,
            &self.permission_context,
        ] {
            if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                return Err(CellError::Invalid(
                    "identity fields require 1..=256 non-control bytes".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Immutable trusted origin for one cell, retained across exec/wait replies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellExecution {
    pub identity: CellIdentity,
    pub cell_id: CellId,
    pub parent_call_id: String,
    pub source_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityCallKey {
    pub cell_id: CellId,
    pub sequence: u64,
}

/// Terminal metadata passed to the trusted backend after durable VM settlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellTerminal {
    pub status: CellStatus,
    pub error: Option<String>,
    pub outcome_unknown: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosedCell {
    pub execution: CellExecution,
    pub terminal: CellTerminal,
}

/// Returned only after all owned VM futures have stopped. Unknown capability
/// outcomes and backend failures remain explicit in each terminal and the flag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellCloseReport {
    pub cells: Vec<ClosedCell>,
    pub outcome_unknown: bool,
}

/// This assertion is supplied only by the owner after stopping its old executor.
/// Acquiring the task execution lock is still required: an assertion cannot
/// steal a scope from a live manager in this or another process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorStopped {
    pub identity: CellIdentity,
    pub evidence: String,
}

#[derive(Clone, Debug)]
pub struct CellLimits {
    pub max_cells: usize,
    pub max_running: usize,
    pub script_bytes: usize,
    pub output_bytes: usize,
    pub output_items: usize,
    pub item_bytes: usize,
    pub media_bytes: usize,
    pub max_output_tokens: usize,
    pub max_yield_time: Duration,
    pub store_bytes: usize,
    pub store_keys: usize,
    pub json_depth: usize,
    pub cache_entries: usize,
}
impl Default for CellLimits {
    fn default() -> Self {
        Self {
            max_cells: 256,
            max_running: 8,
            script_bytes: 64 * 1024,
            output_bytes: 1024 * 1024,
            output_items: 256,
            item_bytes: 64 * 1024,
            media_bytes: 32 * 1024,
            max_output_tokens: 128 * 1024,
            max_yield_time: Duration::from_secs(30),
            store_bytes: 1024 * 1024,
            store_keys: 256,
            json_depth: 32,
            cache_entries: 128,
        }
    }
}
impl CellLimits {
    pub(crate) fn validate(&self) -> Result<(), CellError> {
        if self.max_cells == 0
            || self.max_cells > 10_000
            || self.max_running == 0
            || self.max_running > self.max_cells
            || self.script_bytes == 0
            || self.output_bytes == 0
            || self.output_items == 0
            || self.output_items > 10_000
            || self.output_bytes > 64 * 1024 * 1024
            || self.store_bytes > 64 * 1024 * 1024
            || self.script_bytes > 1024 * 1024
            || self.media_bytes > 8 * 1024 * 1024
            || self.store_keys > 10_000
            || self.cache_entries > 4096
            || self.item_bytes == 0
            || self.media_bytes == 0
            || self.store_bytes == 0
            || self.store_keys == 0
            || self.json_depth == 0
            || self.json_depth > 64
            || self.max_output_tokens < 1024
            || self.max_output_tokens > 16 * 1024 * 1024
            || self.item_bytes > self.max_output_tokens.saturating_sub(1024)
            || self.max_yield_time.is_zero()
            || self.max_yield_time > Duration::from_secs(300)
        {
            return Err(CellError::Invalid("invalid cell limits".into()));
        }
        Ok(())
    }
    pub(crate) fn page_limits(&self, wait: Duration, tokens: usize) -> Result<(), CellError> {
        if wait > self.max_yield_time || !(1024..=self.max_output_tokens).contains(&tokens) {
            return Err(CellError::Invalid(
                "yield_time_ms or max_output_tokens exceeds limits (minimum budget 1024)".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CellError {
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("cell not found in this session/task/attempt")]
    NotFound,
    #[error("cell manager is closed")]
    Closed,
    #[error("cell capacity exhausted")]
    Capacity,
    #[error("cell state requires evidence before more execution")]
    Frozen,
    #[error("session value version does not match")]
    Conflict,
    #[error("journal: {0}")]
    Journal(String),
}
impl From<rusqlite::Error> for CellError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Journal(value.to_string())
    }
}
impl From<serde_json::Error> for CellError {
    fn from(value: serde_json::Error) -> Self {
        Self::Journal(value.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    NotStarted,
    Running,
    Yielded,
    Completed,
    Exited,
    Failed,
    Cancelled,
    /// The VM is gone; no uncertain real call exists. Never replayed automatically.
    Interrupted,
    OutcomeUnknown,
    /// Evidence settled the uncertain calls. The lost VM remains non-resumable.
    Reconciled,
}
impl CellStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::NotStarted | Self::Running | Self::Yielded)
    }
}

/// Fingerprints and exact versions must be supplied by the trusted host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencySnapshot {
    pub fingerprint: String,
    pub versions: BTreeMap<String, String>,
    pub permission_context: String,
}
impl DependencySnapshot {
    pub(crate) fn validate(&self, context: &str, limits: &CellLimits) -> Result<(), CellError> {
        if self.fingerprint.is_empty()
            || self.permission_context != context
            || self.versions.is_empty()
        {
            return Err(CellError::Invalid(
                "read authorization requires exact host dependencies and permission context".into(),
            ));
        }
        validate_json(
            &serde_json::to_value(self)?,
            limits.json_depth,
            limits.item_bytes,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CellOutput {
    Text {
        text: String,
    },
    Image {
        mime_type: String,
        data: String,
    },
    Audio {
        mime_type: String,
        data: String,
    },
    /// Progress metadata only. Binary or arbitrary nested payloads are forbidden.
    Notify {
        message: String,
        completed: Option<u64>,
        total: Option<u64>,
    },
    Exit {
        code: i32,
    },
    Json {
        value: Value,
    },
}
impl CellOutput {
    pub(crate) fn validate(&self, limits: &CellLimits) -> Result<usize, CellError> {
        match self {
            Self::Image { mime_type, data } => {
                if !["image/png", "image/jpeg", "image/webp", "image/gif"]
                    .contains(&mime_type.as_str())
                {
                    return Err(CellError::Invalid("unsupported image MIME type".into()));
                }
                media(data, limits.media_bytes)?;
            }
            Self::Audio { mime_type, data } => {
                if ![
                    "audio/wav",
                    "audio/mpeg",
                    "audio/ogg",
                    "audio/webm",
                    "audio/flac",
                ]
                .contains(&mime_type.as_str())
                {
                    return Err(CellError::Invalid("unsupported audio MIME type".into()));
                }
                media(data, limits.media_bytes)?;
            }
            Self::Notify {
                message,
                completed,
                total,
            } if message.len() > 512 || completed.zip(*total).is_some_and(|(a, b)| a > b) => {
                return Err(CellError::Invalid(
                    "notify accepts bounded progress metadata".into(),
                ));
            }
            _ => {}
        }
        let value = serde_json::to_value(self)?;
        validate_json(&value, limits.json_depth, limits.item_bytes)?;
        Ok(serde_json::to_vec(&value)?.len())
    }
}
fn media(data: &str, limit: usize) -> Result<(), CellError> {
    if data.len() > limit.saturating_add(2) / 3 * 4 || data.is_empty() {
        return Err(CellError::Invalid(
            "media byte limit exceeded or empty media".into(),
        ));
    }
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| CellError::Invalid("media requires canonical padded base64".into()))?;
    if bytes.len() > limit || STANDARD.encode(&bytes) != data {
        return Err(CellError::Invalid(
            "invalid or oversized base64 media".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputEntry {
    pub cursor: u64,
    pub output: CellOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentStatus {
    Pending,
    Succeeded,
    Failed,
    /// The trusted host proved that this intent never dispatched.
    NotExecuted,
    OutcomeUnknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityIntent {
    pub sequence: u64,
    pub name: String,
    pub arguments: Value,
    pub dependencies: Option<DependencySnapshot>,
    pub status: IntentStatus,
    pub result: Option<Value>,
    pub result_omitted: bool,
    /// Full result digest, including successful payloads omitted from delivery.
    #[serde(default)]
    pub result_hash: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellRecord {
    pub cell_id: CellId,
    pub identity: CellIdentity,
    pub source: String,
    pub source_hash: String,
    pub parent_call_id: String,
    pub status: CellStatus,
    pub calls: Vec<CapabilityIntent>,
    pub outputs: Vec<OutputEntry>,
    pub output_bytes: usize,
    pub error: Option<String>,
    /// A failed trusted lifecycle hook freezes the session until the host repairs
    /// its records. Capability evidence alone cannot clear this failure.
    pub backend_error: Option<String>,
    pub reconciliation: Vec<ReconciliationEvidence>,
    /// Append-only host settlement audit. Exact retries do not append duplicates.
    #[serde(default)]
    pub host_settlements: Vec<HostSettlementEvidence>,
    pub revision: u64,
}
impl CellRecord {
    pub fn execution(&self) -> CellExecution {
        CellExecution {
            identity: self.identity.clone(),
            cell_id: self.cell_id.clone(),
            parent_call_id: self.parent_call_id.clone(),
            source_hash: self.source_hash.clone(),
        }
    }
    pub fn unresolved(&self) -> bool {
        self.calls.iter().any(|call| {
            matches!(
                call.status,
                IntentStatus::Pending | IntentStatus::OutcomeUnknown
            )
        })
    }
}

/// One serialized UTF-8 byte is charged as one conservative budget unit. This is
/// NOT a model tokenizer or a media token estimate. The *whole JSON page* fits
/// max_output_tokens bytes; model-specific accounting remains with the provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellPage {
    pub cell_id: CellId,
    pub status: CellStatus,
    /// False after restart or VM teardown, including records still NotStarted.
    pub vm_alive: bool,
    pub cursor: u64,
    pub next_cursor: u64,
    pub outputs: Vec<OutputEntry>,
    pub has_more: bool,
    pub required_budget: Option<usize>,
    pub outcome_unknown: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub script: String,
    pub yield_time_ms: u64,
    pub max_output_tokens: usize,
}
#[derive(Clone, Debug)]
pub struct WaitRequest {
    pub cell_id: CellId,
    pub cursor: u64,
    pub yield_time_ms: u64,
    pub max_output_tokens: usize,
    /// Only an explicit continuation wakes yield_control. Polling may set false.
    pub continue_execution: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoreSnapshot {
    pub version: u64,
    pub value: Value,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Succeeded,
    Failed,
}
/// Runtime must obtain evidence from an actual host observation. The manager
/// compares the source and exact original intent before accepting it. Evidence
/// is an audit reference, not authority inferred from a model's assertion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReconciliationEvidence {
    pub source_hash: String,
    pub call_sequence: u64,
    pub expected_arguments: Value,
    pub observed_result: Value,
    pub evidence: String,
    pub resolution: Resolution,
}

/// Terminal facts read back by a trusted host from its durable execution store.
/// Sequence numbers are the original Cells numbers, starting at one. No method
/// in the model tool or Luau can submit these values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostCallSettlement {
    pub sequence: u64,
    pub name: String,
    pub arguments: Value,
    pub dependencies: Option<DependencySnapshot>,
    pub terminal: HostCallTerminal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HostCallTerminal {
    Succeeded { result: HostResult },
    Failed { error: String },
    NotExecuted { reason: String },
}

/// Omitted successes keep a digest of the full JSON result. They never become
/// failed merely because the payload cannot be delivered to Lua or the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HostResult {
    Value(Value),
    Omitted { sha256: String },
}

/// Only provide this after the host execution and every capability have durable
/// known terminals and the old executor/Device has stopped. The exact failure
/// string binds repair to the failed lifecycle hook, rather than an unaudited
/// boolean that can unfreeze any execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostExecutionSettlement {
    pub expected_backend_error: Option<String>,
    pub terminal: CellTerminal,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostSettlement {
    pub execution: CellExecution,
    pub calls: Vec<HostCallSettlement>,
    pub execution_terminal: Option<HostExecutionSettlement>,
    pub evidence: String,
}

/// The stop assertion and the facts actually accepted are committed atomically
/// with the repaired cell. Cells and the host store use separate transactions:
/// save the host first, then submit its persisted facts, retaining StoppedScope
/// until both steps finish. A failed Cells write leaves the old freeze intact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostSettlementEvidence {
    pub stopped: ExecutorStopped,
    pub settlement: HostSettlement,
}

pub(crate) fn validate_json(value: &Value, depth: usize, bytes: usize) -> Result<(), CellError> {
    fn visit(value: &Value, remaining: usize) -> bool {
        if remaining == 0 {
            return false;
        }
        match value {
            Value::Array(values) => values.iter().all(|v| visit(v, remaining - 1)),
            Value::Object(values) => values.values().all(|v| visit(v, remaining - 1)),
            _ => true,
        }
    }
    struct Counter {
        size: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.size = self.size.saturating_add(buffer.len());
            if self.size > self.limit {
                return Err(std::io::Error::other("JSON size limit"));
            }
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    if !visit(value, depth)
        || serde_json::to_writer(
            Counter {
                size: 0,
                limit: bytes,
            },
            value,
        )
        .is_err()
    {
        return Err(CellError::Invalid(
            "JSON depth or byte limit exceeded".into(),
        ));
    }
    Ok(())
}

/// An unchecked host Value can be far deeper than serde_json's normal parser
/// accepts. Drain it iteratively on rejection instead of recursively dropping it.
pub(crate) fn discard_json(value: Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(mut values) => pending.append(&mut values),
            Value::Object(values) => pending.extend(values.into_values()),
            _ => {}
        }
    }
}

pub(crate) fn bounded_error(message: &str) -> String {
    let mut end = message.len().min(160);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}
