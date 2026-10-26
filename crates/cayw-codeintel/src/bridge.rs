//! In-memory symbol / file-version bridge.

use crate::identity::{normalize_path, validate_kind, validate_name, validate_sha256_hex};
use crate::types::{
    CodeIntelError, Diagnostic, DiagnosticSeverity, FileVersion, SourceLocation, SymbolHit,
    SymbolId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// SHA-256 hex digest of file bytes (same alphabet as Workspace `code.outline` sha256).
pub fn content_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

const DEFAULT_MAX_SYMBOLS_PER_FILE: usize = 1000;
const DEFAULT_MAX_DIAGNOSTICS: usize = 4096;

/// Outline-derived symbol snapshot fed by the host (e.g. from `code.outline`).
///
/// This bridge never parses source text itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolInput {
    pub name: String,
    pub kind: String,
    pub byte_start: u64,
    pub byte_end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_index: Option<u32>,
}

/// Definition lookup query shared by Agent, editor, and Trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionQuery {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// When set, only hits whose `file_version` equals this value are returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_version: Option<u64>,
}

/// Input for [`SymbolVersionBridge::report_diagnostic`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticInput {
    pub path: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub byte_start: u64,
    pub byte_end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
}

#[derive(Debug, Clone)]
struct FileEntry {
    version: FileVersion,
    symbols: Vec<SymbolHit>,
}

#[derive(Debug, Default)]
struct Inner {
    files: HashMap<String, FileEntry>,
    /// name (lowercase) → list of symbol wire ids currently indexed
    by_name: HashMap<String, Vec<SymbolId>>,
    diagnostics: Vec<Diagnostic>,
}

struct Shared {
    inner: Mutex<Inner>,
    diag_seq: AtomicU64,
    max_symbols_per_file: usize,
    max_diagnostics: usize,
}

/// Host-owned shared catalog. [`Clone`] is cheap and shares the same index so
/// Host tools, editor, and Trace can hold one instance.
#[derive(Clone)]
pub struct SymbolVersionBridge {
    shared: Arc<Shared>,
}

impl Default for SymbolVersionBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolVersionBridge {
    pub fn new() -> Self {
        Self {
            shared: std::sync::Arc::new(Shared {
                inner: Mutex::new(Inner::default()),
                diag_seq: AtomicU64::new(1),
                max_symbols_per_file: DEFAULT_MAX_SYMBOLS_PER_FILE,
                max_diagnostics: DEFAULT_MAX_DIAGNOSTICS,
            }),
        }
    }

    pub fn with_budgets(max_symbols_per_file: usize, max_diagnostics: usize) -> Self {
        Self {
            shared: std::sync::Arc::new(Shared {
                inner: Mutex::new(Inner::default()),
                diag_seq: AtomicU64::new(1),
                max_symbols_per_file: max_symbols_per_file.max(1),
                max_diagnostics: max_diagnostics.max(1),
            }),
        }
    }

    /// Publish (or refresh) a file's content digest and outline symbols.
    ///
    /// - New path or changed `content_sha256` → `version` bumps (starts at 1).
    /// - Same digest → version unchanged; symbols replaced in place so consumers
    ///   keep the same [`FileVersion`] identity.
    pub fn publish_file(
        &self,
        path: &str,
        content_sha256: &str,
        language: &str,
        symbols: Vec<SymbolInput>,
    ) -> Result<FileVersion, CodeIntelError> {
        let path = normalize_path(path).map_err(CodeIntelError::Invalid)?;
        let content_sha256 =
            validate_sha256_hex(content_sha256).map_err(CodeIntelError::Invalid)?;
        let language = validate_kind(language).map_err(CodeIntelError::Invalid)?;
        if symbols.len() > self.shared.max_symbols_per_file {
            return Err(CodeIntelError::BudgetExceeded(
                self.shared.max_symbols_per_file,
            ));
        }

        let mut prepared = Vec::with_capacity(symbols.len());
        for (index, input) in symbols.into_iter().enumerate() {
            let name = validate_name(&input.name).map_err(CodeIntelError::Invalid)?;
            let kind = validate_kind(&input.kind).map_err(CodeIntelError::Invalid)?;
            if input.byte_end < input.byte_start {
                return Err(CodeIntelError::Invalid(format!(
                    "symbol {name} has inverted byte range"
                )));
            }
            if let Some(parent) = input.parent_index
                && parent as usize >= index
            {
                return Err(CodeIntelError::Invalid(format!(
                    "symbol {name} parent_index must refer to an earlier symbol"
                )));
            }
            prepared.push((name, kind, input));
        }

        let mut guard = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let next_version = match guard.files.get(&path) {
            Some(existing) if existing.version.content_sha256 == content_sha256 => {
                existing.version.version
            }
            Some(existing) => existing
                .version
                .version
                .checked_add(1)
                .ok_or_else(|| CodeIntelError::Invalid("file version overflow".into()))?,
            None => 1,
        };

        let file = FileVersion {
            path: path.clone(),
            version: next_version,
            content_sha256: content_sha256.clone(),
            language,
        };

        // Drop prior name index entries for this path.
        Self::unindex_path(&mut guard, &path);

        let mut hits = Vec::with_capacity(prepared.len());
        for (name, kind, input) in prepared {
            let id = SymbolId {
                path: path.clone(),
                name: name.clone(),
                kind: kind.clone(),
                byte_start: input.byte_start,
                file_version: file.version,
                content_sha256: content_sha256.clone(),
            };
            let location = SourceLocation {
                path: path.clone(),
                file_version: file.version,
                content_sha256: content_sha256.clone(),
                byte_start: input.byte_start,
                byte_end: input.byte_end,
                start_line: input.start_line,
                end_line: input.end_line,
            };
            let hit = SymbolHit {
                id: id.clone(),
                location,
                file: file.clone(),
                parent_index: input.parent_index,
            };
            let key = name.to_ascii_lowercase();
            guard.by_name.entry(key).or_default().push(id);
            hits.push(hit);
        }

        guard.files.insert(
            path,
            FileEntry {
                version: file.clone(),
                symbols: hits,
            },
        );
        Ok(file)
    }

    pub fn file_version(&self, path: &str) -> Result<Option<FileVersion>, CodeIntelError> {
        let path = normalize_path(path).map_err(CodeIntelError::Invalid)?;
        let guard = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard.files.get(&path).map(|entry| entry.version.clone()))
    }

    pub fn symbols_for(&self, path: &str) -> Result<Vec<SymbolHit>, CodeIntelError> {
        let path = normalize_path(path).map_err(CodeIntelError::Invalid)?;
        let guard = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard
            .files
            .get(&path)
            .map(|entry| entry.symbols.clone())
            .unwrap_or_default())
    }

    /// Resolve a previously issued [`SymbolId`]. Fails when the file moved on
    /// or the digest no longer matches (stale evidence).
    pub fn resolve_symbol(&self, id: &SymbolId) -> Result<SymbolHit, CodeIntelError> {
        let path = normalize_path(&id.path).map_err(CodeIntelError::Invalid)?;
        let guard = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = guard
            .files
            .get(&path)
            .ok_or_else(|| CodeIntelError::FileNotFound(path.clone()))?;
        if entry.version.version != id.file_version {
            return Err(CodeIntelError::StaleVersion {
                path,
                expected: entry.version.version,
                got: id.file_version,
            });
        }
        if entry.version.content_sha256 != id.content_sha256 {
            return Err(CodeIntelError::DigestMismatch { path });
        }
        entry
            .symbols
            .iter()
            .find(|hit| hit.id == *id)
            .cloned()
            .ok_or_else(|| CodeIntelError::SymbolNotFound(id.wire()))
    }

    /// Same-index definitions. Does not speak LSP; looks up ingested symbols.
    pub fn definitions(&self, query: DefinitionQuery) -> Result<Vec<SymbolHit>, CodeIntelError> {
        let name = validate_name(&query.name).map_err(CodeIntelError::Invalid)?;
        let path = match query.path.as_deref() {
            Some(value) => Some(normalize_path(value).map_err(CodeIntelError::Invalid)?),
            None => None,
        };
        let kind = match query.kind.as_deref() {
            Some(value) => Some(validate_kind(value).map_err(CodeIntelError::Invalid)?),
            None => None,
        };
        let guard = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = name.to_ascii_lowercase();
        let Some(ids) = guard.by_name.get(&key) else {
            return Ok(Vec::new());
        };
        let mut hits = Vec::new();
        for id in ids {
            if let Some(ref want_path) = path
                && &id.path != want_path
            {
                continue;
            }
            if let Some(ref want_kind) = kind
                && &id.kind != want_kind
            {
                continue;
            }
            if let Some(want_version) = query.file_version
                && id.file_version != want_version
            {
                continue;
            }
            if let Some(entry) = guard.files.get(&id.path)
                && let Some(hit) = entry.symbols.iter().find(|hit| hit.id == *id)
            {
                hits.push(hit.clone());
            }
        }
        hits.sort_by(|a, b| {
            (&a.id.path, a.id.byte_start, &a.id.kind).cmp(&(
                &b.id.path,
                b.id.byte_start,
                &b.id.kind,
            ))
        });
        Ok(hits)
    }

    /// Record a diagnostic that deep-links to a versioned location.
    ///
    /// When `file_version` / digest are omitted, the current indexed file
    /// version is used. Explicit values must match the indexed file or the
    /// call fails with stale / digest errors (no silent remap).
    pub fn report_diagnostic(&self, input: DiagnosticInput) -> Result<Diagnostic, CodeIntelError> {
        let path = normalize_path(&input.path).map_err(CodeIntelError::Invalid)?;
        let message = input.message.trim();
        if message.is_empty() {
            return Err(CodeIntelError::Invalid(
                "diagnostic message is empty".into(),
            ));
        }
        if message.chars().count() > 1024 {
            return Err(CodeIntelError::Invalid(
                "diagnostic message exceeds 1024 characters".into(),
            ));
        }
        if input.byte_end < input.byte_start {
            return Err(CodeIntelError::Invalid(
                "diagnostic has inverted byte range".into(),
            ));
        }

        let mut guard = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.diagnostics.len() >= self.shared.max_diagnostics {
            return Err(CodeIntelError::BudgetExceeded(self.shared.max_diagnostics));
        }
        let entry = guard
            .files
            .get(&path)
            .ok_or_else(|| CodeIntelError::FileNotFound(path.clone()))?;
        let current = &entry.version;
        if let Some(want) = input.file_version
            && want != current.version
        {
            return Err(CodeIntelError::StaleVersion {
                path: path.clone(),
                expected: current.version,
                got: want,
            });
        }
        if let Some(digest) = input.content_sha256.as_deref() {
            let digest = validate_sha256_hex(digest).map_err(CodeIntelError::Invalid)?;
            if digest != current.content_sha256 {
                return Err(CodeIntelError::DigestMismatch { path: path.clone() });
            }
        }

        let symbol_id = match input.symbol_name.as_deref() {
            Some(name) => {
                let name = validate_name(name).map_err(CodeIntelError::Invalid)?;
                entry
                    .symbols
                    .iter()
                    .find(|hit| hit.id.name == name)
                    .map(|hit| hit.id.clone())
            }
            None => None,
        };

        let location = SourceLocation {
            path: path.clone(),
            file_version: current.version,
            content_sha256: current.content_sha256.clone(),
            byte_start: input.byte_start,
            byte_end: input.byte_end,
            start_line: None,
            end_line: None,
        };
        let id = format!(
            "diag-{}",
            self.shared.diag_seq.fetch_add(1, Ordering::Relaxed)
        );
        let diagnostic = Diagnostic {
            id,
            severity: input.severity,
            message: message.to_owned(),
            location,
            code: input
                .code
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            symbol_id,
        };
        guard.diagnostics.push(diagnostic.clone());
        Ok(diagnostic)
    }

    pub fn diagnostics(&self, path: Option<&str>) -> Result<Vec<Diagnostic>, CodeIntelError> {
        let path = match path {
            Some(value) => Some(normalize_path(value).map_err(CodeIntelError::Invalid)?),
            None => None,
        };
        let guard = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard
            .diagnostics
            .iter()
            .filter(|diag| match &path {
                Some(want) => &diag.location.path == want,
                None => true,
            })
            .cloned()
            .collect())
    }

    pub fn indexed_paths(&self) -> Vec<String> {
        let guard = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut paths: Vec<_> = guard.files.keys().cloned().collect();
        paths.sort();
        paths
    }

    fn unindex_path(guard: &mut Inner, path: &str) {
        if let Some(entry) = guard.files.get(path) {
            for hit in &entry.symbols {
                let key = hit.id.name.to_ascii_lowercase();
                if let Some(list) = guard.by_name.get_mut(&key) {
                    list.retain(|id| id.path != path);
                    if list.is_empty() {
                        guard.by_name.remove(&key);
                    }
                }
            }
        }
        // Drop diagnostics that pointed at any version of this path so callers
        // cannot keep stale error deep-links after a republish without re-checking.
        guard.diagnostics.retain(|diag| diag.location.path != path);
    }
}
