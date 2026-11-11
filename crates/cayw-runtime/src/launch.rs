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
    /// Optional main-session workspace directory for the discoverable local Client.
    /// Fully explicit Headless ignores this field and does not create the directory.
    #[serde(default)]
    main_session_dir: Option<PathBuf>,
    /// Targets the user has configured for `notify.send`. Read only from this
    /// private, model-unwritable file; absent configuration means no target.
    #[serde(default)]
    notification_targets: Vec<String>,
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
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
    #[serde(default)]
    allow_filter: bool,
    #[serde(default)]
    io: ProfileIo,
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
        let mut file = File::open(path).map_err(|_| invalid("cannot be opened"))?;
        let metadata = file
            .metadata()
            .map_err(|_| invalid("cannot be inspected"))?;
        if !metadata.is_file() {
            return Err(invalid("must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| invalid("cannot be read"))?;
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
        for target in &config.notification_targets {
            if target.trim().is_empty()
                || target.len() > 256
                || target.chars().any(char::is_control)
            {
                return Err(invalid(
                    "notification_targets must contain 1..256 ordinary bytes each",
                ));
            }
        }
        Ok(config)
    }

    /// Explicit model selection, without reading credentials or making a request.
    pub fn model(&self) -> Option<&ProviderConfig> {
        self.model.as_ref()
    }

    /// Directory override for the discoverable Client's main session.
    /// `CAYW_MAIN_SESSION` still wins. Explicit Headless does not use this.
    pub fn main_session_dir(&self) -> Option<&Path> {
        self.main_session_dir.as_deref()
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
            notification_targets: self.notification_targets,
            background_model_maintenance: true,
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
            config.max_queue_time = Some(Duration::from_millis(value));
        }
        let budgets = self.budgets;
        config.max_total_tokens = budgets.max_total_tokens;
        config.max_turns = budgets.max_turns;
        config.max_run_time = budgets.max_run_time_ms.map(Duration::from_millis);
        config.max_capability_calls = budgets.max_capability_calls;
        config.max_capability_bytes = budgets.max_capability_bytes;
        if let Some(value) = budgets.auto_externalize_threshold_bytes {
            config.auto_externalize_threshold_bytes = value;
        }
        config.codemode.host_wait = budgets.host_wait_ms.map(Duration::from_millis);
        config.host.call_timeout = budgets.host_call_timeout_ms.map(Duration::from_millis);
        if let Some(host_wait) = config.codemode.host_wait
            && (host_wait.is_zero() || Instant::now().checked_add(host_wait).is_none())
        {
            return Err(invalid("host_wait_ms must be positive and representable"));
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
                        timeout: profile.timeout_ms.map(Duration::from_millis),
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
        // A user-written file must not silently lose a service because of a
        // hard-invalid value. Programmatic callers still degrade at Host
        // construction; only orderable soft limits are normalized here.
        if let Some(web) = &config.host.web_access {
            web.limits
                .clone()
                .normalized()
                .validate()
                .map_err(|_| invalid("has invalid web_access limits"))?;
        }
        if let Some(vision) = config.host.vision {
            vision
                .validate()
                .map_err(|_| invalid("has invalid vision limits"))?;
        }
        // The catalog scope cannot authorize or execute. Host::new performs the
        // same profile/path validation used when opening the actual workspace.
        let mut catalog_config = config.host.clone();
        if self.mcp_servers_file.is_some() || self.pi_compat.as_ref().is_some_and(|pi| pi.enabled) {
            catalog_config.external = Some(std::sync::Arc::new(crate::bridge::CatalogOnly));
        }
        let host = crate::cells::catalog_host(Path::new("/"), catalog_config)
            .map_err(|_| invalid("has an invalid process profile or Host budget"))?;
        let definitions = crate::requests::operation_definitions_for_config(&config, &host);
        let disabled_services = host.disabled_services();
        let disabled = |capability: &str| {
            let service = crate::service_for_capability(capability)?;
            disabled_services
                .iter()
                .find(|disabled| disabled.name == service)
                .map(|disabled| disabled.reason.clone())
        };
        for permission in self.permissions {
            if !permission.arguments.is_object() {
                return Err(invalid("permission arguments must be an object"));
            }
            // A permission for a service that could not start stays unused; it
            // must not stop the workspace from opening.
            if disabled(&permission.capability).is_some() {
                continue;
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
            if requests.operations.is_empty() {
                return Err(invalid("request policy must select at least one operation"));
            }
            // Request operations for a disabled service are dropped from the
            // binding. Opening succeeds; the full list is carried so Runtime
            // can report the reason when the operation is used.
            let all_operations = requests.operations.clone();
            let mut operations = requests.operations;
            operations.retain(|operation| disabled(&operation.capability).is_none());
            config.request_limits = requests.limits;
            if !operations.is_empty() {
                let identity = cayw_requests::HostIdentity::from_host(
                    &host,
                    &config.host,
                    &requests.permission_policy_id,
                )
                .map_err(|_| invalid("has an invalid request policy identity"))?;
                let _catalog =
                    cayw_requests::OperationCatalog::bind(identity, &definitions, operations)
                        .map_err(|_| {
                            invalid("request operations do not match the configured Host")
                        })?;
            }
            config.request_policy = Some(crate::RequestPolicy {
                permission_policy_id: requests.permission_policy_id,
                operations: all_operations,
            });
        }
        Ok(config)
    }
}
