//! Regression tests for runtime services review items:
//! - RT-08: PluginService middle layer redundant schema compilation and validation removed.
//! - RT-M01: ContentService put expected revision validated once at entry and reused across retries.

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_content::{Error as ContentError, Limits};
use cayw_core::SessionState;
use cayw_runtime::{
    ContentConfig, ContentScope, Runtime, RuntimeConfig, RuntimeError, ScopedPermission,
    fixture::script_response,
};
use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(
                &request,
                &self.0,
                &format!("services-{}", request.messages.len()),
            ),
        }]))
    }
}

fn content_rt(root: &Path, script: &str, permissions: Vec<ScopedPermission>) -> Runtime {
    let config = RuntimeConfig {
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "test-device".into(),
            limits: Limits {
                max_content_bytes: 16384,
                ..Default::default()
            },
        }),
        permissions,
        ..Default::default()
    };
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "services"),
        config,
    )
    .unwrap()
}

#[tokio::test]
async fn content_put_validates_revision_once_at_entry_and_rejects_invalid_combination() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let hex64 = "1".repeat(64);
    let args =
        json!({"name":"notes","text":"entry","expected_version":0,"expected_revision":hex64});
    let cfg_perms = vec![ScopedPermission::exact("content.put", args)];
    let script = format!(
        "return cayw.call('content.put', {{name='notes',text='entry',expected_version=0,expected_revision='{}'}})",
        hex64
    );
    let rt = content_rt(&root, &script, cfg_perms);
    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "bad revision at entry", CancellationToken::new())
        .await
        .unwrap();

    assert!(
        rt.content_inspect(&session, None, "notes")
            .unwrap()
            .is_none()
    );
    let failed = rt
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .any(|record| {
            let val = serde_json::to_value(record).unwrap();
            let call = &val["event"]["call"];
            val["event"]["type"] == "capability_settled"
                && call["name"] == "content.put"
                && call["outcome"]["type"] == "failed"
                && call["outcome"]["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("creation requires version 0 without a revision")
        });
    assert!(failed, "expected settled failed capability");
}

#[tokio::test]
async fn content_put_normal_create_and_update_retains_expected_revision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let create_args = json!({"name":"entry_doc","text":"v1","expected_version":0});
    let cfg_perms = vec![ScopedPermission::exact("content.put", create_args)];
    let rt = content_rt(
        &root,
        "return cayw.call('content.put', {name='entry_doc',text='v1',expected_version=0})",
        cfg_perms,
    );
    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "create doc", CancellationToken::new())
        .await
        .unwrap();

    let doc1 = rt
        .content_inspect(&session, None, "entry_doc")
        .unwrap()
        .expect("doc created");
    assert_eq!(doc1.text, "v1");
    assert_eq!(doc1.record.version, 1);
    assert_eq!(doc1.scope, ContentScope::Session);
    drop(rt);

    let update_args = json!({
        "name": "entry_doc",
        "text": "v2",
        "expected_version": 1,
        "expected_revision": doc1.revision_hex
    });
    let updated_perms = vec![ScopedPermission::exact("content.put", update_args)];
    let update_script = format!(
        "return cayw.call('content.put', {{name='entry_doc',text='v2',expected_version=1,expected_revision='{}'}})",
        doc1.revision_hex
    );
    let rt = content_rt(&root, &update_script, updated_perms);
    rt.prompt(&session, "update doc", CancellationToken::new())
        .await
        .unwrap();

    let doc2 = rt
        .content_inspect(&session, None, "entry_doc")
        .unwrap()
        .expect("doc updated");
    assert_eq!(doc2.text, "v2");
    assert_eq!(doc2.record.version, 2);
    assert_eq!(doc2.record.parent_revision, Some(doc1.record.revision));
}

#[cfg(unix)]
struct HeldLock {
    file: std::fs::File,
}

#[cfg(unix)]
impl HeldLock {
    fn acquire(path: &Path) -> Self {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open content .lock");
        file.lock().expect("file lock must succeed");
        Self { file }
    }
}

#[cfg(unix)]
impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = std::fs::File::unlock(&self.file);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn content_put_busy_retry_reuses_expected_revision_and_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let args = json!({"name":"busy_doc","text":"delayed_success","expected_version":0});
    let cfg_perms = vec![ScopedPermission::exact("content.put", args)];
    let rt = content_rt(
        &root,
        "return cayw.call('content.put', {name='busy_doc',text='delayed_success',expected_version=0})",
        cfg_perms,
    );
    let session = rt.create(None).unwrap().id;

    let lock_path = root.join("content").join(".lock");
    let lock = HeldLock::acquire(&lock_path);

    let mut prompt_fut = Box::pin(rt.prompt(&session, "retry put", CancellationToken::new()));

    // Synchronize deterministically: poll the production Future while the lock is held,
    // verifying that the actual Host/Runtime write path observes Pending and reaches
    // capability_started before releasing the lock.
    let watchdog = tokio::time::Instant::now();
    loop {
        assert!(
            futures::poll!(&mut prompt_fut).is_pending(),
            "content.put future must stay Pending while lock is held"
        );
        let events = rt.events(&session, 0, 1000).unwrap();
        let started = events.iter().any(|record| {
            let val = serde_json::to_value(record).unwrap();
            let event = &val["event"];
            event["type"] == "capability_started" && event["call"]["name"] == "content.put"
        });
        if started {
            break;
        }
        assert!(
            watchdog.elapsed() < Duration::from_secs(10),
            "watchdog expired waiting for capability_started"
        );
        tokio::task::yield_now().await;
    }

    // Verify while lock is still held: content.put is actively in busy retry and Pending,
    // has not settled, and doc is not yet committed.
    assert!(
        futures::poll!(&mut prompt_fut).is_pending(),
        "prompt future must be Pending during retry loop while lock is held"
    );
    assert!(
        matches!(
            rt.content_inspect(&session, None, "busy_doc"),
            Err(RuntimeError::Content(ContentError::Busy))
        ),
        "content_inspect must observe busy while exclusive lock is held"
    );
    let settled_early = rt
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .any(|record| {
            let val = serde_json::to_value(record).unwrap();
            val["event"]["type"] == "capability_settled"
                && val["event"]["call"]["name"] == "content.put"
        });
    assert!(
        !settled_early,
        "content.put must not settle while lock is held"
    );

    // Release the lock to allow retry to succeed.
    drop(lock);

    // Await completion with a generous watchdog timeout.
    tokio::time::timeout(Duration::from_secs(10), prompt_fut)
        .await
        .expect("watchdog expired waiting for prompt to succeed")
        .unwrap();

    let doc = rt
        .content_inspect(&session, None, "busy_doc")
        .unwrap()
        .expect("document must be created after acquiring lock on retry");
    assert_eq!(doc.text, "delayed_success");
    assert_eq!(doc.record.version, 1);
    assert_eq!(doc.scope, ContentScope::Session);

    let completed = rt
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .any(|record| {
            let val = serde_json::to_value(record).unwrap();
            let call = &val["event"]["call"];
            val["event"]["type"] == "capability_settled"
                && call["name"] == "content.put"
                && call["outcome"]["type"] == "completed"
        });
    assert!(
        completed,
        "content.put must settle as completed after busy retries"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn content_put_busy_retry_cancelled_preserves_unknown_and_blocks_continuation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let args = json!({"name":"cancel_doc","text":"never_committed","expected_version":0});
    let cfg_perms = vec![ScopedPermission::exact("content.put", args)];
    let rt = content_rt(
        &root,
        "return cayw.call('content.put', {name='cancel_doc',text='never_committed',expected_version=0})",
        cfg_perms,
    );
    let session = rt.create(None).unwrap().id;

    let lock_path = root.join("content").join(".lock");
    let lock = HeldLock::acquire(&lock_path);
    let cancel = CancellationToken::new();

    let mut prompt_fut = Box::pin(rt.prompt(&session, "cancel while busy", cancel.clone()));

    // Synchronize deterministically: poll the production Future while the lock is held,
    // verifying that the actual Host/Runtime write path observes Pending and reaches
    // capability_started before triggering cancellation.
    let watchdog = tokio::time::Instant::now();
    loop {
        assert!(
            futures::poll!(&mut prompt_fut).is_pending(),
            "content.put future must stay Pending while lock is held"
        );
        let events = rt.events(&session, 0, 1000).unwrap();
        let started = events.iter().any(|record| {
            let val = serde_json::to_value(record).unwrap();
            let event = &val["event"];
            event["type"] == "capability_started" && event["call"]["name"] == "content.put"
        });
        if started {
            break;
        }
        assert!(
            watchdog.elapsed() < Duration::from_secs(10),
            "watchdog expired waiting for capability_started"
        );
        tokio::task::yield_now().await;
    }

    // Verify while lock is held: content.put is actively in busy retry and Pending,
    // has not settled, and doc is not committed.
    assert!(
        futures::poll!(&mut prompt_fut).is_pending(),
        "prompt future must be Pending during retry loop while lock is held"
    );
    assert!(
        matches!(
            rt.content_inspect(&session, None, "cancel_doc"),
            Err(RuntimeError::Content(ContentError::Busy))
        ),
        "content_inspect must observe busy while exclusive lock is held"
    );

    // Trigger cancellation specifically while content.put is blocked in busy retry.
    cancel.cancel();

    // Await completion with a generous watchdog timeout.
    tokio::time::timeout(Duration::from_secs(10), prompt_fut)
        .await
        .expect("watchdog expired waiting for prompt to settle on cancellation")
        .unwrap();

    drop(lock);

    // 1. Verify content was never committed while locked.
    assert!(
        rt.content_inspect(&session, None, "cancel_doc")
            .unwrap()
            .is_none()
    );

    // 2. Global prompt cancellation while blocked in busy retry crosses the execution
    // boundary, so Host Executor::External and Cells VM shutdown strictly pair to
    // preserve `outcome_unknown`, never unsafely releasing unknown into a regular failed outcome.
    let events = rt.events(&session, 0, 1000).unwrap();
    let settled_unknown = events.iter().any(|record| {
        let val = serde_json::to_value(record).unwrap();
        let call = &val["event"]["call"];
        let err = call["outcome"]["error"].as_str().unwrap_or_default();
        val["event"]["type"] == "capability_settled"
            && call["name"] == "content.put"
            && call["outcome"]["type"] == "outcome_unknown"
            && (err.contains("external capability cancelled after admission")
                || err.contains("capability stopped after its execution boundary"))
    });

    if !settled_unknown {
        let capability_events: Vec<_> = events
            .iter()
            .filter_map(|r| {
                let val = serde_json::to_value(r).ok()?;
                if val["event"]["call"]["name"] == "content.put" {
                    Some(val)
                } else {
                    None
                }
            })
            .collect();
        eprintln!(
            "content.put capability events on cancellation: {:#?}",
            capability_events
        );
    }
    assert!(
        settled_unknown,
        "content.put cancelled after boundary must settle as outcome_unknown; events: {:#?}",
        events
    );

    // 3. Verify Host and Cells terminal state pairing: session strictly enters NeedsReconciliation
    let view = rt.get(&session).unwrap();
    assert_eq!(
        view.state,
        SessionState::NeedsReconciliation,
        "session must enter NeedsReconciliation when mutating capability is cancelled after admission"
    );
    assert!(matches!(
        rt.enqueue(&session, "must remain blocked"),
        Err(RuntimeError::NeedsReconciliation)
    ));
}

#[cfg(feature = "plugins")]
mod plugins_review {
    use super::*;
    use async_trait::async_trait;
    use cayw_ai::ToolDefinition;
    use cayw_plugins::{Plugin, PluginContext, PluginError, PluginKind, PluginManifest};
    use serde_json::Value;
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct EchoPlugin {
        manifest: PluginManifest,
        calls: AtomicUsize,
    }

    impl EchoPlugin {
        fn new() -> Self {
            Self {
                manifest: PluginManifest {
                    name: "echo_plugin".into(),
                    version: "1.0.0".into(),
                    protocol_version: 1,
                    kind: PluginKind::Native,
                    capabilities: vec![ToolDefinition {
                        name: "echo_plugin.echo".into(),
                        description: "Echo service for review test".into(),
                        parameters: json!({
                            "type": "object",
                            "properties": {
                                "message": {"type": "string"}
                            },
                            "required": ["message"],
                            "additionalProperties": false,
                        }),
                    }],
                    permissions: BTreeMap::new(),
                },
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Plugin for EchoPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }
        async fn call(
            &self,
            method: &str,
            arguments: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, PluginError> {
            if method == "echo_plugin.echo" {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let msg = arguments
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(json!({"echo": msg}))
            } else {
                Err(PluginError::NotFound(method.into()))
            }
        }
    }

    #[tokio::test]
    async fn plugin_capability_invoked_via_host_validation_without_middle_validator() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("workspace")).unwrap();

        // Host and the independently authorized Registry both validate this call.
        let valid_args = json!({"message":"hello-plugin"});
        let config = RuntimeConfig {
            permissions: vec![ScopedPermission::exact("echo_plugin.echo", valid_args)],
            ..Default::default()
        };
        let rt = Runtime::open(
            root.join("sessions.sqlite"),
            root.join("workspace"),
            Arc::new(Script(
                "return cayw.call('echo_plugin.echo', {message='hello-plugin'})".into(),
            )),
            Model::new("fixture", "plugin-review"),
            config,
        )
        .unwrap();

        let plugin = Arc::new(EchoPlugin::new());
        rt.load_native(
            plugin.clone(),
            PluginContext::new(["echo_plugin.echo".into()], []),
        )
        .unwrap();
        let session = rt.create(None).unwrap().id;
        rt.bind_plugin(&session, "echo_plugin").unwrap();

        rt.prompt(&session, "call echo", CancellationToken::new())
            .await
            .unwrap();

        let events: Vec<_> = rt
            .events(&session, 0, 1000)
            .unwrap()
            .into_iter()
            .map(|record| serde_json::to_value(record).unwrap())
            .collect();
        let completed: Vec<_> = events
            .iter()
            .filter(|record| {
                let call = &record["event"]["call"];
                record["event"]["type"] == "capability_settled"
                    && call["name"] == "echo_plugin.echo"
            })
            .collect();
        assert_eq!(completed.len(), 1, "plugin call must settle once");
        let call = &completed[0]["event"]["call"];
        assert_eq!(call["outcome"]["type"], "completed");
        assert_eq!(call["outcome"]["result"]["echo"], "hello-plugin");
        assert!(!call["key"].is_null());
        assert_eq!(
            events
                .iter()
                .filter(|record| {
                    let started = &record["event"]["call"];
                    record["event"]["type"] == "capability_started"
                        && started["name"] == "echo_plugin.echo"
                        && started["key"] == call["key"]
                })
                .count(),
            1,
            "completed plugin call must match its original Host start"
        );
        assert_eq!(plugin.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn plugin_capability_rejects_invalid_schema_before_host_admission() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("workspace")).unwrap();

        // CodeMode rejects this wrong type before planning a Host call.
        let invalid_args = json!({"message": 42});
        let config = RuntimeConfig {
            permissions: vec![ScopedPermission::exact("echo_plugin.echo", invalid_args)],
            ..Default::default()
        };
        let rt = Runtime::open(
            root.join("sessions.sqlite"),
            root.join("workspace"),
            Arc::new(Script(
                "return cayw.call('echo_plugin.echo', {message=42})".into(),
            )),
            Model::new("fixture", "plugin-review"),
            config,
        )
        .unwrap();

        let plugin = Arc::new(EchoPlugin::new());
        rt.load_native(
            plugin.clone(),
            PluginContext::new(["echo_plugin.echo".into()], []),
        )
        .unwrap();
        let session = rt.create(None).unwrap().id;
        rt.bind_plugin(&session, "echo_plugin").unwrap();

        let view = rt
            .prompt(&session, "call echo bad", CancellationToken::new())
            .await
            .unwrap();

        let rejection = view
            .messages
            .iter()
            .find_map(|message| match message {
                cayw_ai::Message::ToolResult(result) if result.is_error => Some(result),
                _ => None,
            })
            .expect("CodeMode must report the invalid argument");
        assert!(!rejection.outcome_unknown);
        let error = cayw_ai::content_text(&rejection.content);
        assert!(
            error.contains("/message") && error.contains("string"),
            "{error}"
        );
        assert!(
            rt.events(&session, 0, 1000)
                .unwrap()
                .into_iter()
                .all(|record| {
                    let val = serde_json::to_value(record).unwrap();
                    val["event"]["call"]["name"] != "echo_plugin.echo"
                }),
            "invalid VM arguments must not plan, start or settle a plugin call"
        );
        assert_eq!(plugin.calls.load(Ordering::SeqCst), 0);
    }
}
