use crate::error::PiCompatError;
use crate::fixture::PiFixturePlugin;
use crate::matrix::CompatMatrix;
use crate::types::*;
use cayw_ai::CancellationToken;
use cayw_plugins::{PluginContext, PluginDescription, PluginError, PluginRegistry};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Optional Pi→CAYW external host mapping (F15 first cut).
///
/// Disabled by default constructors that take `enabled: false`. When enabled,
/// install/call/unload go through `cayw-plugins::PluginRegistry`. Surface
/// checks consult the [`CompatMatrix`]; refused surfaces are recorded as
/// incompatible / unavailable — never silent success.
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

    /// Install the offline fixture package (`cayw-pi-fixture@0.1.0`).
    pub fn install_fixture(&self) -> Result<PluginDescription, PiCompatError> {
        self.ensure_enabled()?;
        let package = PiFixturePlugin::package_id();
        let surface = SurfaceId::new(SurfaceFamily::Lifecycle, "install");
        let level = {
            let matrix = self.matrix.lock().expect("compat matrix poisoned");
            // Lifecycle unload/install share family mapping; use unload row or tools.registerTool.
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

    /// Invoke a mapped tool surface for a loaded package.
    pub async fn call_tool(
        &self,
        package: &PiPackageId,
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

        self.registry
            .describe(&package.name)
            .map_err(|_| PiCompatError::NotLoaded(package.key()))?;

        match self
            .registry
            .call(
                &package.name,
                PiFixturePlugin::TOOL,
                arguments,
                cancel,
                Duration::from_secs(5),
            )
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
