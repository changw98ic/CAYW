//! F16 / SLICE E 烟测：节点显示文件+符号+版本；错误摘要回链证据。

use cayw_codeintel::{SymbolInput, SymbolVersionBridge, content_sha256};
use cayw_core::{AttemptLease, CoreEvent, EventRecord, ExecutionRecord, ExecutionState};
use cayw_trace_debug::{
    Edge, ErrorEvidenceLink, GraphPlan, Node, NodeSymbolHang, NodeTrace, Status, Step,
    SymbolGraphLinker, TraceEnvelope, TraceStore,
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
        edges: vec![
            Edge {
                from: "input".into(),
                to: "condition".into(),
                branch: None,
            },
            Edge {
                from: "condition".into(),
                to: "luau_only".into(),
                branch: None,
            },
        ],
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

fn ingest_manifest(store: &TraceStore, plan: GraphPlan) {
    let source_hash = source_hash_for(&plan);
    let execution = ExecutionRecord {
        execution_id: "exec".into(),
        parent_call_id: "tool".into(),
        task_id: "task".into(),
        source_hash: source_hash.clone(),
        lease: lease(),
        state: ExecutionState::Running,
        terminal: None,
    };
    let envelope = TraceEnvelope {
        schema: cayw_trace_debug::TRACE_SCHEMA.into(),
        trace: NodeTrace::Manifest { plan },
    };
    let trace = event(
        2,
        CoreEvent::Host {
            kind: "trace_node".into(),
            data: json!({
                "execution_id": "exec",
                "task_id": "task",
                "source_hash": source_hash,
                "parent_call_id": "tool",
                "entry": {
                    "cursor": 1,
                    "output": {"type": "json", "value": envelope}
                }
            }),
        },
    );
    store
        .ingest(&[event(1, CoreEvent::ExecutionStarted { execution }), trace])
        .unwrap();
}

#[test]
fn ingest_populates_symbol_hangs_via_linker_and_persists() {
    let directory = tempfile::tempdir().unwrap();
    let bridge = Arc::new(SymbolVersionBridge::new());
    let digest = sha(b"fn input() {}\nfn condition() {}\n");
    bridge
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

    let path = directory.path().join("trace.json");
    let store = TraceStore::open_with_bridge(&path, Arc::clone(&bridge)).unwrap();
    ingest_manifest(&store, demo_plan());

    let project = store.snapshot().projects.get("exec").cloned().unwrap();
    assert_eq!(project.symbol_hangs.len(), 2);
    assert_eq!(
        project.symbol_hangs["input"].file.version, 1,
        "ingest must hang through linker, not leave empty map"
    );
    assert_eq!(
        project.symbol_hangs["condition"]
            .symbol
            .as_ref()
            .unwrap()
            .name,
        "condition"
    );
    assert!(!project.symbol_hangs.contains_key("luau_only"));

    // Persistence: reopen without the bridge still carries the hung snapshot.
    drop(store);
    let reopened = TraceStore::open(&path).unwrap();
    let again = reopened.snapshot();
    let project = again.projects.get("exec").unwrap();
    assert_eq!(project.symbol_hangs.len(), 2);
    assert_eq!(project.symbol_hangs["input"].file.content_sha256, digest);
}

#[test]
fn bridge_version_change_updates_hangs_through_refresh() {
    let directory = tempfile::tempdir().unwrap();
    let bridge = Arc::new(SymbolVersionBridge::new());
    let digest_v1 = sha(b"fn input() {}\n");
    bridge
        .publish_file(
            "examples/demo.rs",
            &digest_v1,
            "rust",
            vec![symbol("input", "function", 0, 12)],
        )
        .unwrap();

    let store =
        TraceStore::open_with_bridge(directory.path().join("trace.json"), Arc::clone(&bridge))
            .unwrap();
    ingest_manifest(&store, demo_plan());
    assert_eq!(
        store.snapshot().projects["exec"].symbol_hangs["input"]
            .file
            .version,
        1
    );

    let digest_v2 = sha(b"fn input() { /* changed */ }\n");
    let published = bridge
        .publish_file(
            "examples/demo.rs",
            &digest_v2,
            "rust",
            vec![
                symbol("input", "function", 0, 28),
                symbol("condition", "function", 30, 50),
            ],
        )
        .unwrap();
    assert_eq!(published.version, 2);

    let before_seq = store.snapshot().sequence;
    store.refresh_symbol_hangs().unwrap();
    let after = store.snapshot();
    assert!(
        after.sequence > before_seq,
        "refresh must advance sequence for HTTP watchers"
    );
    let hang = &after.projects["exec"].symbol_hangs["input"];
    assert_eq!(hang.file.version, 2);
    assert_eq!(hang.file.content_sha256, digest_v2);
    assert_eq!(
        after.projects["exec"].symbol_hangs["condition"]
            .symbol
            .as_ref()
            .unwrap()
            .name,
        "condition"
    );
}

#[test]
fn default_open_with_workspace_sources_feeds_bridge_on_ingest() {
    // Product path: TraceStore::open / open_with_workspace — no manual publish_file.
    let workspace = tempfile::tempdir().unwrap();
    let examples = workspace.path().join("examples");
    std::fs::create_dir_all(&examples).unwrap();
    let source = b"pub fn input() {}
pub fn condition(x: i32) -> bool { x > 0 }
";
    std::fs::write(examples.join("demo.rs"), source).unwrap();

    let store_path = workspace.path().join("trace.json");
    let store = TraceStore::open_with_workspace(&store_path, workspace.path()).unwrap();
    assert!(
        store.workspace_root().is_some(),
        "default product open must attach a workspace producer root"
    );
    assert!(
        store
            .bridge()
            .file_version("examples/demo.rs")
            .unwrap()
            .is_none(),
        "bridge starts empty until ingest feeds the producer"
    );

    ingest_manifest(&store, demo_plan());

    let file = store
        .bridge()
        .file_version("examples/demo.rs")
        .unwrap()
        .expect("ingest must publish workspace source into the bridge");
    assert_eq!(file.version, 1);
    assert_eq!(file.content_sha256, sha(source));

    let symbols = store.bridge().symbols_for("examples/demo.rs").unwrap();
    assert!(
        symbols.iter().any(|s| s.id.name == "input"),
        "producer must index real symbols, not leave an empty publish: {symbols:?}"
    );
    assert!(symbols.iter().any(|s| s.id.name == "condition"));

    let project = store.snapshot().projects.get("exec").cloned().unwrap();
    assert!(
        !project.symbol_hangs.is_empty(),
        "default-start ingest must yield non-empty symbol hangs when workspace has sources"
    );
    assert_eq!(project.symbol_hangs["input"].file.version, 1);
    assert_eq!(
        project.symbol_hangs["condition"]
            .symbol
            .as_ref()
            .unwrap()
            .name,
        "condition"
    );
    assert!(!project.symbol_hangs.contains_key("luau_only"));
}

#[test]
fn default_open_without_matching_workspace_file_still_skips_hang() {
    let workspace = tempfile::tempdir().unwrap();
    // No examples/demo.rs on disk.
    let store =
        TraceStore::open_with_workspace(workspace.path().join("trace.json"), workspace.path())
            .unwrap();
    ingest_manifest(&store, demo_plan());
    let project = store.snapshot().projects.get("exec").cloned().unwrap();
    assert!(
        project.symbol_hangs.is_empty(),
        "missing workspace files must not invent hangs"
    );
}
