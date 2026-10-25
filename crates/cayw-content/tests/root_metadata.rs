#![cfg(target_os = "macos")]

use cayw_content::{ContentStore, Error, ExpectedRevision, Id, Limits, RootMetadata};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use tempfile::TempDir;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

fn root() -> TempDir {
    TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}

fn metadata(path: &std::path::Path) -> RootMetadata {
    let actual = std::fs::metadata(path).unwrap();
    RootMetadata {
        device: actual.dev(),
        inode: actual.ino(),
        mode: actual.mode(),
    }
}

#[test]
fn root_metadata_reports_actual_directory_identity_and_mode() {
    let outer = root();
    let path = outer.path().join("store");
    let db = ContentStore::open(&path, id("device"), Limits::default()).unwrap();
    assert_eq!(db.root_metadata().unwrap(), metadata(&path));
    assert_eq!(db.root_metadata().unwrap().mode & 0o777, 0o700);
}

#[test]
fn root_metadata_and_writes_keep_original_directory_after_path_replacement() {
    let outer = root();
    let path = outer.path().join("store");
    let moved = outer.path().join("moved-store");
    let db = ContentStore::open(&path, id("device"), Limits::default()).unwrap();
    let first = db
        .put(&id("note"), ExpectedRevision::Absent, b"original")
        .unwrap();
    let original = db.root_metadata().unwrap();
    std::fs::rename(&path, &moved).unwrap();
    assert_eq!(db.root_metadata().unwrap(), original);
    assert_eq!(db.root_metadata().unwrap(), metadata(&moved));
    let replacement = ContentStore::open(&path, id("device"), Limits::default()).unwrap();
    let replaced = replacement.root_metadata().unwrap();
    assert_ne!(
        (original.device, original.inode),
        (replaced.device, replaced.inode)
    );
    assert_eq!(replaced, metadata(&path));
    assert_eq!(db.root_metadata().unwrap(), original);
    let second = db
        .try_put(&id("note"), first.expected(), b"updated original")
        .unwrap();
    assert_eq!(second.version, 2);
    assert!(replacement.try_read(&id("note")).unwrap().is_none());
    let reopened = ContentStore::open(&moved, id("device"), Limits::default()).unwrap();
    assert_eq!(reopened.root_metadata().unwrap(), original);
    assert_eq!(
        reopened.try_read(&id("note")).unwrap().unwrap(),
        (second, b"updated original".to_vec())
    );
}

#[test]
fn root_metadata_refreshes_permission_bits_of_the_held_directory() {
    let outer = root();
    let path = outer.path().join("store");
    let moved = outer.path().join("moved-store");
    let db = ContentStore::open(&path, id("device"), Limits::default()).unwrap();
    let original = db.root_metadata().unwrap();
    std::fs::rename(&path, &moved).unwrap();
    let replacement = ContentStore::open(&path, id("device"), Limits::default()).unwrap();
    for mode in [0o720, 0o702, 0o700] {
        std::fs::set_permissions(&moved, std::fs::Permissions::from_mode(mode)).unwrap();
        let actual = db.root_metadata().unwrap();
        assert_eq!(
            (actual.device, actual.inode),
            (original.device, original.inode)
        );
        assert_eq!(actual, metadata(&moved));
        assert_eq!(actual.mode & 0o022, mode & 0o022);
        assert_eq!(replacement.root_metadata().unwrap().mode & 0o022, 0);
    }
}

#[test]
fn private_open_checks_the_held_root_before_store_initialization() {
    let outer = root();
    let path = outer.path().join("candidate");
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o720)).unwrap();
    let stage = path.join(".state-stage");
    std::fs::write(&stage, b"unrelated existing stage").unwrap();
    assert!(matches!(
        ContentStore::open_private(&path, id("device"), Limits::default(), None),
        Err(Error::UnsafePermissions)
    ));
    assert_eq!(std::fs::read(&stage).unwrap(), b"unrelated existing stage");
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
    assert_eq!(metadata(&path).mode & 0o777, 0o720);
}

#[test]
fn bound_open_does_not_initialize_a_replacement_or_recreate_a_missing_root() {
    let outer = root();
    let path = outer.path().join("store");
    let moved = outer.path().join("moved");
    let store = ContentStore::open_private(&path, id("device"), Limits::default(), None).unwrap();
    let original = store.root_metadata().unwrap();
    let expected = Some((original.device, original.inode));
    std::fs::rename(&path, &moved).unwrap();
    assert!(ContentStore::open_private(&path, id("device"), Limits::default(), expected).is_err());
    assert!(!path.exists(), "a bound missing root must not be recreated");
    std::fs::create_dir(&path).unwrap();
    let stage = path.join(".state-stage");
    std::fs::write(&stage, b"replacement stage").unwrap();
    assert!(matches!(
        ContentStore::open_private(&path, id("device"), Limits::default(), expected),
        Err(Error::RootIdentityMismatch)
    ));
    assert_eq!(std::fs::read(&stage).unwrap(), b"replacement stage");
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
    let reopened =
        ContentStore::open_private(&moved, id("device"), Limits::default(), expected).unwrap();
    assert_eq!(reopened.root_metadata().unwrap(), original);
}

#[test]
fn limits_are_validated_without_opening_a_store() {
    Limits::default().validate().unwrap();
    let invalid = Limits {
        max_content_bytes: 0,
        ..Limits::default()
    };
    assert!(matches!(invalid.validate(), Err(Error::InvalidLimits)));
}
