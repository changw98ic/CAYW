//! Explicit host-owned local sources for the injected Bridge boundary.
//!
//! The application prepares a source, registers its handle once and gives only
//! a Bridge consumer to Runtime. Control handles expose bounded observations and
//! shutdown, never a second execution route. No source is created by Core.

pub mod search;
pub mod skills;
mod stdio;

pub use stdio::{MCP_PROTOCOL, ServerInfo, StdioConfig, StdioControl, StdioSnapshot, StdioSource};
