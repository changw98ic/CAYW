use crate::error::PiCompatError;
use crate::fixture::PiFixturePlugin;
use crate::sample::PiSampleExtension;
use crate::matrix::CompatMatrix;
use crate::types::*;
use cayw_ai::CancellationToken;
use cayw_plugins::{
    ExternalConfig, PluginContext, PluginDescription, PluginError, PluginRegistry,
};
use serde_json::Value;
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
/// Two install paths:
/// - [`Self::install_fixture`] — in-process Rust [`PiFixturePlugin`] via `load_native` (CI).
/// - [`Self::install_sample_extension`] / [`Self::install_external`] — disk subprocess
///   via `load_external` (real extension path beyond the fixture).
pub struct PiCompatHost {
    enabled: bool,
    registry: PluginRegistry,
    matrix: Mutex<CompatMatrix>,
}

impl PiCompatHost {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            registry: PluginRegistry::new(),
            matrix: Mutex::new(CompatMatrix::first_cut_baseline()),
        }
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
    /// This is the real path beyond the in-process fixture. The executable must
    /// already exist; missing paths refuse with [`PiCompatError::Unavailable`]
    /// (never silent success). Unmodified npm Pi ExtensionAPI packages are still
    /// out of scope — use a CAYW external-protocol speaker (see sample ext).
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
        let config = ExternalConfig::new(
            executable,
            vec![],
            PiSampleExtension::expected_manifest(),
        );
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

        let desc = self
            .registry
            .describe(&package.name)
            .map_err(|_| PiCompatError::NotLoaded(package.key()))?;
        if !desc
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
            .call(&package.name, tool, arguments, cancel, Duration::from_secs(5))
            .await
        {
            Ok(value) => Ok(value),
            Err(err) => {
                // Cancelled/Timeout are known terminals (remote convention); do not
                // elevate them to OutcomeUnknown. Unloaded/Protocol/Io stay unknown.
                match &err {
                    PluginError::Cancelled => Err(PiCompatError::Cancelled),
                    PluginError::Timeout => Err(PiCompatError::Timeout),
                    other if other.outcome_unknown() => {
                        Err(PiCompatError::OutcomeUnknown(other.to_string()))
                    }
                    other => {
                        let mut matrix = self.matrix.lock().expect("compat matrix poisoned");
                        matrix.record_incompatible(package.clone(), surface, other.to_string());
                        Err(map_plugin_err(err))
                    }
                }
            }
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
        self.registry
            .unload(&package.name)
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
