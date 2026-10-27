//! Standalone integration test: Unified Bridge combination of real MCP Stdio + Pi extension.
//!
//! Validates:
//! 1. Shared `Registry` and unified `Bridge` hosting both ManagedStdio MCP and PiCompatHost.
//! 2. `mcp.discover` lists capabilities from both sources in one catalog.
//! 3. CodeMode dispatches calls to both `local` (MCP) and `cayw-pi-fixture` (Pi).
//! 4. MCP lifecycle (lazy on-demand startup + idle recycle) does not perturb Pi operations.
//! 5. Runtime close and reopen preserves durable task/session state without replay.

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_bridge::{AccessRequest, Limits, Provenance, Registry, RegistryPolicy, Visibility};
use cayw_codemode::CapabilityError;
use cayw_core::SessionState;
use cayw_pi_compat::{PiCompatHost, PiFixturePlugin};
use cayw_plugins::PluginRegistry;
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

struct ComboScriptedProvider;

impl Provider for ComboScriptedProvider {
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

        let (script, call_id) = if last_user_text.contains("combo") {
            (
                r#"
local catalog = cayw.call('mcp.discover', {})
local add_res = cayw.call('mcp.call', {server='local', tool='add', arguments={a=15, b=27}})
local pi_res = cayw.call('mcp.call', {server='cayw-pi-fixture', tool='pi_fixture.echo', arguments={text='mcp-pi-combo-unified'}})
return {
  sum = add_res.value.sum,
  echo = pi_res.value.echo,
  package = pi_res.value.package,
  tool_count = #catalog.tools,
}
"#,
                "call-combo",
            )
        } else {
            (
                r#"
local pi_res = cayw.call('mcp.call', {server='cayw-pi-fixture', tool='pi_fixture.echo', arguments={text='pi-standalone-after-recycle'}})
return {
  echo = pi_res.value.echo,
  package = pi_res.value.package,
}
"#,
                "call-pi-only",
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
async fn unified_bridge_combines_real_mcp_and_pi_compat_extension() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store_path = root.path().join("sessions.db");

    // 1. Shared Bridge Registry for all sources.
    let registry = Registry::new(Limits::default(), Arc::new(AllowAll)).unwrap();

    // 2. Prepare real ManagedStdio MCP source in Lazy mode with 150ms idle timeout.
    let fixture_exe = env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture");
    let spec = ServerSpec {
        id: "local".into(),
        version: "1.0.0".into(),
        origin: "host:mcp-fixture".into(),
        config: StdioConfig::new(fixture_exe, workspace.as_path()),
        lifecycle: LifecycleMode::Lazy,
        idle_timeout: Duration::from_millis(150),
    };
    let prepared = ManagedStdio::prepare(spec, CancellationToken::new())
        .await
        .expect("prepare managed mcp");
    let mcp_control = prepared.control();
    let (mcp_manifest, mcp_handle) = prepared.into_parts();

    registry
        .register(
            cayw_bridge::SourceIdentity {
                id: "local".into(),
                version: "1.0.0".into(),
                origin: "host:mcp-fixture".into(),
            },
            mcp_manifest,
            mcp_handle,
        )
        .expect("register MCP into bridge");

    // 3. Prepare PiCompatHost with shared PluginRegistry and register into Bridge.
    let plugin_registry = Arc::new(PluginRegistry::new());
    let pi_host = Arc::new(PiCompatHost::with_registry(
        Arc::clone(&plugin_registry),
        true,
    ));
    let pi_desc = pi_host.install_fixture().expect("install pi fixture");
    assert_eq!(pi_desc.manifest.name, PiFixturePlugin::PACKAGE_NAME);

    let pi_pkg = PiFixturePlugin::package_id();
    let pi_provenance = pi_host
        .register_into_bridge(&pi_pkg, &registry)
        .expect("register Pi into bridge");
    assert_eq!(pi_provenance.source.id, PiFixturePlugin::PACKAGE_NAME);

    // Initial state: MCP not connected, Pi registered and ready.
    assert!(!mcp_control.snapshot().connected);

    // 4. RuntimeConfig taking both unified Bridge and shared PluginRegistry.
    let config = RuntimeConfig {
        bridge: Some(registry.consumer()),
        plugin_registry: Some(plugin_registry),
        permissions: vec![
            ScopedPermission::exact("mcp.discover", json!({})),
            ScopedPermission::exact(
                "mcp.call",
                json!({"server":"local","tool":"add","arguments":{"a":15,"b":27}}),
            ),
            ScopedPermission::exact(
                "mcp.call",
                json!({
                    "server":"cayw-pi-fixture",
                    "tool":"pi_fixture.echo",
                    "arguments":{"text":"mcp-pi-combo-unified"}
                }),
            ),
            ScopedPermission::exact(
                "mcp.call",
                json!({
                    "server":"cayw-pi-fixture",
                    "tool":"pi_fixture.echo",
                    "arguments":{"text":"pi-standalone-after-recycle"}
                }),
            ),
        ],
        max_run_time: Duration::from_secs(30),
        ..Default::default()
    };

    // 5. Open Runtime.
    let runtime = Runtime::open(
        &store_path,
        workspace.as_path(),
        Arc::new(ComboScriptedProvider),
        Model::new("fixture", "combo-fixture"),
        config.clone(),
    )
    .expect("open runtime");

    // Plugin list in Runtime confirms Pi extension is visible.
    let plugins = runtime.list_plugins();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].manifest.name, PiFixturePlugin::PACKAGE_NAME);

    let session = runtime
        .create(Some("MCP + Pi Unified Combo Session".into()))
        .unwrap();

    // 6. First prompt: combo invocation calling both MCP and Pi in one script.
    let view1 = runtime
        .prompt(
            &session.id,
            "run combo test with MCP and Pi",
            CancellationToken::new(),
        )
        .await
        .expect("combo prompt failed");

    assert_eq!(view1.state, SessionState::Idle);
    let val1 = extract_cell_value(&view1.messages).expect("missing combo cell value");
    assert_eq!(val1["sum"], json!(42));
    assert_eq!(val1["echo"], json!("mcp-pi-combo-unified"));
    assert_eq!(val1["package"], json!(PiFixturePlugin::PACKAGE_NAME));
    // Discover includes both MCP tools (add, sample-prompt) and Pi tools (fixture.echo).
    let tool_count = val1["tool_count"].as_u64().expect("tool_count");
    assert!(
        tool_count >= 3,
        "catalog must include tools from both MCP and Pi (got {tool_count})"
    );

    // The call established the first connection, which may already be idle.
    assert_eq!(mcp_control.snapshot().connect_generation, 1);

    // 7. Observe actual idle recycling within a generous deadline.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = mcp_control.snapshot();
            if !snapshot.connected && snapshot.recycle_count >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("MCP idle recycle deadline");

    // 8. Second prompt: call Pi extension while MCP is stopped.
    let view2 = runtime
        .prompt(
            &session.id,
            "call pi only after mcp recycle",
            CancellationToken::new(),
        )
        .await
        .expect("pi-only prompt failed");

    assert_eq!(view2.state, SessionState::Idle);
    let val2 = extract_cell_value(&view2.messages).expect("missing pi cell value");
    assert_eq!(val2["echo"], json!("pi-standalone-after-recycle"));
    assert_eq!(val2["package"], json!(PiFixturePlugin::PACKAGE_NAME));
    // MCP remained stopped during the Pi-only call.
    assert!(
        !mcp_control.snapshot().connected,
        "MCP remained disconnected during Pi-only call"
    );

    // 9. Inspect durable tasks before closing.
    let tasks_before = runtime.tasks(&session.id).unwrap();
    assert_eq!(tasks_before.len(), 2);
    for t in &tasks_before {
        assert_eq!(t.state, TaskState::Completed);
    }
    let messages_before = runtime.get(&session.id).unwrap().messages;
    let events_before = runtime.events(&session.id, 0, 200).unwrap();
    let audit_before = runtime.audit(&session.id, 0, 200).unwrap();

    // 10. Close and Reopen Runtime.
    drop(runtime);

    let reopened = Runtime::open(
        &store_path,
        workspace.as_path(),
        Arc::new(ComboScriptedProvider),
        Model::new("fixture", "combo-fixture"),
        config,
    )
    .expect("reopen runtime");

    let reopened_session = reopened.get(&session.id).unwrap();
    assert_eq!(reopened_session.state, SessionState::Idle);
    assert_eq!(reopened_session.messages, messages_before);

    let tasks_after = reopened.tasks(&session.id).unwrap();
    assert_eq!(
        serde_json::to_value(&tasks_after).unwrap(),
        serde_json::to_value(&tasks_before).unwrap()
    );

    let health = reopened.health().unwrap();
    assert_eq!(health.running, 0);
    assert_eq!(health.queued, 0);

    let events_after = reopened.events(&session.id, 0, 200).unwrap();
    assert_eq!(events_after, events_before);
    let audit_after = reopened.audit(&session.id, 0, 200).unwrap();
    assert_eq!(
        serde_json::to_value(&audit_after).unwrap(),
        serde_json::to_value(&audit_before).unwrap()
    );

    // 11. Clean shutdown of both sources.
    pi_host.unload(&pi_pkg).unwrap();
    registry.disconnect("local").unwrap();
    tokio::time::timeout(Duration::from_secs(5), mcp_control.wait_closed())
        .await
        .unwrap();
}
