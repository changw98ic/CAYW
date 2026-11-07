use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_plugins::{PLUGIN_PROTOCOL_VERSION, Plugin, PluginError, PluginKind, PluginManifest};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

/// Offline stand-in for one matrix plugin row: `cayw-pi-fixture@0.1.0`.
///
/// Speaks the CAYW native plugin trait (not an unmodified npm ExtensionAPI
/// package). Used to prove install → call → unload on the optional compat host.
pub struct PiFixturePlugin {
    calls: AtomicU64,
}

impl Default for PiFixturePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl PiFixturePlugin {
    pub const PACKAGE_NAME: &'static str = "cayw-pi-fixture";
    pub const PACKAGE_VERSION: &'static str = "0.1.0";
    pub const TOOL: &'static str = "pi_fixture.echo";

    pub fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
        }
    }

    pub fn package_id() -> crate::types::PiPackageId {
        crate::types::PiPackageId::new(Self::PACKAGE_NAME, Self::PACKAGE_VERSION)
    }

    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn build_manifest() -> PluginManifest {
        PluginManifest {
            name: Self::PACKAGE_NAME.into(),
            version: Self::PACKAGE_VERSION.into(),
            protocol_version: PLUGIN_PROTOCOL_VERSION,
            kind: PluginKind::Native,
            capabilities: vec![ToolDefinition {
                name: Self::TOOL.into(),
                description: "Offline Pi-compat fixture echo (F15 first cut)".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }),
            }],
            permissions: Default::default(),
        }
    }
}

#[async_trait]
impl Plugin for PiFixturePlugin {
    fn manifest(&self) -> &PluginManifest {
        use std::sync::OnceLock;
        static MANIFEST: OnceLock<PluginManifest> = OnceLock::new();
        MANIFEST.get_or_init(Self::build_manifest)
    }

    async fn call(
        &self,
        method: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, PluginError> {
        if cancel.is_cancelled() {
            return Err(PluginError::Cancelled);
        }
        if method != Self::TOOL {
            return Err(PluginError::Denied(method.into()));
        }
        let text = arguments
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| PluginError::InvalidArguments("missing text".into()))?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({
            "echo": text,
            "package": Self::PACKAGE_NAME,
            "version": Self::PACKAGE_VERSION,
            "surface": "tools.invoke"
        }))
    }
}
