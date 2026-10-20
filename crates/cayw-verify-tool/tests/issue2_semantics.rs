use cayw_verify_tool::sdk::ScenarioReport;
use serde_json::json;
use std::{fs, process::Command};

#[test]
fn reports_have_one_readable_v2_format() {
    let root = std::env::temp_dir().join(format!("cayw-v2-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let output = root.join("run");
    let result = Command::new(env!("CARGO_BIN_EXE_cayw-verify-tool"))
        .args([
            "sdk-ipc",
            "--samples",
            "1",
            "--scenario",
            "sdk-lifecycle",
            "--out",
        ])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["schema_version"], 2);
    let read = Command::new(env!("CARGO_BIN_EXE_cayw-verify-tool"))
        .arg("read")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn original_child_is_reaped_after_success_failure_timeout_and_future_drop() {
    let root = std::env::temp_dir().join(format!("cayw-child-probe-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cayw-verify-tool"))
        .arg("child-probe")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for mode in ["normal", "failure", "timeout", "drop"] {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(format!("{mode}.json"))).unwrap()).unwrap();
        assert_eq!(value["cleanup_confirmed"], true, "{mode}");
        assert!(value["process_id"].is_u64());
        assert!(value["stdout"].as_array().unwrap().len() <= 4096);
        if mode == "normal" {
            assert_eq!(value["exit_code"], 0);
        } else {
            assert_ne!(value["exit_code"], 0);
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn build_binding_is_measured_from_files_during_compilation() {
    let binding = cayw_verify_tool::provenance::build_binding();
    assert_eq!(
        binding["source_binding_authority"],
        "build-script-file-content"
    );
    assert!(
        binding["source_manifest_verification"]
            .as_str()
            .unwrap()
            .contains("listed")
    );
}

#[test]
fn successful_cleanup_alone_cannot_verify_a_custom_task() {
    let mut report = ScenarioReport::new("custom", 1, json!({}), json!({}));
    report.check("cleanup", true, "The original resource was stopped");
    report.finish();
    assert_eq!(
        report.status, "unverified",
        "Cleanup provides no task result"
    );
}
