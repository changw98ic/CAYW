//! Serializable panel and file-viewer contribution descriptors.

use crate::identity::{validate_contribution_id, validate_extension, validate_title};
use serde::{Deserialize, Serialize};

/// First-cut workbench zones shared by official modules and third parties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelZone {
    Sidebar,
    Bottom,
}

impl PanelZone {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sidebar => "sidebar",
            Self::Bottom => "bottom",
        }
    }
}

/// Who registered a contribution. Both kinds use the same registry API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributorKind {
    Official,
    ThirdParty,
}

/// Where a panel may appear. `Both` is the default for ordinary tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelPlacement {
    /// May open in sidebar or bottom (caller chooses zone at open time).
    Both,
    /// Sidebar only.
    SidebarOnly,
    /// Bottom workspace only; never registered as a sidebar guide entry.
    BottomOnly,
}

impl PanelPlacement {
    pub fn allows(self, zone: PanelZone) -> bool {
        match self {
            Self::Both => true,
            Self::SidebarOnly => zone == PanelZone::Sidebar,
            Self::BottomOnly => zone == PanelZone::Bottom,
        }
    }

    pub fn default_zone(self) -> PanelZone {
        match self {
            Self::Both | Self::SidebarOnly => PanelZone::Sidebar,
            Self::BottomOnly => PanelZone::Bottom,
        }
    }
}

/// A panel / tab type contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelDescriptor {
    pub id: String,
    pub title: String,
    pub contributor: ContributorKind,
    pub placement: PanelPlacement,
    /// Stable sort key; lower values appear first. Ties break by id.
    pub order: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hidden from the new-tab guide / `+` menu while still openable by id.
    #[serde(default)]
    pub hidden: bool,
}

impl PanelDescriptor {
    pub fn validate(self) -> Result<Self, PanelError> {
        validate_contribution_id(&self.id).map_err(PanelError::Invalid)?;
        let title = validate_title(&self.title).map_err(PanelError::Invalid)?;
        let description = match self.description {
            Some(value) => Some(validate_title(&value).map_err(PanelError::Invalid)?),
            None => None,
        };
        Ok(Self {
            title,
            description,
            ..self
        })
    }
}

/// A file-viewer contribution selected by file extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileViewerDescriptor {
    pub id: String,
    pub title: String,
    pub contributor: ContributorKind,
    /// Extensions without a leading dot. Stored lowercase.
    pub extensions: Vec<String>,
    /// Higher priority wins when several viewers claim the same extension.
    /// Ties break by id (lexicographic ascending = earlier wins).
    pub priority: i32,
}

impl FileViewerDescriptor {
    pub fn validate(self) -> Result<Self, PanelError> {
        validate_contribution_id(&self.id).map_err(PanelError::Invalid)?;
        let title = validate_title(&self.title).map_err(PanelError::Invalid)?;
        if self.extensions.is_empty() {
            return Err(PanelError::Invalid(
                "file viewer requires at least one extension".into(),
            ));
        }
        let mut extensions = Vec::with_capacity(self.extensions.len());
        for extension in self.extensions {
            let normalized = validate_extension(&extension).map_err(PanelError::Invalid)?;
            if extensions.contains(&normalized) {
                return Err(PanelError::Invalid(format!(
                    "duplicate extension {normalized}"
                )));
            }
            extensions.push(normalized);
        }
        Ok(Self {
            title,
            extensions,
            ..self
        })
    }
}

/// Snapshot entry returned by list / describe APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelDescription {
    pub registry_id: String,
    pub generation: u64,
    pub kind: ContributionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel: Option<PanelDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_viewer: Option<FileViewerDescriptor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributionKind {
    Panel,
    FileViewer,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PanelError {
    #[error("invalid panel contribution: {0}")]
    Invalid(String),
    #[error("panel contribution already registered: {0}")]
    Conflict(String),
    #[error("panel contribution not registered: {0}")]
    NotFound(String),
    #[error("panel contribution unloaded")]
    Unloaded,
    #[error("panel zone not allowed for contribution {0}")]
    ZoneNotAllowed(String),
    #[error("panel registry unavailable: {0}")]
    Unavailable(String),
}
