//! F16 / SLICE E 烟测：节点显示文件+符号+版本；错误摘要回链证据。

use cayw_codeintel::{SymbolInput, SymbolVersionBridge, content_sha256};
use cayw_trace_debug::{
    ErrorEvidenceLink, GraphPlan, Node, NodeSymbolHang, Status, Step, SymbolGraphLinker,
};
use serde_json::json;
use std::sync::Arc;

fn sha(bytes: &[u8]) -> String {
    content_sha256(bytes)
}

fn symbol(name: &str, kind: &str, start: u64, end: u64) -> SymbolInput {
    SymbolInput {
        name: name.into(),
        kind: kind.into(),
        byte_start: start,
        byte_end: end,
        start_line: Some(1),
        end_line: Some(3),
        parent_index: None,
    }
}

fn demo_plan() -> GraphPlan {
    GraphPlan {
        id: "demo".into(),
        label: "F16 hang demo".into(),
        entry: "input".into(),
        nodes: vec![
            Node {
                id: "input".into(),
                label: "读取输入".into(),
                module: "io".into(),
                source: "return {output=input,state=state}".into(),
                params: json!({}),
                input_pointer: String::new(),
                implementation: "examples/demo.rs::input".into(),
            },
            Node {
                id: "condition".into(),
                label: "条件".into(),
                module: "logic".into(),
                source: "error('bad mapping')".into(),
                params: json!({"threshold": 5}),
                input_pointer: String::new(),
                implementation: "examples/demo.rs::condition".into(),
            },
            Node {
                id: "luau_only".into(),
                label: "纯 Luau".into(),
                module: "logic".into(),
                source: "return {output=1,state=state}".into(),
                params: json!({}),
                input_pointer: String::new(),
                implementation: "luau".into(),
            },
        ],
        edges: vec![],
        input: json!({"value": 10}),
        initial_state: json!({}),
        context: json!({}),
    }
}

#[test]
fn real_task_nodes_show_file_symbol_and_version() {
    let bridge = Arc::new(SymbolVersionBridge::new());
    let digest = sha(b"fn input() {}\nfn condition() {}\n");
    let file = bridge
        .publish_file(
            "examples/demo.rs",
            &digest,
            "rust",
            vec![
                symbol("input", "function", 0, 12),
                symbol("condition", "function", 14, 30),
            ],
        )
        .unwrap();
    assert_eq!(file.version, 1);

    let linker = SymbolGraphLinker::new(bridge);
    let hangs = linker.hang_plan(&demo_plan()).unwrap();

    assert_eq!(hangs.len(), 2, "luau_only must not invent a hang");
    let input: &NodeSymbolHang = hangs.get("input").unwrap();
    assert_eq!(input.file.path, "examples/demo.rs");
    assert_eq!(input.file.version, 1);
    assert_eq!(input.file.content_sha256, digest);
    let sym = input.symbol.as_ref().expect("input symbol");
    assert_eq!(sym.name, "input");
    assert_eq!(sym.file_version, 1);
    assert_eq!(sym.content_sha256, digest);
    assert!(input.location.is_some());

    let condition = hangs.get("condition").unwrap();
    assert_eq!(condition.symbol.as_ref().unwrap().name, "condition");
    assert_eq!(condition.file.version, input.file.version);
    assert!(!hangs.contains_key("luau_only"));
}

#[test]
fn error_summary_deep_links_hang_and_evidence_cursor() {
    let bridge = Arc::new(SymbolVersionBridge::new());
    let digest = sha(b"fn condition() { panic!() }\n");
    bridge
        .publish_file(
            "examples/demo.rs",
            &digest,
            "rust",
            vec![symbol("condition", "function", 0, 24)],
        )
        .unwrap();

    let linker = SymbolGraphLinker::new(Arc::clone(&bridge));
    let hangs = linker.hang_plan(&demo_plan()).unwrap();
    let steps = vec![
        Step {
            node_id: "input".into(),
            status: Status::Completed,
            input: json!({}),
            state_before: json!({}),
            output: Some(json!(1)),
            state_after: Some(json!({})),
            branch: None,
            error: None,
            started_at_ms: 1,
            elapsed_ms: 1.0,
            evidence_cursor: Some(7),
            reused: false,
        },
        Step {
            node_id: "condition".into(),
            status: Status::Failed,
            input: json!({"value": 10}),
            state_before: json!({}),
            output: None,
            state_after: None,
            branch: None,
            error: Some("映射字段 input.amount 不存在".into()),
            started_at_ms: 2,
            elapsed_ms: 0.5,
            evidence_cursor: Some(11),
            reused: false,
        },
    ];

    let links = linker.error_evidence(&hangs, &steps).unwrap();
    assert_eq!(links.len(), 1);
    let ErrorEvidenceLink {
        node_id,
        message,
        hang,
        evidence_cursor,
        diagnostic_id,
    } = &links[0];
    assert_eq!(node_id, "condition");
    assert!(message.contains("input.amount"));
    assert_eq!(*evidence_cursor, Some(11));
    let hang = hang.as_ref().expect("failed node must carry hang");
    assert_eq!(hang.file.path, "examples/demo.rs");
    assert_eq!(hang.symbol.as_ref().unwrap().name, "condition");
    let diag_id = diagnostic_id.as_ref().expect("diagnostic recorded");
    let diags = bridge.diagnostics(Some("examples/demo.rs")).unwrap();
    assert_eq!(diags.len(), 1);
    assert_eq!(&diags[0].id, diag_id);
    assert_eq!(diags[0].location.file_version, hang.file.version);
    assert_eq!(diags[0].location.content_sha256, hang.file.content_sha256);
}

#[test]
fn unpublished_path_does_not_invent_hang() {
    let bridge = Arc::new(SymbolVersionBridge::new());
    let linker = SymbolGraphLinker::new(bridge);
    let hangs = linker.hang_plan(&demo_plan()).unwrap();
    assert!(
        hangs.is_empty(),
        "no hang without published engineering file"
    );
}
