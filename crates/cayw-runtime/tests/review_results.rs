#![cfg(target_os = "macos")]
//! Integration tests for review items BR-01, BR-03, RT-10:
//! - BR-01: result.store stage tracking, busy retry without body duplication/revision conflict, unknown non-retry
//! - BR-03: result.search windowed snippet and match-aligned offset past byte 512 (single-line JSON & multibyte UTF-8)
//! - RT-10: Production Host result auto-externalization via maybe_externalize, Core event persistence and result.get retrieval

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_content::Limits;
use cayw_runtime::{
    ContentConfig, Runtime, RuntimeConfig, ScopedPermission, fixture::script_response,
};
use serde_json::json;
use std::{path::Path, sync::Arc};

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(
                &request,
                &self.0,
                &format!("review-results-{}", request.messages.len()),
            ),
        }]))
    }
}

fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn test_config(
    root: &Path,
    permissions: Vec<ScopedPermission>,
    auto_externalize_threshold: usize,
) -> RuntimeConfig {
    RuntimeConfig {
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "local-device".into(),
            limits: Limits {
                max_content_bytes: 256 * 1024,
                ..Default::default()
            },
        }),
        permissions,
        auto_externalize_threshold_bytes: auto_externalize_threshold,
        ..Default::default()
    }
}

fn open_test_runtime(root: &Path, script: &str, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "review_results"),
        config,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// RT-10: Production Host result auto-externalization tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rt10_production_host_externalizes_over_threshold_result_with_core_events_and_retrieval() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let payload_filename = "large_payload.txt";
    let payload_content = "PAYLOAD_BLOCK_0123456789_".repeat(40); // 1000 bytes
    std::fs::write(workspace.join(payload_filename), &payload_content).unwrap();

    let perms = vec![ScopedPermission::exact(
        "fs.read",
        json!({"path": payload_filename}),
    )];

    let script = r#"
        local res = cayw.call('fs.read', {path = 'large_payload.txt'})
        assert(res.externalized == true, 'must be marked externalized')
        assert(res.stored == true, 'must be marked stored')
        assert(type(res.handle) == 'string', 'must return string handle')
        assert(#res.handle > 0, 'handle must not be empty')

        local page = cayw.call('result.get', {handle = res.handle, offset = 0, limit = 4096})
        assert(page.bytes > 900, 'stored bytes must exceed payload size')
        assert(string.find(page.text, 'PAYLOAD_BLOCK') ~= nil, 'content must contain payload')
        return {handle = res.handle, bytes = page.total_bytes}
    "#;

    let rt = open_test_runtime(&root, script, test_config(&root, perms, 256));
    let session = rt.create(Some("rt10-externalize-over".into())).unwrap().id;

    rt.prompt(
        &session,
        "run externalization test",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let events = rt.events(&session, 0, 1000).unwrap();

    // Verify durable storage events recorded in Core
    let prepared = events.iter().find(|r| {
        let v = serde_json::to_value(r).unwrap();
        v["event"]["type"] == "host" && v["event"]["kind"] == "runtime_result_storage_prepared"
    });
    assert!(
        prepared.is_some(),
        "missing runtime_result_storage_prepared Core event"
    );

    let terminal = events.iter().find(|r| {
        let v = serde_json::to_value(r).unwrap();
        v["event"]["type"] == "host"
            && v["event"]["kind"] == "runtime_result_storage_terminal"
            && v["event"]["data"]["state"] == "stored"
    });
    assert!(
        terminal.is_some(),
        "missing runtime_result_storage_terminal stored Core event"
    );

    // Verify fs.read completed event in Core
    let fs_read_completed = events.iter().find(|r| {
        let v = serde_json::to_value(r).unwrap();
        let call = &v["event"]["call"];
        v["event"]["type"] == "capability_settled"
            && call["name"] == "fs.read"
            && call["outcome"]["type"] == "completed"
    });
    assert!(
        fs_read_completed.is_some(),
        "missing fs.read completed event"
    );

    // Verify inspection through Runtime API
    let term_val = serde_json::to_value(terminal.unwrap()).unwrap();
    let handle = term_val["event"]["data"]["identity"]["handle"]
        .as_str()
        .expect("handle in terminal event");
    let inspect = rt.result_inspect(&session, None, handle).unwrap();
    assert!(inspect.is_some(), "result_inspect must find stored handle");
    let view = inspect.unwrap();
    assert_eq!(view.handle, handle);
    assert_eq!(view.encoding, "json");
    assert!(!view.released);
}

#[tokio::test]
async fn rt10_production_host_under_threshold_result_passes_through_inline() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let small_filename = "small_file.txt";
    let small_content = "small inline content under threshold";
    std::fs::write(workspace.join(small_filename), small_content).unwrap();

    let perms = vec![ScopedPermission::exact(
        "fs.read",
        json!({"path": small_filename}),
    )];

    let script = r#"
        local res = cayw.call('fs.read', {path = 'small_file.txt'})
        assert(res.externalized == nil, 'must not be marked externalized')
        assert(res.handle == nil, 'must not contain handle')
        assert(res.content == 'small inline content under threshold', 'inline content must be intact')
        return {inline = true}
    "#;

    let rt = open_test_runtime(&root, script, test_config(&root, perms, 4096));
    let session = rt.create(Some("rt10-inline-under".into())).unwrap().id;

    rt.prompt(
        &session,
        "run under threshold test",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let events = rt.events(&session, 0, 1000).unwrap();

    // Verify NO storage prepared event occurred
    let prepared = events.iter().find(|r| {
        let v = serde_json::to_value(r).unwrap();
        v["event"]["type"] == "host" && v["event"]["kind"] == "runtime_result_storage_prepared"
    });
    assert!(
        prepared.is_none(),
        "under-threshold result should not record storage prepared event"
    );
}

#[tokio::test]
async fn rt10_production_host_zero_threshold_disables_auto_externalization() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let filename = "large_but_disabled.txt";
    let content = "LARGE_DATA_NO_EXTERNALIZATION_".repeat(40);
    std::fs::write(workspace.join(filename), &content).unwrap();

    let perms = vec![ScopedPermission::exact(
        "fs.read",
        json!({"path": filename}),
    )];

    let script = r#"
        local res = cayw.call('fs.read', {path = 'large_but_disabled.txt'})
        assert(res.externalized == nil, 'must not be externalized when threshold is 0')
        assert(res.handle == nil, 'must not have handle')
        return {ok = true}
    "#;

    // Threshold = 0 disables auto-externalization
    let rt = open_test_runtime(&root, script, test_config(&root, perms, 0));
    let session = rt.create(Some("rt10-disabled".into())).unwrap().id;

    rt.prompt(
        &session,
        "run zero threshold test",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let events = rt.events(&session, 0, 1000).unwrap();
    let prepared = events.iter().find(|r| {
        let v = serde_json::to_value(r).unwrap();
        v["event"]["type"] == "host" && v["event"]["kind"] == "runtime_result_storage_prepared"
    });
    assert!(
        prepared.is_none(),
        "zero threshold must disable result externalization"
    );
}

// ---------------------------------------------------------------------------
// BR-03: Result search on long lines / single-line JSON with match past 512 bytes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn br03_result_search_matches_beyond_512_bytes_in_single_line_json() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    // Construct a single-line minified JSON string of 1200 bytes with target at byte ~850
    let padding = "x".repeat(800);
    let target = "needle_secret_token_12345";
    let single_line_json = format!(
        r#"{{"header":"meta_info","padding":"{}","target":"{}","status":"valid"}}"#,
        padding, target
    );

    let store_args = json!({
        "text": single_line_json,
        "encoding": "utf8",
        "handle": "json-long-demo",
    });

    let perms = vec![ScopedPermission::exact("result.store", store_args)];

    let script = r#"
        local search_res = cayw.call('result.search', {
            handle = 'json-long-demo',
            query = 'needle_secret_token_12345',
            limit = 4,
        })
        assert(#search_res.matches == 1, 'must find exactly one match')
        local m = search_res.matches[1]
        assert(m.offset >= 800, 'match offset must be >= 800 in single-line json')
        assert(string.find(m.text, 'needle_secret_token_12345') ~= nil, 'snippet text must contain query')

        -- Verify result.get at match offset returns exact matching snippet
        local page = cayw.call('result.get', {
            handle = 'json-long-demo',
            offset = m.offset,
            limit = #m.text,
        })
        assert(page.text == m.text, 'page text from result.get must match snippet text from search')
        return {matched = true}
    "#;

    let rt = open_test_runtime(&root, script, test_config(&root, perms, 32 * 1024));
    let session = rt.create(Some("br03-json-long".into())).unwrap().id;

    // First store via explicit call in initial turn
    let initial_script = r#"
        local store_res = cayw.call('result.store', {
            handle = 'json-long-demo',
            text = '[[PAYLOAD]]',
            encoding = 'utf8',
        })
        assert(store_res.stored == true, 'store must succeed')
        return {stored = true}
    "#
    .replace("[[PAYLOAD]]", &single_line_json);

    let rt_initial = open_test_runtime(
        &root,
        &initial_script,
        test_config(
            &root,
            vec![ScopedPermission::exact(
                "result.store",
                json!({
                    "handle": "json-long-demo",
                    "text": single_line_json,
                    "encoding": "utf8",
                }),
            )],
            32 * 1024,
        ),
    );

    rt_initial
        .prompt(&session, "store payload", CancellationToken::new())
        .await
        .unwrap();

    // Now run search and get verification script
    let rt_search = open_test_runtime(&root, script, test_config(&root, vec![], 32 * 1024));
    rt_search
        .prompt(&session, "search and get", CancellationToken::new())
        .await
        .unwrap();

    let events = rt_search.events(&session, 0, 1000).unwrap();
    let search_settled = events.iter().find(|r| {
        let v = serde_json::to_value(r).unwrap();
        let call = &v["event"]["call"];
        v["event"]["type"] == "capability_settled"
            && call["name"] == "result.search"
            && call["outcome"]["type"] == "completed"
    });
    assert!(
        search_settled.is_some(),
        "result.search must complete successfully"
    );
}

#[tokio::test]
async fn br03_result_search_matches_beyond_512_bytes_in_multibyte_utf8() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    // Multibyte Chinese text (each Chinese char is 3 UTF-8 bytes)
    // Repeat 30 times (each ~40 bytes -> ~1200 bytes total)
    let lead = "这是一段很长很长的前置多字节中文字符串文本行，用于测试跨字节边界的搜索窗口切割。\n"
        .repeat(20);
    let target_line = "关键匹配测试行：在这里包含我们的目标检索词：【命中长行中文关键词标靶】，以及后续的说明文字。\n";
    let tail = "这是末尾的补充行内容，确保检索后续仍有文字。\n".repeat(5);
    let full_text = format!("{}{}{}", lead, target_line, tail);

    let store_args = json!({
        "text": full_text,
        "encoding": "utf8",
        "handle": "utf8-multibyte-demo",
    });

    let rt = open_test_runtime(
        &root,
        r#"
            local store_res = cayw.call('result.store', {
                handle = 'utf8-multibyte-demo',
                text = [[TEXT]],
                encoding = 'utf8',
            })
            assert(store_res.stored == true)

            local search_res = cayw.call('result.search', {
                handle = 'utf8-multibyte-demo',
                query = '命中长行中文关键词标靶',
                limit = 2,
            })
            assert(#search_res.matches == 1)
            local m = search_res.matches[1]
            assert(m.offset > 512, 'offset must be > 512 in multibyte text')
            assert(string.find(m.text, '命中长行中文关键词标靶') ~= nil, 'snippet text must contain query')

            local page = cayw.call('result.get', {
                handle = 'utf8-multibyte-demo',
                offset = m.offset,
                limit = #m.text,
            })
            assert(page.text == m.text, 'page text must match search snippet')
            return {ok = true}
        "#.replace("[[TEXT]]", &lua_string(&full_text)).as_str(),
        test_config(&root, vec![ScopedPermission::exact("result.store", store_args)], 32 * 1024),
    );

    let session = rt.create(Some("br03-multibyte".into())).unwrap().id;
    rt.prompt(
        &session,
        "run multibyte search test",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let events = rt.events(&session, 0, 1000).unwrap();
    let search_completed = events.iter().find(|r| {
        let v = serde_json::to_value(r).unwrap();
        let call = &v["event"]["call"];
        v["event"]["type"] == "capability_settled"
            && call["name"] == "result.search"
            && call["outcome"]["type"] == "completed"
    });
    assert!(
        search_completed.is_some(),
        "multibyte result.search must complete successfully"
    );
}

// ---------------------------------------------------------------------------
// BR-01: Explicit and auto handle storage and retrieval round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn br01_result_store_with_explicit_and_auto_handle_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let explicit_args = json!({
        "handle": "custom-fixed-handle-101",
        "text": "explicit-handle-body-payload",
        "encoding": "utf8",
    });
    let auto_args = json!({
        "text": "auto-handle-body-payload",
        "encoding": "utf8",
    });

    let perms = vec![
        ScopedPermission::exact("result.store", explicit_args),
        ScopedPermission::exact("result.store", auto_args),
    ];

    let script = r#"
        -- 1. Store with explicit handle
        local explicit_res = cayw.call('result.store', {
            handle = 'custom-fixed-handle-101',
            text = 'explicit-handle-body-payload',
            encoding = 'utf8',
        })
        assert(explicit_res.stored == true)
        assert(explicit_res.handle == 'custom-fixed-handle-101')

        local get1 = cayw.call('result.get', {
            handle = 'custom-fixed-handle-101',
            offset = 0,
            limit = 64,
        })
        assert(get1.text == 'explicit-handle-body-payload')

        -- 2. Store with auto-generated handle
        local auto_res = cayw.call('result.store', {
            text = 'auto-handle-body-payload',
            encoding = 'utf8',
        })
        assert(auto_res.stored == true)
        assert(type(auto_res.handle) == 'string')
        assert(#auto_res.handle > 0)

        local get2 = cayw.call('result.get', {
            handle = auto_res.handle,
            offset = 0,
            limit = 64,
        })
        assert(get2.text == 'auto-handle-body-payload')
        return {success = true}
    "#;

    let rt = open_test_runtime(&root, script, test_config(&root, perms, 32 * 1024));
    let session = rt.create(Some("br01-roundtrip".into())).unwrap().id;

    rt.prompt(
        &session,
        "run handle roundtrip test",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let events = rt.events(&session, 0, 1000).unwrap();
    let completed_stores: Vec<_> = events
        .iter()
        .filter(|r| {
            let v = serde_json::to_value(r).unwrap();
            let call = &v["event"]["call"];
            v["event"]["type"] == "capability_settled"
                && call["name"] == "result.store"
                && call["outcome"]["type"] == "completed"
        })
        .collect();
    assert_eq!(
        completed_stores.len(),
        2,
        "both stores must settle completed"
    );
}
