//! Launch settings select exact requestable operations; they never issue grants.
use cayw_runtime::{HeadlessConfig, RuntimeConfig};
use serde_json::{Value, json};

fn configuration() -> Value {
    json!({
        "profiles":{"project":{"kind":"test","program":env!("CARGO_BIN_EXE_cayw-runtime-process-fixture"),
            "args":["leaf","/"],"timeout_ms":10000,"max_output_bytes":4096}},
        "requests":{
            "permission_policy_id":"project-test-policy",
            "operations":[{"id":"project-test","capability":"test.run",
                "arguments":{"profile":"project"},"permission_binding":"fixed-project-test"}],
            "limits":{"repeated_valid_threshold":3,"request_lifetime_ms":90000,
                "evaluation_timeout_ms":15000,"max_grant_lifetime_ms":45000}
        }
    })
}
fn parse(input: Value) -> Result<RuntimeConfig, String> {
    serde_json::from_value::<HeadlessConfig>(input)
        .map_err(|error| error.to_string())?
        .into_runtime_config()
        .map_err(|error| error.to_string())
}

#[test]
fn request_configuration_maps_exact_operations_and_bounded_limits_without_granting() {
    // Losing launch request configuration or treating it as ordinary permission
    // would make this test fail before a workspace or process can be started.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("requests.json");
    std::fs::write(&path, serde_json::to_vec(&configuration()).unwrap()).unwrap();
    let config = HeadlessConfig::load(path)
        .unwrap()
        .into_runtime_config()
        .unwrap();
    let policy = config.request_policy.as_ref().unwrap();
    assert_eq!(policy.permission_policy_id, "project-test-policy");
    assert_eq!(policy.operations.len(), 1);
    assert_eq!(policy.operations[0].id, "project-test");
    assert_eq!(policy.operations[0].capability, "test.run");
    assert_eq!(policy.operations[0].arguments, json!({"profile":"project"}));
    assert_eq!(
        policy.operations[0].permission_binding,
        "fixed-project-test"
    );
    assert_eq!(config.request_limits.repeated_valid_threshold, 3);
    assert_eq!(config.request_limits.request_lifetime_ms, 90000);
    assert_eq!(config.request_limits.evaluation_timeout_ms, 15000);
    assert_eq!(config.request_limits.max_grant_lifetime_ms, 45000);
    assert_eq!(
        config.request_limits.max_events,
        cayw_requests::RequestConfig::default().max_events
    );
    assert!(config.permissions.is_empty());
    assert!(!root.path().join("session.db").exists());
    assert!(!root.path().join("leaf.pid").exists());
}

#[test]
fn omitted_request_limits_use_one_default_and_absent_policy_stays_disabled() {
    let config = parse(json!({})).unwrap();
    assert!(config.request_policy.is_none());
    assert_eq!(
        config.request_limits,
        cayw_requests::RequestConfig::default()
    );
    let mut input = configuration();
    input["requests"].as_object_mut().unwrap().remove("limits");
    let config = parse(input).unwrap();
    assert!(config.request_policy.is_some());
    assert_eq!(
        config.request_limits,
        cayw_requests::RequestConfig::default()
    );
    assert!(config.permissions.is_empty());
}

#[test]
fn launch_rejects_unavailable_or_expanded_operations_and_forged_authorization() {
    for change in [
        json!({"operations":[]}),
        json!({"permission_policy_id":""}),
        json!({"operations":[{"id":"project-test","capability":"shell.run","arguments":{"command":"anything"},"permission_binding":"fixed"}]}),
        json!({"operations":[{"id":"project-test","capability":"test.run","arguments":{"profile":"missing"},"permission_binding":"fixed"}]}),
        json!({"operations":[{"id":"project-test","capability":"test.run","arguments":{"profile":"project","args":["--other"]},"permission_binding":"fixed"}]}),
        json!({"operations":[{"id":"project-test","capability":"test.run","arguments":{"profile":"project"},"permission_binding":""}]}),
        json!({"actor":"model"}),
        json!({"approved":true}),
        json!({"grants":[]}),
        json!({"limits":{"repeated_valid_threshold":0}}),
        json!({"limits":{"evaluation_timeout_ms":0}}),
        json!({"limits":{"max_grant_lifetime_ms":u64::MAX}}),
        json!({"limits":{"max_state_bytes":1}}),
        json!({"limits":{"automatic_approval":true}}),
    ] {
        let mut input = configuration();
        for (key, value) in change.as_object().unwrap() {
            input["requests"][key] = value.clone();
        }
        assert!(
            parse(input).is_err(),
            "accepted invalid request settings: {change}"
        );
    }
    let mut input = configuration();
    let operation = input["requests"]["operations"][0].clone();
    input["requests"]["operations"]
        .as_array_mut()
        .unwrap()
        .push(operation);
    assert!(
        parse(input).is_err(),
        "duplicate operation must be rejected"
    );
}
