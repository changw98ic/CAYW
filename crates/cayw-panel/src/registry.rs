//! Host-owned registry shared by official modules and third parties.

use crate::types::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

enum Entry {
    Panel(PanelDescriptor),
    FileViewer(FileViewerDescriptor),
}

struct Stored {
    entry: Entry,
    generation: u64,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, Stored>,
    next_generation: u64,
}

/// Independent contribution catalog. No registry lock is held across caller code.
pub struct PanelRegistry {
    state: Mutex<State>,
    registry_id: String,
}

impl Default for PanelRegistry {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            registry_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// RAII unregister handle. Dropping removes the contribution if this generation
/// is still live. Explicit [`PanelHandle::unregister`] is also supported.
pub struct PanelHandle {
    registry: Arc<PanelRegistry>,
    id: String,
    generation: u64,
    active: bool,
}

impl std::fmt::Debug for PanelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelHandle")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .field("active", &self.active)
            .field("registry_id", &self.registry.registry_id())
            .finish()
    }
}

impl PanelHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn description(&self) -> Result<PanelDescription, PanelError> {
        self.registry.describe(&self.id)
    }

    pub fn unregister(mut self) -> Result<PanelDescription, PanelError> {
        let description = self
            .registry
            .unregister_generation(&self.id, self.generation)?;
        self.active = false;
        Ok(description)
    }
}

impl Drop for PanelHandle {
    fn drop(&mut self) {
        if self.active {
            let _ = self
                .registry
                .unregister_generation(&self.id, self.generation);
            self.active = false;
        }
    }
}

impl PanelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn registry_id(&self) -> &str {
        &self.registry_id
    }

    pub fn register_panel(
        self: &Arc<Self>,
        descriptor: PanelDescriptor,
    ) -> Result<PanelHandle, PanelError> {
        let descriptor = descriptor.validate()?;
        self.insert(descriptor.id.clone(), Entry::Panel(descriptor))
    }

    pub fn register_file_viewer(
        self: &Arc<Self>,
        descriptor: FileViewerDescriptor,
    ) -> Result<PanelHandle, PanelError> {
        let descriptor = descriptor.validate()?;
        self.insert(descriptor.id.clone(), Entry::FileViewer(descriptor))
    }

    fn insert(self: &Arc<Self>, id: String, entry: Entry) -> Result<PanelHandle, PanelError> {
        let mut state = self.state.lock().expect("panel registry poisoned");
        if state.entries.contains_key(&id) {
            return Err(PanelError::Conflict(id));
        }
        state.next_generation = state
            .next_generation
            .checked_add(1)
            .ok_or_else(|| PanelError::Unavailable("generation exhausted".into()))?;
        let generation = state.next_generation;
        state
            .entries
            .insert(id.clone(), Stored { entry, generation });
        Ok(PanelHandle {
            registry: Arc::clone(self),
            id,
            generation,
            active: true,
        })
    }

    pub fn describe(&self, id: &str) -> Result<PanelDescription, PanelError> {
        let state = self.state.lock().expect("panel registry poisoned");
        let stored = state
            .entries
            .get(id)
            .ok_or_else(|| PanelError::NotFound(id.into()))?;
        Ok(description(&self.registry_id, stored))
    }

    pub fn list(&self) -> Vec<PanelDescription> {
        let state = self.state.lock().expect("panel registry poisoned");
        state
            .entries
            .values()
            .map(|stored| description(&self.registry_id, stored))
            .collect()
    }

    pub fn list_panels(&self, zone: Option<PanelZone>) -> Vec<PanelDescriptor> {
        let state = self.state.lock().expect("panel registry poisoned");
        let mut panels: Vec<PanelDescriptor> = state
            .entries
            .values()
            .filter_map(|stored| match &stored.entry {
                Entry::Panel(panel) => match zone {
                    Some(required) if !panel.placement.allows(required) => None,
                    _ => Some(panel.clone()),
                },
                Entry::FileViewer(_) => None,
            })
            .collect();
        panels.sort_by(|left, right| {
            left.order
                .cmp(&right.order)
                .then_with(|| left.id.cmp(&right.id))
        });
        panels
    }

    pub fn guide_entries(&self, zone: PanelZone) -> Vec<PanelDescriptor> {
        self.list_panels(Some(zone))
            .into_iter()
            .filter(|panel| !panel.hidden)
            .filter(|panel| match zone {
                PanelZone::Sidebar => panel.placement != PanelPlacement::BottomOnly,
                PanelZone::Bottom => true,
            })
            .collect()
    }

    /// Resolve the winning file viewer for a path or bare extension.
    pub fn resolve_file_viewer(&self, path_or_extension: &str) -> Option<FileViewerDescriptor> {
        let extension = extension_of(path_or_extension)?;
        let state = self.state.lock().expect("panel registry poisoned");
        let mut candidates: Vec<&FileViewerDescriptor> = state
            .entries
            .values()
            .filter_map(|stored| match &stored.entry {
                Entry::FileViewer(viewer)
                    if viewer.extensions.iter().any(|item| item == &extension) =>
                {
                    Some(viewer)
                }
                _ => None,
            })
            .collect();
        candidates.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.id.cmp(&right.id))
        });
        candidates.first().cloned().cloned()
    }

    pub fn unload(&self, id: &str) -> Result<PanelDescription, PanelError> {
        let mut state = self.state.lock().expect("panel registry poisoned");
        let stored = state
            .entries
            .remove(id)
            .ok_or_else(|| PanelError::NotFound(id.into()))?;
        Ok(description(&self.registry_id, &stored))
    }

    fn unregister_generation(
        &self,
        id: &str,
        generation: u64,
    ) -> Result<PanelDescription, PanelError> {
        let mut state = self.state.lock().expect("panel registry poisoned");
        match state.entries.get(id) {
            Some(stored) if stored.generation == generation => {
                let stored = state.entries.remove(id).expect("just checked");
                Ok(description(&self.registry_id, &stored))
            }
            Some(_) => Err(PanelError::Unloaded),
            None => Err(PanelError::NotFound(id.into())),
        }
    }

    pub(crate) fn require_panel(
        &self,
        id: &str,
        zone: PanelZone,
    ) -> Result<PanelDescriptor, PanelError> {
        let state = self.state.lock().expect("panel registry poisoned");
        let stored = state
            .entries
            .get(id)
            .ok_or_else(|| PanelError::NotFound(id.into()))?;
        match &stored.entry {
            Entry::Panel(panel) => {
                if !panel.placement.allows(zone) {
                    return Err(PanelError::ZoneNotAllowed(id.into()));
                }
                Ok(panel.clone())
            }
            Entry::FileViewer(_) => Err(PanelError::Invalid(format!(
                "{id} is a file viewer, not a panel"
            ))),
        }
    }
}

fn description(registry_id: &str, stored: &Stored) -> PanelDescription {
    match &stored.entry {
        Entry::Panel(panel) => PanelDescription {
            registry_id: registry_id.to_owned(),
            generation: stored.generation,
            kind: ContributionKind::Panel,
            panel: Some(panel.clone()),
            file_viewer: None,
        },
        Entry::FileViewer(viewer) => PanelDescription {
            registry_id: registry_id.to_owned(),
            generation: stored.generation,
            kind: ContributionKind::FileViewer,
            panel: None,
            file_viewer: Some(viewer.clone()),
        },
    }
}

fn extension_of(path_or_extension: &str) -> Option<String> {
    let trimmed = path_or_extension.trim();
    if trimmed.is_empty() {
        return None;
    }
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let (_, extension) = if name.contains('.') {
        name.rsplit_once('.')?
    } else {
        ("", name)
    };
    crate::identity::validate_extension(extension).ok()
}
