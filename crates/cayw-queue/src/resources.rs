//! Durable execution admission over host-declared memory. This is not an OS
//! memory ceiling: it does not measure or constrain a process's actual RSS.
use crate::QueueError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcePolicy {
    pub max_declared_memory_bytes: u64,
}
impl Default for ResourcePolicy {
    fn default() -> Self {
        Self {
            max_declared_memory_bytes: 1024 * 1024 * 1024,
        }
    }
}
impl ResourcePolicy {
    pub fn validate(&self) -> Result<(), QueueError> {
        if self.max_declared_memory_bytes == 0 || self.max_declared_memory_bytes > i64::MAX as u64 {
            return Err(QueueError::InvalidInput(
                "resource declared memory limit must be between 1 and i64::MAX bytes".into(),
            ));
        }
        Ok(())
    }
}

/// Trusted host input. Zero declares no memory reservation; it is not a measured
/// zero. Requiring an unavailable execution guarantee is always rejected.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskResources {
    pub declared_memory_bytes: u64,
    pub hard_memory_limit_bytes: Option<u64>,
    pub cpu_quota_percent: Option<u32>,
    pub gpu_units: u32,
    pub io_bandwidth_bytes_per_sec: Option<u64>,
}
impl TaskResources {
    pub fn validate(&self, policy: &ResourcePolicy) -> Result<(), QueueError> {
        policy.validate()?;
        let unsupported = if self.hard_memory_limit_bytes.is_some() {
            Some("hard memory limit")
        } else if self.cpu_quota_percent.is_some() {
            Some("CPU quota")
        } else if self.gpu_units != 0 {
            Some("GPU reservation or isolation")
        } else if self.io_bandwidth_bytes_per_sec.is_some() {
            Some("I/O bandwidth limit")
        } else {
            None
        };
        if let Some(resource) = unsupported {
            return Err(QueueError::InvalidInput(format!(
                "unsupported resource guarantee: {resource}"
            )));
        }
        if self.declared_memory_bytes > policy.max_declared_memory_bytes {
            return Err(QueueError::InvalidInput(
                "task declared memory exceeds the resource policy total".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResourceUsage {
    pub max_declared_memory_bytes: u64,
    pub reserved_declared_memory_bytes: u64,
    /// Counts admitted tasks, including declarations of zero and draining work.
    /// Original Session/Task/Attempt/Worker/fence identities are on TaskRecord.
    pub holding_tasks: usize,
}
