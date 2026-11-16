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
async fn slow_reader_receives_every_event_with_backpressure() {
    let (observer, mut reader) = Observer::channel(ObserverConfig {
        capacity: 2,
        ..Default::default()
    })
    .unwrap();
    for n in 0..2 {
        assert_eq!(
            observer.publish(observation(n)).await,
            PublishOutcome::Queued
        );
    }
    let pending = observer.publish(observation(2));
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    assert_eq!(reader.recv().await.unwrap().fields["count"], 0);
    assert_eq!(pending.await, PublishOutcome::Queued);
    let (produced, ()) = tokio::join!(
        async {
            for n in 3..1000 {
                assert_eq!(
                    observer.publish(observation(n)).await,
                    PublishOutcome::Queued
                );
            }
            observer.stats().queued
        },
        async {
            for n in 1..1000 {
                assert_eq!(reader.recv().await.unwrap().fields["count"], n);
            }
        }
    );
    assert_eq!(produced, 1000);
}
#[tokio::test]
async fn unloading_wakes_a_waiting_publisher_and_preserves_queued_events() {
    let (observer, mut reader) = Observer::channel(ObserverConfig {
        capacity: 1,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        observer.publish(observation(0)).await,
        PublishOutcome::Queued
    );
    let pending = observer.publish(observation(1));
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    observer.request_shutdown();
    assert_eq!(pending.await, PublishOutcome::DroppedClosed);
    assert_eq!(reader.recv().await.unwrap().fields["count"], 0);
    assert_eq!(reader.recv().await, None);
}
#[tokio::test]
async fn reader_failure_close_and_plugin_unload_do_not_block_producer() {
    let (observer, mut reader) = Observer::channel(ObserverConfig::default()).unwrap();
    reader.close();
    assert_eq!(
        observer.publish(observation(0)).await,
        PublishOutcome::DroppedClosed
    );
    let (observer, reader) = Observer::channel(ObserverConfig::default()).unwrap();
    let failed = tokio::spawn(async move {
        drop(reader);
        Err::<(), _>("sink failed")
    });
    assert_eq!(failed.await.unwrap(), Err("sink failed"));
    assert_eq!(
        observer.publish(observation(0)).await,
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
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    assert_eq!(stats["queued"], 0);
    registry.unload("observer").unwrap();
    assert_eq!(
        observer.publish(observation(0)).await,
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
        observer
            .publish(Observation {
                kind: "event".into(),
                fields
            })
            .await,
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
async fn configured_redaction_preserves_large_events() {
    let config = ObserverConfig {
        allow_private: true,
        allow_thinking: true,
        redact_keys: ["customer".into()].into(),
        ..Default::default()
    };
    let (observer, mut reader) = Observer::channel(config).unwrap();
    assert_eq!(
        observer
            .publish(Observation {
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
            })
            .await,
        PublishOutcome::Queued
    );
    let event = reader.recv().await.unwrap();
    assert_eq!(event.fields["thinking"], "explicitly enabled");
    assert_eq!(event.fields["customer"], "[redacted]");
    assert_eq!(event.fields["password"], "[redacted]");
    let mut huge = observation(0);
    huge.fields.get_mut("count").unwrap().value = json!("x".repeat(1024 * 1024));
    assert_eq!(observer.publish(huge).await, PublishOutcome::Queued);
    assert_eq!(
        reader.recv().await.unwrap().fields["count"],
        "x".repeat(1024 * 1024)
    );
    let mut invalid = observation(0);
    invalid.kind = "unsafe kind".into();
    assert_eq!(
        observer.publish(invalid).await,
        PublishOutcome::DroppedInvalid
    );
    assert_eq!(observer.stats().dropped_invalid, 1);
}

#[test]
fn capacity_accepts_tokio_maximum_and_rejects_unrepresentable_values() {
    let maximum = tokio::sync::Semaphore::MAX_PERMITS;
    assert!(
        Observer::channel(ObserverConfig {
            capacity: maximum,
            ..Default::default()
        })
        .is_ok()
    );
    for capacity in [0, maximum + 1] {
        assert!(matches!(
            Observer::channel(ObserverConfig {
                capacity,
                ..Default::default()
            }),
            Err(PluginError::InvalidArguments(_))
        ));
    }
}
