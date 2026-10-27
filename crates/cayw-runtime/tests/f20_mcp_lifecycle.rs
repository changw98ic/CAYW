//! Standalone F20 MCP lifecycle integration test:
//! ManagedStdio lazy source + CodeMode MCP invocation + idle recycle + reopen persistence.

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_bridge::{AccessRequest, Limits, Provenance, Registry, RegistryPolicy, Visibility};
use cayw_codemode::CapabilityError;
use cayw_core::SessionState;
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState, fixture::script_response};
use cayw_sources::{LifecycleMode, ManagedStdio, ServerSpec, StdioConfig};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

struct AllowAll;

#[async_trait]
impl RegistryPolicy for AllowAll {
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

struct ScriptedProvider;

impl Provider for ScriptedProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let last_user_text = request
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content, .. } => Some(cayw_ai::content_text(content)),
                _ => None,
            })
            .unwrap_or_default();

        let (script, call_id) = if last_user_text.contains("first") {
            (
                r#"
local result = cayw.call('mcp.call', {server='local', tool='add', arguments={a=3, b=4}})
return { sum = result.value.sum }
"#,
                "call-first",
            )
        } else {
            (
                r#"
local result = cayw.call('mcp.call', {server='local', tool='add', arguments={a=10, b=20}})
return { sum = result.value.sum }
"#,
                "call-second",
            )
        };
        let message = script_response(&request, script, call_id);
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn extract_cell_value(messages: &[Message]) -> Option<Value> {
    messages.iter().rev().find_map(|message| match message {
        Message::ToolResult(result) => result.structured_content.as_ref().and_then(cell_value),
        _ => None,
    })
}

fn cell_value(page: &Value) -> Option<Value> {
    page.get("outputs")?.as_array()?.iter().find_map(|entry| {
        let output = &entry["output"];
        (output["type"].as_str() == Some("json")).then(|| output["value"].clone())
    })
}

#[tokio::test]
async fn managed_mcp_lifecycle_and_result_reopen() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store_path = root.path().join("sessions.db");

    // 1. Prepare ManagedStdio in Lazy mode with 150ms idle timeout.
    let fixture_exe = env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture");
    let spec = ServerSpec {
        id: "local".into(),
        version: "1.0.0".into(),
        origin: "host:f20-mcp-test".into(),
        config: StdioConfig::new(fixture_exe, workspace.as_path()),
        lifecycle: LifecycleMode::Lazy,
        idle_timeout: Duration::from_millis(150),
    };

    let prepared = ManagedStdio::prepare(spec, CancellationToken::new())
        .await
        .expect("ManagedStdio::prepare failed");
    let control = prepared.control();
    let (manifest, handle) = prepared.into_parts();

    // The manifest probe has exited; no live connection has been established.
    let initial_snap = control.snapshot();
    assert!(
        !initial_snap.connected,
        "initial lazy MCP must not be connected"
    );
    assert_eq!(
        initial_snap.spawn_count, 1,
        "initial probe accounted for 1 spawn"
    );
    assert_eq!(
        initial_snap.connect_generation, 0,
        "initial connect generation must be 0"
    );

    // 2. Register source handle into live Bridge Registry.
    let registry = Registry::new(Limits::default(), Arc::new(AllowAll)).unwrap();
    registry
        .register(
            cayw_bridge::SourceIdentity {
                id: "local".into(),
                version: "1.0.0".into(),
                origin: "host:f20-mcp-test".into(),
            },
            manifest,
            handle,
        )
        .expect("register into bridge");

    let config = RuntimeConfig {
        bridge: Some(registry.consumer()),
        permissions: vec![
            ScopedPermission::exact(
                "mcp.call",
                json!({"server":"local","tool":"add","arguments":{"a":3,"b":4}}),
            ),
            ScopedPermission::exact(
                "mcp.call",
                json!({"server":"local","tool":"add","arguments":{"a":10,"b":20}}),
            ),
        ],
        max_run_time: Duration::from_secs(30),
        ..Default::default()
    };

    // 3. Open Runtime and create a session.
    let runtime = Runtime::open(
        &store_path,
        workspace.as_path(),
        Arc::new(ScriptedProvider),
        Model::new("fixture", "f20-lifecycle-fixture"),
        config.clone(),
    )
    .expect("open runtime");

    let session = runtime
        .create(Some("F20 Managed MCP Session".into()))
        .unwrap();

    // 4. First call: starts child process on-demand.
    let view1 = runtime
        .prompt(
            &session.id,
            "first call to lazy MCP",
            CancellationToken::new(),
        )
        .await
        .expect("first prompt failed");

    assert_eq!(view1.state, SessionState::Idle);
    let val1 = extract_cell_value(&view1.messages).expect("missing first cell value");
    assert_eq!(val1["sum"], json!(7));

    let snap_after_first = control.snapshot();
    assert_eq!(
        snap_after_first.connect_generation, 1,
        "first call generation must be 1"
    );
    assert!(
        snap_after_first.spawn_count >= 2,
        "first call spawned live child"
    );

    // 5. Observe actual idle recycling within a generous deadline.
    let snap_recycled = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = control.snapshot();
            if !snapshot.connected && snapshot.recycle_count >= 1 {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("MCP idle recycle deadline");
    assert!(
        !snap_recycled.connected,
        "idle recycle must stop child process"
    );
    assert!(
        snap_recycled.recycle_count >= 1,
        "recycle count must be incremented"
    );
    assert_eq!(
        snap_recycled.connect_generation, 1,
        "generation stays 1 while stopped"
    );

    // 6. Next call: restarts child with new generation.
    let view2 = runtime
        .prompt(
            &session.id,
            "second call to lazy MCP",
            CancellationToken::new(),
        )
        .await
        .expect("second prompt failed");

    assert_eq!(view2.state, SessionState::Idle);
    let val2 = extract_cell_value(&view2.messages).expect("missing second cell value");
    assert_eq!(val2["sum"], json!(30));

    let snap_restarted = control.snapshot();
    assert_eq!(
        snap_restarted.connect_generation, 2,
        "restarted child must advance generation to 2"
    );
    assert!(
        snap_restarted.spawn_count >= 3,
        "spawn count must be >= 3 after restart"
    );

    // 7. Record durable facts before close.
    let tasks_before = runtime.tasks(&session.id).unwrap();
    assert_eq!(tasks_before.len(), 2, "must have 2 completed tasks");
    for t in &tasks_before {
        assert_eq!(
            t.state,
            TaskState::Completed,
            "task before close must have settled terminal state"
        );
    }
    let events_before = runtime.events(&session.id, 0, 200).unwrap();
    let messages_before = runtime.get(&session.id).unwrap().messages;
    let audit_before = runtime.audit(&session.id, 0, 200).unwrap();
    assert!(!audit_before.is_empty(), "audit log must record calls");

    // 8. Close and reopen Runtime.
    drop(runtime);

    let reopened = Runtime::open(
        &store_path,
        workspace.as_path(),
        Arc::new(ScriptedProvider),
        Model::new("fixture", "f20-lifecycle-fixture"),
        config,
    )
    .expect("reopen runtime");

    // Verify session and tasks stay intact without replaying.
    let reopened_session = reopened.get(&session.id).unwrap();
    assert_eq!(reopened_session.state, SessionState::Idle);
    assert_eq!(reopened_session.messages, messages_before);

    let tasks_after = reopened.tasks(&session.id).unwrap();
    assert_eq!(
        serde_json::to_value(&tasks_after).unwrap(),
        serde_json::to_value(&tasks_before).unwrap()
    );

    // Verify health confirms no replayed tasks: 0 running, 0 queued.
    let health = reopened.health().unwrap();
    assert_eq!(health.running, 0);
    assert_eq!(health.queued, 0);

    // Verify events and audit log match exactly.
    let events_after = reopened.events(&session.id, 0, 200).unwrap();
    assert_eq!(events_after, events_before);
    let audit_after = reopened.audit(&session.id, 0, 200).unwrap();
    assert_eq!(
        serde_json::to_value(&audit_after).unwrap(),
        serde_json::to_value(&audit_before).unwrap()
    );

    // 9. Clean disconnect and await close.
    registry.disconnect("local").unwrap();
    tokio::time::timeout(Duration::from_secs(5), control.wait_closed())
        .await
        .unwrap();
}
