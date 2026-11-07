use crate::types::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One matrix cell: plugin@version × surface → verdict + notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatRow {
    pub package: PiPackageId,
    pub surface: SurfaceId,
    pub level: CompatibilityLevel,
    /// Human-readable detail (Chinese-ready docs may quote this).
    pub detail: String,
    /// CAYW seam that carries the mapping when level allows invoke.
    pub cayw_seam: Option<String>,
}

/// Version / compatibility matrix for the optional Pi→CAYW host.
///
/// First cut proves the **hello tool slice** only (`registerTool` / `invoke`).
/// Commands / events / sendMessage / session / UI are unavailable or incompatible.
/// Does **not** claim full Pi ExtensionAPI or F15 complete. Missing combinations
/// are [`CompatibilityLevel::Unavailable`] when queried, never silent success.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompatMatrix {
    pub pi_abi_package: String,
    pub pi_abi_version: String,
    pub cayw_plugin_protocol: u32,
    pub pi_compat_protocol: u32,
    rows: BTreeMap<String, CompatRow>,
}

impl CompatMatrix {
    pub fn first_cut_baseline() -> Self {
        let mut matrix = Self {
            pi_abi_package: PINNED_PI_CODING_AGENT.into(),
            pi_abi_version: PINNED_PI_CODING_AGENT_VERSION.into(),
            cayw_plugin_protocol: CAYW_PLUGIN_PROTOCOL,
            pi_compat_protocol: PI_COMPAT_PROTOCOL_VERSION,
            rows: BTreeMap::new(),
        };
        matrix.seed_first_cut_rows();
        matrix
    }

    fn seed_first_cut_rows(&mut self) {
        // Fixture package used for offline install/call/unload smoke.
        let fixture = PiPackageId::new("cayw-pi-fixture", "0.1.0");
        self.insert(CompatRow {
            package: fixture.clone(),
            surface: SurfaceId::new(SurfaceFamily::Tools, "registerTool"),
            level: CompatibilityLevel::Partial,
            detail: "Maps to cayw-plugins capability registration on load; not a Node ExtensionAPI object.".into(),
            cayw_seam: Some("cayw_plugins::PluginRegistry::load_native".into()),
        });
        self.insert(CompatRow {
            package: fixture.clone(),
            surface: SurfaceId::new(SurfaceFamily::Tools, "invoke"),
            level: CompatibilityLevel::Partial,
            detail: "Maps to PluginRegistry::call with host-granted context.".into(),
            cayw_seam: Some("cayw_plugins::PluginRegistry::call".into()),
        });
        self.insert(CompatRow {
            package: fixture.clone(),
            surface: SurfaceId::new(SurfaceFamily::Lifecycle, "unload"),
            level: CompatibilityLevel::Partial,
            detail: "Maps to PluginRegistry::unload; cancels in-flight generation.".into(),
            cayw_seam: Some("cayw_plugins::PluginRegistry::unload".into()),
        });
        self.insert(CompatRow {
            package: fixture.clone(),
            surface: SurfaceId::new(SurfaceFamily::Commands, "registerCommand"),
            level: CompatibilityLevel::Unavailable,
            detail: "Slash/command registration not in F15 first cut.".into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: fixture.clone(),
            surface: SurfaceId::new(SurfaceFamily::Events, "on"),
            level: CompatibilityLevel::Unavailable,
            detail: "Pi event bus subscription not wired in first cut.".into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: fixture.clone(),
            surface: SurfaceId::new(SurfaceFamily::Session, "sessionManager"),
            level: CompatibilityLevel::Unavailable,
            detail: "Pi SessionManager not exposed; CAYW Session remains authoritative.".into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: fixture.clone(),
            surface: SurfaceId::new(SurfaceFamily::Interaction, "ui.select"),
            level: CompatibilityLevel::Incompatible,
            detail: "No Pi UIContext bridge in Rust Core first cut; explicit incompatible.".into(),
            cayw_seam: None,
        });

        // Disk/subprocess sample extension (real path beyond in-process fixture).
        let sample = PiPackageId::new("cayw-pi-sample-ext", "0.1.0");
        self.insert(CompatRow {
            package: sample.clone(),
            surface: SurfaceId::new(SurfaceFamily::Tools, "registerTool"),
            level: CompatibilityLevel::Partial,
            detail: "Homemade CAYW-protocol sample via load_external (not Pi ExtensionAPI).".into(),
            cayw_seam: Some("cayw_plugins::PluginRegistry::load_external".into()),
        });
        self.insert(CompatRow {
            package: sample.clone(),
            surface: SurfaceId::new(SurfaceFamily::Tools, "invoke"),
            level: CompatibilityLevel::Partial,
            detail: "Homemade sample pi_sample.ping via external JSON protocol (not Pi ABI)."
                .into(),
            cayw_seam: Some("cayw_plugins::PluginRegistry::call".into()),
        });
        self.insert(CompatRow {
            package: sample.clone(),
            surface: SurfaceId::new(SurfaceFamily::Lifecycle, "unload"),
            level: CompatibilityLevel::Partial,
            detail: "Maps to PluginRegistry::unload for the external child.".into(),
            cayw_seam: Some("cayw_plugins::PluginRegistry::unload".into()),
        });
        self.insert(CompatRow {
            package: sample.clone(),
            surface: SurfaceId::new(SurfaceFamily::Commands, "registerCommand"),
            level: CompatibilityLevel::Unavailable,
            detail: "Slash/command registration not in F15 first cut.".into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: sample.clone(),
            surface: SurfaceId::new(SurfaceFamily::Events, "on"),
            level: CompatibilityLevel::Unavailable,
            detail: "Pi event bus subscription not wired in first cut.".into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: sample.clone(),
            surface: SurfaceId::new(SurfaceFamily::Session, "sessionManager"),
            level: CompatibilityLevel::Unavailable,
            detail: "Pi SessionManager not exposed; CAYW Session remains authoritative.".into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: sample.clone(),
            surface: SurfaceId::new(SurfaceFamily::Interaction, "ui.select"),
            level: CompatibilityLevel::Incompatible,
            detail: "No Pi UIContext bridge in Rust Core first cut; explicit incompatible.".into(),
            cayw_seam: None,
        });

        // Primary acceptance: unmodified npm Pi hello.ts via Node ExtensionAPI adapter.
        let real = PiPackageId::new(PINNED_PI_CODING_AGENT, PINNED_PI_CODING_AGENT_VERSION);
        self.insert(CompatRow {
            package: real.clone(),
            surface: SurfaceId::new(SurfaceFamily::Tools, "registerTool"),
            level: CompatibilityLevel::Partial,
            detail: "Proven hello-tool slice only: unmodified examples/extensions/hello.ts via npm/pi-npm-bridge.mjs (registerTool) + load_external. Not full ExtensionAPI.".into(),
            cayw_seam: Some("cayw_plugins::PluginRegistry::load_external + pi-npm-bridge".into()),
        });
        self.insert(CompatRow {
            package: real.clone(),
            surface: SurfaceId::new(SurfaceFamily::Tools, "invoke"),
            level: CompatibilityLevel::Partial,
            detail: "hello tool execute() from unmodified npm extension only; adapter returns Pi AgentToolResult as CAYW JSON. Other ExtensionAPI surfaces unsupported.".into(),
            cayw_seam: Some("cayw_plugins::PluginRegistry::call + pi-npm-bridge".into()),
        });
        self.insert(CompatRow {
            package: real.clone(),
            surface: SurfaceId::new(SurfaceFamily::Lifecycle, "install"),
            level: CompatibilityLevel::Partial,
            detail: "Node >=22.19 + npm ci deps required; missing pieces record unavailable."
                .into(),
            cayw_seam: Some("PiCompatHost::install_npm_hello_extension".into()),
        });
        self.insert(CompatRow {
            package: real.clone(),
            surface: SurfaceId::new(SurfaceFamily::Lifecycle, "unload"),
            level: CompatibilityLevel::Partial,
            detail: "Maps to PluginRegistry::unload (kills Node adapter child).".into(),
            cayw_seam: Some("cayw_plugins::PluginRegistry::unload".into()),
        });
        self.insert(CompatRow {
            package: real.clone(),
            surface: SurfaceId::new(SurfaceFamily::Commands, "registerCommand"),
            level: CompatibilityLevel::Unavailable,
            detail: "Bridge throws unsupported on registerCommand (not a silent noop); not a CAYW capability."
                .into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: real.clone(),
            surface: SurfaceId::new(SurfaceFamily::Events, "on"),
            level: CompatibilityLevel::Unavailable,
            detail: "Bridge throws unsupported on pi.on / events.*; event bus not wired to CAYW."
                .into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: real.clone(),
            surface: SurfaceId::new(SurfaceFamily::Session, "sessionManager"),
            level: CompatibilityLevel::Unavailable,
            detail: "Pi SessionManager not exposed; CAYW Session remains authoritative.".into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: real.clone(),
            surface: SurfaceId::new(SurfaceFamily::Session, "sendMessage"),
            level: CompatibilityLevel::Unavailable,
            detail: "Bridge throws unsupported on sendMessage; not wired to CAYW Session.".into(),
            cayw_seam: None,
        });
        self.insert(CompatRow {
            package: real.clone(),
            surface: SurfaceId::new(SurfaceFamily::Interaction, "ui.select"),
            level: CompatibilityLevel::Incompatible,
            detail: "Bridge throws unsupported on ui.select/confirm/input/notify; no Pi UIContext bridge.".into(),
            cayw_seam: None,
        });
    }

    fn row_key(package: &PiPackageId, surface: &SurfaceId) -> String {
        format!("{}::{}", package.key(), surface.key())
    }

    pub fn insert(&mut self, row: CompatRow) {
        let key = Self::row_key(&row.package, &row.surface);
        self.rows.insert(key, row);
    }

    pub fn rows(&self) -> impl Iterator<Item = &CompatRow> {
        self.rows.values()
    }

    pub fn lookup(&self, package: &PiPackageId, surface: &SurfaceId) -> CompatRow {
        if let Some(row) = self.rows.get(&Self::row_key(package, surface)) {
            return row.clone();
        }
        // Wildcard family row (name "*").
        let wildcard = SurfaceId::new(surface.family, "*");
        if let Some(row) = self.rows.get(&Self::row_key(package, &wildcard)) {
            return CompatRow {
                surface: surface.clone(),
                ..row.clone()
            };
        }
        CompatRow {
            package: package.clone(),
            surface: surface.clone(),
            level: CompatibilityLevel::Unavailable,
            detail: "No matrix row; treated as unavailable (not silent success).".into(),
            cayw_seam: None,
        }
    }

    /// Record an observed failure as incompatible (never silent).
    pub fn record_incompatible(
        &mut self,
        package: PiPackageId,
        surface: SurfaceId,
        detail: impl Into<String>,
    ) -> CompatRow {
        let row = CompatRow {
            package,
            surface,
            level: CompatibilityLevel::Incompatible,
            detail: detail.into(),
            cayw_seam: None,
        };
        self.insert(row.clone());
        row
    }

    /// Record a missing / not-yet-wired surface as unavailable (never silent).
    pub fn record_unavailable(
        &mut self,
        package: PiPackageId,
        surface: SurfaceId,
        detail: impl Into<String>,
    ) -> CompatRow {
        let row = CompatRow {
            package,
            surface,
            level: CompatibilityLevel::Unavailable,
            detail: detail.into(),
            cayw_seam: None,
        };
        self.insert(row.clone());
        row
    }

    pub fn markdown_table(&self) -> String {
        let mut out = String::from(
            "| 包@version | 表面 | 级别 | CAYW 接缝 | 说明 |\n| --- | --- | --- | --- | --- |\n",
        );
        for row in self.rows.values() {
            out.push_str(&format!(
                "| `{}` | `{}` | {} | {} | {} |\n",
                row.package.key(),
                row.surface.key(),
                row.level.as_str(),
                row.cayw_seam.as_deref().unwrap_or("—"),
                row.detail.replace('|', "\\|"),
            ));
        }
        out
    }
}
