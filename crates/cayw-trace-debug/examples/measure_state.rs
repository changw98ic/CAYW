//! Compare browser state transfer against the full capture on a copied store.
use cayw_trace_debug::TraceStore;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("expected copied trace path")?;
    let store = TraceStore::open(&path)?;
    let started = Instant::now();
    let capture = serde_json::to_vec(&store.snapshot())?;
    let capture_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let initial = store.state_update(None);
    let initial_bytes = serde_json::to_vec(&initial)?.len();
    let initial_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let idle = serde_json::to_vec(&store.state_update(Some(initial.sequence)))?;
    let idle_ms = started.elapsed().as_secs_f64() * 1000.0;
    let event = cayw_core::EventRecord {
        protocol_version: cayw_core::PROTOCOL_VERSION,
        session_id: "trace-storage-benchmark".into(),
        cursor: store.session_cursor("trace-storage-benchmark") + 1,
        timestamp: cayw_trace_debug::now_ms(),
        attempt: None,
        event: cayw_core::CoreEvent::Host {
            kind: "benchmark".into(),
            data: serde_json::json!({"append":true}),
        },
    };
    let before_bytes = std::fs::metadata(&path)?.len();
    let started = Instant::now();
    store.ingest(std::slice::from_ref(&event))?;
    let append_ms = started.elapsed().as_secs_f64() * 1000.0;
    let after_bytes = std::fs::metadata(&path)?.len();
    let expected = store.snapshot().events.len();
    drop(store);
    let reopened = TraceStore::open(&path)?;
    assert_eq!(reopened.snapshot().events.len(), expected);
    assert_eq!(reopened.event(&event.session_id, event.cursor), Some(event));
    println!(
        "{}",
        serde_json::json!({
            "capture_bytes":capture.len(), "capture_ms":capture_ms,
            "initial_bytes":initial_bytes, "initial_ms":initial_ms,
            "unchanged_bytes":idle.len(), "unchanged_ms":idle_ms,
            "projects":initial.projects.len(), "sequence":initial.sequence,
            "append_ms":append_ms,"database_bytes_before":before_bytes,"database_bytes_after":after_bytes,"reopened_events":expected
        })
    );
    Ok(())
}
