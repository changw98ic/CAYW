use cayw_agent::AgentEvent;
use cayw_ai::{AssistantEvent, AssistantMessage, Message, Model, StopReason};
use cayw_core::{CoreError, MAX_METADATA_BYTES, SessionStore};
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
fn mc02_provider_metadata_leaf_limits_enforced_on_append_agent_event() {
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
    let snapshot_before = store.snapshot(&session.id).unwrap();

    // 1. Depth > 32
    let mut msg_deep = AssistantMessage::new(&Model::new("test", "model"));
    msg_deep.provider_metadata.insert("deep".into(), nested(33));
    let event_deep = AgentEvent::MessageStart {
        message: Message::Assistant(msg_deep),
    };
    assert!(matches!(
        store.append_agent_event(&lease, &event_deep),
        Err(CoreError::InvalidInput(_))
    ));
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot_before);

    // 2. Nodes > 4096
    let mut msg_nodes = AssistantMessage::new(&Model::new("test", "model"));
    msg_nodes
        .provider_metadata
        .insert("nodes".into(), Value::Array(vec![Value::Null; 4096]));
    let event_nodes = AgentEvent::MessageStart {
        message: Message::Assistant(msg_nodes),
    };
    assert!(matches!(
        store.append_agent_event(&lease, &event_nodes),
        Err(CoreError::InvalidInput(_))
    ));
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot_before);

    // 3. Bytes > 65536
    let mut msg_bytes = AssistantMessage::new(&Model::new("test", "model"));
    msg_bytes
        .provider_metadata
        .insert("bytes".into(), Value::String("x".repeat(65537)));
    let event_bytes = AgentEvent::MessageStart {
        message: Message::Assistant(msg_bytes),
    };
    assert!(matches!(
        store.append_agent_event(&lease, &event_bytes),
        Err(CoreError::InvalidInput(_))
    ));
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot_before);

    // 4. Valid provider_metadata is stored, MessageUpdate rejection & round-trips intact
    let mut msg_valid = AssistantMessage::new(&Model::new("test", "model"));
    msg_valid.stop_reason = StopReason::Stop;
    msg_valid.provider_metadata.insert(
        "replay_token".into(),
        json!({"item_id": "item-12345", "signature": "sig-abc-67890"}),
    );
    let event_start = AgentEvent::MessageStart {
        message: Message::Assistant(msg_valid.clone()),
    };
    store.append_agent_event(&lease, &event_start).unwrap();
    let snapshot_after_start = store.snapshot(&session.id).unwrap();

    // 4a. MessageUpdate carrying invalid provider_metadata (depth > 32) is rejected
    let mut msg_update_deep = msg_valid.clone();
    msg_update_deep
        .provider_metadata
        .insert("deep_update".into(), nested(33));
    let event_update_deep = AgentEvent::MessageUpdate {
        message: msg_update_deep.clone(),
        event: Box::new(AssistantEvent::TextDelta {
            index: 0,
            delta: "deep".into(),
            partial: msg_update_deep,
        }),
    };
    assert!(matches!(
        store.append_agent_event(&lease, &event_update_deep),
        Err(CoreError::InvalidInput(_))
    ));
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot_after_start);

    // 4b. MessageUpdate carrying invalid provider_metadata (bytes > 64KiB) is rejected
    let mut msg_update_bytes = msg_valid.clone();
    msg_update_bytes
        .provider_metadata
        .insert("bytes_update".into(), Value::String("y".repeat(65537)));
    let event_update_bytes = AgentEvent::MessageUpdate {
        message: msg_update_bytes.clone(),
        event: Box::new(AssistantEvent::TextDelta {
            index: 0,
            delta: "bytes".into(),
            partial: msg_update_bytes,
        }),
    };
    assert!(matches!(
        store.append_agent_event(&lease, &event_update_bytes),
        Err(CoreError::InvalidInput(_))
    ));
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot_after_start);

    // 4c. Valid MessageUpdate succeeds and preserves valid provider_metadata
    let mut msg_update_valid = msg_valid.clone();
    msg_update_valid.content = vec![cayw_ai::Content::text("streamed chunk")];
    let event_update_valid = AgentEvent::MessageUpdate {
        message: msg_update_valid.clone(),
        event: Box::new(AssistantEvent::TextDelta {
            index: 0,
            delta: "streamed chunk".into(),
            partial: msg_update_valid.clone(),
        }),
    };
    let update_rec = store
        .append_agent_event(&lease, &event_update_valid)
        .unwrap();
    assert_eq!(update_rec.cursor, snapshot_after_start.cursor + 1);

    let event_end = AgentEvent::MessageEnd {
        message: Message::Assistant(msg_update_valid.clone()),
    };
    let end_rec = store.append_agent_event(&lease, &event_end).unwrap();
    assert_eq!(end_rec.cursor, snapshot_after_start.cursor + 2);

    let reopened = SessionStore::open(db.path()).unwrap();
    let snapshot = reopened.snapshot(&session.id).unwrap();
    assert_eq!(snapshot.cursor, snapshot_before.cursor + 3);
    let messages = snapshot.messages;
    assert_eq!(messages.len(), 1);
    match &messages[0] {
        Message::Assistant(saved) => {
            assert_eq!(saved.provider_metadata, msg_valid.provider_metadata);
            assert_eq!(saved.content, msg_update_valid.content);
        }
        other => panic!("expected assistant message, got {other:?}"),
    }
}

#[test]
fn mc04_byte_count_writer_boundary_and_escape_checks() {
    let db = Database::new();
    let store = db.open();
    let session = store.create_session("mc04 boundary").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();

    // Exact boundary: "..." has 2 quotes + 65534 ascii chars = 65536 bytes == MAX_METADATA_BYTES
    let exact_str = "x".repeat(MAX_METADATA_BYTES - 2);
    let exact_event = store.append_host_event(&lease, "exact", Value::String(exact_str));
    assert!(
        exact_event.is_ok(),
        "exact 64KiB metadata bytes must be accepted"
    );

    let snapshot_after_exact = store.snapshot(&session.id).unwrap();

    // Exact + 1: "..." has 2 quotes + 65535 ascii chars = 65537 bytes > MAX_METADATA_BYTES
    let overflow_str = "x".repeat(MAX_METADATA_BYTES - 1);
    let overflow_event = store.append_host_event(&lease, "overflow", Value::String(overflow_str));
    assert!(
        matches!(overflow_event, Err(CoreError::InvalidInput(msg)) if msg.contains("Metadata byte limit exceeded")),
        "64KiB + 1 byte must be rejected"
    );
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot_after_exact);

    // Escaped string: length is 32768, but serialized as \" becomes 65536 + 2 quotes = 65538 bytes
    let escaped_str = "\"".repeat(32768);
    let escaped_event = store.append_host_event(&lease, "escaped", Value::String(escaped_str));
    assert!(
        matches!(escaped_event, Err(CoreError::InvalidInput(msg)) if msg.contains("Metadata byte limit exceeded")),
        "string that expands on JSON escape beyond 64KiB must be rejected"
    );
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot_after_exact);

    // Ultra-large string (e.g. 5MB) must be rejected without allocating 5MB full JSON buffer
    let huge_str = "a".repeat(5 * 1024 * 1024);
    let huge_event = store.append_host_event(&lease, "huge", Value::String(huge_str));
    assert!(
        matches!(huge_event, Err(CoreError::InvalidInput(msg)) if msg.contains("Metadata byte limit exceeded")),
        "huge string must be rejected"
    );
    assert_eq!(store.snapshot(&session.id).unwrap(), snapshot_after_exact);
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
