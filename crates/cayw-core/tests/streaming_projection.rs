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
            message: message.clone(),
            event: Box::new(AssistantEvent::TextDelta {
                index: 0,
                delta: format!("chunk {index}"),
                partial: message.clone(),
            }),
        };
        store.append_agent_event(&lease, &event).unwrap();
        store.validate_lease(&lease).unwrap();
        assert_eq!(store.snapshot(&id).unwrap().messages, prefix.messages);
    }
    let peer = SessionStore::open(&path).unwrap();
    let before_recovery = peer.snapshot(&id).unwrap();
    assert_eq!(before_recovery.cursor, prefix.cursor + 32);
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
fn rejected_stream_update_rolls_back_and_valid_updates_survive_reopen() {
    let directory = Database::new();
    let path = directory.0.join("session.db");
    let store = SessionStore::open(&path).unwrap();
    let id = store.create_session("rollback").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    let message = AssistantMessage::new(&Model::new("fixture", "stream"));
    let update = AgentEvent::MessageUpdate {
        message: message.clone(),
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
