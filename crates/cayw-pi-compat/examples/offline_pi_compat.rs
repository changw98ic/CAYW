//! Offline F15 smoke: fixture + optional sample-from-disk; print matrix.

use cayw_ai::CancellationToken;
use cayw_pi_compat::*;
use serde_json::json;
use std::path::PathBuf;

fn sample_exe() -> Option<PathBuf> {
    if let Ok(exe) = std::env::var("CARGO_BIN_EXE_cayw-pi-sample-ext") {
        let path = PathBuf::from(exe);
        if path.is_file() {
            return Some(path);
        }
    }
    let candidates = [
        PathBuf::from("target/debug/cayw-pi-sample-ext"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/cayw-pi-sample-ext"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

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

    let desc = host.install_fixture().expect("install fixture");
    println!(
        "fixture installed {}@{} generation={}",
        desc.manifest.name, desc.manifest.version, desc.generation
    );

    let value = host
        .call_tool(
            &package,
            PiFixturePlugin::TOOL,
            json!({"text": "offline-ok"}),
            CancellationToken::new(),
        )
        .await
        .expect("call fixture");
    println!("fixture call result: {value}");
    host.unload(&package).expect("unload fixture");
    println!("unloaded {}", package.key());

    // Real path beyond fixture: sample extension from disk when the bin is built.
    if let Some(exe) = sample_exe() {
        let sample = PiSampleExtension::package_id();
        match host
            .install_sample_extension(&exe, CancellationToken::new())
            .await
        {
            Ok(desc) => {
                println!(
                    "sample installed {}@{} from {}",
                    desc.manifest.name,
                    desc.manifest.version,
                    exe.display()
                );
                let value = host
                    .call_tool(
                        &sample,
                        PiSampleExtension::TOOL,
                        json!({"note": "offline-sample"}),
                        CancellationToken::new(),
                    )
                    .await
                    .expect("call sample");
                println!("sample call result: {value}");
                host.unload(&sample).expect("unload sample");
            }
            Err(err) => println!("sample path refused (explicit): {err}"),
        }
    } else {
        println!("sample binary not found; skipped (fixture path only this run)");
    }

    // Explicit refusal path
    let refuse = host.probe_surface(
        &package,
        SurfaceId::new(SurfaceFamily::Interaction, "ui.select"),
    );
    println!("interaction probe: {refuse:?}");

    println!("\n## 版本/兼容矩阵（首刀）\n");
    print!("{}", host.matrix_snapshot().markdown_table());
}
