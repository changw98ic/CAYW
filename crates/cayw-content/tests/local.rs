use cayw_content::{ContentHash, ContentStore, Error, ExpectedRevision, Id, Limits};
use std::process::Command;
use tempfile::TempDir;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

fn store(root: &TempDir) -> ContentStore {
    ContentStore::open(root.path(), id("device-a"), Limits::default()).unwrap()
}

#[test]
fn persistent_read_checks_hash_and_revision_after_reopen() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let first = store(&root)
        .put(&id("notes"), ExpectedRevision::Absent, b"hello")
        .unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(
        first.content_hash.to_string(),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    let reopened = store(&root);
    let (record, bytes) = reopened.read(&id("notes")).unwrap().unwrap();
    assert_eq!(record, first);
    assert_eq!(bytes, b"hello");
    let second = reopened
        .put(&id("notes"), first.expected(), b"world")
        .unwrap();
    assert_eq!(second.version, 2);
    assert_eq!(second.parent_revision, Some(first.revision));
    assert_eq!(
        store(&root).read(&id("notes")).unwrap().unwrap().1,
        b"world"
    );
}

#[test]
fn stale_version_and_same_version_different_branch_are_rejected() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let db = store(&root);
    let first = db
        .put(&id("note"), ExpectedRevision::Absent, b"one")
        .unwrap();
    let other_root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let other = store(&other_root)
        .put(&id("note"), ExpectedRevision::Absent, b"one")
        .unwrap();
    assert!(matches!(
        db.put(&id("note"), other.expected(), b"fork"),
        Err(Error::Conflict { .. })
    ));
    let second = db.put(&id("note"), first.expected(), b"two").unwrap();
    assert!(matches!(
        db.put(&id("note"), first.expected(), b"stale"),
        Err(Error::Conflict { .. })
    ));
    assert!(matches!(
        db.put(&id("note"), ExpectedRevision::Absent, b"again"),
        Err(Error::Conflict { .. })
    ));
    assert_eq!(db.read(&id("note")).unwrap().unwrap().0, second);
}

#[test]
fn rejects_untrusted_record_ids_in_api_and_json() {
    for bad in [
        "", ".", "..", "../x", "/tmp/x", "a/b", "a\\b", "C:\\x", "a b", "a\n", "ümlaut",
    ] {
        assert!(Id::new(bad).is_err(), "accepted {bad:?}");
        let json = serde_json::to_string(bad).unwrap();
        assert!(serde_json::from_str::<Id>(&json).is_err());
    }
    assert!(Id::new("a".repeat(65)).is_err());
}

#[test]
fn quotas_apply_to_content_unique_disk_bytes_and_record_count() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let limits = Limits {
        max_content_bytes: 4,
        max_total_bytes: 6,
        max_records: 2,
        ..Limits::default()
    };
    let db = ContentStore::open(root.path(), id("device-a"), limits).unwrap();
    assert!(matches!(
        db.put(&id("a"), ExpectedRevision::Absent, b"12345"),
        Err(Error::ContentTooLarge)
    ));
    let first = db.put(&id("a"), ExpectedRevision::Absent, b"1234").unwrap();
    // Deduplication does not charge the same immutable object twice.
    db.put(&id("b"), ExpectedRevision::Absent, b"1234").unwrap();
    assert!(matches!(
        db.put(&id("a"), first.expected(), b"5678"),
        Err(Error::QuotaExceeded)
    ));
    assert!(matches!(
        db.put(&id("c"), ExpectedRevision::Absent, b"1"),
        Err(Error::QuotaExceeded)
    ));
    assert_eq!(db.read(&id("a")).unwrap().unwrap().1, b"1234");
}

#[test]
fn refuses_an_existing_store_with_different_device_or_limits() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    store(&root);
    assert!(matches!(
        ContentStore::open(root.path(), id("different"), Limits::default()),
        Err(Error::StoreConfigurationMismatch)
    ));
    assert!(matches!(
        ContentStore::open(
            root.path(),
            id("device-a"),
            Limits {
                max_content_bytes: 8,
                ..Limits::default()
            }
        ),
        Err(Error::StoreConfigurationMismatch)
    ));
}

#[test]
fn modified_blob_is_never_returned_or_silently_repaired() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let db = store(&root);
    let record = db
        .put(&id("note"), ExpectedRevision::Absent, b"hello")
        .unwrap();
    std::fs::write(
        root.path()
            .join("objects")
            .join(record.content_hash.to_string()),
        b"jello",
    )
    .unwrap();
    assert!(matches!(db.read(&id("note")), Err(Error::Integrity)));
    assert!(matches!(
        db.put(&id("other"), ExpectedRevision::Absent, b"hello"),
        Err(Error::Integrity)
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_root_ancestor_object_state_and_lock_are_rejected() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let outer = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    symlink(root.path(), outer.path().join("link")).unwrap();
    assert!(
        ContentStore::open(outer.path().join("link"), id("device-a"), Limits::default()).is_err()
    );
    std::fs::create_dir(root.path().join("nested")).unwrap();
    assert!(
        ContentStore::open(
            outer.path().join("link/nested"),
            id("device-a"),
            Limits::default()
        )
        .is_err()
    );
    let db = store(&root);
    let record = db
        .put(&id("note"), ExpectedRevision::Absent, b"hello")
        .unwrap();
    let blob = root
        .path()
        .join("objects")
        .join(record.content_hash.to_string());
    std::fs::remove_file(&blob).unwrap();
    let external = outer.path().join("secret");
    std::fs::write(&external, b"hello").unwrap();
    symlink(&external, &blob).unwrap();
    assert!(db.read(&id("note")).is_err());
    std::fs::remove_file(&blob).unwrap();
    symlink(outer.path(), root.path().join("objects-link")).unwrap();
    std::fs::rename(root.path().join("objects"), root.path().join("old-objects")).unwrap();
    std::fs::rename(
        root.path().join("objects-link"),
        root.path().join("objects"),
    )
    .unwrap();
    assert!(
        db.put(&id("other"), ExpectedRevision::Absent, b"new")
            .is_err()
    );
    for filename in ["state.json", ".lock"] {
        let another = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let store = store(&another);
        drop(store);
        std::fs::remove_file(another.path().join(filename)).unwrap();
        symlink(&external, another.path().join(filename)).unwrap();
        assert!(ContentStore::open(another.path(), id("device-a"), Limits::default()).is_err());
        assert_eq!(std::fs::read(&external).unwrap(), b"hello");
    }
}

#[test]
fn orphan_objects_after_interruption_still_count_toward_quota() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let limits = Limits {
        max_content_bytes: 8,
        max_total_bytes: 8,
        ..Limits::default()
    };
    let db = ContentStore::open(root.path(), id("device-a"), limits).unwrap();
    let bytes = b"orphan!!";
    std::fs::write(
        root.path()
            .join("objects")
            .join(ContentHash::of(bytes).to_string()),
        bytes,
    )
    .unwrap();
    assert!(matches!(
        db.put(&id("new"), ExpectedRevision::Absent, b"x"),
        Err(Error::QuotaExceeded)
    ));
}

#[test]
fn parallel_process_compare_and_swap_has_exactly_one_winner() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let db = store(&root);
    let first = db
        .put(&id("counter"), ExpectedRevision::Absent, b"initial")
        .unwrap();
    let expected = serde_json::to_string(&first.expected()).unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut children: Vec<_> = (0..6)
        .map(|_| {
            Command::new(&executable)
                .args(["--exact", "process_cas_worker", "--nocapture"])
                .env("CAYW_CONTENT_TEST_ROOT", root.path())
                .env("CAYW_CONTENT_EXPECTED", &expected)
                .spawn()
                .unwrap()
        })
        .collect();
    let outcomes: Vec<_> = children
        .iter_mut()
        .map(|child| child.wait().unwrap().code().unwrap())
        .collect();
    assert!(
        outcomes.iter().all(|code| matches!(code, 0 | 3)),
        "every losing process must report a revision conflict: {outcomes:?}"
    );
    let winners = outcomes.iter().filter(|code| **code == 0).count();
    assert_eq!(winners, 1);
    assert_eq!(db.read(&id("counter")).unwrap().unwrap().0.version, 2);
}

#[test]
fn process_cas_worker() {
    let Some(root) = std::env::var_os("CAYW_CONTENT_TEST_ROOT") else {
        return;
    };
    let expected: ExpectedRevision =
        serde_json::from_str(&std::env::var("CAYW_CONTENT_EXPECTED").unwrap()).unwrap();
    let db = ContentStore::open(root, id("device-a"), Limits::default()).unwrap();
    match db.put(&id("counter"), expected, b"winner") {
        Ok(_) => std::process::exit(0),
        Err(Error::Conflict { .. }) => std::process::exit(3),
        Err(error) => panic!("unexpected CAS result: {error}"),
    }
}
