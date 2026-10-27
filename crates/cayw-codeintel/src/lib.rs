//! Shared symbol / file-version bridge for CAYW Agent, editor, and Trace surfaces.
//!
//! First cut (F04 / SLICE C): host-owned in-memory catalog that
//! - tracks per-path file versions (monotonic + content SHA-256),
//! - binds symbol ids from ingested outline snapshots **or** a local producer
//!   that reads workspace file bytes and extracts a lightweight Rust outline,
//! - answers same-index **definitions** queries, and
//! - stores **diagnostics** that always carry a source location + file version
//!   so error paths can deep-link back to evidence.
//!
//! [`SymbolVersionBridge`] is cheaply [`Clone`]d via `Arc` so Host tools, the
//! visual editor, and Trace share one index instance.
//!
//! Cross-file impact cascades, dependency graphs, and live LSP client pools
//! remain out of scope. Host/Runtime tool names and Trace query routes are the
//! product entry for this bridge.
//!
//! Design reference (not a code transplant): language-service + symbol/version
//! sharing shape described by [apmantza/pi-lens](https://github.com/apmantza/pi-lens).
//! See `docs/sources/f04-symbol-version-bridge.md`.
#![forbid(unsafe_code)]

mod bridge;
mod identity;
mod producer;
mod types;

pub use bridge::{
    DefinitionQuery, DiagnosticInput, SymbolInput, SymbolVersionBridge, content_sha256,
};
pub use identity::{normalize_path, validate_kind, validate_name, validate_sha256_hex};
pub use producer::{
    language_for_path, lightweight_rust_outline, publish_bytes, publish_outline_json, publish_path,
    publish_under_workspace, symbol_inputs_from_outline,
};
pub use types::*;

/// Wire / document protocol for this first-cut bridge.
pub const CODEINTEL_PROTOCOL_VERSION: u32 = 1;
