#![cfg(target_os = "macos")]
//! Result sources integration tests:
//! Host (web.fetch, web.extract, vision.crop) -> Device ContentStore -> result.get (utf8 & base64)
//! Persistence across reopen and cross-session isolation.
//! Honoring explicit denial / revocation over automatic session result reads.
//! Standalone Host SDK fallback when ContentStore is absent.

use base64::Engine;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_codemode::CapabilityHost;
use cayw_content::Limits as ContentLimits;
use cayw_core::ApprovalRequest;
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate};
use cayw_runtime::{
    ApprovalChoice, ContentConfig, Runtime, RuntimeConfig, ScopedPermission,
    fixture::script_response,
};
use cayw_vision::Limits as VisionLimits;
use cayw_web_access::WebAccessOptions;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

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

fn test_config(root: &Path, permissions: Vec<ScopedPermission>) -> RuntimeConfig {
    RuntimeConfig {
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "test-device".into(),
            limits: ContentLimits {
                max_content_bytes: 128 * 1024,
                ..Default::default()
            },
        }),
        host: HostConfig {
            web_access: Some(WebAccessOptions::fixture()),
            vision: Some(VisionLimits::default()),
            ..Default::default()
        },
        permissions,
        ..Default::default()
    }
}

fn open_runtime(root: &Path, script: &str, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "results"),
        config,
    )
    .unwrap()
}

fn completed_result(rt: &Runtime, session: &str, name: &str) -> Value {
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
        .unwrap_or_else(|| panic!("missing completed event for {name}"))
}

fn completed_results(rt: &Runtime, session: &str, name: &str) -> Vec<Value> {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|record| {
            let value = serde_json::to_value(record).unwrap();
            let call = &value["event"]["call"];
            (value["event"]["type"] == "capability_settled"
                && call["name"] == name
                && call["outcome"]["type"] == "completed")
                .then(|| call["outcome"]["result"].clone())
        })
        .collect()
}

fn failed_contains(rt: &Runtime, session: &str, name: &str, needle: &str) {
    let found = rt
        .events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|record| {
            let value = serde_json::to_value(record).unwrap();
            let event = &value["event"];
            if event["type"] == "capability_settled" {
                let call = &event["call"];
                if call["name"] == name && call["outcome"]["type"] == "failed" {
                    return call["outcome"]["error"].as_str().map(str::to_string);
                }
            } else if event["type"] == "execution_finished" {
                let exec = &event["execution"];
                if exec["state"] == "failed" {
                    return exec["terminal"]["error"].as_str().map(str::to_string);
                }
            }
            None
        })
        .unwrap_or_else(|| panic!("missing failed event for {name}"));
    assert!(
        found.contains(needle),
        "expected failure containing {needle:?}, got {found}"
    );
}

fn sample_image_8x8() -> (String, Vec<u8>) {
    let mut rgba = vec![0u8; 8 * 8 * 4];
    for y in 0..4 {
        for x in 0..4 {
            let idx = (y * 8 + x) * 4;
            rgba[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&rgba);
    let mut cropped_expected = vec![0u8; 4 * 4 * 4];
    for i in 0..4 * 4 {
        cropped_expected[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
    }
    (b64, cropped_expected)
}

#[tokio::test]
async fn web_fetch_and_extract_produce_formal_result_handle_and_read_via_result_get() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let perms = vec![
        ScopedPermission::exact("web.fetch", json!({"url": "https://example.com/"})),
        ScopedPermission::exact("web.extract", json!({"url": "https://example.com/"})),
    ];

    let script = r#"
        local fetch_res = cayw.call('web.fetch', {url='https://example.com/'})
        local extract_res = cayw.call('web.extract', {url='https://example.com/'})
        local get_res = cayw.call('result.get', {handle=fetch_res.handle, offset=0, limit=2048})
        return {fetch=fetch_res, extract=extract_res, page=get_res}
    "#;

    let rt = open_runtime(&root, script, test_config(&root, perms));
    let session = rt.create(None).unwrap().id;

    rt.prompt(
        &session,
        "fetch and extract web content",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let fetch_out = completed_result(&rt, &session, "web.fetch");
    assert!(fetch_out.get("handle").is_some());
    let handle = fetch_out["handle"].as_str().unwrap();
    assert_ne!(handle, "webc-1", "handle must not be private webc-N stub");
    assert_eq!(fetch_out["scope"], "session");
    assert_eq!(fetch_out["session_id"], session);
    assert_eq!(fetch_out["encoding"], "utf8");
    assert_eq!(fetch_out["stored"], true);
    assert_eq!(fetch_out["url"], "https://example.com/");
    assert_eq!(fetch_out["final_url"], "https://example.com/");
    assert_eq!(fetch_out["status"], 200);
    assert_eq!(fetch_out["citation"]["content_ref"], handle);
    assert!(fetch_out["bytes"].as_u64().unwrap() > 0);
    assert!(fetch_out["content_hash"].is_string());

    let extract_out = completed_result(&rt, &session, "web.extract");
    assert!(extract_out.get("handle").is_some());
    assert_eq!(extract_out["scope"], "session");
    assert_eq!(extract_out["session_id"], session);
    assert_eq!(extract_out["encoding"], "utf8");
    assert_eq!(extract_out["stored"], true);
    assert_eq!(extract_out["title"], "Example Domain");
    assert_eq!(
        extract_out["citation"]["content_ref"],
        extract_out["handle"]
    );
    assert_ne!(fetch_out["handle"], extract_out["handle"]);

    let get_out = completed_result(&rt, &session, "result.get");
    assert_eq!(get_out["handle"], handle);
    assert_eq!(get_out["released"], false);
    let text = get_out["text"].as_str().unwrap();
    assert!(
        text.contains("Example Domain"),
        "result.get body must contain expected text: {text}"
    );
}

#[tokio::test]
async fn vision_crop_produces_formal_base64_result_handle_and_read_via_result_get() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let (b64_img, expected_raw) = sample_image_8x8();

    let perms = vec![ScopedPermission::exact(
        "vision.crop",
        json!({
            "image": {
                "width": 8,
                "height": 8,
                "rgba_base64": b64_img,
            },
            "x": 0,
            "y": 0,
            "width": 4,
            "height": 4,
        }),
    )];

    let script = format!(
        r#"
        local crop_res = cayw.call('vision.crop', {{
            image = {{ width = 8, height = 8, rgba_base64 = '{b64_img}' }},
            x = 0, y = 0, width = 4, height = 4
        }})
        local get_res = cayw.call('result.get', {{handle=crop_res.handle, offset=0, limit=2048}})
        return {{crop=crop_res, page=get_res}}
    "#
    );

    let rt = open_runtime(&root, &script, test_config(&root, perms));
    let session = rt.create(None).unwrap().id;

    rt.prompt(&session, "crop image", CancellationToken::new())
        .await
        .unwrap();

    let crop_out = completed_result(&rt, &session, "vision.crop");
    assert!(crop_out.get("handle").is_some());
    let handle = crop_out["handle"].as_str().unwrap();
    assert_eq!(crop_out["scope"], "session");
    assert_eq!(crop_out["session_id"], session);
    assert_eq!(crop_out["encoding"], "base64");
    assert_eq!(crop_out["stored"], true);
    assert_eq!(crop_out["width"], 4);
    assert_eq!(crop_out["height"], 4);
    assert_eq!(crop_out["mime_type"], "image/x-rgba8");
    assert!(crop_out["image_sha256"].is_string());
    assert!(crop_out["bytes"].as_u64().unwrap() > 0);
    assert!(crop_out["content_hash"].is_string());
    assert!(
        crop_out.get("rgba_base64").is_none(),
        "vision.crop with ContentStore must return formal ResultHandleView, not raw rgba_base64"
    );

    let get_out = completed_result(&rt, &session, "result.get");
    assert_eq!(get_out["handle"], handle);
    assert_eq!(get_out["released"], false);
    let page_text = get_out["text"].as_str().unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(page_text)
        .expect("page text must be valid base64");
    assert_eq!(decoded, expected_raw);
    assert_eq!(crop_out["bytes"], page_text.len() as u64);
    assert_eq!(
        crop_out["content_hash"],
        cayw_content::ContentHash::of(page_text.as_bytes()).to_string()
    );
}

#[tokio::test]
async fn session_reopen_and_cross_session_isolation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let (b64_img, _) = sample_image_8x8();

    let perms = vec![
        ScopedPermission::exact("web.fetch", json!({"url": "https://example.com/"})),
        ScopedPermission::exact(
            "vision.crop",
            json!({
                "image": {
                    "width": 8,
                    "height": 8,
                    "rgba_base64": b64_img,
                },
                "x": 0,
                "y": 0,
                "width": 4,
                "height": 4,
            }),
        ),
    ];

    let populate_script = format!(
        r#"
        local fetch_res = cayw.call('web.fetch', {{url='https://example.com/'}})
        local crop_res = cayw.call('vision.crop', {{
            image = {{ width = 8, height = 8, rgba_base64 = '{b64_img}' }},
            x = 0, y = 0, width = 4, height = 4
        }})
        return {{fetch=fetch_res, crop=crop_res}}
    "#
    );

    let (session_a, web_handle, vision_handle) = {
        let rt = open_runtime(&root, &populate_script, test_config(&root, perms.clone()));
        let s_a = rt.create(None).unwrap().id;
        rt.prompt(&s_a, "populate handles", CancellationToken::new())
            .await
            .unwrap();

        let fetch_res = completed_result(&rt, &s_a, "web.fetch");
        let crop_res = completed_result(&rt, &s_a, "vision.crop");

        (
            s_a,
            fetch_res["handle"].as_str().unwrap().to_string(),
            crop_res["handle"].as_str().unwrap().to_string(),
        )
    };

    // Reopen runtime from disk and verify Session A still reads its own handles.
    let read_script = format!(
        r#"
        local p1 = cayw.call('result.get', {{handle='{web_handle}', offset=0, limit=2048}})
        local p2 = cayw.call('result.get', {{handle='{vision_handle}', offset=0, limit=2048}})
        return {{p1=p1, p2=p2}}
    "#
    );

    {
        let rt = open_runtime(&root, &read_script, test_config(&root, perms.clone()));
        rt.prompt(
            &session_a,
            "read reopened handles",
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let pages = completed_results(&rt, &session_a, "result.get");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0]["handle"], web_handle);
        assert_eq!(pages[1]["handle"], vision_handle);
        assert!(
            pages[0]["text"]
                .as_str()
                .unwrap()
                .contains("<title>Example Domain</title>")
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(pages[1]["text"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, sample_image_8x8().1);
    }

    // Now in Session B, verify reading Session A's handles is refused (cross-session isolation).
    let cross_read_script = format!(
        r#"
        local ok, err = pcall(function()
            return cayw.call('result.get', {{handle='{web_handle}', offset=0, limit=2048}})
        end)
        assert(ok == false, 'cross-session read must fail')
        return {{refused=tostring(err)}}
    "#
    );

    {
        let mut b_perms = perms.clone();
        b_perms.push(ScopedPermission::exact(
            "result.get",
            json!({"handle": web_handle, "offset": 0, "limit": 2048}),
        ));
        let rt = open_runtime(&root, &cross_read_script, test_config(&root, b_perms));
        let session_b = rt.create(None).unwrap().id;
        rt.prompt(
            &session_b,
            "attempt cross-session read",
            CancellationToken::new(),
        )
        .await
        .unwrap();

        failed_contains(&rt, &session_b, "result.get", "result handle not found");
    }
}

async fn wait_for_approval(rt: &Runtime, session_id: &str) -> ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(a) = rt.get(session_id).unwrap().pending_approvals.first() {
                return a.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("approval request timed out")
}

#[tokio::test]
async fn explicit_denial_not_bypassed_by_later_handle_and_new_approval_restores_reads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let store_args = json!({"handle":"denied-handle","text":"secret data","encoding":"utf8"});
    let perms = vec![ScopedPermission::exact("result.store", store_args.clone())];

    let rt = open_runtime(
        &root,
        "return cayw.call('result.get', {handle='denied-handle', offset=0, limit=2048})",
        test_config(&root, perms.clone()),
    );
    let session = rt.create(None).unwrap().id;

    // 1. First read before handle exists triggers approval request.
    let rt_clone = rt.clone();
    let s_clone = session.clone();
    let join_handle = tokio::spawn(async move {
        rt_clone
            .prompt(&s_clone, "read before exist", CancellationToken::new())
            .await
    });

    let app = wait_for_approval(&rt, &session).await;
    assert_eq!(app.input.capability, "result.get");

    // 2. User explicitly denies approval.
    rt.approve(&session, &app.id, ApprovalChoice::Deny, "security-officer")
        .unwrap();

    let _view = join_handle.await.unwrap().unwrap();
    failed_contains(&rt, &session, "result.get", "denied");

    // 3. Now store the result with that exact handle.
    let store_script = r#"
        local s = cayw.call('result.store', {handle='denied-handle', text='secret data', encoding='utf8'})
        return s
    "#;
    let rt_store = open_runtime(&root, store_script, test_config(&root, perms.clone()));
    rt_store
        .prompt(&session, "store handle", CancellationToken::new())
        .await
        .unwrap();

    // 4. Verify the handle physically exists in ContentStore for this session.
    let inspected = rt_store
        .result_inspect(&session, None, "denied-handle")
        .unwrap()
        .expect("handle must exist in ContentStore");
    assert_eq!(inspected.handle, "denied-handle");

    // 5. Subsequent attempt to read 'denied-handle' must NOT be auto-admitted by is_session_result_read
    // because an explicit denial was recorded for this session and scope.
    let read_again_script = r#"
        return cayw.call('result.get', {handle='denied-handle', offset=0, limit=2048})
    "#;
    let rt_read = open_runtime(&root, read_again_script, test_config(&root, perms.clone()));
    let rt_clone2 = rt_read.clone();
    let s_clone2 = session.clone();
    let join_read = tokio::spawn(async move {
        rt_clone2
            .prompt(&s_clone2, "read after stored", CancellationToken::new())
            .await
    });

    // It must either be blocked at approval again (not auto-admitted) or fail immediately.
    let app2 = wait_for_approval(&rt_read, &session).await;
    assert_eq!(app2.input.capability, "result.get");

    rt_read
        .approve(
            &session,
            &app2.id,
            ApprovalChoice::AllowTask,
            "trusted-reviewer",
        )
        .unwrap();
    join_read.await.unwrap().unwrap();
    assert_eq!(
        completed_result(&rt_read, &session, "result.get")["text"],
        "secret data"
    );
    drop(rt_read);
    drop(rt_store);
    drop(rt);

    // Task teardown revokes task grants. It must not act as a permanent user denial.
    let rt = open_runtime(
        &root,
        "return cayw.call('result.get',{handle='denied-handle',offset=0,limit=2048})",
        test_config(&root, perms),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        rt.prompt(
            &session,
            "read after task grant cleanup",
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        completed_result(&rt, &session, "result.get")["text"],
        "secret data"
    );
}

#[tokio::test]
async fn explicit_revoke_blocks_automatic_read_until_a_new_exact_approval() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let rt = open_runtime(
        &root,
        "pcall(function() return cayw.call('result.get',{handle='revoked-result',limit=2048}) end); return cayw.call('result.store',{handle='hold',text='waiting for another decision'})",
        test_config(&root, vec![]),
    );
    let session = rt.create(None).unwrap().id;
    let run_rt = rt.clone();
    let run_session = session.clone();
    let run = tokio::spawn(async move {
        run_rt
            .prompt(
                &run_session,
                "approve a missing result read",
                CancellationToken::new(),
            )
            .await
    });
    let approval = wait_for_approval(&rt, &session).await;
    rt.approve(
        &session,
        &approval.id,
        ApprovalChoice::AllowTask,
        "reviewer",
    )
    .unwrap();
    let holding = wait_for_approval(&rt, &session).await;
    assert_eq!(holding.input.capability, "result.store");
    failed_contains(&rt, &session, "result.get", "not found");
    let grant = rt
        .grants(&session)
        .unwrap()
        .into_iter()
        .find(|grant| grant.approval_id == approval.id)
        .unwrap();
    assert_eq!(grant.state, "active");
    rt.revoke(&session, &grant.id, "reviewer").unwrap();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        rt.audit(&session, 0, 1000)
            .unwrap()
            .iter()
            .any(|row| row.kind == "grant_revoked" && row.data["grant_id"] == grant.id)
    );
    drop(rt);

    let store = json!({"text":"revoked content","handle":"revoked-result"});
    let rt = open_runtime(
        &root,
        "return cayw.call('result.store',{text='revoked content',handle='revoked-result'})",
        test_config(&root, vec![ScopedPermission::exact("result.store", store)]),
    );
    rt.prompt(&session, "produce the result", CancellationToken::new())
        .await
        .unwrap();
    drop(rt);

    let rt = open_runtime(
        &root,
        "return cayw.call('result.get',{handle='revoked-result',limit=2048})",
        test_config(&root, vec![]),
    );
    let run_rt = rt.clone();
    let run_session = session.clone();
    let mut run = tokio::spawn(async move {
        run_rt
            .prompt(
                &run_session,
                "read revoked result",
                CancellationToken::new(),
            )
            .await
    });
    tokio::select! {
        result = &mut run => panic!("read completed before a new exact approval: {result:?}; audit={:?}; grants={:?}", rt.audit(&session, 0, 1000).unwrap(), rt.grants(&session).unwrap()),
        approval = wait_for_approval(&rt, &session) => {
            assert_eq!(approval.input.capability, "result.get");
        }
    }
    let approval = rt
        .get(&session)
        .unwrap()
        .pending_approvals
        .into_iter()
        .find(|approval| approval.input.capability == "result.get")
        .unwrap();
    assert!(completed_results(&rt, &session, "result.get").is_empty());
    rt.approve(
        &session,
        &approval.id,
        ApprovalChoice::AllowOnce,
        "reviewer",
    )
    .unwrap();
    run.await.unwrap().unwrap();
    assert_eq!(
        completed_result(&rt, &session, "result.get")["text"],
        "revoked content"
    );
    drop(rt);

    let rt = open_runtime(
        &root,
        "return cayw.call('result.get',{handle='revoked-result',limit=2048})",
        test_config(&root, vec![]),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        rt.prompt(
            &session,
            "read with latest decision",
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        completed_result(&rt, &session, "result.get")["text"],
        "revoked content"
    );
}

#[tokio::test]
async fn producer_storage_failure_is_a_failed_original_call_without_a_fallback_handle() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let mut config = test_config(
        &root,
        vec![ScopedPermission::exact(
            "web.fetch",
            json!({"url":"https://example.com/"}),
        )],
    );
    config.content.as_mut().unwrap().limits.max_content_bytes = 64;
    let rt = open_runtime(
        &root,
        "return cayw.call('web.fetch',{url='https://example.com/'})",
        config,
    );
    let session = rt.create(None).unwrap().id;
    rt.prompt(
        &session,
        "storage is deliberately too small",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    failed_contains(&rt, &session, "web.fetch", "content exceeds");
    assert!(completed_results(&rt, &session, "web.fetch").is_empty());
    assert_eq!(
        rt.events(&session, 0, 1000)
            .unwrap()
            .into_iter()
            .filter(|record| {
                let value = serde_json::to_value(record).unwrap();
                value["event"]["type"] == "capability_started"
                    && value["event"]["call"]["name"] == "web.fetch"
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn configured_star_scope_never_grants_a_result_operation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let rt = open_runtime(
        &root,
        "return cayw.call('result.store',{handle='star',text='must require exact approval'})",
        test_config(
            &root,
            vec![ScopedPermission {
                capability: "result.store".into(),
                scope: "*".into(),
            }],
        ),
    );
    let session = rt.create(None).unwrap().id;
    let run_rt = rt.clone();
    let run_session = session.clone();
    let run = tokio::spawn(async move {
        run_rt
            .prompt(&run_session, "exact scope only", CancellationToken::new())
            .await
    });
    let approval = wait_for_approval(&rt, &session).await;
    rt.approve(&session, &approval.id, ApprovalChoice::Deny, "reviewer")
        .unwrap();
    run.await.unwrap().unwrap();
    assert!(rt.result_inspect(&session, None, "star").unwrap().is_none());
}

#[tokio::test]
async fn produced_handles_keep_encodings_separate_and_released_filtered_originals_stay_released() {
    use cayw_host::FilteredOriginalStore;
    use cayw_vision::ImageSink;
    use cayw_web_access::ContentSink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let rt = open_runtime(&root, "return {}", test_config(&root, vec![]));
    let session = rt.create(None).unwrap().id;
    let store = Arc::new(
        cayw_content::ContentStore::open(
            root.join("content"),
            cayw_content::Id::new("test-device").unwrap(),
            ContentLimits {
                max_content_bytes: 128 * 1024,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let web = cayw_host::ContentWebSink::new(store.clone(), &session);
    let vision = cayw_host::ContentVisionSink::new(store.clone(), &session);
    let filtered = cayw_host::ContentFilteredOriginalStore::with_session(store.clone(), &session);
    let bytes = b"AAAA";
    let digest = cayw_content::ContentHash::of(bytes).to_string();
    let text = web.put("same-source", bytes, Some("text/plain")).unwrap();
    let binary = web
        .put("same-source", bytes, Some("application/octet-stream"))
        .unwrap();
    let image = vision
        .put("same-source", bytes, Some("image/x-rgba8"))
        .unwrap();
    assert_ne!(text["handle"], binary["handle"]);
    assert_ne!(image["handle"], binary["handle"]);
    assert_eq!(text["encoding"], "utf8");
    assert_eq!(binary["encoding"], "base64");
    assert_eq!(
        web.get(binary["handle"].as_str().unwrap())
            .unwrap()
            .unwrap(),
        bytes
    );
    let original = filtered.put_original(&digest, bytes).unwrap();
    assert_eq!(original["handle"], digest);
    drop(rt);
    let script = format!("return cayw.call('result.release',{{handle='{digest}'}})");
    let rt = open_runtime(&root, &script, test_config(&root, vec![]));
    rt.prompt(
        &session,
        "release filtered original",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        filtered
            .get_original(&digest)
            .unwrap_err()
            .message
            .contains("released")
    );
    let produced = filtered.put_original(&digest, bytes).unwrap();
    assert_ne!(produced["handle"], digest);
    assert_eq!(produced["content_hash"], digest);
    assert_eq!(
        filtered
            .get_original(produced["handle"].as_str().unwrap())
            .unwrap()
            .unwrap(),
        bytes
    );
    assert!(
        rt.result_inspect(&session, None, &digest)
            .unwrap()
            .unwrap()
            .released
    );
    // Metadata is authoritative: a matching body cannot hide a conflicting encoding.
    let meta = cayw_host::result_meta_key(
        "test-device",
        &session,
        None,
        text["handle"].as_str().unwrap(),
    )
    .unwrap();
    let (record, raw) = store.try_read(&meta).unwrap().unwrap();
    let mut value: Value = serde_json::from_slice(&raw).unwrap();
    value["encoding"] = json!("json");
    store
        .try_put(
            &meta,
            record.expected(),
            &serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    assert!(
        web.put("same-source", bytes, Some("text/plain"))
            .unwrap_err()
            .to_string()
            .contains("metadata conflicts")
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn producer_after_completed_transaction_is_not_blocked_by_a_forked_child() {
    use cayw_web_access::ContentSink;
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::Command,
        sync::{Barrier, mpsc},
    };

    unsafe extern "C" {
        fn read(fd: i32, buffer: *mut u8, count: usize) -> isize;
        fn write(fd: i32, buffer: *const u8, count: usize) -> isize;
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let store = Arc::new(
        cayw_content::ContentStore::open(
            root.join("content"),
            cayw_content::Id::new("fork-device").unwrap(),
            ContentLimits::default(),
        )
        .unwrap(),
    );
    let (held_tx, held_rx) = mpsc::sync_channel(1);
    let release_transaction = Arc::new(Barrier::new(2));
    let checkpoint_release = release_transaction.clone();
    cayw_ai::faults::install(Arc::new(move |point, record| {
        if point == "content_state_synced" && record["name"] == "producer-fork-lock-probe" {
            held_tx.send(()).unwrap();
            checkpoint_release.wait();
        }
    }))
    .unwrap();
    let writer_store = store.clone();
    let writer = std::thread::spawn(move || {
        writer_store.try_put(
            &cayw_content::Id::new("producer-fork-lock-probe").unwrap(),
            cayw_content::ExpectedRevision::Absent,
            b"committed before the producer",
        )
    });
    held_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    let (mut child_ready, signal_ready) = std::os::unix::net::UnixStream::pair().unwrap();
    let (mut release_child, await_release) = std::os::unix::net::UnixStream::pair().unwrap();
    child_ready
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let child = std::thread::spawn(move || {
        let ready_fd = signal_ready.as_raw_fd();
        let release_fd = await_release.as_raw_fd();
        let mut command = Command::new("/usr/bin/true");
        // The child deliberately pauses after fork and before exec, when another
        // thread's CLOEXEC transaction descriptor is still inherited. Only
        // async-signal-safe reads and writes run in this child callback.
        unsafe {
            command.pre_exec(move || {
                if write(ready_fd, b"R".as_ptr(), 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut byte = 0;
                if read(release_fd, &mut byte, 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().unwrap().wait().unwrap()
    });
    let mut ready = [0];
    child_ready.read_exact(&mut ready).unwrap();
    assert_eq!(ready, *b"R");
    let web = cayw_host::ContentWebSink::new(store.clone(), "producer-session");
    let busy = web
        .put("while-transaction", b"not committed", Some("text/plain"))
        .unwrap_err();
    assert!(busy.to_string().contains("busy"));
    assert!(!busy.outcome_unknown);
    release_transaction.wait();
    writer.join().unwrap().unwrap();

    let result = web.put("after-transaction", b"new body", Some("text/plain"));
    release_child.write_all(b"X").unwrap();
    assert!(child.join().unwrap().success());
    assert_eq!(result.unwrap()["stored"], true);
}

#[tokio::test]
async fn vision_handle_input_reads_decoded_pixels_once() {
    use cayw_vision::ImageSink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let store = Arc::new(
        cayw_content::ContentStore::open(
            root.join("content"),
            cayw_content::Id::new("test-device").unwrap(),
            ContentLimits {
                max_content_bytes: 128 * 1024,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let sink = Arc::new(cayw_host::ContentVisionSink::new(store, "image-session"));
    // The raw pixels are themselves valid base64 text. Treating sink.get's
    // decoded bytes as another encoded layer would corrupt the image length.
    let raw = b"AAAAAAAAAAAAAAAA";
    let image = cayw_vision::ImageBuffer::from_rgba8(2, 2, raw.to_vec()).unwrap();
    let view = sink
        .put(image.sha256(), raw, Some("image/x-rgba8"))
        .unwrap();
    let service = cayw_vision::VisionService::with_router(
        cayw_vision::VisionRouter::new(
            Arc::new(cayw_vision::FixtureOcrEngine::new()),
            Arc::new(cayw_vision::CountingDescribeBackend::new()),
        ),
        VisionLimits::default(),
    )
    .unwrap()
    .with_sink(sink.clone());
    let crop = service.execute_value("vision.crop", &json!({"image":{"width":2,"height":2,"handle":view["handle"]},"x":0,"y":0,"width":2,"height":2}), &CancellationToken::new()).unwrap();
    assert_eq!(crop["image_sha256"], image.sha256());
    assert_eq!(
        sink.get(crop["handle"].as_str().unwrap()).unwrap().unwrap(),
        raw
    );
}

#[tokio::test]
async fn released_web_and_vision_results_are_not_revived_by_reproduction() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let (b64_img, expected) = sample_image_8x8();
    let crop_args = json!({"image":{"width":8,"height":8,"rgba_base64":b64_img},"x":0,"y":0,"width":4,"height":4});
    let permissions = vec![
        ScopedPermission::exact("web.fetch", json!({"url":"https://example.com/"})),
        ScopedPermission::exact("vision.crop", crop_args),
    ];
    let script = format!(
        r#"
        local web1 = cayw.call('web.fetch', {{url='https://example.com/'}})
        local image1 = cayw.call('vision.crop', {{image={{width=8,height=8,rgba_base64='{b64_img}'}},x=0,y=0,width=4,height=4}})
        cayw.call('result.release', {{handle=web1.handle}})
        cayw.call('result.release', {{handle=image1.handle}})
        local web2 = cayw.call('web.fetch', {{url='https://example.com/'}})
        local image2 = cayw.call('vision.crop', {{image={{width=8,height=8,rgba_base64='{b64_img}'}},x=0,y=0,width=4,height=4}})
        assert(web1.handle ~= web2.handle)
        assert(image1.handle ~= image2.handle)
        cayw.call('result.get', {{handle=web2.handle,limit=2048}})
        return cayw.call('result.get', {{handle=image2.handle,limit=2048}})
    "#
    );
    let rt = open_runtime(&root, &script, test_config(&root, permissions));
    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "release and reproduce", CancellationToken::new())
        .await
        .unwrap();
    let web = completed_results(&rt, &session, "web.fetch");
    let images = completed_results(&rt, &session, "vision.crop");
    assert_eq!(web.len(), 2);
    assert_eq!(images.len(), 2);
    for results in [&web, &images] {
        assert_ne!(results[0]["handle"], results[1]["handle"]);
        assert!(
            rt.result_inspect(&session, None, results[0]["handle"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .released
        );
        assert!(
            !rt.result_inspect(&session, None, results[1]["handle"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .released
        );
        assert_eq!(results[0]["content_hash"], results[1]["content_hash"]);
    }
    let pages = completed_results(&rt, &session, "result.get");
    assert_eq!(pages.len(), 2);
    assert!(
        pages[0]["text"]
            .as_str()
            .unwrap()
            .contains("Example Domain")
    );
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(pages[1]["text"].as_str().unwrap())
            .unwrap(),
        expected
    );
}

struct AllowAllGate;
#[async_trait::async_trait]
impl PermissionGate for AllowAllGate {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), cayw_codemode::CapabilityError> {
        Ok(())
    }
}

#[derive(Default)]
struct DummyAudit;
#[async_trait::async_trait]
impl AuditSink for DummyAudit {
    async fn started(
        &self,
        capability: &str,
        _: &Value,
    ) -> Result<String, cayw_codemode::CapabilityError> {
        Ok(format!("audit-{capability}"))
    }
    async fn settled(
        &self,
        _: &str,
        _: &Result<Value, cayw_codemode::CapabilityError>,
    ) -> Result<(), cayw_codemode::CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &str) -> Result<(), cayw_codemode::CapabilityError> {
        Ok(())
    }
}

#[tokio::test]
async fn standalone_host_without_content_store_preserves_sdk_behavior() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().join("workspace");
    std::fs::create_dir(&ws).unwrap();

    let config = HostConfig {
        web_access: Some(WebAccessOptions::fixture()),
        vision: Some(VisionLimits::default()),
        content_store: None,
        ..Default::default()
    };

    let host = Host::new(&ws, Arc::new(AllowAllGate), Arc::new(DummyAudit), config).unwrap();

    // 1. web.fetch without ContentStore returns FetchResult with webc-N citation
    let fetch_val = host
        .call(
            "web.fetch",
            json!({"url": "https://example.com/"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(fetch_val["status"], 200);
    let cid = fetch_val["content"]["id"].as_str().unwrap();
    assert!(
        cid.starts_with("webc-"),
        "standalone fetch without sink returns internal webc-N citation id, got: {cid}"
    );

    // 2. vision.crop without ContentStore returns {width, height, image_sha256, rgba_base64}
    let (b64_img, _) = sample_image_8x8();
    let crop_val = host
        .call(
            "vision.crop",
            json!({
                "image": {
                    "width": 8,
                    "height": 8,
                    "rgba_base64": b64_img
                },
                "x": 0, "y": 0, "width": 4, "height": 4
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(crop_val["width"], 4);
    assert_eq!(crop_val["height"], 4);
    assert!(crop_val["rgba_base64"].is_string());
    assert!(crop_val["image_sha256"].is_string());
}
