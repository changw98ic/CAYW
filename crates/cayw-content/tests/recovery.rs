use cayw_content::{ContentStore, Error, ExpectedRevision, Id, Limits};
use tempfile::TempDir;
fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
fn root() -> TempDir {
    TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}

#[test]
fn a_missing_authority_file_is_not_reinitialized_over_existing_content() {
    let root = root();
    let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    db.put(&id("record"), ExpectedRevision::Absent, b"must survive")
        .unwrap();
    drop(db);
    std::fs::remove_file(root.path().join("state.json")).unwrap();
    assert!(matches!(
        ContentStore::open(root.path(), id("device"), Limits::default()),
        Err(Error::Integrity)
    ));
    assert!(!root.path().join("state.json").exists());
}

#[cfg(feature = "encryption")]
#[test]
fn a_missing_authority_file_cannot_erase_revocation_in_an_empty_store() {
    use cayw_content::KeyHandle;
    let root = root();
    let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    db.revoke_key(&id("revoked")).unwrap();
    drop(db);
    std::fs::remove_file(root.path().join("state.json")).unwrap();
    let reopened = ContentStore::open(root.path(), id("device"), Limits::default());
    if let Ok(db) = &reopened {
        assert!(
            db.register_key(&KeyHandle::from_bytes(id("revoked"), [1; 32]))
                .is_err(),
            "missing state erased a durable key revocation"
        );
    }
    assert!(matches!(reopened, Err(Error::Integrity)));
}

#[test]
fn interrupted_staging_files_never_change_the_authoritative_record() {
    let root = root();
    let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    let record = db
        .put(&id("record"), ExpectedRevision::Absent, b"committed")
        .unwrap();
    drop(db);
    std::fs::write(
        root.path().join(".state-stage"),
        b"{partially written metadata",
    )
    .unwrap();
    std::fs::write(root.path().join(".object-stage"), b"partial content").unwrap();
    let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    assert_eq!(
        db.read(&id("record")).unwrap().unwrap(),
        (record.clone(), b"committed".to_vec())
    );
    db.put(&id("record"), record.expected(), b"next committed")
        .unwrap();
    assert_eq!(
        db.read(&id("record")).unwrap().unwrap().1,
        b"next committed"
    );
}

#[test]
fn content_written_by_an_exited_process_reopens_without_network() {
    let root = root();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_write_worker", "--nocapture"])
        .env("CAYW_CONTENT_RECOVERY_ROOT", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    let (record, bytes) = db.read(&id("record")).unwrap().unwrap();
    assert_eq!(record.version, 1);
    assert_eq!(bytes, b"persisted by another process");
}

#[test]
fn process_write_worker() {
    let Some(root) = std::env::var_os("CAYW_CONTENT_RECOVERY_ROOT") else {
        return;
    };
    let db = ContentStore::open(root, id("device"), Limits::default()).unwrap();
    db.put(
        &id("record"),
        ExpectedRevision::Absent,
        b"persisted by another process",
    )
    .unwrap();
    std::process::exit(0);
}

#[test]
fn corrupt_authority_file_fails_closed_without_replacing_it() {
    let root = root();
    ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    std::fs::write(root.path().join("state.json"), b"{broken").unwrap();
    assert!(ContentStore::open(root.path(), id("device"), Limits::default()).is_err());
    assert_eq!(
        std::fs::read(root.path().join("state.json")).unwrap(),
        b"{broken"
    );
}
