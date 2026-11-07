//! F16 / SLICE E: hang Trace nodes onto engineering file / symbol / content versions.
//!
//! Uses the shared [`cayw_codeintel::SymbolVersionBridge`]. Does **not** invent
//! architecture graphs for undeclared programs; only resolves nodes whose
//! `implementation` names a path published into the bridge (either by a host
//! snapshot or by the workspace producer wired on default Trace startup).

use crate::types::{ErrorEvidenceLink, GraphPlan, Node, NodeSymbolHang, Status, Step};
use cayw_codeintel::{
    CodeIntelError, DefinitionQuery, DiagnosticInput, DiagnosticSeverity, SymbolVersionBridge,
    content_sha256, language_for_path, publish_under_workspace,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

/// Linker that attaches versioned file/symbol anchors to declared Trace nodes.
#[derive(Clone)]
pub struct SymbolGraphLinker {
    bridge: Arc<SymbolVersionBridge>,
}

impl SymbolGraphLinker {
    pub fn new(bridge: Arc<SymbolVersionBridge>) -> Self {
        Self { bridge }
    }

    pub fn bridge(&self) -> &SymbolVersionBridge {
        &self.bridge
    }

    pub fn shared_bridge(&self) -> Arc<SymbolVersionBridge> {
        Arc::clone(&self.bridge)
    }

    /// Resolve hangs for every node whose `implementation` points at an indexed path.
    ///
    /// Missing paths / symbols are skipped (no hang), never invented. Returns a
    /// map keyed by `node.id`.
    pub fn hang_plan(&self, plan: &GraphPlan) -> Result<BTreeMap<String, NodeSymbolHang>, String> {
        let mut out = BTreeMap::new();
        for node in &plan.nodes {
            if let Some(hang) = self.hang_node(node)? {
                out.insert(node.id.clone(), hang);
            }
        }
        Ok(out)
    }

    /// Resolve a single node's engineering hang, if any.
    pub fn hang_node(&self, node: &Node) -> Result<Option<NodeSymbolHang>, String> {
        let Some((path, symbol_hint)) = parse_implementation(&node.implementation) else {
            return Ok(None);
        };
        let file = match self.bridge.file_version(&path) {
            Ok(Some(file)) => file,
            Ok(None) => return Ok(None),
            Err(err) => return Err(map_codeintel(err)),
        };

        let (symbol, location) = if let Some(hint) = symbol_hint.as_deref() {
            let hits = self
                .bridge
                .definitions(DefinitionQuery {
                    name: hint.into(),
                    path: Some(path.clone()),
                    kind: None,
                    file_version: Some(file.version),
                })
                .map_err(map_codeintel)?;
            if let Some(hit) = hits.into_iter().next() {
                (Some(hit.id), Some(hit.location))
            } else {
                // File known but symbol name not indexed: still hang the file version.
                (None, None)
            }
        } else {
            (None, None)
        };

        Ok(Some(NodeSymbolHang {
            node_id: node.id.clone(),
            file,
            symbol,
            location,
        }))
    }

    /// Build error summaries for failed steps that deep-link to hang + evidence cursor.
    ///
    /// When a hang has a location, also records a diagnostic on the shared bridge
    /// so Agent / editor / Trace share the same evidence id.
    pub fn error_evidence(
        &self,
        hangs: &BTreeMap<String, NodeSymbolHang>,
        steps: &[Step],
    ) -> Result<Vec<ErrorEvidenceLink>, String> {
        let mut links = Vec::new();
        for step in steps {
            if step.status != Status::Failed {
                continue;
            }
            let message = step
                .error
                .clone()
                .unwrap_or_else(|| "节点失败（无详细错误）".into());
            let hang = hangs.get(&step.node_id).cloned();
            let mut diagnostic_id = None;
            if let Some(ref hang) = hang
                && let Some(ref location) = hang.location
            {
                let reported = self
                    .bridge
                    .report_diagnostic(DiagnosticInput {
                        path: hang.file.path.clone(),
                        severity: DiagnosticSeverity::Error,
                        message: message.clone(),
                        byte_start: location.byte_start,
                        byte_end: location.byte_end,
                        file_version: Some(hang.file.version),
                        content_sha256: Some(hang.file.content_sha256.clone()),
                        code: Some(format!("trace.{}", step.node_id)),
                        symbol_name: hang.symbol.as_ref().map(|s| s.name.clone()),
                    })
                    .map_err(map_codeintel)?;
                diagnostic_id = Some(reported.id);
            }
            links.push(ErrorEvidenceLink {
                node_id: step.node_id.clone(),
                message,
                hang,
                evidence_cursor: step.evidence_cursor,
                diagnostic_id,
            });
        }
        Ok(links)
    }
}

/// Publish plan `implementation` paths from an open workspace into the bridge.
///
/// Used on the default Trace product path so ingest/mount does not depend on a
/// pre-filled empty bridge. Missing files are skipped (no hang invented).
///
/// Already-indexed paths are re-published when on-disk content digest differs
/// from the bridge entry (mtime alone is not trusted). Unchanged digests are
/// left alone so a host/manual outline is not overwritten by the lightweight
/// producer on every ingest/revise/refresh.
/// Returns how many paths were newly published or refreshed.
pub fn feed_plan_from_workspace(
    bridge: &SymbolVersionBridge,
    plan: &GraphPlan,
    workspace: &Path,
) -> Result<usize, String> {
    let mut seen = BTreeSet::new();
    let mut published = 0usize;
    for node in &plan.nodes {
        let Some((path, _)) = parse_implementation(&node.implementation) else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        let abs = workspace.join(&path);
        if !abs.is_file() {
            continue;
        }
        match bridge.file_version(&path) {
            Ok(Some(existing)) => {
                // Digest check: refresh only when workspace bytes moved on.
                match std::fs::read(&abs) {
                    Ok(bytes) if content_sha256(&bytes) == existing.content_sha256 => continue,
                    Ok(_) => {}
                    Err(_) => continue, // unreadable: keep prior index
                }
            }
            Ok(None) => {}
            Err(err) => return Err(map_codeintel(err)),
        }
        let language = language_for_path(&path);
        match publish_under_workspace(bridge, workspace, &path, language) {
            Ok(_) => published += 1,
            Err(err) => {
                // Unreadable / non-UTF-8 Rust: skip; hang stays absent / prior.
                let _ = err;
            }
        }
    }
    Ok(published)
}

/// Parse `implementation` into `(path, optional symbol name)`.
///
/// Accepted forms (engineering paths only):
/// - `examples/demo.rs::condition`
/// - `src/lib.rs:hello`
/// - `crates/foo/src/bar.rs#Baz`
/// - `src/lib.rs` (file only)
///
/// Non-path tokens such as `luau`, `local`, or bare `test:label` are ignored.
pub fn parse_implementation(implementation: &str) -> Option<(String, Option<String>)> {
    let raw = implementation.trim();
    if raw.is_empty() {
        return None;
    }

    let (path_part, symbol) = if let Some((path, sym)) = raw.split_once("::") {
        (path, Some(sym.trim()))
    } else if let Some((path, sym)) = raw.split_once('#') {
        (path, Some(sym.trim()))
    } else if looks_like_path(raw) {
        // Prefer last ':' only when left side looks like a path (has / or extension).
        if let Some((path, sym)) = raw.rsplit_once(':')
            && looks_like_path(path)
            && !sym.is_empty()
            && !sym.contains('/')
        {
            (path, Some(sym.trim()))
        } else {
            (raw, None)
        }
    } else {
        return None;
    };

    let path = path_part.trim();
    if !looks_like_path(path) {
        return None;
    }
    let symbol = symbol
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some((path.to_string(), symbol))
}

fn looks_like_path(value: &str) -> bool {
    if value.is_empty() || value.contains('\\') {
        return false;
    }
    value.contains('/')
        || value.ends_with(".rs")
        || value.ends_with(".luau")
        || value.ends_with(".lua")
        || value.ends_with(".ts")
        || value.ends_with(".tsx")
        || value.ends_with(".js")
        || value.ends_with(".jsx")
        || value.ends_with(".py")
}

fn map_codeintel(err: CodeIntelError) -> String {
    err.to_string()
}

#[cfg(test)]
mod parse_tests {
    use super::parse_implementation;

    #[test]
    fn parses_demo_style_double_colon() {
        assert_eq!(
            parse_implementation("examples/demo.rs::condition"),
            Some(("examples/demo.rs".into(), Some("condition".into())))
        );
    }

    #[test]
    fn parses_hash_and_file_only() {
        assert_eq!(
            parse_implementation("src/lib.rs#hello"),
            Some(("src/lib.rs".into(), Some("hello".into())))
        );
        assert_eq!(
            parse_implementation("src/lib.rs"),
            Some(("src/lib.rs".into(), None))
        );
    }

    #[test]
    fn ignores_non_path_implementations() {
        assert_eq!(parse_implementation("luau"), None);
        assert_eq!(parse_implementation("local"), None);
        assert_eq!(parse_implementation("test:shared-state"), None);
    }
}
