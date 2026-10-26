//! Host-owned panel workbench for the loopback WebUI.
//!
//! Keeps the shared [`cayw_panel`] registry and session hooks alive for the
//! gateway process, seeds the first-cut official/third-party sample panels, and
//! exposes catalog / open / activate / close / interact helpers for HTTP and UI.

use cayw_panel::{
    ContributorKind, OpenPanelRequest, OpenedPanel, PANEL_PROTOCOL_VERSION, PanelDescriptor,
    PanelError, PanelHandle, PanelPlacement, PanelRegistry, PanelZone, WorkspaceHooks,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Official + sample contributions held for the lifetime of the WebUI process.
pub struct Workbench {
    registry: Arc<PanelRegistry>,
    hooks: WorkspaceHooks,
    _handles: Vec<PanelHandle>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PanelCatalog {
    pub protocol_version: u32,
    pub registry_id: String,
    pub sidebar: Vec<PanelDescriptor>,
    pub bottom: Vec<PanelDescriptor>,
}

impl Workbench {
    pub fn with_builtin_panels() -> Result<Self, PanelError> {
        let registry = Arc::new(PanelRegistry::new());
        let handles = vec![
            registry.register_panel(PanelDescriptor {
                id: "files".into(),
                title: "Files".into(),
                contributor: ContributorKind::Official,
                placement: PanelPlacement::Both,
                order: 10,
                description: Some("Workspace file tree / editor reveal".into()),
                hidden: false,
            })?,
            registry.register_panel(PanelDescriptor {
                id: "terminal-bottom".into(),
                title: "Terminal".into(),
                contributor: ContributorKind::Official,
                placement: PanelPlacement::BottomOnly,
                order: 20,
                description: Some("Bottom terminal slot".into()),
                hidden: false,
            })?,
            registry.register_panel(PanelDescriptor {
                id: "vendor:demo".into(),
                title: "Demo".into(),
                contributor: ContributorKind::ThirdParty,
                placement: PanelPlacement::SidebarOnly,
                order: 40,
                description: Some("Sample third-party sidebar panel".into()),
                hidden: false,
            })?,
        ];
        let hooks = WorkspaceHooks::new(Arc::clone(&registry));
        Ok(Self {
            registry,
            hooks,
            _handles: handles,
        })
    }

    pub fn catalog(&self) -> PanelCatalog {
        PanelCatalog {
            protocol_version: PANEL_PROTOCOL_VERSION,
            registry_id: self.registry.registry_id().to_owned(),
            sidebar: self.registry.guide_entries(PanelZone::Sidebar),
            bottom: self.registry.guide_entries(PanelZone::Bottom),
        }
    }

    pub fn open(
        &self,
        session_id: &str,
        request: OpenPanelRequest,
    ) -> Result<OpenedPanel, PanelError> {
        self.hooks.open(session_id, request)
    }

    pub fn activate(&self, session_id: &str, instance_id: &str) -> Result<OpenedPanel, PanelError> {
        self.hooks.activate(session_id, instance_id)
    }

    pub fn close(&self, session_id: &str, instance_id: &str) -> Result<OpenedPanel, PanelError> {
        self.hooks.close(session_id, instance_id)
    }

    pub fn list_open(
        &self,
        session_id: &str,
        zone: Option<PanelZone>,
    ) -> Result<Vec<OpenedPanel>, PanelError> {
        self.hooks.list_open(session_id, zone)
    }

    /// Minimal editor / terminal interaction against an open instance.
    ///
    /// - `editor`: re-open the same panel id with an updated path (reveal).
    /// - `terminal`: activate a bottom-zone terminal instance.
    pub fn interact(
        &self,
        session_id: &str,
        instance_id: &str,
        kind: &str,
        path: Option<String>,
    ) -> Result<OpenedPanel, PanelError> {
        let opened = self
            .list_open(session_id, None)?
            .into_iter()
            .find(|item| item.instance_id == instance_id)
            .ok_or_else(|| PanelError::NotFound(instance_id.into()))?;
        match kind {
            "editor" => {
                let path = path
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| PanelError::Invalid("editor interact requires path".into()))?;
                // Close+reopen keeps WorkspaceHooks as the single mutator while
                // refreshing the file-oriented path hint for the same panel id.
                let _ = self.hooks.close(session_id, instance_id)?;
                self.hooks.open(
                    session_id,
                    OpenPanelRequest {
                        panel_id: opened.panel_id,
                        zone: Some(opened.zone),
                        path: Some(path),
                        activate: true,
                    },
                )
            }
            "terminal" => {
                if opened.zone != PanelZone::Bottom {
                    return Err(PanelError::Invalid(
                        "terminal interact requires a bottom-zone panel".into(),
                    ));
                }
                self.hooks.activate(session_id, instance_id)
            }
            _ => Err(PanelError::Invalid(format!("unknown interact kind {kind}"))),
        }
    }
}

pub fn panel_error_response(error: PanelError) -> (u16, Value) {
    let (status, code) = match &error {
        PanelError::Invalid(_) => (400, "invalid_request"),
        PanelError::Conflict(_) => (409, "conflict"),
        PanelError::NotFound(_) => (404, "not_found"),
        PanelError::Unloaded => (404, "unloaded"),
        PanelError::ZoneNotAllowed(_) => (400, "zone_not_allowed"),
        PanelError::Unavailable(_) => (503, "unavailable"),
    };
    (
        status,
        json!({"error":{"code":code,"message":error.to_string(),"outcome_unknown":false}}),
    )
}
