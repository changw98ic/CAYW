//! Product entry: external mcpServers JSON → prepare → Registry → Runtime Bridge.
use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_bridge::{AccessRequest, Limits, Provenance, Registry, RegistryPolicy, Visibility};
use cayw_codemode::CapabilityError;
use cayw_core::SessionState;
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, fixture::script_response};
use cayw_sources::install_mcp_servers_file;
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

struct Scripted;
impl Provider for Scripted {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = script_response(
            &request,
            r#"
local catalog = cayw.call('mcp.discover', {})
local result = cayw.call('mcp.call', {server='local', tool='add', arguments={a=7, b=9}})
local prompts = cayw.call('mcp.prompts.list', {server='local'})
return {
  sum = result.value.sum,
  tools = #catalog.tools,
  prompt = prompts.prompts[1].name,
}
"#,
            "host-source-import",
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn cell_value(page: &Value) -> Option<Value> {
    page.get("outputs")?.as_array()?.iter().find_map(|entry| {
        let output = &entry["output"];
        (output["type"].as_str() == Some("json")).then(|| output["value"].clone())
    })
}

#[tokio::test]
async fn import_prepare_registry_lands_in_runtime_bridge() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let fixture = env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture");
    let config_path = root.path().join("mcp.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{"mcpServers":{{"local":{{"command":"{fixture}","lifecycle":"lazy","idleTimeout":0.05}},"remote":{{"url":"https://example.invalid/mcp"}}}}}}"#
        ),
    )
    .unwrap();

    let registry = Registry::new(Limits::default(), Arc::new(AllowAll)).unwrap();
    let report = install_mcp_servers_file(
        &config_path,
        workspace.as_path(),
        &registry,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.installed.len(), 1);
    assert_eq!(report.skipped.len(), 1);
    let control = report.installed[0].control.clone();
    assert!(!control.snapshot().connected);

    let config = RuntimeConfig {
        bridge: Some(registry.consumer()),
        permissions: vec![
            ScopedPermission::exact("mcp.discover", json!({})),
            ScopedPermission::exact(
                "mcp.call",
                json!({"server":"local","tool":"add","arguments":{"a":7,"b":9}}),
            ),
            ScopedPermission::exact("mcp.prompts.list", json!({"server":"local"})),
        ],
        max_run_time: Duration::from_secs(30),
        ..Default::default()
    };
    let runtime = Runtime::open(
        root.path().join("sessions.db"),
        workspace.as_path(),
        Arc::new(Scripted),
        Model::new("fixture", "host-source-import"),
        config,
    )
    .unwrap();
    let session = runtime.create(Some("import product entry".into())).unwrap();
    let view = runtime
        .prompt(
            &session.id,
            "discover and add through imported managed MCP",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_capabilities.is_empty());
    let value = view
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::ToolResult(result) => result.structured_content.as_ref().and_then(cell_value),
            _ => None,
        })
        .expect("missing cell value");
    assert_eq!(value["sum"], json!(16));
    assert_eq!(value["tools"], json!(2));
    assert_eq!(value["prompt"], json!("add-help"));
    assert!(control.snapshot().connected);
    assert!(control.snapshot().spawn_count >= 2);

    registry.disconnect("local").unwrap();
    tokio::time::timeout(Duration::from_secs(10), control.wait_closed())
        .await
        .unwrap();
}
