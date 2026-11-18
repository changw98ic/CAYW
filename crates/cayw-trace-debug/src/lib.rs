//! Optional trace consumer. Replay has no Runtime or live Host execution handle.
pub mod capture;
pub mod engine;
pub mod server;
mod storage;
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
    io::{BufReader, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("本地追踪文件：{0}")]
    Io(#[from] std::io::Error),
    #[error("追踪数据格式：{0}")]
    Json(serde_json::Error),
    #[error("Trace database: {0}")]
    Database(#[from] rusqlite::Error),
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        match error.io_error_kind() {
            Some(kind) => Self::Io(std::io::Error::new(kind, error)),
            None => Self::Json(error),
        }
    }
}

/// Preserve byte-exact evidence independently of the source file, including
/// when validation fails and a user later repairs the original in place.
fn archive_document(path: &Path, suffix: &str) -> Result<(), Error> {
    let mut archive = path.with_extension(suffix);
    reject_symlink(&archive)?;
    if archive.try_exists()? {
        let mut source = File::open(path)?;
        let mut evidence = File::open(&archive)?;
        let mut remaining = source.metadata()?.len();
        let mut same = remaining == evidence.metadata()?.len();
        let mut left = [0u8; 64 * 1024];
        let mut right = [0u8; 64 * 1024];
        while same && remaining > 0 {
            let count = remaining.min(left.len() as u64) as usize;
            source.read_exact(&mut left[..count])?;
            evidence.read_exact(&mut right[..count])?;
            same = left[..count] == right[..count];
            remaining -= count as u64;
        }
        if same {
            return Ok(());
        }
        // A repaired input is another piece of evidence, never an overwrite.
        archive = path.with_extension(format!("{}-{suffix}", uuid::Uuid::new_v4()));
    }

    let temp = path.with_extension(format!("{}.evidence.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), Error> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        std::io::copy(&mut File::open(path)?, &mut output)?;
        output.sync_all()?;
        // Link the completed copy atomically without replacing existing evidence.
        std::fs::hard_link(&temp, &archive)?;
        File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;
        Ok(())
    })();
    let cleanup = std::fs::remove_file(&temp);
    result?;
    cleanup?;
    Ok(())
}

fn read_document(path: &Path) -> Result<(Document, bool), Error> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Document::default(), false));
        }
        Err(error) => return Err(error.into()),
    };
    let mut raw: serde_json::Value = serde_json::from_reader(BufReader::new(file))?;
    let mut migrated = false;
    if let Some(events) = raw
        .get_mut("events")
        .and_then(serde_json::Value::as_array_mut)
    {
        if events.iter().any(|event| event["protocol_version"] == 3) {
            archive_document(path, "v3-evidence.json")?;
        }
        for event in events {
            if event["protocol_version"] == 3 {
                let canonical = cayw_core::migrate_v3_event_json(&serde_json::to_string(event)?)
                    .map_err(|error| Error::Invalid(error.to_string()))?;
                *event = serde_json::to_value(canonical)?;
                migrated = true;
            } else if event["protocol_version"] != cayw_core::PROTOCOL_VERSION {
                return Err(Error::Invalid("追踪事件版本不受支持".into()));
            }
        }
    }
    Ok((serde_json::from_value(raw)?, migrated))
}

fn projection_clone(document: &Document) -> Document {
    Document {
        schema: document.schema.clone(),
        sequence: document.sequence,
        sessions: document.sessions.clone(),
        projects: document.projects.clone(),
        events: Vec::new(),
        metrics: document.metrics.clone(),
    }
}

pub struct TraceStore {
    database: Mutex<rusqlite::Connection>,
    document: Mutex<Document>,
    project_changes: Mutex<std::collections::BTreeMap<String, (u64, Vec<u8>)>>,
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
        let (database, document, migrated) = storage::open(&path)?;
        if document.schema != TRACE_SCHEMA {
            return Err(Error::Invalid("追踪文件版本不受支持".into()));
        }
        let store = Arc::new(Self {
            database: Mutex::new(database),
            project_changes: Mutex::new(
                document
                    .projects
                    .iter()
                    .map(|(id, project)| {
                        Ok((
                            id.clone(),
                            (document.sequence, serde_json::to_vec(project)?),
                        ))
                    })
                    .collect::<Result<_, serde_json::Error>>()?,
            ),
            document: Mutex::new(document),
            edits: tokio::sync::Mutex::new(()),
            linker: SymbolGraphLinker::new(bridge),
            workspace_root,
            _lock: lock,
        });
        // A lost plugin process cannot resume a partially evaluated Lua stack.
        let mut recovered = store.projection();
        let mut changed = migrated;
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

    fn projection(&self) -> Document {
        self.document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Full export. Normal observation reads the projection or a single event.
    pub fn snapshot(&self) -> Result<Document, Error> {
        let current = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut document = projection_clone(&current);
        document.events = storage::events(
            &self
                .database
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )?;
        Ok(document)
    }

    /// Read progress without cloning raw event history.
    pub fn sequence(&self) -> u64 {
        self.document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sequence
    }

    pub fn session_cursor(&self, session: &str) -> u64 {
        self.document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sessions
            .get(session)
            .map_or(0, |capture| capture.after)
    }

    fn track_project_changes(&self, document: &Document) -> Result<(), Error> {
        let mut changes = self
            .project_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, project) in &document.projects {
            let bytes = serde_json::to_vec(project)?;
            if changes
                .get(id)
                .is_none_or(|(_, previous)| *previous != bytes)
            {
                changes.insert(id.clone(), (document.sequence, bytes));
            }
        }
        Ok(())
    }

    pub fn state_update(&self, after: Option<u64>) -> StateUpdate {
        let document = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changes = self
            .project_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reset = after.is_none_or(|after| after > document.sequence);
        let projects = document
            .projects
            .iter()
            .filter(|(id, _)| {
                reset
                    || changes
                        .get(*id)
                        .is_some_and(|(sequence, _)| *sequence > after.unwrap())
            })
            .map(|(id, project)| (id.clone(), project.clone()))
            .collect();
        StateUpdate {
            schema: "cayw.trace.state.v1".into(),
            sequence: document.sequence,
            reset,
            sessions: document.sessions.clone(),
            projects,
        }
    }

    pub fn event(&self, session: &str, cursor: u64) -> Result<Option<EventRecord>, Error> {
        storage::event(
            &self
                .database
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            session,
            cursor,
        )
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
        let mut next = projection_clone(&current);
        let mut changed = false;
        for project in next.projects.values_mut() {
            let before = (project.symbol_hangs.clone(), project.gaps.clone());
            link_project(project, &self.linker, self.workspace_root.as_deref());
            if (project.symbol_hangs.clone(), project.gaps.clone()) != before {
                changed = true;
            }
        }
        if changed {
            next.sequence = next
                .sequence
                .checked_add(1)
                .ok_or_else(|| Error::Invalid("追踪序号耗尽".into()))?;
            self.persist(&next, &[])?;
            self.track_project_changes(&next)?;
            *current = next;
        }
        Ok(())
    }

    fn persist(&self, document: &Document, events: &[EventRecord]) -> Result<(), Error> {
        storage::persist(
            &mut self
                .database
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            document,
            events,
        )
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
        self.persist(&document, &[])?;
        self.track_project_changes(&document)?;
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
        let mut next = Document {
            schema: current.schema.clone(),
            sequence: current.sequence,
            sessions: current.sessions.clone(),
            metrics: current.metrics.clone(),
            projects: Default::default(),
            events: Vec::new(),
        };
        let mut changed = false;
        for event in events {
            let after = next
                .sessions
                .entry(event.session_id.clone())
                .or_default()
                .after;
            if event.protocol_version != cayw_core::PROTOCOL_VERSION {
                return Err(Error::Invalid("Runtime 事件版本不受支持".into()));
            }
            if event.cursor <= after {
                if !next.events.iter().any(|record| record == event)
                    && self.event(&event.session_id, event.cursor)?.as_ref() != Some(event)
                {
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
            capture::load_affected_projects(&current, &mut next, event);
            capture::apply(&mut next, event)?;
            next.events.push(event.clone());
            let session = next.sessions.entry(event.session_id.clone()).or_default();
            session.after = event.cursor;
            session.error = None;
            changed = true;
        }
        if changed {
            for project in next.projects.values_mut() {
                link_project(project, &self.linker, self.workspace_root.as_deref());
            }
            next.sequence += 1;
            self.persist(&next, &next.events)?;
            self.track_project_changes(&next)?;
            current.projects.append(&mut next.projects);
            current.sessions = next.sessions;
            current.metrics = next.metrics;
            current.sequence = next.sequence;
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
        let mut next = Document {
            schema: current.schema.clone(),
            sequence: current.sequence,
            sessions: current.sessions.clone(),
            metrics: current.metrics.clone(),
            projects: Default::default(),
            events: Vec::new(),
        };
        next.sessions.entry(session_id.into()).or_default().error = message;
        next.sequence += 1;
        self.persist(&next, &[])?;
        current.sessions = next.sessions;
        current.sequence = next.sequence;
        Ok(())
    }

    pub async fn edit(
        &self,
        project_id: &str,
        edit: Edit,
        cancel: CancellationToken,
    ) -> Result<Document, Error> {
        let _editing = self.edits.lock().await;
        let mut document = self.projection();
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
        let document = self.projection();
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
        let revision = Revision {
            version,
            parent: Some(parent),
            changed_node: changed_node.clone(),
            plan: plan.clone(),
            created_at_ms: now_ms(),
            reason: reason.into(),
        };
        self.change_project(project_id, parent, |project, _| {
            project.revisions.push(revision);
            link_project(project, &self.linker, self.workspace_root.as_deref());
        })?;
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
        self.change_project(project_id, version, |project, metrics| {
            project.evaluations.push(result);
            if metrics.save_to_result_ms.len() < 1024 {
                metrics
                    .save_to_result_ms
                    .push(started.elapsed().as_secs_f64() * 1000.0);
            }
        })?;
        self.snapshot()
    }

    fn change_project(
        &self,
        project_id: &str,
        expected_version: u64,
        change: impl FnOnce(&mut Project, &mut Metrics),
    ) -> Result<(), Error> {
        let mut current = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut project = current
            .projects
            .get(project_id)
            .ok_or_else(|| Error::Invalid("找不到这次运行".into()))?
            .clone();
        check_revision(&project, expected_version)?;
        let mut metrics = current.metrics.clone();
        change(&mut project, &mut metrics);
        let mut next = Document {
            schema: current.schema.clone(),
            sequence: current.sequence + 1,
            sessions: current.sessions.clone(),
            projects: [(project_id.to_owned(), project)].into(),
            events: Vec::new(),
            metrics,
        };
        self.persist(&next, &[])?;
        self.track_project_changes(&next)?;
        current.projects.append(&mut next.projects);
        current.sequence = next.sequence;
        current.metrics = next.metrics;
        Ok(())
    }
}

/// Populate [`Project::symbol_hangs`] from the project's latest plan via the
/// shared linker. Empty when there is no plan yet or no indexed path matches.
///
/// When `workspace` is set, plan paths are published (and re-published on
/// content-digest change) through the codeintel workspace producer before hanging.
fn link_project(project: &mut Project, linker: &SymbolGraphLinker, workspace: Option<&Path>) {
    const SOURCE_GAP: &str = "源码挂载暂不可用：";
    project
        .gaps
        .retain(|gap| !gap.message.starts_with(SOURCE_GAP));
    let Some(revision) = project.revisions.last() else {
        project.symbol_hangs.clear();
        return;
    };
    let linked = (|| {
        if let Some(root) = workspace {
            feed_plan_from_workspace(linker.bridge(), &revision.plan, root)?;
        }
        linker.hang_plan(&revision.plan)
    })();
    match linked {
        Ok(hangs) => project.symbol_hangs = hangs,
        Err(error) => {
            project.symbol_hangs.clear();
            project.gaps.push(Gap {
                node_id: None,
                message: format!("{SOURCE_GAP}{error}"),
            });
        }
    }
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
