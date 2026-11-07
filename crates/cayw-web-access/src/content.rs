//! Citation metadata and either Device-backed or standalone process-local bodies.

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
    handle_view: Option<serde_json::Value>,
}

/// External sink hook for authoritative Device content / result storage.
pub trait ContentSink: Send + Sync {
    fn put(
        &self,
        id: &str,
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> Result<serde_json::Value, WebAccessError>;
    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, WebAccessError>;
}

/// Process-local store of fetched / extracted bodies keyed by `ContentRef.id`.
#[derive(Clone, Default)]
pub struct ContentStore {
    inner: Arc<Mutex<Inner>>,
    sink: Option<Arc<dyn ContentSink>>,
}

impl std::fmt::Debug for ContentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentStore")
            .field("inner", &self.inner)
            .field("has_sink", &self.sink.is_some())
            .finish()
    }
}

impl ContentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_sink(sink: Arc<dyn ContentSink>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            sink: Some(sink),
        }
    }

    pub fn put_text(
        &self,
        kind: ContentKind,
        text: impl Into<String>,
        mime_type: Option<String>,
        source_url: Option<String>,
        title: Option<String>,
    ) -> Result<ContentRef, WebAccessError> {
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
    ) -> Result<ContentRef, WebAccessError> {
        let content_sha256 = hex_sha256(&bytes);
        let byte_len = bytes.len() as u64;
        let (id, handle_view, stored_text, stored_bytes) = if let Some(sink) = &self.sink {
            let identity = serde_json::to_vec(&(kind, mime_type.as_deref(), &content_sha256))
                .expect("web content identity serializes");
            let view = sink.put(&hex_sha256(&identity), &bytes, mime_type.as_deref())?;
            let id = view
                .get("handle")
                .and_then(|h| h.as_str())
                .ok_or_else(|| {
                    WebAccessError::new(
                        FailureClass::ContentStorage,
                        "content sink returned no result handle",
                    )
                })?
                .to_string();
            // When sink is active, do not retain redundant text or bytes copies in memory.
            (id, Some(view), None, Vec::new())
        } else {
            let mut guard = self.inner.lock().expect("content store lock");
            guard.next += 1;
            (format!("webc-{}", guard.next), None, text, bytes)
        };
        let meta = ContentRef {
            id: id.clone(),
            kind,
            content_sha256,
            byte_len,
            mime_type: mime_type.clone(),
            source_url,
            title,
        };
        let mut guard = self.inner.lock().expect("content store lock");
        guard.bodies.insert(
            id,
            StoredBody {
                meta: meta.clone(),
                text: stored_text,
                bytes: stored_bytes,
                handle_view,
            },
        );
        Ok(meta)
    }

    pub fn get_handle_view(&self, id: &str) -> Option<serde_json::Value> {
        let guard = self.inner.lock().expect("content store lock");
        guard.bodies.get(id).and_then(|b| b.handle_view.clone())
    }

    /// Evict cached body text/bytes to retain only ContentRef metadata.
    pub fn evict_body(&self, id: &str) {
        if let Ok(mut guard) = self.inner.lock()
            && let Some(body) = guard.bodies.get_mut(id)
        {
            body.text = None;
            body.bytes = Vec::new();
        }
    }

    pub fn get_meta(&self, id: &str) -> Result<ContentRef, WebAccessError> {
        let guard = self.inner.lock().expect("content store lock");
        guard.bodies.get(id).map(|b| b.meta.clone()).ok_or_else(|| {
            WebAccessError::new(FailureClass::NotFound, format!("unknown content ref {id}"))
        })
    }

    pub fn get_text(&self, id: &str) -> Result<String, WebAccessError> {
        if let Some(sink) = &self.sink {
            let bytes = sink.get(id)?.ok_or_else(|| {
                WebAccessError::new(FailureClass::NotFound, format!("unknown content ref {id}"))
            })?;
            return String::from_utf8(bytes).map_err(|_| {
                WebAccessError::new(
                    FailureClass::ExtractFailed,
                    "content body is not UTF-8 text",
                )
            });
        }
        let (cached_text, cached_bytes) = {
            let guard = self.inner.lock().expect("content store lock");
            let body = guard.bodies.get(id).ok_or_else(|| {
                WebAccessError::new(FailureClass::NotFound, format!("unknown content ref {id}"))
            })?;
            (body.text.clone(), body.bytes.clone())
        };
        if let Some(text) = cached_text {
            return Ok(text);
        }
        if !cached_bytes.is_empty() {
            return String::from_utf8(cached_bytes).map_err(|_| {
                WebAccessError::new(
                    FailureClass::ExtractFailed,
                    "content body is not UTF-8 text",
                )
            });
        }
        Err(WebAccessError::new(
            FailureClass::NotFound,
            format!("content body for {id} was evicted and unavailable in sink"),
        ))
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
        let meta = store
            .put_text(
                ContentKind::Text,
                "hello",
                Some("text/plain".into()),
                Some("https://example.com".into()),
                Some("Hello".into()),
            )
            .unwrap();
        assert_eq!(meta.byte_len, 5);
        assert_eq!(store.get_text(&meta.id).unwrap(), "hello");
        assert_eq!(
            store.get_meta(&meta.id).unwrap().title.as_deref(),
            Some("Hello")
        );
    }

    struct MockSink(Mutex<HashMap<String, Vec<u8>>>);
    impl ContentSink for MockSink {
        fn put(
            &self,
            id: &str,
            bytes: &[u8],
            _mime_type: Option<&str>,
        ) -> Result<serde_json::Value, WebAccessError> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_string(), bytes.to_vec());
            Ok(serde_json::json!({"handle": id, "stored":true, "released":false}))
        }
        fn get(&self, id: &str) -> Result<Option<Vec<u8>>, WebAccessError> {
            Ok(self.0.lock().unwrap().get(id).cloned())
        }
    }

    struct FailedSink {
        unknown: bool,
    }
    impl ContentSink for FailedSink {
        fn put(
            &self,
            _: &str,
            _: &[u8],
            _: Option<&str>,
        ) -> Result<serde_json::Value, WebAccessError> {
            let mut error =
                WebAccessError::new(FailureClass::ContentStorage, "injected sink commit failure");
            error.outcome_unknown = self.unknown;
            Err(error)
        }
        fn get(&self, _: &str) -> Result<Option<Vec<u8>>, WebAccessError> {
            Err(WebAccessError::new(
                FailureClass::ContentStorage,
                "injected sink read failure",
            ))
        }
    }

    #[test]
    fn sink_failures_never_return_a_process_local_success() {
        for unknown in [false, true] {
            let store = ContentStore::with_sink(Arc::new(FailedSink { unknown }));
            let error = store
                .put_text(
                    ContentKind::Text,
                    "body",
                    Some("text/plain".into()),
                    None,
                    None,
                )
                .unwrap_err();
            assert_eq!(error.diagnostic.class, FailureClass::ContentStorage);
            assert_eq!(error.outcome_unknown, unknown);
            assert!(store.is_empty());
        }
    }

    #[test]
    fn authoritative_sink_refusal_is_not_bypassed_by_cached_text() {
        let sink = Arc::new(MockSink(Mutex::new(HashMap::new())));
        let store = ContentStore::with_sink(sink.clone());
        let meta = store
            .put_text(
                ContentKind::Text,
                "cached secret",
                Some("text/plain".into()),
                None,
                None,
            )
            .unwrap();
        sink.0.lock().unwrap().remove(&meta.id);
        assert!(store.get_text(&meta.id).is_err());
    }

    #[test]
    fn equal_bytes_with_different_content_kinds_have_distinct_handles() {
        let sink = Arc::new(MockSink(Mutex::new(HashMap::new())));
        let store = ContentStore::with_sink(sink);
        let html = store
            .put_text(
                ContentKind::Html,
                "same text",
                Some("text/html".into()),
                None,
                None,
            )
            .unwrap();
        let text = store
            .put_text(
                ContentKind::Text,
                "same text",
                Some("text/html".into()),
                None,
                None,
            )
            .unwrap();
        assert_ne!(html.id, text.id);
        assert_eq!(html.content_sha256, text.content_sha256);
        assert_eq!(store.get_meta(&html.id).unwrap().kind, ContentKind::Html);
        assert_eq!(store.get_meta(&text.id).unwrap().kind, ContentKind::Text);
    }

    #[test]
    fn sink_forward_and_fallback_on_eviction() {
        let sink = Arc::new(MockSink(Mutex::new(HashMap::new())));
        let store = ContentStore::with_sink(sink.clone());
        let meta = store
            .put_text(
                ContentKind::Text,
                "authoritative device content",
                Some("text/plain".into()),
                Some("https://example.com".into()),
                Some("Title".into()),
            )
            .unwrap();
        // Verify sink received content
        assert_eq!(
            sink.0.lock().unwrap().get(&meta.id).unwrap(),
            b"authoritative device content"
        );
        // Verify sink mode does not store text or bytes duplicate in process memory
        {
            let guard = store.inner.lock().unwrap();
            let body = guard.bodies.get(&meta.id).unwrap();
            assert!(body.text.is_none());
            assert!(body.bytes.is_empty());
        }
        // Evict cached body from local store
        store.evict_body(&meta.id);
        // Read through store falls back to reading from sink
        assert_eq!(
            store.get_text(&meta.id).unwrap(),
            "authoritative device content"
        );
    }

    #[test]
    fn standalone_mode_retains_body_and_supports_eviction() {
        let store = ContentStore::new();
        let meta = store
            .put_text(
                ContentKind::Text,
                "standalone text",
                Some("text/plain".into()),
                None,
                None,
            )
            .unwrap();
        {
            let guard = store.inner.lock().unwrap();
            let body = guard.bodies.get(&meta.id).unwrap();
            assert_eq!(body.text.as_deref(), Some("standalone text"));
            assert_eq!(body.bytes.as_slice(), b"standalone text");
        }
        assert_eq!(store.get_text(&meta.id).unwrap(), "standalone text");
        store.evict_body(&meta.id);
        {
            let guard = store.inner.lock().unwrap();
            let body = guard.bodies.get(&meta.id).unwrap();
            assert!(body.text.is_none());
            assert!(body.bytes.is_empty());
            assert_eq!(body.bytes.capacity(), 0);
        }
        assert!(store.get_text(&meta.id).is_err());
    }
}
