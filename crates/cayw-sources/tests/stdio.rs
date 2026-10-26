use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{
    AccessRequest, DispatchDropState, DispatchJournal, DispatchOutcome, DispatchRecord,
    InvocationContext, Limits, Provenance, Registry, RegistryPolicy, SourceIdentity, Visibility,
};
use cayw_sources::{MCP_PROTOCOL, StdioConfig, StdioSource};
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
async fn source(root: &Path) -> StdioSource {
    StdioSource::connect(
        StdioConfig::new(
            Path::new(env!("CARGO_BIN_EXE_cayw-sources-local-fixture")),
            root,
        ),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn one_prepared_connection_discovers_metadata_and_calls_only_after_registry_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let prepared = source(root.path()).await;
    assert_eq!(MCP_PROTOCOL, "2026-07-28");
    assert_eq!(prepared.server_info().name, "cayw-local-fixture");
    assert_eq!(prepared.manifest().tools.len(), 2);
    assert!(prepared.manifest().skills.is_empty());
    let control = prepared.control();
    let snapshot = control.snapshot();
    assert_eq!(snapshot.spawned, 1);
    assert_eq!(
        snapshot.methods,
        vec!["tools/list", "resources/list", "prompts/list"]
    );
    assert_eq!(snapshot.tool_calls, 0);
    assert_eq!(snapshot.resource_reads, 0);
    let (manifest, handle) = prepared.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry
        .register(
            SourceIdentity {
                id: "local".into(),
                version: "2".into(),
                origin: "host:local-stdio".into(),
            },
            manifest,
            handle,
        )
        .unwrap();
    let result = call(
        &registry,
        1,
        "mcp.call",
        json!({"server":"local","tool":"add","arguments":{"a":2,"b":3}}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result["value"]["sum"], 5);
    assert_eq!(result["value"]["connections"], 1);
    assert_eq!(control.snapshot().tool_calls, 1);
    assert_eq!(
        control.snapshot().methods,
        vec!["tools/list", "resources/list", "prompts/list", "tools/call"]
    );
    let resource = call(
        &registry,
        2,
        "mcp.resources.read",
        json!({"server":"local","uri":"fixture://local/addition"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        resource["value"]["text"]
            .as_str()
            .unwrap()
            .starts_with("# Local addition")
    );
    assert_eq!(control.snapshot().resource_reads, 1);
    registry.disconnect("local").unwrap();
    tokio::time::timeout(Duration::from_secs(5), control.wait_closed())
        .await
        .unwrap();
    assert!(control.snapshot().reaped);
}
#[tokio::test]
async fn actual_cancel_sends_original_request_id_and_discards_late_response() {
    let root = tempfile::tempdir().unwrap();
    let prepared = source(root.path()).await;
    let control = prepared.control();
    let (manifest, handle) = prepared.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry
        .register(
            SourceIdentity {
                id: "local".into(),
                version: "2".into(),
                origin: "host:local-stdio".into(),
            },
            manifest,
            handle,
        )
        .unwrap();
    let cancel = CancellationToken::new();
    let pending = call(
        &registry,
        1,
        "mcp.call",
        json!({"server":"local","tool":"wait","arguments":{}}),
        cancel.clone(),
    );
    tokio::pin!(pending);
    tokio::select! { result=&mut pending=>panic!("unexpected {result:?}"), _=async {
        tokio::time::timeout(Duration::from_secs(5),async {while control.snapshot().progress==0 {tokio::task::yield_now().await;}}).await.unwrap();
    }=>{} }
    cancel.cancel();
    assert!(pending.await.unwrap_err().outcome_unknown);
    tokio::time::timeout(Duration::from_secs(5), async {
        while control.snapshot().ignored_late == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(control.snapshot().cancellations, 1);
    assert!(registry.snapshot("local").unwrap().outcome_unknown);
    registry.disconnect("local").unwrap();
    control.wait_closed().await;
}
#[tokio::test]
async fn invalid_paths_bounds_and_pre_cancel_never_spawn_a_child() {
    let root = tempfile::tempdir().unwrap();
    let mut config = StdioConfig::new("relative-program", root.path());
    assert!(
        StdioSource::connect(config.clone(), CancellationToken::new())
            .await
            .is_err()
    );
    config.program = Path::new(env!("CARGO_BIN_EXE_cayw-sources-local-fixture")).into();
    config.max_frame_bytes = 0;
    assert!(
        StdioSource::connect(config, CancellationToken::new())
            .await
            .is_err()
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        StdioSource::connect(
            StdioConfig::new(
                Path::new(env!("CARGO_BIN_EXE_cayw-sources-local-fixture")),
                root.path()
            ),
            cancel
        )
        .await
        .is_err()
    );
}
