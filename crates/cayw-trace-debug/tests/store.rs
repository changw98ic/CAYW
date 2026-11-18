use cayw_ai::CancellationToken;
use cayw_core::{
    AttemptLease, CoreEvent, EventRecord, ExecutionOutcome, ExecutionRecord, ExecutionState,
};
use cayw_trace_debug::{
    Edit, Error, GraphPlan, Node, NodeTrace, Status, TraceEnvelope, TraceStore, Undo,
};
use serde_json::json;

fn plan() -> GraphPlan {
    GraphPlan {
        id: "store-plan".into(),
        label: "持久版本".into(),
        entry: "input".into(),
        nodes: vec![Node {
            id: "input".into(),
            label: "输入".into(),
            module: "计算".into(),
            source: "return {output=input.value + params.offset, state=state}".into(),
            params: json!({"offset":0}),
            input_pointer: String::new(),
            implementation: "test:input".into(),
        }],
        edges: vec![],
        input: json!({"value":4}),
        initial_state: json!({"count":0}),
        context: json!({}),
    }
}
fn source_hash() -> String {
    use sha2::{Digest, Sha256};
    format!(
        "sha256:{:x}",
        Sha256::digest(
            cayw_trace_debug::engine::script_for_plan(&plan())
                .unwrap()
                .as_bytes()
        )
    )
}

fn lease() -> AttemptLease {
    AttemptLease {
        session_id: "session".into(),
        attempt_id: "attempt".into(),
        worker_id: "worker".into(),
        fence: 1,
    }
}
fn event(cursor: u64, event: CoreEvent) -> EventRecord {
    EventRecord {
        protocol_version: cayw_core::PROTOCOL_VERSION,
        session_id: "session".into(),
        cursor,
        timestamp: cayw_trace_debug::now_ms(),
        attempt: Some(lease()),
        event,
    }
}
fn execution() -> ExecutionRecord {
    ExecutionRecord {
        execution_id: "exec".into(),
        parent_call_id: "tool".into(),
        task_id: "task".into(),
        source_hash: source_hash(),
        lease: lease(),
        state: ExecutionState::Running,
        terminal: None,
    }
}
fn initialized(path: &std::path::Path) -> std::sync::Arc<TraceStore> {
    let store = TraceStore::open(path).unwrap();
    // This fixture supplies wire-shaped snapshots without a Runtime.
    let envelope = TraceEnvelope {
        schema: cayw_trace_debug::TRACE_SCHEMA.into(),
        trace: NodeTrace::Manifest { plan: plan() },
    };
    let trace = event(
        2,
        CoreEvent::Host {
            kind: "trace_node".into(),
            data: json!({"execution_id":"exec","task_id":"task","source_hash":source_hash(),"parent_call_id":"tool","entry":{"cursor":1,"output":{"type":"json","value":envelope}}}),
        },
    );
    store
        .ingest(&[
            event(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(),
                },
            ),
            trace,
        ])
        .unwrap();
    let started = TraceEnvelope {
        schema: cayw_trace_debug::TRACE_SCHEMA.into(),
        trace: NodeTrace::Started {
            node_id: "input".into(),
            input: json!({"value":4}),
            state_before: json!({"count":0}),
            context_before: json!(null),
            seed: None,
        },
    };
    let done = TraceEnvelope {
        schema: cayw_trace_debug::TRACE_SCHEMA.into(),
        trace: NodeTrace::Completed {
            node_id: "input".into(),
            result: cayw_trace_debug::NodeResult {
                output: json!(4),
                state: json!({"count":0}),
                branch: None,
            },
            elapsed_ms: 1.0,
        },
    };
    let trace = |cursor, value| {
        event(
            cursor,
            CoreEvent::Host {
                kind: "trace_node".into(),
                data: json!({"execution_id":"exec","task_id":"task","source_hash":source_hash(),"parent_call_id":"tool","entry":{"cursor":cursor-1,"output":{"type":"json","value":value}}}),
            },
        )
    };
    let mut finished = execution();
    finished.state = ExecutionState::Completed;
    finished.terminal = Some(ExecutionOutcome::Completed {
        result: json!({"status":"completed"}),
    });
    store
        .ingest(&[
            trace(3, json!(started)),
            trace(4, json!(done)),
            event(
                5,
                CoreEvent::ExecutionFinished {
                    execution: finished,
                },
            ),
        ])
        .unwrap();
    store
}

#[tokio::test]
async fn edits_reopen_undo_and_stale_saves_preserve_actual_results() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    let store = initialized(&path);
    let edit = Edit {
        expected_version: 1,
        node_id: "input".into(),
        source: plan().nodes[0].source.clone(),
        params: json!({"offset":3}),
        input_pointer: String::new(),
    };
    let document = store
        .edit("exec", edit.clone(), CancellationToken::new())
        .await
        .unwrap();
    let project = &document.projects["exec"];
    assert_eq!(project.version(), 2);
    assert_eq!(project.latest().unwrap().output, Some(json!(7)));
    assert_eq!(project.latest().unwrap().status, Status::Completed);
    assert_eq!(project.evaluations[0].output, Some(json!(4)));
    assert!(matches!(
        store.edit("exec", edit, CancellationToken::new()).await,
        Err(Error::Conflict(_))
    ));
    let document = store
        .undo(
            "exec",
            Undo {
                expected_version: 2,
                target_version: 1,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let project = &document.projects["exec"];
    assert_eq!(project.version(), 3);
    assert_eq!(project.latest().unwrap().output, Some(json!(4)));
    assert_eq!(project.evaluations.len(), 3);
    assert_eq!(project.evaluations[1].output, Some(json!(7)));
    assert_eq!(document.sessions["session"].after, 5);
    assert!(TraceStore::open(&path).is_err());
    drop(store);
    let reopened = TraceStore::open(&path).unwrap();
    assert_eq!(
        reopened.snapshot().unwrap().projects["exec"]
            .evaluations
            .len(),
        3
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn invalid_edits_fail_before_advancing_the_saved_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    let store = initialized(&path);
    let before = store.snapshot().unwrap();
    let edit = Edit {
        expected_version: 1,
        node_id: "input".into(),
        source: plan().nodes[0].source.clone(),
        params: json!({"offset":1}),
        // A non-empty pointer must start with '/'.
        input_pointer: "value".into(),
    };
    let error = store
        .edit("exec", edit, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Invalid(_)), "{error:?}");
    let after = store.snapshot().unwrap();
    assert_eq!(after.sequence, before.sequence);
    assert_eq!(after.projects["exec"].version(), 1);
    assert_eq!(after.projects["exec"].evaluations.len(), 1);
    drop(store);
    let reopened = TraceStore::open(&path).unwrap();
    let persisted = reopened.snapshot().unwrap();
    assert_eq!(persisted.projects["exec"].version(), 1);
    assert_eq!(persisted.projects["exec"].evaluations.len(), 1);
}

#[test]
fn cursor_gaps_never_advance_saved_position() {
    let dir = tempfile::tempdir().unwrap();
    let store = TraceStore::open(dir.path().join("trace.json")).unwrap();
    assert!(
        store
            .ingest(&[event(2, CoreEvent::AttemptStarted { lease: lease() })])
            .is_err()
    );
    assert!(!store.snapshot().unwrap().sessions.contains_key("session"));
    let first = event(1, CoreEvent::AttemptStarted { lease: lease() });
    store.ingest(std::slice::from_ref(&first)).unwrap();
    let sequence = store.snapshot().unwrap().sequence;
    store.ingest(&[first]).unwrap();
    assert_eq!(store.snapshot().unwrap().sequence, sequence);
}

#[test]
fn symlinks_cannot_replace_an_existing_trace_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.json");
    let target = dir.path().join("user-data");
    std::fs::write(&target, b"untouched").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(TraceStore::open(&path).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
}

#[test]
fn json_migration_preserves_original_bytes_and_appends_transactionally() {
    let dir = tempfile::tempdir().unwrap();
    let original = initialized(&dir.path().join("source.sqlite"))
        .snapshot()
        .unwrap();
    let path = dir.path().join("trace.json");
    let bytes = serde_json::to_vec_pretty(&original).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let store = TraceStore::open(&path).unwrap();
    assert_eq!(
        std::fs::read(path.with_extension("json-evidence.json")).unwrap(),
        bytes
    );
    assert_eq!(
        serde_json::to_value(store.snapshot().unwrap()).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    // Abort after the header update: the whole transaction must roll back.
    connection.execute_batch("CREATE TRIGGER fail_append BEFORE INSERT ON trace_events BEGIN SELECT RAISE(ABORT, 'injected disk write failure'); END;").unwrap();
    let next = event(
        6,
        CoreEvent::Host {
            kind: "verification".into(),
            data: json!({"retained":true}),
        },
    );
    assert!(store.ingest(std::slice::from_ref(&next)).is_err());
    assert_eq!(store.session_cursor("session"), 5);
    drop(store);
    let store = TraceStore::open(&path).unwrap();
    assert_eq!(store.session_cursor("session"), 5);
    assert_eq!(store.snapshot().unwrap().events, original.events);
    connection.execute_batch("DROP TRIGGER fail_append; CREATE TRIGGER preserve_raw BEFORE UPDATE ON trace_events BEGIN SELECT RAISE(ABORT, 'historical rewrite'); END;").unwrap();
    store.ingest(std::slice::from_ref(&next)).unwrap();
    store
        .connection_error("session", Some("disconnected".into()))
        .unwrap();
    drop(store);
    let store = TraceStore::open(&path).unwrap();
    assert_eq!(store.session_cursor("session"), 6);
    assert_eq!(
        &store.snapshot().unwrap().events[..5],
        original.events.as_slice()
    );
    assert_eq!(store.snapshot().unwrap().events[5], next);
}
