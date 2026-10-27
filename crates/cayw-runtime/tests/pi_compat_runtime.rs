#![cfg(feature = "pi-compat")]

use async_trait::async_trait;
use cayw_ai::{CancellationToken, Model};
use cayw_bridge::{AccessRequest, Limits, Provenance, Registry, RegistryPolicy, Visibility};
use cayw_codemode::CapabilityError;
use cayw_pi_compat::{PiCompatError, PiCompatHost, PiFixturePlugin};
use cayw_runtime::{Runtime, RuntimeConfig, fixture::OfflineProvider};
use serde_json::json;
use std::sync::Arc;

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

#[tokio::test]
async fn pi_compat_shares_runtime_plugin_registry_and_enables_discovery_and_call() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let runtime = Runtime::open(
        dir.path().join("sessions.db"),
        workspace,
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "test"),
        RuntimeConfig::default(),
    )
    .unwrap();

    // 1. Initial plugin list is empty
    assert!(runtime.list_plugins().is_empty());

    // 2. Disabled PiCompatHost refuses operations
    let disabled_host = PiCompatHost::with_registry(runtime.plugin_registry(), false);
    assert!(!disabled_host.enabled());
    assert!(matches!(
        disabled_host.install_fixture(),
        Err(PiCompatError::Disabled)
    ));

    // 3. Enabled host with Runtime's shared PluginRegistry
    let pi_host = Arc::new(PiCompatHost::with_registry(runtime.plugin_registry(), true));
    assert!(pi_host.enabled());

    let package = PiFixturePlugin::package_id();
    let desc = pi_host.install_fixture().unwrap();
    assert_eq!(desc.manifest.name, PiFixturePlugin::PACKAGE_NAME);

    // 4. Runtime's unified plugin list reflects the installed Pi compat extension
    let plugins = runtime.list_plugins();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].manifest.name, PiFixturePlugin::PACKAGE_NAME);
    assert!(
        plugins[0]
            .manifest
            .capabilities
            .iter()
            .any(|c| c.name == PiFixturePlugin::TOOL)
    );

    // 5. Tool invocation through the shared registry works as expected
    let res = pi_host
        .call_tool(
            &package,
            PiFixturePlugin::TOOL,
            json!({"text": "runtime integration"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(res["echo"], "runtime integration");
    assert_eq!(res["package"], PiFixturePlugin::PACKAGE_NAME);
    assert_eq!(res["version"], PiFixturePlugin::PACKAGE_VERSION);
    assert_eq!(res["surface"], "tools.invoke");

    // 6. Unified Bridge registration without a private isolated registry
    let bridge_registry = Registry::new(Limits::default(), Arc::new(AllowAll)).unwrap();
    let provenance = pi_host
        .register_into_bridge(&package, &bridge_registry)
        .unwrap();
    assert_eq!(provenance.source.id, PiFixturePlugin::PACKAGE_NAME);

    // 7. Unload cleanly removes from Runtime's shared registry
    pi_host.unload(&package).unwrap();
    assert!(runtime.list_plugins().is_empty());

    // 8. Subsequent call confirms unloaded state
    let call_after_unload = pi_host
        .call_tool(
            &package,
            PiFixturePlugin::TOOL,
            json!({"text": "after unload"}),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        call_after_unload,
        Err(PiCompatError::NotLoaded(_))
    ));
}
