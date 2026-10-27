//! Integration test connecting symbol production, graph hangs, and local recompute:
//! Workspace source definition -> trace/symbol hang -> node save -> recompute downstream only -> inspect old & new evidence.

use cayw_codeintel::{SymbolVersionBridge, publish_under_workspace};
use cayw_core::{
    AttemptLease, CoreEvent, EventRecord, ExecutionOutcome, ExecutionRecord, ExecutionState,
};
use cayw_trace_debug::{
    Edge, Edit, GraphPlan, Node, NodeResult, NodeTrace, Status, TraceEnvelope, TraceStore,
};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn snapshot_seed(input: &Value, state: &Value, context: &Value) -> Value {
    fn walk(value: &Value, tables: &mut Map<String, Value>, next_id: &mut u64) -> Value {
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
            Value::Array(items) => {
                *next_id += 1;
                let id = next_id.to_string();
                let walked: Vec<Value> = items
                    .iter()
                    .map(|item| walk(item, tables, next_id))
                    .collect();
                tables.insert(id.clone(), json!({ "$array": true, "$items": walked }));
                json!({ "$ref": id })
            }
            Value::Object(fields) => {
                *next_id += 1;
                let id = next_id.to_string();
                let mut out = Map::new();
                for (key, item) in fields {
                    out.insert(key.clone(), walk(item, tables, next_id));
                }
                tables.insert(id.clone(), Value::Object(out));
                json!({ "$ref": id })
            }
        }
    }
    let mut tables = Map::new();
    let mut next_id = 0u64;
    let input_root = walk(input, &mut tables, &mut next_id);
    let state_root = walk(state, &mut tables, &mut next_id);
    let context_root = walk(context, &mut tables, &mut next_id);
    json!({
        "tables": Value::Object(tables),
        "roots": {
            "input": input_root,
            "state": state_root,
            "context": context_root,
        }
    })
}

fn source_hash_for(plan: &GraphPlan) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "sha256:{:x}",
        Sha256::digest(
            cayw_trace_debug::engine::script_for_plan(plan)
                .unwrap()
                .as_bytes()
        )
    )
}

fn lease() -> AttemptLease {
    AttemptLease {
        session_id: "session-test".into(),
        attempt_id: "attempt-test".into(),
        worker_id: "worker-test".into(),
        fence: 1,
    }
}

fn event(cursor: u64, event: CoreEvent) -> EventRecord {
    EventRecord {
        protocol_version: 3,
        session_id: "session-test".into(),
        cursor,
        timestamp: cayw_trace_debug::now_ms(),
        attempt: Some(lease()),
        event,
    }
}

fn trace_event(cursor: u64, source_hash: &str, envelope: TraceEnvelope) -> EventRecord {
    event(
        cursor,
        CoreEvent::Host {
            kind: "trace_node".into(),
            data: json!({
                "execution_id": "exec-pipeline",
                "task_id": "task-pipeline",
                "source_hash": source_hash,
                "parent_call_id": "tool-call",
                "entry": {
                    "cursor": cursor - 1,
                    "output": {"type": "json", "value": envelope}
                }
            }),
        },
    )
}

#[tokio::test]
async fn symbol_production_and_local_recompute_pipeline() {
    // 1. 工作区源码定义 (Workspace source definition)
    let workspace_dir = tempfile::tempdir().unwrap();
    let src_dir = workspace_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("pipeline.rs");
    std::fs::write(
        &file_path,
        b"pub fn calculate() {\n    // initial implementation\n}\n\npub fn summarize() {\n    // summary step\n}\n",
    )
    .unwrap();

    // 2. 符号生产与版本跟踪 (Symbol production via codeintel producer)
    let bridge = Arc::new(SymbolVersionBridge::new());
    let file_ver =
        publish_under_workspace(&bridge, workspace_dir.path(), "src/pipeline.rs", "rust").unwrap();
    assert_eq!(file_ver.version, 1);
    assert_eq!(file_ver.path, "src/pipeline.rs");

    let defs = bridge
        .definitions(cayw_codeintel::DefinitionQuery {
            name: "calculate".into(),
            path: Some("src/pipeline.rs".into()),
            kind: None,
            file_version: Some(1),
        })
        .unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].id.name, "calculate");

    // 3. 真实 trace 文件与符号关联 (Real trace file / symbol association)
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.json");
    let store = TraceStore::open_with_bridge_and_workspace(
        &trace_path,
        Arc::clone(&bridge),
        Some(workspace_dir.path().to_path_buf()),
    )
    .unwrap();

    let plan = GraphPlan {
        id: "pipeline-plan".into(),
        label: "测试数据流水线".into(),
        entry: "input".into(),
        input: json!({"val": 10}),
        initial_state: json!({}),
        context: json!({"context": true}),
        nodes: vec![
            Node {
                id: "input".into(),
                label: "输入数据".into(),
                module: "io".into(),
                source: "return {output = input, state = state}".into(),
                params: json!({}),
                input_pointer: String::new(),
                implementation: "luau".into(),
            },
            Node {
                id: "calculate".into(),
                label: "计算步骤".into(),
                module: "compute".into(),
                source: "return {output = {doubled = input.val * 2}, state = state}".into(),
                params: json!({}),
                input_pointer: String::new(),
                implementation: "src/pipeline.rs::calculate".into(),
            },
            Node {
                id: "summarize".into(),
                label: "汇总步骤".into(),
                module: "summary".into(),
                source: "return {output = {result = input.doubled + 5}, state = state}".into(),
                params: json!({}),
                input_pointer: String::new(),
                implementation: "src/pipeline.rs::summarize".into(),
            },
        ],
        edges: vec![
            Edge {
                from: "input".into(),
                to: "calculate".into(),
                branch: None,
            },
            Edge {
                from: "calculate".into(),
                to: "summarize".into(),
                branch: None,
            },
        ],
    };

    let source_hash = source_hash_for(&plan);
    let execution = ExecutionRecord {
        execution_id: "exec-pipeline".into(),
        parent_call_id: "tool-call".into(),
        task_id: "task-pipeline".into(),
        source_hash: source_hash.clone(),
        lease: lease(),
        state: ExecutionState::Running,
        terminal: None,
    };

    // Ingest execution start & manifest
    store
        .ingest(&[
            event(1, CoreEvent::ExecutionStarted { execution }),
            trace_event(
                2,
                &source_hash,
                TraceEnvelope {
                    schema: cayw_trace_debug::TRACE_SCHEMA.into(),
                    trace: NodeTrace::Manifest { plan: plan.clone() },
                },
            ),
        ])
        .unwrap();

    // Ingest started & completed for input node
    let input_raw = json!({"val": 10});
    let empty_state = json!({});
    let ctx = json!({"context": true});
    let seed_input = snapshot_seed(&input_raw, &empty_state, &ctx);
    store
        .ingest(&[
            trace_event(
                3,
                &source_hash,
                TraceEnvelope {
                    schema: cayw_trace_debug::TRACE_SCHEMA.into(),
                    trace: NodeTrace::Started {
                        node_id: "input".into(),
                        input: input_raw.clone(),
                        state_before: empty_state.clone(),
                        context_before: ctx.clone(),
                        seed: Some(seed_input),
                    },
                },
            ),
            trace_event(
                4,
                &source_hash,
                TraceEnvelope {
                    schema: cayw_trace_debug::TRACE_SCHEMA.into(),
                    trace: NodeTrace::Completed {
                        node_id: "input".into(),
                        result: NodeResult {
                            output: input_raw.clone(),
                            state: empty_state.clone(),
                            branch: None,
                        },
                        elapsed_ms: 1.0,
                    },
                },
            ),
        ])
        .unwrap();

    // Ingest started & completed for calculate node (output: doubled = 20)
    let calc_out = json!({"doubled": 20});
    let seed_calc = snapshot_seed(&input_raw, &empty_state, &ctx);
    store
        .ingest(&[
            trace_event(
                5,
                &source_hash,
                TraceEnvelope {
                    schema: cayw_trace_debug::TRACE_SCHEMA.into(),
                    trace: NodeTrace::Started {
                        node_id: "calculate".into(),
                        input: input_raw.clone(),
                        state_before: empty_state.clone(),
                        context_before: ctx.clone(),
                        seed: Some(seed_calc),
                    },
                },
            ),
            trace_event(
                6,
                &source_hash,
                TraceEnvelope {
                    schema: cayw_trace_debug::TRACE_SCHEMA.into(),
                    trace: NodeTrace::Completed {
                        node_id: "calculate".into(),
                        result: NodeResult {
                            output: calc_out.clone(),
                            state: empty_state.clone(),
                            branch: None,
                        },
                        elapsed_ms: 1.0,
                    },
                },
            ),
        ])
        .unwrap();

    // Ingest started & completed for summarize node (output: result = 25)
    let sum_out = json!({"result": 25});
    let seed_sum = snapshot_seed(&calc_out, &empty_state, &ctx);
    let finished = ExecutionRecord {
        execution_id: "exec-pipeline".into(),
        parent_call_id: "tool-call".into(),
        task_id: "task-pipeline".into(),
        source_hash: source_hash.clone(),
        lease: lease(),
        state: ExecutionState::Completed,
        terminal: Some(ExecutionOutcome::Completed {
            result: sum_out.clone(),
        }),
    };
    store
        .ingest(&[
            trace_event(
                7,
                &source_hash,
                TraceEnvelope {
                    schema: cayw_trace_debug::TRACE_SCHEMA.into(),
                    trace: NodeTrace::Started {
                        node_id: "summarize".into(),
                        input: calc_out.clone(),
                        state_before: empty_state.clone(),
                        context_before: ctx.clone(),
                        seed: Some(seed_sum),
                    },
                },
            ),
            trace_event(
                8,
                &source_hash,
                TraceEnvelope {
                    schema: cayw_trace_debug::TRACE_SCHEMA.into(),
                    trace: NodeTrace::Completed {
                        node_id: "summarize".into(),
                        result: NodeResult {
                            output: sum_out.clone(),
                            state: empty_state.clone(),
                            branch: None,
                        },
                        elapsed_ms: 1.0,
                    },
                },
            ),
            event(
                9,
                CoreEvent::ExecutionFinished {
                    execution: finished,
                },
            ),
        ])
        .unwrap();

    // 验证初始状态 (Verify initial trace snapshot)
    let snap1 = store.snapshot();
    let proj1 = &snap1.projects["exec-pipeline"];
    assert_eq!(proj1.version(), 1);
    assert_eq!(proj1.evaluations.len(), 1);
    assert_eq!(proj1.evaluations[0].status, Status::Completed);
    assert_eq!(proj1.evaluations[0].output, Some(json!({"result": 25})));

    // 验证符号关联 (Verify symbol hangs on nodes)
    let hang_calc = proj1.symbol_hangs.get("calculate").expect("calculate hang");
    assert_eq!(hang_calc.file.path, "src/pipeline.rs");
    assert_eq!(hang_calc.file.version, 1);
    assert_eq!(hang_calc.symbol.as_ref().unwrap().name, "calculate");

    let hang_sum = proj1.symbol_hangs.get("summarize").expect("summarize hang");
    assert_eq!(hang_sum.file.path, "src/pipeline.rs");
    assert_eq!(hang_sum.file.version, 1);
    assert_eq!(hang_sum.symbol.as_ref().unwrap().name, "summarize");

    // 4. 保存可求值节点并触发局部重算 (Save evaluatable node -> recompute affected downstream only)
    let edit = Edit {
        expected_version: 1,
        node_id: "calculate".into(),
        // Change logic: multiply by 10 instead of 2 (10 * 10 = 100, then 100 + 5 = 105)
        source: "return {output = {doubled = input.val * 10}, state = state}".into(),
        params: json!({}),
        input_pointer: String::new(),
    };

    let doc2 = store
        .edit("exec-pipeline", edit, CancellationToken::new())
        .await
        .unwrap();
    let proj2 = &doc2.projects["exec-pipeline"];
    assert_eq!(proj2.version(), 2);
    assert_eq!(proj2.evaluations.len(), 2);

    let recomputed = proj2.latest().expect("latest evaluation");
    assert_eq!(recomputed.mode, "recompute");
    assert_eq!(recomputed.status, Status::Completed);
    assert_eq!(recomputed.output, Some(json!({"result": 105})));

    // 5. 查看新旧证据对比 (Inspect old vs new evidence)
    // Upstream 'input' must be reused, not executed!
    assert!(
        recomputed.steps[0].reused,
        "upstream node input must be reused from base"
    );
    assert!(
        !recomputed.steps[1].reused,
        "edited node calculate must be recomputed"
    );
    assert!(
        !recomputed.steps[2].reused,
        "downstream node summarize must be recomputed"
    );
    assert_eq!(
        recomputed.executed_nodes,
        vec!["calculate", "summarize"],
        "only edited and downstream nodes run in Luau VM"
    );

    // Old evaluation is preserved intact
    assert_eq!(
        proj2.evaluations[0].output,
        Some(json!({"result": 25})),
        "version 1 output is preserved"
    );

    // 6. 源码变更后刷新符号挂载版本 (Workspace source change & refresh)
    std::fs::write(
        &file_path,
        b"pub fn calculate() {\n    // revised implementation with updated logic\n}\n\npub fn summarize() {\n    // revised summary\n}\n",
    )
    .unwrap();

    store.refresh_symbol_hangs().unwrap();
    let snap3 = store.snapshot();
    let proj3 = &snap3.projects["exec-pipeline"];
    let refreshed_hang = proj3.symbol_hangs.get("calculate").expect("calculate hang");
    assert_eq!(
        refreshed_hang.file.version, 2,
        "refresh_symbol_hangs bumps version after source file edit"
    );
    assert_ne!(
        refreshed_hang.file.content_sha256, file_ver.content_sha256,
        "content SHA-256 changes on disk edit"
    );
}
