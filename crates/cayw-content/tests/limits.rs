//! Per-object and total-content quotas remain independent.

use cayw_content::{Error, Limits};

#[test]
fn total_content_quota_is_independent_of_the_per_object_quota() {
    let limits = Limits {
        max_content_bytes: 1024,
        max_total_bytes: 4,
        max_records: 8,
        max_security_entries: 16,
    };
    // Order alone is no longer a validation error.
    assert!(limits.validate().is_ok());
    assert_eq!(limits.max_total_bytes, 4);
}

#[test]
fn hard_invalid_content_limits_are_still_rejected() {
    for limits in [
        Limits {
            max_content_bytes: 0,
            ..Limits::default()
        },
        Limits {
            max_records: 0,
            ..Limits::default()
        },
        Limits {
            max_security_entries: 0,
            ..Limits::default()
        },
    ] {
        assert!(
            matches!(limits.validate(), Err(Error::InvalidLimits)),
            "expected invalid limits for {limits:?}"
        );
    }
}

#[test]
fn explicit_total_quota_is_enforced_with_default_unbounded_objects() {
    use cayw_content::{ContentStore, ExpectedRevision, Id};
    let root = tempfile::TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let limits = Limits {
        max_total_bytes: 4,
        ..Limits::default()
    };
    let store = ContentStore::open(root.path(), Id::new("device").unwrap(), limits).unwrap();
    store
        .put(
            &Id::new("exact").unwrap(),
            ExpectedRevision::Absent,
            b"1234",
        )
        .unwrap();
    assert!(matches!(
        store.put(&Id::new("extra").unwrap(), ExpectedRevision::Absent, b"x"),
        Err(Error::QuotaExceeded)
    ));
    assert_eq!(store.limits().max_total_bytes, 4);
}
