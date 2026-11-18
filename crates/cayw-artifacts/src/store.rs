//! Session-scoped HTML artifact store with snapshot recovery (F09).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::identity::{validate_body, validate_id, validate_title};
use crate::types::{
    ArtifactError, ArtifactPreview, ArtifactRecord, ArtifactSummary, PutArtifactRequest,
    UpdateArtifactRequest,
};

/// Session-owned artifact catalog. Runtime opens the on-disk database lazily.
#[derive(Debug, Default)]
pub struct ArtifactStore {
    path: Option<PathBuf>,
    connection: Mutex<Option<Connection>>,
}

fn storage_error(error: impl std::fmt::Display) -> ArtifactError {
    ArtifactError::Io(error.to_string())
}

fn lookup(
    connection: &Connection,
    id: &str,
    session: Option<&str>,
) -> Result<Option<ArtifactRecord>, ArtifactError> {
    let body: Option<String> = connection
        .query_row(
            "SELECT body FROM artifact_cards WHERE id=?1 AND (?2 IS NULL OR session_id=?2)",
            params![id, session],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    body.map(|body| serde_json::from_str(&body).map_err(storage_error))
        .transpose()
}

fn read_rows(
    connection: &Connection,
    session: Option<&str>,
) -> Result<Vec<ArtifactRecord>, ArtifactError> {
    let mut query = match session {
        Some(_) => connection.prepare("SELECT body FROM artifact_cards WHERE session_id=?1"),
        None => connection.prepare("SELECT body FROM artifact_cards ORDER BY id"),
    }
    .map_err(storage_error)?;
    let mut rows = match session {
        Some(id) => query.query([id]),
        None => query.query([]),
    }
    .map_err(storage_error)?;
    let mut artifacts = Vec::new();
    while let Some(row) = rows.next().map_err(storage_error)? {
        let body: String = row.get(0).map_err(storage_error)?;
        artifacts.push(serde_json::from_str(&body).map_err(storage_error)?);
    }
    Ok(artifacts)
}

fn insert(connection: &Connection, record: &ArtifactRecord) -> Result<(), ArtifactError> {
    let body = serde_json::to_string(record).map_err(storage_error)?;
    connection
        .execute(
            "INSERT INTO artifact_cards(id,session_id,body) VALUES(?1,?2,?3)",
            params![record.artifact_id, record.session_id, body],
        )
        .map_err(storage_error)?;
    Ok(())
}

/// Portable snapshot for process-restart recovery tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSnapshot {
    pub artifacts: Vec<ArtifactRecord>,
}

impl ArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn durable(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            connection: Mutex::new(None),
        }
    }

    fn with_db<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, ArtifactError>,
    ) -> Result<T, ArtifactError> {
        let mut database = self.connection.lock().expect("artifact store lock");
        if database.is_none() {
            let connection = match &self.path {
                Some(path) => Connection::open(path),
                None => Connection::open_in_memory(),
            }
            .map_err(storage_error)?;
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS artifact_cards(
                    id TEXT PRIMARY KEY NOT NULL,
                    session_id TEXT NOT NULL,
                    body TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS artifact_cards_by_session ON artifact_cards(session_id);"
            ).map_err(storage_error)?;
            *database = Some(connection);
        }
        operation(database.as_mut().expect("artifact database opened"))
    }

    pub fn put(&self, req: PutArtifactRequest) -> Result<ArtifactRecord, ArtifactError> {
        validate_id("artifact_id", &req.artifact_id).map_err(ArtifactError::Invalid)?;
        validate_id("session_id", &req.session_id).map_err(ArtifactError::Invalid)?;
        let title = validate_title(&req.title).map_err(ArtifactError::Invalid)?;
        validate_body(&req.body).map_err(ArtifactError::Invalid)?;
        if let Some(handle) = &req.result_handle {
            validate_id("result_handle", handle).map_err(ArtifactError::Invalid)?;
        }

        let record = ArtifactRecord {
            artifact_id: req.artifact_id.clone(),
            session_id: req.session_id,
            kind: req.kind,
            title,
            body: req.body,
            content_version: 1,
            created_at_ms: req.at_ms,
            updated_at_ms: req.at_ms,
            result_handle: req.result_handle,
        };
        self.with_db(|connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            if lookup(&tx, &record.artifact_id, None)?.is_some() {
                return Err(ArtifactError::AlreadyExists(record.artifact_id));
            }
            insert(&tx, &record)?;
            tx.commit().map_err(storage_error)?;
            Ok(record)
        })
    }

    pub fn update(&self, req: UpdateArtifactRequest) -> Result<ArtifactRecord, ArtifactError> {
        validate_id("artifact_id", &req.artifact_id).map_err(ArtifactError::Invalid)?;
        validate_id("session_id", &req.session_id).map_err(ArtifactError::Invalid)?;
        if req.title.is_none() && req.body.is_none() {
            return Err(ArtifactError::Invalid(
                "update requires title and/or body".into(),
            ));
        }
        // Validate proposed fields before touching the stored record so a later
        // failure (bad body / version overflow) cannot leave a mutated title.
        let new_title = match &req.title {
            Some(title) => Some(validate_title(title).map_err(ArtifactError::Invalid)?),
            None => None,
        };
        if let Some(body) = &req.body {
            validate_body(body).map_err(ArtifactError::Invalid)?;
        }

        self.with_db(|connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            let mut existing = lookup(&tx, &req.artifact_id, None)?
                .ok_or_else(|| ArtifactError::NotFound(req.artifact_id.clone()))?;
            if existing.session_id != req.session_id {
                return Err(ArtifactError::SessionMismatch {
                    artifact_id: req.artifact_id,
                    owner: existing.session_id.clone(),
                    claimed: req.session_id,
                });
            }
            if let Some(expected) = req.expected_content_version
                && expected != existing.content_version
            {
                return Err(ArtifactError::VersionMismatch {
                    artifact_id: req.artifact_id,
                    expected,
                    actual: existing.content_version,
                });
            }
            let next_version = existing
                .content_version
                .checked_add(1)
                .ok_or_else(|| ArtifactError::Invalid("content_version overflow".into()))?;
            if let Some(title) = new_title {
                existing.title = title;
            }
            if let Some(body) = req.body {
                existing.body = body;
            }
            existing.content_version = next_version;
            existing.updated_at_ms = req.at_ms;
            let body = serde_json::to_string(&existing).map_err(storage_error)?;
            tx.execute(
                "UPDATE artifact_cards SET body=?1 WHERE id=?2",
                params![body, existing.artifact_id],
            )
            .map_err(storage_error)?;
            tx.commit().map_err(storage_error)?;
            Ok(existing)
        })
    }

    pub fn get(&self, artifact_id: &str) -> Result<ArtifactRecord, ArtifactError> {
        validate_id("artifact_id", artifact_id).map_err(ArtifactError::Invalid)?;
        self.with_db(|connection| {
            lookup(connection, artifact_id, None)?
                .ok_or_else(|| ArtifactError::NotFound(artifact_id.to_owned()))
        })
    }

    /// Renderer projection for UI / read-only consumers (not the storage record).
    pub fn preview(&self, artifact_id: &str) -> Result<ArtifactPreview, ArtifactError> {
        let record = self.get(artifact_id)?;
        Ok(ArtifactPreview::from_record(&record))
    }

    pub fn preview_for_session(
        &self,
        artifact_id: &str,
        session_id: &str,
    ) -> Result<ArtifactPreview, ArtifactError> {
        validate_id("artifact_id", artifact_id).map_err(ArtifactError::Invalid)?;
        self.with_db(|connection| {
            let record = lookup(connection, artifact_id, Some(session_id))?
                .ok_or_else(|| ArtifactError::NotFound(artifact_id.to_owned()))?;
            Ok(ArtifactPreview::from_record(&record))
        })
    }

    pub fn list_by_session(&self, session_id: &str) -> Result<Vec<ArtifactRecord>, ArtifactError> {
        validate_id("session_id", session_id).map_err(ArtifactError::Invalid)?;
        let mut out = self.with_db(|connection| read_rows(connection, Some(session_id)))?;
        out.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.artifact_id.cmp(&b.artifact_id))
        });
        Ok(out)
    }

    pub fn summaries(&self, session_id: &str) -> Result<Vec<ArtifactSummary>, ArtifactError> {
        self.with_db(|connection| {
            let mut query = connection
                .prepare(
                    "SELECT json_remove(body, '$.body') FROM artifact_cards WHERE session_id=?1
                 ORDER BY json_extract(body, '$.created_at_ms'), id",
                )
                .map_err(storage_error)?;
            query
                .query_map([session_id], |row| row.get::<_, String>(0))
                .map_err(storage_error)?
                .map(|row| {
                    serde_json::from_str(&row.map_err(storage_error)?).map_err(storage_error)
                })
                .collect()
        })
    }

    pub fn export_snapshot(&self) -> Result<ArtifactSnapshot, ArtifactError> {
        Ok(ArtifactSnapshot {
            artifacts: self.with_db(|connection| read_rows(connection, None))?,
        })
    }

    pub(crate) fn prepare_snapshot(
        snapshot: ArtifactSnapshot,
    ) -> Result<BTreeMap<String, ArtifactRecord>, ArtifactError> {
        let mut map = BTreeMap::new();
        for record in snapshot.artifacts {
            validate_id("artifact_id", &record.artifact_id).map_err(ArtifactError::Invalid)?;
            validate_id("session_id", &record.session_id).map_err(ArtifactError::Invalid)?;
            let _ = validate_title(&record.title).map_err(ArtifactError::Invalid)?;
            validate_body(&record.body).map_err(ArtifactError::Invalid)?;
            if record.content_version == 0 {
                return Err(ArtifactError::Invalid(
                    "content_version must be >= 1".into(),
                ));
            }
            if map.insert(record.artifact_id.clone(), record).is_some() {
                return Err(ArtifactError::Invalid(
                    "snapshot contains duplicate artifact_id".into(),
                ));
            }
        }
        Ok(map)
    }

    pub(crate) fn commit_prepared(
        &self,
        artifacts: BTreeMap<String, ArtifactRecord>,
    ) -> Result<(), ArtifactError> {
        self.with_db(|connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            tx.execute("DELETE FROM artifact_cards", [])
                .map_err(storage_error)?;
            for record in artifacts.values() {
                insert(&tx, record)?;
            }
            tx.commit().map_err(storage_error)
        })
    }

    pub fn restore_snapshot(&self, snapshot: ArtifactSnapshot) -> Result<(), ArtifactError> {
        self.commit_prepared(Self::prepare_snapshot(snapshot)?)
    }
}

/// Shared handle used by the action protocol.
pub type SharedArtifactStore = Arc<ArtifactStore>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ArtifactKind;

    #[test]
    fn failed_update_does_not_mutate_title() {
        let store = ArtifactStore::new();
        store
            .put(PutArtifactRequest {
                artifact_id: "card-1".into(),
                session_id: "sess-a".into(),
                kind: ArtifactKind::HtmlCard,
                title: "Original".into(),
                body: "<p>hello</p>".into(),
                at_ms: 100,
                result_handle: None,
            })
            .unwrap();
        let err = store
            .update(UpdateArtifactRequest {
                artifact_id: "card-1".into(),
                session_id: "sess-a".into(),
                title: Some("Would-Be-New".into()),
                body: Some("".into()),
                at_ms: 200,
                expected_content_version: Some(1),
            })
            .expect_err("empty body must fail the update");
        assert!(
            matches!(err, ArtifactError::Invalid(ref msg) if msg.contains("body")),
            "expected body validation error, got {err:?}"
        );
        let got = store.get("card-1").unwrap();
        assert_eq!(
            got.title, "Original",
            "failed update must not leave a mutated title"
        );
        assert_eq!(got.body, "<p>hello</p>");
        assert_eq!(got.content_version, 1);
        assert_eq!(got.updated_at_ms, 100);
    }

    #[test]
    fn put_preview_and_versioned_update() {
        let store = ArtifactStore::new();
        let put = store
            .put(PutArtifactRequest {
                artifact_id: "card-1".into(),
                session_id: "sess-a".into(),
                kind: ArtifactKind::HtmlCard,
                title: "Summary".into(),
                body: "<p>hello</p>".into(),
                at_ms: 100,
                result_handle: None,
            })
            .unwrap();
        assert_eq!(put.content_version, 1);
        let preview = store.preview("card-1").unwrap();
        assert_eq!(preview.html, "<p>hello</p>");
        assert_eq!(preview.content_type, ArtifactPreview::HTML_CONTENT_TYPE);
        let updated = store
            .update(UpdateArtifactRequest {
                artifact_id: "card-1".into(),
                session_id: "sess-a".into(),
                title: None,
                body: Some("<p>world</p>".into()),
                at_ms: 200,
                expected_content_version: Some(1),
            })
            .unwrap();
        assert_eq!(updated.content_version, 2);
        assert_eq!(updated.body, "<p>world</p>");
    }
}
