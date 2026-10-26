//! Explicit host-owned local sources for the injected Bridge boundary.
//!
//! The application prepares a source, registers its handle once and gives only
//! a Bridge consumer to Runtime. Control handles expose bounded observations and
//! shutdown, never a second execution route. No source is created by Core.
//!
//! [`lifecycle`] adds lazy start / idle recycle on top of [`StdioSource`].
//! [`config`] imports common external `mcpServers` JSON shapes into host specs.
//! [`install`] is the product entry that finishes import → prepare → Registry.

pub mod config;
pub mod install;
pub mod lifecycle;
pub mod search;
pub mod skills;
mod stdio;

pub use config::{
    ImportReport, ImportedServer, SkippedServer, import_mcp_servers_json, import_mcp_servers_value,
};
pub use install::{InstallReport, InstalledServer, install_imported, install_mcp_servers_file};
pub use lifecycle::{LifecycleMode, ManagedControl, ManagedSnapshot, ManagedStdio, ServerSpec};
pub use stdio::{MCP_PROTOCOL, ServerInfo, StdioConfig, StdioControl, StdioSnapshot, StdioSource};
