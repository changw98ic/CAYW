use cayw_ai::{AssistantMessage, Content, Model};
use cayw_trace_debug::{Document, TraceStore};
use serde_json::json;

fn old_document() -> serde_json::Value {
    let mut message = AssistantMessage::new(&Model::new("fixture", "migration"));
    message.content.push(Content::text("retained output"));
    let mut document = Document::default();
    document.sessions.entry("session".into()).or_default().after = 1;
    let mut raw = serde_json::to_value(document).unwrap();
    raw["events"] = json!([{
        "protocol_version":3,"session_id":"session","cursor":1,"timestamp":10,"attempt":null,
        "event":{"type":"agent","event":{"type":"message_update","message":message,
            "event":{"type":"text_delta","index":0,"delta":"output","partial":message}}}
    }]);
    raw
}

#[test]
fn v3_trace_preserves_exact_evidence_and_deduplicates_canonical_core_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    let raw = old_document();
    let bytes = serde_json::to_vec_pretty(&raw).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let canonical = cayw_core::migrate_v3_event_json(&raw["events"][0].to_string()).unwrap();
    let store = TraceStore::open(&path).unwrap();
    assert_eq!(store.snapshot().events, vec![canonical.clone()]);
    store.ingest(std::slice::from_ref(&canonical)).unwrap();
    assert_eq!(store.snapshot().events.len(), 1);
    assert_eq!(
        std::fs::read(path.with_extension("v3-evidence.json")).unwrap(),
        bytes
    );
    drop(store);
    let reopened = TraceStore::open(&path).unwrap();
    reopened.ingest(std::slice::from_ref(&canonical)).unwrap();
    assert_eq!(reopened.snapshot().events, vec![canonical]);
    assert_eq!(
        std::fs::read(path.with_extension("v3-evidence.json")).unwrap(),
        bytes
    );
}

#[test]
fn conflicting_legacy_snapshot_never_rewrites_original_trace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    let mut raw = old_document();
    raw["events"][0]["event"]["event"]["event"]["partial"]["content"][0]["text"] =
        json!("different");
    let bytes = serde_json::to_vec(&raw).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    for _ in 0..2 {
        assert!(TraceStore::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::read(path.with_extension("v3-evidence.json")).unwrap(),
            bytes
        );
    }
    std::fs::write(&path, serde_json::to_vec(&old_document()).unwrap()).unwrap();
    assert!(TraceStore::open(&path).is_ok());
    assert_eq!(
        std::fs::read(path.with_extension("v3-evidence.json")).unwrap(),
        bytes
    );
}
