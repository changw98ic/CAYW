//! In-memory content reference store for citations (F10 first cut).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::error::WebAccessError;
use crate::types::{ContentKind, ContentRef, FailureClass};

#[derive(Debug, Default)]
struct Inner {
    next: u64,
    bodies: HashMap<String, StoredBody>,
}

#[derive(Debug, Clone)]
struct StoredBody {
    meta: ContentRef,
    text: Option<String>,
    bytes: Vec<u8>,
}

/// Process-local store of fetched / extracted bodies keyed by `ContentRef.id`.
#[derive(Debug, Clone, Default)]
pub struct ContentStore {
    inner: Arc<Mutex<Inner>>,
}

impl ContentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_text(
        &self,
        kind: ContentKind,
        text: impl Into<String>,
        mime_type: Option<String>,
        source_url: Option<String>,
        title: Option<String>,
    ) -> ContentRef {
        let text = text.into();
        let bytes = text.as_bytes().to_vec();
        self.put_bytes(kind, bytes, Some(text), mime_type, source_url, title)
    }

    pub fn put_bytes(
        &self,
        kind: ContentKind,
        bytes: Vec<u8>,
        text: Option<String>,
        mime_type: Option<String>,
        source_url: Option<String>,
        title: Option<String>,
    ) -> ContentRef {
        let mut guard = self.inner.lock().expect("content store lock");
        guard.next += 1;
        let id = format!("webc-{}", guard.next);
        let content_sha256 = hex_sha256(&bytes);
        let meta = ContentRef {
            id: id.clone(),
            kind,
            content_sha256,
            byte_len: bytes.len() as u64,
            mime_type,
            source_url,
            title,
        };
        guard.bodies.insert(
            id,
            StoredBody {
                meta: meta.clone(),
                text,
                bytes,
            },
        );
        meta
    }

    pub fn get_meta(&self, id: &str) -> Result<ContentRef, WebAccessError> {
        let guard = self.inner.lock().expect("content store lock");
        guard
            .bodies
            .get(id)
            .map(|b| b.meta.clone())
            .ok_or_else(|| WebAccessError::new(FailureClass::NotFound, format!("unknown content ref {id}")))
    }

    pub fn get_text(&self, id: &str) -> Result<String, WebAccessError> {
        let guard = self.inner.lock().expect("content store lock");
        let body = guard
            .bodies
            .get(id)
            .ok_or_else(|| WebAccessError::new(FailureClass::NotFound, format!("unknown content ref {id}")))?;
        if let Some(text) = &body.text {
            return Ok(text.clone());
        }
        String::from_utf8(body.bytes.clone()).map_err(|_| {
            WebAccessError::new(FailureClass::ExtractFailed, "content body is not UTF-8 text")
        })
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("content store lock").bodies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_get_roundtrip() {
        let store = ContentStore::new();
        let meta = store.put_text(
            ContentKind::Text,
            "hello",
            Some("text/plain".into()),
            Some("https://example.com".into()),
            Some("Hello".into()),
        );
        assert_eq!(meta.byte_len, 5);
        assert_eq!(store.get_text(&meta.id).unwrap(), "hello");
        assert_eq!(store.get_meta(&meta.id).unwrap().title.as_deref(), Some("Hello"));
    }
}
