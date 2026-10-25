//! Final policy checks reject changes made during an earlier opening step.
//! Fixture writes model a settled amendment; these tests do not depend on a
//! thread scheduler hitting the lock-upgrade window.
use crate::{ContentConfig, ResourcePolicy, RuntimeError, content, queue::Queue};
use cayw_content::{ContentStore, Error, Limits};
use rusqlite::Connection;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

#[test]
fn competing_amendment_is_rejected_before_releasing_a_live_shared_lock() {
    use crate::coordination::{Coordination, amend_policy, amend_stopped_policy};
    use std::sync::atomic::{AtomicUsize, Ordering};

    for mode in ["instance", "opening", "offline"] {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("sessions.sqlite");
        drop(Connection::open(&database).unwrap());
        let coordination = (mode != "offline").then(|| Coordination::new(&database).unwrap());
        let _opening = (mode == "opening").then(|| {
            coordination
                .as_ref()
                .unwrap()
                .opening_policy(&database)
                .unwrap()
        });
        let amendment_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(format!("{}.runtime-policy-amend-lock", database.display()))
            .unwrap();
        amendment_lock.lock().unwrap();
        let calls = AtomicUsize::new(0);
        let change = || {
            let peer_amendment = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("{}.runtime-policy-amend-lock", database.display()))
                .unwrap();
            assert!(matches!(
                peer_amendment.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(()))
        };
        let result = match mode {
            "instance" => {
                let coordinator = coordination.as_ref().unwrap();
                let _lifecycle = coordinator.enter().unwrap();
                coordinator.amend_policy(change)
            }
            "opening" => amend_policy(&database, change),
            "offline" => amend_stopped_policy(&database, change),
            _ => unreachable!(),
        };
        assert!(result.is_err(), "{mode} entered a competing amendment");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        if coordination.is_some() {
            let peer = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("{}.runtime-policy-lock", database.display()))
                .unwrap();
            assert!(matches!(
                peer.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        }
        drop(amendment_lock);
        let result = match mode {
            "instance" => {
                let coordinator = coordination.as_ref().unwrap();
                let _lifecycle = coordinator.enter().unwrap();
                coordinator.amend_policy(change)
            }
            "opening" => amend_policy(&database, change),
            "offline" => amend_stopped_policy(&database, change),
            _ => unreachable!(),
        };
        assert!(result.is_ok(), "{mode}: {result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn final_queue_check_rejects_each_changed_policy_field_without_rewriting_it() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("sessions.sqlite");
    let resources = ResourcePolicy {
        max_declared_memory_bytes: 100,
    };
    let queue = Queue::open_with_resources(&database, 1, 4, 8, &resources).unwrap();
    let db = Connection::open(&database).unwrap();
    queue.check_policy(1, 4, 8, &resources).unwrap();
    for (field, original, changed) in [
        ("concurrency", 1, 2),
        ("capacity", 4, 5),
        ("fairness_after", 8, 9),
        ("max_declared_memory_bytes", 100, 200),
    ] {
        let update = format!("UPDATE runtime_queue_config SET {field}=?1 WHERE singleton=1");
        db.execute(&update, [changed]).unwrap();
        assert!(matches!(
            queue.check_policy(1, 4, 8, &resources),
            Err(RuntimeError::InvalidInput(_))
        ));
        let stored: i64 = db
            .query_row(
                &format!("SELECT {field} FROM runtime_queue_config WHERE singleton=1"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, changed, "the check must not amend {field}");
        db.execute(&update, [original]).unwrap();
        queue.check_policy(1, 4, 8, &resources).unwrap();
    }
}

struct ContentFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    database: PathBuf,
    config: ContentConfig,
    store: Arc<ContentStore>,
}
impl ContentFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let database = root.join("sessions.sqlite");
        drop(Connection::open(&database).unwrap());
        let config = ContentConfig {
            directory: root.join("content"),
            device_id: "policy-check-device".into(),
            limits: Limits {
                max_content_bytes: 1024,
                max_total_bytes: 4096,
                max_records: 8,
                ..Limits::default()
            },
        };
        let store = content::open(&database, &workspace, Some(&config))
            .unwrap()
            .unwrap();
        Self {
            _temp: temp,
            root,
            database,
            config,
            store,
        }
    }
    fn check(&self) -> Result<(), RuntimeError> {
        content::check_binding(&self.database, Some(&self.config), Some(&self.store))
    }
}

#[test]
fn final_content_check_rejects_a_changed_binding_without_adopting_it() {
    let fixture = ContentFixture::new();
    fixture.check().unwrap();
    let db = Connection::open(&fixture.database).unwrap();
    let original: String = db
        .query_row(
            "SELECT identity_json FROM runtime_content_config WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut changed: Value = serde_json::from_str(&original).unwrap();
    changed["config"]["limits"]["max_total_bytes"] = Value::from(8192);
    let changed = serde_json::to_string(&changed).unwrap();
    db.execute(
        "UPDATE runtime_content_config SET identity_json=?1 WHERE singleton=1",
        [&changed],
    )
    .unwrap();
    assert!(matches!(
        fixture.check(),
        Err(RuntimeError::InvalidInput(_))
    ));
    let retained: String = db
        .query_row(
            "SELECT identity_json FROM runtime_content_config WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, changed);
    assert_eq!(fixture.store.limits(), fixture.config.limits);
    db.execute(
        "UPDATE runtime_content_config SET identity_json=?1 WHERE singleton=1",
        [&original],
    )
    .unwrap();
    fixture.check().unwrap();
}

#[test]
fn final_content_check_uses_the_held_root_and_rejects_changed_store_limits() {
    let fixture = ContentFixture::new();
    let moved = fixture.root.join("held-content");
    std::fs::rename(&fixture.config.directory, &moved).unwrap();
    std::fs::create_dir(&fixture.config.directory).unwrap();
    fixture.check().unwrap();
    assert!(!fixture.config.directory.join("state.json").exists());
    assert!(!fixture.config.directory.join(".lock").exists());
    assert!(!fixture.config.directory.join("objects").exists());

    let state_path = moved.join("state.json");
    let original = std::fs::read(&state_path).unwrap();
    let mut changed: Value = serde_json::from_slice(&original).unwrap();
    changed["limits"]["max_total_bytes"] = Value::from(8192);
    let changed = serde_json::to_vec(&changed).unwrap();
    std::fs::write(&state_path, &changed).unwrap();
    assert!(matches!(
        fixture.check(),
        Err(RuntimeError::Content(Error::StoreConfigurationMismatch))
    ));
    assert_eq!(std::fs::read(&state_path).unwrap(), changed);
    assert!(!fixture.config.directory.join("state.json").exists());
    assert!(!fixture.config.directory.join(".lock").exists());
    std::fs::write(state_path, original).unwrap();
    fixture.check().unwrap();
}
