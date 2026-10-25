//! Durable execution admission over host-declared memory. This is not an OS
//! memory ceiling: it does not measure or constrain a process's actual RSS.
use crate::{Runtime, RuntimeError};
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
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.max_declared_memory_bytes == 0 || self.max_declared_memory_bytes > i64::MAX as u64 {
            return Err(RuntimeError::InvalidInput(
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
    pub fn validate(&self, policy: &ResourcePolicy) -> Result<(), RuntimeError> {
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
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported resource guarantee: {resource}"
            )));
        }
        if self.declared_memory_bytes > policy.max_declared_memory_bytes {
            return Err(RuntimeError::InvalidInput(
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

#[derive(Clone, Debug, Serialize)]
pub struct ResourceCapability {
    pub resource: &'static str,
    pub limit_type: &'static str,
    pub status: &'static str,
    pub mechanism: &'static str,
    pub scope: &'static str,
    pub error_behavior: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResourceView {
    pub policy: ResourcePolicy,
    pub usage: ResourceUsage,
    pub matrix: Vec<ResourceCapability>,
}

impl Runtime {
    pub fn resources(&self) -> Result<ResourceView, RuntimeError> {
        Ok(ResourceView {
            policy: self.inner.config.resources.clone(),
            usage: self.inner.queue.resource_usage()?,
            matrix: vec![
                ResourceCapability {
                    resource: "declared_memory",
                    limit_type: "software_admission",
                    status: "implemented",
                    mechanism: "one SQLite IMMEDIATE transaction checks declared bytes and reserves the original task's execution slot",
                    scope: "all Runtime connections to this database share one effective total; approval park releases execution admission, not measured VM memory; a trusted host may replace the total only after every connection has stopped and no task is live",
                    error_behavior: "over-total declarations are rejected before enqueue; a ready task waits without bypassing the selected queue head; unconfirmed cleanup retains the reservation; a concurrent connection keeps the persisted total",
                },
                ResourceCapability {
                    resource: "actual_memory",
                    limit_type: "hard_limit",
                    status: "unsupported",
                    mechanism: "no RSS enforcement or sampling",
                    scope: "managed process and descendants",
                    error_behavior: "an explicit hard_memory_limit_bytes requirement is rejected before enqueue; unavailable measurements are not reported as zero",
                },
                ResourceCapability {
                    resource: "cpu",
                    limit_type: "hard_limit",
                    status: "unsupported",
                    mechanism: "no CPU quota or CPU-time sampling",
                    scope: "managed process and descendants",
                    error_behavior: "an explicit cpu_quota_percent requirement is rejected before enqueue; unavailable measurements are not reported as zero",
                },
                ResourceCapability {
                    resource: "gpu",
                    limit_type: "hard_limit",
                    status: "unsupported",
                    mechanism: "no GPU allocation, isolation or sampling",
                    scope: "macOS local baseline",
                    error_behavior: "nonzero gpu_units is rejected before enqueue; unavailable measurements are not reported as zero",
                },
                ResourceCapability {
                    resource: "io_bandwidth",
                    limit_type: "hard_limit",
                    status: "unsupported",
                    mechanism: "no disk or network bandwidth limiter",
                    scope: "managed process and descendants",
                    error_behavior: "an explicit io_bandwidth_bytes_per_sec requirement is rejected before enqueue",
                },
                ResourceCapability {
                    resource: "process_output",
                    limit_type: "software_budget",
                    status: "implemented",
                    mechanism: "Device supervisor counts raw stdout and stderr bytes against the host profile's lifetime output budget",
                    scope: "managed pipe or PTY output; not disk or network I/O bandwidth",
                    error_behavior: "budget exhaustion stops the original process group; Host records the terminal result or unknown effect; cleanup must be confirmed before execution admission is released",
                },
                ResourceCapability {
                    resource: "elapsed_time",
                    limit_type: "software_budget",
                    status: "implemented",
                    mechanism: "Runtime run deadline and Device process lifetime timeout request cancellation through the original owner",
                    scope: "original task and managed process lifetime; not CPU time",
                    error_behavior: "timeout cancels and settles through Host and Device cleanup; an unconfirmed outcome remains draining",
                },
            ],
        })
    }
}
