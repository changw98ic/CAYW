//! Per-task process ownership is separate from a VM or a model tool future.
use crate::{Runtime, RuntimeError, execution::RunContext, lock, queue::TaskLease};
use cayw_core::AttemptLease;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

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
            evidence TEXT);",
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

pub(crate) fn is_process(name: &str) -> bool {
    matches!(
        name,
        "git.status"
            | "git.diff"
            | "git.log"
            | "git.show"
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

    /// Host-only proof for an original process scope whose supervisor was lost.
    /// This confirms cleanup only; uncertain file/remote effects remain in Core.
    /// It is deliberately absent from model tools and the ordinary RPC methods.
    pub fn confirm_processes_stopped(
        &self,
        task_id: &str,
        actor: &str,
        evidence: &str,
    ) -> Result<(), RuntimeError> {
        if actor.trim().is_empty()
            || actor.len() > 256
            || evidence.trim().is_empty()
            || evidence.len() > 4096
        {
            return Err(RuntimeError::InvalidInput(
                "process cleanup requires a host actor and 1..4096 bytes of observed evidence"
                    .into(),
            ));
        }
        let _coordinate = self.inner.coordination.enter()?;
        let record = self.inner.queue.get(task_id)?;
        if lock(&self.inner.active).contains_key(&record.session_id) {
            return Err(RuntimeError::Busy);
        }
        let _ownership = self.inner.coordination.task(task_id)?;
        if let Some(host) = lock(&self.inner.processes).get(task_id).cloned() {
            host.close_processes();
            if host.active_processes() != 0 {
                return Err(RuntimeError::Busy);
            }
        }
        self.inner.process_journal.finish(
            task_id,
            record.worker_id.as_deref(),
            "host_verified",
            actor,
            evidence,
        )?;
        lock(&self.inner.processes).remove(task_id);
        self.inner.changed.notify_waiters();
        Ok(())
    }
}
