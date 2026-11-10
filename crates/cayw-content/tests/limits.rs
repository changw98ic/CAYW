//! Orderable content limits are normalized; hard-invalid limits still fail.

use cayw_content::{Error, Limits};

#[test]
fn normalized_raises_total_to_content_and_validate_stays_strict() {
    let limits = Limits {
        max_content_bytes: 1024,
        max_total_bytes: 4,
        max_records: 8,
        max_security_entries: 16,
    };
    // Order alone is no longer a validation error.
    assert!(limits.validate().is_ok());
    let normalized = limits.normalized();
    assert_eq!(normalized.max_total_bytes, normalized.max_content_bytes);
    assert!(normalized.validate().is_ok());
}

#[test]
fn hard_invalid_content_limits_are_still_rejected() {
    for limits in [
        Limits {
            max_content_bytes: 0,
            ..Limits::default()
        },
        Limits {
            max_content_bytes: 1024 * 1024 * 1024 + 1,
            ..Limits::default()
        },
        Limits {
            max_records: 0,
            ..Limits::default()
        },
        Limits {
            max_records: 10_001,
            ..Limits::default()
        },
        Limits {
            max_security_entries: 0,
            ..Limits::default()
        },
        Limits {
            max_security_entries: 65_537,
            ..Limits::default()
        },
    ] {
        assert!(
            matches!(limits.validate(), Err(Error::InvalidLimits)),
            "expected invalid limits for {limits:?}"
        );
        assert!(matches!(
            limits.normalized().validate(),
            Err(Error::InvalidLimits)
        ));
    }
}
