use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-sdk-cli-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn cli(out: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cayw-harness"))
        .arg("sdk-ipc")
        .arg("--out")
        .arg(out)
        .args(args)
        .output()
        .unwrap()
}
fn json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

#[test]
fn full_sdk_ipc_suite_records_real_faults_and_original_call_identity() {
    let temp = Temporary::new();
    let out = temp.0.join("positive");
    let result = cli(&out, &["--samples", "1"]);
    let summary = json(&out.join("summary.json"));
    assert!(
        result.status.success(),
        "exit {:?}; stderr {}; summary {}",
        result.status.code(),
        String::from_utf8_lossy(&result.stderr),
        summary
    );
    assert!(result.stderr.is_empty());
    assert_eq!(summary["semantic_passed"], true);
    assert_eq!(summary["sample_count"], 7);
    assert_eq!(summary["issue_2_complete"], false);
    assert_eq!(summary["self_verified_source"], false);
    let meta = json(&out.join("meta.json"));
    assert_eq!(meta["real_provider"]["calls"], 0);
    assert_eq!(meta["real_provider"]["status"], "unverified");
    assert_eq!(
        meta["binding"]["executable"]["sha256"],
        hash(Path::new(env!("CARGO_BIN_EXE_cayw-harness")))
    );
    assert_eq!(
        summary["artifacts"]["samples_sha256"],
        hash(&out.join("samples.jsonl"))
    );
    assert_eq!(
        summary["artifacts"]["meta_sha256"],
        hash(&out.join("meta.json"))
    );
    let samples = lines(&out.join("samples.jsonl"));
    assert_eq!(samples.len(), 7);
    for sample in &samples {
        assert_eq!(sample["status"], "passed", "{sample}");
        assert_eq!(sample["run_id"], meta["run_id"]);
        assert!(sample["timing_ns"]["scenario_total"].is_u64());
        let root = out
            .join("samples/0001")
            .join(sample["scenario"].as_str().unwrap());
        let raw = lines(&root.join("records.jsonl"));
        assert_eq!(raw, sample["observations"].as_array().unwrap().clone());
        assert_eq!(
            sample["artifacts"]["records"]["sha256"],
            hash(&root.join("records.jsonl"))
        );
        assert!(sample["specification"]["permissions"]["grants"].is_array());
        assert_eq!(
            sample["specification"]["permissions"]["product_user_approval"],
            false
        );
        for (index, record) in raw.iter().enumerate() {
            assert_eq!(record["sequence"], index + 1);
        }
    }
    let unknown = samples
        .iter()
        .find(|s| s["scenario"] == "host-restart-unknown")
        .unwrap();
    for name in [
        "original_attempt_running",
        "effect_after_original_durable_start",
        "unknown_blocks_new_work",
        "durable_effect_not_repeated",
        "unknown_survives_another_reopen",
        "no_provider_replay_across_recovery",
    ] {
        assert!(
            unknown["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == name && c["passed"] == true),
            "{name}: {unknown}"
        );
    }
    for platform in summary["platforms"].as_array().unwrap() {
        if platform["os"] != std::env::consts::OS {
            assert_eq!(platform["status"], "unverified");
        }
    }
}

#[test]
fn wrong_fixture_and_skipped_saved_cursor_both_exit_failure_with_raw_reports() {
    let temp = Temporary::new();
    for (control, scenario, expected) in [
        ("wrong-script", "sdk-lifecycle", "actual_pipeline_effect"),
        (
            "skip-cursor",
            "ipc-reconnect",
            "saved_cursor_resume_is_exact",
        ),
    ] {
        let out = temp.0.join(control);
        let result = cli(
            &out,
            &[
                "--samples",
                "1",
                "--scenario",
                scenario,
                "--negative-control",
                control,
            ],
        );
        assert!(
            !result.status.success(),
            "negative control was accepted: {control}"
        );
        assert!(
            result.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let summary = json(&out.join("summary.json"));
        assert_eq!(summary["semantic_passed"], false);
        assert_eq!(summary["failed_samples"], 1);
        let samples = lines(&out.join("samples.jsonl"));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0]["status"], "failed");
        assert!(
            samples[0]["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == expected && c["passed"] == false),
            "{samples:?}"
        );
    }
}

#[test]
fn sdk_options_reject_existing_outputs_invalid_counts_and_duplicate_flags() {
    let temp = Temporary::new();
    let out = temp.0.join("existing");
    fs::create_dir(&out).unwrap();
    fs::write(out.join("sentinel"), "keep").unwrap();
    assert!(!cli(&out, &["--samples", "1"]).status.success());
    assert_eq!(fs::read(out.join("sentinel")).unwrap(), b"keep");
    for args in [
        vec!["--samples", "0"],
        vec!["--samples", "1001"],
        vec!["--samples", "1", "--samples", "1"],
        vec![
            "--samples",
            "1",
            "--scenario",
            "ipc-cancel",
            "--negative-control",
            "wrong-script",
        ],
        vec!["--samples"],
    ] {
        let new = temp.0.join("must-not-exist");
        assert!(!cli(&new, &args).status.success());
        assert!(!new.exists());
    }
}
