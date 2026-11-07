use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{
    AccessRequest, DispatchDropState, DispatchJournal, DispatchOutcome, DispatchRecord,
    InvocationContext, Limits, Provenance, Registry, RegistryPolicy, SourceIdentity, Visibility,
};
use cayw_codemode::CapabilityError;
use cayw_sources::{StdioConfig, StdioControl, StdioSource};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
fn capture(
    label: &str,
    registry: Option<&Registry>,
    control: &StdioControl,
    error: Option<&CapabilityError>,
    config: &StdioConfig,
    details: Value,
) {
    let Some(parent) = std::env::var_os("CAYW_ISSUE9_ARTIFACTS_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(parent).join(format!("stdio-{label}"));
    std::fs::create_dir(&dir).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join("result.json"))
        .unwrap();
    serde_json::to_writer_pretty(&mut file,&json!({
        "fault":label,"source":registry.map(|r|r.snapshot("local")),"transport":control.snapshot(),
        "error":error.map(|e|json!({"message":e.message,"outcome_unknown":e.outcome_unknown})),
        "config":{"program":config.program,"directory":config.directory,"arguments":config.arguments,
            "environment":config.environment,"max_frame_bytes":config.max_frame_bytes,
            "max_pending":config.max_pending,"max_observations":config.max_observations,
            "max_received_frames":config.max_received_frames,"max_stderr_bytes":config.max_stderr_bytes,
            "request_timeout_ms":config.request_timeout.as_millis(),"cleanup_timeout_ms":config.cleanup_timeout.as_millis()},
        "details":details
    })).unwrap();
    std::io::Write::write_all(&mut file, b"\n").unwrap();
    file.sync_all().unwrap();
}
struct Policy;
#[async_trait]
impl RegistryPolicy for Policy {
    fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
        true
    }
    async fn authorize(
        &self,
        _: &AccessRequest,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}
struct Journal;
#[async_trait]
impl DispatchJournal for Journal {
    async fn started(&self, _: &DispatchRecord) -> Result<(), CapabilityError> {
        Ok(())
    }
    async fn settled(
        &self,
        _: &DispatchRecord,
        _: &DispatchOutcome,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &DispatchRecord, _: DispatchDropState) -> Result<(), CapabilityError> {
        Ok(())
    }
}
async fn setup(root: &Path, fault: &str) -> (Registry, StdioControl, StdioConfig) {
    let mut config = StdioConfig::new(env!("CARGO_BIN_EXE_cayw-sources-local-fixture"), root);
    config
        .environment
        .insert("CAYW_MCP_FIXTURE_FAULT".into(), fault.into());
    config.max_frame_bytes = 4096;
    config.max_stderr_bytes = 64;
    config.max_received_frames = 32;
    let prepared = StdioSource::connect(config.clone(), CancellationToken::new())
        .await
        .unwrap();
    let control = prepared.control();
    let (manifest, handle) = prepared.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry
        .register(
            SourceIdentity {
                id: "local".into(),
                version: "fault-test".into(),
                origin: "host:actual-child-negative-control".into(),
            },
            manifest,
            handle,
        )
        .unwrap();
    (registry, control, config)
}
async fn call(registry: &Registry) -> Result<Value, CapabilityError> {
    registry
        .consumer()
        .scoped(
            "mcp.call",
            InvocationContext {
                session_id: "s".into(),
                task_id: "t".into(),
                attempt_id: "a".into(),
                execution_id: "e".into(),
                sequence: 1,
            },
            Arc::new(Journal),
        )
        .unwrap()
        .call(
            "mcp.call",
            json!({"server":"local","tool":"add","arguments":{"a":17,"b":25}}),
            CancellationToken::new(),
        )
        .await
}
async fn close(registry: &Registry, control: &StdioControl) {
    registry.disconnect("local").unwrap();
    tokio::time::timeout(Duration::from_secs(10), control.wait_closed())
        .await
        .unwrap();
    assert!(control.snapshot().reaped);
}
#[tokio::test]
async fn actual_server_rejection_preserves_original_request_and_allows_no_replay() {
    let root = tempfile::tempdir().unwrap();
    let (registry, control, config) = setup(root.path(), "tool-rejected").await;
    let error = call(&registry).await.unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(
        error
            .message
            .contains("MCP error -32602 for original request 4")
    );
    assert_eq!(control.snapshot().tool_calls, 1);
    assert!(!registry.snapshot("local").unwrap().outcome_unknown);
    close(&registry, &control).await;
    capture(
        "tool-rejected",
        Some(&registry),
        &control,
        Some(&error),
        &config,
        json!({}),
    );
}
#[tokio::test]
async fn actual_child_exit_malformed_frames_and_identity_change_retain_unknown() {
    for fault in [
        "tool-exit",
        "oversized-frame",
        "malformed-error",
        "changed-identity",
        "tool-error",
        "server-request",
        "frame-flood",
    ] {
        let root = tempfile::tempdir().unwrap();
        let (registry, control, config) = setup(root.path(), fault).await;
        let error = call(&registry).await.unwrap_err();
        assert!(error.outcome_unknown, "{fault}: {error:?}");
        assert!(registry.snapshot("local").unwrap().outcome_unknown);
        assert_eq!(control.snapshot().tool_calls, 1);
        assert!(control.snapshot().observed_frame_bytes <= 4096);
        close(&registry, &control).await;
        capture(
            fault,
            Some(&registry),
            &control,
            Some(&error),
            &config,
            json!({}),
        );
    }
}
fn direct_request(operation: cayw_bridge::SourceOperation, id: u64) -> cayw_bridge::SourceRequest {
    let (kind, target) = match &operation {
        cayw_bridge::SourceOperation::GetPrompt { name, .. } => {
            (cayw_bridge::Operation::GetPrompt, name.clone())
        }
        cayw_bridge::SourceOperation::ReadResource { uri } => {
            (cayw_bridge::Operation::ReadResource, uri.clone())
        }
        cayw_bridge::SourceOperation::InvokeTool { name, .. } => {
            (cayw_bridge::Operation::Invoke, name.clone())
        }
        _ => unreachable!(),
    };
    cayw_bridge::SourceRequest {
        key: cayw_bridge::RequestKey {
            id,
            source: Provenance {
                registry_incarnation: "direct-fixture".into(),
                source: SourceIdentity {
                    id: "local".into(),
                    version: "2".into(),
                    origin: "host:direct-regression".into(),
                },
                generation: 1,
                manifest_sha256: "0".repeat(64),
            },
            operation: kind,
            target,
        },
        operation,
        max_result_bytes: 4096,
    }
}
#[tokio::test]
async fn readonly_cancel_and_deadline_release_the_original_pending_slot() {
    for stop in ["cancel", "deadline", "drop"] {
        let root = tempfile::tempdir().unwrap();
        let mut config = StdioConfig::new(
            env!("CARGO_BIN_EXE_cayw-sources-local-fixture"),
            root.path(),
        );
        config.max_pending = 1;
        config.request_timeout = Duration::from_millis(300);
        config
            .environment
            .insert("CAYW_MCP_FIXTURE_FAULT".into(), "resource-wait".into());
        let source = StdioSource::connect(config.clone(), CancellationToken::new())
            .await
            .unwrap();
        let control = source.control();
        let (_, handle) = source.into_parts();
        let cancel = CancellationToken::new();
        let mut pending = Box::pin(handle.request(
            direct_request(
                cayw_bridge::SourceOperation::ReadResource {
                    uri: "fixture://local/addition".into(),
                },
                1,
            ),
            cancel.clone(),
        ));
        tokio::select! {
            result=&mut pending=>panic!("read returned before cancellation: {result:?}"),
            _=async{tokio::time::timeout(Duration::from_secs(10),async{while control.snapshot().resource_reads==0{tokio::task::yield_now().await;}}).await.unwrap();}=>{}
        }
        if stop == "cancel" {
            cancel.cancel();
        }
        let error = if stop != "drop" {
            let error = pending.as_mut().await.unwrap_err();
            assert!(!error.outcome_unknown, "{stop}: {error:?}");
            Some(error)
        } else {
            None
        };
        drop(pending);
        let next = handle
            .request(
                direct_request(
                    cayw_bridge::SourceOperation::InvokeTool {
                        name: "add".into(),
                        arguments: json!({"a":17,"b":25}),
                    },
                    2,
                ),
                CancellationToken::new(),
            )
            .await;
        assert!(
            next.is_ok(),
            "{stop}: original pending slot was not released: {next:?}"
        );
        let next = next.unwrap();
        assert_eq!(next.value["sum"], 42);
        tokio::time::timeout(Duration::from_secs(10), async {
            while control.snapshot().ignored_late == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(control.snapshot().cancellations, 1);
        assert_eq!(control.snapshot().resource_reads, 1);
        handle.disconnect();
        tokio::time::timeout(Duration::from_secs(10), control.wait_closed())
            .await
            .unwrap();
        assert!(control.snapshot().reaped);
        capture(
            &format!("readonly-{stop}"),
            None,
            &control,
            error.as_ref(),
            &config,
            json!({"stop":stop,"resource_request":{"key":direct_request(cayw_bridge::SourceOperation::ReadResource{uri:"fixture://local/addition".into()},1).key,"uri":"fixture://local/addition","max_result_bytes":4096},"subsequent_reply":{"key":next.key,"value":next.value}}),
        );
    }
}
#[tokio::test]
async fn stderr_is_bounded_diagnostic_data_and_does_not_fail_successful_tool() {
    let root = tempfile::tempdir().unwrap();
    let (registry, control, config) = setup(root.path(), "stderr").await;
    assert_eq!(call(&registry).await.unwrap()["value"]["sum"], 42);
    tokio::time::timeout(Duration::from_secs(10), async {
        while control.snapshot().stderr_discarded_bytes == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(control.snapshot().stderr.len(), 64);
    assert_eq!(control.snapshot().stderr_discarded_bytes, 4096 - 64);
    assert!(control.snapshot().failure.is_none());
    close(&registry, &control).await;
    capture(
        "stderr",
        Some(&registry),
        &control,
        None,
        &config,
        json!({"expected_sum":42}),
    );
}
