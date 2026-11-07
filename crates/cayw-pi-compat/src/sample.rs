use cayw_ai::ToolDefinition;
use cayw_plugins::{PLUGIN_PROTOCOL_VERSION, PluginKind, PluginManifest};
use serde_json::json;

/// Disk / subprocess sample extension identity for the real (non-fixture) path.
///
/// Speaks the CAYW external plugin JSON protocol over stdin/stdout. Distinct
/// from [`crate::PiFixturePlugin`] (in-process `load_native`). Used to prove
/// `PluginRegistry::load_external` through [`crate::PiCompatHost`].
pub struct PiSampleExtension;

impl PiSampleExtension {
    pub const PACKAGE_NAME: &'static str = "cayw-pi-sample-ext";
    pub const PACKAGE_VERSION: &'static str = "0.1.0";
    pub const TOOL: &'static str = "pi_sample.ping";
    /// Binary target name (`cargo run -p cayw-pi-compat --bin cayw-pi-sample-ext`).
    pub const BIN_NAME: &'static str = "cayw-pi-sample-ext";

    pub fn package_id() -> crate::types::PiPackageId {
        crate::types::PiPackageId::new(Self::PACKAGE_NAME, Self::PACKAGE_VERSION)
    }

    pub fn expected_manifest() -> PluginManifest {
        PluginManifest {
            name: Self::PACKAGE_NAME.into(),
            version: Self::PACKAGE_VERSION.into(),
            protocol_version: PLUGIN_PROTOCOL_VERSION,
            kind: PluginKind::External,
            capabilities: vec![ToolDefinition {
                name: Self::TOOL.into(),
                description: "Minimal Pi-compat sample extension ping (disk/subprocess path)"
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "note": { "type": "string" }
                    },
                    "additionalProperties": false
                }),
            }],
            permissions: Default::default(),
        }
    }
}
