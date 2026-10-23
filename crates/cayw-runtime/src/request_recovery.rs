//! Explicit host administration for Requests storage that cannot admit work.
use crate::{
    Runtime, RuntimeConfig, RuntimeError,
    recovery::{Owners, StoppedResources, stopped_owners_from},
};
use cayw_core::{AttemptLease, CoreError, CoreEvent, SessionStore};
use cayw_requests::{
    AuthorityInspection, AuthorityRecoveryGuard, AuthorityRepairReceipt, AuthorityRepairRequest,
    RequestConfig, RequestError, RequestStore, TaskIdentity,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) fn request_path(store_path: &Path) -> Result<PathBuf, RuntimeError> {
    let mut path = store_path
        .canonicalize()
        .map_err(|error| RuntimeError::Workspace(error.to_string()))?
        .into_os_string();
    path.push(".requests.sqlite");
    Ok(path.into())
}

struct AuthorityOwners {
    _owners: Vec<Owners>,
    verified: Vec<TaskIdentity>,
}
impl AuthorityRecoveryGuard for AuthorityOwners {
    fn verified_tasks(&self) -> &[TaskIdentity] {
        &self.verified
    }
}

fn original_leases(
    store: &SessionStore,
    session: &str,
    end: u64,
) -> Result<BTreeMap<String, AttemptLease>, RuntimeError> {
    let mut leases = BTreeMap::new();
    let mut cursor = 0;
    while cursor < end {
        let page = store.events(
            session,
            cursor,
            (end - cursor).min(cayw_core::MAX_EVENT_PAGE as u64) as usize,
        )?;
        if page.is_empty() {
            return Err(CoreError::CorruptStore(
                "original Session event prefix is incomplete".into(),
            )
            .into());
        }
        for record in page {
            if record.session_id != session || record.cursor != cursor + 1 {
                return Err(CoreError::CorruptStore(
                    "original Session event cursors are not continuous".into(),
                )
                .into());
            }
            cursor = record.cursor;
            if let CoreEvent::AttemptStarted { lease } = record.event
                && (lease.session_id != session
                    || record.attempt.as_ref() != Some(&lease)
                    || leases.insert(lease.attempt_id.clone(), lease).is_some())
            {
                return Err(CoreError::CorruptStore(
                    "original Attempt lease is missing or duplicated".into(),
                )
                .into());
            }
        }
    }
    Ok(leases)
}

impl Runtime {
    /// Read the original permission authority even when `Runtime::open` refuses
    /// admission. This does not create storage, advance time or enable anything.
    pub fn inspect_request_authority(
        store_path: impl AsRef<Path>,
        limits: RequestConfig,
    ) -> Result<AuthorityInspection, RuntimeError> {
        Ok(RequestStore::inspect_authority(
            request_path(store_path.as_ref())?,
            limits,
        )?)
    }

    /// Confirm resource cleanup after the original process supervisor was lost,
    /// including when the stopped Requests authority prevents `Runtime::open`.
    /// The host callback must independently verify its original processes and
    /// resources. It runs with the original Task and Cells ownership held.
    /// This never reconciles effects, enables Requests, or releases Queue capacity.
    #[cfg(target_os = "macos")]
    pub fn confirm_lost_processes_stopped(
        store_path: impl AsRef<Path>,
        config: RuntimeConfig,
        target: &crate::CancelTarget,
        actor: &cayw_requests::HostActor,
        evidence: &str,
        verify_stopped: impl FnOnce(&crate::ProcessCleanupInspection) -> Result<(), String>,
    ) -> Result<(), RuntimeError> {
        config.validate()?;
        if evidence.trim().is_empty() || evidence.len() > 4096 {
            return Err(RuntimeError::InvalidInput(
                "process cleanup requires 1..4096 bytes of independent host evidence".into(),
            ));
        }
        let store_path = store_path
            .as_ref()
            .canonicalize()
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
        let coordination = crate::coordination::Coordination::new(&store_path)?;
        let _coordinate = coordination.enter()?;
        let queue = crate::queue::Queue::open(
            &store_path,
            config.max_concurrent_runs,
            config.max_queued_runs,
        )?;
        let task = queue.get(&target.task_id)?;
        if task.cancel_target() != *target {
            return Err(RuntimeError::InvalidInput(
                "process cleanup differs from its original Task identity".into(),
            ));
        }
        let _task_owner = coordination.task(&target.task_id)?;
        let store = SessionStore::open(&store_path)?;
        let snapshot = store.snapshot(&target.session_id)?;
        let leases = original_leases(&store, &target.session_id, snapshot.cursor)?;
        let lease = task
            .attempt_id
            .as_ref()
            .and_then(|id| leases.get(id))
            .filter(|lease| {
                lease.session_id == target.session_id
                    && Some(&lease.worker_id) == target.worker_id.as_ref()
            })
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "process cleanup has no matching original Core Attempt".into(),
                )
            })?;
        let processes = crate::processes::ProcessJournal::open(&store_path)?;
        let inspection = processes.inspect_lost(target, lease)?;
        let stopped = |identity: &cayw_host::ProcessIdentity| -> Result<(), RuntimeError> {
            match identity.is_running() {
                Ok(false) => Ok(()),
                Ok(true) => Err(RuntimeError::Busy),
                Err(error) => Err(RuntimeError::Worker(format!(
                    "original process cleanup cannot be observed: {error}"
                ))),
            }
        };
        // A live original Host can still retain Device permits and cleanup
        // resources after its Task future stopped. Never discard those owners.
        stopped(&inspection.supervisor)?;
        let cells = Arc::new(
            cayw_cells::SqliteJournal::open(&store_path)
                .map_err(|error| RuntimeError::Workspace(error.to_string()))?,
        );
        let cell_identity = cayw_cells::CellIdentity {
            session_id: lease.session_id.clone(),
            task_id: task.id.clone(),
            attempt_id: lease.attempt_id.clone(),
            permission_context: format!("{}:{}", lease.worker_id, lease.fence),
        };
        let scope = cells
            .stopped_scope(
                cayw_cells::ExecutorStopped {
                    identity: cell_identity,
                    evidence: format!(
                        "Original Task {} locked; recorded supervisor incarnation has stopped",
                        task.id
                    ),
                },
                config.cells.clone(),
            )
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
        let records = scope
            .records()
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
        let mut calls = BTreeMap::new();
        let mut cursor = 0;
        while cursor < snapshot.cursor {
            let page = store.events(
                &target.session_id,
                cursor,
                (snapshot.cursor - cursor).min(cayw_core::MAX_EVENT_PAGE as u64) as usize,
            )?;
            if page.is_empty() {
                return Err(CoreError::CorruptStore(
                    "original process call events are incomplete".into(),
                )
                .into());
            }
            for event in page {
                if event.session_id != target.session_id || event.cursor != cursor + 1 {
                    return Err(CoreError::CorruptStore(
                        "original process call event cursors differ".into(),
                    )
                    .into());
                }
                cursor = event.cursor;
                match event.event {
                    CoreEvent::CapabilityPlanned { call }
                    | CoreEvent::CapabilityStarted { call }
                    | CoreEvent::CapabilitySettled { call }
                    | CoreEvent::CapabilityReconciled { call, .. } => {
                        calls.insert(call.key.clone(), call);
                    }
                    CoreEvent::AttemptRecovered { capabilities, .. } => {
                        for call in capabilities {
                            calls.insert(call.key.clone(), call);
                        }
                    }
                    _ => {}
                }
            }
        }
        for process in &inspection.processes {
            stopped(&process.identity)?;
            let call = calls
                .get(&process.key)
                .filter(|call: &&cayw_core::CapabilityCallRecord| {
                    call.lease == *lease
                        && call.task_id == task.id
                        && crate::processes::is_process(&call.name)
                        && call.stage != cayw_core::CapabilityStage::Planned
                })
                .ok_or_else(|| {
                    CoreError::CorruptStore(
                        "recorded process has no matching original started capability".into(),
                    )
                })?;
            let record = records
                .iter()
                .find(|record| record.cell_id == call.key.execution_id)
                .ok_or_else(|| {
                    CoreError::CorruptStore(
                        "recorded process has no original stopped Cells execution".into(),
                    )
                })?;
            if record.parent_call_id != call.parent_call_id
                || record.source_hash != call.source_hash
                || !record.calls.iter().any(|intent| {
                    call.key.sequence.checked_add(1) == Some(intent.sequence)
                        && intent.name == call.name
                        && intent.arguments == call.arguments
                })
            {
                return Err(CoreError::CorruptStore(
                    "recorded process differs from its original Cells intent".into(),
                )
                .into());
            }
        }
        verify_stopped(&inspection).map_err(|error| {
            RuntimeError::Worker(format!(
                "independent process cleanup evidence rejected: {error}"
            ))
        })?;
        processes.confirm_lost(&inspection, actor, evidence)?;
        Ok(())
    }

    /// Host-only administration. Retain all original Task and stopped Cells
    /// owners through the authority's independent repair commits. No model or
    /// ordinary RPC input can construct the trusted HostActor in this request.
    /// Core/Queue terminals and occupied capacity are left for explicit recovery.
    pub fn repair_request_authority(
        store_path: impl AsRef<Path>,
        config: RuntimeConfig,
        request: &AuthorityRepairRequest,
    ) -> Result<AuthorityRepairReceipt, RuntimeError> {
        config.validate()?;
        let store_path = store_path
            .as_ref()
            .canonicalize()
            .map_err(|error| RuntimeError::Workspace(error.to_string()))?;
        let requests_path = request_path(&store_path)?;
        let coordination = crate::coordination::Coordination::new(&store_path)?;
        let _coordinate = coordination.enter()?;
        let store = SessionStore::open(&store_path)?;
        let queue = crate::queue::Queue::open(
            &store_path,
            config.max_concurrent_runs,
            config.max_queued_runs,
        )?;
        let processes = crate::processes::ProcessJournal::open(&store_path)?;
        let cells = Arc::new(
            cayw_cells::SqliteJournal::open(&store_path)
                .map_err(|error| RuntimeError::Workspace(error.to_string()))?,
        );
        let mut owners = Vec::new();
        let mut tasks = BTreeMap::new();
        // An original Worker can exist before Requests.start_task committed.
        // Therefore inspection.tasks alone is not the complete ownership set.
        for snapshot in store.list_sessions()? {
            let leases = original_leases(&store, &snapshot.id, snapshot.cursor)?;
            owners.push(stopped_owners_from(
                StoppedResources {
                    queue: &queue,
                    cells: &cells,
                    coordination: &coordination,
                    process_journal: &processes,
                    limits: &config.cells,
                },
                &snapshot.id,
                &leases,
            )?);
            for task in queue.list(&snapshot.id)? {
                tasks.insert(task.id.clone(), task);
            }
        }
        let mut verified = Vec::new();
        for inspected in &request.inspection.tasks {
            let identity = &inspected.identity;
            let task = tasks.get(&identity.task_id).ok_or_else(|| {
                RuntimeError::InvalidInput("authority task has no original Runtime Task".into())
            })?;
            if task.session_id != identity.session_id
                || task.attempt_id.as_ref() != Some(&identity.attempt_id)
            {
                return Err(RuntimeError::InvalidInput(
                    "authority task differs from its original Runtime Attempt".into(),
                ));
            }
            verified.push(identity.clone());
        }
        let owners = AuthorityOwners {
            _owners: owners,
            verified,
        };
        // Acquire lifecycle/Task/Cells ownership before entering Requests' own
        // transaction. The callback only transfers the already-held proof.
        Ok(RequestStore::repair_authority(
            requests_path,
            config.request_limits,
            request,
            |observed| {
                if observed != &request.inspection {
                    return Err(RequestError::Stale);
                }
                Ok(owners)
            },
        )?)
    }
}
