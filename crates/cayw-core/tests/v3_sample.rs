//! Read-only check for the real Core v3 storage sample under `tests/fixtures`.
//!
//! The sample was produced through the public `SessionStore` + `Agent` API from
//! the frozen pre-move baseline and checked in as read-only evidence. This test
//! copies it, opens the copy, reads the event log and calls the explicit
//! recovery entry. Opening a v3 sample must not migrate or rewrite it.
//!
//! `EventRecord.timestamp` (Unix milliseconds) is the persisted time field. Its
//! monotonicity is not part of the protocol because the system clock can move
//! backwards. `OutcomeUnknown` recovery is covered by the existing recovery
//! tests in this crate; this sample only proves v3 bytes remain readable.

use cayw_core::{CoreEvent, SessionState, SessionStore};
use std::{fs, path::PathBuf};
use uuid::Uuid;

fn sample_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3-sample.sqlite")
}

#[test]
fn real_v3_sample_opens_reads_and_recovers() {
    let source = sample_path();
    assert!(
        source.exists(),
        "v3 sample is missing; it is checked-in read-only evidence, see tests/fixtures/README.md"
    );
    let directory = std::env::temp_dir().join(format!("cayw-v3-sample-{}", Uuid::new_v4()));
    fs::create_dir_all(&directory).unwrap();
    let copy = directory.join("sample.sqlite");
    fs::copy(&source, &copy).unwrap();
    let store = SessionStore::open(&copy).unwrap();
    let sessions = store.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    let id = sessions[0].id.clone();
    let snapshot = store.snapshot(&id).unwrap();
    assert_eq!(snapshot.state, SessionState::Idle);
    let events = store.events(&id, 0, 1000).unwrap();
    assert_eq!(events.len(), 22, "sample should contain its complete log");
    assert!(events.iter().all(|record| record.protocol_version == 3));
    // The time field is present and positive; clock monotonicity is not promised.
    assert!(events.iter().all(|record| record.timestamp > 0));
    assert!(
        events
            .iter()
            .any(|record| matches!(&record.event, CoreEvent::SessionCreated { .. }))
    );
    assert!(
        events
            .iter()
            .any(|record| matches!(&record.event, CoreEvent::AttemptStarted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|record| matches!(&record.event, CoreEvent::Agent { .. }))
    );
    assert!(
        events
            .iter()
            .any(|record| matches!(&record.event, CoreEvent::ToolStarted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|record| matches!(&record.event, CoreEvent::AttemptFinished { .. }))
    );
    // A settled idle session is unchanged by the explicit recovery entry: the v3
    // sample is read, not migrated or rewritten. Unknown recovery has its own tests.
    let recovered = store.recover(&id).unwrap();
    assert_eq!(recovered, snapshot);
    assert_eq!(store.events(&id, 0, 1000).unwrap().len(), events.len());
    drop(store);
    let _ = fs::remove_dir_all(directory);
}
