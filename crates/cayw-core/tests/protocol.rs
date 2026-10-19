use cayw_core::{CoreError, CoreRequest, CoreResponse, PROTOCOL_VERSION, ProtocolEnvelope};
use serde_json::json;

#[test]
fn version_mismatch_is_rejected_before_interpreting_a_request() {
    let wire = json!({"version": PROTOCOL_VERSION + 1, "request_id":"req", "payload":{"type":"future_feature"}}).to_string();
    assert!(matches!(
        ProtocolEnvelope::<CoreRequest>::decode(&wire),
        Err(CoreError::ProtocolVersion { .. })
    ));
}

#[test]
fn old_approval_shape_and_missing_execution_facts_cannot_enter_the_current_protocol() {
    let old = json!({
        "version":1, "request_id":"legacy", "payload":{
            "type":"request_approval", "lease":{"session_id":"s","attempt_id":"a","worker_id":"w","fence":1},
            "input":{"task_id":"task","call_id":"c","capability":"fs.write","scope":"file","reason":"ask"}
        }
    });
    assert!(matches!(
        ProtocolEnvelope::<CoreRequest>::decode(&old.to_string()),
        Err(CoreError::ProtocolVersion { found: 1, .. })
    ));
    let mut disguised = old;
    disguised["version"] = json!(PROTOCOL_VERSION);
    assert!(ProtocolEnvelope::<CoreRequest>::decode(&disguised.to_string()).is_err());
    let incomplete_snapshot = json!({
        "id":"s","label":"retained", "messages":[],"cursor":1,"state":"idle","attempt":null,
        "pending_approvals":[],"unresolved_calls":[]
    });
    assert!(serde_json::from_value::<cayw_core::SessionSnapshot>(incomplete_snapshot).is_err());
}

#[test]
fn typed_execution_requests_and_events_round_trip_without_losing_their_origin() {
    use cayw_core::{
        AttemptLease, CapabilityCallKey, CapabilityInput, CoreEvent, ExecutionInput,
        ExecutionRecord, ExecutionState,
    };
    let lease = AttemptLease {
        session_id: "s".into(),
        attempt_id: "a".into(),
        worker_id: "w".into(),
        fence: 9,
    };
    let input = ExecutionInput {
        execution_id: "vm".into(),
        parent_call_id: "original-parent".into(),
        task_id: "host-task".into(),
        source_hash: "sha256:source".into(),
    };
    for request in [
        CoreRequest::BeginExecution {
            lease: lease.clone(),
            input: input.clone(),
        },
        CoreRequest::PlanCapability {
            lease: lease.clone(),
            input: CapabilityInput {
                key: CapabilityCallKey {
                    execution_id: "vm".into(),
                    sequence: 0,
                },
                name: "fs.write".into(),
                arguments: json!({"path":"note.txt"}),
            },
        },
    ] {
        let envelope = ProtocolEnvelope::new("req", request).unwrap();
        let wire = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            ProtocolEnvelope::<CoreRequest>::decode(&wire).unwrap(),
            envelope
        );
    }
    let execution = ExecutionRecord {
        execution_id: input.execution_id,
        parent_call_id: input.parent_call_id,
        task_id: input.task_id,
        source_hash: input.source_hash,
        lease,
        state: ExecutionState::Running,
        terminal: None,
    };
    let event = CoreEvent::ExecutionStarted { execution };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["type"], "execution_started");
    assert_eq!(wire["execution"]["lease"]["fence"], 9);
    assert_eq!(serde_json::from_value::<CoreEvent>(wire).unwrap(), event);
}

#[test]
fn current_protocol_round_trips_and_rejects_unknown_fields_and_empty_request_ids() {
    let envelope = ProtocolEnvelope::new(
        "req-1",
        CoreRequest::Snapshot {
            session_id: "session-1".into(),
        },
    )
    .unwrap();
    let encoded = serde_json::to_string(&envelope).unwrap();
    assert_eq!(
        ProtocolEnvelope::<CoreRequest>::decode(&encoded).unwrap(),
        envelope
    );
    assert!(ProtocolEnvelope::<CoreRequest>::decode(&json!({"version":PROTOCOL_VERSION,"request_id":"req","payload":{"type":"snapshot","session_id":"s","extra":true}}).to_string()).is_err());
    assert!(
        ProtocolEnvelope::<CoreRequest>::decode(
            &json!({"version":PROTOCOL_VERSION,"request_id":"","payload":{"type":"list_sessions"}})
                .to_string()
        )
        .is_err()
    );
    assert!(ProtocolEnvelope::<CoreRequest>::decode(&json!({"version":PROTOCOL_VERSION,"request_id":"req","payload":{"type":"list_sessions"},"extra":true}).to_string()).is_err());
    let response = ProtocolEnvelope::new(
        "req-1",
        CoreResponse::Error {
            code: "needs_reconciliation".into(),
            message: "inspect effects before continuing".into(),
        },
    )
    .unwrap();
    let encoded = serde_json::to_string(&response).unwrap();
    assert_eq!(
        ProtocolEnvelope::<CoreResponse>::decode(&encoded).unwrap(),
        response
    );
}
