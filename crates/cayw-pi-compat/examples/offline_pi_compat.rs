//! Offline F15 smoke: enable host → install fixture → call → unload; print matrix.

use cayw_ai::CancellationToken;
use cayw_pi_compat::*;
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let host = PiCompatHost::new(true);
    let package = PiFixturePlugin::package_id();

    println!(
        "Pi ABI pin: {}@{}",
        PINNED_PI_CODING_AGENT, PINNED_PI_CODING_AGENT_VERSION
    );
    println!(
        "CAYW plugin protocol: {}; pi-compat protocol: {}",
        CAYW_PLUGIN_PROTOCOL, PI_COMPAT_PROTOCOL_VERSION
    );

    let desc = host.install_fixture().expect("install");
    println!(
        "installed {}@{} generation={}",
        desc.manifest.name, desc.manifest.version, desc.generation
    );

    let value = host
        .call_tool(
            &package,
            json!({"text": "offline-ok"}),
            CancellationToken::new(),
        )
        .await
        .expect("call");
    println!("call result: {value}");

    host.unload(&package).expect("unload");
    println!("unloaded {}", package.key());

    // Explicit refusal path
    let refuse = host.probe_surface(
        &package,
        SurfaceId::new(SurfaceFamily::Interaction, "ui.select"),
    );
    println!("interaction probe: {refuse:?}");

    println!("\n## 版本/兼容矩阵（首刀）\n");
    print!("{}", host.matrix_snapshot().markdown_table());
}
