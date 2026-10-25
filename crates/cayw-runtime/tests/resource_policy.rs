use cayw_runtime::{ResourcePolicy, RuntimeError, TaskResources};
use serde_json::json;

#[test]
fn policy_uses_a_positive_signed_storage_range_and_one_gib_default() {
    let default = ResourcePolicy::default();
    assert_eq!(default.max_declared_memory_bytes, 1024 * 1024 * 1024);
    default.validate().unwrap();
    ResourcePolicy {
        max_declared_memory_bytes: i64::MAX as u64,
    }
    .validate()
    .unwrap();
    for limit in [0, i64::MAX as u64 + 1, u64::MAX] {
        assert!(matches!(
            ResourcePolicy {
                max_declared_memory_bytes: limit
            }
            .validate(),
            Err(RuntimeError::InvalidInput(_))
        ));
    }
}

#[test]
fn declaration_checks_the_total_and_never_claims_a_hard_memory_guarantee() {
    let policy = ResourcePolicy {
        max_declared_memory_bytes: 100,
    };
    for bytes in [0, 1, 100] {
        TaskResources {
            declared_memory_bytes: bytes,
            ..Default::default()
        }
        .validate(&policy)
        .unwrap();
    }
    assert!(
        TaskResources {
            declared_memory_bytes: 101,
            ..Default::default()
        }
        .validate(&policy)
        .unwrap_err()
        .to_string()
        .contains("declared memory")
    );
    assert!(
        TaskResources::default()
            .validate(&ResourcePolicy {
                max_declared_memory_bytes: 0,
            })
            .is_err()
    );
}

#[test]
fn unsupported_guarantees_are_rejected_even_when_the_requested_number_is_zero() {
    let policy = ResourcePolicy::default();
    let cases = [
        (
            TaskResources {
                hard_memory_limit_bytes: Some(0),
                ..Default::default()
            },
            "hard memory",
        ),
        (
            TaskResources {
                cpu_quota_percent: Some(0),
                ..Default::default()
            },
            "CPU",
        ),
        (
            TaskResources {
                gpu_units: 1,
                ..Default::default()
            },
            "GPU",
        ),
        (
            TaskResources {
                io_bandwidth_bytes_per_sec: Some(0),
                ..Default::default()
            },
            "I/O bandwidth",
        ),
    ];
    for (request, resource) in cases {
        let error = request.validate(&policy).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(error.to_string().contains(resource), "{error}");
    }
}

#[test]
fn resource_input_roundtrips_without_unknown_fields_or_numeric_coercion() {
    let default: TaskResources = serde_json::from_value(json!({})).unwrap();
    assert_eq!(default, TaskResources::default());
    assert_eq!(default.declared_memory_bytes, 0);
    let request = TaskResources {
        declared_memory_bytes: 512,
        ..Default::default()
    };
    assert_eq!(
        serde_json::from_value::<TaskResources>(serde_json::to_value(&request).unwrap()).unwrap(),
        request
    );
    for value in [
        json!({"declared_memory_bytes": "512"}),
        json!({"declared_memory_bytes": -1}),
        json!({"declared_memory_bytes": 1.5}),
        json!({"declared_memory_bytes": 512, "fallback_to_observation": true}),
    ] {
        assert!(serde_json::from_value::<TaskResources>(value).is_err());
    }
    assert!(
        serde_json::from_value::<ResourcePolicy>(
            json!({"max_declared_memory_bytes": 1024, "fallback": true})
        )
        .is_err()
    );
}
