//! Optional trace consumer. Replay has no Runtime or live Host execution handle.
pub mod capture;
pub mod engine;
pub mod server;
pub mod symbol_graph;
pub mod types;

pub use symbol_graph::{SymbolGraphLinker, feed_plan_from_workspace, parse_implementation};
pub use types::*;

use cayw_ai::CancellationToken;
use cayw_codeintel::SymbolVersionBridge;
use cayw_core::EventRecord;
use std::{
    env,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_STORE_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("本地追踪文件：{0}")]
    Io(#[from] std::io::Error),
    #[error("追踪数据格式：{0}")]
    Json(#[from] serde_json::Error),
}

pub struct TraceStore {
    path: PathBuf,
    document: Mutex<Document>,
    edits: tokio::sync::Mutex<()>,
    /// Shared F04 bridge used to hang declared nodes onto engineering versions.
    linker: SymbolGraphLinker,
    /// Open project / workspace root. When set, ingest/revise feed the bridge
    /// via the codeintel producer before hanging symbols (default product path),
    /// refreshing already-indexed paths when on-disk content digest changes.
    workspace_root: Option<PathBuf>,
    _lock: File,
}

impl TraceStore {
    /// Default product entry: empty bridge + workspace producer root.
    ///
    /// Workspace resolution order: `CAYW_WORKSPACE`, else process current dir.
    /// Ingest/revise then publish plan `implementation` paths from that root
    /// into the bridge before hanging (no manual `publish_file` required).
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>, Error> {
        Self::open_with_workspace(path, default_workspace_root())
    }

    /// Open with an explicit project/workspace root for the symbol producer.
    pub fn open_with_workspace(
        path: impl AsRef<Path>,
        workspace: impl AsRef<Path>,
    ) -> Result<Arc<Self>, Error> {
        Self::open_with_bridge_and_workspace(
            path,
            Arc::new(SymbolVersionBridge::new()),
            Some(workspace.as_ref().to_path_buf()),
        )
    }

    /// Open a store wired to a Host/Runtime-shared [`SymbolVersionBridge`].
    ///
    /// Does not attach a workspace producer (caller feeds the bridge). Prefer
    /// [`Self::open`] / [`Self::open_with_workspace`] for the default product path.
    pub fn open_with_bridge(
        path: impl AsRef<Path>,
        bridge: Arc<SymbolVersionBridge>,
    ) -> Result<Arc<Self>, Error> {
        Self::open_with_bridge_and_workspace(path, bridge, None)
    }

    /// Open with both a shared bridge and an optional workspace producer root.
    ///
    /// Ingest and revise populate [`Project::symbol_hangs`] through
    /// [`SymbolGraphLinker`] after feeding (or digest-refreshing) plan paths from
    /// `workspace_root`; call [`Self::refresh_symbol_hangs`] after an external
    /// bridge publish or workspace edit so HTTP state / UI see the bump.
    pub fn open_with_bridge_and_workspace(
        path: impl AsRef<Path>,
        bridge: Arc<SymbolVersionBridge>,
        workspace_root: Option<PathBuf>,
    ) -> Result<Arc<Self>, Error> {
        let path = path.as_ref().to_path_buf();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let lock_path = path.with_extension("lock");
        reject_symlink(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path)?;
        lock.try_lock()
            .map_err(|_| Error::Conflict("追踪文件正由另一个插件进程使用".into()))?;
        reject_symlink(&path)?;
        let document: Document = match File::open(&path) {
            Ok(file) => {
                if file.metadata()?.len() > MAX_STORE_BYTES as u64 {
                    return Err(Error::Invalid("追踪文件超过 32 MiB 上限".into()));
                }
                let mut bytes = Vec::new();
                file.take(MAX_STORE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                serde_json::from_slice(&bytes)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Document::default(),
            Err(error) => return Err(error.into()),
        };
        if document.schema != TRACE_SCHEMA {
            return Err(Error::Invalid("追踪文件版本不受支持".into()));
        }
        let store = Arc::new(Self {
            path,
            document: Mutex::new(document),
            edits: tokio::sync::Mutex::new(()),
            linker: SymbolGraphLinker::new(bridge),
            workspace_root,
            _lock: lock,
        });
        // A lost plugin process cannot resume a partially evaluated Lua stack.
        let mut recovered = store.snapshot();
        let mut changed = false;
        for project in recovered.projects.values_mut() {
            if project.version() > 1
                && project
                    .latest()
                    .is_none_or(|e| e.version != project.version())
            {
                project.evaluations.push(Evaluation {
                    id: uuid::Uuid::new_v4().to_string(),
                    version: project.version(),
                    mode: "replay".into(),
                    status: Status::Blocked,
                    steps: vec![],
                    output: None,
                    gaps: vec![Gap {
                        node_id: None,
                        message: "插件在本轮求值完成前停止；已保存内容，尚无该版本的计算结果"
                            .into(),
                    }],
                    elapsed_ms: 0.0,
                    saved_at_ms: now_ms(),
                    executed_nodes: Vec::new(),
                });
                changed = true;
            }
        }
        if changed {
            store.commit(recovered)?;
        }
        Ok(store)
    }

    pub fn snapshot(&self) -> Document {
        self.document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn bridge(&self) -> Arc<SymbolVersionBridge> {
        self.linker.shared_bridge()
    }

    pub fn linker(&self) -> &SymbolGraphLinker {
        &self.linker
    }

    /// Project root used by the default symbol producer, if any.
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    /// Re-resolve hangs for every project from the current bridge catalog.
    ///
    /// Persists and advances `sequence` only when at least one project's hangs
    /// change (for example after `publish_file` bumps a content version).
    pub fn refresh_symbol_hangs(&self) -> Result<(), Error> {
        let mut current = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = current.clone();
        let mut changed = false;
        for project in next.projects.values_mut() {
            let before = project.symbol_hangs.clone();
            link_project(project, &self.linker, self.workspace_root.as_deref())?;
            if project.symbol_hangs != before {
                changed = true;
            }
        }
        if changed {
            next.sequence = next
                .sequence
                .checked_add(1)
                .ok_or_else(|| Error::Invalid("追踪序号耗尽".into()))?;
            self.persist(&next)?;
            *current = next;
        }
        Ok(())
    }

    fn persist(&self, document: &Document) -> Result<(), Error> {
        let bytes = serde_json::to_vec(document)?;
        if bytes.len() > MAX_STORE_BYTES {
            return Err(Error::Invalid(
                "追踪文件达到 32 MiB 上限；请为新的采集选择另一文件".into(),
            ));
        }
        let temp = self
            .path
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<(), Error> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)?;
            File::open(
                self.path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?
            .sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    fn commit(&self, mut document: Document) -> Result<(), Error> {
        let mut current = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if document.sequence != current.sequence {
            return Err(Error::Conflict("追踪状态已变化".into()));
        }
        document.sequence = document
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Invalid("追踪序号耗尽".into()))?;
        self.persist(&document)?;
        *current = document;
        Ok(())
    }

    pub fn ingest(&self, events: &[EventRecord]) -> Result<(), Error> {
        if events.is_empty() {
            return Ok(());
        }
        let mut current = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = current.clone();
        let mut changed = false;
        for event in events {
            let after = next
                .sessions
                .entry(event.session_id.clone())
                .or_default()
                .after;
            if event.protocol_version != 3 {
                return Err(Error::Invalid("Runtime 事件版本不受支持".into()));
            }
            if event.cursor <= after {
                if !next.events.iter().any(|record| record == event) {
                    return Err(Error::Invalid("同一会话游标的原始事件不一致".into()));
                }
                continue;
            }
            if event.cursor != after + 1 {
                return Err(Error::Invalid(format!(
                    "会话 {} 的追踪记录缺失：期待 {}，收到 {}",
                    event.session_id,
                    after + 1,
                    event.cursor
                )));
            }
            if next.events.len() >= 100_000 {
                return Err(Error::Invalid(
                    "本文件达到十万条事件上限，请新建追踪文件".into(),
                ));
            }
            capture::apply(&mut next, event)?;
            next.events.push(event.clone());
            let session = next.sessions.entry(event.session_id.clone()).or_default();
            session.after = event.cursor;
            session.error = None;
            changed = true;
        }
        if changed {
            if next.projects.len() > 256 {
                return Err(Error::Invalid(
                    "本文件达到 256 次执行上限，请新建追踪文件".into(),
                ));
            }
            for project in next.projects.values_mut() {
                link_project(project, &self.linker, self.workspace_root.as_deref())?;
            }
            next.sequence += 1;
            self.persist(&next)?;
            *current = next;
        }
        Ok(())
    }

    pub fn connection_error(&self, session_id: &str, message: Option<String>) -> Result<(), Error> {
        let mut current = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current
            .sessions
            .get(session_id)
            .is_some_and(|s| s.error == message)
        {
            return Ok(());
        }
        let mut next = current.clone();
        next.sessions.entry(session_id.into()).or_default().error = message;
        next.sequence += 1;
        self.persist(&next)?;
        *current = next;
        Ok(())
    }

    pub async fn edit(
        &self,
        project_id: &str,
        edit: Edit,
        cancel: CancellationToken,
    ) -> Result<Document, Error> {
        let _editing = self.edits.lock().await;
        let mut document = self.snapshot();
        let project = document
            .projects
            .get_mut(project_id)
            .ok_or_else(|| Error::Invalid("找不到这次运行".into()))?;
        check_revision(project, edit.expected_version)?;
        let mut plan = project
            .revisions
            .last()
            .ok_or_else(|| Error::Invalid("未捕获可编辑节点计划".into()))?
            .plan
            .clone();
        let node = plan
            .nodes
            .iter_mut()
            .find(|node| node.id == edit.node_id)
            .ok_or_else(|| Error::Invalid("找不到该节点".into()))?;
        node.source = edit.source;
        node.params = edit.params;
        node.input_pointer = edit.input_pointer;
        self.revise(
            document,
            project_id,
            plan,
            Some(edit.node_id),
            "edit",
            cancel,
        )
        .await
    }

    pub async fn undo(
        &self,
        project_id: &str,
        undo: Undo,
        cancel: CancellationToken,
    ) -> Result<Document, Error> {
        let _editing = self.edits.lock().await;
        let document = self.snapshot();
        let project = document
            .projects
            .get(project_id)
            .ok_or_else(|| Error::Invalid("找不到这次运行".into()))?;
        check_revision(project, undo.expected_version)?;
        let plan = project
            .revisions
            .iter()
            .find(|r| r.version == undo.target_version)
            .ok_or_else(|| Error::Invalid("找不到目标内容版本".into()))?
            .plan
            .clone();
        self.revise(document, project_id, plan, None, "undo", cancel)
            .await
    }

    async fn revise(
        &self,
        mut document: Document,
        project_id: &str,
        plan: GraphPlan,
        changed_node: Option<String>,
        reason: &str,
        cancel: CancellationToken,
    ) -> Result<Document, Error> {
        // Validate before appending the revision. A rejected plan must not
        // advance the saved version or leave a partial evaluation behind, so
        // this check runs before any document mutation. The evaluation below
        // uses the internal no-revalidation path because this is the same
        // immutable plan.
        engine::validate_plan(&plan)?;
        let started = std::time::Instant::now();
        let project = document
            .projects
            .get_mut(project_id)
            .expect("validated project");
        let parent = project.version();
        let version = parent + 1;
        // Node saves use mode=recompute: reuse upstream steps without re-running
        // their bodies; the VM seeds at the edited node from the captured shared
        // seed (context + table identity). Undo uses full-entry replay.
        // The base proves the edited node was on a captured path; a missing node
        // is a gap, not permission to borrow an older branch's input.
        let base = if changed_node.is_some() {
            project
                .evaluations
                .iter()
                .rev()
                .find(|result| !result.steps.is_empty())
                .cloned()
        } else {
            project.latest().cloned()
        };
        let calls = project.calls.clone();
        let runtime_status = project.runtime_status.clone();
        let project_trace_error = project.trace_error.clone();
        project.revisions.push(Revision {
            version,
            parent: Some(parent),
            changed_node: changed_node.clone(),
            plan: plan.clone(),
            created_at_ms: now_ms(),
            reason: reason.into(),
        });
        link_project(project, &self.linker, self.workspace_root.as_deref())?;
        self.commit(document)?;
        let local_mode = if changed_node.is_some() {
            "recompute"
        } else {
            "replay"
        };
        let result =
            if runtime_status == Status::OutcomeUnknown || runtime_status == Status::Running {
                Evaluation {
                    id: uuid::Uuid::new_v4().to_string(),
                    version,
                    mode: local_mode.into(),
                    status: Status::Blocked,
                    steps: vec![],
                    output: None,
                    gaps: vec![Gap {
                        node_id: changed_node.clone(),
                        message: if runtime_status == Status::Running {
                            "原始执行仍在运行；输入和前置状态尚未完整捕获".into()
                        } else {
                            "原始执行存在未知效果，不能自动局部重算".into()
                        },
                    }],
                    elapsed_ms: 0.0,
                    saved_at_ms: now_ms(),
                    executed_nodes: Vec::new(),
                }
            } else if let Some(error) = &project_trace_error {
                Evaluation {
                    id: uuid::Uuid::new_v4().to_string(),
                    version,
                    mode: local_mode.into(),
                    status: Status::Blocked,
                    steps: vec![],
                    output: None,
                    gaps: vec![Gap {
                        node_id: changed_node.clone(),
                        message: format!("原始节点追踪不完整：{error}；不能复用其输入或状态"),
                    }],
                    elapsed_ms: 0.0,
                    saved_at_ms: now_ms(),
                    executed_nodes: Vec::new(),
                }
            } else {
                match engine::evaluate_validated(
                    &plan,
                    base.as_ref(),
                    changed_node.as_deref(),
                    &calls,
                    version,
                    cancel,
                )
                .await
                {
                    Ok(result) => result,
                    Err(error) => Evaluation {
                        id: uuid::Uuid::new_v4().to_string(),
                        version,
                        mode: local_mode.into(),
                        status: Status::Blocked,
                        steps: vec![],
                        output: None,
                        gaps: vec![Gap {
                            node_id: changed_node,
                            message: error.to_string(),
                        }],
                        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                        saved_at_ms: now_ms(),
                        executed_nodes: Vec::new(),
                    },
                }
            };
        // Ingest may have advanced while the VM ran; append onto its current state.
        let mut current = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = current.clone();
        let project = next
            .projects
            .get_mut(project_id)
            .ok_or_else(|| Error::Invalid("运行记录丢失".into()))?;
        if project.version() != version {
            return Err(Error::Conflict("内容版本已变化，旧结果未应用".into()));
        }
        project.evaluations.push(result);
        if next.metrics.save_to_result_ms.len() < 1024 {
            next.metrics
                .save_to_result_ms
                .push(started.elapsed().as_secs_f64() * 1000.0);
        }
        next.sequence += 1;
        self.persist(&next)?;
        *current = next;
        Ok(current.clone())
    }
}

/// Populate [`Project::symbol_hangs`] from the project's latest plan via the
/// shared linker. Empty when there is no plan yet or no indexed path matches.
///
/// When `workspace` is set, plan paths are published (and re-published on
/// content-digest change) through the codeintel workspace producer before hanging.
fn link_project(
    project: &mut Project,
    linker: &SymbolGraphLinker,
    workspace: Option<&Path>,
) -> Result<(), Error> {
    let Some(revision) = project.revisions.last() else {
        project.symbol_hangs.clear();
        return Ok(());
    };
    if let Some(root) = workspace {
        let _ = feed_plan_from_workspace(linker.bridge(), &revision.plan, root)
            .map_err(Error::Invalid)?;
    }
    project.symbol_hangs = linker.hang_plan(&revision.plan).map_err(Error::Invalid)?;
    Ok(())
}

fn default_workspace_root() -> PathBuf {
    env::var_os("CAYW_WORKSPACE")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn check_revision(project: &Project, expected: u64) -> Result<(), Error> {
    if expected != project.version() {
        return Err(Error::Conflict(format!(
            "当前内容版本为 {}，请重新载入后保存",
            project.version()
        )));
    }
    if project.revisions.len() >= MAX_HISTORY || project.evaluations.len() >= MAX_HISTORY {
        return Err(Error::Invalid(
            "本次运行达到 256 个版本上限，请使用新的追踪文件".into(),
        ));
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), Error> {
    match path.symlink_metadata() {
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(Error::Invalid("追踪文件和锁必须是普通文件".into()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
