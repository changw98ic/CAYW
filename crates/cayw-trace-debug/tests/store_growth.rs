use cayw_trace_debug::TraceStore;

#[test]
fn capture_can_cross_32_mib_reopen_and_continue() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    let store = TraceStore::open(&path).unwrap();
    let message = "x".repeat(33 * 1024 * 1024);
    store
        .connection_error("session", Some(message.clone()))
        .unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > 32 * 1024 * 1024);
    drop(store);

    let reopened = TraceStore::open(&path).unwrap();
    assert_eq!(
        reopened.snapshot().unwrap().sessions["session"]
            .error
            .as_ref(),
        Some(&message)
    );
    reopened
        .connection_error("next-session", Some("still recording".into()))
        .unwrap();
    drop(reopened);
    let saved = TraceStore::open(&path).unwrap().snapshot().unwrap();
    assert_eq!(saved.sessions["session"].error.as_ref(), Some(&message));
    assert_eq!(
        saved.sessions["next-session"].error.as_deref(),
        Some("still recording")
    );
}

#[test]
fn malformed_large_store_is_an_error_not_an_empty_capture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    std::fs::write(&path, format!("{}{{", " ".repeat(33 * 1024 * 1024))).unwrap();
    assert!(matches!(
        TraceStore::open(&path),
        Err(cayw_trace_debug::Error::Json(_))
    ));
}

fn event(cursor: u64, event: cayw_core::CoreEvent) -> cayw_core::EventRecord {
    cayw_core::EventRecord {
        protocol_version: cayw_core::PROTOCOL_VERSION,
        session_id: "session".into(),
        cursor,
        timestamp: cursor,
        attempt: None,
        event,
    }
}

#[test]
fn ingestion_preserves_more_than_100000_events_and_256_executions() {
    use cayw_core::{AttemptLease, CoreEvent, ExecutionRecord, ExecutionState};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    let store = TraceStore::open(&path).unwrap();
    let mut events: Vec<_> = (1..=100_001)
        .map(|cursor| {
            event(
                cursor,
                CoreEvent::Host {
                    kind: "progress".into(),
                    data: serde_json::json!({"value": cursor}),
                },
            )
        })
        .collect();
    for index in 0..257 {
        events.push(event(
            events.len() as u64 + 1,
            CoreEvent::ExecutionStarted {
                execution: ExecutionRecord {
                    execution_id: format!("execution-{index}"),
                    parent_call_id: format!("call-{index}"),
                    task_id: "task".into(),
                    source_hash: "source".into(),
                    lease: AttemptLease {
                        session_id: "session".into(),
                        attempt_id: "attempt".into(),
                        worker_id: "worker".into(),
                        fence: 1,
                    },
                    state: ExecutionState::Running,
                    terminal: None,
                },
            },
        ));
    }
    store.ingest(&events).unwrap();
    drop(store);
    let reopened = TraceStore::open(&path).unwrap();
    let snapshot = reopened.snapshot().unwrap();
    assert_eq!(snapshot.events, events);
    assert_eq!(snapshot.projects.len(), 257);
    assert_eq!(snapshot.sessions["session"].after, events.len() as u64);
    // Duplicate input remains idempotent; conflicting input still fails.
    reopened.ingest(&events[events.len() - 1..]).unwrap();
    let mut conflict = events.last().unwrap().clone();
    conflict.timestamp += 1;
    assert!(reopened.ingest(&[conflict]).is_err());
}

#[tokio::test]
async fn editing_and_undo_continue_after_256_versions() {
    use cayw_trace_debug::{
        Document, Edit, Evaluation, GraphPlan, Node, Project, Revision, Status, Undo,
    };
    use std::collections::BTreeMap;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    let plan = GraphPlan {
        id: "plan".into(),
        label: "plan".into(),
        entry: "node".into(),
        nodes: vec![Node {
            id: "node".into(),
            label: "node".into(),
            module: "test".into(),
            source: "return {output=input,state=state}".into(),
            params: serde_json::json!({}),
            input_pointer: String::new(),
            implementation: "luau".into(),
        }],
        edges: vec![],
        input: serde_json::json!({}),
        initial_state: serde_json::json!({}),
        context: serde_json::json!({}),
    };
    let mut document = Document::default();
    document.projects.insert(
        "project".into(),
        Project {
            id: "project".into(),
            session_id: "session".into(),
            attempt_id: "attempt".into(),
            task_id: "task".into(),
            source_hash: "source".into(),
            parent_call_id: "call".into(),
            lease: None,
            revisions: (1..=256)
                .map(|version| Revision {
                    version,
                    parent: None,
                    changed_node: None,
                    plan: plan.clone(),
                    created_at_ms: 0,
                    reason: "edit".into(),
                })
                .collect(),
            evaluations: (1..=256)
                .map(|version| Evaluation {
                    id: version.to_string(),
                    version,
                    mode: "replay".into(),
                    status: Status::Blocked,
                    steps: vec![],
                    output: None,
                    gaps: vec![],
                    elapsed_ms: 0.0,
                    saved_at_ms: 0,
                    executed_nodes: vec![],
                })
                .collect(),
            calls: vec![],
            gaps: vec![],
            active_node: None,
            trace_error: None,
            runtime_status: Status::Running,
            symbol_hangs: BTreeMap::new(),
        },
    );
    serde_json::to_writer(std::fs::File::create(&path).unwrap(), &document).unwrap();
    let store = TraceStore::open(&path).unwrap();
    let edited = store
        .edit(
            "project",
            Edit {
                expected_version: 256,
                node_id: "node".into(),
                source: plan.nodes[0].source.clone(),
                params: serde_json::json!({}),
                input_pointer: String::new(),
            },
            cayw_ai::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(edited.projects["project"].version(), 257);
    let undone = store
        .undo(
            "project",
            Undo {
                expected_version: 257,
                target_version: 1,
            },
            cayw_ai::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(undone.projects["project"].version(), 258);
    drop(store);
    let saved = TraceStore::open(&path).unwrap().snapshot().unwrap();
    assert_eq!(saved.projects["project"].revisions.len(), 258);
    assert_eq!(saved.projects["project"].evaluations.len(), 258);
}

#[test]
fn large_event_capture_reopens_without_losing_payloads_or_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    let store = TraceStore::open(&path).unwrap();
    // Individual records remain smaller than the existing IPC frame size.
    let payload = "x".repeat(4 * 1024 * 1024);
    let events: Vec<_> = (1..=9)
        .map(|cursor| {
            event(
                cursor,
                cayw_core::CoreEvent::Host {
                    kind: "progress".into(),
                    data: serde_json::json!({"payload": payload}),
                },
            )
        })
        .collect();
    store.ingest(&events).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > 32 * 1024 * 1024);
    drop(store);
    let store = TraceStore::open(&path).unwrap();
    assert_eq!(store.snapshot().unwrap().events, events);
    let next = event(
        10,
        cayw_core::CoreEvent::Host {
            kind: "progress".into(),
            data: serde_json::json!({"done": true}),
        },
    );
    store.ingest(std::slice::from_ref(&next)).unwrap();
    drop(store);
    let saved = TraceStore::open(&path).unwrap().snapshot().unwrap();
    assert_eq!(saved.sessions["session"].after, 10);
    assert_eq!(saved.events.len(), 10);
    assert_eq!(saved.events.last(), Some(&next));
}

#[test]
fn streaming_io_errors_keep_their_io_category() {
    struct FailedWriter;
    impl std::io::Write for FailedWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "disk full",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let error = serde_json::to_writer(FailedWriter, &serde_json::json!({"event": 1})).unwrap_err();
    match cayw_trace_debug::Error::from(error) {
        cayw_trace_debug::Error::Io(error) => {
            assert_eq!(error.kind(), std::io::ErrorKind::StorageFull)
        }
        other => panic!("expected I/O failure, got {other}"),
    }
}
