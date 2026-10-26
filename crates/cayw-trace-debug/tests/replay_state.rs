use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost, CodeMode, Limits};
use cayw_core::{
    AttemptLease, CoreEvent, EventRecord, ExecutionOutcome, ExecutionRecord, ExecutionState,
};
use cayw_trace_debug::{Edit, GraphPlan, Node, Status, TraceStore, Undo, engine};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

struct NoHost;

#[async_trait]
impl CapabilityHost for NoHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![]
    }

    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        panic!("the shared state fixture must not call the Host")
    }
}

fn node(id: &str, source: &str) -> Node {
    Node {
        id: id.into(),
        label: id.into(),
        module: "shared-state".into(),
        source: source.into(),
        params: json!({"factor":1}),
        input_pointer: String::new(),
        implementation: "test:shared-state".into(),
    }
}

/// Node-save `recompute` and undo `replay` must both preserve upstream context
/// mutations and shared Luau table identity. Recompute still marks upstream
/// steps as reused, but the VM runs from the entry so those semantics hold.
async fn check_saved_and_undone_results(a: &str, b: &str, context: Value, pointer: &str) {
    let mut b = node("b", b);
    b.input_pointer = pointer.into();
    let plan = GraphPlan {
        id: "shared-state".into(),
        label: "Shared state".into(),
        entry: "a".into(),
        nodes: vec![node("a", a), b.clone()],
        edges: vec![cayw_trace_debug::Edge {
            from: "a".into(),
            to: "b".into(),
            branch: None,
        }],
        input: json!({}),
        initial_state: json!({}),
        context,
    };
    let source = engine::script_for_plan(&plan).unwrap();
    let report = CodeMode::new(Arc::new(NoHost), Limits::default())
        .unwrap()
        .execute(&source, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.return_value["output"], 9);
    let lease = AttemptLease {
        session_id: "session".into(),
        attempt_id: "attempt".into(),
        worker_id: "worker".into(),
        fence: 1,
    };
    let mut execution = ExecutionRecord {
        execution_id: "execution".into(),
        parent_call_id: "parent".into(),
        task_id: "task".into(),
        source_hash: format!("sha256:{:x}", Sha256::digest(source.as_bytes())),
        lease: lease.clone(),
        state: ExecutionState::Running,
        terminal: None,
    };
    let event = |cursor, event| EventRecord {
        protocol_version: 3,
        session_id: "session".into(),
        cursor,
        timestamp: 1000 + cursor,
        attempt: Some(lease.clone()),
        event,
    };
    let mut events = vec![event(
        1,
        CoreEvent::ExecutionStarted {
            execution: execution.clone(),
        },
    )];
    for (index, output) in report.outputs.iter().enumerate() {
        events.push(event(
            index as u64 + 2,
            CoreEvent::Host {
                kind: "trace_node".into(),
                data: json!({
                    "execution_id":execution.execution_id,
                    "parent_call_id":execution.parent_call_id,
                    "task_id":execution.task_id,
                    "source_hash":execution.source_hash,
                    "entry":{"cursor":index+1,"output":{"type":"json","value":output}}
                }),
            },
        ));
    }
    execution.state = ExecutionState::Completed;
    execution.terminal = Some(ExecutionOutcome::Completed {
        result: json!({"cell_id":"execution"}),
    });
    events.push(event(
        events.len() as u64 + 1,
        CoreEvent::ExecutionFinished { execution },
    ));
    let dir = tempfile::tempdir().unwrap();
    let store = TraceStore::open(dir.path().join("trace.json")).unwrap();
    store.ingest(&events).unwrap();
    let captured = store.snapshot();
    let observed = captured.projects["execution"].evaluations[0].clone();
    assert_eq!(observed.status, Status::Completed);
    assert_eq!(observed.mode, "observed");
    assert_eq!(observed.output, Some(json!(9)));
    assert_eq!(captured.projects["execution"].trace_error, None);

    let commented = format!("{}\n-- same behavior\n", b.source);
    for (version, params, expected) in [(1, json!({"factor":1}), 9), (2, json!({"factor":2}), 18)] {
        let document = store
            .edit(
                "execution",
                Edit {
                    expected_version: version,
                    node_id: "b".into(),
                    source: commented.clone(),
                    params,
                    input_pointer: pointer.into(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let project = &document.projects["execution"];
        let result = project.latest().unwrap();
        assert_eq!(result.status, Status::Completed, "{result:?}");
        assert_eq!(result.mode, "recompute", "{result:?}");
        assert!(result.id.starts_with("recompute:"), "{result:?}");
        assert!(
            result.steps[0].reused && !result.steps[1].reused,
            "upstream reused; edited node recomputed: {result:?}"
        );
        assert_eq!(result.output, Some(json!(expected)), "{result:?}");
        assert!(result.gaps.is_empty(), "{result:?}");
        assert_eq!(project.evaluations[0], observed);
        assert_eq!(result.version, version + 1);
    }
    let document = store
        .undo(
            "execution",
            Undo {
                expected_version: 3,
                target_version: 1,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let project = &document.projects["execution"];
    let result = project.latest().unwrap();
    assert_eq!(result.status, Status::Completed, "{result:?}");
    assert_eq!(result.mode, "replay", "{result:?}");
    assert!(result.id.starts_with("replay:"), "{result:?}");
    assert!(
        result.steps.iter().all(|step| !step.reused),
        "undo full-entry replay recomputes every step: {result:?}"
    );
    assert_eq!(
        result.output,
        Some(json!(9)),
        "undo full-entry replay keeps shared identity: {result:?}"
    );
    assert!(result.gaps.is_empty(), "{result:?}");
    assert_eq!(project.evaluations[0], observed);
    assert_eq!(document.events, captured.events);
}

#[tokio::test]
async fn edits_and_undo_keep_upstream_context_changes() {
    check_saved_and_undone_results(
        "context.n=9; return {output=input,state=state}",
        "return {output=context.n * params.factor,state=state}",
        json!({"n":1}),
        "",
    )
    .await;
}

#[tokio::test]
async fn edits_and_undo_keep_shared_input_and_state() {
    check_saved_and_undone_results(
        "return {output=state,state=state}",
        "state.n=9; return {output=(input.n or 1) * params.factor,state=state}",
        json!({}),
        "",
    )
    .await;
}

#[tokio::test]
async fn input_mapping_keeps_nested_state_and_context_references() {
    check_saved_and_undone_results(
        "state.item=context; return {output={payload=context},state=state}",
        "state.item.n=9; return {output=input.n * params.factor,state=state}",
        json!({"n":1}),
        "/payload",
    )
    .await;
}
