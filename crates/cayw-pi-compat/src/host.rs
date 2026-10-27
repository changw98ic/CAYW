use crate::error::PiCompatError;
use crate::fixture::PiFixturePlugin;
use crate::matrix::CompatMatrix;
use crate::npm_ext::PiNpmHelloExtension;
use crate::sample::PiSampleExtension;
use crate::types::*;
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{
    Manifest, Provenance, Registry, SourceHandle, SourceIdentity, SourceOperation, SourceReply,
    SourceRequest, ToolSpec,
};
use cayw_codemode::CapabilityError;
use cayw_plugins::{ExternalConfig, PluginContext, PluginDescription, PluginError, PluginRegistry};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Optional Pi→CAYW external host mapping (F15 first cut).
///
/// Disabled by default constructors that take `enabled: false`. When enabled,
/// install/call/unload go through `cayw-plugins::PluginRegistry`. Surface
/// checks consult the [`CompatMatrix`]; refused surfaces are recorded as
/// incompatible / unavailable — never silent success.
///
/// Install paths:
/// - [`Self::install_fixture`] — in-process Rust [`PiFixturePlugin`] via `load_native` (CI).
/// - [`Self::install_sample_extension`] / [`Self::install_external`] — disk subprocess
///   via `load_external` (homemade CAYW-protocol sample; not Pi ExtensionAPI).
/// - [`Self::install_npm_hello_extension`] — **primary acceptance**: unmodified npm Pi
///   `hello.ts` via Node Pi↔CAYW adapter + `load_external`.
pub struct PiCompatHost {
    enabled: bool,
    registry: Arc<PluginRegistry>,
    matrix: Mutex<CompatMatrix>,
}

impl PiCompatHost {
    pub fn new(enabled: bool) -> Self {
        Self::with_registry(Arc::new(PluginRegistry::new()), enabled)
    }

    pub fn with_registry(registry: Arc<PluginRegistry>, enabled: bool) -> Self {
        Self {
            enabled,
            registry,
            matrix: Mutex::new(CompatMatrix::first_cut_baseline()),
        }
    }

    pub fn registry(&self) -> &Arc<PluginRegistry> {
        &self.registry
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn matrix_snapshot(&self) -> CompatMatrix {
        self.matrix.lock().expect("compat matrix poisoned").clone()
    }

    pub fn ensure_enabled(&self) -> Result<(), PiCompatError> {
        if self.enabled {
            Ok(())
        } else {
            Err(PiCompatError::Disabled)
        }
    }

    /// Install the offline fixture package (`cayw-pi-fixture@0.1.0`) via `load_native`.
    pub fn install_fixture(&self) -> Result<PluginDescription, PiCompatError> {
        self.ensure_enabled()?;
        let package = PiFixturePlugin::package_id();
        let surface = SurfaceId::new(SurfaceFamily::Lifecycle, "install");
        let level = {
            let matrix = self.matrix.lock().expect("compat matrix poisoned");
            let row = matrix.lookup(
                &package,
                &SurfaceId::new(SurfaceFamily::Tools, "registerTool"),
            );
            row.level
        };
        if !level.allows_invoke() {
            let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.record_incompatible(
                package.clone(),
                surface,
                format!("install refused: matrix level {}", level.as_str()),
            );
            return Err(PiCompatError::Incompatible(package.key()));
        }

        let plugin = Arc::new(PiFixturePlugin::new());
        let context = PluginContext::new([PiFixturePlugin::TOOL.into()], []);
        match self.registry.load_native(plugin, context) {
            Ok(desc) => Ok(desc),
            Err(PluginError::Conflict(name)) => Err(PiCompatError::AlreadyLoaded(name)),
            Err(err) => {
                let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
                matrix.record_incompatible(package, surface, err.to_string());
                Err(map_plugin_err(err))
            }
        }
    }

    /// Install a disk/subprocess extension via `PluginRegistry::load_external`.
    ///
    /// Used by both the homemade sample binary and the Node Pi ExtensionAPI
    /// adapter (`install_npm_hello_extension`). The executable must already
    /// exist; missing paths refuse with [`PiCompatError::Unavailable`]
    /// (never silent success).
    pub async fn install_external(
        &self,
        config: ExternalConfig,
        cancel: CancellationToken,
    ) -> Result<PluginDescription, PiCompatError> {
        self.ensure_enabled()?;
        let package = PiPackageId::new(
            config.expected_manifest.name.clone(),
            config.expected_manifest.version.clone(),
        );
        let surface = SurfaceId::new(SurfaceFamily::Lifecycle, "install");

        if !config.executable.is_file() {
            let detail = format!(
                "external executable missing: {}",
                config.executable.display()
            );
            let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.record_unavailable(package.clone(), surface, detail.clone());
            return Err(PiCompatError::Unavailable(detail));
        }

        let level = {
            let matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix
                .lookup(
                    &package,
                    &SurfaceId::new(SurfaceFamily::Tools, "registerTool"),
                )
                .level
        };
        if !level.allows_invoke() {
            let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.record_incompatible(
                package.clone(),
                surface,
                format!("install_external refused: matrix level {}", level.as_str()),
            );
            return Err(PiCompatError::Incompatible(package.key()));
        }

        let granted: Vec<String> = config
            .expected_manifest
            .capabilities
            .iter()
            .map(|c| c.name.clone())
            .collect();
        let context = PluginContext::new(granted, []);
        match self.registry.load_external(config, context, cancel).await {
            Ok(desc) => Ok(desc),
            Err(PluginError::Conflict(name)) => Err(PiCompatError::AlreadyLoaded(name)),
            Err(PluginError::Cancelled) => Err(PiCompatError::Cancelled),
            Err(PluginError::Timeout) => Err(PiCompatError::Timeout),
            Err(err) => {
                let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
                matrix.record_incompatible(package, surface, err.to_string());
                Err(map_plugin_err(err))
            }
        }
    }

    /// Convenience: load the documented sample extension from an absolute path
    /// when present (`cayw-pi-sample-ext@0.1.0` → `pi_sample.ping`).
    pub async fn install_sample_extension(
        &self,
        executable: impl AsRef<Path>,
        cancel: CancellationToken,
    ) -> Result<PluginDescription, PiCompatError> {
        let executable = absolute_existing_or_path(executable.as_ref());
        let config =
            ExternalConfig::new(executable, vec![], PiSampleExtension::expected_manifest());
        self.install_external(config, cancel).await
    }

    /// Primary acceptance path: load the unmodified npm Pi `hello.ts` extension.
    ///
    /// Spawns Node running `npm/pi-npm-bridge.mjs`, which implements a minimal
    /// Pi `ExtensionAPI`, loads
    /// `@earendil-works/pi-coding-agent@0.84.1` `examples/extensions/hello.ts`
    /// **without modifying it**, and translates registerTool/execute to the CAYW
    /// external JSON protocol. Missing Node ≥22.19, npm deps, or the extension
    /// file refuse with [`PiCompatError::Unavailable`] (never silent success).
    pub async fn install_npm_hello_extension(
        &self,
        cancel: CancellationToken,
    ) -> Result<PluginDescription, PiCompatError> {
        self.ensure_enabled()?;
        let package = PiNpmHelloExtension::package_id();
        let surface = SurfaceId::new(SurfaceFamily::Lifecycle, "install");

        if let Err(detail) = PiNpmHelloExtension::ensure_npm_deps() {
            let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.record_unavailable(package.clone(), surface, detail.clone());
            return Err(PiCompatError::Unavailable(detail));
        }

        let Some(node) = PiNpmHelloExtension::resolve_node() else {
            let detail = "Node >= 22.19 required for Pi 0.84.1 ExtensionAPI host (set CAYW_PI_NODE or install node22)".to_string();
            let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.record_unavailable(package.clone(), surface, detail.clone());
            return Err(PiCompatError::Unavailable(detail));
        };

        let bridge = PiNpmHelloExtension::bridge_script();
        let extension = PiNpmHelloExtension::extension_path();
        if !bridge.is_file() || !extension.is_file() {
            let detail = format!(
                "npm bridge or extension missing: bridge={} extension={}",
                bridge.display(),
                extension.display()
            );
            let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.record_unavailable(package.clone(), surface, detail.clone());
            return Err(PiCompatError::Unavailable(detail));
        }

        let node = absolute_existing_or_path(&node);
        let bridge = absolute_existing_or_path(&bridge);
        let extension = absolute_existing_or_path(&extension);
        let npm_dir = absolute_existing_or_path(&PiNpmHelloExtension::npm_dir());

        let mut environment = BTreeMap::new();
        // ExternalConfig clears the environment; pass a minimal PATH so Node can start.
        let path_value = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
        environment.insert("PATH".into(), path_value);
        if let Ok(home) = std::env::var("HOME") {
            environment.insert("HOME".into(), home);
        }
        environment.insert(
            "LANG".into(),
            std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()),
        );

        let mut config = ExternalConfig::new(
            node,
            vec![
                bridge.display().to_string(),
                "--extension".into(),
                extension.display().to_string(),
                "--package-name".into(),
                PiNpmHelloExtension::PACKAGE_NAME.into(),
                "--package-version".into(),
                PiNpmHelloExtension::PACKAGE_VERSION.into(),
            ],
            PiNpmHelloExtension::expected_manifest(),
        );
        config.environment = environment;
        config.working_directory = Some(npm_dir);
        // jiti + first TS load can exceed the default 10s on cold CI disks.
        config.handshake_timeout = Duration::from_secs(60);

        self.install_external(config, cancel).await
    }

    /// Invoke a mapped tool capability for a loaded package.
    ///
    /// `tool` is the capability name from the loaded manifest (e.g.
    /// [`PiFixturePlugin::TOOL`] or [`PiSampleExtension::TOOL`]) — not hardcoded
    /// to the fixture.
    pub async fn call_tool(
        &self,
        package: &PiPackageId,
        tool: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, PiCompatError> {
        self.ensure_enabled()?;
        let description = self
            .registry
            .describe(&package.name)
            .map_err(|_| PiCompatError::NotLoaded(package.key()))?;
        self.call_pinned_tool(package, &description, tool, arguments, cancel)
            .await
    }

    async fn call_pinned_tool(
        &self,
        package: &PiPackageId,
        description: &PluginDescription,
        tool: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, PiCompatError> {
        self.ensure_enabled()?;
        if description.manifest.name != package.name
            || description.manifest.version != package.version
        {
            return Err(PiCompatError::NotLoaded(package.key()));
        }
        if cancel.is_cancelled() {
            return Err(PiCompatError::Cancelled);
        }
        let surface = SurfaceId::new(SurfaceFamily::Tools, "invoke");
        let row = {
            let matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.lookup(package, &surface)
        };
        if !row.level.allows_invoke() {
            let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
            let recorded = matrix.record_incompatible(
                package.clone(),
                surface.clone(),
                format!(
                    "invoke refused: matrix level {} ({})",
                    row.level.as_str(),
                    row.detail
                ),
            );
            return match recorded.level {
                CompatibilityLevel::Incompatible => Err(PiCompatError::Incompatible(surface.key())),
                _ => Err(PiCompatError::Unavailable(surface.key())),
            };
        }

        if !description
            .manifest
            .capabilities
            .iter()
            .any(|c| c.name == tool)
        {
            return Err(PiCompatError::Failed(format!(
                "capability `{tool}` not in loaded manifest for {}",
                package.key()
            )));
        }

        match self
            .registry
            .call_exact(description, tool, arguments, cancel, Duration::from_secs(5))
            .await
        {
            Ok(value) => Ok(value),
            Err(err) if err.outcome_unknown() => {
                Err(PiCompatError::OutcomeUnknown(err.to_string()))
            }
            Err(err) => Err(map_plugin_err(err)),
        }
    }

    /// Unload a previously installed package.
    pub fn unload(&self, package: &PiPackageId) -> Result<PluginDescription, PiCompatError> {
        self.ensure_enabled()?;
        let surface = SurfaceId::new(SurfaceFamily::Lifecycle, "unload");
        let row = {
            let matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.lookup(package, &surface)
        };
        if !row.level.allows_invoke() {
            let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.record_incompatible(
                package.clone(),
                surface,
                format!("unload refused: matrix level {}", row.level.as_str()),
            );
            return Err(PiCompatError::Incompatible(package.key()));
        }
        let description = self
            .registry
            .describe(&package.name)
            .map_err(|_| PiCompatError::NotLoaded(package.key()))?;
        if description.manifest.version != package.version {
            return Err(PiCompatError::NotLoaded(package.key()));
        }
        self.registry
            .unload_exact(&description)
            .map_err(|err| match err {
                PluginError::NotFound(name) => PiCompatError::NotLoaded(name),
                other => {
                    let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
                    matrix.record_incompatible(package.clone(), surface, other.to_string());
                    map_plugin_err(other)
                }
            })
    }

    /// Probe a surface without invoking side effects; records incompatible when refused.
    pub fn probe_surface(
        &self,
        package: &PiPackageId,
        surface: SurfaceId,
    ) -> Result<CompatibilityLevel, PiCompatError> {
        self.ensure_enabled()?;
        let row = {
            let matrix = self.matrix.lock().expect("compat matrix poisoned");
            matrix.lookup(package, &surface)
        };
        if row.level.allows_invoke() {
            return Ok(row.level);
        }
        let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
        let recorded = matrix.record_incompatible(
            package.clone(),
            surface.clone(),
            format!("probe refused: {} — {}", row.level.as_str(), row.detail),
        );
        match recorded.level {
            CompatibilityLevel::Incompatible => Err(PiCompatError::Incompatible(surface.key())),
            CompatibilityLevel::Unavailable => Err(PiCompatError::Unavailable(surface.key())),
            other => Err(PiCompatError::Failed(format!(
                "unexpected probe level {}",
                other.as_str()
            ))),
        }
    }

    /// Register a loaded Pi package into a `cayw-bridge` Registry.
    ///
    /// Exposes tools declared in the package manifest through the unified
    /// Bridge catalog without maintaining an isolated private registry.
    pub fn register_into_bridge(
        self: &Arc<Self>,
        package: &PiPackageId,
        registry: &Registry,
    ) -> Result<Provenance, PiCompatError> {
        self.ensure_enabled()?;
        let desc = self
            .registry
            .describe(&package.name)
            .map_err(|_| PiCompatError::NotLoaded(package.key()))?;

        if desc.manifest.version != package.version {
            return Err(PiCompatError::NotLoaded(package.key()));
        }
        let tools = desc
            .manifest
            .capabilities
            .iter()
            .map(|cap| ToolSpec {
                name: cap.name.clone(),
                description: cap.description.clone(),
                input_schema: cap.parameters.clone(),
                output_schema: None,
            })
            .collect();

        let manifest = Manifest {
            tools,
            skills: vec![],
            resources: vec![],
            prompts: vec![],
            search: false,
        };

        let identity = SourceIdentity {
            id: package.name.clone(),
            version: package.version.clone(),
            origin: "pi-compat".into(),
        };

        let handle = Box::new(PiBridgeHandle {
            host: Arc::clone(self),
            package: package.clone(),
            ownership_key: format!(
                "pi-compat:{}:{}:{}",
                package.key(),
                desc.registry_id,
                desc.generation
            ),
            description: desc,
        });

        registry
            .register(identity, manifest, handle)
            .map_err(|err| PiCompatError::Failed(err.to_string()))
    }
}

struct PiBridgeHandle {
    host: Arc<PiCompatHost>,
    package: PiPackageId,
    description: PluginDescription,
    ownership_key: String,
}

#[async_trait]
impl SourceHandle for PiBridgeHandle {
    fn ownership_key(&self) -> &str {
        &self.ownership_key
    }

    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        match request.operation {
            SourceOperation::InvokeTool { name, arguments } => {
                let value = self
                    .host
                    .call_pinned_tool(&self.package, &self.description, &name, arguments, cancel)
                    .await
                    .map_err(|err| match err {
                        PiCompatError::Cancelled => CapabilityError::stopped(
                            cayw_codemode::CapabilityStop::Cancelled,
                            "pi tool cancelled",
                        ),
                        PiCompatError::Timeout => CapabilityError::stopped(
                            cayw_codemode::CapabilityStop::Timeout,
                            "pi tool timed out",
                        ),
                        other if other.outcome_unknown() => {
                            CapabilityError::unknown(other.to_string())
                        }
                        other => CapabilityError::new(other.to_string()),
                    })?;
                Ok(SourceReply {
                    key: request.key,
                    value,
                })
            }
            _ => Err(CapabilityError::new(
                "pi source handle only supports InvokeTool operation",
            )),
        }
    }

    fn disconnect(&self) {
        let _ = self.host.registry.unload_exact(&self.description);
    }
}

fn absolute_existing_or_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn map_plugin_err(err: PluginError) -> PiCompatError {
    match err {
        PluginError::NotFound(name) => PiCompatError::NotLoaded(name),
        PluginError::Conflict(name) => PiCompatError::AlreadyLoaded(name),
        PluginError::Denied(msg)
        | PluginError::InvalidArguments(msg)
        | PluginError::InvalidManifest(msg)
        | PluginError::Protocol(msg)
        | PluginError::Io(msg)
        | PluginError::Unavailable(msg)
        | PluginError::Failed(msg) => PiCompatError::Failed(msg),
        PluginError::Cancelled => PiCompatError::Cancelled,
        PluginError::Timeout => PiCompatError::Timeout,
        // Unloaded mid-call: side effects may already have started.
        PluginError::Unloaded => PiCompatError::OutcomeUnknown(err.to_string()),
        PluginError::OutcomeUnknown(msg) => PiCompatError::OutcomeUnknown(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_bridge::{Operation, RequestKey};
    use serde_json::json;

    #[tokio::test]
    async fn old_bridge_source_cannot_call_or_unload_a_replacement() {
        let host = Arc::new(PiCompatHost::new(true));
        let package = PiFixturePlugin::package_id();
        let description = host.install_fixture().unwrap();
        let source = PiBridgeHandle {
            host: host.clone(),
            package: package.clone(),
            description,
            ownership_key: "old-pi-source".into(),
        };
        host.unload(&package).unwrap();
        let replacement = Arc::new(PiFixturePlugin::new());
        let current = host
            .registry
            .load_native(
                replacement.clone(),
                PluginContext::new([PiFixturePlugin::TOOL.into()], []),
            )
            .unwrap();
        let error = source
            .request(
                SourceRequest {
                    key: RequestKey {
                        id: 1,
                        source: Provenance {
                            registry_incarnation: "test".into(),
                            source: SourceIdentity {
                                id: package.name.clone(),
                                version: package.version.clone(),
                                origin: "pi-compat".into(),
                            },
                            generation: 1,
                            manifest_sha256: "0".repeat(64),
                        },
                        operation: Operation::Invoke,
                        target: PiFixturePlugin::TOOL.into(),
                    },
                    operation: SourceOperation::InvokeTool {
                        name: PiFixturePlugin::TOOL.into(),
                        arguments: json!({"text":"stale"}),
                    },
                    max_result_bytes: 4096,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(replacement.call_count(), 0);
        source.disconnect();
        assert_eq!(host.registry.describe(&package.name).unwrap(), current);
        assert_eq!(
            host.call_tool(
                &package,
                PiFixturePlugin::TOOL,
                json!({"text":"current"}),
                CancellationToken::new()
            )
            .await
            .unwrap()["echo"],
            "current"
        );
    }
}
