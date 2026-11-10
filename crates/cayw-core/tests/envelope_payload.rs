use cayw_core::{
    CoreEvent, CoreRequest, CoreResponse, EventRecord, ProtocolEnvelope, ProtocolPayload,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn nested(depth: usize) -> Value {
    (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
}

#[test]
fn generic_envelope_accepts_owned_json_without_recursive_destruction() {
    const CHILD: &str = "CAYW_CORE_GENERIC_ENVELOPE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "generic_envelope_accepts_owned_json_without_recursive_destruction",
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
                let wrapped = Versioned {
                    revision: 7,
                    payload: Some(vec![Box::new(std::collections::BTreeMap::from([(
                        "deep".to_owned(),
                        nested(200),
                    )]))]),
                };
                let envelope = ProtocolEnvelope::new(id, wrapped);
                if id == "valid" {
                    assert!(envelope.is_ok());
                } else {
                    assert!(envelope.is_err());
                }
                let request = CoreRequest::AppendHostEvent {
                    lease: cayw_core::AttemptLease {
                        session_id: "s".into(),
                        attempt_id: "a".into(),
                        worker_id: "w".into(),
                        fence: 1,
                    },
                    kind: "data".into(),
                    data: nested(200),
                };
                assert!(ProtocolEnvelope::new(id, request).is_ok() == (id == "valid"));
                let event = EventRecord {
                    protocol_version: 3,
                    session_id: "s".into(),
                    cursor: 1,
                    timestamp: 1,
                    attempt: None,
                    event: CoreEvent::Host {
                        kind: "data".into(),
                        data: nested(200),
                    },
                };
                assert!(
                    ProtocolEnvelope::new(
                        id,
                        CoreResponse::Events {
                            events: vec![event]
                        }
                    )
                    .is_ok()
                        == (id == "valid")
                );
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
fn provider_usage_fields_round_trip_large_and_deep_values() {
    let mut message = cayw_ai::AssistantMessage::new(&cayw_ai::Model::new("fixture", "usage"));
    message
        .usage_fields
        .insert("large".into(), Value::String("x".repeat(3 * 1024 * 1024)));
    message.usage_fields.insert("deep".into(), nested(64));
    let envelope = ProtocolEnvelope::new("usage", message.clone()).unwrap();
    let wire = serde_json::to_string(&envelope).unwrap();
    let decoded = ProtocolEnvelope::<cayw_ai::AssistantMessage>::decode(&wire).unwrap();
    assert_eq!(decoded.payload.usage_fields, message.usage_fields);
}

#[test]
fn provider_metadata_round_trips_large_and_deep_values() {
    let mut message = cayw_ai::AssistantMessage::new(&cayw_ai::Model::new("fixture", "metadata"));
    message
        .provider_metadata
        .insert("large".into(), Value::String("x".repeat(3 * 1024 * 1024)));
    message.provider_metadata.insert("deep".into(), nested(64));
    let envelope = ProtocolEnvelope::new("metadata", message.clone()).unwrap();
    let wire = serde_json::to_string(&envelope).unwrap();
    let decoded = ProtocolEnvelope::<cayw_ai::AssistantMessage>::decode(&wire).unwrap();
    assert_eq!(decoded.payload.provider_metadata, message.provider_metadata);
}

#[test]
fn provider_metadata_preserves_valid_replay_data_across_roundtrip() {
    let mut message = cayw_ai::AssistantMessage::new(&cayw_ai::Model::new("fixture", "metadata"));
    message.provider_metadata.insert(
        "replay_signature".into(),
        json!({"item_id": "item-123", "opaque_hash": "sha256-abcdef", "valid": true}),
    );
    let envelope = ProtocolEnvelope::new("metadata", message.clone()).unwrap();
    let wire = serde_json::to_string(&envelope).unwrap();
    let decoded = ProtocolEnvelope::<cayw_ai::AssistantMessage>::decode(&wire).unwrap();
    assert_eq!(decoded.payload.provider_metadata, message.provider_metadata);
}

#[test]
fn decode_rejects_only_serde_recursion_and_syntax_failures() {
    let wire = format!(
        "{{\"version\":3,\"request_id\":\"limits\",\"payload\":{}{}}}",
        "[".repeat(25000),
        "]".repeat(25000)
    );
    assert!(ProtocolEnvelope::<Value>::decode(&wire).is_err());

    // In-process values are accepted; the JSON reader is the boundary that
    // enforces serde_json's recursion limit.
    let deep = nested(200);
    let envelope = ProtocolEnvelope::new(
        "limits",
        Versioned {
            revision: 7,
            payload: deep.clone(),
        },
    )
    .unwrap();
    assert_eq!(envelope.payload.payload, deep);
    let wire =
        json!({"version": 3, "request_id": "limits", "payload": {"revision": 7, "payload": deep}})
            .to_string();
    assert!(ProtocolEnvelope::<Versioned<Value>>::decode(&wire).is_err());
}
