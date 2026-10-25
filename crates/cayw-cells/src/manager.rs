use crate::{bridge::Bridge, *};
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityHost, CapabilityStop, CodeMode, ExecutionReport, Limits};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::{AbortHandle, JoinSet},
};
use uuid::Uuid;

/// Owns every VM/task in one session/task. Runtime holds an Arc to this manager
/// for as long as it permits cells to run, including between model tool calls.
/// `close` cancels and joins. Drop cancels and aborts all tasks; no task is detached.
/// Prior-attempt records in this session/task may be read or reconciled, but
/// their missing VM cannot be resumed. A second live manager for this scope is
/// rejected. Runtime must share the journal and coordinator across its managers.
pub struct CellManager {
    identity: CellIdentity,
    journal: Arc<SqliteJournal>,
    backend: Arc<dyn CellBackend>,
    reads: Arc<ReadCoordinator>,
    limits: CellLimits,
    vm_limits: Limits,
    cells: Mutex<BTreeMap<CellId, Arc<Run>>>,
    tasks: Mutex<JoinSet<()>>,
    capacity: Arc<Semaphore>,
    cancel: CancellationToken,
    closed: AtomicBool,
    _scope: Arc<ScopeLease>,
}
impl CellManager {
    pub fn new(
        identity: CellIdentity,
        journal: Arc<SqliteJournal>,
        backend: Arc<dyn CellBackend>,
        reads: Arc<ReadCoordinator>,
        limits: CellLimits,
        vm_limits: Limits,
    ) -> Result<Arc<Self>, CellError> {
        identity.validate()?;
        limits.validate()?;
        let host_catalog = backend.capabilities();
        let host_capability_count = host_catalog
            .iter()
            .filter(|definition| CellValueOperation::from_name(&definition.name).is_none())
            .count();
        let catalog = crate::values::with_definitions(host_catalog);
        // Validate the exact VM catalog, including canonical value definitions,
        // before claiming ownership.
        CodeMode::new(Arc::new(BackendCatalog(catalog.clone())), vm_limits.clone())
            .map_err(|e| CellError::Invalid(e.to_string()))?;
        // The two canonical value capabilities are internal trusted helpers and
        // do not consume the host's 256-capability budget.
        if host_capability_count > 256 || vm_limits.host_calls > 4096 {
            return Err(CellError::Invalid(
                "catalog or host call budget too large".into(),
            ));
        }
        if catalog.iter().any(|d| d.name.starts_with("__cells_")) {
            return Err(CellError::Invalid(
                "host capability uses reserved __cells_ prefix".into(),
            ));
        }
        let execution_lock = journal.claim(&identity)?;
        let records = journal.records(&identity, &limits)?;
        let needs_recovery = records.iter().any(|r| {
            matches!(r.status, CellStatus::Running | CellStatus::Yielded) || r.unresolved()
        });
        let scope = Arc::new(ScopeLease {
            _execution_lock: execution_lock,
        });
        let cancel = CancellationToken::new();
        let cells = records
            .into_iter()
            .map(|record| {
                let id = record.cell_id.clone();
                (
                    id,
                    Run::new(
                        record,
                        backend.clone(),
                        journal.clone(),
                        reads.clone(),
                        RunLimits {
                            cell: limits.clone(),
                            shutdown_grace: vm_limits.shutdown_grace,
                        },
                        cancel.child_token(),
                        false,
                    ),
                )
            })
            .collect();
        if needs_recovery || journal.is_frozen(&identity.session_id)? {
            reads.freeze(&identity.session_id);
        }
        Ok(Arc::new(Self {
            identity,
            journal,
            backend,
            reads,
            capacity: Arc::new(Semaphore::new(limits.max_running)),
            limits,
            vm_limits,
            cells: Mutex::new(cells),
            tasks: Mutex::new(JoinSet::new()),
            cancel,
            closed: AtomicBool::new(false),
            _scope: scope,
        }))
    }
    pub fn identity(&self) -> &CellIdentity {
        &self.identity
    }
    pub fn limits(&self) -> &CellLimits {
        &self.limits
    }
    /// Scoped executor sharing this manager's journal and value limits. It does
    /// not authorize by itself; the caller must apply its host policy first.
    pub fn values(&self) -> CellValueExecutor {
        CellValueExecutor::new(self.journal.clone(), self.limits.clone())
    }
    pub fn shutdown_grace(&self) -> Duration {
        self.vm_limits.shutdown_grace
    }
    pub fn record(&self, cell_id: &str) -> Result<CellRecord, CellError> {
        Ok(self
            .get(cell_id)?
            .record
            .lock()
            .expect("cell lock poisoned")
            .clone())
    }
    fn get(&self, id: &str) -> Result<Arc<Run>, CellError> {
        self.cells
            .lock()
            .expect("manager lock poisoned")
            .get(id)
            .cloned()
            .ok_or(CellError::NotFound)
    }
    pub async fn exec(
        &self,
        request: ExecRequest,
        parent_call_id: &str,
        cancel: CancellationToken,
    ) -> Result<CellPage, CellError> {
        self.limits.page_limits(
            Duration::from_millis(request.yield_time_ms),
            request.max_output_tokens,
        )?;
        if parent_call_id.is_empty()
            || parent_call_id.len() > 256
            || parent_call_id.chars().any(char::is_control)
        {
            return Err(CellError::Invalid(
                "trusted parent_call_id requires 1..=256 non-control bytes".into(),
            ));
        }
        if request.script.is_empty()
            || request.script.len() > self.limits.script_bytes
            || cancel.is_cancelled()
        {
            return Err(CellError::Invalid(
                "empty/oversized source or invocation already cancelled".into(),
            ));
        }
        let run = {
            // Serialize creation with close so no task can appear after shutdown.
            let mut tasks = self.tasks.lock().expect("task lock poisoned");
            while tasks.try_join_next().is_some() {}
            if self.closed.load(Ordering::Acquire) {
                return Err(CellError::Closed);
            }
            if self.journal.is_frozen(&self.identity.session_id)?
                || self.reads.is_frozen(&self.identity.session_id)
            {
                return Err(CellError::Frozen);
            }
            let session_vm_lock = self
                .journal
                .claim_session(&self.identity.session_id, false)?;
            let permit = self
                .capacity
                .clone()
                .try_acquire_owned()
                .map_err(|_| CellError::Capacity)?;
            let record = CellRecord {
                cell_id: Uuid::new_v4().to_string(),
                identity: self.identity.clone(),
                parent_call_id: parent_call_id.to_owned(),
                source_hash: format!("sha256:{:x}", Sha256::digest(request.script.as_bytes())),
                source: request.script,
                status: CellStatus::NotStarted,
                calls: Vec::new(),
                outputs: Vec::new(),
                output_bytes: 0,
                error: None,
                stop: None,
                backend_error: None,
                reconciliation: Vec::new(),
                host_settlements: Vec::new(),
                revision: 1,
            };
            self.journal.create(&record, &self.limits)?;
            let run = Run::new(
                record,
                self.backend.clone(),
                self.journal.clone(),
                self.reads.clone(),
                RunLimits {
                    cell: self.limits.clone(),
                    shutdown_grace: self.vm_limits.shutdown_grace,
                },
                self.cancel.child_token(),
                true,
            );
            self.cells
                .lock()
                .expect("manager lock poisoned")
                .insert(run.id(), run.clone());
            if let Err(error) = self.backend.created(&run.execution) {
                run.hook_failed(&error);
                run.live.store(false, Ordering::Release);
                return Err(error);
            }
            let host = match Bridge::new(self.backend.clone(), run.clone()) {
                Ok(host) => Arc::new(host),
                Err(error) => {
                    run.finish(Some(error.to_string()), false);
                    run.live.store(false, Ordering::Release);
                    return Err(error);
                }
            };
            let mode = match CodeMode::new(host.clone(), self.vm_limits.clone()) {
                Ok(mode) => mode,
                Err(error) => {
                    run.finish(Some(error.to_string()), false);
                    run.live.store(false, Ordering::Release);
                    return Err(CellError::Invalid(error.to_string()));
                }
            };
            let task_run = run.clone();
            let completion = Completion {
                run: task_run.clone(),
                _scope: self._scope.clone(),
                _session_vm_lock: session_vm_lock,
            };
            let abort = tasks.spawn(async move {
                let _completion = completion;
                let _permit = permit;
                if let Err(error) = task_run.update(|r| {
                    r.status = CellStatus::Running;
                    Ok(())
                }) {
                    task_run.finish(Some(error.to_string()), false);
                    return;
                }
                let source = task_run
                    .record
                    .lock()
                    .expect("cell lock poisoned")
                    .source
                    .clone();
                match mode
                    .execute_with_services(&source, task_run.cancel.clone(), host)
                    .await
                {
                    Ok(report) => task_run.complete_report(report, None),
                    Err(error) => {
                        let stop = error.kind.stop();
                        let message = error.to_string();
                        if let Err(fault) = task_run.update(|record| {
                            record.stop = stop;
                            Ok(())
                        }) {
                            task_run.hook_failed(&fault);
                        }
                        task_run.complete_report(*error.report, Some(message));
                    }
                }
            });
            *run.abort.lock().expect("abort lock poisoned") = Some(abort);
            run
        };
        let cell_id = run.id();
        // Dropping an in-flight exec/wait cancels its cell. A normal timed yield
        // disarms this guard so the managed VM stays alive between invocations.
        self.wait_internal(
            run,
            WaitRequest {
                cell_id,
                cursor: 0,
                yield_time_ms: request.yield_time_ms,
                max_output_tokens: request.max_output_tokens,
                continue_execution: false,
            },
            cancel,
        )
        .await
    }
    pub async fn wait(
        &self,
        request: WaitRequest,
        cancel: CancellationToken,
    ) -> Result<CellPage, CellError> {
        self.limits.page_limits(
            Duration::from_millis(request.yield_time_ms),
            request.max_output_tokens,
        )?;
        let run = self.get(&request.cell_id)?;
        self.wait_internal(run, request, cancel).await
    }
    async fn wait_internal(
        &self,
        run: Arc<Run>,
        request: WaitRequest,
        cancel: CancellationToken,
    ) -> Result<CellPage, CellError> {
        if request.cursor > run.record.lock().expect("cell lock poisoned").outputs.len() as u64 {
            return Err(CellError::Invalid("cursor exceeds published output".into()));
        }
        if request.continue_execution && !cancel.is_cancelled() {
            run.resume()?;
        }
        let mut guard = InvocationGuard {
            run: run.clone(),
            armed: true,
        };
        let mut changed = run.changed.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(request.yield_time_ms);
        loop {
            let ready = {
                let record = run.record.lock().expect("cell lock poisoned");
                record.outputs.len() as u64 > request.cursor
                    || record.status == CellStatus::Yielded
                    || !run.live.load(Ordering::Acquire)
            };
            let publishing_terminal = run
                .record
                .lock()
                .expect("cell lock poisoned")
                .status
                .is_terminal()
                && run.live.load(Ordering::Acquire);
            if publishing_terminal {
                let _ = changed.changed().await;
                continue;
            }
            if cancel.is_cancelled() {
                run.stop();
                break;
            }
            if ready || request.yield_time_ms == 0 {
                break;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => { run.stop(); break; },
                _ = tokio::time::sleep_until(deadline) => break,
                result = changed.changed() => if result.is_err() { break; },
            }
        }
        if run.cancel.is_cancelled() {
            run.wait_stopped().await;
        }
        let page = run.page(request.cursor, request.max_output_tokens)?;
        guard.armed = false;
        Ok(page)
    }
    pub async fn cancel(&self, id: &str) -> Result<CellPage, CellError> {
        let run = self.get(id)?;
        run.stop();
        self.wait_internal(
            run,
            WaitRequest {
                cell_id: id.into(),
                cursor: 0,
                yield_time_ms: 0,
                max_output_tokens: self.limits.max_output_tokens,
                continue_execution: false,
            },
            CancellationToken::new(),
        )
        .await
    }
    pub async fn close(&self) -> CellCloseReport {
        let mut tasks = {
            let mut tasks = self.tasks.lock().expect("task lock poisoned");
            self.closed.store(true, Ordering::Release);
            self.cancel.cancel();
            std::mem::take(&mut *tasks)
        };
        let deadline =
            tokio::time::Instant::now() + self.vm_limits.shutdown_grace + Duration::from_secs(1);
        let joined = tokio::time::timeout_at(deadline, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        if joined.is_err() {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
        // Concurrent close callers must also wait for tasks owned by the first
        // caller. Keep task ownership in JoinSet while waiting; if this close
        // future is dropped, its JoinSet aborts every remaining task.
        let runs: Vec<_> = self
            .cells
            .lock()
            .expect("manager lock poisoned")
            .values()
            .cloned()
            .collect();
        for run in &runs {
            run.wait_stopped().await;
        }
        let cells: Vec<_> = runs
            .iter()
            .map(|run| {
                let record = run.record.lock().expect("cell lock poisoned");
                ClosedCell {
                    execution: run.execution.clone(),
                    terminal: CellTerminal {
                        status: record.status,
                        error: record.error.clone(),
                        stop: record.stop,
                        outcome_unknown: record.status == CellStatus::OutcomeUnknown
                            || record.unresolved()
                            || record.backend_error.is_some(),
                    },
                }
            })
            .collect();
        CellCloseReport {
            outcome_unknown: cells.iter().any(|cell| cell.terminal.outcome_unknown),
            cells,
        }
    }
    /// Observe one stopped cell while this manager retains its original task
    /// ownership. Uses the same terminal checks as StoppedScope, but cannot repair
    /// backend lifecycle failures. Runtime cross-store recovery uses StoppedScope
    /// after dropping the old manager, before modifying its durable host facts.
    pub fn reconcile(
        &self,
        id: &str,
        evidence: ReconciliationEvidence,
    ) -> Result<CellRecord, CellError> {
        // Inspect both raw Values before Serialize/Clone/Eq can recurse into
        // them. Drain both on rejection, including an unvisited deep sibling.
        if let Err(error) = validate_json(
            &evidence.expected_arguments,
            self.limits.json_depth.saturating_sub(1),
            self.limits.item_bytes,
        )
        .and_then(|()| {
            validate_json(
                &evidence.observed_result,
                self.limits.json_depth.saturating_sub(1),
                self.limits.item_bytes,
            )
        }) {
            discard_json(evidence.expected_arguments);
            discard_json(evidence.observed_result);
            return Err(error);
        }
        // Preserve the existing whole-evidence byte/depth contract after its
        // container payloads are bounded. All later returns now own safe Values.
        validate_json(
            &serde_json::to_value(&evidence)?,
            self.limits.json_depth,
            self.limits.item_bytes,
        )?;
        if evidence.evidence.trim().is_empty() {
            return Err(CellError::Invalid(
                "reconciliation requires an observation reference".into(),
            ));
        }
        let run = self.get(id)?;
        if run.live.load(Ordering::Acquire) {
            return Err(CellError::Invalid(
                "cancel and join the live cell before reconciling".into(),
            ));
        }
        let mut guard = run.record.lock().expect("cell lock poisoned");
        let mut record = guard.clone();
        if evidence.source_hash != record.source_hash {
            return Err(CellError::Conflict);
        }
        let call = record
            .calls
            .iter()
            .find(|c| c.sequence == evidence.call_sequence)
            .ok_or(CellError::NotFound)?;
        let terminal = match evidence.resolution {
            Resolution::Succeeded => HostCallTerminal::Succeeded {
                result: HostResult::Value(evidence.observed_result.clone()),
            },
            Resolution::Failed => HostCallTerminal::Failed {
                error: evidence
                    .observed_result
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| evidence.observed_result.to_string()),
            },
        };
        let settlement = HostSettlement {
            execution: record.execution(),
            calls: vec![HostCallSettlement {
                sequence: evidence.call_sequence,
                name: call.name.clone(),
                arguments: evidence.expected_arguments.clone(),
                dependencies: call.dependencies.clone(),
                terminal,
            }],
            execution_terminal: None,
            evidence: evidence.evidence.clone(),
        };
        let (settled, changed) = crate::settlement::apply_settlement(
            record.clone(),
            settlement,
            &ExecutorStopped {
                identity: record.identity.clone(),
                evidence: "manager owns original task lock and observed cell VM completion".into(),
            },
            &self.limits,
        )?;
        if changed {
            let previous = record.revision;
            record = settled;
            record.reconciliation.push(evidence);
            self.journal.persist(&record, previous)?;
            *guard = record.clone();
        }
        drop(guard);
        if let Ok(_vm_gate) = self.journal.claim_session(&self.identity.session_id, true) {
            match self.journal.thaw_stopped_session(&self.identity.session_id) {
                Ok(()) => self.reads.thaw_session(&self.identity.session_id),
                Err(CellError::Frozen) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(record)
    }
}
impl Drop for CellManager {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.tasks
            .get_mut()
            .expect("task lock poisoned")
            .abort_all();
        // The shared lease remains held by aborted tasks until their VM futures
        // are actually dropped. Drop cannot synchronously join Tokio tasks.
    }
}
struct ScopeLease {
    _execution_lock: crate::journal::JournalLock,
}

/// Catalog validation only. Deliberately cannot execute: every production call
/// must carry a concrete immutable CellExecution through Bridge.
struct BackendCatalog(Vec<cayw_ai::ToolDefinition>);
#[async_trait::async_trait]
impl CapabilityHost for BackendCatalog {
    fn capabilities(&self) -> Vec<cayw_ai::ToolDefinition> {
        self.0.clone()
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, cayw_codemode::CapabilityError> {
        Err(cayw_codemode::CapabilityError::new(
            "catalog validation cannot dispatch",
        ))
    }
}

pub(crate) struct Run {
    pub(crate) execution: CellExecution,
    backend: Arc<dyn CellBackend>,
    shutdown_grace: Duration,
    stopped_at: Mutex<Option<tokio::time::Instant>>,
    finished: AtomicBool,
    pub(crate) record: Mutex<CellRecord>,
    pub(crate) journal: Arc<SqliteJournal>,
    pub(crate) reads: Arc<ReadCoordinator>,
    pub(crate) limits: CellLimits,
    pub(crate) cancel: CancellationToken,
    pub(crate) changed: watch::Sender<u64>,
    pub(crate) live: AtomicBool,
    pub(crate) abort: Mutex<Option<AbortHandle>>,
    pub(crate) resume_rx: tokio::sync::Mutex<mpsc::Receiver<()>>,
    resume_tx: mpsc::Sender<()>,
    pub(crate) exited: AtomicBool,
    durability_failed: AtomicBool,
}
struct RunLimits {
    cell: CellLimits,
    shutdown_grace: Duration,
}
impl Run {
    fn new(
        record: CellRecord,
        backend: Arc<dyn CellBackend>,
        journal: Arc<SqliteJournal>,
        reads: Arc<ReadCoordinator>,
        limits: RunLimits,
        cancel: CancellationToken,
        live: bool,
    ) -> Arc<Self> {
        let (changed, _) = watch::channel(0);
        let (resume_tx, resume_rx) = mpsc::channel(1);
        Arc::new(Self {
            execution: record.execution(),
            backend,
            shutdown_grace: limits.shutdown_grace,
            stopped_at: Mutex::default(),
            finished: AtomicBool::new(!live),
            record: Mutex::new(record),
            journal,
            reads,
            limits: limits.cell,
            cancel,
            changed,
            live: AtomicBool::new(live),
            abort: Mutex::default(),
            resume_rx: tokio::sync::Mutex::new(resume_rx),
            resume_tx,
            exited: AtomicBool::new(false),
            durability_failed: AtomicBool::new(false),
        })
    }
    pub(crate) fn id(&self) -> String {
        self.record
            .lock()
            .expect("cell lock poisoned")
            .cell_id
            .clone()
    }
    pub(crate) fn identity(&self) -> CellIdentity {
        self.execution.identity.clone()
    }
    pub(crate) fn check(&self) -> Result<(), CellError> {
        if self.cancel.is_cancelled() {
            return Err(CellError::Closed);
        }
        if self.exited.load(Ordering::Acquire) {
            return Err(CellError::Exited);
        }
        let identity = self.identity();
        if self.journal.is_frozen(&identity.session_id)?
            || self.reads.is_frozen(&identity.session_id)
        {
            return Err(CellError::Frozen);
        }
        Ok(())
    }
    pub(crate) fn update(
        &self,
        edit: impl FnOnce(&mut CellRecord) -> Result<(), CellError>,
    ) -> Result<(), CellError> {
        let mut guard = self.record.lock().expect("cell lock poisoned");
        let mut record = guard.clone();
        edit(&mut record)?;
        let previous = record.revision;
        record.revision += 1;
        if let Err(error) = self.journal.persist(&record, previous) {
            self.durability_failed.store(true, Ordering::Release);
            self.reads.freeze(&record.identity.session_id);
            self.cancel.cancel();
            return Err(error);
        }
        *guard = record;
        self.changed
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        Ok(())
    }
    pub(crate) fn emit(&self, output: CellOutput) -> Result<(), CellError> {
        let bytes = output.validate(&self.limits)?;
        let mut entry = None;
        self.update(|record| {
            if record.outputs.len() >= self.limits.output_items
                || record.output_bytes.saturating_add(bytes) > self.limits.output_bytes
            {
                return Err(CellError::Capacity);
            }
            let committed = OutputEntry {
                cursor: record.outputs.len() as u64 + 1,
                output,
            };
            entry = Some(committed.clone());
            record.outputs.push(committed);
            record.output_bytes += bytes;
            Ok(())
        })?;
        self.backend.output(
            &self.execution,
            &entry.expect("successful output commit retained its entry"),
        )
    }
    fn resume(&self) -> Result<(), CellError> {
        {
            let record = self.record.lock().expect("cell lock poisoned");
            if record.status != CellStatus::Yielded || !self.live.load(Ordering::Acquire) {
                return Ok(());
            }
        }
        self.check()?;
        let mut guard = self.record.lock().expect("cell lock poisoned");
        if guard.status != CellStatus::Yielded {
            return Ok(());
        }
        let mut record = guard.clone();
        record.status = CellStatus::Running;
        let previous = record.revision;
        record.revision += 1;
        self.journal.persist(&record, previous)?;
        *guard = record;
        self.resume_tx
            .try_send(())
            .map_err(|_| CellError::Invalid("continuation already queued".into()))?;
        self.changed
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        Ok(())
    }
    fn abort_now(&self) {
        self.cancel.cancel();
        if let Some(abort) = &*self.abort.lock().expect("abort lock poisoned") {
            abort.abort();
        }
    }
    fn stop(&self) {
        if self
            .record
            .lock()
            .expect("cell lock poisoned")
            .status
            .is_terminal()
        {
            return;
        }
        self.cancel.cancel();
        self.stopped_at
            .lock()
            .expect("stop clock poisoned")
            .get_or_insert_with(tokio::time::Instant::now);
    }
    async fn wait_stopped(&self) {
        let deadline = *self
            .stopped_at
            .lock()
            .expect("stop clock poisoned")
            .get_or_insert_with(tokio::time::Instant::now)
            + self.shutdown_grace
            + Duration::from_secs(1);
        let mut changes = self.changed.subscribe();
        while self.live.load(Ordering::Acquire) {
            if tokio::time::timeout_at(deadline, changes.changed())
                .await
                .is_err()
            {
                self.abort_now();
                while self.live.load(Ordering::Acquire) {
                    if changes.changed().await.is_err() {
                        break;
                    }
                }
                break;
            }
        }
    }
    fn complete_report(&self, report: ExecutionReport, mut error: Option<String>) {
        // All explicit outputs were validated and committed live by VmServices.
        if !self.exited.load(Ordering::Acquire)
            && error.is_none()
            && report.return_value != Value::Null
            && let Err(fault) = self.emit(CellOutput::Json {
                value: report.return_value,
            })
        {
            let message = fault.to_string();
            let stop = fault.capability_error().stop;
            if let Err(fault) = self.update(|record| {
                record.stop = stop;
                Ok(())
            }) {
                self.hook_failed(&fault);
            }
            error = Some(message);
        }
        self.finish(error, true);
    }
    fn hook_failed(&self, error: &CellError) {
        self.durability_failed.store(true, Ordering::Release);
        self.reads.freeze(&self.execution.identity.session_id);
        self.cancel.cancel();
        let _ = self.update(|record| {
            record.status = CellStatus::OutcomeUnknown;
            record.error = Some(bounded_error(&error.to_string()));
            record.backend_error = record.error.clone();
            Ok(())
        });
    }
    fn finish(&self, error: Option<String>, completed: bool) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let cancelled = self.cancel.is_cancelled();
        let exited = self.exited.load(Ordering::Acquire);
        let failed = self.durability_failed.load(Ordering::Acquire);
        let result = self.update(|record| {
            for call in &mut record.calls {
                if call.status == IntentStatus::Pending {
                    call.status = IntentStatus::OutcomeUnknown;
                }
            }
            let unknown = failed || record.unresolved();
            if exited {
                record.stop.get_or_insert(CapabilityStop::Exited);
            } else if cancelled || !completed {
                record.stop.get_or_insert(CapabilityStop::Cancelled);
            }
            if unknown {
                self.reads.freeze(&record.identity.session_id);
            }
            record.status = if unknown {
                CellStatus::OutcomeUnknown
            } else if exited {
                CellStatus::Exited
            } else if cancelled || !completed {
                CellStatus::Cancelled
            } else if error.is_some() {
                CellStatus::Failed
            } else {
                CellStatus::Completed
            };
            record.error = if exited && !unknown {
                None
            } else {
                error.map(|e| bounded_error(&e))
            };
            Ok(())
        });
        if result.is_err() {
            let mut record = self.record.lock().expect("cell lock poisoned");
            record.status = CellStatus::OutcomeUnknown;
            record.error = Some("Durable journal unavailable; execution stopped".into());
        } else {
            let terminal = {
                let record = self.record.lock().expect("cell lock poisoned");
                CellTerminal {
                    status: record.status,
                    error: record.error.clone(),
                    outcome_unknown: record.status == CellStatus::OutcomeUnknown,
                    stop: record.stop,
                }
            };
            #[cfg(feature = "fault-injection")]
            cayw_ai::faults::hit(
                "cells.execution_terminal.committed",
                serde_json::json!({"execution":self.execution,"terminal":terminal}),
            );
            if let Err(error) = self.backend.finished(&self.execution, &terminal) {
                self.hook_failed(&error);
            }
        }
    }
    fn page(&self, cursor: u64, budget: usize) -> Result<CellPage, CellError> {
        let record = self.record.lock().expect("cell lock poisoned");
        let mut page = CellPage {
            cell_id: record.cell_id.clone(),
            status: record.status,
            vm_alive: self.live.load(Ordering::Acquire),
            cursor,
            next_cursor: cursor,
            outputs: Vec::new(),
            has_more: cursor < record.outputs.len() as u64,
            required_budget: None,
            outcome_unknown: record.status == CellStatus::OutcomeUnknown,
            stop: record.stop,
            error: record.error.as_ref().map(|error| {
                let mut end = error.len().min(64);
                while !error.is_char_boundary(end) {
                    end -= 1;
                }
                error[..end].to_owned()
            }),
        };
        for entry in record.outputs.iter().skip(cursor as usize) {
            page.outputs.push(entry.clone());
            page.next_cursor = entry.cursor;
            page.has_more = page.next_cursor < record.outputs.len() as u64;
            let bytes = serde_json::to_vec(&page)?.len();
            if bytes > budget {
                page.outputs.pop();
                page.next_cursor = page.outputs.last().map_or(cursor, |e| e.cursor);
                page.has_more = true;
                if page.outputs.is_empty() {
                    page.required_budget = Some(bytes);
                }
                break;
            }
        }
        // required_budget itself adds bytes; reserve room without losing output.
        if serde_json::to_vec(&page)?.len() > budget {
            page.error = None;
        }
        if serde_json::to_vec(&page)?.len() > budget {
            return Err(CellError::Invalid(
                "page metadata exceeds output budget".into(),
            ));
        }
        Ok(page)
    }
}
struct Completion {
    run: Arc<Run>,
    _scope: Arc<ScopeLease>,
    _session_vm_lock: crate::journal::JournalLock,
}
impl Drop for Completion {
    fn drop(&mut self) {
        if !self.run.finished.load(Ordering::Acquire) {
            self.run.finish(
                Some("Execution stopped before a final result".into()),
                false,
            );
        }
        self.run.live.store(false, Ordering::Release);
        self.run
            .changed
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}
struct InvocationGuard {
    run: Arc<Run>,
    armed: bool,
}
impl Drop for InvocationGuard {
    fn drop(&mut self) {
        if self.armed {
            // The manager still owns/polls this VM task after exec/wait is
            // dropped. Cancel first; CodeMode polls the same started capability
            // through shutdown_grace before dropping an unresolved invocation.
            self.run.stop();
        }
    }
}
