use cayw_core::{AttemptLease, CoreEvent, SessionStore};
use serde_json::{Value, json};
use std::path::PathBuf;

struct Db(PathBuf);
impl Db {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("cayw-child-audit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("state.db")
    }
    fn open(&self) -> SessionStore {
        SessionStore::open(self.path()).unwrap()
    }
}
impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn origin(store: &SessionStore, linked: bool) -> (AttemptLease, u64, Value) {
    let parent = store.create_session("parent").unwrap();
    let child = store.create_session("child").unwrap();
    let lease = store.begin_attempt(&parent.id, "parent-worker").unwrap();
    let identity = json!({"parent_session":parent.id,"parent_task":"parent-task",
        "child_session":child.id,"child_task":"child-task","depth":1,"label":"child",
        "prompt_bytes":5,"mode":"background","detached":true,"state":"queued"});
    let started = store
        .append_host_event(&lease, "agent_child_started", identity.clone())
        .unwrap();
    if linked {
        store
            .append_control_event(
                &child.id,
                child.cursor,
                None,
                "runtime-local-agent",
                "agent_child_linked",
                identity,
            )
            .unwrap();
    }
    let completion = json!({"parent_session":parent.id,"parent_task":"parent-task",
        "child_session":child.id,"child_task":"child-task","depth":1,"label":"child",
        "state":"completed","error":null});
    (lease, started.cursor, completion)
}

#[test]
fn completion_after_parent_finish_and_new_attempt_replays_with_original_identity() {
    let db = Db::new();
    let store = db.open();
    let (parent, started, data) = origin(&store, true);
    store.finish_attempt(&parent).unwrap();
    let current = store
        .begin_attempt(&parent.session_id, "new-worker")
        .unwrap();
    assert!(
        store
            .append_host_event(&parent, "agent_child_finished", data.clone())
            .is_err()
    );
    let record = store
        .append_detached_child_completion(&parent, started, data.clone())
        .unwrap();
    assert_eq!(record.attempt.as_ref(), Some(&current));
    let CoreEvent::Host { data: saved, .. } = &record.event else {
        panic!("host event");
    };
    assert_eq!(
        saved["parent_attempt"],
        serde_json::to_value(&parent).unwrap()
    );
    drop(store);
    let reopened = db.open();
    assert_eq!(
        reopened.snapshot(&parent.session_id).unwrap().attempt,
        Some(current)
    );
    let retry = reopened
        .append_detached_child_completion(&parent, started, data)
        .unwrap();
    assert_eq!(retry.cursor, record.cursor);
}

#[test]
fn concurrent_completion_is_unique_and_conflicting_terminal_is_rejected() {
    let db = Db::new();
    let store = db.open();
    let (parent, started, data) = origin(&store, true);
    store.finish_attempt(&parent).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (store, parent, data, barrier) =
                (db.open(), parent.clone(), data.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .append_detached_child_completion(&parent, started, data)
                    .unwrap()
                    .cursor
            })
        })
        .collect();
    let cursors: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(cursors[0], cursors[1]);
    let mut conflict = data.clone();
    conflict["state"] = json!("failed");
    assert!(
        store
            .append_detached_child_completion(&parent, started, conflict)
            .is_err()
    );
    let records = store
        .events_by_kind(
            &parent.session_id,
            0,
            100,
            "host",
            Some("agent_child_finished"),
        )
        .unwrap();
    assert_eq!(records.len(), 1);
    assert!(db.open().snapshot(&parent.session_id).is_ok());
}

#[test]
fn missing_or_forged_origin_is_rejected_and_write_failure_is_retryable() {
    let db = Db::new();
    let store = db.open();
    let (missing, cursor, data) = origin(&store, false);
    assert!(
        store
            .append_detached_child_completion(&missing, cursor, data)
            .is_err()
    );
    let (parent, started, data) = origin(&store, true);
    let mut forged = parent.clone();
    forged.attempt_id = "forged".into();
    assert!(
        store
            .append_detached_child_completion(&forged, started, data.clone())
            .is_err()
    );
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute_batch(
        "CREATE TRIGGER reject_completion BEFORE INSERT ON core_events
        WHEN json_extract(NEW.record, '$.event.kind')='agent_child_finished'
        BEGIN SELECT RAISE(ABORT, 'injected completion failure'); END;",
    )
    .unwrap();
    assert!(
        store
            .append_detached_child_completion(&parent, started, data.clone())
            .is_err()
    );
    assert!(
        store
            .events_by_kind(
                &parent.session_id,
                0,
                100,
                "host",
                Some("agent_child_finished")
            )
            .unwrap()
            .is_empty()
    );
    sql.execute_batch("DROP TRIGGER reject_completion").unwrap();
    store
        .append_detached_child_completion(&parent, started, data)
        .unwrap();
    assert!(db.open().snapshot(&parent.session_id).is_ok());
}
