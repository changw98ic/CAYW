#![cfg(target_os = "macos")]
//! F01/F03 result handles: store → page/search/batch → release on ContentStore.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_content::Limits;
use cayw_runtime::{
    ContentConfig, Runtime, RuntimeConfig, ScopedPermission, fixture::script_response,
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(
                &request,
                &self.0,
                &format!("results-{}", request.messages.len()),
            ),
        }]))
    }
}

fn config(root: &Path, permissions: Vec<ScopedPermission>) -> RuntimeConfig {
    RuntimeConfig {
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "local-device".into(),
            limits: Limits {
                max_content_bytes: 64 * 1024,
                ..Default::default()
            },
        }),
        permissions,
        ..Default::default()
    }
}

fn open(root: &Path, script: &str, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "results"),
        config,
    )
    .unwrap()
}

fn completed(rt: &Runtime, session: &str, name: &str) -> Value {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|record| {
            let value = serde_json::to_value(record).unwrap();
            let call = &value["event"]["call"];
            (value["event"]["type"] == "capability_settled"
                && call["name"] == name
                && call["outcome"]["type"] == "completed")
                .then(|| call["outcome"]["result"].clone())
        })
        .unwrap_or_else(|| panic!("missing completed {name}"))
}

fn failed_contains(rt: &Runtime, session: &str, name: &str, needle: &str) {
    let found = rt
        .events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|record| {
            let value = serde_json::to_value(record).unwrap();
            let call = &value["event"]["call"];
            (value["event"]["type"] == "capability_settled"
                && call["name"] == name
                && call["outcome"]["type"] == "failed")
                .then(|| call["outcome"]["error"].as_str().unwrap_or("").to_string())
        })
        .expect("failed receipt");
    assert!(
        found.contains(needle),
        "expected failure containing {needle:?}, got {found}"
    );
}

#[tokio::test]
async fn store_get_search_batch_and_release_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let body = "line-one\nneedle alpha\nline-three\nneedle beta\ntrail";
    let store_args = json!({"text":body,"encoding":"utf8","handle":"demo1"});
    let get_args = json!({"handle":"demo1","offset":0,"limit":16});
    let get_more = json!({"handle":"demo1","offset":16,"limit":64});
    let search_args = json!({"handle":"demo1","query":"needle","limit":8});
    let batch_args = json!({"handles":["demo1","missing"],"include_page":true,"limit":8});
    let release_args = json!({"handle":"demo1"});
    let get_after = json!({"handle":"demo1","offset":0,"limit":8});

    let cfg = config(
        &root,
        vec![
            ScopedPermission::exact("result.store", store_args.clone()),
            ScopedPermission::exact("result.get", get_args.clone()),
            ScopedPermission::exact("result.get", get_more.clone()),
            ScopedPermission::exact("result.search", search_args.clone()),
            ScopedPermission::exact("result.batch_get", batch_args.clone()),
            ScopedPermission::exact("result.release", release_args.clone()),
            ScopedPermission::exact("result.get", get_after.clone()),
        ],
    );

    let script = format!(
        r#"
local stored = cayw.call('result.store', {{text=[[{body}]],encoding='utf8',handle='demo1'}})
assert(stored.handle == 'demo1')
assert(stored.bytes == {bytes})
local page = cayw.call('result.get', {{handle='demo1',offset=0,limit=16}})
assert(page.has_more == true)
local rest = cayw.call('result.get', {{handle='demo1',offset=16,limit=64}})
assert(rest.has_more == false)
assert(page.text .. rest.text == [[{body}]])
local found = cayw.call('result.search', {{handle='demo1',query='needle',limit=8}})
assert(#found.matches == 2)
local batch = cayw.call('result.batch_get', {{handles={{'demo1','missing'}},include_page=true,limit=8}})
assert(batch.items[1].ok == true)
assert(batch.items[2].ok == false)
local released = cayw.call('result.release', {{handle='demo1'}})
assert(released.released == true)
local ok, err = pcall(function()
  return cayw.call('result.get', {{handle='demo1',offset=0,limit=8}})
end)
assert(ok == false)
return {{handle=stored.handle, matches=#found.matches, released=released.released, refused=tostring(err)}}
"#,
        body = body,
        bytes = body.len(),
    );

    let rt = open(&root, &script, cfg);
    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "results", CancellationToken::new())
        .await
        .unwrap();

    let stored = completed(&rt, &session, "result.store");
    assert_eq!(stored["handle"], "demo1");
    assert_eq!(stored["bytes"], body.len() as u64);
    assert_eq!(stored["released"], false);

    let page = completed(&rt, &session, "result.get");
    // last get is the post-release attempt; find search instead via inspect
    let view = rt
        .result_inspect(&session, None, "demo1")
        .unwrap()
        .expect("handle metadata survives release");
    assert!(view.released);
    assert_eq!(view.bytes, body.len() as u64);

    let search = rt
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|record| {
            let value = serde_json::to_value(record).unwrap();
            let call = &value["event"]["call"];
            (value["event"]["type"] == "capability_settled"
                && call["name"] == "result.search"
                && call["outcome"]["type"] == "completed")
                .then(|| call["outcome"]["result"].clone())
        })
        .unwrap();
    assert_eq!(search["matches"].as_array().unwrap().len(), 2);

    failed_contains(&rt, &session, "result.get", "released");
    let _ = page;
}

#[tokio::test]
async fn task_scoped_handle_is_isolated_from_session_and_other_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let store_args = json!({"text":"secret-task","encoding":"utf8","scope":"task","handle":"t1"});
    let get_task = json!({"handle":"t1","scope":"task","offset":0,"limit":32});
    let get_session = json!({"handle":"t1","scope":"session","offset":0,"limit":32});
    let cfg = config(
        &root,
        vec![
            ScopedPermission::exact("result.store", store_args),
            ScopedPermission::exact("result.get", get_task),
            ScopedPermission::exact("result.get", get_session),
        ],
    );
    let rt = open(
        &root,
        "local s=cayw.call('result.store',{text='secret-task',encoding='utf8',scope='task',handle='t1'})\nlocal ok=cayw.call('result.get',{handle='t1',scope='task',offset=0,limit=32})\nlocal bad=cayw.call('result.get',{handle='t1',scope='session',offset=0,limit=32})\nreturn {ok=ok.text,bad=bad}",
        cfg,
    );
    let a = rt.create(None).unwrap().id;
    rt.prompt(&a, "task isolate", CancellationToken::new())
        .await
        .unwrap();
    let task = rt.tasks(&a).unwrap().pop().unwrap();
    let view = rt
        .result_inspect(&a, Some(&task.id), "t1")
        .unwrap()
        .unwrap();
    assert_eq!(view.task_id.as_deref(), Some(task.id.as_str()));
    assert!(rt.result_inspect(&a, None, "t1").unwrap().is_none());
    let b = rt.create(None).unwrap().id;
    assert!(
        rt.result_inspect(&b, Some(&task.id), "t1").is_err()
            || rt
                .result_inspect(&b, Some(&task.id), "t1")
                .unwrap()
                .is_none()
    );
}

#[tokio::test]
async fn filter_read_original_reopen_and_release() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let script_path = root.join("noisy.sh");
    let script_body = "#!/bin/sh\ncat <<'EOF'\nCompiling demo v0.1.0\n   Compiling unused v0.2.0\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.01s\n     Running unittests src/lib.rs\n\nrunning 3 tests\ntest a::ok ... ok\ntest b::ok ... ok\ntest c::ok ... ok\n\ntest result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\nEOF\n";
    std::fs::write(&script_path, script_body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    let profile = cayw_host::Profile {
        kind: cayw_host::ProfileKind::Test,
        program: script_path,
        args: vec![],
        env: std::collections::BTreeMap::new(),
        timeout: std::time::Duration::from_secs(10),
        max_output_bytes: 64 * 1024,
        allow_filter: false,
        io: cayw_host::ProcessIo::Pipes,
    };

    let mut cfg = config(
        &root,
        vec![ScopedPermission::exact(
            "test.run",
            json!({"profile": "noisy"}),
        )],
    );
    cfg.host.profiles.insert("noisy".into(), profile);
    cfg.host.output_filter = Some(cayw_host::FilterOptions {
        enabled: true,
        profile: cayw_host::FilterProfile::CargoLike,
        head_lines: 2,
        tail_lines: 2,
        max_lines: 12,
    });

    let script1 = r#"
local res = cayw.call('test.run', {profile='noisy'})
assert(res.output_filter ~= nil)
assert(res.output_filter.stdout.original_handle ~= nil)
local handle = res.output_filter.stdout.original_handle.handle
assert(handle ~= nil)
local page = cayw.call('result.get', {handle=handle, offset=0, limit=64})
assert(page.text ~= nil)
return {handle=handle, text_len=#page.text}
"#;

    let rt1 = open(&root, script1, cfg.clone());
    let session = rt1.create(None).unwrap().id;
    rt1.prompt(&session, "filter run", CancellationToken::new())
        .await
        .unwrap();

    let test_res = completed(&rt1, &session, "test.run");
    let handle = test_res["output_filter"]["stdout"]["original_handle"]["handle"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(handle.len(), 64);

    let get1 = completed(&rt1, &session, "result.get");
    assert_eq!(get1["handle"], handle);
    assert!(!get1["text"].as_str().unwrap().is_empty());

    // Drop first runtime to simulate process exit
    drop(rt1);

    // Reopen runtime from the same database and workspace
    let script2 = format!(
        r#"
local page = cayw.call('result.get', {{handle='{handle}', offset=0, limit=64}})
assert(page.text ~= nil)
local rel = cayw.call('result.release', {{handle='{handle}'}})
assert(rel.released == true)
local ok, err = pcall(function()
  return cayw.call('result.get', {{handle='{handle}', offset=0, limit=64}})
end)
assert(ok == false)
return {{reopened=true, released=rel.released}}
"#,
        handle = handle,
    );

    let rt2 = open(&root, &script2, cfg);
    rt2.prompt(&session, "reopened task", CancellationToken::new())
        .await
        .unwrap();

    let rel = completed(&rt2, &session, "result.release");
    assert_eq!(rel["released"], true);
    failed_contains(&rt2, &session, "result.get", "released");
}

#[tokio::test]
async fn shared_read_auto_externalizes_oversized_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    // Create a 40 KiB file (auto_externalize_threshold_bytes set to 32 KiB)
    let content = "0123456789abcdef\n".repeat(2500);
    std::fs::write(workspace.join("large.txt"), &content).unwrap();

    let mut cfg = config(
        &root,
        vec![ScopedPermission::exact(
            "fs.read",
            json!({"path": "large.txt"}),
        )],
    );
    cfg.host.frozen_reads = true;
    cfg.auto_externalize_threshold_bytes = 32 * 1024;

    let script = r#"
local res = cayw.call('fs.read', {path='large.txt'})
assert(res.externalized == true)
assert(res.handle ~= nil)
local page = cayw.call('result.get', {handle=res.handle, offset=0, limit=100})
assert(page.text ~= nil)
return {handle=res.handle, page_len=#page.text}
"#;

    let rt = open(&root, script, cfg);
    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "read big", CancellationToken::new())
        .await
        .unwrap();

    let page = completed(&rt, &session, "result.get");
    assert!(page["total_bytes"].as_u64().unwrap() >= content.len() as u64);
    assert!(page["text"].as_str().unwrap().contains("0123456789abcdef"));
}

struct StepScript(std::sync::Mutex<std::collections::VecDeque<String>>);
impl Provider for StepScript {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let is_tool_result = matches!(
            request.messages.last(),
            Some(cayw_ai::Message::ToolResult(_))
        );
        let script = if is_tool_result {
            String::new()
        } else {
            self.0.lock().unwrap().pop_front().unwrap_or_default()
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(
                &request,
                &script,
                &format!("results-{}", request.messages.len()),
            ),
        }]))
    }
}

#[tokio::test]
async fn subsequent_task_in_same_session_inherits_authorization_for_session_result() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let store_args = json!({
        "text": "session-scoped-inherited-payload-data",
        "encoding": "utf8",
        "scope": "session",
        "handle": "session-result-h1",
    });

    // Only result.store has pre-configured ScopedPermission.
    // result.get is NOT in the static configuration!
    let cfg = config(
        &root,
        vec![ScopedPermission::exact("result.store", store_args)],
    );

    let script1 = r#"
local s = cayw.call('result.store', {
    text = 'session-scoped-inherited-payload-data',
    encoding = 'utf8',
    scope = 'session',
    handle = 'session-result-h1',
})
assert(s.handle == 'session-result-h1')
return {stored = s.handle}
"#;

    let script2 = r#"
local g = cayw.call('result.get', {
    handle = 'session-result-h1',
    offset = 0,
    limit = 64,
})
assert(g.text == 'session-scoped-inherited-payload-data')
return {text = g.text}
"#;

    let provider = Arc::new(StepScript(std::sync::Mutex::new(
        vec![script1.to_string(), script2.to_string()].into(),
    )));

    let rt = Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "results"),
        cfg,
    )
    .unwrap();

    let session = rt.create(None).unwrap().id;

    // Task 1: stores the result
    rt.prompt(&session, "task 1", CancellationToken::new())
        .await
        .unwrap();
    let stored = completed(&rt, &session, "result.store");
    assert_eq!(stored["handle"], "session-result-h1");

    // Task 2: in the same session, reads the result without explicit ScopedPermission
    rt.prompt(&session, "task 2", CancellationToken::new())
        .await
        .unwrap();
    let get_res = completed(&rt, &session, "result.get");
    assert_eq!(get_res["handle"], "session-result-h1");
    assert_eq!(get_res["text"], "session-scoped-inherited-payload-data");

    // Another session cannot access session 1's handle
    let other_session = rt.create(None).unwrap().id;
    assert!(
        rt.result_inspect(&other_session, None, "session-result-h1")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn headless_config_output_filter_preserves_availability_without_content_store() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let config_path = root.join("headless.json");
    let json_config = json!({
        "budgets": {
            "max_turns": 10,
            "max_total_tokens": 100000,
            "max_run_time_ms": 30000,
            "max_capability_calls": 32,
            "max_capability_bytes": 1048576,
            "auto_externalize_threshold_bytes": 32768,
            "host_wait_ms": 10000,
            "host_call_timeout_ms": 10000
        },
        "output_filter": {
            "enabled": true,
            "profile": "cargo_like",
            "head_lines": 5,
            "tail_lines": 5,
            "max_lines": 20
        },
        "profiles": {
            "hello": {
                "kind": "test",
                "program": "/bin/echo",
                "args": ["hello world"],
                "allow_filter": true
            }
        }
    });
    std::fs::write(&config_path, serde_json::to_vec(&json_config).unwrap()).unwrap();

    let headless = cayw_runtime::HeadlessConfig::load(&config_path).unwrap();
    assert!(headless.output_filter.is_some());

    let mut rt_cfg = headless.into_runtime_config().unwrap();
    assert!(rt_cfg.host.output_filter.is_some());
    assert!(rt_cfg.content.is_none());

    // Verify Runtime opens with this config and executes profile successfully
    rt_cfg.permissions = vec![ScopedPermission::exact(
        "test.run",
        json!({"profile": "hello"}),
    )];

    let script = r#"
local res = cayw.call('test.run', {profile='hello'})
assert(res.exit_code == 0)
assert(res.success == true)
return {ok=true}
"#;

    let rt = Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "results"),
        rt_cfg,
    )
    .unwrap();

    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "check hello", CancellationToken::new())
        .await
        .unwrap();

    let res = completed(&rt, &session, "test.run");
    assert_eq!(res["exit_code"], 0);
    assert_eq!(res["success"], true);
}
