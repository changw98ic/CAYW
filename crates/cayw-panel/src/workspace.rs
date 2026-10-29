//! In-memory sidebar / bottom workspace open-state hooks (first cut).

use crate::registry::PanelRegistry;
use crate::types::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Request to open a registered panel in a workbench zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenPanelRequest {
    pub panel_id: String,
    /// When omitted, the contribution's default zone is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<PanelZone>,
    /// Optional path / resource hint for file-oriented panels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Soft focus: place without activating when false.
    #[serde(default = "default_true")]
    pub activate: bool,
}

fn default_true() -> bool {
    true
}

/// One opened panel instance within a session workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenedPanel {
    pub instance_id: String,
    pub panel_id: String,
    pub zone: PanelZone,
    pub title: String,
    pub contributor: ContributorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub active: bool,
}

#[derive(Default)]
struct SessionState {
    next_instance: u64,
    opened: Vec<OpenedPanel>,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<String, SessionState>,
}

/// Session-scoped open / activate / close hooks over a shared [`PanelRegistry`].
///
/// This first cut keeps placement state in memory. It does not render UI, touch
/// Core, or grant permissions. Official and third-party panels share these hooks.
pub struct WorkspaceHooks {
    registry: Arc<PanelRegistry>,
    state: Mutex<State>,
}

impl WorkspaceHooks {
    pub fn new(registry: Arc<PanelRegistry>) -> Self {
        Self {
            registry,
            state: Mutex::default(),
        }
    }

    pub fn registry(&self) -> &Arc<PanelRegistry> {
        &self.registry
    }

    pub fn open(
        &self,
        session_id: &str,
        request: OpenPanelRequest,
    ) -> Result<OpenedPanel, PanelError> {
        validate_session_id(session_id)?;
        // Resolve descriptor and target zone under a single registry snapshot.
        let (panel, zone) = self
            .registry
            .require_panel(&request.panel_id, request.zone)?;
        let path = match request.path {
            Some(value) => Some(validate_path(&value)?),
            None => None,
        };

        let mut state = self.state.lock().expect("workspace hooks poisoned");
        let session = state.sessions.entry(session_id.to_owned()).or_default();
        session.next_instance = session
            .next_instance
            .checked_add(1)
            .ok_or_else(|| PanelError::Unavailable("instance id exhausted".into()))?;
        let instance_id = format!("{}:{}", request.panel_id, session.next_instance);
        if request.activate {
            for opened in &mut session.opened {
                if opened.zone == zone {
                    opened.active = false;
                }
            }
        }
        let opened = OpenedPanel {
            instance_id,
            panel_id: panel.id.clone(),
            zone,
            title: panel.title.clone(),
            contributor: panel.contributor,
            path,
            active: request.activate,
        };
        session.opened.push(opened.clone());
        Ok(opened)
    }

    pub fn activate(&self, session_id: &str, instance_id: &str) -> Result<OpenedPanel, PanelError> {
        validate_session_id(session_id)?;
        let mut state = self.state.lock().expect("workspace hooks poisoned");
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| PanelError::NotFound(session_id.into()))?;
        let zone = session
            .opened
            .iter()
            .find(|opened| opened.instance_id == instance_id)
            .map(|opened| opened.zone)
            .ok_or_else(|| PanelError::NotFound(instance_id.into()))?;
        for opened in &mut session.opened {
            if opened.zone == zone {
                opened.active = opened.instance_id == instance_id;
            }
        }
        session
            .opened
            .iter()
            .find(|opened| opened.instance_id == instance_id)
            .cloned()
            .ok_or_else(|| PanelError::NotFound(instance_id.into()))
    }

    pub fn update_path(
        &self,
        session_id: &str,
        instance_id: &str,
        path: &str,
    ) -> Result<OpenedPanel, PanelError> {
        validate_session_id(session_id)?;
        let normalized = validate_path(path)?;
        let mut state = self.state.lock().expect("workspace hooks poisoned");
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| PanelError::NotFound(session_id.into()))?;
        let zone = session
            .opened
            .iter()
            .find(|opened| opened.instance_id == instance_id)
            .map(|opened| opened.zone)
            .ok_or_else(|| PanelError::NotFound(instance_id.into()))?;
        let mut updated = None;
        for opened in &mut session.opened {
            if opened.zone == zone {
                if opened.instance_id == instance_id {
                    opened.path = Some(normalized.clone());
                    opened.active = true;
                    updated = Some(opened.clone());
                } else {
                    opened.active = false;
                }
            }
        }
        updated.ok_or_else(|| PanelError::NotFound(instance_id.into()))
    }

    pub fn close(&self, session_id: &str, instance_id: &str) -> Result<OpenedPanel, PanelError> {
        validate_session_id(session_id)?;
        let mut state = self.state.lock().expect("workspace hooks poisoned");
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| PanelError::NotFound(session_id.into()))?;
        let position = session
            .opened
            .iter()
            .position(|opened| opened.instance_id == instance_id)
            .ok_or_else(|| PanelError::NotFound(instance_id.into()))?;
        let removed = session.opened.remove(position);
        if removed.active
            && let Some(fallback) = session
                .opened
                .iter_mut()
                .rev()
                .find(|opened| opened.zone == removed.zone)
        {
            fallback.active = true;
        }
        if session.opened.is_empty() {
            state.sessions.remove(session_id);
        }
        Ok(removed)
    }

    pub fn list_open(
        &self,
        session_id: &str,
        zone: Option<PanelZone>,
    ) -> Result<Vec<OpenedPanel>, PanelError> {
        validate_session_id(session_id)?;
        let state = self.state.lock().expect("workspace hooks poisoned");
        let Some(session) = state.sessions.get(session_id) else {
            return Ok(Vec::new());
        };
        Ok(session
            .opened
            .iter()
            .filter(|opened| zone.is_none_or(|required| opened.zone == required))
            .cloned()
            .collect())
    }
}

fn validate_session_id(session_id: &str) -> Result<(), PanelError> {
    if session_id.is_empty() || session_id.len() > 128 {
        return Err(PanelError::Invalid(
            "session id must be 1..=128 bytes".into(),
        ));
    }
    if !session_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        return Err(PanelError::Invalid(
            "session id has unsupported characters".into(),
        ));
    }
    Ok(())
}

fn validate_path(value: &str) -> Result<String, PanelError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(PanelError::Invalid("path is empty".into()));
    }
    if trimmed.len() > 4096 {
        return Err(PanelError::Invalid("path exceeds 4096 bytes".into()));
    }
    Ok(trimmed.to_owned())
}
