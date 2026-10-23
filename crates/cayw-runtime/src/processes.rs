//! Per-task process ownership is separate from a VM or a model tool future.
use crate::{Runtime, RuntimeError, execution::RunContext, lock, queue::TaskLease};
use cayw_core::AttemptLease;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

/// Original process records presented to a trusted host's cleanup verifier.
/// An inspection never authorizes signals, business reconciliation or new work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCleanupInspection {
    pub target: crate::CancelTarget,
    pub lease: AttemptLease,
    pub supervisor: cayw_host::ProcessIdentity,
    pub processes: Vec<ProcessCleanupProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCleanupProcess {
    pub key: cayw_core::CapabilityCallKey,
    pub identity: cayw_host::ProcessIdentity,
    pub state: String,
}

pub(crate) struct ProcessJournal {
    connection: Mutex<Connection>,
}
impl ProcessJournal {
    pub(crate) fn open(path: &Path) -> Result<Self, RuntimeError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS runtime_process_scopes (
            task TEXT PRIMARY KEY, session TEXT NOT NULL, attempt TEXT NOT NULL,
            worker TEXT NOT NULL, fence INTEGER NOT NULL, state TEXT NOT NULL,
            evidence TEXT);
            CREATE TABLE IF NOT EXISTS runtime_process_identities (
            task TEXT NOT NULL, session TEXT NOT NULL, attempt TEXT NOT NULL,
            worker TEXT NOT NULL, fence INTEGER NOT NULL,
            execution TEXT NOT NULL, sequence INTEGER NOT NULL,
            pid INTEGER NOT NULL, parent_pid INTEGER NOT NULL, process_group INTEGER NOT NULL,
            started_seconds INTEGER NOT NULL, started_microseconds INTEGER NOT NULL,
            state TEXT NOT NULL,
            PRIMARY KEY (task, execution, sequence, pid, started_seconds, started_microseconds));
            CREATE TABLE IF NOT EXISTS runtime_process_supervisors (
            task TEXT PRIMARY KEY, session TEXT NOT NULL, attempt TEXT NOT NULL,
            worker TEXT NOT NULL, fence INTEGER NOT NULL,
            pid INTEGER NOT NULL, parent_pid INTEGER NOT NULL, process_group INTEGER NOT NULL,
            started_seconds INTEGER NOT NULL, started_microseconds INTEGER NOT NULL);",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
    pub(crate) fn starting(&self, task: &str, lease: &AttemptLease) -> Result<(), RuntimeError> {
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO runtime_process_scopes(task,session,attempt,worker,fence,state)
            VALUES(?1,?2,?3,?4,?5,'open')",
            params![
                task,
                lease.session_id,
                lease.attempt_id,
                lease.worker_id,
                crate::audit::sql_u64(lease.fence)?
            ],
        )?;
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_process_scopes
            WHERE task=?1 AND session=?2 AND attempt=?3 AND worker=?4 AND fence=?5 AND state='open')",
            params![task, lease.session_id, lease.attempt_id, lease.worker_id, crate::audit::sql_u64(lease.fence)?], |row| row.get(0))?;
        if !valid {
            return Err(RuntimeError::Worker(
                "process scope is closed or belongs to another attempt".into(),
            ));
        }
        #[cfg(target_os = "macos")]
        {
            let supervisor =
                cayw_host::ProcessIdentity::observe(std::process::id()).map_err(|error| {
                    RuntimeError::Worker(format!("observe original process supervisor: {error}"))
                })?;
            tx.execute(
                "INSERT OR IGNORE INTO runtime_process_supervisors(
                task,session,attempt,worker,fence,pid,parent_pid,process_group,started_seconds,started_microseconds)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![task, lease.session_id, lease.attempt_id, lease.worker_id,
                    crate::audit::sql_u64(lease.fence)?, i64::from(supervisor.pid),
                    i64::from(supervisor.parent_pid), i64::from(supervisor.process_group),
                    crate::audit::sql_u64(supervisor.started_seconds)?, i64::from(supervisor.started_microseconds)],
            )?;
            if original_supervisor(&tx, task, lease)? != supervisor {
                return Err(RuntimeError::Worker(
                    "process scope has a different original supervisor".into(),
                ));
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn pending(&self, task: &str) -> Result<bool, RuntimeError> {
        Ok(lock(&self.connection).query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_process_scopes WHERE task=?1 AND state='open')",
            [task],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn inspect_lost(
        &self,
        target: &crate::CancelTarget,
        lease: &AttemptLease,
    ) -> Result<ProcessCleanupInspection, RuntimeError> {
        let connection = lock(&self.connection);
        let owned: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_process_scopes
            WHERE task=?1 AND session=?2 AND attempt=?3 AND worker=?4 AND fence=?5)",
            params![
                target.task_id,
                lease.session_id,
                lease.attempt_id,
                lease.worker_id,
                crate::audit::sql_u64(lease.fence)?
            ],
            |row| row.get(0),
        )?;
        if !owned {
            return Err(RuntimeError::Worker(
                "process scope differs from the original Task and Core lease".into(),
            ));
        }
        let supervisor = original_supervisor(&connection, &target.task_id, lease)?;
        let mut statement = connection.prepare(
            "SELECT session,attempt,worker,fence,execution,sequence,
            pid,parent_pid,process_group,started_seconds,started_microseconds,state
            FROM runtime_process_identities WHERE task=?1
            ORDER BY execution,sequence,pid,started_seconds,started_microseconds",
        )?;
        let processes = statement
            .query_map([&target.task_id], |row| {
                let original = AttemptLease {
                    session_id: row.get(0)?,
                    attempt_id: row.get(1)?,
                    worker_id: row.get(2)?,
                    fence: crate::audit::read_u64(row, 3)?,
                };
                Ok((
                    original,
                    ProcessCleanupProcess {
                        key: cayw_core::CapabilityCallKey {
                            execution_id: row.get(4)?,
                            sequence: crate::audit::read_u64(row, 5)?,
                        },
                        identity: process_identity(row, 6)?,
                        state: row.get(11)?,
                    },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut verified = Vec::new();
        for (original, process) in processes {
            if &original != lease
                || !matches!(process.state.as_str(), "started" | "observed" | "stopped")
            {
                return Err(RuntimeError::Worker(
                    "recorded process differs from its original lease or state".into(),
                ));
            }
            verified.push(process);
        }
        Ok(ProcessCleanupInspection {
            target: target.clone(),
            lease: lease.clone(),
            supervisor,
            processes: verified,
        })
    }

    pub(crate) fn confirm_lost(
        &self,
        inspection: &ProcessCleanupInspection,
        actor: &cayw_requests::HostActor,
        evidence: &str,
    ) -> Result<(), RuntimeError> {
        self.finish(
            &inspection.target.task_id,
            Some(&inspection.lease.worker_id),
            "host_verified",
            actor.as_str(),
            evidence,
        )
    }

    /// Record an OS process that actually started for this exact planned call.
    /// The scope must still be open for the original attempt, and the written row
    /// is re-read with the full original lease and call identity so a reused or
    /// foreign incarnation can never be mistaken for this call's process.
    pub(crate) fn identity_started(
        &self,
        call: &ProcessCallIdentity<'_>,
        identity: &cayw_host::ProcessIdentity,
    ) -> Result<(), RuntimeError> {
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_open_scope(&tx, call)?;
        tx.execute(
            "INSERT OR IGNORE INTO runtime_process_identities(
            task,session,attempt,worker,fence,execution,sequence,
            pid,parent_pid,process_group,started_seconds,started_microseconds,state)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'started')",
            params![
                call.task,
                call.lease.session_id,
                call.lease.attempt_id,
                call.lease.worker_id,
                crate::audit::sql_u64(call.lease.fence)?,
                call.execution_id,
                crate::audit::sql_u64(call.sequence)?,
                i64::from(identity.pid),
                i64::from(identity.parent_pid),
                i64::from(identity.process_group),
                crate::audit::sql_u64(identity.started_seconds)?,
                i64::from(identity.started_microseconds),
            ],
        )?;
        require_owned_identity(&tx, call, identity)?;
        tx.commit()?;
        Ok(())
    }

    /// Record every further incarnation observed in this call's process group.
    /// A descendant reported here is a new OS identity that has no row yet, so it
    /// is inserted for the original open scope and call and then re-read with the
    /// full lease and call identity. A row already confirmed `stopped` keeps its
    /// terminal state, so a late observation can never reopen a finished record,
    /// and no record is ever migrated into another task or attempt.
    pub(crate) fn identity_observed(
        &self,
        call: &ProcessCallIdentity<'_>,
        identities: &[cayw_host::ProcessIdentity],
    ) -> Result<(), RuntimeError> {
        if identities.is_empty() {
            return Ok(());
        }
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_open_scope(&tx, call)?;
        for identity in identities {
            tx.execute(
                "INSERT OR IGNORE INTO runtime_process_identities(
                task,session,attempt,worker,fence,execution,sequence,
                pid,parent_pid,process_group,started_seconds,started_microseconds,state)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'observed')",
                params![
                    call.task,
                    call.lease.session_id,
                    call.lease.attempt_id,
                    call.lease.worker_id,
                    crate::audit::sql_u64(call.lease.fence)?,
                    call.execution_id,
                    crate::audit::sql_u64(call.sequence)?,
                    i64::from(identity.pid),
                    i64::from(identity.parent_pid),
                    i64::from(identity.process_group),
                    crate::audit::sql_u64(identity.started_seconds)?,
                    i64::from(identity.started_microseconds),
                ],
            )?;
            require_owned_identity(&tx, call, identity)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Confirm that the operating system has finished the recorded incarnation.
    /// Only the row already written for the original lease and call is updated;
    /// this records resource cleanup only and never revises a business outcome.
    pub(crate) fn identity_stopped(
        &self,
        call: &ProcessCallIdentity<'_>,
        identity: &cayw_host::ProcessIdentity,
    ) -> Result<(), RuntimeError> {
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE runtime_process_identities SET state='stopped'
            WHERE task=?1 AND session=?2 AND attempt=?3 AND worker=?4 AND fence=?5
            AND execution=?6 AND sequence=?7 AND pid=?8
            AND started_seconds=?9 AND started_microseconds=?10
            AND parent_pid=?11 AND process_group=?12",
            params![
                call.task,
                call.lease.session_id,
                call.lease.attempt_id,
                call.lease.worker_id,
                crate::audit::sql_u64(call.lease.fence)?,
                call.execution_id,
                crate::audit::sql_u64(call.sequence)?,
                i64::from(identity.pid),
                crate::audit::sql_u64(identity.started_seconds)?,
                i64::from(identity.started_microseconds),
                i64::from(identity.parent_pid),
                i64::from(identity.process_group),
            ],
        )?;
        if changed != 1 {
            return Err(RuntimeError::Worker(
                "a stopped OS process is unrecorded or belongs to another attempt and call".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }
    fn settled(&self, task: &TaskLease, evidence: &str) -> Result<(), RuntimeError> {
        self.finish(
            &task.id,
            Some(&task.worker),
            "drained",
            "runtime-process-supervisor",
            evidence,
        )
    }
    fn finish(
        &self,
        task: &str,
        worker: Option<&str>,
        state: &str,
        actor: &str,
        evidence: &str,
    ) -> Result<(), RuntimeError> {
        let mut connection = lock(&self.connection);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String, String)> = tx
            .query_row(
                "SELECT session,attempt,worker,state FROM runtime_process_scopes WHERE task=?1",
                [task],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((session, attempt, original_worker, current)) = row else {
            tx.commit()?;
            return Ok(());
        };
        if worker.is_some_and(|worker| worker != original_worker) {
            return Err(RuntimeError::Worker(
                "process cleanup has a different task worker".into(),
            ));
        }
        if current != "open" {
            tx.commit()?;
            return Ok(());
        }
        let payload = serde_json::json!({"attempt_id":attempt,"worker_id":original_worker,"actor":actor,"evidence":evidence,"state":state});
        tx.execute(
            "INSERT INTO runtime_audit(session,task,kind,timestamp,payload)
            VALUES(?1,?2,'processes_stopped',?3,?4)",
            params![
                session,
                task,
                crate::audit::sql_u64(cayw_ai::timestamp_ms())?,
                serde_json::to_string(&payload)?
            ],
        )?;
        tx.execute(
            "UPDATE runtime_process_scopes SET state=?2,evidence=?3 WHERE task=?1 AND state='open'",
            params![task, state, evidence],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn process_identity(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<cayw_host::ProcessIdentity> {
    let identity = cayw_host::ProcessIdentity {
        pid: row.get(offset)?,
        parent_pid: row.get(offset + 1)?,
        process_group: row.get(offset + 2)?,
        started_seconds: crate::audit::read_u64(row, offset + 3)?,
        started_microseconds: row.get(offset + 4)?,
    };
    if identity.pid == 0
        || identity.started_seconds == 0
        || identity.started_microseconds >= 1_000_000
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(identity)
}

fn original_supervisor(
    connection: &Connection,
    task: &str,
    lease: &AttemptLease,
) -> Result<cayw_host::ProcessIdentity, RuntimeError> {
    connection
        .query_row(
            "SELECT pid,parent_pid,process_group,started_seconds,started_microseconds
        FROM runtime_process_supervisors
        WHERE task=?1 AND session=?2 AND attempt=?3 AND worker=?4 AND fence=?5",
            params![
                task,
                lease.session_id,
                lease.attempt_id,
                lease.worker_id,
                crate::audit::sql_u64(lease.fence)?
            ],
            |row| process_identity(row, 0),
        )
        .map_err(RuntimeError::from)
}

/// The exact planned call that owns an observed OS process incarnation. Every
/// identity write carries the original lease and call so a replaced attempt or a
/// reused call sequence can never adopt another execution's process record.
#[derive(Clone, Copy)]
pub(crate) struct ProcessCallIdentity<'a> {
    pub task: &'a str,
    pub lease: &'a AttemptLease,
    pub execution_id: &'a str,
    pub sequence: u64,
}

/// Require that this exact lease still owns an open process scope. Every new
/// process record is written against the original planned call, so a replaced
/// attempt or an already closed scope can never adopt an observed incarnation.
fn require_open_scope(
    tx: &rusqlite::Transaction<'_>,
    call: &ProcessCallIdentity<'_>,
) -> Result<(), RuntimeError> {
    let open: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_process_scopes
        WHERE task=?1 AND session=?2 AND attempt=?3 AND worker=?4 AND fence=?5 AND state='open')",
        params![
            call.task,
            call.lease.session_id,
            call.lease.attempt_id,
            call.lease.worker_id,
            crate::audit::sql_u64(call.lease.fence)?,
        ],
        |row| row.get(0),
    )?;
    if !open {
        return Err(RuntimeError::Worker(
            "an OS process belongs to a process scope that is closed or owned by another attempt"
                .into(),
        ));
    }
    Ok(())
}

/// Re-read one written incarnation under the full original lease, call and OS
/// identity. The predicate is the same one `stopped` uses, including the recorded
/// topology: a row carrying the same primary key but different parent or group
/// data is not this incarnation's record and must not pass as one.
fn require_owned_identity(
    tx: &rusqlite::Transaction<'_>,
    call: &ProcessCallIdentity<'_>,
    identity: &cayw_host::ProcessIdentity,
) -> Result<(), RuntimeError> {
    let owned: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_process_identities
        WHERE task=?1 AND session=?2 AND attempt=?3 AND worker=?4 AND fence=?5
        AND execution=?6 AND sequence=?7 AND pid=?8
        AND started_seconds=?9 AND started_microseconds=?10
        AND parent_pid=?11 AND process_group=?12)",
        params![
            call.task,
            call.lease.session_id,
            call.lease.attempt_id,
            call.lease.worker_id,
            crate::audit::sql_u64(call.lease.fence)?,
            call.execution_id,
            crate::audit::sql_u64(call.sequence)?,
            i64::from(identity.pid),
            crate::audit::sql_u64(identity.started_seconds)?,
            i64::from(identity.started_microseconds),
            i64::from(identity.parent_pid),
            i64::from(identity.process_group),
        ],
        |row| row.get(0),
    )?;
    if !owned {
        return Err(RuntimeError::Worker(
            "an OS process identity is unrecorded or belongs to another attempt and call".into(),
        ));
    }
    Ok(())
}

pub(crate) fn is_process(name: &str) -> bool {
    matches!(
        name,
        "git.status"
            | "git.diff"
            | "git.log"
            | "git.show"
            | "git.blame"
            | "git.stage"
            | "git.commit"
            | "git.branch"
            | "git.worktree"
            | "exec_command"
            | "write_stdin"
            | "build.run"
            | "test.run"
            | "check.run"
            | "lint.run"
            | "format.run"
    )
}

pub(crate) fn register(
    context: &RunContext,
    host: Arc<cayw_host::Host>,
) -> Result<(), RuntimeError> {
    let mut owners = lock(&context.inner.processes);
    if owners.contains_key(&context.task.id) {
        return Err(RuntimeError::Worker(
            "task already has a process owner".into(),
        ));
    }
    owners.insert(context.task.id.clone(), host);
    Ok(())
}

pub(crate) async fn drain(inner: &Arc<crate::Inner>, task: &TaskLease) -> Result<(), RuntimeError> {
    let host = lock(&inner.processes).get(&task.id).cloned();
    if let Some(host) = host {
        host.close_processes();
        host.drain_processes().await.map_err(|error| {
            RuntimeError::Worker(format!("process cleanup is not confirmed: {error}"))
        })?;
        if host.active_processes() != 0 {
            return Err(RuntimeError::Worker(
                "process supervisors remain after drain".into(),
            ));
        }
        inner.process_journal.settled(
            task,
            "All owned process supervisors exited after managed process cleanup",
        )?;
        lock(&inner.processes).remove(&task.id);
    } else if inner.process_journal.pending(&task.id)? {
        return Err(RuntimeError::Worker(
            "the original process supervisor is unavailable; host inspection is required".into(),
        ));
    }
    Ok(())
}

impl Runtime {
    /// Retries cleanup of a stopped local worker. This never starts a new process.
    /// A lost process owner after a host crash requires independent host evidence.
    pub async fn drain_task_processes(&self, task_id: &str) -> Result<(), RuntimeError> {
        let (task, _ownership) = {
            let _coordinate = self.inner.coordination.enter()?;
            let record = self.inner.queue.get(task_id)?;
            if lock(&self.inner.active).contains_key(&record.session_id) {
                return Err(RuntimeError::Busy);
            }
            let ownership = self.inner.coordination.task(task_id)?;
            let task = TaskLease {
                id: record.id,
                worker: record.worker_id.ok_or_else(|| {
                    RuntimeError::InvalidInput("task has no original worker".into())
                })?,
                fence: record.fence,
            };
            (task, ownership)
        };
        drain(&self.inner, &task).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = "task";

    fn lease(fence: u64) -> AttemptLease {
        AttemptLease {
            session_id: "session".into(),
            attempt_id: "attempt".into(),
            worker_id: "worker".into(),
            fence,
        }
    }

    fn identity(pid: u32) -> cayw_host::ProcessIdentity {
        cayw_host::ProcessIdentity {
            pid,
            parent_pid: 1,
            process_group: pid,
            started_seconds: 1_700_000_000,
            started_microseconds: 5,
        }
    }

    fn open(dir: &std::path::Path) -> ProcessJournal {
        let journal = ProcessJournal::open(&dir.join("journal.db")).unwrap();
        // `finish` writes the cleanup audit row; the real runtime creates this
        // table in its own store, so this test creates the same shape locally.
        lock(&journal.connection)
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS runtime_audit(
                session TEXT NOT NULL, task TEXT, kind TEXT NOT NULL,
                timestamp INTEGER NOT NULL, payload TEXT NOT NULL);",
            )
            .unwrap();
        journal
    }

    fn state(journal: &ProcessJournal, pid: u32) -> Option<String> {
        lock(&journal.connection)
            .query_row(
                "SELECT state FROM runtime_process_identities WHERE pid=?1",
                [i64::from(pid)],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    /// A late report about an incarnation that already finished must never reopen
    /// its record. The supervisor's own loop can report a member after the cleanup
    /// record was written, and that report carries no new information.
    #[test]
    fn a_late_observation_cannot_revert_a_stopped_record() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open(dir.path());
        let lease = lease(1);
        journal.starting(TASK, &lease).unwrap();
        let call = ProcessCallIdentity {
            task: TASK,
            lease: &lease,
            execution_id: "cell",
            sequence: 0,
        };
        let leader = identity(4242);
        journal.identity_started(&call, &leader).unwrap();
        assert_eq!(state(&journal, 4242).as_deref(), Some("started"));
        journal.identity_stopped(&call, &leader).unwrap();
        assert_eq!(state(&journal, 4242).as_deref(), Some("stopped"));
        journal
            .identity_observed(&call, std::slice::from_ref(&leader))
            .unwrap();
        assert_eq!(
            state(&journal, 4242).as_deref(),
            Some("stopped"),
            "a late observation must not reopen a finished record"
        );
        // A genuinely new descendant in the same managed group is still recorded,
        // so the late report is a no-op only for the incarnation that finished.
        let child = cayw_host::ProcessIdentity {
            parent_pid: 4242,
            process_group: 4242,
            ..identity(4243)
        };
        journal.identity_observed(&call, &[child]).unwrap();
        assert_eq!(state(&journal, 4243).as_deref(), Some("observed"));
        assert_eq!(state(&journal, 4242).as_deref(), Some("stopped"));
    }

    /// A replaced attempt, and a scope that is already closed, can never adopt an
    /// observed process — including one that really is in the same process group.
    #[test]
    fn a_replaced_attempt_cannot_adopt_an_observed_process() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open(dir.path());
        let original = lease(1);
        journal.starting(TASK, &original).unwrap();
        let call = ProcessCallIdentity {
            task: TASK,
            lease: &original,
            execution_id: "cell",
            sequence: 0,
        };
        journal.identity_started(&call, &identity(5000)).unwrap();

        // A later attempt cannot even open a scope for the same task.
        let replaced = lease(2);
        assert!(journal.starting(TASK, &replaced).is_err());
        let stale = ProcessCallIdentity {
            task: TASK,
            lease: &replaced,
            execution_id: "cell",
            sequence: 0,
        };
        assert!(journal.identity_started(&stale, &identity(5001)).is_err());
        assert!(
            journal
                .identity_observed(&stale, &[identity(5002)])
                .is_err()
        );
        assert!(journal.identity_stopped(&stale, &identity(5000)).is_err());
        assert!(state(&journal, 5001).is_none());
        assert!(state(&journal, 5002).is_none());
        // The original attempt keeps its own record untouched.
        assert_eq!(state(&journal, 5000).as_deref(), Some("started"));

        // Once the original scope is closed, even the original attempt cannot
        // write another incarnation into it.
        journal
            .finish(TASK, None, "drained", "test", "closed")
            .unwrap();
        assert!(journal.identity_started(&call, &identity(5003)).is_err());
        assert!(journal.identity_observed(&call, &[identity(5004)]).is_err());
        assert!(state(&journal, 5003).is_none());
        assert!(state(&journal, 5004).is_none());
    }
}
