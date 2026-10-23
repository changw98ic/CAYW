//! Explicit test-build checkpoints. Ordinary builds contain no fault controller.
use serde_json::Value;
use std::sync::{Arc, OnceLock};

type Observer = dyn Fn(&'static str, Value) + Send + Sync;
static OBSERVER: OnceLock<Arc<Observer>> = OnceLock::new();

/// Install one trusted test observer before starting the Runtime. A checkpoint
/// may block its own test process; it never selects a production action.
pub fn install(observer: Arc<Observer>) -> Result<(), &'static str> {
    OBSERVER
        .set(observer)
        .map_err(|_| "fault observer already installed")
}

pub fn hit(point: &'static str, data: Value) {
    if let Some(observer) = OBSERVER.get() {
        observer(point, data);
    }
}
