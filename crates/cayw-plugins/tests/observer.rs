#![cfg(feature = "observer")]
use cayw_plugins::{observer::*, *};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

fn observation(n: u64) -> Observation {
    Observation {
        kind: "tool.completed".into(),
        fields: [(
            "count".into(),
            ObservedField {
                value: json!(n),
                sensitivity: Sensitivity::Public,
            },
        )]
        .into(),
    }
}
#[tokio::test]
async fn slow_reader_drops_full_events_without_blocking_and_can_resume() {
    let (observer, mut reader) = Observer::channel(ObserverConfig {
        capacity: 2,
        ..Default::default()
    })
    .unwrap();
    let slow = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        [reader.recv().await.unwrap(), reader.recv().await.unwrap()]
    });
    for n in 0..1000 {
        assert_eq!(
            observer.publish(observation(n)),
            if n < 2 {
                PublishOutcome::Queued
            } else {
                PublishOutcome::DroppedFull
            }
        );
    }
    let events = slow.await.unwrap();
    assert_eq!(events[0].fields["count"], 0);
    assert_eq!(events[1].fields["count"], 1);
    assert_eq!(observer.stats().dropped_full, 998);
    assert_eq!(
        observer.publish(observation(1001)),
        PublishOutcome::DroppedClosed
    );
}
#[tokio::test]
async fn reader_failure_close_and_plugin_unload_do_not_block_producer() {
    let (observer, mut reader) = Observer::channel(ObserverConfig::default()).unwrap();
    reader.close();
    assert_eq!(
        observer.publish(observation(0)),
        PublishOutcome::DroppedClosed
    );
    let (observer, reader) = Observer::channel(ObserverConfig::default()).unwrap();
    let failed = tokio::spawn(async move {
        drop(reader);
        Err::<(), _>("sink failed")
    });
    assert_eq!(failed.await.unwrap(), Err("sink failed"));
    assert_eq!(
        observer.publish(observation(0)),
        PublishOutcome::DroppedClosed
    );
    let (observer, mut reader) = Observer::channel(ObserverConfig::default()).unwrap();
    let registry = PluginRegistry::new();
    registry
        .load_native(
            Arc::new(observer.clone()),
            PluginContext::new(["observer.stats".into()], []),
        )
        .unwrap();
    let stats = registry
        .call(
            "observer",
            "observer.stats",
            json!({}),
            CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_eq!(stats["queued"], 0);
    registry.unload("observer").unwrap();
    assert_eq!(
        observer.publish(observation(0)),
        PublishOutcome::DroppedClosed
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), reader.recv())
            .await
            .unwrap(),
        None
    );
}
#[tokio::test]
async fn default_redaction_removes_thinking_credentials_private_and_nested_keys() {
    let (observer, mut reader) = Observer::channel(ObserverConfig::default()).unwrap();
    let fields = BTreeMap::from([
        (
            "thinking".into(),
            ObservedField {
                value: json!("reason"),
                sensitivity: Sensitivity::Thinking,
            },
        ),
        (
            "text".into(),
            ObservedField {
                value: json!("private content"),
                sensitivity: Sensitivity::Private,
            },
        ),
        (
            "credential".into(),
            ObservedField {
                value: json!("secret"),
                sensitivity: Sensitivity::Credential,
            },
        ),
        (
            "data".into(),
            ObservedField {
                value: json!({"safe":1,"nested":[{"api_key":"hidden","Authorization":"Bearer hidden","reasoning":"hidden"}]}),
                sensitivity: Sensitivity::Public,
            },
        ),
    ]);
    assert_eq!(
        observer.publish(Observation {
            kind: "event".into(),
            fields
        }),
        PublishOutcome::Queued
    );
    let event = reader.recv().await.unwrap();
    assert_eq!(event.fields["thinking"], "[redacted]");
    assert_eq!(event.fields["text"], "[redacted]");
    assert_eq!(event.fields["credential"], "[redacted]");
    assert_eq!(event.fields["data"]["safe"], 1);
    assert_eq!(event.fields["data"]["nested"][0]["api_key"], "[redacted]");
    assert_eq!(event.fields["data"]["nested"][0]["reasoning"], "[redacted]");
}
#[tokio::test]
async fn configured_redaction_and_event_size_limits_apply_before_enqueue() {
    let config = ObserverConfig {
        allow_private: true,
        allow_thinking: true,
        redact_keys: ["customer".into()].into(),
        max_event_bytes: 256,
        ..Default::default()
    };
    let (observer, mut reader) = Observer::channel(config).unwrap();
    assert_eq!(
        observer.publish(Observation {
            kind: "event".into(),
            fields: [
                (
                    "thinking".into(),
                    ObservedField {
                        value: json!("explicitly enabled"),
                        sensitivity: Sensitivity::Thinking
                    }
                ),
                (
                    "customer".into(),
                    ObservedField {
                        value: json!("hidden"),
                        sensitivity: Sensitivity::Private
                    }
                ),
                (
                    "password".into(),
                    ObservedField {
                        value: json!("hidden"),
                        sensitivity: Sensitivity::Public
                    }
                ),
            ]
            .into()
        }),
        PublishOutcome::Queued
    );
    let event = reader.recv().await.unwrap();
    assert_eq!(event.fields["thinking"], "explicitly enabled");
    assert_eq!(event.fields["customer"], "[redacted]");
    assert_eq!(event.fields["password"], "[redacted]");
    let mut huge = observation(0);
    huge.fields.get_mut("count").unwrap().value = json!("x".repeat(300));
    assert_eq!(observer.publish(huge), PublishOutcome::DroppedOversize);
    let mut invalid = observation(0);
    invalid.kind = "unsafe kind".into();
    assert_eq!(observer.publish(invalid), PublishOutcome::DroppedInvalid);
    assert_eq!(observer.stats().dropped_oversize, 1);
    assert_eq!(observer.stats().dropped_invalid, 1);
}
