//! Bounded Luau orchestration over capabilities explicitly supplied by a Rust host.
//!
//! Each execution gets a fresh sandbox. The host remains responsible for permissions,
//! capability cancellation, and reconciliation of effects whose outcome is unknown.

mod json;
mod runtime;
mod tool;

pub use tool::CodeModeTool;

use async_trait::async_trait;
use cayw_ai::{ArgumentValidator, CancellationToken, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

/// Implementations authorize every call, honor cancellation, and never detach side effects.
/// Returning an ordinary error asserts known failure. Use `CapabilityError::unknown`
/// when an effect might have occurred without confirmation. After cancellation,
/// a started call keeps being polled within `Limits::shutdown_grace`; cleanup and
/// durable audit must settle within that budget or its outcome remains unknown.
#[async_trait]
pub trait CapabilityHost: Send + Sync {
    fn capabilities(&self) -> Vec<ToolDefinition>;
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError>;
}

/// Trusted helper operations, installed as closures in the restricted VM.
/// These are not capabilities and never appear in discovery or consume host calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmOperation {
    Store,
    Load,
    Output,
    Exit,
    Yield,
    Definition,
    Search,
}

/// Rust-owned cell services. Only `yield_control()` can pause the VM active clock.
/// `emit` commits output synchronously before user code can continue. Helper
/// closures are installed separately from the user chunk; no dispatch handle or
/// bootstrap environment is exposed to that chunk.
#[async_trait]
pub trait VmServices: Send + Sync {
    fn emit(&self, value: Value) -> Result<(), CapabilityError>;
    async fn invoke(
        &self,
        operation: VmOperation,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError>;
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct CapabilityError {
    pub message: String,
    pub outcome_unknown: bool,
}

impl CapabilityError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: false,
        }
    }
    pub fn unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Limits {
    /// Total Lua allocator bytes, including the sandbox and standard libraries.
    pub memory_bytes: usize,
    /// Luau interrupt checkpoints (function entries and loop back edges), not VM opcodes.
    pub instruction_budget: u64,
    /// Active time for VM setup, compilation, execution, and awaited host calls.
    /// Only the trusted cell yield helper pauses this clock. Host permission
    /// waits and ordinary host calls continue consuming it. An already-started
    /// call may additionally use `shutdown_grace` to settle.
    pub timeout: Duration,
    /// Additional time to poll the same started host future after cancellation or
    /// timeout. No new call is dispatched during this interval. Must be positive
    /// and leave room for the agent's one-second settlement margin in an Instant.
    pub shutdown_grace: Duration,
    pub host_calls: usize,
    /// Shared JSON payload budget: arguments, host results/errors, emits, return value
    /// and retained diagnostics. Original host errors are charged before truncation.
    /// Audit metadata is separately bounded by `host_calls`; omitted results keep status.
    pub output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 16 * 1024 * 1024,
            instruction_budget: 100_000,
            timeout: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(2),
            host_calls: 64,
            output_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Succeeded,
    Failed,
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityCall {
    pub sequence: usize,
    pub name: String,
    pub arguments: Option<Value>,
    pub status: CallStatus,
    pub result: Option<Value>,
    pub result_omitted: bool,
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub calls: Vec<CapabilityCall>,
    pub outputs: Vec<Value>,
    pub return_value: Value,
    pub outcome_unknown: bool,
    pub instruction_checkpoints: u64,
    pub output_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidLimits,
    InvalidCatalog,
    Script,
    Conversion,
    MemoryLimit,
    InstructionLimit,
    Timeout,
    Cancelled,
    HostCallLimit,
    OutputLimit,
    UnknownCapability,
    Arguments,
    Capability,
    OutcomeUnknown,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct CodeModeError {
    pub kind: ErrorKind,
    /// At most 2 KiB of UTF-8, charged to the payload budget as JSON text.
    /// Empty if no non-empty prefix fits; kind and the audit remain available.
    pub message: String,
    pub report: Box<ExecutionReport>,
}

impl CodeModeError {
    fn empty(kind: ErrorKind, message: impl Into<String>, limit: usize) -> Self {
        let message = message.into();
        let (message, output_bytes) = json::fit_message(&message, limit);
        Self {
            kind,
            message,
            report: Box::new(ExecutionReport {
                output_bytes,
                ..ExecutionReport::default()
            }),
        }
    }
}

struct Capability {
    definition: ToolDefinition,
    validator: ArgumentValidator,
}

pub struct CodeMode {
    host: Arc<dyn CapabilityHost>,
    limits: Limits,
    catalog: Arc<BTreeMap<String, Capability>>,
}

impl CodeMode {
    /// Captures an advertised catalog; its schemas cannot load external references.
    /// Catalog membership never grants permission to execute a capability.
    pub fn new(host: Arc<dyn CapabilityHost>, limits: Limits) -> Result<Self, CodeModeError> {
        if limits.memory_bytes == 0
            || limits.instruction_budget == 0
            || limits.timeout.is_zero()
            || limits.shutdown_grace.is_zero()
            || limits.host_calls == 0
            || limits.output_bytes == 0
            || Instant::now().checked_add(limits.timeout).is_none()
            || limits
                .shutdown_grace
                .checked_add(Duration::from_secs(1))
                .and_then(|grace| Instant::now().checked_add(grace))
                .is_none()
        {
            return Err(CodeModeError::empty(
                ErrorKind::InvalidLimits,
                "Every limit must be non-zero; timeout and shutdown grace plus the agent margin must fit an Instant",
                limits.output_bytes,
            ));
        }
        let mut catalog = BTreeMap::new();
        for definition in host.capabilities() {
            if definition.name.is_empty()
                || definition.name.len() > 256
                || catalog.contains_key(&definition.name)
            {
                return Err(CodeModeError::empty(
                    ErrorKind::InvalidCatalog,
                    "Capability names must be unique and contain 1..=256 UTF-8 bytes",
                    limits.output_bytes,
                ));
            }
            let validator = ArgumentValidator::new(&definition.parameters).map_err(|error| {
                CodeModeError::empty(
                    ErrorKind::InvalidCatalog,
                    error.to_string(),
                    limits.output_bytes,
                )
            })?;
            catalog.insert(
                definition.name.clone(),
                Capability {
                    definition,
                    validator,
                },
            );
        }
        Ok(Self {
            host,
            limits,
            catalog: Arc::new(catalog),
        })
    }

    /// Returns the complete report on success, or the same audit in `error.report`.
    /// Cancellation/timeout cancels the started host's child token and polls that
    /// same future for at most `Limits::shutdown_grace`. Its actual terminal
    /// status is retained while the execution still returns Cancelled/Timeout.
    /// Grace expiry drops it with an explicitly unknown outcome; nothing is replayed.
    /// Dropping this entire future only cancels synchronously: it does not await
    /// cleanup or undo external effects. Non-yielding host code cannot be preempted.
    pub async fn execute(
        &self,
        script: &str,
        cancel: CancellationToken,
    ) -> Result<ExecutionReport, CodeModeError> {
        runtime::execute(self, script, cancel, None).await
    }

    /// Runs the same bounded VM with trusted cell helpers and a durable live
    /// output observer. Services are held for the whole VM lifetime.
    pub async fn execute_with_services(
        &self,
        script: &str,
        cancel: CancellationToken,
        services: Arc<dyn VmServices>,
    ) -> Result<ExecutionReport, CodeModeError> {
        runtime::execute(self, script, cancel, Some(services)).await
    }
}
