use crate::types::*;

/// Static first-cut map from Pi surface family → intended CAYW seam description.
///
/// This is documentation-grade routing used by the host and matrix; it does not
/// load Node packages or claim full ExtensionAPI coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyMapping {
    pub family: SurfaceFamily,
    pub cayw_seam: &'static str,
    pub default_level: CompatibilityLevel,
    pub note: &'static str,
}

pub const FAMILY_MAPPINGS: &[FamilyMapping] = &[
    FamilyMapping {
        family: SurfaceFamily::Tools,
        cayw_seam: "cayw_plugins::PluginRegistry (capabilities / call)",
        default_level: CompatibilityLevel::Partial,
        note: "registerTool / invoke subset via native or external plugins",
    },
    FamilyMapping {
        family: SurfaceFamily::Commands,
        cayw_seam: "未接线（后续：Runtime/WebUI 命令槽）",
        default_level: CompatibilityLevel::Unavailable,
        note: "registerCommand not in first cut",
    },
    FamilyMapping {
        family: SurfaceFamily::Session,
        cayw_seam: "cayw-core / cayw-runtime Session（权威在 CAYW）",
        default_level: CompatibilityLevel::Unavailable,
        note: "Pi SessionManager not exposed",
    },
    FamilyMapping {
        family: SurfaceFamily::Events,
        cayw_seam: "未接线（后续：Runtime 事件投影）",
        default_level: CompatibilityLevel::Unavailable,
        note: "ExtensionAPI.on event bus not wired",
    },
    FamilyMapping {
        family: SurfaceFamily::Interaction,
        cayw_seam: "未接线（无 Pi UIContext）",
        default_level: CompatibilityLevel::Incompatible,
        note: "UI select/confirm/input incompatible in Core-only first cut",
    },
    FamilyMapping {
        family: SurfaceFamily::Lifecycle,
        cayw_seam: "cayw_plugins::PluginRegistry::load_* / unload",
        default_level: CompatibilityLevel::Partial,
        note: "install / unload / generation cancel",
    },
];

pub fn mapping_for(family: SurfaceFamily) -> &'static FamilyMapping {
    FAMILY_MAPPINGS
        .iter()
        .find(|m| m.family == family)
        .expect("all SurfaceFamily variants listed")
}
