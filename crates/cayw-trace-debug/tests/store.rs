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
        protocol_version: 3,
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
    assert_eq!(reopened.snapshot().projects["exec"].evaluations.len(), 3);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
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
    assert!(!store.snapshot().sessions.contains_key("session"));
    let first = event(1, CoreEvent::AttemptStarted { lease: lease() });
    store.ingest(std::slice::from_ref(&first)).unwrap();
    let sequence = store.snapshot().sequence;
    store.ingest(&[first]).unwrap();
    assert_eq!(store.snapshot().sequence, sequence);
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
