use cayw_agent::AgentEvent;
use cayw_ai::{AssistantEvent, AssistantMessage, Message, Model, StopReason};
use cayw_core::{CoreError, SessionStore};
use std::path::PathBuf;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("cayw-core-stream-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn streamed_projection_keeps_history_and_observes_peer_recovery_and_damage() {
    let directory = Database::new();
    let path = directory.0.join("session.db");
    let store = SessionStore::open(&path).unwrap();
    let id = store.create_session("large streamed history").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    let user = Message::user("retained source content\n".repeat(50_000));
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: user.clone(),
            },
        )
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageEnd {
                message: user.clone(),
            },
        )
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
    let prefix = store.snapshot(&id).unwrap();
    for index in 0..32 {
        message.content = vec![cayw_ai::Content::text(format!("chunk {index}"))];
        let event = AgentEvent::MessageUpdate {
            event: Box::new(AssistantEvent::Snapshot {
                message: message.clone(),
            }),
        };
        store.append_agent_event(&lease, &event).unwrap();
        store.validate_lease(&lease).unwrap();
        assert_eq!(store.snapshot(&id).unwrap().messages, prefix.messages);
    }
    let before_host = store.snapshot(&id).unwrap();
    let host = store
        .append_host_event(&lease, "stream_progress", serde_json::json!({"chunk": 32}))
        .unwrap();
    assert!(matches!(
        store.append_control_event(
            &id,
            before_host.cursor,
            Some(&lease),
            "host",
            "stream_note",
            serde_json::json!({"note": "stale"}),
        ),
        Err(CoreError::InvalidEvent(_))
    ));
    let control = store
        .append_control_event(
            &id,
            host.cursor,
            Some(&lease),
            "host",
            "stream_note",
            serde_json::json!({"note": "retained"}),
        )
        .unwrap();
    assert_eq!(host.attempt.as_ref(), Some(&lease));
    assert_eq!(control.attempt.as_ref(), Some(&lease));
    assert_eq!(
        store.events(&id, before_host.cursor, 100).unwrap(),
        [host, control]
    );
    let mut expected = before_host;
    expected.cursor += 2;
    assert_eq!(store.snapshot(&id).unwrap(), expected);
    let peer = SessionStore::open(&path).unwrap();
    let before_recovery = peer.snapshot(&id).unwrap();
    assert_eq!(before_recovery, expected);
    assert_eq!(before_recovery.cursor, prefix.cursor + 34);
    assert_eq!(before_recovery.messages, vec![user]);
    peer.recover(&id).unwrap();
    assert!(matches!(
        store.validate_lease(&lease),
        Err(CoreError::StaleLease)
    ));
    let current = peer.begin_attempt(&id, "new-worker").unwrap();
    assert!(
        store
            .append_agent_event(&lease, &AgentEvent::AgentStart)
            .is_err()
    );
    store.validate_lease(&current).unwrap();
    // Damage at an unchanged cursor must invalidate the cached parse as well.
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE core_sessions SET projection=json_set(projection, '$.id', 'wrong') WHERE id=?1",
            [&id],
        )
        .unwrap();
    assert!(store.snapshot(&id).is_err());
    assert!(store.validate_lease(&current).is_err());
}

#[test]
fn rotating_cached_sessions_observe_peer_writes_recovery_and_same_cursor_damage() {
    let directory = Database::new();
    let path = directory.0.join("session.db");
    let store = SessionStore::open(&path).unwrap();
    let first = store.create_session("first").unwrap();
    let first_lease = store.begin_attempt(&first.id, "first-worker").unwrap();
    let second = store.create_session("second").unwrap();
    let second_lease = store.begin_attempt(&second.id, "second-worker").unwrap();
    let first_before = store.snapshot(&first.id).unwrap();
    let second_before = store.snapshot(&second.id).unwrap();
    let peer = SessionStore::open(&path).unwrap();
    let event = peer
        .append_host_event(
            &first_lease,
            "peer_fixture",
            serde_json::json!({"value": 1}),
        )
        .unwrap();
    assert_eq!(store.snapshot(&second.id).unwrap(), second_before);
    assert_eq!(
        store.snapshot(&first.id).unwrap().cursor,
        first_before.cursor + 1
    );
    assert_eq!(
        store.events(&first.id, first_before.cursor, 100).unwrap(),
        [event]
    );
    peer.recover(&first.id).unwrap();
    assert!(matches!(
        store.validate_lease(&first_lease),
        Err(CoreError::StaleLease)
    ));
    store.validate_lease(&second_lease).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE core_sessions SET projection=json_set(projection, '$.id', 'wrong') WHERE id=?1",
            [&second.id],
        )
        .unwrap();
    assert!(store.snapshot(&second.id).is_err());
    assert!(store.validate_lease(&second_lease).is_err());
    assert_eq!(
        store.snapshot(&first.id).unwrap(),
        peer.snapshot(&first.id).unwrap()
    );
}

#[test]
fn rejected_stream_update_rolls_back_and_valid_updates_survive_reopen() {
    let directory = Database::new();
    let path = directory.0.join("session.db");
    let store = SessionStore::open(&path).unwrap();
    let id = store.create_session("rollback").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    let message = AssistantMessage::new(&Model::new("fixture", "stream"));
    let update = AgentEvent::MessageUpdate {
        event: Box::new(AssistantEvent::Start {
            message: message.clone(),
        }),
    };
    let before = store.snapshot(&id).unwrap();
    assert!(store.append_agent_event(&lease, &update).is_err());
    assert_eq!(store.snapshot(&id).unwrap(), before);
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: Message::Assistant(message.clone()),
            },
        )
        .unwrap();
    store.append_agent_event(&lease, &update).unwrap();
    let mut terminal = message;
    terminal.stop_reason = StopReason::Stop;
    let terminal = Message::Assistant(terminal.clone());
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageEnd {
                message: terminal.clone(),
            },
        )
        .unwrap();
    let Message::Assistant(assistant) = terminal.clone() else {
        unreachable!()
    };
    store
        .append_agent_event(
            &lease,
            &AgentEvent::TurnEnd {
                message: assistant,
                tool_results: vec![],
            },
        )
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::AgentEnd {
                messages: vec![terminal],
            },
        )
        .unwrap();
    store.finish_attempt(&lease).unwrap();
    assert_eq!(
        SessionStore::open(&path).unwrap().snapshot(&id).unwrap(),
        store.snapshot(&id).unwrap()
    );
}

#[test]
fn deltas_and_control_observations_do_not_rewrite_checkpoint_and_replay_across_peers() {
    use cayw_ai::Content;
    use rusqlite::Connection;
    let directory = Database::new();
    let path = directory.0.join("stream.db");
    let store = SessionStore::open(&path).unwrap();
    let id = store.create_session("checkpoint").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    let mut message = AssistantMessage::new(&Model::new("fixture", "stream"));
    let prefix = "p".repeat(1024 * 1024);
    message.content = vec![Content::text(&prefix)];
    for event in [
        AgentEvent::AgentStart,
        AgentEvent::TurnStart,
        AgentEvent::MessageStart {
            message: Message::Assistant(message.clone()),
        },
    ] {
        store.append_agent_event(&lease, &event).unwrap();
    }
    let database = Connection::open(&path).unwrap();
    let checkpoint: String = database
        .query_row("SELECT projection FROM core_sessions", [], |row| row.get(0))
        .unwrap();
    let base_cursor = store.snapshot(&id).unwrap().cursor;
    database.execute_batch("CREATE TABLE projection_writes(bytes INTEGER NOT NULL); CREATE TRIGGER count_projection_writes AFTER UPDATE OF projection ON core_sessions BEGIN INSERT INTO projection_writes VALUES(length(NEW.projection)); END;").unwrap();
    let peer = SessionStore::open(&path).unwrap();
    for i in 0..200 {
        let writer = if i % 2 == 0 { &store } else { &peer };
        let event = writer
            .append_agent_event(
                &lease,
                &AgentEvent::MessageUpdate {
                    event: Box::new(AssistantEvent::TextDelta {
                        index: 0,
                        delta: "x".into(),
                    }),
                },
            )
            .unwrap();
        writer
            .append_control_event(
                &id,
                event.cursor,
                Some(&lease),
                "observer",
                "stream_observed",
                serde_json::json!({"fragment":i}),
            )
            .unwrap();
    }
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM projection_writes", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        database
            .query_row("SELECT projection FROM core_sessions", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        checkpoint
    );
    let suffix_bytes: i64 = database
        .query_row(
            "SELECT sum(length(record)) FROM core_events WHERE cursor>?1",
            [base_cursor as i64],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        suffix_bytes < prefix.len() as i64,
        "400 small updates must not repeat the 1 MiB prefix"
    );
    assert_eq!(store.snapshot(&id).unwrap().cursor, base_cursor + 400);
    drop(peer);
    drop(store);
    let reopened = SessionStore::open(&path).unwrap();
    assert_eq!(reopened.snapshot(&id).unwrap().cursor, base_cursor + 400);
    // A checkpoint snapshot validates the fully reconstructed message on reopen.
    let final_text = format!("{prefix}{}", "x".repeat(200));
    message.content = vec![Content::text(final_text.clone())];
    message.stop_reason = StopReason::Stop;
    reopened
        .append_agent_event(
            &lease,
            &AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            },
        )
        .unwrap();
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM projection_writes", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let snapshot = reopened.snapshot(&id).unwrap();
    let Message::Assistant(assistant) = snapshot.messages.last().unwrap() else {
        panic!("assistant terminal")
    };
    assert_eq!(assistant.content, vec![Content::text(final_text)]);
    // Recovery observes the committed terminal and never replays its effects.
    reopened.recover(&id).unwrap();
    assert_eq!(reopened.snapshot(&id).unwrap().messages, snapshot.messages);
}
