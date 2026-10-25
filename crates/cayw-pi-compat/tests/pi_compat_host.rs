use cayw_ai::CancellationToken;
use cayw_pi_compat::*;
use serde_json::json;

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
        .call_tool(&package, json!({"text": "你好"}), CancellationToken::new())
        .await
        .expect("call tool");
    assert_eq!(value["echo"], "你好");
    assert_eq!(value["package"], "cayw-pi-fixture");

    let unloaded = host.unload(&package).expect("unload");
    assert_eq!(unloaded.manifest.name, package.name);
    assert!(matches!(
        host.call_tool(&package, json!({"text": "x"}), CancellationToken::new())
            .await,
        Err(PiCompatError::NotLoaded(_))
    ));
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
fn matrix_markdown_lists_fixture_and_pinned_abi() {
    let md = CompatMatrix::first_cut_baseline().markdown_table();
    assert!(md.contains("cayw-pi-fixture@0.1.0"));
    assert!(md.contains(PINNED_PI_CODING_AGENT));
    assert!(md.contains(PINNED_PI_CODING_AGENT_VERSION));
    assert!(md.contains("incompatible") || md.contains("unavailable"));
}

#[test]
fn family_mappings_cover_all_surfaces() {
    for family in SurfaceFamily::ALL {
        let m = mapping_for(family);
        assert_eq!(m.family, family);
        assert!(!m.cayw_seam.is_empty());
    }
}
