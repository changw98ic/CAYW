use cayw_core::{CoreError, SessionStore};
use std::path::PathBuf;
use uuid::Uuid;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("cayw-core-schema-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn path(&self) -> PathBuf {
        self.0.join("session.sqlite")
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// A rejected file must not be migrated, switched to WAL or otherwise rewritten.
#[test]
fn unrecognized_or_incompatible_schema_files_are_rejected_without_changes() {
    for sql in [
        "PRAGMA user_version=99; CREATE TABLE user_data (value TEXT); INSERT INTO user_data VALUES ('retain');",
        "PRAGMA user_version=0; PRAGMA application_id=1128356183; CREATE TABLE core_sessions(id TEXT, projection TEXT);",
        "PRAGMA user_version=1; PRAGMA application_id=1128356183; CREATE TABLE core_sessions(id TEXT, projection TEXT); CREATE TABLE core_events(session_id TEXT, cursor INTEGER, record TEXT);",
        "PRAGMA user_version=1; CREATE TABLE unrelated (secret TEXT); INSERT INTO unrelated VALUES ('retain');",
        "CREATE TABLE sqliteBackup (value TEXT); INSERT INTO sqliteBackup VALUES ('retain');",
        "CREATE TABLE SQLITEX (value TEXT); INSERT INTO SQLITEX VALUES ('retain');",
    ] {
        let database = Database::new();
        {
            let db = rusqlite::Connection::open(database.path()).unwrap();
            db.execute_batch(sql).unwrap();
        }
        let before = std::fs::read(database.path()).unwrap();
        assert!(matches!(
            SessionStore::open(database.path()),
            Err(CoreError::SchemaVersion { .. } | CoreError::ForeignDatabase)
        ));
        assert_eq!(std::fs::read(database.path()).unwrap(), before);
        let db = rusqlite::Connection::open(database.path()).unwrap();
        let journal: String = db
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "delete");
    }
}

#[test]
fn unreadable_projection_fails_instead_of_resetting_a_session() {
    let database = Database::new();
    let store = SessionStore::open(database.path()).unwrap();
    let id = store.create_session("retained log").unwrap().id;
    let db = rusqlite::Connection::open(database.path()).unwrap();
    let event: String = db
        .query_row(
            "SELECT record FROM core_events WHERE session_id=?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    db.execute(
        "UPDATE core_sessions SET projection='{}' WHERE id=?1",
        [&id],
    )
    .unwrap();
    assert!(store.snapshot(&id).is_err());
    assert!(store.begin_attempt(&id, "worker").is_err());
    let retained: String = db
        .query_row(
            "SELECT record FROM core_events WHERE session_id=?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, event);
    assert_eq!(
        db.query_row("SELECT count(*) FROM core_events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
