use cayw_ai::CancellationToken;
use cayw_pi_compat::*;
use serde_json::json;
use std::path::PathBuf;

#[tokio::test]
async fn disabled_host_refuses_without_silent_success() {
    let host = PiCompatHost::new(false);
    assert!(!host.enabled());
    assert!(matches!(
        host.install_fixture(),
        Err(PiCompatError::Disabled)
    ));
}

#[tokio::test]
async fn fixture_install_call_unload_smoke() {
    let host = PiCompatHost::new(true);
    let package = PiFixturePlugin::package_id();

    let installed = host.install_fixture().expect("install fixture");
    assert_eq!(installed.manifest.name, package.name);
    assert_eq!(installed.manifest.version, package.version);

    let value = host
        .call_tool(
            &package,
            PiFixturePlugin::TOOL,
            json!({"text": "你好"}),
            CancellationToken::new(),
        )
        .await
        .expect("call tool");
    assert_eq!(value["echo"], "你好");
    assert_eq!(value["package"], "cayw-pi-fixture");

    let unloaded = host.unload(&package).expect("unload");
    assert_eq!(unloaded.manifest.name, package.name);
    assert!(matches!(
        host.call_tool(
            &package,
            PiFixturePlugin::TOOL,
            json!({"text": "x"}),
            CancellationToken::new()
        )
        .await,
        Err(PiCompatError::NotLoaded(_))
    ));
}

#[tokio::test]
async fn sample_extension_from_disk_install_call_unload() {
    let host = PiCompatHost::new(true);
    let package = PiSampleExtension::package_id();
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_cayw-pi-sample-ext"));
    assert!(
        exe.is_file(),
        "sample extension binary must be built for this test"
    );

    let installed = host
        .install_sample_extension(&exe, CancellationToken::new())
        .await
        .expect("install sample from disk");
    assert_eq!(installed.manifest.name, package.name);
    assert_eq!(installed.manifest.kind, cayw_plugins::PluginKind::External);

    let value = host
        .call_tool(
            &package,
            PiSampleExtension::TOOL,
            json!({"note": "from-disk"}),
            CancellationToken::new(),
        )
        .await
        .expect("call sample tool");
    assert_eq!(value["pong"], true);
    assert_eq!(value["path"], "external-subprocess");
    assert_eq!(value["note"], "from-disk");
    // Homemade sample must NOT claim Pi ExtensionAPI.
    assert!(value.get("pi_extension_api").is_none());

    host.unload(&package).expect("unload sample");
}

/// Primary F15 acceptance: real unmodified npm Pi hello.ts via ExtensionAPI adapter.
///
/// Fails if Node/npm deps are missing (explicit Unavailable) — never closes by
/// falling back to the homemade Rust sample alone.
#[tokio::test]
async fn npm_hello_extension_install_call_unload() {
    let host = PiCompatHost::new(true);
    let package = PiNpmHelloExtension::package_id();

    // Hard requirements — do not silently skip.
    assert_ne!(
        package.name,
        PiSampleExtension::PACKAGE_NAME,
        "acceptance package must be the real npm Pi package, not the Rust sample"
    );
    assert_eq!(package.name, PINNED_PI_CODING_AGENT);
    assert_eq!(package.version, PINNED_PI_CODING_AGENT_VERSION);

    let installed = host
        .install_npm_hello_extension(CancellationToken::new())
        .await
        .unwrap_or_else(|err| {
            panic!(
                "PRIMARY ACCEPTANCE FAILED loading unmodified npm Pi hello.ts: {err:?}\n\
                 Need Node >= 22.19 (CAYW_PI_NODE), `npm ci` in crates/cayw-pi-compat/npm, \
                 and the published examples/extensions/hello.ts. Do NOT substitute \
                 cayw-pi-sample-ext."
            )
        });

    assert_eq!(installed.manifest.name, PINNED_PI_CODING_AGENT);
    assert_eq!(installed.manifest.version, PINNED_PI_CODING_AGENT_VERSION);
    assert_eq!(installed.manifest.kind, cayw_plugins::PluginKind::External);
    assert!(
        installed
            .manifest
            .capabilities
            .iter()
            .any(|c| c.name == PiNpmHelloExtension::TOOL),
        "manifest must expose hello from unmodified extension"
    );

    let hello = installed
        .manifest
        .capabilities
        .iter()
        .find(|tool| tool.name == PiNpmHelloExtension::TOOL)
        .unwrap();
    assert_eq!(hello.parameters["additionalProperties"], false);
    assert!(
        host.call_tool(
            &package,
            PiNpmHelloExtension::TOOL,
            json!({"name": "CAYW", "unexpected": true}),
            CancellationToken::new(),
        )
        .await
        .is_err()
    );

    let value = host
        .call_tool(
            &package,
            PiNpmHelloExtension::TOOL,
            json!({"name": "CAYW"}),
            CancellationToken::new(),
        )
        .await
        .expect("call hello from unmodified npm extension");

    // Proof this is the Pi ExtensionAPI path, not the Rust stub.
    assert_eq!(value["pi_extension_api"], true);
    assert_eq!(value["protocol"], "pi-ExtensionAPI");
    assert_eq!(value["package"], PINNED_PI_CODING_AGENT);
    assert_eq!(value["package_version"], PINNED_PI_CODING_AGENT_VERSION);
    let ext_path = value["extension_path"].as_str().unwrap_or("");
    assert!(
        ext_path.ends_with("examples/extensions/hello.ts"),
        "extension_path must point at unmodified hello.ts, got {ext_path}"
    );
    assert!(
        !ext_path.contains("cayw-pi-sample-ext"),
        "must not be the Rust sample path"
    );
    let text = value["text"].as_str().unwrap_or("");
    assert!(
        text.contains("Hello, CAYW!"),
        "expected greeting from hello.ts execute(), got {text:?}"
    );
    assert_eq!(value["details"]["greeted"], "CAYW");

    host.unload(&package).expect("unload npm hello");
}

/// Guard: swapping the acceptance target for the Rust sample alone must fail.
#[tokio::test]
async fn acceptance_requires_pi_extension_api_markers() {
    let host = PiCompatHost::new(true);
    let sample_pkg = PiSampleExtension::package_id();
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_cayw-pi-sample-ext"));
    host.install_sample_extension(&exe, CancellationToken::new())
        .await
        .expect("install sample");
    let sample_value = host
        .call_tool(
            &sample_pkg,
            PiSampleExtension::TOOL,
            json!({"note": "stub"}),
            CancellationToken::new(),
        )
        .await
        .expect("call sample");
    host.unload(&sample_pkg).expect("unload sample");

    // Sample alone lacks Pi ExtensionAPI proof markers.
    assert!(sample_value.get("pi_extension_api").is_none());
    assert_ne!(
        sample_value.get("protocol"),
        Some(&json!("pi-ExtensionAPI"))
    );
    assert_ne!(sample_pkg.name, PINNED_PI_CODING_AGENT);

    // Primary path must still succeed with the real markers.
    let npm_pkg = PiNpmHelloExtension::package_id();
    host.install_npm_hello_extension(CancellationToken::new())
        .await
        .expect("npm hello required for acceptance");
    let npm_value = host
        .call_tool(
            &npm_pkg,
            PiNpmHelloExtension::TOOL,
            json!({"name": "Guard"}),
            CancellationToken::new(),
        )
        .await
        .expect("call npm hello");
    assert_eq!(npm_value["pi_extension_api"], true);
    assert_eq!(npm_value["protocol"], "pi-ExtensionAPI");
    assert!(
        npm_value["text"]
            .as_str()
            .unwrap_or("")
            .contains("Hello, Guard!")
    );
    host.unload(&npm_pkg).expect("unload npm");
}

#[tokio::test]
async fn missing_sample_executable_is_unavailable_not_silent() {
    let host = PiCompatHost::new(true);
    let missing = PathBuf::from("/tmp/cayw-pi-sample-ext-does-not-exist-f15");
    let err = host
        .install_sample_extension(&missing, CancellationToken::new())
        .await
        .expect_err("missing path must refuse");
    assert!(matches!(err, PiCompatError::Unavailable(_)), "got {err:?}");
    let matrix = host.matrix_snapshot();
    let row = matrix.lookup(
        &PiSampleExtension::package_id(),
        &SurfaceId::new(SurfaceFamily::Lifecycle, "install"),
    );
    assert_eq!(row.level, CompatibilityLevel::Unavailable);
}

#[tokio::test]
async fn incompatible_interaction_surface_is_recorded_not_silent() {
    let host = PiCompatHost::new(true);
    let package = PiFixturePlugin::package_id();
    let surface = SurfaceId::new(SurfaceFamily::Interaction, "ui.select");

    let err = host
        .probe_surface(&package, surface.clone())
        .expect_err("must refuse");
    assert!(matches!(err, PiCompatError::Incompatible(_)));

    let matrix = host.matrix_snapshot();
    let row = matrix.lookup(&package, &surface);
    assert_eq!(row.level, CompatibilityLevel::Incompatible);
}

#[tokio::test]
async fn real_npm_package_tools_are_partial_not_unavailable() {
    let matrix = CompatMatrix::first_cut_baseline();
    let real = PiPackageId::new(PINNED_PI_CODING_AGENT, PINNED_PI_CODING_AGENT_VERSION);
    let register = matrix.lookup(&real, &SurfaceId::new(SurfaceFamily::Tools, "registerTool"));
    assert_eq!(register.level, CompatibilityLevel::Partial);
    assert!(register.level.allows_invoke());
    assert!(register.detail.contains("hello.ts") || register.detail.contains("pi-npm-bridge"));
    let invoke = matrix.lookup(&real, &SurfaceId::new(SurfaceFamily::Tools, "invoke"));
    assert_eq!(invoke.level, CompatibilityLevel::Partial);
    let commands = matrix.lookup(
        &real,
        &SurfaceId::new(SurfaceFamily::Commands, "registerCommand"),
    );
    assert_eq!(commands.level, CompatibilityLevel::Unavailable);
    assert!(
        commands.detail.contains("unsupported") || commands.detail.contains("not a CAYW"),
        "commands detail should admit bridge throws unsupported, got {}",
        commands.detail
    );
    let events = matrix.lookup(&real, &SurfaceId::new(SurfaceFamily::Events, "on"));
    assert_eq!(events.level, CompatibilityLevel::Unavailable);
    let send = matrix.lookup(
        &real,
        &SurfaceId::new(SurfaceFamily::Session, "sendMessage"),
    );
    assert_eq!(send.level, CompatibilityLevel::Unavailable);
    assert!(
        send.detail.contains("unsupported") || send.detail.contains("sendMessage"),
        "sendMessage must be explicit unavailable, got {}",
        send.detail
    );
    // Honesty: tools partial must not be marketed as full ExtensionAPI.
    assert!(
        register.detail.contains("hello") || register.detail.contains("Not full"),
        "registerTool detail should scope to hello slice, got {}",
        register.detail
    );
}

#[test]
fn matrix_markdown_lists_fixture_sample_and_pinned_abi() {
    let md = CompatMatrix::first_cut_baseline().markdown_table();
    assert!(md.contains("cayw-pi-fixture@0.1.0"));
    assert!(md.contains("cayw-pi-sample-ext@0.1.0"));
    assert!(md.contains(PINNED_PI_CODING_AGENT));
    assert!(md.contains(PINNED_PI_CODING_AGENT_VERSION));
    assert!(md.contains("hello.ts") || md.contains("pi-npm-bridge"));
    assert!(md.contains("incompatible") || md.contains("unavailable"));
    assert!(md.contains("load_external"));
}

#[test]
fn family_mappings_cover_all_surfaces() {
    for family in SurfaceFamily::ALL {
        let m = mapping_for(family);
        assert_eq!(m.family, family);
        assert!(!m.cayw_seam.is_empty());
    }
}

#[tokio::test]
async fn cancelled_call_is_known_not_outcome_unknown() {
    let host = PiCompatHost::new(true);
    let package = PiFixturePlugin::package_id();
    host.install_fixture().expect("install fixture");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = host
        .call_tool(
            &package,
            PiFixturePlugin::TOOL,
            json!({"text": "x"}),
            cancel,
        )
        .await
        .expect_err("pre-cancelled call must fail");
    assert!(
        matches!(err, PiCompatError::Cancelled),
        "Cancelled must stay a known terminal, got {err:?}"
    );
    assert!(!err.outcome_unknown());
}

#[tokio::test]
async fn cancellation_after_plugin_dispatch_keeps_outcome_unknown() {
    struct PendingPlugin(tokio::sync::Notify);
    #[async_trait::async_trait]
    impl cayw_plugins::Plugin for PendingPlugin {
        fn manifest(&self) -> &cayw_plugins::PluginManifest {
            use std::sync::OnceLock;
            static MANIFEST: OnceLock<cayw_plugins::PluginManifest> = OnceLock::new();
            MANIFEST.get_or_init(PiFixturePlugin::build_manifest)
        }
        async fn call(
            &self,
            _: &str,
            _: serde_json::Value,
            _: CancellationToken,
        ) -> Result<serde_json::Value, cayw_plugins::PluginError> {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    let host = std::sync::Arc::new(PiCompatHost::new(true));
    let plugin = std::sync::Arc::new(PendingPlugin(tokio::sync::Notify::new()));
    host.registry()
        .load_native(
            plugin.clone(),
            cayw_plugins::PluginContext::new([PiFixturePlugin::TOOL.into()], []),
        )
        .unwrap();
    let cancel = CancellationToken::new();
    let running_host = host.clone();
    let running_cancel = cancel.clone();
    let running = tokio::spawn(async move {
        running_host
            .call_tool(
                &PiFixturePlugin::package_id(),
                PiFixturePlugin::TOOL,
                json!({"text":"started"}),
                running_cancel,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), plugin.0.notified())
        .await
        .unwrap();
    cancel.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.outcome_unknown(),
        "in-flight cancellation must retain unknown: {error:?}"
    );
}

#[tokio::test]
async fn shared_registry_across_compat_hosts() {
    let shared = std::sync::Arc::new(cayw_plugins::PluginRegistry::new());
    let host_a = PiCompatHost::with_registry(shared.clone(), true);
    let host_b = PiCompatHost::with_registry(shared.clone(), true);

    let package = PiFixturePlugin::package_id();
    host_a.install_fixture().expect("install on host a");

    // host_b shares the same registry, so the plugin is visible and callable
    let value = host_b
        .call_tool(
            &package,
            PiFixturePlugin::TOOL,
            json!({"text": "shared"}),
            CancellationToken::new(),
        )
        .await
        .expect("call via host b");
    assert_eq!(value["echo"], "shared");

    host_a.unload(&package).expect("unload");
}

#[tokio::test]
async fn register_into_bridge_and_invoke() {
    use async_trait::async_trait;
    use cayw_bridge::*;

    struct TestPolicy;
    #[async_trait]
    impl RegistryPolicy for TestPolicy {
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

    let host = std::sync::Arc::new(PiCompatHost::new(true));
    let package = PiFixturePlugin::package_id();
    host.install_fixture().expect("install fixture");

    let registry = Registry::new(Limits::default(), std::sync::Arc::new(TestPolicy)).unwrap();
    let prov = host
        .register_into_bridge(&package, &registry)
        .expect("register into bridge");
    assert_eq!(prov.source.id, package.name);

    let bridge = registry.consumer();
    let snapshot = bridge.binding_snapshot();
    assert!(snapshot.sources.contains_key(&package.name));
    let registered_prov = snapshot.sources.get(&package.name).unwrap();
    assert_eq!(registered_prov.source.id, package.name);

    host.unload(&package).expect("unload");
}
