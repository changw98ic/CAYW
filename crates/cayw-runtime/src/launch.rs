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
    pub output_filter: Option<cayw_host::FilterOptions>,
    background: Option<crate::BackgroundConfig>,
    profiles: BTreeMap<String, ProcessProfile>,
    permissions: Vec<Permission>,
    requests: Option<Requests>,
    /// Absolute path to an external `mcpServers` JSON file (Claude Desktop / Cursor style).
    /// Only absolute-path local stdio entries are installed into the Runtime Bridge.
    mcp_servers_file: Option<PathBuf>,
    /// Optional web search / fetch / extract options.
    web_access: Option<cayw_web_access::WebAccessOptions>,
    /// Optional vision capabilities limits.
    vision: Option<cayw_vision::Limits>,
    /// Optional Pi extension compatibility host options.
    pi_compat: Option<PiCompatConfig>,
}

/// Configuration for the optional Pi extension compatibility host (`cayw-pi-compat`).
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PiCompatConfig {
    pub enabled: bool,
    #[serde(default)]
    pub fixture: bool,
    #[serde(default)]
    pub sample_extension: Option<PathBuf>,
    #[serde(default)]
    pub npm_hello: bool,
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
    /// Soft Host/tool result auto-externalize threshold; see RuntimeConfig.
    auto_externalize_threshold_bytes: Option<usize>,
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
        if let Some(path) = &config.mcp_servers_file
            && (!path.is_absolute() || !path.is_file())
        {
            return Err(invalid(
                "mcp_servers_file must be an absolute existing regular file",
            ));
        }
        Ok(config)
    }

    /// Explicit model selection, without reading credentials or making a request.
    pub fn model(&self) -> Option<&ProviderConfig> {
        self.model.as_ref()
    }

    /// External MCP servers config file for the product import → prepare → Registry path.
    pub fn mcp_servers_file(&self) -> Option<&Path> {
        self.mcp_servers_file.as_deref()
    }

    /// Optional web-access options.
    pub fn web_access(&self) -> Option<&cayw_web_access::WebAccessOptions> {
        self.web_access.as_ref()
    }

    /// Optional vision limits.
    pub fn vision(&self) -> Option<&cayw_vision::Limits> {
        self.vision.as_ref()
    }

    /// Optional Pi extension compatibility options.
    pub fn pi_compat(&self) -> Option<&PiCompatConfig> {
        self.pi_compat.as_ref()
    }

    /// Optional background task configuration.
    pub fn background(&self) -> Option<&crate::BackgroundConfig> {
        self.background.as_ref()
    }

    /// Enable the optional Pi host before validating its capability permissions.
    pub fn enable_pi_compat(&mut self) {
        self.pi_compat
            .get_or_insert_with(PiCompatConfig::default)
            .enabled = true;
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
            background: self.background,
            ..RuntimeConfig::default()
        };
        if let Some(bg) = &config.background {
            bg.validate()
                .map_err(|_| invalid("has invalid background config"))?;
        }
        if let Some(filter) = self.output_filter {
            config.host.output_filter = Some(filter);
        }
        config.host.web_access = self.web_access;
        config.host.vision = self.vision;
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
        if let Some(value) = budgets.auto_externalize_threshold_bytes {
            config.auto_externalize_threshold_bytes = value;
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
        let mut catalog_config = config.host.clone();
        if self.mcp_servers_file.is_some() || self.pi_compat.as_ref().is_some_and(|pi| pi.enabled) {
            catalog_config.external = Some(std::sync::Arc::new(crate::bridge::CatalogOnly));
        }
        let host = crate::cells::catalog_host(Path::new("/"), catalog_config)
            .map_err(|_| invalid("has an invalid process profile or Host budget"))?;
        let definitions = crate::requests::operation_definitions_for_config(&config, &host);
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
