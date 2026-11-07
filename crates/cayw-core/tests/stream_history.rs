use cayw_core::SessionStore;
use rusqlite::Connection;
use serde_json::json;
use std::path::PathBuf;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("cayw-stream-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn path(&self) -> PathBuf {
        self.0.join("session.db")
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn reopening_installs_wait_log_index_without_changing_history_or_protocol() {
    let database = Database::new();
    let store = SessionStore::open(database.path()).unwrap();
    let session = store.create_session("stream history").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();
    for fragment in 0..256 {
        store
            .append_host_event(
                &lease,
                "stream_fixture",
                json!({"fragment": fragment, "text": "x".repeat(512)}),
            )
            .unwrap();
    }
    let snapshot = store.snapshot(&session.id).unwrap();
    let events = store.events(&session.id, 0, 1000).unwrap();
    drop(store);

    // A current v3 database created before this index has the same event format.
    let connection = Connection::open(database.path()).unwrap();
    connection
        .execute_batch("DROP INDEX core_execution_wait_log")
        .unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    drop(connection);

    let reopened = SessionStore::open(database.path()).unwrap();
    assert_eq!(reopened.snapshot(&session.id).unwrap(), snapshot);
    assert_eq!(reopened.events(&session.id, 0, 1000).unwrap(), events);
    let connection = Connection::open(database.path()).unwrap();
    assert_eq!(
        connection
            .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        version
    );
    let mut statement = connection
        .prepare(
            "EXPLAIN QUERY PLAN SELECT cursor, record FROM core_events
             WHERE session_id=?1 AND json_extract(record, '$.event.type')='execution_wait_registered'
             ORDER BY cursor",
        )
        .unwrap();
    let plan = statement
        .query_map([&session.id], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter().any(|step| {
            step.contains("core_execution_wait_log") || step.contains("core_events_by_type")
        }),
        "{plan:?}"
    );
    assert!(
        !plan
            .iter()
            .any(|step| step.contains("SCAN") || step.contains("TEMP B-TREE")),
        "{plan:?}"
    );
}
