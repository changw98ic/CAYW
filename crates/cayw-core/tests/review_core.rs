use cayw_agent::AgentEvent;
use cayw_ai::{AssistantEvent, AssistantMessage, Message, Model, StopReason};
use cayw_core::SessionStore;
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("cayw-core-review-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn path(&self) -> PathBuf {
        self.0.join("session.sqlite")
    }
    fn open(&self) -> SessionStore {
        SessionStore::open(self.path()).unwrap()
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn nested(depth: usize) -> Value {
    (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
}

#[test]
fn provider_metadata_values_are_stored_and_replayed_without_artificial_limits() {
    let db = Database::new();
    let store = db.open();
    let session = store.create_session("mc02 limits").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();

    // Only serde_json's own recursion limit rejects a value that cannot be read
    // back; there is no separate Core depth or byte ceiling.
    let mut msg_too_deep = AssistantMessage::new(&Model::new("test", "model"));
    msg_too_deep
        .provider_metadata
        .insert("deep".into(), nested(130));
    let snapshot_before = store.snapshot(&session.id).unwrap();
    assert!(
        store
            .append_agent_event(
                &lease,
                &AgentEvent::MessageStart {
                    message: Message::Assistant(msg_too_deep),
                },
            )
            .is_err()
    );
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot_before);

    // Large and deep-but-readable metadata is persisted and round-trips.
    let mut msg_valid = AssistantMessage::new(&Model::new("test", "model"));
    msg_valid.stop_reason = StopReason::Stop;
    msg_valid
        .provider_metadata
        .insert("large".into(), Value::String("x".repeat(3 * 1024 * 1024)));
    msg_valid
        .provider_metadata
        .insert("deep".into(), nested(64));
    let message = Message::Assistant(msg_valid.clone());
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: message.clone(),
            },
        )
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::MessageEnd { message })
        .unwrap();

    let reopened = SessionStore::open(db.path()).unwrap();
    let snapshot = reopened.snapshot(&session.id).unwrap();
    assert_eq!(snapshot.messages.len(), 1);
    match &snapshot.messages[0] {
        Message::Assistant(saved) => {
            assert_eq!(saved.provider_metadata, msg_valid.provider_metadata);
        }
        other => panic!("expected assistant message, got {other:?}"),
    }
}

#[test]
fn large_metadata_values_are_not_refused_by_artificial_byte_limits() {
    let db = Database::new();
    let store = db.open();
    let session = store.create_session("mc04 large").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();

    for (kind, value) in [
        ("large", Value::String("x".repeat(5 * 1024 * 1024))),
        ("escaped", Value::String("\"".repeat(2 * 1024 * 1024))),
        ("deep", nested(64)),
    ] {
        store.append_host_event(&lease, kind, value).unwrap();
    }
    let snapshot = store.snapshot(&session.id).unwrap();

    // A value beyond serde_json's typed-reader recursion limit is still a known
    // refusal and must leave the committed state untouched.
    assert!(
        store
            .append_host_event(&lease, "too-deep", nested(130))
            .is_err()
    );
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot);
    let reopened = SessionStore::open(db.path()).unwrap();
    assert_eq!(reopened.snapshot(&session.id).unwrap(), snapshot);
}

#[test]
fn mc05_no_duplicate_validation_and_streaming_persist() {
    let db = Database::new();
    let store = db.open();
    let session = store.create_session("mc05 stream").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();
    let base_cursor = store.snapshot(&session.id).unwrap().cursor;
    assert_eq!(base_cursor, 2);

    // 1. Streaming persistence succeeds for host events and cursor increments monotonically
    for i in 0..10 {
        let event = store
            .append_host_event(&lease, "ping", json!({"i": i}))
            .unwrap();
        assert_eq!(event.cursor, base_cursor + (i + 1) as u64);
    }
    let snapshot = store.snapshot(&session.id).unwrap();
    assert_eq!(snapshot.cursor, base_cursor + 10);

    // 2. Interleaved streaming: alternate MessageUpdate (json_set fast path) and Host events (mutate)
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();

    let mut message = AssistantMessage::new(&Model::new("fixture", "stream"));
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: Message::Assistant(message.clone()),
            },
        )
        .unwrap();

    let mut expected_cursor = snapshot.cursor + 3; // AgentStart (13) + TurnStart (14) + MessageStart (15)
    assert_eq!(store.snapshot(&session.id).unwrap().cursor, expected_cursor);

    for i in 0..5 {
        message.content = vec![cayw_ai::Content::text(format!("chunk {i}"))];
        let update_event = AgentEvent::MessageUpdate {
            message: message.clone(),
            event: Box::new(AssistantEvent::TextDelta {
                index: 0,
                delta: format!("chunk {i}"),
                partial: message.clone(),
            }),
        };
        let agent_rec = store.append_agent_event(&lease, &update_event).unwrap();
        expected_cursor += 1;
        assert_eq!(agent_rec.cursor, expected_cursor);
        store.validate_lease(&lease).unwrap();

        let host_rec = store
            .append_host_event(&lease, "progress", json!({"chunk": i}))
            .unwrap();
        expected_cursor += 1;
        assert_eq!(host_rec.cursor, expected_cursor);
        store.validate_lease(&lease).unwrap();
    }

    message.stop_reason = StopReason::Stop;
    let end_rec = store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageEnd {
                message: Message::Assistant(message.clone()),
            },
        )
        .unwrap();
    expected_cursor += 1;
    assert_eq!(end_rec.cursor, expected_cursor);

    // Reopen store from disk to verify projection cache consistency with on-disk state
    let reopened = SessionStore::open(db.path()).unwrap();
    let final_snapshot = reopened.snapshot(&session.id).unwrap();
    assert_eq!(final_snapshot.cursor, expected_cursor);
    assert_eq!(final_snapshot.messages.len(), 1);
    match &final_snapshot.messages[0] {
        Message::Assistant(saved) => {
            assert_eq!(saved.content, message.content);
            assert_eq!(saved.stop_reason, StopReason::Stop);
        }
        other => panic!("expected assistant message, got {other:?}"),
    }

    let all_events = reopened.events(&session.id, 0, 100).unwrap();
    assert_eq!(all_events.len() as u64, expected_cursor);
    for (idx, evt) in all_events.iter().enumerate() {
        assert_eq!(evt.cursor, (idx + 1) as u64);
    }
}
