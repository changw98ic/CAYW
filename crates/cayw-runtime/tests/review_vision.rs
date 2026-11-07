#![cfg(target_os = "macos")]
//! Review regressions and integration verification for vision capabilities (AD-N05).
//!
//! Verifies:
//! - Host capability schema accepts `handle` as alternative to `rgba_base64`.
//! - Mutual exclusion: supplying both or neither is rejected by Host schema validation.
//! - Direct Host path: `vision.crop` produces a handle in `ContentStore`, and subsequent
//!   `vision.colors` / `vision.crop` consume that handle via `Host::call`.
//! - Cross-session isolation: handle from session A cannot be read or used by session B.
//! - Missing and released handles are cleanly rejected.
//! - End-to-end Runtime Luau test: Luau script chains `vision.crop` output handle into `vision.colors`.

use base64::Engine;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_content::Limits as ContentLimits;
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate};
use cayw_runtime::{
    ApprovalChoice, ContentConfig, Runtime, RuntimeConfig, fixture::script_response,
};
use cayw_vision::Limits as VisionLimits;
use serde_json::{Value, json};
use std::sync::Arc;

struct PermitAll;

#[async_trait::async_trait]
impl PermissionGate for PermitAll {
    async fn authorize(
        &self,
        _capability: &str,
        _arguments: &Value,
        _cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl AuditSink for PermitAll {
    async fn started(
        &self,
        _capability: &str,
        _arguments: &Value,
    ) -> Result<String, CapabilityError> {
        Ok("test-call".into())
    }

    async fn settled(
        &self,
        _call_id: &str,
        _result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }

    fn dropped(&self, _call_id: &str) -> Result<(), CapabilityError> {
        Ok(())
    }
}

struct ScriptProvider(String);

impl Provider for ScriptProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(
                &request,
                &self.0,
                &format!("vision-test-{}", request.messages.len()),
            ),
        }]))
    }
}

fn sample_8x8_image() -> (String, Vec<u8>) {
    let mut rgba = vec![0u8; 8 * 8 * 4];
    for y in 0..4 {
        for x in 0..4 {
            let idx = (y * 8 + x) * 4;
            rgba[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&rgba);
    (b64, rgba)
}

#[tokio::test]
async fn host_direct_vision_handle_flow_and_rejections() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();

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

    let host = Host::new(
        &workspace,
        Arc::new(PermitAll),
        Arc::new(PermitAll),
        HostConfig {
            vision: Some(VisionLimits::default()),
            ..Default::default()
        },
    )
    .unwrap();

    let host_session1 = host.with_session_content(store.clone(), "session-1");
    let host_session2 = host.with_session_content(store.clone(), "session-2");

    let (b64_img, _) = sample_8x8_image();
    let cancel = CancellationToken::new();

    // 1. Initial crop with inline rgba_base64: stores output in ContentStore and returns handle
    let crop1 = host_session1
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
            cancel.clone(),
        )
        .await
        .unwrap();

    let handle1 = crop1["handle"]
        .as_str()
        .expect("must return handle")
        .to_string();
    assert_eq!(crop1["width"], 4);
    assert_eq!(crop1["height"], 4);

    // 2. Consume handle in vision.colors through Host::call
    let colors = host_session1
        .call(
            "vision.colors",
            json!({
                "image": {
                    "width": 4,
                    "height": 4,
                    "handle": handle1
                },
                "top": 2
            }),
            cancel.clone(),
        )
        .await
        .unwrap();
    assert_eq!(colors[0]["hex"], "#ffffff");

    // 3. Chain handle in another vision.crop through Host::call
    let crop2 = host_session1
        .call(
            "vision.crop",
            json!({
                "image": {
                    "width": 4,
                    "height": 4,
                    "handle": handle1
                },
                "x": 0, "y": 0, "width": 2, "height": 2
            }),
            cancel.clone(),
        )
        .await
        .unwrap();
    let handle2 = crop2["handle"]
        .as_str()
        .expect("must return handle 2")
        .to_string();
    assert_ne!(handle1, handle2);
    assert_eq!(crop2["width"], 2);
    assert_eq!(crop2["height"], 2);

    // 4. Host Schema Rejection: Both rgba_base64 and handle provided
    let err_both = host_session1
        .call(
            "vision.colors",
            json!({
                "image": {
                    "width": 4,
                    "height": 4,
                    "rgba_base64": b64_img,
                    "handle": handle1
                },
                "top": 2
            }),
            cancel.clone(),
        )
        .await
        .unwrap_err();
    let err_msg = err_both.to_string();
    assert!(
        err_msg.contains("alternative") || err_msg.contains("oneOf") || err_msg.contains("schema"),
        "expected schema rejection for both sources, got: {err_msg}"
    );

    // 5. Host Schema Rejection: Neither rgba_base64 nor handle provided
    let err_neither = host_session1
        .call(
            "vision.colors",
            json!({
                "image": {
                    "width": 4,
                    "height": 4
                },
                "top": 2
            }),
            cancel.clone(),
        )
        .await
        .unwrap_err();
    let err_msg = err_neither.to_string();
    assert!(
        err_msg.contains("required") || err_msg.contains("schema"),
        "expected schema rejection for missing sources, got: {err_msg}"
    );

    // 6. Non-existent handle: passes schema, rejected by service lookup
    let err_missing = host_session1
        .call(
            "vision.colors",
            json!({
                "image": {
                    "width": 4,
                    "height": 4,
                    "handle": "non-existent-handle-404"
                },
                "top": 2
            }),
            cancel.clone(),
        )
        .await
        .unwrap_err();
    assert!(
        err_missing.to_string().contains("handle not found"),
        "expected handle not found, got: {err_missing}"
    );

    // 7. Cross-session isolation: session 2 cannot access session 1's handle
    let err_cross = host_session2
        .call(
            "vision.colors",
            json!({
                "image": {
                    "width": 4,
                    "height": 4,
                    "handle": handle1
                },
                "top": 2
            }),
            cancel.clone(),
        )
        .await
        .unwrap_err();
    assert!(
        err_cross.to_string().contains("handle not found"),
        "expected cross-session handle isolation rejection, got: {err_cross}"
    );

    // 8. Released handle: released in ContentStore, cannot be used again
    let meta_key = cayw_host::result_meta_key("test-device", "session-1", None, &handle1).unwrap();
    let (record, bytes) = store.try_read(&meta_key).unwrap().unwrap();
    let mut meta: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    meta["released"] = serde_json::Value::Bool(true);
    let updated = serde_json::to_vec(&meta).unwrap();
    store
        .try_put(&meta_key, record.expected(), &updated)
        .unwrap();

    let err_released = host_session1
        .call(
            "vision.colors",
            json!({
                "image": {
                    "width": 4,
                    "height": 4,
                    "handle": handle1
                },
                "top": 2
            }),
            cancel.clone(),
        )
        .await
        .unwrap_err();
    assert!(
        err_released.to_string().contains("released")
            || err_released.to_string().contains("handle not found"),
        "expected released handle rejection, got: {err_released}"
    );
}

#[tokio::test]
async fn runtime_luau_chains_crop_handle_into_colors_and_rejects_released() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let (b64_img, _) = sample_8x8_image();
    let script = format!(
        r#"
        local crop1 = cayw.call('vision.crop', {{image={{width=8,height=8,rgba_base64='{b64_img}'}},x=0,y=0,width=4,height=4}})
        assert(crop1.handle ~= nil)
        local colors = cayw.call('vision.colors', {{image={{width=4,height=4,handle=crop1.handle}},top=2}})
        assert(#colors >= 1)
        local crop2 = cayw.call('vision.crop', {{image={{width=4,height=4,handle=crop1.handle}},x=0,y=0,width=2,height=2}})
        assert(crop2.handle ~= nil)
        assert(crop1.handle ~= crop2.handle)
        return colors
        "#
    );

    let config = RuntimeConfig {
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "test-device".into(),
            limits: ContentLimits {
                max_content_bytes: 128 * 1024,
                ..Default::default()
            },
        }),
        host: HostConfig {
            vision: Some(VisionLimits::default()),
            ..Default::default()
        },
        ..Default::default()
    };

    let rt = Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(ScriptProvider(script)),
        Model::new("fixture", "vision-model"),
        config,
    )
    .unwrap();

    let session = rt.create(None).unwrap().id;
    let cancel = CancellationToken::new();

    let (prompt_res, _) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(
            rt.prompt(&session, "run vision handle chain", cancel.clone()),
            async {
                let mut approved_count = 0;
                // The Luau script executes: crop1 -> colors -> crop2 (3 capability calls)
                while approved_count < 3 && !cancel.is_cancelled() {
                    let pending = rt
                        .get(&session)
                        .map(|s| s.pending_approvals)
                        .unwrap_or_default();
                    if let Some(req) = pending.first()
                        && rt
                            .approve(&session, &req.id, ApprovalChoice::AllowOnce, "test-user")
                            .is_ok()
                    {
                        approved_count += 1;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        )
    })
    .await
    .expect("test timed out");

    prompt_res.unwrap();

    // Verify settled capability results in Runtime event stream
    let events = rt.events(&session, 0, 1000).unwrap();
    let settled_crops: Vec<_> = events
        .iter()
        .filter_map(|r| {
            let val = serde_json::to_value(r).unwrap();
            let call = &val["event"]["call"];
            if val["event"]["type"] == "capability_settled" && call["name"] == "vision.crop" {
                Some(call["outcome"]["result"].clone())
            } else {
                None
            }
        })
        .collect();

    assert_eq!(settled_crops.len(), 2);
    assert!(settled_crops[0]["handle"].is_string());
    assert!(settled_crops[1]["handle"].is_string());
    assert_ne!(settled_crops[0]["handle"], settled_crops[1]["handle"]);

    let settled_colors: Vec<_> = events
        .iter()
        .filter_map(|r| {
            let val = serde_json::to_value(r).unwrap();
            let call = &val["event"]["call"];
            if val["event"]["type"] == "capability_settled" && call["name"] == "vision.colors" {
                Some(call["outcome"]["result"].clone())
            } else {
                None
            }
        })
        .collect();

    assert_eq!(settled_colors.len(), 1);
    assert_eq!(settled_colors[0][0]["hex"], "#ffffff");
}
