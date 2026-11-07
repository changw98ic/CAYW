//! Permission grants and operational task diagnostics. Capability execution and
//! result pairing live exclusively in the Core session log.
use crate::{RuntimeError, lock};
use cayw_ai::timestamp_ms;
use cayw_core::AttemptLease;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, sync::Mutex};
use uuid::Uuid;

pub(crate) fn sql_u64(n: u64) -> Result<i64, RuntimeError> {
    i64::try_from(n)
        .map_err(|_| RuntimeError::InvalidInput("integer exceeds persistent counter range".into()))
}
pub(crate) fn read_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let n: i64 = row.get(index)?;
    u64::try_from(n).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, n))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub cursor: u64,
    pub session_id: String,
    pub task_id: Option<String>,
    pub kind: String,
    pub timestamp: u64,
    pub data: Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRecord {
    pub id: String,
    pub session_id: String,
    pub attempt_id: String,
    pub approval_id: String,
    pub capability: String,
    pub scope: String,
    pub mode: String,
    pub actor: String,
    pub expires_at: u64,
    pub state: String,
    pub consumed: bool,
}
fn grant_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GrantRecord> {
    Ok(GrantRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        attempt_id: row.get(2)?,
        approval_id: row.get(3)?,
        capability: row.get(4)?,
        scope: row.get(5)?,
        mode: row.get(6)?,
        actor: row.get(7)?,
        expires_at: read_u64(row, 8)?,
        state: row.get(9)?,
        consumed: row.get(10)?,
    })
}
pub(crate) enum GrantDecision {
    Allow(Box<GrantRecord>),
    Deny,
}
pub(crate) struct AuditDb {
    connection: Mutex<Connection>,
}
impl AuditDb {
    pub(crate) fn open(path: &Path) -> Result<Self, RuntimeError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        // An old side binding cannot be guessed into a full canonical checkpoint.
        // Preserve it for explicit host reconciliation instead of silently thawing.
        let old_bindings: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='runtime_bindings')", [], |row| row.get(0))?;
        if old_bindings {
            return Err(RuntimeError::InvalidInput("obsolete runtime plugin bindings require explicit host reconciliation and migration".into()));
        }
        connection.execute_batch(
            "BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS runtime_audit (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT NOT NULL,
                task TEXT, kind TEXT NOT NULL, timestamp INTEGER NOT NULL, payload TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_audit_session ON runtime_audit(session,seq);
            CREATE TABLE IF NOT EXISTS runtime_grants (
                id TEXT PRIMARY KEY, session TEXT NOT NULL, attempt TEXT NOT NULL,
                approval TEXT NOT NULL UNIQUE, capability TEXT NOT NULL, scope TEXT NOT NULL,
                mode TEXT NOT NULL, actor TEXT NOT NULL, expires INTEGER NOT NULL,
                state TEXT NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
            COMMIT;",
        )?;
        // An obsolete child-call table, if present, cannot silently disappear:
        // refuse stores with live records that require a host data migration.
        let old: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='runtime_capability_calls')",[],|r|r.get(0))?;
        if old {
            return Err(RuntimeError::InvalidInput("obsolete runtime capability journal requires explicit host reconciliation and migration".into()));
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
    pub(crate) fn record(
        &self,
        session: &str,
        task: Option<&str>,
        kind: &str,
        data: Value,
    ) -> Result<(), RuntimeError> {
        lock(&self.connection).execute("INSERT INTO runtime_audit(session,task,kind,timestamp,payload) VALUES (?1,?2,?3,?4,?5)",params![session,task,kind,sql_u64(timestamp_ms())?,serde_json::to_string(&data)?])?;
        Ok(())
    }
    pub(crate) fn events(
        &self,
        session: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, RuntimeError> {
        if limit == 0 || limit > 1000 {
            return Err(RuntimeError::InvalidInput(
                "audit page must contain 1..1000 events".into(),
            ));
        }
        let db = lock(&self.connection);
        let mut statement = db.prepare("SELECT seq,task,kind,timestamp,payload FROM runtime_audit WHERE session=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
        let rows = statement.query_map(params![session, sql_u64(after)?, limit as i64], |row| {
            Ok((
                read_u64(row, 0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                read_u64(row, 3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut records = vec![];
        for row in rows {
            let (cursor, task_id, kind, timestamp, payload) = row?;
            records.push(AuditRecord {
                cursor,
                session_id: session.into(),
                task_id,
                kind,
                timestamp,
                data: serde_json::from_str(&payload)?,
            });
        }
        Ok(records)
    }
    pub(crate) fn insert_grant(
        &self,
        approval: &cayw_core::ApprovalRequest,
        mode: &str,
        actor: &str,
        expires: u64,
    ) -> Result<GrantRecord, RuntimeError> {
        let record = GrantRecord {
            id: Uuid::new_v4().to_string(),
            session_id: approval.session_id.clone(),
            attempt_id: approval.attempt_id.clone(),
            approval_id: approval.id.clone(),
            capability: approval.input.capability.clone(),
            scope: approval.input.scope.clone(),
            mode: mode.into(),
            actor: actor.into(),
            expires_at: expires,
            state: "preparing".into(),
            consumed: false,
        };
        lock(&self.connection).execute("INSERT INTO runtime_grants(id,session,attempt,approval,capability,scope,mode,actor,expires,state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![record.id,record.session_id,record.attempt_id,record.approval_id,record.capability,record.scope,record.mode,record.actor,sql_u64(record.expires_at)?,record.state])?;
        Ok(record)
    }
    pub(crate) fn begin_decision(
        &self,
        approval: &cayw_core::ApprovalRequest,
        choice: &str,
        actor: &str,
    ) -> Result<u64, RuntimeError> {
        let db = lock(&self.connection);
        db.execute(
            "INSERT INTO runtime_audit(session,task,kind,timestamp,payload) VALUES (?1,?2,'approval_decision_started',?3,?4)",
            params![approval.session_id, approval.input.task_id, sql_u64(timestamp_ms())?, serde_json::to_string(&serde_json::json!({"approval_id":approval.id,"attempt_id":approval.attempt_id,"choice":choice,"actor":actor}))?],
        )?;
        u64::try_from(db.last_insert_rowid())
            .map_err(|_| RuntimeError::Worker("approval decision cursor is invalid".into()))
    }
    pub(crate) fn activate_grant(
        &self,
        id: &str,
        allow: bool,
        decision: u64,
    ) -> Result<(), RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let decision = sql_u64(decision)?;
        let n = tx.execute(
            "UPDATE runtime_grants SET state=?2 WHERE id=?1 AND state='preparing' AND EXISTS(SELECT 1 FROM runtime_audit a WHERE a.seq=?3 AND a.kind='approval_decision_started' AND a.session=runtime_grants.session AND json_extract(a.payload,'$.attempt_id')=runtime_grants.attempt AND json_extract(a.payload,'$.approval_id')=runtime_grants.approval)",
            params![id, if allow { "active" } else { "denied" }, decision],
        )?;
        if n != 1 {
            return Err(RuntimeError::StaleApproval);
        }
        // Permission and its completed decision become visible together. A
        // failed commit cannot leave an active grant without this evidence.
        tx.execute(
            "INSERT INTO runtime_audit(session,task,kind,timestamp,payload) SELECT session,task,'approval_decision_committed',?2,?3 FROM runtime_audit WHERE seq=?1",
            params![decision, sql_u64(timestamp_ms())?, serde_json::to_string(&serde_json::json!({"decision_cursor":decision,"grant_id":id}))?],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn decision_pending(&self, task: &str) -> Result<bool, RuntimeError> {
        Ok(lock(&self.connection).query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_audit s WHERE s.task=?1 AND s.kind='approval_decision_started' AND NOT EXISTS(SELECT 1 FROM runtime_audit c WHERE c.session=s.session AND c.task=s.task AND c.kind='approval_decision_committed' AND json_extract(c.payload,'$.decision_cursor')=s.seq))",
            [task],
            |row| row.get(0),
        )?)
    }
    pub(crate) fn grants(&self, session: &str) -> Result<Vec<GrantRecord>, RuntimeError> {
        let db = lock(&self.connection);
        let mut statement = db.prepare("SELECT id,session,attempt,approval,capability,scope,mode,actor,expires,state,consumed FROM runtime_grants WHERE session=?1 ORDER BY rowid")?;
        Ok(statement
            .query_map([session], |row| {
                Ok(GrantRecord {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    attempt_id: row.get(2)?,
                    approval_id: row.get(3)?,
                    capability: row.get(4)?,
                    scope: row.get(5)?,
                    mode: row.get(6)?,
                    actor: row.get(7)?,
                    expires_at: read_u64(row, 8)?,
                    state: row.get(9)?,
                    consumed: row.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub(crate) fn consume_grant(
        &self,
        lease: &AttemptLease,
        capability: &str,
        scope: &str,
        approval_id: Option<&str>,
    ) -> Result<Option<GrantDecision>, RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut statement = tx.prepare("SELECT id,session,attempt,approval,capability,scope,mode,actor,expires,state,consumed FROM runtime_grants WHERE session=?1 AND attempt=?2 AND capability=?3 AND scope=?4 AND consumed=0 AND ((mode='deny' AND state='denied') OR (expires>?5 AND ((mode='allow_task' AND state='active') OR approval=?6))) ORDER BY rowid DESC LIMIT 1")?;
        let record = statement
            .query_row(
                params![
                    lease.session_id,
                    lease.attempt_id,
                    capability,
                    scope,
                    sql_u64(timestamp_ms())?,
                    approval_id
                ],
                grant_row,
            )
            .optional()?;
        drop(statement);
        let result = match record {
            Some(record) if record.state == "preparing" => None,
            Some(record) if record.state != "active" => Some(GrantDecision::Deny),
            Some(mut record) => {
                if record.mode == "allow_once" {
                    tx.execute(
                        "UPDATE runtime_grants SET consumed=1 WHERE id=?1 AND consumed=0",
                        [&record.id],
                    )?;
                    record.consumed = true;
                }
                Some(GrantDecision::Allow(Box::new(record)))
            }
            None => None,
        };
        tx.commit()?;
        Ok(result)
    }
    /// Recheck the same admitted grant; a newer approval cannot replace it.
    pub(crate) fn check_grant(&self, expected: &GrantRecord) -> Result<(), RuntimeError> {
        let db = lock(&self.connection);
        let current = db.query_row(
            "SELECT id,session,attempt,approval,capability,scope,mode,actor,expires,state,consumed FROM runtime_grants WHERE id=?1",
            [&expected.id], grant_row,
        ).optional()?;
        if current.as_ref() != Some(expected)
            || expected.state != "active"
            || expected.expires_at <= timestamp_ms()
        {
            return Err(RuntimeError::StaleApproval);
        }
        Ok(())
    }
    pub(crate) fn revoke(&self, session: &str, id: &str, actor: &str) -> Result<(), RuntimeError> {
        let mut db = lock(&self.connection);
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute("UPDATE runtime_grants SET state='revoked' WHERE session=?1 AND id=?2 AND state IN ('active','preparing')",params![session,id])?;
        if changed != 1 {
            return Err(RuntimeError::InvalidInput("active grant not found".into()));
        }
        tx.execute("INSERT INTO runtime_audit(session,kind,timestamp,payload) VALUES (?1,'grant_revoked',?2,?3)",params![session,sql_u64(timestamp_ms())?,serde_json::to_string(&serde_json::json!({"grant_id":id,"actor":actor}))?])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn revoke_session(&self, session: &str, reason: &str) -> Result<(), RuntimeError> {
        lock(&self.connection).execute("UPDATE runtime_grants SET state='revoked' WHERE session=?1 AND state IN ('active','preparing')",[session])?;
        self.record(
            session,
            None,
            "grants_expired",
            serde_json::json!({"reason":reason}),
        )
    }
    /// The latest exact decision controls automatic result reads. Task cleanup
    /// also marks grants revoked, but only an explicit revoke has grant_revoked
    /// evidence. A later approval replaces an earlier denial or revocation.
    pub(crate) fn blocks_session_result_read(
        &self,
        session: &str,
        capability: &str,
        scope: &str,
    ) -> Result<bool, RuntimeError> {
        let db = lock(&self.connection);
        let decision = db.query_row(
            "SELECT g.state, EXISTS(SELECT 1 FROM runtime_audit a WHERE a.session=g.session AND a.kind='grant_revoked' AND json_extract(a.payload,'$.grant_id')=g.id) FROM runtime_grants g WHERE g.session=?1 AND g.capability=?2 AND g.scope=?3 ORDER BY g.rowid DESC LIMIT 1",
            params![session, capability, scope],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        ).optional()?;
        Ok(decision.is_some_and(|(state, explicitly_revoked)| {
            state == "denied" || state == "preparing" || explicitly_revoked
        }))
    }
}
