#![cfg(feature = "debug")]
use cayw_plugins::{debug::*, *};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

fn record(n: u64) -> BTreeMap<String, Value> {
    [("count".into(), json!(n))].into()
}

#[tokio::test]
async fn slow_reader_drops_full_records_without_blocking() {
    let (debug, mut reader) = DebugLog::channel(DebugConfig {
        capacity: 2,
        ..Default::default()
    })
    .unwrap();
    let slow = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        [reader.recv().await.unwrap(), reader.recv().await.unwrap()]
    });
    for n in 0..1000 {
        let outcome = debug.publish("tool.completed", record(n));
        if n < 2 {
            assert!(outcome.is_ok());
        } else {
            assert_eq!(outcome.unwrap_err(), PublishOutcome::DroppedFull);
        }
    }
    let events = slow.await.unwrap();
    assert_eq!(events[0].fields["count"], 0);
    assert!(events[0].at_ms > 0);
    assert_eq!(events[1].fields["count"], 1);
    assert_eq!(debug.stats().dropped_full, 998);
    assert_eq!(
        debug.publish("tool.completed", record(1001)).unwrap_err(),
        PublishOutcome::DroppedClosed
    );
}

#[tokio::test]
async fn reader_close_and_unload_return_immediately() {
    let (debug, mut reader) = DebugLog::channel(DebugConfig::default()).unwrap();
    reader.close();
    assert_eq!(
        debug.publish("tool.completed", record(0)).unwrap_err(),
        PublishOutcome::DroppedClosed
    );
    let (debug, mut reader) = DebugLog::channel(DebugConfig::default()).unwrap();
    let registry = PluginRegistry::new();
    registry
        .load_native(
            Arc::new(debug.clone()),
            PluginContext::new(["debug.stats".into()], []),
        )
        .unwrap();
    registry.unload("debug").unwrap();
    assert_eq!(
        debug.publish("tool.completed", record(0)).unwrap_err(),
        PublishOutcome::DroppedClosed
    );
    assert!(matches!(
        registry.check_available("debug"),
        Err(PluginError::NotFound(_))
    ));
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_secs(1),
            registry.call(
                "debug",
                "debug.stats",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1),
            ),
        )
        .await,
        Ok(Err(PluginError::NotFound(_)))
    ));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), reader.recv())
            .await
            .unwrap(),
        None
    );
}

fn source(cursor: u64) -> SourceEvent {
    SourceEvent {
        cursor,
        at_ms: 1_700_000_000_000 + cursor,
        session_id: "session-1".into(),
        kind: "capability_settled".into(),
        fields: [
            ("tool".into(), json!("fs.read")),
            ("call_id".into(), json!(format!("call-{cursor}"))),
            ("failed".into(), json!(false)),
            (
                "conversation".into(),
                json!([{"role":"user","text":"read the file"}]),
            ),
            ("api_key".into(), json!("hidden")),
        ]
        .into(),
    }
}

#[tokio::test]
async fn follow_copies_runtime_events_and_disable_stops_copying() {
    let (debug, mut reader) = DebugLog::channel(DebugConfig::default()).unwrap();
    let follow = DebugFollow::new(debug);
    let copied = follow.sync(&[source(1), source(2)]);
    assert_eq!(copied.len(), 2);
    assert_eq!(copied[0].cursor, 1);
    assert_eq!(copied[1].cursor, 2);
    let first = reader.recv().await.unwrap();
    assert_eq!(first.kind, "capability_settled");
    assert!(first.at_ms > 0);
    assert_eq!(first.fields["event_at_ms"], 1_700_000_000_001_u64);
    assert_eq!(first.fields["session"], "session-1");
    assert_eq!(first.fields["cursor"], 1);
    assert_eq!(first.fields["tool"], "fs.read");
    assert_eq!(first.fields["conversation"][0]["text"], "read the file");
    assert_eq!(first.fields["api_key"], "[redacted]");
    follow.disable();
    let paused = follow.sync(&[source(3)]);
    assert_eq!(paused.len(), 1);
    assert_eq!(paused[0].cursor, 3);
    assert!(paused[0].record.is_none());
    follow.enable();
    let resumed = follow.sync(&[source(4)]);
    assert_eq!(resumed[0].cursor, 4);
    assert!(resumed[0].record.is_some());
    assert_eq!(reader.recv().await.unwrap().fields["cursor"], 2);
    assert_eq!(reader.recv().await.unwrap().fields["cursor"], 4);
}

#[tokio::test]
async fn sensitive_keys_are_removed_without_caller_classification() {
    let (debug, mut reader) = DebugLog::channel(DebugConfig::default()).unwrap();
    let fields = BTreeMap::from([
        (
            "conversation".into(),
            json!([{"role":"user","text":"read the file"}]),
        ),
        ("reason".into(), json!("read failed: missing file")),
        (
            "data".into(),
            json!({"safe":1,"nested":[{"api_key":"hidden","Authorization":"Bearer hidden","reasoning":"hidden"}]}),
        ),
        ("password".into(), json!("hidden")),
    ]);
    assert!(debug.publish("tool.completed", fields).is_ok());
    let event = reader.recv().await.unwrap();
    assert!(event.at_ms > 0);
    assert_eq!(event.fields["conversation"][0]["text"], "read the file");
    assert_eq!(event.fields["reason"], "read failed: missing file");
    assert_eq!(event.fields["data"]["safe"], 1);
    assert_eq!(event.fields["data"]["nested"][0]["api_key"], "[redacted]");
    assert_eq!(
        event.fields["data"]["nested"][0]["Authorization"],
        "[redacted]"
    );
    assert_eq!(event.fields["data"]["nested"][0]["reasoning"], "[redacted]");
    assert_eq!(event.fields["password"], "[redacted]");
}
