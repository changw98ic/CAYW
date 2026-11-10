//! Event paging stops at a byte budget without reporting a corrupt log, and
//! the next request continues from the last returned cursor.

use cayw_core::{MAX_EVENT_PAGE_BYTES, SessionStore};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("cayw-core-page-{}", Uuid::new_v4()));
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

const LARGE_BYTES: usize = 1_500_000;

fn seed(store: &SessionStore) -> (String, Vec<u64>) {
    let session = store.create_session("page budget").unwrap();
    let lease = store.begin_attempt(&session.id, "worker").unwrap();
    let mut cursors = Vec::new();
    for index in 0..3 {
        let event = store
            .append_host_event(
                &lease,
                "page-budget",
                json!({"index": index, "payload": "x".repeat(LARGE_BYTES)}),
            )
            .unwrap();
        cursors.push(event.cursor);
    }
    (session.id, cursors)
}

#[test]
fn events_page_stops_at_byte_budget_and_continues_from_last_cursor() {
    let database = Database::new();
    let store = database.open();
    let (session, host_cursors) = seed(&store);

    let first = store.events(&session, 0, 1000).unwrap();
    assert!(
        !first.is_empty(),
        "a byte-bounded page must still return at least one record"
    );
    assert!(
        first.len() < 5,
        "the page must stop before all records fit past the byte budget"
    );
    let bytes: usize = first
        .iter()
        .map(|record| serde_json::to_vec(record).unwrap().len())
        .sum();
    assert!(
        bytes <= MAX_EVENT_PAGE_BYTES,
        "page serialized {} bytes, over the {} byte budget",
        bytes,
        MAX_EVENT_PAGE_BYTES
    );
    let first_cursor = first.first().unwrap().cursor;
    let last_cursor = first.last().unwrap().cursor;
    assert_eq!(first_cursor, 1);
    for (offset, record) in first.iter().enumerate() {
        assert_eq!(record.cursor, first_cursor + offset as u64);
    }
    assert!(
        last_cursor < host_cursors[2],
        "the largest event must be deferred to the next page"
    );

    let second = store.events(&session, last_cursor, 1000).unwrap();
    assert!(!second.is_empty());
    assert_eq!(second.first().unwrap().cursor, last_cursor + 1);
    assert_eq!(second.last().unwrap().cursor, host_cursors[2]);
    assert_eq!(first.len(), last_cursor as usize);

    // Re-reading the first page yields the same boundary instead of CorruptStore.
    let repeated = store.events(&session, 0, 1000).unwrap();
    assert_eq!(repeated.len(), first.len());
    assert_eq!(repeated.last().unwrap().cursor, last_cursor);
}

#[test]
fn events_by_kind_page_stops_at_byte_budget_and_continues() {
    let database = Database::new();
    let store = database.open();
    let (session, host_cursors) = seed(&store);

    let first = store
        .events_by_kind(&session, 0, 1000, "host", Some("page-budget"))
        .unwrap();
    assert_eq!(first.len(), 2, "two large records fit, the third does not");
    let bytes: usize = first
        .iter()
        .map(|record| serde_json::to_vec(record).unwrap().len())
        .sum();
    assert!(bytes <= MAX_EVENT_PAGE_BYTES);
    assert_eq!(first[0].cursor, host_cursors[0]);
    assert_eq!(first[1].cursor, host_cursors[1]);

    let second = store
        .events_by_kind(
            &session,
            first.last().unwrap().cursor,
            1000,
            "host",
            Some("page-budget"),
        )
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].cursor, host_cursors[2]);
}

#[test]
fn a_single_large_event_is_returned_in_one_page() {
    let database = Database::new();
    let store = database.open();
    let session = store.create_session("single huge").unwrap().id;
    let lease = store.begin_attempt(&session, "worker").unwrap();
    let huge = store
        .append_host_event(
            &lease,
            "page-budget-single",
            Value::String("y".repeat(5 * 1024 * 1024)),
        )
        .unwrap();

    let page = store
        .events_by_kind(&session, 0, 1000, "host", Some("page-budget-single"))
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].cursor, huge.cursor);
}
