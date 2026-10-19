//! Host-controlled plugins. Native code is trusted; a child process is not a sandbox.
//! Returned JSON is data, never an approval or a new capability grant.

mod external;
mod registry;

#[cfg(feature = "evaluation")]
pub mod evaluation;
#[cfg(feature = "observer")]
pub mod observer;
#[cfg(feature = "strongflow")]
pub mod strongflow;

pub use async_trait::async_trait;
pub use cayw_ai::{CancellationToken, ToolDefinition};
pub use external::*;
pub use registry::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const PLUGIN_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Native,
    External,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub kind: PluginKind,
    pub capabilities: Vec<ToolDefinition>,
    /// Capability name -> permissions required in addition to the capability grant.
    pub permissions: BTreeMap<String, BTreeSet<String>>,
}

/// The host constructs this context. It is never taken from a plugin response.
#[derive(Debug, Clone, Default)]
pub struct PluginContext {
    capabilities: BTreeSet<String>,
    permissions: BTreeSet<String>,
}

impl PluginContext {
    pub fn new(
        capabilities: impl IntoIterator<Item = String>,
        permissions: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            capabilities: capabilities.into_iter().collect(),
            permissions: permissions.into_iter().collect(),
        }
    }

    pub(crate) fn allows(&self, manifest: &PluginManifest, method: &str) -> bool {
        self.capabilities.contains(method)
            && manifest
                .permissions
                .get(method)
                .is_none_or(|needed| needed.is_subset(&self.permissions))
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PluginError {
    #[error("invalid plugin manifest: {0}")]
    InvalidManifest(String),
    #[error("plugin already loaded: {0}")]
    Conflict(String),
    #[error("plugin not loaded: {0}")]
    NotFound(String),
    #[error("plugin capability denied: {0}")]
    Denied(String),
    #[error("invalid plugin arguments: {0}")]
    InvalidArguments(String),
    #[error("plugin protocol error: {0}")]
    Protocol(String),
    #[error("plugin I/O error: {0}")]
    Io(String),
    #[error("plugin call cancelled; effects may need reconciliation")]
    Cancelled,
    #[error("plugin call timed out; effects may need reconciliation")]
    Timeout,
    #[error("plugin unloaded or replaced during call; effects may need reconciliation")]
    Unloaded,
    #[error("plugin unavailable: {0}")]
    Unavailable(String),
    #[error("plugin operation failed: {0}")]
    Failed(String),
    #[error("plugin outcome unknown: {0}")]
    OutcomeUnknown(String),
}

impl PluginError {
    /// Conservative: transport failures and interrupted calls never prove rollback.
    pub fn outcome_unknown(&self) -> bool {
        matches!(
            self,
            Self::Protocol(_)
                | Self::Io(_)
                | Self::Cancelled
                | Self::Timeout
                | Self::Unloaded
                | Self::OutcomeUnknown(_)
        )
    }
}

/// Implementations must yield to the executor, honor cancellation, and avoid blocking
/// in `shutdown`. A native plugin is trusted Rust code, with no process isolation.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;
    /// Check lifecycle state without invoking a capability or waiting for an
    /// async supervisor. Implementations may perform nonblocking process cleanup.
    fn check_available(&self) -> Result<(), PluginError> {
        Ok(())
    }
    /// Trusted-host checkpoint access, never an agent-facing capability.
    #[cfg(feature = "strongflow")]
    fn strongflow_state(&self) -> Result<strongflow::StrongFlowState, PluginError> {
        Err(PluginError::Unavailable(
            "plugin does not provide host-verifiable StrongFlow state".into(),
        ))
    }
    async fn call(
        &self,
        method: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, PluginError>;
    fn shutdown(&self) {}
}

pub(crate) struct CancelOnDrop(pub CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
