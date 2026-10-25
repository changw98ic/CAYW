use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{
    AccessRequest, DispatchDropState, DispatchJournal, DispatchOutcome, DispatchRecord,
    InvocationContext, Limits, Provenance, Registry, RegistryPolicy, SourceIdentity, Visibility,
};
use cayw_sources::{LifecycleMode, ManagedStdio, ServerSpec, StdioConfig, import_mcp_servers_json};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

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
    ) -> Result<(), cayw_codemode::CapabilityError> {
        Ok(())
    }
}
struct Journal;
#[async_trait]
impl DispatchJournal for Journal {
    async fn started(&self, _: &DispatchRecord) -> Result<(), cayw_codemode::CapabilityError> {
        Ok(())
    }
    async fn settled(
        &self,
        _: &DispatchRecord,
        _: &DispatchOutcome,
    ) -> Result<(), cayw_codemode::CapabilityError> {
        Ok(())
    }
    fn dropped(
        &self,
        _: &DispatchRecord,
        _: DispatchDropState,
    ) -> Result<(), cayw_codemode::CapabilityError> {
        Ok(())
    }
}

async fn call(
    registry: &Registry,
    sequence: u64,
    name: &str,
    args: Value,
    cancel: CancellationToken,
) -> Result<Value, cayw_codemode::CapabilityError> {
    registry
        .consumer()
        .scoped(
            name,
            InvocationContext {
                session_id: "session".into(),
                task_id: "task".into(),
                attempt_id: "attempt".into(),
                execution_id: "cell".into(),
                sequence,
            },
            Arc::new(Journal),
        )
        .unwrap()
        .call(name, args, cancel)
        .await
}

fn fixture_program() -> std::path::PathBuf {
    Path::new(env!("CARGO_BIN_EXE_cayw-sources-local-fixture")).to_path_buf()
}

fn spec(root: &Path, lifecycle: LifecycleMode, idle: Duration) -> ServerSpec {
    ServerSpec {
        id: "local".into(),
        version: "2".into(),
        origin: "host:test-managed-stdio".into(),
        config: StdioConfig::new(fixture_program(), root),
        lifecycle,
        idle_timeout: idle,
    }
}

#[tokio::test]
async fn lazy_prepare_stops_probe_and_starts_on_codemode_call() {
    let root = tempfile::tempdir().unwrap();
    let prepared = ManagedStdio::prepare(
        spec(root.path(), LifecycleMode::Lazy, Duration::from_secs(60)),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let control = prepared.control();
    let snap = control.snapshot();
    assert!(!snap.connected);
    assert_eq!(snap.spawn_count, 1);
    assert_eq!(snap.connect_generation, 0);
    assert_eq!(prepared.manifest().tools.len(), 2);
    assert_eq!(prepared.manifest().prompts.len(), 1);

    let (manifest, handle) = prepared.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry
        .register(
            SourceIdentity {
                id: "local".into(),
                version: "2".into(),
                origin: "host:test-managed-stdio".into(),
            },
            manifest,
            handle,
        )
        .unwrap();

    let discovered = call(
        &registry,
        1,
        "mcp.discover",
        json!({"server":"local"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(discovered["tools"].as_array().unwrap().len(), 2);
    // Metadata discovery must not start the process.
    assert!(!control.snapshot().connected);
    assert_eq!(control.snapshot().spawn_count, 1);

    let prompts = call(
        &registry,
        2,
        "mcp.prompts.list",
        json!({"server":"local"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(prompts["prompts"][0]["name"], "add-help");
    assert!(!control.snapshot().connected);

    let result = call(
        &registry,
        3,
        "mcp.call",
        json!({"server":"local","tool":"add","arguments":{"a":2,"b":3}}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result["value"]["sum"], 5);
    let after = control.snapshot();
    assert!(after.connected);
    assert_eq!(after.spawn_count, 2);
    assert_eq!(after.connect_generation, 1);

    let prompt = call(
        &registry,
        4,
        "mcp.prompts.get",
        json!({"server":"local","name":"add-help","arguments":{"topic":"sum"}}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        prompt["value"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("sum")
    );

    registry.disconnect("local").unwrap();
    tokio::time::timeout(Duration::from_secs(5), control.wait_closed())
        .await
        .unwrap();
}

#[tokio::test]
async fn lazy_idle_timeout_recycles_without_dropping_catalog() {
    let root = tempfile::tempdir().unwrap();
    let prepared = ManagedStdio::prepare(
        spec(root.path(), LifecycleMode::Lazy, Duration::from_millis(150)),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let control = prepared.control();
    let (manifest, handle) = prepared.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry
        .register(
            SourceIdentity {
                id: "local".into(),
                version: "2".into(),
                origin: "host:test-managed-stdio".into(),
            },
            manifest,
            handle,
        )
        .unwrap();

    call(
        &registry,
        1,
        "mcp.call",
        json!({"server":"local","tool":"add","arguments":{"a":1,"b":1}}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(control.snapshot().connected);
    let generation = control.snapshot().connect_generation;

    tokio::time::sleep(Duration::from_millis(400)).await;
    let recycled = control.snapshot();
    assert!(!recycled.connected, "idle recycle should stop the child");
    assert!(recycled.recycle_count >= 1);
    assert_eq!(recycled.connect_generation, generation);

    let again = call(
        &registry,
        2,
        "mcp.call",
        json!({"server":"local","tool":"add","arguments":{"a":4,"b":5}}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(again["value"]["sum"], 9);
    let restarted = control.snapshot();
    assert!(restarted.connected);
    assert_eq!(restarted.connect_generation, generation + 1);
    assert!(restarted.spawn_count >= 3);

    registry.disconnect("local").unwrap();
    tokio::time::timeout(Duration::from_secs(5), control.wait_closed())
        .await
        .unwrap();
}

#[tokio::test]
async fn import_external_mcp_servers_json_prepares_lazy_source() {
    let root = tempfile::tempdir().unwrap();
    let program = fixture_program();
    let config_path = root.path().join("mcp.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{"mcpServers":{{"local":{{"command":"{}","lifecycle":"lazy","idleTimeout":0.01}}}}}}"#,
            program.display()
        ),
    )
    .unwrap();
    let text = std::fs::read_to_string(&config_path).unwrap();
    let report = import_mcp_servers_json(&text, Some(&config_path), root.path()).unwrap();
    assert_eq!(report.accepted.len(), 1);
    assert_eq!(report.accepted[0].spec.lifecycle, LifecycleMode::Lazy);

    let prepared = ManagedStdio::prepare(report.accepted[0].spec.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(!prepared.control().snapshot().connected);
    assert_eq!(prepared.server_info().name, "cayw-local-fixture");
    prepared.control().close();
    tokio::time::timeout(Duration::from_secs(5), prepared.control().wait_closed())
        .await
        .unwrap();
}
