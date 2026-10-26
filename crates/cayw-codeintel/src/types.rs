//! Serializable file versions, symbol ids, locations, and diagnostics.

use serde::{Deserialize, Serialize};

/// Monotonic file revision bound to a content digest.
///
/// `version` increments when `content_sha256` changes for the same path.
/// Equal digests keep the same version so Agent / editor / Trace share one id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileVersion {
    pub path: String,
    pub version: u64,
    pub content_sha256: String,
    pub language: String,
}

/// Stable symbol identity scoped to a concrete [`FileVersion`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolId {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub byte_start: u64,
    pub file_version: u64,
    pub content_sha256: String,
}

impl SymbolId {
    /// Compact wire form: `sv1:<path>|<kind>|<name>|<byte_start>|<file_version>|<sha256>`.
    pub fn wire(&self) -> String {
        format!(
            "sv1:{}|{}|{}|{}|{}|{}",
            self.path,
            self.kind,
            self.name,
            self.byte_start,
            self.file_version,
            self.content_sha256
        )
    }
}

/// Byte / line range inside a versioned file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub path: String,
    pub file_version: u64,
    pub content_sha256: String,
    pub byte_start: u64,
    pub byte_end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
}

/// One indexed symbol plus its location and owning file version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolHit {
    pub id: SymbolId,
    pub location: SourceLocation,
    pub file: FileVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_index: Option<u32>,
}

/// Diagnostic severity for the first-cut bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

/// A finding that always deep-links to a versioned source location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub id: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub location: SourceLocation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_id: Option<SymbolId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodeIntelError {
    #[error("invalid codeintel input: {0}")]
    Invalid(String),
    #[error("file not indexed: {0}")]
    FileNotFound(String),
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error("stale file version for {path}: expected {expected}, got {got}")]
    StaleVersion {
        path: String,
        expected: u64,
        got: u64,
    },
    #[error("content digest mismatch for {path}")]
    DigestMismatch { path: String },
    #[error("symbol budget exceeded (max {0})")]
    BudgetExceeded(usize),
}
