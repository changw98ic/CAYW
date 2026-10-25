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
