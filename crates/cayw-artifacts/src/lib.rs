//! HTML artifact display and host action protocol (F09 / F19 first cut).
//!
//! Provides a Session-owned store for generic HTML cards, charts, and
//! standalone preview bodies, plus a unified host action protocol that turns
//! card / form / chart interactions into structured intents (follow-up task,
//! tool invoke, state update, or read-only ack). Cross-Session actions are
//! rejected. Snapshots support in-process and disk-backed restart recovery.
//!
//! `FollowUpTask` / `InvokeTool` call an injected [`HostActionDispatch`] bridge
//! so they actually hand off to Runtime/Host (or [`RecordingHostDispatch`] in
//! tests). This crate does **not** depend on `cayw-runtime` / `cayw-host`;
//! consumers implement the trait by forwarding to existing public SDK / Host
//! call paths. It does not grant new Host capabilities or sandbox HTML.
//!
//! First cut (F09 + F19 / SLICE J+K) + audit remediation:
//! - put / update / preview / list artifacts by Session
//! - `preview` returns [`ArtifactPreview`] (renderer projection), not the
//!   storage [`ArtifactRecord`]
//! - content_version + optional F01 `result_handle` reference
//! - action dispatch with Session ownership, version pins, and host bridge
//! - action + UI-state snapshot restore (memory + `{root}/artifacts/recovery.json`)
//!
//! Out of scope: WebUI/TUI chrome wiring, HTML sandboxing, equating Trace /
//! visual-editor pages with generic artifact display, ContentStore resolution
//! of `result_handle`.
//!
//! Design references (not code transplants):
//! - [Nagi-ovo/dsh-visualize](https://github.com/Nagi-ovo/dsh-visualize) (HTML
//!   cards / charts / preview shape)
//! - Plannotator feedback path (session-bound follow-up; CAYW owns the host
//!   action protocol because dsh-visualize buttons do not post to the main
//!   dialogue today)
//!
//! See `docs/sources/f09-f19-artifact-actions.md`.
#![forbid(unsafe_code)]

mod actions;
mod dispatch;
mod identity;
mod persist;
mod store;
mod types;

pub use actions::{ActionProtocol, ActionSnapshot};
pub use dispatch::{
    FollowUpDispatch, HostActionDispatch, HostDispatchResult, RecordedHostCall,
    RecordingHostDispatch, SharedHostDispatch, ToolDispatch,
};
pub use identity::{validate_body, validate_id, validate_title};
pub use persist::{
    ARTIFACTS_DIR_NAME, RECOVERY_FILE_NAME, load_recovery, recovery_path, save_recovery,
};
pub use store::{ArtifactSnapshot, ArtifactStore, SharedArtifactStore};
pub use types::*;

/// Wire / document protocol for this first-cut artifact + action layer.
pub const ARTIFACT_PROTOCOL_VERSION: u32 = 1;

use std::path::Path;
use std::sync::Arc;

/// Convenience bundle: one store + one action protocol sharing it.
#[derive(Clone)]
pub struct ArtifactWorkspace {
    pub store: Arc<ArtifactStore>,
    pub actions: Arc<ActionProtocol>,
    dispatch: SharedHostDispatch,
}

impl std::fmt::Debug for ArtifactWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactWorkspace")
            .field("store", &self.store)
            .field("actions", &self.actions)
            .finish_non_exhaustive()
    }
}

impl ArtifactWorkspace {
    /// In-memory workspace with a [`RecordingHostDispatch`] bridge (tests /
    /// offline). Production should use [`with_dispatch`] and wire Runtime/Host.
    pub fn new() -> Self {
        Self::with_dispatch(Arc::new(RecordingHostDispatch::new()))
    }

    /// Workspace that hands `FollowUpTask` / `InvokeTool` to `dispatch`.
    pub fn with_dispatch(dispatch: SharedHostDispatch) -> Self {
        let store = Arc::new(ArtifactStore::new());
        let actions = Arc::new(ActionProtocol::new(
            Arc::clone(&store),
            Arc::clone(&dispatch),
        ));
        Self {
            store,
            actions,
            dispatch,
        }
    }

    /// Shared host/runtime bridge used by the action protocol.
    pub fn host_dispatch(&self) -> &SharedHostDispatch {
        &self.dispatch
    }

    /// Export both artifact catalog and action log for recovery.
    pub fn export_recovery(&self) -> RecoveryBundle {
        RecoveryBundle {
            artifacts: self.store.export_snapshot(),
            actions: self.actions.export_snapshot(),
        }
    }

    /// Replace both maps from a recovery bundle (simulates restart).
    pub fn restore_recovery(&self, bundle: RecoveryBundle) -> Result<(), ArtifactError> {
        self.store.restore_snapshot(bundle.artifacts)?;
        self.actions.restore_snapshot(bundle.actions)?;
        Ok(())
    }

    /// Persist recovery under `{root}/artifacts/recovery.json`.
    pub fn save_to_disk(&self, root: &Path) -> Result<std::path::PathBuf, ArtifactError> {
        save_recovery(root, &self.export_recovery())
    }

    /// Load recovery from `{root}/artifacts/recovery.json` into a new workspace
    /// that keeps `dispatch` for subsequent actions.
    pub fn load_from_disk(
        root: &Path,
        dispatch: SharedHostDispatch,
    ) -> Result<Self, ArtifactError> {
        let bundle = load_recovery(root)?;
        let ws = Self::with_dispatch(dispatch);
        ws.restore_recovery(bundle)?;
        Ok(ws)
    }
}

impl Default for ArtifactWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

/// Combined snapshot used by offline recovery smoke tests and disk persist.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBundle {
    pub artifacts: ArtifactSnapshot,
    pub actions: ActionSnapshot,
}
