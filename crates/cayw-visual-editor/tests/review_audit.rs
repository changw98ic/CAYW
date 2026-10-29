// Review regression suite for ET-03, ET-06, and ET-08.
// ET-03: Bounded line reading during StateAudit::open
// ET-06: Avoid redundant parsing in Project::apply
// ET-08: Simplified scan() signature without dead state_dir

use cayw_visual_editor::audit::{AUDIT_FILE, AuditStore, UNKNOWN_MARKER};
use cayw_visual_editor::{EditorConfig, EditorError, Project, start};
use serde_json::json;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const PAGE: &str = "<!DOCTYPE html><html><head><title>Test</title></head><body><main><h1 id=\"title\">Original Heading</h1><p id=\"desc\">Paragraph text.</p></main></body></html>\n";

fn setup_fixture() -> (TempDir, TempDir) {
    let project = tempfile::tempdir().expect("project tempdir");
    let state = tempfile::tempdir().expect("state tempdir");
    fs::write(project.path().join("index.html"), PAGE).expect("write page");
    (project, state)
}

fn config(project: &Path, state: &Path) -> EditorConfig {
    EditorConfig {
        project_root: project.to_path_buf(),
        page: "index.html".to_owned(),
        state_dir: state.to_path_buf(),
        editor_addr: "127.0.0.1:0".parse().expect("editor addr"),
        preview_addr: "127.0.0.1:0".parse().expect("preview addr"),
        token: TOKEN.to_owned(),
        framework: None,
        node: "node".into(),
        companion: cayw_visual_editor::default_companion(),
        codeintel: None,
    }
}

// -----------------------------------------------------------------------------
// ET-03: StateAudit::open bounded line reading
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn et03_giant_line_without_newline_is_rejected_bounded() {
    let (project, state) = setup_fixture();
    let log_path = state.path().join("server-events.jsonl");

    // Write 3 MiB of 'x' without a newline
    let giant = vec![b'x'; 3 * 1024 * 1024];
    fs::write(&log_path, giant).expect("write giant line");

    let result = start(config(project.path(), state.path())).await;
    match result {
        Err(EditorError::Unresolved(msg)) => {
            assert!(
                msg.contains("exceeds the 1048576-byte audit line limit"),
                "unexpected error message: {msg}"
            );
        }
        Err(other) => panic!("expected audit line limit error, got: {other:?}"),
        Ok(handle) => {
            handle.cancel();
            panic!("oversized audit line was accepted");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn et03_line_exceeding_limit_by_one_byte_is_rejected() {
    let (project, state) = setup_fixture();
    let log_path = state.path().join("server-events.jsonl");

    // Exactly 1 MiB + 1 byte ending in newline
    let mut over = vec![b'a'; 1024 * 1024];
    over.push(b'\n'); // total 1,048,577 bytes
    fs::write(&log_path, over).expect("write limit+1 line");

    let result = start(config(project.path(), state.path())).await;
    match result {
        Err(EditorError::Unresolved(msg)) => {
            assert!(
                msg.contains("exceeds the 1048576-byte audit line limit"),
                "unexpected error message: {msg}"
            );
        }
        Err(other) => panic!("expected audit line limit error, got: {other:?}"),
        Ok(handle) => {
            handle.cancel();
            panic!("oversized audit line was accepted");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn et03_multi_chunk_valid_line_is_accepted() {
    let (project, state) = setup_fixture();
    let log_path = state.path().join("server-events.jsonl");

    // A large valid JSON record spanning multiple BufReader chunks (e.g. 16 KiB)
    let padding = "x".repeat(16 * 1024);
    let start_event = json!({
        "at_unix_ms": 1000,
        "event": "started",
        "request": "req-1",
        "kind": "style",
        "detail": { "padding": padding },
    });
    let settle_event = json!({
        "at_unix_ms": 1050,
        "event": "settled",
        "request": "req-1",
        "outcome": "ok",
        "outcome_unknown": false,
    });

    let mut content = serde_json::to_string(&start_event).expect("json");
    content.push('\n');
    content.push_str(&serde_json::to_string(&settle_event).expect("json"));
    content.push('\n');
    fs::write(&log_path, content).expect("write multi-chunk events");

    let handle = start(config(project.path(), state.path()))
        .await
        .expect("starts with multi-chunk line");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn et03_large_total_log_with_many_valid_lines_replays_successfully() {
    let (project, state) = setup_fixture();
    let log_path = state.path().join("server-events.jsonl");

    // Write 50 paired start/settle events (100 lines total)
    let mut content = String::new();
    for i in 0..50 {
        let req_id = format!("req-{i}");
        let start_ev = json!({
            "at_unix_ms": 1000 + i,
            "event": "started",
            "request": req_id,
            "kind": "style",
            "detail": { "padding": "p".repeat(2048) },
        });
        let settle_ev = json!({
            "at_unix_ms": 1050 + i,
            "event": "settled",
            "request": req_id,
            "outcome": "ok",
            "outcome_unknown": false,
        });
        content.push_str(&serde_json::to_string(&start_ev).expect("json"));
        content.push('\n');
        content.push_str(&serde_json::to_string(&settle_ev).expect("json"));
        content.push('\n');
    }
    fs::write(&log_path, content).expect("write paired events");

    let handle = start(config(project.path(), state.path()))
        .await
        .expect("starts with 100 valid lines");
    handle.shutdown().await;
}

// -----------------------------------------------------------------------------
// ET-06: Project::apply reuse of parsed document
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn et06_project_apply_style_text_and_undo_redo_consistent() {
    let (project_dir, state_dir) = setup_fixture();
    let project = Project::open(project_dir.path(), "index.html", state_dir.path())
        .await
        .expect("open project");

    let snap_initial = project.snapshot().await.expect("initial snapshot");
    let title_node = snap_initial
        .nodes
        .iter()
        .find(|n| n.tag == "h1")
        .expect("h1 node");
    let title_id = title_node.id.clone();

    // 1. Style apply
    let snap_after_style = project
        .apply(json!({
            "kind": "style",
            "id": title_id,
            "properties": { "color": "#00ff00" }
        }))
        .await
        .expect("apply style");
    assert!(snap_after_style.html.contains("#00ff00"));
    assert!(project.dirty().await);
    let (can_undo, can_redo) = project.history_flags().await;
    assert!(can_undo);
    assert!(!can_redo);

    // 2. Text apply
    let title_node_after = snap_after_style
        .nodes
        .iter()
        .find(|n| n.tag == "h1")
        .expect("h1 node");
    let snap_after_text = project
        .apply(json!({
            "kind": "text",
            "id": title_node_after.id,
            "text": "Modified Heading"
        }))
        .await
        .expect("apply text");
    assert!(snap_after_text.html.contains("Modified Heading"));

    // Verify snapshot() produces equivalent output to apply() result
    let snap_read = project.snapshot().await.expect("read snapshot");
    assert_eq!(snap_read.revision, snap_after_text.revision);
    assert_eq!(snap_read.html, snap_after_text.html);
    assert_eq!(snap_read.nodes.len(), snap_after_text.nodes.len());

    // 3. Undo
    let snap_undo = project
        .apply(json!({ "kind": "undo" }))
        .await
        .expect("undo");
    assert!(snap_undo.html.contains("Original Heading"));
    let (can_undo, can_redo) = project.history_flags().await;
    assert!(can_undo);
    assert!(can_redo);

    // 4. Redo
    let snap_redo = project
        .apply(json!({ "kind": "redo" }))
        .await
        .expect("redo");
    assert!(snap_redo.html.contains("Modified Heading"));
    let (can_undo, can_redo) = project.history_flags().await;
    assert!(can_undo);
    assert!(!can_redo);
}

// -----------------------------------------------------------------------------
// ET-08: AuditStore::open and scan without dead state_dir parameter
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn et08_audit_store_open_succeeds_on_clean_dir() {
    let (_, state_dir) = setup_fixture();
    let store = AuditStore::open(state_dir.path()).expect("open clean audit store");
    assert!(!store.unknown());
    let canonical_state = state_dir
        .path()
        .canonicalize()
        .expect("canonicalize state dir");
    assert_eq!(store.audit_path(), canonical_state.join(AUDIT_FILE));
    assert_eq!(store.marker_path(), canonical_state.join(UNKNOWN_MARKER));
}

#[tokio::test(flavor = "multi_thread")]
async fn et08_audit_store_open_flags_unsettled_and_writes_marker() {
    let (_, state_dir) = setup_fixture();
    let audit_file = state_dir.path().join(AUDIT_FILE);
    let marker_path = state_dir.path().join(UNKNOWN_MARKER);
    assert!(!marker_path.exists());

    // Write an unsettled started record
    let record = json!({
        "event": "started",
        "call_id": "call-001",
        "capability": "fs.write",
        "arguments": {},
        "result": null,
        "outcome_unknown": false,
        "at_ms": 1000,
    });
    let mut line = serde_json::to_string(&record).expect("json");
    line.push('\n');
    fs::write(&audit_file, line).expect("write audit line");

    let result = AuditStore::open(state_dir.path());
    assert!(result.is_err(), "unsettled call must fail AuditStore::open");
    let err = result.err().expect("error");
    assert_eq!(err.code, "reconciliation_required");
    assert!(err.outcome_unknown);

    // Verify durable unknown marker was written
    assert!(
        marker_path.exists(),
        "unknown.json marker must be written on unsettled call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn et08_audit_store_open_flags_corrupt_record_and_writes_marker() {
    let (_, state_dir) = setup_fixture();
    let audit_file = state_dir.path().join(AUDIT_FILE);
    let marker_path = state_dir.path().join(UNKNOWN_MARKER);
    assert!(!marker_path.exists());

    fs::write(&audit_file, "this is not valid audit json\n").expect("write corrupt line");

    let result = AuditStore::open(state_dir.path());
    assert!(result.is_err(), "corrupt record must fail AuditStore::open");
    let err = result.err().expect("error");
    assert_eq!(err.code, "reconciliation_required");
    assert!(err.outcome_unknown);

    assert!(
        marker_path.exists(),
        "unknown.json marker must be written on corrupt audit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn et08_audit_store_open_refuses_when_marker_exists() {
    let (_, state_dir) = setup_fixture();
    let marker_path = state_dir.path().join(UNKNOWN_MARKER);
    fs::write(&marker_path, b"{\"code\":\"reconciliation_required\"}\n").expect("write marker");

    let result = AuditStore::open(state_dir.path());
    assert!(
        result.is_err(),
        "existing marker must fail AuditStore::open"
    );
    let err = result.err().expect("error");
    assert_eq!(err.code, "reconciliation_required");
    assert!(err.outcome_unknown);
    assert!(err.message.contains("unresolved unknown operation"));
}

#[tokio::test(flavor = "multi_thread")]
async fn et08_audit_store_open_flags_dropped_or_unknown_settlement() {
    let (_, state_dir) = setup_fixture();
    let audit_file = state_dir.path().join(AUDIT_FILE);
    let marker_path = state_dir.path().join(UNKNOWN_MARKER);

    // Case 1: started then dropped
    let start_rec = json!({
        "event": "started",
        "call_id": "call-dropped",
        "capability": "fs.write",
        "arguments": {},
        "result": null,
        "outcome_unknown": false,
        "at_ms": 1000,
    });
    let drop_rec = json!({
        "event": "dropped",
        "call_id": "call-dropped",
        "capability": "",
        "arguments": null,
        "result": null,
        "outcome_unknown": true,
        "at_ms": 1050,
    });
    let content = format!(
        "{}\n{}\n",
        serde_json::to_string(&start_rec).expect("json"),
        serde_json::to_string(&drop_rec).expect("json")
    );
    fs::write(&audit_file, content).expect("write dropped audit");

    let result = AuditStore::open(state_dir.path());
    assert!(result.is_err(), "dropped call must fail AuditStore::open");
    let err = result.err().expect("error");
    assert_eq!(err.code, "reconciliation_required");
    assert!(err.outcome_unknown);
    assert!(
        marker_path.exists(),
        "marker must be written for dropped call"
    );

    // Case 2: started then settled with outcome_unknown: true
    let (_, state_dir2) = setup_fixture();
    let audit_file2 = state_dir2.path().join(AUDIT_FILE);
    let marker_path2 = state_dir2.path().join(UNKNOWN_MARKER);
    let start_rec2 = json!({
        "event": "started",
        "call_id": "call-unknown",
        "capability": "fs.write",
        "arguments": {},
        "result": null,
        "outcome_unknown": false,
        "at_ms": 1000,
    });
    let settle_unknown_rec = json!({
        "event": "settled",
        "call_id": "call-unknown",
        "capability": "",
        "arguments": null,
        "result": null,
        "outcome_unknown": true,
        "at_ms": 1050,
    });
    let content2 = format!(
        "{}\n{}\n",
        serde_json::to_string(&start_rec2).expect("json"),
        serde_json::to_string(&settle_unknown_rec).expect("json")
    );
    fs::write(&audit_file2, content2).expect("write unknown settled audit");

    let result2 = AuditStore::open(state_dir2.path());
    assert!(
        result2.is_err(),
        "unknown settlement must fail AuditStore::open"
    );
    let err2 = result2.err().expect("error");
    assert_eq!(err2.code, "reconciliation_required");
    assert!(err2.outcome_unknown);
    assert!(
        marker_path2.exists(),
        "marker must be written for unknown settlement"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn et08_audit_store_open_exclusive_lock_conflict() {
    let (_, state_dir) = setup_fixture();
    let first_store = AuditStore::open(state_dir.path()).expect("first open succeeds");

    let second_result = AuditStore::open(state_dir.path());
    assert!(
        second_result.is_err(),
        "concurrent open on same state dir must fail"
    );
    let err = second_result.err().expect("error");
    assert_eq!(err.code, "state_dir_busy");
    assert!(!err.outcome_unknown);

    drop(first_store);
    let third_store =
        AuditStore::open(state_dir.path()).expect("reopen succeeds after first dropped");
    assert!(!third_store.unknown());
}

#[tokio::test(flavor = "multi_thread")]
async fn et08_audit_store_open_refuses_oversized_audit_file() {
    let (_, state_dir) = setup_fixture();
    let audit_file = state_dir.path().join(AUDIT_FILE);
    let marker_path = state_dir.path().join(UNKNOWN_MARKER);

    let file = fs::File::create(&audit_file).expect("create audit file");
    file.set_len(64 * 1024 * 1024 + 1)
        .expect("set oversized length");
    drop(file);

    let result = AuditStore::open(state_dir.path());
    assert!(
        result.is_err(),
        "oversized audit file must fail AuditStore::open"
    );
    let err = result.err().expect("error");
    assert_eq!(err.code, "reconciliation_required");
    assert!(err.outcome_unknown);
    assert!(
        marker_path.exists(),
        "unknown.json marker must be written on oversized audit file"
    );
}
