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

    host.unload(&package).expect("unload sample");
}

#[tokio::test]
async fn missing_sample_executable_is_unavailable_not_silent() {
    let host = PiCompatHost::new(true);
    let missing = PathBuf::from("/tmp/cayw-pi-sample-ext-does-not-exist-f15");
    let err = host
        .install_sample_extension(&missing, CancellationToken::new())
        .await
        .expect_err("missing path must refuse");
    assert!(
        matches!(err, PiCompatError::Unavailable(_)),
        "got {err:?}"
    );
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
async fn real_npm_package_row_is_unavailable() {
    let matrix = CompatMatrix::first_cut_baseline();
    let real = PiPackageId::new(PINNED_PI_CODING_AGENT, PINNED_PI_CODING_AGENT_VERSION);
    let row = matrix.lookup(&real, &SurfaceId::new(SurfaceFamily::Tools, "registerTool"));
    assert_eq!(row.level, CompatibilityLevel::Unavailable);
    assert!(!row.level.allows_invoke());
}

#[test]
fn matrix_markdown_lists_fixture_sample_and_pinned_abi() {
    let md = CompatMatrix::first_cut_baseline().markdown_table();
    assert!(md.contains("cayw-pi-fixture@0.1.0"));
    assert!(md.contains("cayw-pi-sample-ext@0.1.0"));
    assert!(md.contains(PINNED_PI_CODING_AGENT));
    assert!(md.contains(PINNED_PI_CODING_AGENT_VERSION));
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
        .call_tool(&package, PiFixturePlugin::TOOL, json!({"text": "x"}), cancel)
        .await
        .expect_err("pre-cancelled call must fail");
    assert!(
        matches!(err, PiCompatError::Cancelled),
        "Cancelled must stay a known terminal, got {err:?}"
    );
    assert!(!err.outcome_unknown());
}
