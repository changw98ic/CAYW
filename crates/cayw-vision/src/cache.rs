//! Content-addressed vision answer cache.
//!
//! Cache hits must not increment billable backend counters. Keys cover the
//! operation name plus image / prompt digests so OCR and describe never collide.

use crate::types::VisionError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

/// Cache lookup key (stable across process restarts when digests match).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheKey {
    pub operation: String,
    pub image_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_sha256: Option<String>,
}

/// Hit / miss counters for diagnostics (not a billing ledger).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
}

#[derive(Debug, Clone)]
struct Entry {
    payload: String,
}

/// In-memory LRU-ish answer cache (evicts oldest insertion when full).
pub struct AnswerCache {
    inner: Mutex<Inner>,
    max_entries: usize,
}

#[derive(Debug, Default)]
struct Inner {
    map: HashMap<CacheKey, Entry>,
    order: Vec<CacheKey>,
    hits: u64,
    misses: u64,
}

impl AnswerCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_entries: max_entries.max(1),
        }
    }

    pub fn get(&self, key: &CacheKey) -> Result<Option<String>, VisionError> {
        let mut guard = self.inner.lock().map_err(|_| VisionError::CachePoisoned)?;
        if let Some(entry) = guard.map.get(key).map(|e| e.payload.clone()) {
            guard.hits = guard.hits.saturating_add(1);
            return Ok(Some(entry));
        }
        guard.misses = guard.misses.saturating_add(1);
        Ok(None)
    }

    pub fn put(&self, key: CacheKey, payload: String) -> Result<(), VisionError> {
        let mut guard = self.inner.lock().map_err(|_| VisionError::CachePoisoned)?;
        if let std::collections::hash_map::Entry::Occupied(mut occupied) =
            guard.map.entry(key.clone())
        {
            occupied.insert(Entry { payload });
            return Ok(());
        }
        while guard.map.len() >= self.max_entries {
            if let Some(old) = guard.order.first().cloned() {
                guard.order.remove(0);
                guard.map.remove(&old);
            } else {
                break;
            }
        }
        guard.order.push(key.clone());
        guard.map.insert(key, Entry { payload });
        Ok(())
    }

    pub fn stats(&self) -> Result<CacheStats, VisionError> {
        let guard = self.inner.lock().map_err(|_| VisionError::CachePoisoned)?;
        Ok(CacheStats {
            hits: guard.hits,
            misses: guard.misses,
            entries: guard.map.len(),
        })
    }
}

impl Default for AnswerCache {
    fn default() -> Self {
        Self::new(200)
    }
}
