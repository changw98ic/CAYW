//! Host-owned configuration for the installable headless runtime.
use crate::{ProviderConfig, RuntimeConfig, RuntimeError, ScopedPermission};
use cayw_ai::ArgumentValidator;
use cayw_host::{Profile, ProfileKind};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Explicit host settings shared by the SDK client and `cayw serve --config`.
/// Profiles do not grant permission; each invocation still uses the normal gate.
/// This launch format supports pipe-based profiles, not terminal handoffs.
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeadlessConfig {
    model: Option<ProviderConfig>,
    budgets: Budgets,
    scheduler: Scheduler,
    resources: crate::ResourcePolicy,
    content: Option<crate::ContentConfig>,
    profiles: BTreeMap<String, ProcessProfile>,
    permissions: Vec<Permission>,
    requests: Option<Requests>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Requests {
    permission_policy_id: String,
    operations: Vec<cayw_requests::OperationScope>,
    #[serde(default)]
    limits: cayw_requests::RequestConfig,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Budgets {
    max_total_tokens: Option<u64>,
    max_turns: Option<usize>,
    max_run_time_ms: Option<u64>,
    max_capability_calls: Option<usize>,
    max_capability_bytes: Option<usize>,
    host_wait_ms: Option<u64>,
    host_call_timeout_ms: Option<u64>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Scheduler {
    max_concurrent_runs: Option<usize>,
    max_accepted_tasks: Option<usize>,
    fairness_after_dispatches: Option<usize>,
    max_queue_time_ms: Option<u64>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessProfile {
    kind: Kind,
    program: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default = "process_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "process_output_bytes")]
    max_output_bytes: usize,
    #[serde(default)]
    allow_filter: bool,
    #[serde(default)]
    io: ProfileIo,
}
fn process_timeout_ms() -> u64 {
    30_000
}
fn process_output_bytes() -> usize {
    256 * 1024
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Build,
    Test,
    Check,
    Lint,
    Format,
}
impl From<Kind> for ProfileKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Build => Self::Build,
            Kind::Test => Self::Test,
            Kind::Check => Self::Check,
            Kind::Lint => Self::Lint,
            Kind::Format => Self::Format,
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProfileIo {
    #[default]
    Pipes,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Permission {
    capability: String,
    arguments: Value,
}

fn invalid(reason: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("headless configuration {reason}"))
}

impl HeadlessConfig {
    /// Read one explicitly named JSON file. Diagnostics never echo its values.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let file = File::open(path).map_err(|_| invalid("cannot be opened"))?;
        let metadata = file
            .metadata()
            .map_err(|_| invalid("cannot be inspected"))?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return Err(invalid("must be a regular file of at most 1 MiB"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid("cannot be read"))?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(invalid("exceeds 1 MiB"));
        }
        let config: Self = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("must be valid JSON with only supported fields and values"))?;
        if let Some(model) = &config.model {
            model.validate()?;
        }
        Ok(config)
    }

    /// Explicit model selection, without reading credentials or making a request.
    pub fn model(&self) -> Option<&ProviderConfig> {
        self.model.as_ref()
    }

    /// Validate the existing runtime/Host contract without starting a model or
    /// a process, creating a database, or opening a workspace execution store.
    pub fn into_runtime_config(self) -> Result<RuntimeConfig, RuntimeError> {
        if let Some(model) = &self.model {
            model.validate()?;
        }
        let mut config = RuntimeConfig {
            resources: self.resources,
            content: self.content,
            ..RuntimeConfig::default()
        };
        if let Some(value) = self.scheduler.max_concurrent_runs {
            config.max_concurrent_runs = value;
        }
        if let Some(value) = self.scheduler.max_accepted_tasks {
            config.max_queued_runs = value;
        }
        if let Some(value) = self.scheduler.fairness_after_dispatches {
            config.fairness_after_dispatches = value;
        }
        if let Some(value) = self.scheduler.max_queue_time_ms {
            config.max_queue_time = Duration::from_millis(value);
        }
        let budgets = self.budgets;
        if let Some(value) = budgets.max_total_tokens {
            config.max_total_tokens = value;
        }
        if let Some(value) = budgets.max_turns {
            config.max_turns = value;
        }
        if let Some(value) = budgets.max_run_time_ms {
            config.max_run_time = Duration::from_millis(value);
        }
        if let Some(value) = budgets.max_capability_calls {
            config.max_capability_calls = value;
        }
        if let Some(value) = budgets.max_capability_bytes {
            config.max_capability_bytes = value;
        }
        if let Some(value) = budgets.host_wait_ms {
            config.codemode.host_wait = Duration::from_millis(value);
        }
        if let Some(value) = budgets.host_call_timeout_ms {
            config.host.call_timeout = Duration::from_millis(value);
        }
        if config.codemode.host_wait.is_zero()
            || config.codemode.host_wait > Duration::from_secs(3600)
            || Instant::now()
                .checked_add(config.codemode.host_wait)
                .is_none()
        {
            return Err(invalid(
                "host_wait_ms must be positive and at most one hour",
            ));
        }
        config.host.profiles = self
            .profiles
            .into_iter()
            .map(|(name, profile)| {
                (
                    name,
                    Profile {
                        kind: profile.kind.into(),
                        program: profile.program,
                        args: profile.args,
                        env: profile.env,
                        timeout: Duration::from_millis(profile.timeout_ms),
                        max_output_bytes: profile.max_output_bytes,
                        allow_filter: profile.allow_filter,
                        io: match profile.io {
                            ProfileIo::Pipes => cayw_host::ProcessIo::Pipes,
                        },
                    },
                )
            })
            .collect();
        config
            .validate()
            .map_err(|_| invalid("has invalid runtime budgets"))?;
        // The catalog scope cannot authorize or execute. Host::new performs the
        // same profile/path validation used when opening the actual workspace.
        let host = crate::cells::catalog_host(Path::new("/"), config.host.clone())
            .map_err(|_| invalid("has an invalid process profile or Host budget"))?;
        let mut definitions = cayw_codemode::CapabilityHost::capabilities(&host);
        definitions.extend(crate::content::definitions(config.content.as_ref()));
        for permission in self.permissions {
            if !permission.arguments.is_object() {
                return Err(invalid("permission arguments must be an object"));
            }
            let definition = definitions
                .iter()
                .find(|definition| definition.name == permission.capability)
                .ok_or_else(|| invalid("permission names an unavailable capability"))?;
            ArgumentValidator::new(&definition.parameters)
                .and_then(|validator| validator.validate(&permission.arguments))
                .map_err(|_| invalid("permission arguments do not match the capability"))?;
            config.permissions.push(ScopedPermission::exact(
                permission.capability,
                permission.arguments,
            ));
        }
        if let Some(requests) = self.requests {
            requests
                .limits
                .validate()
                .map_err(|_| invalid("has invalid request limits"))?;
            let identity = cayw_requests::HostIdentity::from_host(
                &host,
                &config.host,
                &requests.permission_policy_id,
            )
            .map_err(|_| invalid("has an invalid request policy identity"))?;
            let catalog =
                cayw_requests::OperationCatalog::bind(identity, &definitions, requests.operations)
                    .map_err(|_| invalid("request operations do not match the configured Host"))?;
            config.request_limits = requests.limits;
            config.request_policy = Some(crate::RequestPolicy {
                permission_policy_id: requests.permission_policy_id,
                operations: catalog.operations(),
            });
        }
        Ok(config)
    }
}
