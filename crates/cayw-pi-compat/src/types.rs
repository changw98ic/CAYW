use serde::{Deserialize, Serialize};

/// Wire / document protocol for this first-cut Pi compat host.
pub const PI_COMPAT_PROTOCOL_VERSION: u32 = 1;

/// Pinned Pi coding-agent ABI inventory used by the first-cut matrix
/// (same pin as pi2dsh's documented ExtensionAPI baseline).
pub const PINNED_PI_CODING_AGENT: &str = "@earendil-works/pi-coding-agent";
pub const PINNED_PI_CODING_AGENT_VERSION: &str = "0.84.1";

/// CAYW side of the version pair for this slice.
pub const CAYW_PLUGIN_PROTOCOL: u32 = cayw_plugins::PLUGIN_PROTOCOL_VERSION;

/// High-level Pi public extension surface families mapped by F15.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceFamily {
    Tools,
    Commands,
    Session,
    Events,
    Interaction,
    Lifecycle,
}

impl SurfaceFamily {
    pub const ALL: [SurfaceFamily; 6] = [
        Self::Tools,
        Self::Commands,
        Self::Session,
        Self::Events,
        Self::Interaction,
        Self::Lifecycle,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Commands => "commands",
            Self::Session => "session",
            Self::Events => "events",
            Self::Interaction => "interaction",
            Self::Lifecycle => "lifecycle",
        }
    }
}

/// Compatibility verdict for one surface or plugin row.
///
/// Failures must be recorded as [`Incompatible`] or [`Unavailable`], never
/// silently treated as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityLevel {
    /// Semantics match the documented CAYW seam for this slice.
    Full,
    /// Accepted with a stated difference (see row detail).
    Partial,
    /// Not implemented on this host yet; call is refused.
    Unavailable,
    /// Explicitly incompatible for the pinned versions; call is refused.
    Incompatible,
}

impl CompatibilityLevel {
    pub fn allows_invoke(self) -> bool {
        matches!(self, Self::Full | Self::Partial)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Partial => "partial",
            Self::Unavailable => "unavailable",
            Self::Incompatible => "incompatible",
        }
    }
}

/// Identity of a Pi-shaped package row in the matrix.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PiPackageId {
    pub name: String,
    pub version: String,
}

impl PiPackageId {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }

    pub fn key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// One mapped Pi surface leaf (family + name).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SurfaceId {
    pub family: SurfaceFamily,
    pub name: String,
}

impl SurfaceId {
    pub fn new(family: SurfaceFamily, name: impl Into<String>) -> Self {
        Self {
            family,
            name: name.into(),
        }
    }

    pub fn key(&self) -> String {
        format!("{}.{}", self.family.as_str(), self.name)
    }
}
