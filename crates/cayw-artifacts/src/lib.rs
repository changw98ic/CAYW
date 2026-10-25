//! HTML artifact display and host action protocol (F09 / F19 first cut).
//!
//! Provides a Session-owned store for generic HTML cards, charts, and
//! standalone preview bodies, plus a unified host action protocol that turns
//! card / form / chart interactions into structured intents (follow-up task,
//! tool invoke, state update, or read-only ack). Cross-Session actions are
//! rejected. Snapshots support in-process restart recovery tests.
//!
//! This crate does **not** render UI, call an LLM, grant Host capabilities, or
//! start Runtime tasks by itself — consumers (WebUI / TUI / Runtime) interpret
//! accepted intents through existing public SDK / IPC paths.
//!
//! First cut (F09 + F19 / SLICE J+K):
//! - put / update / preview / list artifacts by Session
//! - content_version + optional F01 `result_handle` reference
//! - action dispatch with Session ownership and version pins
//! - action + UI-state snapshot restore
//!
//! Out of scope: WebUI/TUI wiring, real Runtime task launch, sandboxing HTML,
//! equating Trace / visual-editor pages with generic artifact display.
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
mod identity;
mod store;
mod types;

pub use actions::{ActionProtocol, ActionSnapshot};
pub use identity::{validate_body, validate_id, validate_title};
pub use store::{ArtifactSnapshot, ArtifactStore, SharedArtifactStore};
pub use types::*;

/// Wire / document protocol for this first-cut artifact + action layer.
pub const ARTIFACT_PROTOCOL_VERSION: u32 = 1;

use std::sync::Arc;

/// Convenience bundle: one store + one action protocol sharing it.
#[derive(Debug, Clone)]
pub struct ArtifactWorkspace {
    pub store: Arc<ArtifactStore>,
    pub actions: Arc<ActionProtocol>,
}

impl ArtifactWorkspace {
    pub fn new() -> Self {
        let store = Arc::new(ArtifactStore::new());
        let actions = Arc::new(ActionProtocol::new(Arc::clone(&store)));
        Self { store, actions }
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
}

impl Default for ArtifactWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

/// Combined snapshot used by offline recovery smoke tests.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBundle {
    pub artifacts: ArtifactSnapshot,
    pub actions: ActionSnapshot,
}
