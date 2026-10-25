//! Shared panel / file-viewer extension API for CAYW workbench surfaces.
//!
//! Official modules and third-party contributors register through the same
//! [`PanelRegistry`] and [`WorkspaceHooks`] types. Registration is host-owned
//! catalog data only: it never grants Core private state, Host capabilities,
//! approvals, or new execution rights. UI consumers (TUI / WebUI) decide how
//! to render a contribution; user actions still travel the existing public
//! SDK / IPC and permission paths.
//!
//! First cut (F07 / SLICE H): sidebar and bottom workspace zones, panel tab
//! contributions, and file-viewer contributions with extension-based resolve.
//! Built-in file editing, Git, interactive terminals, background-task pages,
//! and sub-agent pages are out of scope for this slice; they can later register
//! through this same API.
//!
//! Design reference (not a code transplant): the registerTab /
//! registerFileViewer shape described by
//! [omdsh-dev/DSH-better-sidebar](https://github.com/omdsh-dev/DSH-better-sidebar).
//! See `docs/sources/f07-panel-extension.md` for the source record.
#![forbid(unsafe_code)]

mod identity;
mod registry;
mod types;
mod workspace;

pub use identity::{validate_contribution_id, validate_extension, validate_title};
pub use registry::{PanelHandle, PanelRegistry};
pub use types::*;
pub use workspace::{OpenPanelRequest, OpenedPanel, WorkspaceHooks};

/// Wire / document protocol for this first-cut catalog.
pub const PANEL_PROTOCOL_VERSION: u32 = 1;
