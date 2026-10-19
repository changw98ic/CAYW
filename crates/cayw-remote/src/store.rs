use crate::protocol::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, time::Duration};

pub(crate) struct Store {
    db: Connection,
    max_records: usize,
}
fn storage(_: rusqlite::Error) -> RemoteError {
    RemoteError::Storage
}

impl Store {
    pub(crate) fn open(
        path: &Path,
        operator: &Secret,
        limits: &Limits,
    ) -> Result<Self, RemoteError> {
        let db = Connection::open(path).map_err(storage)?;
        db.busy_timeout(Duration::from_secs(2)).map_err(storage)?;
        // One server owns this file. Exclusive locking is retained until close.
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA locking_mode=EXCLUSIVE;
            BEGIN EXCLUSIVE;
            CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS devices(id TEXT PRIMARY KEY,secret_hash TEXT NOT NULL,revoked INTEGER NOT NULL DEFAULT 0,generation INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS codes(hash TEXT PRIMARY KEY,expires INTEGER NOT NULL,used INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS bindings(id TEXT PRIMARY KEY,code_hash TEXT NOT NULL,secret_hash TEXT NOT NULL,device TEXT NOT NULL,expires INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS requests(device TEXT NOT NULL,id TEXT NOT NULL,hash TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(device,id));
            COMMIT;").map_err(storage)?;
        let operator_hash = hash(operator.expose().as_bytes());
        let old: Option<String> = db
            .query_row("SELECT value FROM metadata WHERE key='operator'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(storage)?;
        if let Some(old) = old {
            if old != operator_hash {
                return Err(RemoteError::Unauthorized);
            }
        } else {
            db.execute(
                "INSERT INTO metadata VALUES('operator',?1)",
                [&operator_hash],
            )
            .map_err(storage)?;
        }
        let store = Self {
            db,
            max_records: limits.max_records,
        };
        // Recover terminal uncertainty before accepting any connection.
        let mut stmt = store
            .db
            .prepare("SELECT record FROM requests")
            .map_err(storage)?;
        let records = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        drop(stmt);
        for record in records {
            let mut record: RequestRecord =
                serde_json::from_str(&record).map_err(|_| RemoteError::Storage)?;
            if !record.state.is_terminal() {
                record.state = RequestState::OutcomeUnknown;
                record.error = Some(RemoteError::OutcomeUnknown);
                store.update(&record)?;
            }
        }
        Ok(store)
    }
    pub(crate) fn operator_auth(&self, candidate: &str) -> bool {
        self.db
            .query_row("SELECT value FROM metadata WHERE key='operator'", [], |r| {
                r.get::<_, String>(0)
            })
            .is_ok_and(|stored| stored == hash(candidate.as_bytes()))
    }
    fn capacity(&self, table: &str) -> Result<(), RemoteError> {
        let sql = match table {
            "codes" => "SELECT COUNT(*) FROM codes",
            "devices" => "SELECT COUNT(*) FROM devices",
            "bindings" => "SELECT COUNT(*) FROM bindings",
            _ => "SELECT COUNT(*) FROM requests",
        };
        let count: i64 = self.db.query_row(sql, [], |r| r.get(0)).map_err(storage)?;
        if count >= self.max_records as i64 {
            return Err(RemoteError::Busy);
        }
        Ok(())
    }
    pub(crate) fn pair(&mut self, ttl: Duration) -> Result<(String, i64), RemoteError> {
        self.db
            .execute("DELETE FROM codes WHERE expires < ?1", [now_ms()?])
            .map_err(storage)?;
        self.capacity("codes")?;
        let code = random_id()?;
        let expires = deadline(ttl)?;
        self.db
            .execute(
                "INSERT INTO codes(hash,expires) VALUES(?1,?2)",
                params![hash(code.as_bytes()), expires],
            )
            .map_err(storage)?;
        Ok((code, expires))
    }
    pub(crate) fn bind(
        &mut self,
        id: &str,
        code: &str,
        secret: &str,
        ttl: Duration,
    ) -> Result<String, RemoteError> {
        if !valid_id(id) || Secret::new(code).is_err() || Secret::new(secret).is_err() {
            return Err(RemoteError::InvalidRequest);
        }
        let code_hash = hash(code.as_bytes());
        let secret_hash = hash(secret.as_bytes());
        let old: Option<(String, String, String, i64)> = self
            .db
            .query_row(
                "SELECT code_hash,secret_hash,device,expires FROM bindings WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(storage)?;
        if let Some((c, s, device, expires)) = old {
            if c != code_hash || s != secret_hash {
                return Err(RemoteError::Conflict);
            }
            if expires <= now_ms()? {
                return Err(RemoteError::Expired);
            }
            if self.device(&device)?.revoked {
                return Err(RemoteError::Revoked);
            }
            return Ok(device);
        }
        let code: Option<(i64, bool)> = self
            .db
            .query_row(
                "SELECT expires,used FROM codes WHERE hash=?1",
                [&code_hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(storage)?;
        let Some((expires, used)) = code else {
            return Err(RemoteError::Expired);
        };
        if used {
            return Err(RemoteError::Conflict);
        }
        if expires <= now_ms()? {
            return Err(RemoteError::Expired);
        }
        self.capacity("devices")?;
        self.capacity("bindings")?;
        let device = random_id()?;
        let tx = self.db.transaction().map_err(storage)?;
        tx.execute("UPDATE codes SET used=1 WHERE hash=?1", [&code_hash])
            .map_err(storage)?;
        tx.execute(
            "INSERT INTO devices(id,secret_hash) VALUES(?1,?2)",
            params![device, secret_hash],
        )
        .map_err(storage)?;
        tx.execute(
            "INSERT INTO bindings VALUES(?1,?2,?3,?4,?5)",
            params![id, code_hash, secret_hash, device, deadline(ttl)?],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(device)
    }
    pub(crate) fn device(&self, id: &str) -> Result<DeviceStatus, RemoteError> {
        self.db
            .query_row(
                "SELECT generation,revoked FROM devices WHERE id=?1",
                [id],
                |r| {
                    Ok(DeviceStatus {
                        device_id: id.into(),
                        generation: u64::try_from(r.get::<_, i64>(0)?)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        revoked: r.get(1)?,
                        connected: false,
                    })
                },
            )
            .optional()
            .map_err(storage)?
            .ok_or(RemoteError::NotFound)
    }
    pub(crate) fn device_auth(&self, id: &str, secret: &str) -> bool {
        self.db
            .query_row(
                "SELECT secret_hash FROM devices WHERE id=?1 AND revoked=0",
                [id],
                |r| r.get::<_, String>(0),
            )
            .is_ok_and(|s| s == hash(secret.as_bytes()))
    }
    pub(crate) fn next_generation(&self, device: &str) -> Result<u64, RemoteError> {
        let status = self.device(device)?;
        if status.revoked {
            return Err(RemoteError::Revoked);
        }
        let generation = status
            .generation
            .checked_add(1)
            .filter(|g| *g <= i64::MAX as u64)
            .ok_or(RemoteError::Storage)?;
        self.db
            .execute(
                "UPDATE devices SET generation=?1 WHERE id=?2",
                params![generation as i64, device],
            )
            .map_err(storage)?;
        Ok(generation)
    }
    pub(crate) fn revoke(&self, device: &str) -> Result<(), RemoteError> {
        self.device(device)?;
        self.db
            .execute("UPDATE devices SET revoked=1 WHERE id=?1", [device])
            .map_err(storage)?;
        Ok(())
    }
    pub(crate) fn request(
        &self,
        device: &str,
        id: &str,
    ) -> Result<(String, RequestRecord), RemoteError> {
        let value: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT hash,record FROM requests WHERE device=?1 AND id=?2",
                params![device, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(storage)?;
        let (hash, record) = value.ok_or(RemoteError::NotFound)?;
        Ok((
            hash,
            serde_json::from_str(&record).map_err(|_| RemoteError::Storage)?,
        ))
    }
    pub(crate) fn insert(&self, hash: &str, record: &RequestRecord) -> Result<(), RemoteError> {
        self.capacity("requests")?;
        let json = serde_json::to_string(record).map_err(|_| RemoteError::Storage)?;
        self.db
            .execute(
                "INSERT INTO requests VALUES(?1,?2,?3,?4)",
                params![record.device_id, record.request_id, hash, json],
            )
            .map_err(storage)?;
        Ok(())
    }
    pub(crate) fn update(&self, record: &RequestRecord) -> Result<(), RemoteError> {
        let json = serde_json::to_string(record).map_err(|_| RemoteError::Storage)?;
        self.db
            .execute(
                "UPDATE requests SET record=?1 WHERE device=?2 AND id=?3",
                params![json, record.device_id, record.request_id],
            )
            .map_err(storage)?;
        Ok(())
    }
}
