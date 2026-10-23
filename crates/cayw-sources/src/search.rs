//! Host-owned Workspace text search for the injected Bridge boundary.
//!
//! The host explicitly chooses one workspace root. `open` holds that root through
//! `cayw-workspace`'s fixed directory capability; later searches never reopen the
//! ambient path. The source exposes only the existing bounded `fs.grep` executor:
//! it creates no search index, shell, subprocess, or network connection.
//!
//! `search.query` is the only accepted bridge operation. The external arguments
//! are mapped to `fs.grep` with literal matching, bounded matches and two context
//! lines. Each hit keeps its file path, line numbers, matching text and context,
//! and the reply adds counts plus truncation/scan-complete status. This is a
//! bounded scan of the existing workspace, not a persistent semantic index.

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{Manifest, Operation, SourceHandle, SourceOperation, SourceReply, SourceRequest};
use cayw_codemode::CapabilityError;
use cayw_workspace::{Limits as WorkspaceLimits, Workspace};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use uuid::Uuid;

const MAX_QUERY_BYTES: usize = 8192;
const MAX_PATH_BYTES: usize = 4096;
const MAX_MATCHES: u64 = 1000;
const DEFAULT_MATCHES: u64 = 100;
const CONTEXT_LINES: usize = 2;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    path: Option<String>,
    limit: Option<u64>,
}

/// Cloned read-only observation handle for one search source.
#[derive(Clone)]
pub struct WorkspaceSearchStats {
    inner: Arc<StatsInner>,
}

/// Serializable point-in-time control observation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkspaceSearchSnapshot {
    pub instance_id: String,
    pub queries: u64,
    pub rejections: u64,
    pub closed: bool,
}

struct StatsInner {
    instance_id: String,
    queries: AtomicU64,
    rejections: AtomicU64,
    closed: AtomicBool,
}

impl WorkspaceSearchStats {
    pub fn snapshot(&self) -> WorkspaceSearchSnapshot {
        WorkspaceSearchSnapshot {
            instance_id: self.inner.instance_id.clone(),
            queries: self.inner.queries.load(Ordering::Acquire),
            rejections: self.inner.rejections.load(Ordering::Acquire),
            closed: self.inner.closed.load(Ordering::Acquire),
        }
    }

    fn query(&self) {
        let _ = self.inner.queries.fetch_add(1, Ordering::AcqRel);
    }

    fn reject(&self) {
        let _ = self.inner.rejections.fetch_add(1, Ordering::AcqRel);
    }

    fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
    }

    fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }
}

/// One fixed workspace root plus the immutable bridge manifest.
pub struct WorkspaceSearch {
    workspace: Workspace,
    manifest: Manifest,
    stats: WorkspaceSearchStats,
    ownership_key: String,
}

impl WorkspaceSearch {
    /// Opens the host-selected root once. The workspace capability stays fixed
    /// even if the directory's ambient name is later moved or replaced.
    pub fn open(root: impl AsRef<std::path::Path>) -> Result<Self, CapabilityError> {
        let workspace = Workspace::open(root, WorkspaceLimits::default())?;
        let instance_id = Uuid::new_v4().to_string();
        Ok(Self {
            workspace,
            manifest: Manifest {
                search: true,
                ..Default::default()
            },
            stats: WorkspaceSearchStats {
                inner: Arc::new(StatsInner {
                    instance_id: instance_id.clone(),
                    queries: AtomicU64::new(0),
                    rejections: AtomicU64::new(0),
                    closed: AtomicBool::new(false),
                }),
            },
            ownership_key: format!("workspace-search:{instance_id}"),
        })
    }

    /// The manifest declares exactly one capability: `search.query`.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn stats(&self) -> WorkspaceSearchStats {
        self.stats.clone()
    }

    /// Moves the fixed root into exactly one registry slot.
    pub fn into_handle(self) -> Box<dyn SourceHandle> {
        Box::new(self)
    }
}

#[async_trait]
impl SourceHandle for WorkspaceSearch {
    fn ownership_key(&self) -> &str {
        &self.ownership_key
    }

    async fn request(
        &self,
        request: SourceRequest,
        cancel: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        let SourceRequest {
            key,
            operation,
            max_result_bytes,
        } = request;
        let SourceOperation::Search { arguments } = operation else {
            self.stats.reject();
            return Err(failure(
                "workspace search source only supports search.query",
            ));
        };
        if self.stats.is_closed() {
            self.stats.reject();
            return Err(failure("workspace search source is closed"));
        }
        if cancel.is_cancelled() {
            self.stats.reject();
            return Err(failure("workspace search cancelled before dispatch"));
        }
        if key.operation != Operation::Search || key.target != "search.query" {
            self.stats.reject();
            return Err(failure("workspace search request identity mismatch"));
        }
        let args: SearchArgs = serde_json::from_value(arguments).map_err(|_| {
            self.stats.reject();
            failure("invalid search arguments")
        })?;
        if args.query.is_empty()
            || args.query.len() > MAX_QUERY_BYTES
            || args.path.as_ref().is_some_and(|path| {
                path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0')
            })
            || args
                .limit
                .is_some_and(|limit| limit == 0 || limit > MAX_MATCHES)
        {
            self.stats.reject();
            return Err(failure("invalid search bounds"));
        }

        let query = args.query;
        let path = args.path.unwrap_or_else(|| ".".into());
        let limit = args.limit.unwrap_or(DEFAULT_MATCHES);
        let grep_arguments = json!({
            "path": &path,
            "pattern": &query,
            "literal": true,
            "respect_gitignore": true,
            "max_matches": limit,
            "context_lines": CONTEXT_LINES,
        });

        // Count only work that actually reaches the bounded scanner. A denied
        // Host permission therefore leaves both the source and this counter idle.
        self.stats.query();
        let mut grep = self
            .workspace
            .execute("fs.grep", &grep_arguments, cancel.clone())
            .await
            .map_err(|error| {
                // `fs.grep` is read-only, so an executor failure is a known
                // failure even if the inner error was conservatively marked unknown.
                failure(format!("workspace search failed: {}", error.message))
            })?;
        if self.stats.is_closed() {
            self.stats.reject();
            return Err(failure("workspace search source closed during dispatch"));
        }
        if cancel.is_cancelled() {
            self.stats.reject();
            return Err(failure("workspace search cancelled during dispatch"));
        }

        let matches = grep["matches"].take();
        let match_count = matches.as_array().map_or(0, |items| items.len());
        let file_count = matches
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["path"].as_str())
                    .collect::<BTreeSet<_>>()
                    .len()
            })
            .unwrap_or(0);
        let value = json!({
            "query": query,
            "path": path,
            "limit": limit,
            "matches": matches,
            "summary": {
                "match_count": match_count,
                "file_count": file_count,
                "truncated": grep["truncated"],
                "scan_complete": grep["scan_complete"],
                "skipped": grep["skipped"],
                "protected_entries_omitted": grep["protected_entries_omitted"],
                "respect_gitignore": grep["respect_gitignore"],
                "scan": grep["scan"],
            },
        });

        // The registry later wraps this value as {request, source, value}. Check
        // that exact envelope here so an over-budget read-only search remains a
        // known failure instead of becoming an unknown effect after dispatch.
        let envelope = json!({"request": &key, "source": &key.source, "value": &value});
        let envelope_bytes = serde_json::to_vec(&envelope)
            .map_err(|_| failure("workspace search result could not be serialized"))?;
        if envelope_bytes.len() > max_result_bytes {
            self.stats.reject();
            return Err(failure(
                "workspace search result exceeds the source result budget",
            ));
        }

        Ok(SourceReply { key, value })
    }

    fn disconnect(&self) {
        self.stats.close();
    }
}

fn failure(message: impl Into<String>) -> CapabilityError {
    CapabilityError::new(message)
}
