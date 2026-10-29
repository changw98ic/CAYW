use cayw_verify_tool::evidence::{Bundle, BundleWriter, compare, scenario_coverage_complete};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

fn temp_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    loop {
        let path = std::env::temp_dir().join(format!(
            "cayw-cov-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create temp directory: {error}"),
        }
    }
}

fn valid_meta(root: &Path, samples_per_scenario: u32, scenario_names: &[&str]) -> Value {
    let binding = cayw_verify_tool::sdk::local_binding().unwrap();
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    json!({
        "schema_version": 2,
        "run_id": run_id,
        "source": "offline synthetic",
        "binding": binding,
        "samples_per_scenario": samples_per_scenario,
        "out": root,
        "scenarios": scenario_names.iter().map(|name| json!({
            "name": name,
            "specification": {"inputs": {}, "permissions": {"grants": []}}
        })).collect::<Vec<_>>(),
        "measurement": {
            "clock": "std::time::Instant monotonic elapsed",
            "duration_unit": "ns",
            "sampling": "unit test",
            "performance_thresholds": null
        },
        "real_provider": {"status": "unverified", "calls": 0, "credentials_read": false},
        "issue_2_complete": false,
        "artifacts": {"samples": "samples.jsonl", "summary": "summary.json"}
    })
}

fn write_sample(writer: &mut BundleWriter, scenario: &str, iteration: u32) {
    let mut report = writer.start_sample(scenario, iteration).unwrap();
    let root = writer.sample_root(scenario, iteration);
    let sample_file = root.join("artifact.txt");
    fs::write(&sample_file, b"sample data").unwrap();
    report
        .observe("test_observation", json!({"iteration": iteration}))
        .unwrap();
    report.task_check("test_check", true, "Sample check passed");
    writer.complete_sample(report).unwrap();
}

#[test]
fn incomplete_scenario_coverage_is_detected_and_not_marked_passed() {
    let root = temp_dir("incomplete-scenarios");
    let meta = valid_meta(&root, 1, &["scenario-a", "scenario-b"]);
    let mut writer = BundleWriter::new(&root, meta, None).unwrap();
    write_sample(&mut writer, "scenario-a", 1);
    let passed = writer.finish(false).unwrap();
    assert!(
        !passed,
        "finish(false) must not report passed when declared scenario is missing"
    );

    let bundle = Bundle::read(&root).unwrap();
    assert_eq!(bundle.samples.len(), 1);
    assert_eq!(
        bundle.summary["semantic_passed"], false,
        "read back summary must not be marked semantic_passed"
    );
    assert_eq!(bundle.summary["sampling_stopped"], false);
    assert_eq!(bundle.summary["sample_count"], 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn incomplete_repetitions_coverage_is_detected_and_not_marked_passed() {
    let root = temp_dir("incomplete-repetitions");
    let meta = valid_meta(&root, 2, &["scenario-a"]);
    let mut writer = BundleWriter::new(&root, meta, None).unwrap();
    write_sample(&mut writer, "scenario-a", 1);
    let passed = writer.finish(false).unwrap();
    assert!(
        !passed,
        "finish(false) must not report passed when repetition is missing"
    );

    let bundle = Bundle::read(&root).unwrap();
    assert_eq!(bundle.samples.len(), 1);
    assert_eq!(bundle.summary["semantic_passed"], false);
    assert_eq!(bundle.summary["sampling_stopped"], false);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn complete_scenarios_and_repetitions_passes_and_reads_back() {
    let root = temp_dir("complete-coverage");
    let meta = valid_meta(&root, 2, &["scenario-a", "scenario-b"]);
    let mut writer = BundleWriter::new(&root, meta.clone(), None).unwrap();
    write_sample(&mut writer, "scenario-a", 1);
    write_sample(&mut writer, "scenario-a", 2);
    write_sample(&mut writer, "scenario-b", 1);
    write_sample(&mut writer, "scenario-b", 2);
    let passed = writer.finish(false).unwrap();
    assert!(
        passed,
        "finish(false) must report passed when all scenarios and repetitions succeed"
    );

    let bundle = Bundle::read(&root).unwrap();
    assert_eq!(bundle.samples.len(), 4);
    assert_eq!(bundle.summary["semantic_passed"], true);
    assert_eq!(bundle.summary["sampling_stopped"], false);
    assert!(scenario_coverage_complete(&bundle.meta, &bundle.samples));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicitly_stopped_sampling_is_marked_failed_while_retaining_evidence() {
    let root = temp_dir("stopped-sampling");
    let meta = valid_meta(&root, 1, &["scenario-a"]);
    let mut writer = BundleWriter::new(&root, meta, None).unwrap();
    write_sample(&mut writer, "scenario-a", 1);
    let passed = writer.finish(true).unwrap();
    assert!(
        !passed,
        "finish(true) must report false when sampling was explicitly stopped"
    );

    let bundle = Bundle::read(&root).unwrap();
    assert_eq!(bundle.samples.len(), 1);
    assert_eq!(bundle.summary["semantic_passed"], false);
    assert_eq!(bundle.summary["sampling_stopped"], true);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn sample_iteration_exceeding_declared_bound_is_rejected() {
    let root = temp_dir("bound-check");
    let meta = valid_meta(&root, 1, &["scenario-a"]);
    let mut writer = BundleWriter::new(&root, meta, None).unwrap();
    let result = writer.start_sample("scenario-a", 2);
    assert!(
        result.is_err(),
        "start_sample must reject iteration > samples_per_scenario"
    );

    write_sample(&mut writer, "scenario-a", 1);
    writer.finish(false).unwrap();

    let sample_line = fs::read_to_string(root.join("samples.jsonl")).unwrap();
    let mut sample_val: Value = serde_json::from_str(sample_line.lines().next().unwrap()).unwrap();
    sample_val["iteration"] = 2.into();
    sample_val["sample_id"] = "0002/scenario-a".into();

    let invalid_dir = temp_dir("invalid-iteration-reader");
    fs::create_dir_all(invalid_dir.join("samples/0002/scenario-a")).unwrap();
    fs::copy(root.join("meta.json"), invalid_dir.join("meta.json")).unwrap();

    let invalid_samples_line = serde_json::to_string(&sample_val).unwrap() + "\n";
    fs::write(invalid_dir.join("samples.jsonl"), &invalid_samples_line).unwrap();

    let mut stored = sample_val.clone();
    stored.as_object_mut().unwrap().remove("run_id");
    stored.as_object_mut().unwrap().remove("meta");
    fs::write(
        invalid_dir.join("samples/0002/scenario-a/report.json"),
        serde_json::to_string(&stored).unwrap(),
    )
    .unwrap();

    let records_content = serde_json::to_string(&json!({
        "sequence": 1,
        "kind": "scenario_start",
        "value": {
            "sample_id": "0002/scenario-a",
            "specification": {"inputs": {}, "permissions": {"grants": []}},
            "binding": stored["binding"]
        }
    }))
    .unwrap()
        + "\n"
        + &serde_json::to_string(&json!({
            "sequence": 2,
            "kind": "scenario_terminal",
            "value": {
                "status": "passed",
                "checks": stored["checks"],
                "task_checks": stored["task_checks"],
                "timing_ns": stored["timing_ns"],
                "process_peak_rss": null,
                "artifacts": {}
            }
        }))
        .unwrap()
        + "\n";
    fs::write(
        invalid_dir.join("samples/0002/scenario-a/records.jsonl"),
        records_content,
    )
    .unwrap();

    let sha = format!("{:x}", Sha256::digest(invalid_samples_line.as_bytes()));
    let meta_sha = format!(
        "{:x}",
        Sha256::digest(fs::read(invalid_dir.join("meta.json")).unwrap())
    );
    let summary = json!({
        "schema_version": 2,
        "run_id": sample_val["run_id"],
        "source": "offline synthetic",
        "binding": sample_val["binding"],
        "sampling_stopped": false,
        "issue_2_complete": false,
        "real_provider": {"status": "unverified", "calls": 0, "credentials_read": false},
        "source_bound": false,
        "self_verified_source": false,
        "build_input_verification": {"valid": true, "binary_matches_source_manifest": false, "gaps": []},
        "artifacts": {"meta_sha256": meta_sha, "samples_sha256": sha},
        "sample_count": 1,
        "semantic_passed": false,
        "failed_samples": 0,
        "unsupported_samples": 0,
        "unverified_samples": 0,
        "statuses": [],
        "timing_summary": [],
        "repair_summary": []
    });
    fs::write(
        invalid_dir.join("summary.json"),
        serde_json::to_string(&summary).unwrap(),
    )
    .unwrap();

    let read_result = Bundle::read(&invalid_dir);
    assert!(
        read_result.is_err(),
        "Bundle::read must reject a sample with iteration > meta[samples_per_scenario]"
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(invalid_dir);
}

#[test]
fn tampered_semantic_passed_on_incomplete_bundle_is_rejected() {
    let root = temp_dir("tampered-bundle");
    let meta = valid_meta(&root, 1, &["scenario-a", "scenario-b"]);
    let mut writer = BundleWriter::new(&root, meta, None).unwrap();
    write_sample(&mut writer, "scenario-a", 1);
    writer.finish(false).unwrap();

    // Tamper with summary.json to claim semantic_passed: true
    let summary_path = root.join("summary.json");
    let mut summary: Value = serde_json::from_slice(&fs::read(&summary_path).unwrap()).unwrap();
    summary["semantic_passed"] = true.into();
    fs::write(&summary_path, serde_json::to_string(&summary).unwrap()).unwrap();

    let read_result = Bundle::read(&root);
    assert!(
        read_result.is_err(),
        "Bundle::read must reject a bundle where summary semantic_passed contradicts incomplete scenario coverage"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn compare_reports_incomplete_scenario_repetitions() {
    let root_left = temp_dir("compare-left");
    let root_right = temp_dir("compare-right");

    let meta_left = valid_meta(&root_left, 3, &["scenario-a"]);
    let mut writer_left = BundleWriter::new(&root_left, meta_left, None).unwrap();
    write_sample(&mut writer_left, "scenario-a", 1);
    writer_left.finish(false).unwrap();

    let meta_right = valid_meta(&root_right, 3, &["scenario-a"]);
    let mut writer_right = BundleWriter::new(&root_right, meta_right, None).unwrap();
    write_sample(&mut writer_right, "scenario-a", 1);
    write_sample(&mut writer_right, "scenario-a", 2);
    write_sample(&mut writer_right, "scenario-a", 3);
    writer_right.finish(false).unwrap();

    let bundle_left = Bundle::read(&root_left).unwrap();
    let bundle_right = Bundle::read(&root_right).unwrap();

    let comparison = compare(&bundle_left, &bundle_right);
    assert_ne!(comparison.status, "comparable");
    assert!(
        comparison
            .reasons
            .iter()
            .any(|r| r.contains("incomplete scenario repetitions")),
        "comparison reasons must report incomplete scenario repetitions for left bundle"
    );

    let _ = fs::remove_dir_all(root_left);
    let _ = fs::remove_dir_all(root_right);
}
