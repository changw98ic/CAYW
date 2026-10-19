use cayw_core::{
    CoreEvent, CoreRequest, CoreResponse, EventRecord, ProtocolEnvelope, ProtocolPayload,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn nested(depth: usize) -> Value {
    (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
}

#[test]
fn generic_envelope_rejects_and_disposes_owned_json_request_and_response_leaves() {
    const CHILD: &str = "CAYW_CORE_GENERIC_ENVELOPE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "generic_envelope_rejects_and_disposes_owned_json_request_and_response_leaves",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "generic envelope child {:?}: {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            for id in ["", "valid", &"x".repeat(257)] {
                assert!(ProtocolEnvelope::new(id, nested(25_000)).is_err());
                let request = CoreRequest::AppendHostEvent {
                    lease: cayw_core::AttemptLease {
                        session_id: "s".into(),
                        attempt_id: "a".into(),
                        worker_id: "w".into(),
                        fence: 1,
                    },
                    kind: "data".into(),
                    data: nested(25_000),
                };
                assert!(ProtocolEnvelope::new(id, request).is_err());
                let event = EventRecord {
                    protocol_version: 3,
                    session_id: "s".into(),
                    cursor: 1,
                    timestamp: 1,
                    attempt: None,
                    event: CoreEvent::Host {
                        kind: "data".into(),
                        data: nested(25_000),
                    },
                };
                assert!(
                    ProtocolEnvelope::new(
                        id,
                        CoreResponse::Events {
                            events: vec![event]
                        }
                    )
                    .is_err()
                );
                let wrapped = Versioned {
                    revision: 7,
                    payload: Some(vec![Box::new(std::collections::BTreeMap::from([(
                        "deep".to_owned(),
                        nested(25_000),
                    )]))]),
                };
                assert!(ProtocolEnvelope::new(id, wrapped).is_err());
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Versioned<T> {
    revision: u32,
    payload: T,
}

impl<T: ProtocolPayload> ProtocolPayload for Versioned<T> {
    fn json_values(&self) -> Vec<&Value> {
        self.payload.json_values()
    }
    fn json_values_mut(&mut self) -> Vec<&mut Value> {
        self.payload.json_values_mut()
    }
}

#[test]
fn custom_versioned_payload_and_scalar_payloads_round_trip() {
    let envelope = ProtocolEnvelope::new(
        "custom",
        Versioned {
            revision: 7,
            payload: json!({"operation":"retained"}),
        },
    )
    .unwrap();
    let wire = serde_json::to_string(&envelope).unwrap();
    assert_eq!(
        ProtocolEnvelope::<Versioned<Value>>::decode(&wire).unwrap(),
        envelope
    );
    assert!(
        ProtocolEnvelope::new(
            "",
            Versioned {
                revision: 7,
                payload: json!({"operation":"retained"})
            }
        )
        .is_err()
    );
    let scalar = ProtocolEnvelope::new("scalar", "retained".to_owned()).unwrap();
    assert_eq!(
        ProtocolEnvelope::<String>::decode(&serde_json::to_string(&scalar).unwrap()).unwrap(),
        scalar
    );
    let containers = ProtocolEnvelope::new(
        "containers",
        Versioned {
            revision: 8,
            payload: Some(vec![Box::new(std::collections::BTreeMap::from([(
                "data".to_owned(),
                json!({"kept": true}),
            )]))]),
        },
    )
    .unwrap();
    assert_eq!(ProtocolEnvelope::<Versioned<Option<Vec<Box<std::collections::BTreeMap<String, Value>>>>>>::decode(
        &serde_json::to_string(&containers).unwrap()).unwrap(), containers);
    let scalar = ProtocolEnvelope::new("number", Some(vec![Box::new(42_u64)])).unwrap();
    assert_eq!(
        ProtocolEnvelope::<Option<Vec<Box<u64>>>>::decode(&serde_json::to_string(&scalar).unwrap())
            .unwrap(),
        scalar
    );
}

#[test]
fn decode_and_new_apply_the_same_json_leaf_limits() {
    for data in [
        nested(33),
        Value::Array(vec![Value::Null; 4096]),
        Value::String("x".repeat(65537)),
    ] {
        let wire = json!({"version": 3, "request_id": "limits", "payload": {"revision": 7, "payload": data.clone()}}).to_string();
        assert!(ProtocolEnvelope::<Versioned<Value>>::decode(&wire).is_err());
        assert!(
            ProtocolEnvelope::new(
                "limits",
                Versioned {
                    revision: 7,
                    payload: data
                }
            )
            .is_err()
        );
    }
    let wire = format!(
        "{{\"version\":3,\"request_id\":\"limits\",\"payload\":{}{}}}",
        "[".repeat(25000),
        "]".repeat(25000)
    );
    assert!(ProtocolEnvelope::<Value>::decode(&wire).is_err());
}
