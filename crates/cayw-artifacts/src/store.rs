//! Session-scoped HTML artifact store with snapshot recovery (F09).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::identity::{validate_body, validate_id, validate_title};
use crate::types::{
    ArtifactError, ArtifactPreview, ArtifactRecord, PutArtifactRequest, UpdateArtifactRequest,
};

/// In-memory artifact catalog keyed by `artifact_id`.
#[derive(Debug, Default)]
pub struct ArtifactStore {
    inner: Mutex<StoreInner>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreInner {
    artifacts: BTreeMap<String, ArtifactRecord>,
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

    pub fn put(&self, req: PutArtifactRequest) -> Result<ArtifactRecord, ArtifactError> {
        validate_id("artifact_id", &req.artifact_id).map_err(ArtifactError::Invalid)?;
        validate_id("session_id", &req.session_id).map_err(ArtifactError::Invalid)?;
        let title = validate_title(&req.title).map_err(ArtifactError::Invalid)?;
        validate_body(&req.body).map_err(ArtifactError::Invalid)?;
        if let Some(handle) = &req.result_handle {
            validate_id("result_handle", handle).map_err(ArtifactError::Invalid)?;
        }

        let mut guard = self.inner.lock().expect("artifact store lock");
        if guard.artifacts.contains_key(&req.artifact_id) {
            return Err(ArtifactError::AlreadyExists(req.artifact_id));
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
        guard
            .artifacts
            .insert(record.artifact_id.clone(), record.clone());
        Ok(record)
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

        let mut guard = self.inner.lock().expect("artifact store lock");
        let existing = guard
            .artifacts
            .get_mut(&req.artifact_id)
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
        Ok(existing.clone())
    }

    pub fn get(&self, artifact_id: &str) -> Result<ArtifactRecord, ArtifactError> {
        validate_id("artifact_id", artifact_id).map_err(ArtifactError::Invalid)?;
        let guard = self.inner.lock().expect("artifact store lock");
        guard
            .artifacts
            .get(artifact_id)
            .cloned()
            .ok_or_else(|| ArtifactError::NotFound(artifact_id.to_owned()))
    }

    /// Renderer projection for UI / read-only consumers (not the storage record).
    pub fn preview(&self, artifact_id: &str) -> Result<ArtifactPreview, ArtifactError> {
        let record = self.get(artifact_id)?;
        Ok(ArtifactPreview::from_record(&record))
    }

    pub fn list_by_session(&self, session_id: &str) -> Result<Vec<ArtifactRecord>, ArtifactError> {
        validate_id("session_id", session_id).map_err(ArtifactError::Invalid)?;
        let guard = self.inner.lock().expect("artifact store lock");
        let mut out: Vec<_> = guard
            .artifacts
            .values()
            .filter(|r| r.session_id == session_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.artifact_id.cmp(&b.artifact_id))
        });
        Ok(out)
    }

    pub fn export_snapshot(&self) -> ArtifactSnapshot {
        let guard = self.inner.lock().expect("artifact store lock");
        ArtifactSnapshot {
            artifacts: guard.artifacts.values().cloned().collect(),
        }
    }

    pub fn restore_snapshot(&self, snapshot: ArtifactSnapshot) -> Result<(), ArtifactError> {
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
        let mut guard = self.inner.lock().expect("artifact store lock");
        guard.artifacts = map;
        Ok(())
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
