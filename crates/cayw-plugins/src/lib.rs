//! Host-controlled plugins. Native code is trusted; a child process is not a sandbox.
//! Returned JSON is data, never an approval or a new capability grant.

mod external;
mod registry;

#[cfg(feature = "fusion")]
pub mod fusion;
#[cfg(feature = "jev")]
pub mod jev;
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
pub enum PluginRole {
    Client,
    WorkspaceArchitecture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Native,
    External,
}

/// Exact OS identity of one plugin process. The microsecond startup time
/// distinguishes a reused PID; `process_group` also covers owned descendants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginProcessIdentity {
    pub pid: u32,
    pub started_seconds: u64,
    pub started_microseconds: u32,
    pub command: String,
    pub process_group: Option<u32>,
}

impl PluginProcessIdentity {
    /// Capture one macOS process incarnation without signalling it.
    #[cfg(target_os = "macos")]
    pub fn observe(pid: u32, command: impl Into<String>) -> std::io::Result<Self> {
        let info = inspect_process(pid)?;
        if info.pbi_start_tvsec == 0 || info.pbi_start_tvusec >= 1_000_000 {
            return Err(std::io::Error::other(
                "invalid plugin process startup identity",
            ));
        }
        Ok(Self {
            pid,
            started_seconds: info.pbi_start_tvsec,
            started_microseconds: info.pbi_start_tvusec as u32,
            command: command.into(),
            process_group: (info.pbi_pgid > 0).then_some(info.pbi_pgid),
        })
    }

    /// Check the exact original PID incarnation. Unreadable state is an error,
    /// not evidence that cleanup completed.
    #[cfg(target_os = "macos")]
    pub fn is_running(&self) -> std::io::Result<bool> {
        let info = match inspect_process(self.pid) {
            Ok(info) => info,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(info.pbi_start_tvsec == self.started_seconds
            && info.pbi_start_tvusec as u32 == self.started_microseconds
            && info.pbi_status != libc::SZOMB)
    }

    /// Check whether any process still belongs to the original process group.
    /// A reused group keeps cleanup blocked, which is conservative.
    #[cfg(target_os = "macos")]
    pub fn process_group_running(&self) -> std::io::Result<bool> {
        let Some(group) = self.process_group else {
            return Ok(false);
        };
        let group = libc::pid_t::try_from(group)
            .ok()
            .filter(|group| *group > 1)
            .ok_or_else(|| std::io::Error::other("invalid plugin process group"))?;
        // SAFETY: signal 0 performs a read-only existence/permission check.
        if unsafe { libc::kill(-group, 0) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(error),
        }
    }
}

#[cfg(target_os = "macos")]
fn inspect_process(pid: u32) -> std::io::Result<libc::proc_bsdinfo> {
    let pid = libc::pid_t::try_from(pid)
        .ok()
        .filter(|pid| *pid > 1)
        .ok_or_else(|| std::io::Error::other("invalid plugin process ID"))?;
    // SAFETY: proc_bsdinfo is C POD and the writable buffer has exactly `size` bytes.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as libc::c_int;
    let count = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if count != size {
        return Err(if count == 0 {
            std::io::Error::last_os_error()
        } else {
            std::io::Error::other("incomplete plugin process identity observation")
        });
    }
    if info.pbi_pid != pid as u32 {
        return Err(std::io::Error::other(
            "plugin process identity changed during inspection",
        ));
    }
    Ok(info)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub role: PluginRole,
    pub kind: PluginKind,
    pub capabilities: Vec<ToolDefinition>,
    /// Capability name -> permissions required in addition to the capability grant.
    pub permissions: BTreeMap<String, BTreeSet<String>>,
}

impl PluginManifest {
    /// Validate compatibility and capability schemas without executing code.
    pub fn validate(&self) -> Result<(), PluginError> {
        registry::validate_manifest(self).map(|_| ())
    }
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
    /// Cleanup could not be confirmed. The exact process identity is retained so
    /// a later retry can re-observe the same process instead of trusting text.
    #[error("plugin outcome unknown: {message}")]
    OutcomeUnknownProcess {
        message: String,
        process: Option<PluginProcessIdentity>,
    },
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
                | Self::OutcomeUnknownProcess { .. }
        )
    }

    /// Exact process retained by an unconfirmed cleanup, when the host observed it.
    pub fn process_identity(&self) -> Option<&PluginProcessIdentity> {
        match self {
            Self::OutcomeUnknownProcess { process, .. } => process.as_ref(),
            _ => None,
        }
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
    /// Close admission immediately. This method must not block.
    fn request_shutdown(&self) {}
    /// Wait until all owned work and processes have published a terminal state.
    async fn wait_shutdown(&self) -> Result<(), PluginError> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), PluginError> {
        self.request_shutdown();
        self.wait_shutdown().await
    }
}

pub(crate) struct CancelOnDrop(pub CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) fn valid_name(value: &str) -> bool {
    // Allow npm scoped package names (`@scope/name`) so F15 can load real Pi
    // packages under their published identity without renaming.
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@' | b'/'))
}
