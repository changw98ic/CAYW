use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost, CodeMode, Limits};
use cayw_core::CapabilityOutcome;
use cayw_trace_debug::engine::{evaluate, script_for_plan, validate_plan};
use cayw_trace_debug::types::{
    Edge, Evaluation, GraphPlan, Node, NodeTrace, RecordedCall, Status, Step, TraceEnvelope,
};
use serde_json::{Value, json};
use std::sync::Arc;

fn node(id: &str, source: &str, pointer: &str) -> Node {
    Node {
        id: id.into(),
        label: id.into(),
        module: "demo".into(),
        source: source.into(),
        params: json!({}),
        input_pointer: pointer.into(),
        implementation: "local".into(),
    }
}

fn edge(from: &str, to: &str, branch: Option<&str>) -> Edge {
    Edge {
        from: from.into(),
        to: to.into(),
        branch: branch.map(str::to_owned),
    }
}

fn plan(entry: &str, nodes: Vec<Node>, edges: Vec<Edge>, input: Value) -> GraphPlan {
    GraphPlan {
        id: "plan".into(),
        label: "plan".into(),
        entry: entry.into(),
        nodes,
        edges,
        input,
        initial_state: json!({}),
        context: json!({"context": true}),
    }
}

fn step(
    node_id: &str,
    input: Value,
    state_before: Value,
    output: Value,
    state_after: Value,
    branch: Option<&str>,
    evidence: u64,
) -> Step {
    Step {
        node_id: node_id.into(),
        status: Status::Completed,
        input,
        state_before,
        context_before: Value::Null,
        seed: None,
        output: Some(output),
        state_after: Some(state_after),
        branch: branch.map(str::to_owned),
        error: None,
        started_at_ms: 1,
        elapsed_ms: 1.0,
        evidence_cursor: Some(evidence),
        reused: false,
    }
}

fn observed(steps: Vec<Step>) -> Evaluation {
    let output = steps.last().and_then(|step| step.output.clone());
    Evaluation {
        id: "observed:execution".into(),
        version: 0,
        mode: "observed".into(),
        status: Status::Completed,
        steps,
        output,
        gaps: Vec::new(),
        elapsed_ms: 5.0,
        saved_at_ms: 1,
        executed_nodes: Vec::new(),
    }
}

fn call(
    node_id: &str,
    name: &str,
    arguments: Value,
    outcome: CapabilityOutcome,
    evidence: u64,
) -> RecordedCall {
    RecordedCall {
        execution_id: "execution".into(),
        sequence: evidence,
        name: name.into(),
        arguments,
        outcome: Some(outcome),
        started_at_ms: Some(1),
        settled_at_ms: Some(2),
        evidence_cursor: evidence,
        node_id: Some(node_id.into()),
    }
}

fn deep_value(depth: usize) -> Value {
    let mut value = json!(1);
    for _ in 0..depth {
        value = json!([value]);
    }
    value
}

#[test]
fn plan_validation_rejects_broken_graphs_and_unsafe_values() {
    let good = plan(
        "a",
        vec![node("a", "return {output = input, state = state}", "")],
        vec![],
        json!({"n": 1}),
    );
    assert!(validate_plan(&good).is_ok());

    let mut missing_entry = good.clone();
    missing_entry.entry = "missing".into();
    assert!(matches!(
        validate_plan(&missing_entry),
        Err(cayw_trace_debug::Error::Invalid(_))
    ));

    let mut duplicate = good.clone();
    duplicate
        .nodes
        .push(node("a", "return {output = 1, state = state}", ""));
    assert!(validate_plan(&duplicate).is_err());

    let mut cycle = plan(
        "a",
        vec![
            node("a", "return {output = 1, state = state}", ""),
            node("b", "return {output = 2, state = state}", ""),
        ],
        vec![edge("a", "b", None), edge("b", "a", None)],
        json!({}),
    );
    assert!(validate_plan(&cycle).is_err());

    cycle
        .nodes
        .push(node("c", "return {output = 3, state = state}", ""));
    assert!(
        validate_plan(&cycle).is_err(),
        "unreachable nodes are rejected"
    );

    let mut bad_pointer = good.clone();
    bad_pointer.nodes[0].input_pointer = "a/b".into();
    assert!(validate_plan(&bad_pointer).is_err());
    bad_pointer.nodes[0].input_pointer = "/a~2".into();
    assert!(validate_plan(&bad_pointer).is_err());

    let mut too_deep = good.clone();
    too_deep.input = deep_value(32);
    assert!(validate_plan(&too_deep).is_err());

    let mut unsafe_integer = good.clone();
    unsafe_integer.input = json!(9_007_199_254_740_992_u64);
    assert!(validate_plan(&unsafe_integer).is_err());

    let mut oversized_source = good.clone();
    oversized_source.nodes[0].source = format!(
        "return {{output = input, state = state}} --{}",
        "x".repeat(16 * 1024)
    );
    assert!(validate_plan(&oversized_source).is_err());

    let mut oversized_plan = good.clone();
    oversized_plan.input = Value::String("x".repeat(300 * 1024));
    assert!(validate_plan(&oversized_plan).is_err());
}

struct NoHost;

#[async_trait]
impl CapabilityHost for NoHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Err(CapabilityError::new("no host"))
    }
}

async fn run_script(script: &str) -> Vec<Value> {
    let mode = CodeMode::new(Arc::new(NoHost), Limits::default()).unwrap();
    mode.execute(script, CancellationToken::new())
        .await
        .unwrap()
        .outputs
}

fn envelopes(outputs: &[Value]) -> Vec<TraceEnvelope> {
    outputs
        .iter()
        .map(|value| serde_json::from_value(value.clone()).expect("trace envelope"))
        .collect()
}

#[tokio::test]
async fn real_script_emits_manifest_and_started_records_the_raw_incoming_value() {
    let plan = plan(
        "read",
        vec![
            node(
                "read",
                "return {output = {value = input.value}, state = state}",
                "",
            ),
            node(
                "double",
                "return {output = input * 2, state = {count = (state.count or 0) + 1}}",
                "/value",
            ),
        ],
        vec![edge("read", "double", None)],
        json!({"value": 21, "ignored": true}),
    );
    let script = script_for_plan(&plan).unwrap();
    let trace = envelopes(&run_script(&script).await);
    assert_eq!(trace.len(), 5, "{trace:#?}");
    match &trace[0].trace {
        NodeTrace::Manifest { plan: embedded } => assert_eq!(embedded, &plan),
        other => panic!("expected manifest, got {other:?}"),
    }
    match &trace[1].trace {
        NodeTrace::Started {
            node_id,
            input,
            state_before,
            context_before,
            seed,
        } => {
            assert_eq!(node_id, "read");
            // Raw incoming is the whole plan input, before the "/value" mapping.
            assert_eq!(input, &json!({"value": 21, "ignored": true}));
            assert_eq!(state_before, &json!({}));
            assert_eq!(context_before, &json!({"context": true}));
            assert!(seed.is_some(), "started traces carry a shared seed");
        }
        other => panic!("expected started, got {other:?}"),
    }
    match &trace[2].trace {
        NodeTrace::Completed {
            node_id, result, ..
        } => {
            assert_eq!(node_id, "read");
            assert_eq!(result.output, json!({"value": 21}));
        }
        other => panic!("expected completed, got {other:?}"),
    }
    match &trace[3].trace {
        NodeTrace::Started { node_id, input, .. } => {
            assert_eq!(node_id, "double");
            // The downstream incoming value is the predecessor output, before
            // this node's own "/value" mapping.
            assert_eq!(input, &json!({"value": 21}));
        }
        other => panic!("expected started, got {other:?}"),
    }
    match &trace[4].trace {
        NodeTrace::Completed {
            node_id, result, ..
        } => {
            assert_eq!(node_id, "double");
            assert_eq!(result.output, json!(42));
            assert_eq!(result.state, json!({"count": 1}));
        }
        other => panic!("expected completed, got {other:?}"),
    }
}

#[tokio::test]
async fn real_script_follows_the_returned_branch() {
    let plan = plan(
        "cond",
        vec![
            node(
                "cond",
                "return {output = input, state = state, branch = input.n >= 0 and \"high\" or \"low\"}",
                "",
            ),
            node("high", "return {output = \"high\", state = state}", ""),
            node("low", "return {output = \"low\", state = state}", ""),
        ],
        vec![
            edge("cond", "high", Some("high")),
            edge("cond", "low", Some("low")),
        ],
        json!({"n": 1}),
    );
    let trace = envelopes(&run_script(&script_for_plan(&plan).unwrap()).await);
    let visited: Vec<String> = trace
        .iter()
        .filter_map(|envelope| match &envelope.trace {
            NodeTrace::Started { node_id, .. } => Some(node_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(visited, vec!["cond".to_owned(), "high".to_owned()]);

    let mut low_plan = plan.clone();
    low_plan.input = json!({"n": -1});
    let trace = envelopes(&run_script(&script_for_plan(&low_plan).unwrap()).await);
    let visited: Vec<String> = trace
        .iter()
        .filter_map(|envelope| match &envelope.trace {
            NodeTrace::Started { node_id, .. } => Some(node_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(visited, vec!["cond".to_owned(), "low".to_owned()]);
}

#[tokio::test]
async fn local_recompute_remaps_raw_input_at_the_edited_node() {
    let base_plan = plan(
        "a",
        vec![
            node(
                "a",
                "return {output = {n = input.n, tag = \"a\"}, state = {count = 1}}",
                "",
            ),
            node(
                "b",
                "return {output = {value = input}, state = {count = state.count + 1}}",
                "/n",
            ),
            node(
                "c",
                "return {output = {final = input}, state = state}",
                "/value",
            ),
        ],
        vec![edge("a", "b", None), edge("b", "c", None)],
        json!({"n": 2}),
    );
    let base = observed(vec![
        step(
            "a",
            json!({"n": 2}),
            json!({}),
            json!({"n": 2, "tag": "a"}),
            json!({"count": 1}),
            None,
            1,
        ),
        step(
            "b",
            json!({"n": 2, "tag": "a"}),
            json!({"count": 1}),
            json!({"value": 2}),
            json!({"count": 2}),
            None,
            2,
        ),
        step(
            "c",
            json!({"value": 2}),
            json!({"count": 2}),
            json!({"final": 2}),
            json!({"count": 2}),
            None,
            3,
        ),
    ]);

    let mut edited = base_plan.clone();
    edited.nodes[1].source =
        "return {output = {value = input * 10}, state = {count = state.count + 1}}".into();
    let evaluated = evaluate(
        &edited,
        Some(&base),
        Some("b"),
        &[],
        1,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(evaluated.status, Status::Completed, "{evaluated:#?}");
    assert_eq!(evaluated.mode, "recompute");
    assert_eq!(evaluated.steps.len(), 3);
    assert!(
        evaluated.steps[0].reused,
        "upstream of the edit is reused from the captured base"
    );
    assert!(
        !evaluated.steps[1].reused && !evaluated.steps[2].reused,
        "the edited node and affected downstream are recomputed"
    );
    assert_eq!(
        evaluated.steps[1].input,
        json!({"n": 2, "tag": "a"}),
        "the raw incoming value stays pre-mapping"
    );
    assert_eq!(
        evaluated.steps[1].state_before,
        json!({"count": 1}),
        "the captured pre-state at the edit point is used"
    );
    assert_eq!(evaluated.steps[1].output, Some(json!({"value": 20})));
    assert_eq!(
        evaluated.steps[2].input,
        json!({"value": 20}),
        "downstream nodes use the new output"
    );
    assert_eq!(evaluated.output, Some(json!({"final": 20})));

    // Changing the pointer must re-map the raw incoming value exactly once.
    let mut remapped = base_plan.clone();
    remapped.nodes[1].input_pointer = String::new();
    remapped.nodes[1].source = "return {output = {value = input}, state = state}".into();
    let evaluated = evaluate(
        &remapped,
        Some(&base),
        Some("b"),
        &[],
        2,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(evaluated.status, Status::Completed, "{evaluated:#?}");
    assert_eq!(evaluated.mode, "recompute");
    assert!(evaluated.steps[0].reused);
    assert!(!evaluated.steps[1].reused);
    assert_eq!(
        evaluated.steps[1].output,
        Some(json!({"value": {"n": 2, "tag": "a"}})),
        "the whole raw incoming value is the new input"
    );
    assert_eq!(
        evaluated.output,
        Some(json!({"final": {"n": 2, "tag": "a"}}))
    );
}

#[tokio::test]
async fn replay_answers_host_calls_only_from_exact_captured_responses() {
    let plan = plan(
        "a",
        vec![node(
            "a",
            "return {output = cayw.call(\"echo\", {value = input.n}), state = state}",
            "",
        )],
        vec![],
        json!({"n": 3}),
    );
    let base = observed(vec![step(
        "a",
        json!({"n": 3}),
        json!({}),
        json!(10),
        json!({}),
        None,
        1,
    )]);
    let captured = vec![call(
        "a",
        "echo",
        json!({"value": 3}),
        CapabilityOutcome::Completed { result: json!(10) },
        9,
    )];

    let evaluated = evaluate(
        &plan,
        Some(&base),
        Some("a"),
        &captured,
        1,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(evaluated.status, Status::Completed, "{evaluated:#?}");
    assert_eq!(evaluated.steps[0].output, Some(json!(10)));
    assert_eq!(evaluated.steps[0].evidence_cursor, Some(9));

    // A different argument is not answered by the captured response.
    let mut changed = plan.clone();
    changed.nodes[0].source =
        "return {output = cayw.call(\"echo\", {value = input.n + 1}), state = state}".into();
    let evaluated = evaluate(
        &changed,
        Some(&base),
        Some("a"),
        &captured,
        2,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(evaluated.status, Status::Blocked, "{evaluated:#?}");
    assert_eq!(
        evaluated.steps[0].status,
        Status::Blocked,
        "a mismatched capture is a replay gap, not an unknown effect"
    );
    assert!(!evaluated.gaps.is_empty());

    // A second call has no captured response; replay stops as blocked.
    let mut twice = plan.clone();
    twice.nodes[0].source = "local first = cayw.call(\"echo\", {value = input.n}); return {output = cayw.call(\"echo\", {value = first}), state = state}".into();
    let evaluated = evaluate(
        &twice,
        Some(&base),
        Some("a"),
        &captured,
        3,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(evaluated.status, Status::Blocked, "{evaluated:#?}");
    assert_eq!(evaluated.steps[0].status, Status::Blocked);
    assert!(
        evaluated
            .gaps
            .iter()
            .any(|gap| gap.message.contains("no captured response"))
    );

    // A capability outside the captured set is not replayable. It is a replay
    // gap, not an unknown effect, because replay never dispatches it.
    let mut new_capability = plan.clone();
    new_capability.nodes[0].source =
        "return {output = cayw.call(\"other\", {value = input.n}), state = state}".into();
    let evaluated = evaluate(
        &new_capability,
        Some(&base),
        Some("a"),
        &captured,
        3,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(evaluated.status, Status::Blocked, "{evaluated:#?}");
    assert_eq!(evaluated.steps[0].status, Status::Blocked);

    // Captured unknown and known failure outcomes keep their meaning.
    for (outcome, expected) in [
        (
            CapabilityOutcome::OutcomeUnknown {
                error: "lost response".into(),
            },
            Status::OutcomeUnknown,
        ),
        (
            CapabilityOutcome::Failed {
                error: "denied".into(),
            },
            Status::Failed,
        ),
    ] {
        let captured = vec![call("a", "echo", json!({"value": 3}), outcome, 4)];
        let evaluated = evaluate(
            &plan,
            Some(&base),
            Some("a"),
            &captured,
            4,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(evaluated.steps[0].status, expected, "{evaluated:#?}");
        assert!(!evaluated.gaps.is_empty());
    }
}

#[tokio::test]
async fn replay_preserves_node_outputs_and_matches_calls_within_each_node() {
    let plan = plan(
        "a",
        vec![
            node(
                "a",
                "cayw.emit({message='starting'}); return {output=cayw.call('echo', {}),state=state}",
                "",
            ),
            node(
                "b",
                "cayw.emit('continuing'); return {output=cayw.call('echo', {}),state=state}",
                "",
            ),
            node("c", "return {output=input,state=state}", ""),
        ],
        vec![edge("a", "b", None), edge("b", "c", None)],
        json!({}),
    );
    let captured = vec![
        call(
            "a",
            "echo",
            json!({}),
            CapabilityOutcome::Completed { result: json!(10) },
            9,
        ),
        call(
            "b",
            "echo",
            json!({}),
            CapabilityOutcome::Completed { result: json!(20) },
            12,
        ),
    ];
    let replay = evaluate(&plan, None, None, &captured, 1, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(replay.status, Status::Completed, "{replay:#?}");
    assert_eq!(replay.output, Some(json!(20)));
    assert_eq!(replay.steps[0].output, Some(json!(10)));
    assert_eq!(replay.steps[0].evidence_cursor, Some(9));
    assert_eq!(replay.steps[1].evidence_cursor, Some(12));
    assert_eq!(replay.steps[2].evidence_cursor, None);

    let replay = evaluate(
        &plan,
        None,
        None,
        &captured[1..],
        2,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(replay.status, Status::Blocked, "{replay:#?}");
    assert_eq!(replay.steps.len(), 1);
    assert_eq!(replay.steps[0].node_id, "a");
    assert!(replay.gaps[0].message.contains("no captured response"));
}

#[tokio::test]
async fn replay_uses_the_new_branch_and_its_downstream_nodes() {
    let base_plan = plan(
        "cond",
        vec![
            node(
                "cond",
                "return {output = input, state = state, branch = \"high\"}",
                "",
            ),
            node("high", "return {output = \"high\", state = state}", ""),
            node("low", "return {output = \"low\", state = state}", ""),
        ],
        vec![
            edge("cond", "high", Some("high")),
            edge("cond", "low", Some("low")),
        ],
        json!({"n": 1}),
    );
    let base = observed(vec![
        step(
            "cond",
            json!({"n": 1}),
            json!({}),
            json!({"n": 1}),
            json!({}),
            Some("high"),
            1,
        ),
        step(
            "high",
            json!({"n": 1}),
            json!({}),
            json!("high"),
            json!({}),
            None,
            2,
        ),
    ]);

    let mut edited = base_plan.clone();
    edited.nodes[0].source = "return {output = input, state = state, branch = \"low\"}".into();
    let evaluated = evaluate(
        &edited,
        Some(&base),
        Some("cond"),
        &[],
        1,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(evaluated.status, Status::Completed, "{evaluated:#?}");
    let ids: Vec<&str> = evaluated
        .steps
        .iter()
        .map(|step| step.node_id.as_str())
        .collect();
    assert_eq!(ids, vec!["cond", "low"]);
    assert_eq!(evaluated.output, Some(json!("low")));
}

#[tokio::test]
async fn replay_reports_missing_context_and_cancellation_as_blocked() {
    let plan = plan(
        "a",
        vec![
            node("a", "return {output = input, state = state}", ""),
            node("b", "return {output = input, state = state}", ""),
        ],
        vec![edge("a", "b", None)],
        json!({"n": 1}),
    );
    let blocked = evaluate(&plan, None, Some("b"), &[], 1, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(blocked.status, Status::Blocked);
    assert!(blocked.steps.is_empty());
    assert!(
        blocked
            .gaps
            .iter()
            .any(|gap| gap.node_id.as_deref() == Some("b"))
    );

    let cancel = CancellationToken::new();
    cancel.cancel();
    let blocked = evaluate(&plan, None, None, &[], 2, cancel).await.unwrap();
    assert_eq!(blocked.status, Status::Blocked);
    assert!(
        blocked
            .gaps
            .iter()
            .any(|gap| gap.message.contains("cancelled"))
    );

    // A syntactically valid pointer that does not exist fails the node.
    let mut bad_pointer = plan.clone();
    bad_pointer.nodes[0].input_pointer = "/missing".into();
    let evaluated = evaluate(&bad_pointer, None, None, &[], 3, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(evaluated.status, Status::Failed, "{evaluated:#?}");
    assert!(
        evaluated
            .gaps
            .iter()
            .any(|gap| gap.message.contains("input_pointer"))
    );
}

#[tokio::test]
async fn replay_validates_the_complete_node_result() {
    for source in [
        "return {output = 1}",
        "return 5",
        "return {output = 1, state = {}, extra = true}",
        "return {output = 1, state = {}, branch = \"\"}",
    ] {
        let plan = plan("a", vec![node("a", source, "")], vec![], json!({}));
        let evaluated = evaluate(&plan, None, None, &[], 1, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(evaluated.status, Status::Failed, "{source}: {evaluated:#?}");
        assert_eq!(evaluated.steps[0].status, Status::Failed);
        assert!(!evaluated.gaps.is_empty());
    }
}

fn blocked_evaluation() -> Evaluation {
    Evaluation {
        id: "replay:previous".into(),
        version: 1,
        mode: "replay".into(),
        status: Status::Blocked,
        steps: Vec::new(),
        output: None,
        gaps: vec![cayw_trace_debug::types::Gap {
            node_id: None,
            message: "previous version was blocked".into(),
        }],
        elapsed_ms: 0.0,
        saved_at_ms: 1,
        executed_nodes: Vec::new(),
    }
}

#[tokio::test]
async fn a_blocked_base_without_steps_restarts_only_from_the_entry() {
    let plan = plan(
        "a",
        vec![
            node("a", "return {output = input, state = state}", ""),
            node("b", "return {output = input, state = state}", ""),
        ],
        vec![edge("a", "b", None)],
        json!({"n": 7}),
    );
    let base = blocked_evaluation();

    // The entry still has a truthful plan input and initial state.
    let evaluated = evaluate(
        &plan,
        Some(&base),
        Some("a"),
        &[],
        2,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(evaluated.status, Status::Completed, "{evaluated:#?}");
    assert_eq!(evaluated.steps[0].input, json!({"n": 7}));

    // Any other node has no captured input or pre-state.
    let blocked = evaluate(
        &plan,
        Some(&base),
        Some("b"),
        &[],
        3,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(blocked.status, Status::Blocked);
    assert!(blocked.steps.is_empty());
}

#[tokio::test]
async fn undo_without_a_changed_node_always_recomputes_from_the_entry() {
    let plan = plan(
        "a",
        vec![node("a", "return {output = input, state = state}", "")],
        vec![],
        json!({"n": 1}),
    );
    let base = observed(vec![step(
        "a",
        json!({"n": 99}),
        json!({}),
        json!({"n": 99}),
        json!({}),
        None,
        1,
    )]);
    let evaluated = evaluate(&plan, Some(&base), None, &[], 2, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(evaluated.status, Status::Completed, "{evaluated:#?}");
    assert_eq!(
        evaluated.steps[0].input,
        json!({"n": 1}),
        "the revision's own input is used, not the older capture"
    );
    assert_eq!(evaluated.output, Some(json!({"n": 1})));
    assert!(evaluated.steps.iter().all(|step| !step.reused));
}

#[tokio::test]
async fn replay_never_repeats_a_captured_business_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("side-effect.txt");
    let path_text = path.to_string_lossy().into_owned();
    let mut write = node(
        "write",
        "return {output = cayw.call(\"fs.write\", {path = params.path, content = \"x\"}), state = state}",
        "",
    );
    write.params = json!({"path": path_text.clone()});
    let plan = plan("write", vec![write], vec![], json!({}));
    let captured = vec![call(
        "write",
        "fs.write",
        json!({"path": path_text, "content": "x"}),
        CapabilityOutcome::Completed {
            result: json!({"bytes_written": 1}),
        },
        3,
    )];
    let evaluated = evaluate(&plan, None, None, &captured, 1, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(evaluated.status, Status::Completed, "{evaluated:#?}");
    assert_eq!(
        evaluated.output,
        Some(json!({"bytes_written": 1})),
        "the captured response is reused"
    );
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "replay must not perform the captured write again"
    );
    assert!(!path.exists());
}

#[tokio::test]
async fn replay_hides_discovery_instead_of_fabricating_a_catalog() {
    let plan = plan(
        "a",
        vec![node(
            "a",
            "return {output = cayw.discover and \"exposed\" or \"hidden\", state = state}",
            "",
        )],
        vec![],
        json!({}),
    );
    let evaluated = evaluate(&plan, None, None, &[], 1, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(evaluated.status, Status::Completed, "{evaluated:#?}");
    assert_eq!(evaluated.output, Some(json!("hidden")));
}

#[tokio::test]
async fn a_long_running_node_is_stopped_by_the_instruction_budget() {
    let plan = plan(
        "a",
        vec![node("a", "while true do end", "")],
        vec![],
        json!({}),
    );
    let evaluated = evaluate(&plan, None, None, &[], 1, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(evaluated.status, Status::Failed, "{evaluated:#?}");
    assert_eq!(evaluated.steps[0].status, Status::Failed);
    assert!(
        evaluated
            .gaps
            .iter()
            .any(|gap| gap.message.contains("InstructionLimit")),
        "{:?}",
        evaluated.gaps
    );
}

#[tokio::test]
async fn real_script_emits_started_before_mapping_and_result_validation() {
    for (source, pointer, expected) in [
        (
            "return {output = 1, state = state}",
            "/missing",
            "input_pointer",
        ),
        (
            "return {output = 1, state = state, extra = true}",
            "",
            "unexpected field",
        ),
        (
            "return {output = 1, state = state, branch = \"\"}",
            "",
            "must not be empty",
        ),
        (
            "return {output = 1, state = state, branch = \"new\"}",
            "",
            "no edge for branch",
        ),
    ] {
        let plan = plan("a", vec![node("a", source, pointer)], vec![], json!({}));
        let mode = CodeMode::new(Arc::new(NoHost), Limits::default()).unwrap();
        let error = mode
            .execute(&script_for_plan(&plan).unwrap(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.message.contains(expected), "{source}: {error:?}");
        let trace = envelopes(&error.report.outputs);
        assert_eq!(trace.len(), 2, "{source}: {trace:#?}");
        assert!(matches!(&trace[0].trace, NodeTrace::Manifest { .. }));
        match &trace[1].trace {
            // The failed node stays identified even when mapping or validation
            // fails, so capture can mark the active step failed.
            NodeTrace::Started { node_id, .. } => assert_eq!(node_id, "a"),
            other => panic!("expected started, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn real_and_replay_reject_the_same_array_pointer_and_deep_output() {
    for (source, pointer, input, expected) in [
        (
            "return {output=input,state=state}",
            "/+1",
            json!([10, 20]),
            "input_pointer",
        ),
        (
            "local v=1; for i=1,33 do v={v} end; return {output=v,state=state}",
            "",
            json!({}),
            "nested deeper",
        ),
    ] {
        let plan = plan("a", vec![node("a", source, pointer)], vec![], input);
        let mode = CodeMode::new(Arc::new(NoHost), Limits::default()).unwrap();
        let original = mode
            .execute(&script_for_plan(&plan).unwrap(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(original.message.contains(expected), "{original:?}");
        let replay = evaluate(&plan, None, None, &[], 1, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(replay.status, Status::Failed, "{replay:?}");
        assert!(replay.steps[0].error.as_deref().unwrap().contains(expected));
    }
}

#[tokio::test]
async fn node_output_cannot_impersonate_the_trace_runner() {
    for emitter in ["cayw.emit", "_G.cayw.emit"] {
        let source = format!(
            "{emitter}({{schema='cayw.trace.v1',trace={{kind='started',node_id='b'}}}}); return {{output=1,state=state}}"
        );
        let plan = plan("a", vec![node("a", &source, "")], vec![], json!({}));
        let mode = CodeMode::new(Arc::new(NoHost), Limits::default()).unwrap();
        let error = mode
            .execute(&script_for_plan(&plan).unwrap(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.message.contains("reserved trace schema"), "{error:?}");
        assert_eq!(
            error.report.outputs.len(),
            2,
            "only manifest and actual start are emitted"
        );
        let replay = evaluate(&plan, None, None, &[], 1, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(replay.status, Status::Failed);
    }
    let escaped = plan(
        "a",
        vec![node(
            "a",
            "return 1 end; cayw.emit({schema='cayw.trace.v1'}); local function injected()",
            "",
        )],
        vec![],
        json!({}),
    );
    assert!(
        script_for_plan(&escaped).is_err(),
        "source cannot close the generated function"
    );
    let hidden = plan(
        "a",
        vec![node(
            "a",
            "return {output=__node_api==nil and __resolve==nil and __build_nodes==nil,state=state}",
            "",
        )],
        vec![],
        json!({}),
    );
    let trace = envelopes(&run_script(&script_for_plan(&hidden).unwrap()).await);
    match &trace.last().unwrap().trace {
        NodeTrace::Completed { result, .. } => assert_eq!(result.output, json!(true)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn invalid_plans_never_render_a_script() {
    let plan = plan(
        "a",
        vec![node("a", "return {output = input, state = state}", "")],
        vec![edge("a", "missing", None)],
        json!({}),
    );
    assert!(matches!(
        script_for_plan(&plan),
        Err(cayw_trace_debug::Error::Invalid(_))
    ));
}

#[tokio::test]
async fn local_recompute_reuses_upstream_and_marks_mode() {
    let base_plan = plan(
        "a",
        vec![
            node(
                "a",
                "return {output = {n = input.n, tag = \"a\"}, state = {count = 1}}",
                "",
            ),
            node(
                "b",
                "return {output = {value = input}, state = {count = state.count + 1}}",
                "/n",
            ),
            node(
                "c",
                "return {output = {final = input}, state = state}",
                "/value",
            ),
        ],
        vec![edge("a", "b", None), edge("b", "c", None)],
        json!({"n": 2}),
    );
    let base = observed(vec![
        step(
            "a",
            json!({"n": 2}),
            json!({}),
            json!({"n": 2, "tag": "a"}),
            json!({"count": 1}),
            None,
            1,
        ),
        step(
            "b",
            json!({"n": 2, "tag": "a"}),
            json!({"count": 1}),
            json!({"value": 2}),
            json!({"count": 2}),
            None,
            2,
        ),
        step(
            "c",
            json!({"value": 2}),
            json!({"count": 2}),
            json!({"final": 2}),
            json!({"count": 2}),
            None,
            3,
        ),
    ]);

    let mut edited = base_plan.clone();
    edited.nodes[1].source =
        "return {output = {value = input * 10}, state = {count = state.count + 1}}".into();
    let recomputed = evaluate(
        &edited,
        Some(&base),
        Some("b"),
        &[],
        1,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(recomputed.mode, "recompute");
    assert!(recomputed.id.starts_with("recompute:"));
    assert!(recomputed.steps[0].reused);
    assert_eq!(
        recomputed.steps[0].output,
        Some(json!({"n": 2, "tag": "a"}))
    );
    assert!(!recomputed.steps[1].reused);
    assert_eq!(
        recomputed.executed_nodes,
        vec!["b".to_owned(), "c".to_owned()],
        "upstream a must not re-execute: {recomputed:?}"
    );
    assert_eq!(recomputed.output, Some(json!({"final": 20})));

    // Undo / full-entry path stays replay and does not reuse.
    let replayed = evaluate(
        &base_plan,
        Some(&base),
        None,
        &[],
        2,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(replayed.mode, "replay");
    assert!(replayed.id.starts_with("replay:"));
    assert!(replayed.steps.iter().all(|step| !step.reused));
    assert_eq!(replayed.output, Some(json!({"final": 2})));
}

#[tokio::test]
async fn local_recompute_skips_upstream_bodies_and_counts_execution() {
    // Upstream performs a Host call. If the body re-ran, the captured response
    // would be consumed; executed_nodes would also list "a".
    let base_plan = plan(
        "a",
        vec![
            node(
                "a",
                r#"local _ = cayw.call("mark", {node = "a"}); return {output = {n = 1}, state = {count = 1}}"#,
                "",
            ),
            node(
                "b",
                "return {output = {final = input.n * 2}, state = {count = state.count + 1}}",
                "",
            ),
            node(
                "c",
                "return {output = {final = input.final + 1}, state = state}",
                "",
            ),
        ],
        vec![edge("a", "b", None), edge("b", "c", None)],
        json!({}),
    );
    let calls = vec![call(
        "a",
        "mark",
        json!({"node": "a"}),
        CapabilityOutcome::Completed {
            result: json!({"ok": true}),
        },
        10,
    )];
    let base = observed(vec![
        step(
            "a",
            json!({}),
            json!({}),
            json!({"n": 1}),
            json!({"count": 1}),
            None,
            1,
        ),
        step(
            "b",
            json!({"n": 1}),
            json!({"count": 1}),
            json!({"final": 2}),
            json!({"count": 2}),
            None,
            2,
        ),
        step(
            "c",
            json!({"final": 2}),
            json!({"count": 2}),
            json!({"final": 3}),
            json!({"count": 2}),
            None,
            3,
        ),
    ]);
    let mut edited = base_plan.clone();
    edited.nodes[1].source =
        "return {output = {final = input.n * 10}, state = {count = state.count + 1}}".into();
    let recomputed = evaluate(
        &edited,
        Some(&base),
        Some("b"),
        &calls,
        1,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(recomputed.status, Status::Completed, "{recomputed:?}");
    assert_eq!(recomputed.mode, "recompute");
    assert!(recomputed.steps[0].reused);
    assert!(!recomputed.steps[1].reused);
    assert!(!recomputed.steps[2].reused);
    assert_eq!(
        recomputed.executed_nodes,
        vec!["b".to_owned(), "c".to_owned()],
        "upstream node a must not appear in executed_nodes: {recomputed:?}"
    );
    assert_eq!(recomputed.output, Some(json!({"final": 11})));
    assert!(recomputed.gaps.is_empty(), "{recomputed:?}");
}
