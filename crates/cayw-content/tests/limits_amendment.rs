#![cfg(unix)]
//! Explicit quota amendment for one bound local store. Device, root identity,
//! records and object bytes stay authoritative; stale handles keep failing
//! closed when the effective limits differ.
use cayw_content::{ContentStore, Error, ExpectedRevision, Id, Limits};
use tempfile::TempDir;

fn dir() -> TempDir {
    TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}
fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

#[test]
fn amendment_preserves_root_device_and_records_and_rejects_data_loss() {
    let root = dir();
    let path = root.path().join("store");
    let device = id("device-a");
    let original = Limits {
        max_content_bytes: 1024,
        max_total_bytes: 4096,
        max_records: 2,
        ..Limits::default()
    };
    let store = ContentStore::open_private(&path, device.clone(), original, None).unwrap();
    let record = store
        .put(&id("note"), ExpectedRevision::Absent, b"kept")
        .unwrap();
    let metadata = store.root_metadata().unwrap();
    drop(store);

    let increased = Limits {
        max_total_bytes: original.max_total_bytes + 1024,
        max_records: original.max_records + 1,
        ..original
    };
    let amended = ContentStore::open_private_amending_limits(
        &path,
        device.clone(),
        increased,
        (metadata.device, metadata.inode),
    )
    .unwrap();
    assert_eq!(amended.limits(), increased);
    assert_eq!(
        amended.read(&id("note")).unwrap().unwrap(),
        (record, b"kept".to_vec())
    );
    drop(amended);

    // A handle that still claims the old limits sees the changed policy and
    // fails closed instead of silently using the old numbers.
    assert!(matches!(
        ContentStore::open_private(
            &path,
            device.clone(),
            original,
            Some((metadata.device, metadata.inode))
        ),
        Err(Error::StoreConfigurationMismatch)
    ));

    // The original root identity is still required for a quota amendment.
    assert!(matches!(
        ContentStore::open_private_amending_limits(
            &path,
            device.clone(),
            increased,
            (metadata.device, metadata.inode + 1)
        ),
        Err(Error::RootIdentityMismatch)
    ));

    // A decrease that would invalidate an existing record is rejected and does
    // not rewrite the durable limits.
    let shrink = Limits {
        max_content_bytes: 2,
        ..increased
    };
    assert!(matches!(
        ContentStore::open_private_amending_limits(
            &path,
            device.clone(),
            shrink,
            (metadata.device, metadata.inode)
        ),
        Err(Error::Integrity) | Err(Error::QuotaExceeded)
    ));
    let reopened = ContentStore::open_private(
        &path,
        device,
        increased,
        Some((metadata.device, metadata.inode)),
    )
    .unwrap();
    assert_eq!(reopened.read(&id("note")).unwrap().unwrap().1, b"kept");
}

#[test]
fn amendment_with_equal_limits_is_a_no_op_and_still_checks_identity() {
    let root = dir();
    let path = root.path().join("store");
    let device = id("device-a");
    let limits = Limits::default();
    let store = ContentStore::open_private(&path, device.clone(), limits, None).unwrap();
    store
        .put(&id("note"), ExpectedRevision::Absent, b"value")
        .unwrap();
    let metadata = store.root_metadata().unwrap();
    drop(store);
    let reopened = ContentStore::open_private_amending_limits(
        &path,
        device,
        limits,
        (metadata.device, metadata.inode),
    )
    .unwrap();
    assert_eq!(reopened.limits(), limits);
    assert_eq!(reopened.read(&id("note")).unwrap().unwrap().1, b"value");
    assert!(matches!(
        ContentStore::open_private_amending_limits(
            &path,
            id("device-a"),
            limits,
            (metadata.device, metadata.inode + 7)
        ),
        Err(Error::RootIdentityMismatch)
    ));
}
