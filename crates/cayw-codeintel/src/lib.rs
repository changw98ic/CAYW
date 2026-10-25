//! Shared symbol / file-version bridge for CAYW Agent, editor, and Trace surfaces.
//!
//! First cut (F04 / SLICE C): host-owned in-memory catalog that
//! - tracks per-path file versions (monotonic + content SHA-256),
//! - binds symbol ids to those versions from **ingested** outline snapshots
//!   (does not reimplement `code.outline` / syn parsing),
//! - answers same-index **definitions** queries, and
//! - stores **diagnostics** that always carry a source location + file version
//!   so error paths can deep-link back to evidence.
//!
//! Cross-file impact cascades, dependency graphs, live LSP client pools, and
//! Host/Runtime tool wiring are out of scope for this slice.
//!
//! Design reference (not a code transplant): language-service + symbol/version
//! sharing shape described by [apmantza/pi-lens](https://github.com/apmantza/pi-lens).
//! See `docs/sources/f04-symbol-version-bridge.md`.
#![forbid(unsafe_code)]

mod bridge;
mod identity;
mod types;

pub use bridge::{
    DefinitionQuery, DiagnosticInput, SymbolInput, SymbolVersionBridge, content_sha256,
};
pub use identity::{normalize_path, validate_kind, validate_name, validate_sha256_hex};
pub use types::*;

/// Wire / document protocol for this first-cut bridge.
pub const CODEINTEL_PROTOCOL_VERSION: u32 = 1;
