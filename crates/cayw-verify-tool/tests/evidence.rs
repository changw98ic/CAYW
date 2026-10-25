use cayw_verify_tool::sdk::ScenarioReport;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::OnceLock,
};

fn directory() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "cw-evidence-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path
}
fn cli(args: &[&str], paths: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cayw-verify-tool"))
        .args(args)
        .args(paths)
        .output()
        .unwrap()
}
fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}
fn baseline() -> &'static Path {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        let root = directory().join("baseline");
        let output = cli(
            &[
                "sdk-ipc",
                "--samples",
                "3",
                "--scenario",
                "sdk-lifecycle",
                "--out",
            ],
            &[&root],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let read = cli(&["read"], &[&root]);
        assert!(
            read.status.success(),
            "baseline rejected: {}",
            String::from_utf8_lossy(&read.stdout)
        );
        root
    })
}
fn luau_baseline() -> &'static Path {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        let root = directory().join("luau");
        let output = cli(&["luau", "--samples", "1", "--out"], &[&root]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let read = cli(&["read"], &[&root]);
        assert!(
            read.status.success(),
            "{}",
            String::from_utf8_lossy(&read.stdout)
        );
        root
    })
}
fn event_baseline() -> &'static Path {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        let root = directory().join("events");
        let output = cli(
            &[
                "sdk-ipc",
                "--samples",
                "1",
                "--scenario",
                "ipc-event-reconnect",
                "--out",
            ],
            &[&root],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let read = cli(&["read"], &[&root]);
        assert!(
            read.status.success(),
            "{}",
            String::from_utf8_lossy(&read.stdout)
        );
        root
    })
}
fn fault_baseline() -> &'static Path {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        let root = directory().join("faults");
        let output = cli(&["sdk-ipc", "--samples", "1", "--out"], &[&root]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let read = cli(&["read"], &[&root]);
        assert!(
            read.status.success(),
            "{}",
            String::from_utf8_lossy(&read.stdout)
        );
        root
    })
}
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}
fn copy() -> PathBuf {
    let path = directory().join("copy");
    copy_tree(baseline(), &path);
    path
}
fn index(root: &Path) -> Vec<Value> {
    fs::read_to_string(root.join("samples.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn write_index(root: &Path, samples: &[Value]) {
    let content = samples
        .iter()
        .map(|sample| serde_json::to_string(sample).unwrap() + "\n")
        .collect::<String>();
    fs::write(root.join("samples.jsonl"), content).unwrap();
    let mut summary = read_json(&root.join("summary.json"));
    summary["artifacts"]["samples_sha256"] = hash(&root.join("samples.jsonl")).into();
    write_json(&root.join("summary.json"), &summary);
}
fn store_sample(root: &Path, sample: &Value) {
    let mut stored = sample.clone();
    stored.as_object_mut().unwrap().remove("run_id");
    stored.as_object_mut().unwrap().remove("meta");
    write_json(
        &root
            .join("samples")
            .join(sample["sample_id"].as_str().unwrap())
            .join("report.json"),
        &stored,
    );
}
fn refresh_sample(root: &Path, sample: &mut Value) {
    let directory = root
        .join("samples")
        .join(sample["sample_id"].as_str().unwrap());
    let content = sample["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| serde_json::to_string(value).unwrap() + "\n")
        .collect::<String>();
    fs::write(directory.join("records.jsonl"), content).unwrap();
    sample["artifacts"]["records"]["sha256"] = hash(&directory.join("records.jsonl")).into();
    store_sample(root, sample);
}
fn visit(value: &mut Value, change: &mut impl FnMut(&mut Value)) {
    change(value);
    match value {
        Value::Object(object) => {
            for child in object.values_mut() {
                visit(child, change);
            }
        }
        Value::Array(array) => {
            for child in array {
                visit(child, change);
            }
        }
        _ => {}
    }
}
fn rejected(root: &Path) {
    let output = cli(&["read"], &[root]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !output.status.success(),
        "damaged evidence accepted with status {}",
        value["status"]
    );
    assert_eq!(value["status"], "invalid");
}

#[test]
fn ipc_lifecycle_requires_its_original_wire_attachment() {
    let baseline = fault_baseline();
    for damage in ["omit-attachment", "replace-attachment", "omit-observation"] {
        let root = directory().join("ipc-wire");
        copy_tree(baseline, &root);
        let mut samples = index(&root);
        let sample = samples
            .iter_mut()
            .find(|sample| sample["scenario"] == "ipc-lifecycle")
            .unwrap();
        let path = root
            .join("samples")
            .join(sample["sample_id"].as_str().unwrap());
        match damage {
            "omit-attachment" => {
                sample["artifacts"]
                    .as_object_mut()
                    .unwrap()
                    .remove("ipc_frames");
            }
            "replace-attachment" => {
                let wire = path.join("ipc-frames.jsonl");
                fs::write(&wire, b"not an IPC journal\n").unwrap();
                sample["artifacts"]["ipc_frames"]["sha256"] = hash(&wire).into();
            }
            "omit-observation" => {
                sample["observations"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|row| row["kind"] != "actual_ipc_frames");
            }
            _ => unreachable!(),
        }
        let mut artifacts = sample["artifacts"].clone();
        artifacts.as_object_mut().unwrap().remove("records");
        for (index, row) in sample["observations"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            row["sequence"] = (index + 1).into();
            if row["kind"] == "scenario_terminal" {
                row["value"]["artifacts"] = artifacts.clone();
            }
        }
        refresh_sample(&root, sample);
        write_index(&root, &samples);
        rejected(&root);
    }
}

#[test]
fn ipc_original_wire_bytes_must_match_the_decoded_frames() {
    let baseline = fault_baseline();
    for scenario in ["ipc-lifecycle", "ipc-event-reconnect"] {
        for damage in [
            "byte-length",
            "byte-digest",
            "decoded-frame",
            "connection",
            "sequence",
        ] {
            let root = directory().join("ipc-bytes");
            copy_tree(baseline, &root);
            let mut samples = index(&root);
            let sample = samples
                .iter_mut()
                .find(|sample| sample["scenario"] == scenario)
                .unwrap();
            let wire = root
                .join("samples")
                .join(sample["sample_id"].as_str().unwrap())
                .join("ipc-frames.jsonl");
            let mut frames: Vec<Value> = fs::read_to_string(&wire)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            match damage {
                "byte-length" => frames[0]["bytes"] = 0.into(),
                "byte-digest" => frames[0]["sha256"] = "0".repeat(64).into(),
                "decoded-frame" => {
                    let raw = b"{}\n";
                    frames[0]["utf8"] = "{}\n".into();
                    frames[0]["bytes"] = raw.len().into();
                    frames[0]["sha256"] = format!("{:x}", Sha256::digest(raw)).into();
                }
                "connection" => frames[0]["connection"] = Value::Null,
                "sequence" => frames[0]["sequence"] = 99.into(),
                _ => unreachable!(),
            }
            let bytes = frames
                .iter()
                .map(|frame| serde_json::to_string(frame).unwrap() + "\n")
                .collect::<String>();
            fs::write(&wire, bytes).unwrap();
            sample["artifacts"]["ipc_frames"]["sha256"] = hash(&wire).into();
            let mut artifacts = sample["artifacts"].clone();
            artifacts.as_object_mut().unwrap().remove("records");
            for row in sample["observations"].as_array_mut().unwrap() {
                if row["kind"] == "actual_ipc_frames" {
                    row["value"] = json!(frames);
                }
                if row["kind"] == "scenario_terminal" {
                    row["value"]["artifacts"] = artifacts.clone();
                }
            }
            refresh_sample(&root, sample);
            write_index(&root, &samples);
            rejected(&root);
        }
    }
}

#[test]
fn ipc_lifecycle_wire_must_retain_its_original_requests_replies_and_pages() {
    let baseline = fault_baseline();
    for damage in [
        "empty",
        "unrelated",
        "prompt-input",
        "prompt-reply",
        "event-content",
        "missing-reply",
        "auth-token",
        "auth-result",
        "request-order",
    ] {
        let root = directory().join("ipc-lifecycle-content");
        copy_tree(baseline, &root);
        let mut samples = index(&root);
        let sample = samples
            .iter_mut()
            .find(|sample| sample["scenario"] == "ipc-lifecycle")
            .unwrap();
        let wire = root
            .join("samples")
            .join(sample["sample_id"].as_str().unwrap())
            .join("ipc-frames.jsonl");
        let mut frames: Vec<Value> = fs::read_to_string(&wire)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let request = |method: &str| {
            frames
                .iter()
                .position(|frame| {
                    frame["direction"] == "client_to_server" && frame["frame"]["method"] == method
                })
                .unwrap()
        };
        let hello = request("hello");
        let prompt = request("session.prompt");
        let page = request("session.events");
        let reply = |original: usize| {
            frames
                .iter()
                .position(|frame| {
                    frame["direction"] == "server_to_client"
                        && frame["connection"] == frames[original]["connection"]
                        && frame["frame"]["id"] == frames[original]["frame"]["id"]
                })
                .unwrap()
        };
        let hello_reply = reply(hello);
        let prompt_reply = reply(prompt);
        let page_reply = reply(page);
        match damage {
            "empty" => frames.clear(),
            "unrelated" => {
                frames.truncate(1);
                frames[0]["frame"] = json!({});
            }
            "prompt-input" => frames[prompt]["frame"]["params"]["text"] = "changed".into(),
            "prompt-reply" => {
                frames[prompt_reply]["frame"]["result"]["label"] = "changed".into();
            }
            "event-content" => {
                frames[page_reply]["frame"]["result"][0]["timestamp"] = 0.into();
            }
            "missing-reply" => {
                frames.remove(prompt_reply);
            }
            "auth-token" => frames[hello]["frame"]["params"]["token"] = "wrong".into(),
            "auth-result" => {
                frames[hello_reply]["frame"]["result"]["version"] = 0.into();
            }
            "request-order" => frames.swap(prompt, prompt_reply),
            _ => unreachable!(),
        }
        for (index, frame) in frames.iter_mut().enumerate() {
            frame["sequence"] = (index + 1).into();
            let mut raw = serde_json::to_vec(&frame["frame"]).unwrap();
            if frame["direction"] != "relay_terminal" {
                raw.push(b'\n');
            }
            frame["bytes"] = raw.len().into();
            frame["sha256"] = format!("{:x}", Sha256::digest(&raw)).into();
            frame["utf8"] = String::from_utf8(raw).unwrap().into();
        }
        let bytes = frames
            .iter()
            .map(|frame| serde_json::to_string(frame).unwrap() + "\n")
            .collect::<String>();
        fs::write(&wire, bytes).unwrap();
        sample["artifacts"]["ipc_frames"]["sha256"] = hash(&wire).into();
        let mut artifacts = sample["artifacts"].clone();
        artifacts.as_object_mut().unwrap().remove("records");
        for row in sample["observations"].as_array_mut().unwrap() {
            if row["kind"] == "actual_ipc_frames" {
                row["value"] = json!(frames);
            }
            if row["kind"] == "scenario_terminal" {
                row["value"]["artifacts"] = artifacts.clone();
            }
        }
        refresh_sample(&root, sample);
        write_index(&root, &samples);
        rejected(&root);
    }
}

#[test]
fn ipc_session_creation_events_must_match_the_original_snapshots() {
    let baseline = fault_baseline();
    let mut accepted = Vec::new();
    for scenario in [
        "ipc-lifecycle",
        "ipc-event-reconnect",
        "ipc-reconnect",
        "ipc-cancel",
    ] {
        let root = directory().join("ipc-session-origin");
        copy_tree(baseline, &root);
        let mut samples = index(&root);
        let sample = samples
            .iter_mut()
            .find(|sample| sample["scenario"] == scenario)
            .unwrap();
        let snapshots = |sample: &Value| {
            sample["observations"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["kind"].as_str().unwrap().ends_with("_session"))
                .cloned()
                .collect::<Vec<_>>()
        };
        let original_snapshots = snapshots(sample);
        assert!(!original_snapshots.is_empty());
        let mut changes = 0;
        let mut alter = |value: &mut Value| {
            if value["event"]["type"] == "session_created" {
                value["event"]["label"] = "altered event with original snapshots retained".into();
                changes += 1;
            }
        };
        visit(&mut sample["event_pages"], &mut alter);
        visit(&mut sample["observations"], &mut alter);
        let wire = root
            .join("samples")
            .join(sample["sample_id"].as_str().unwrap())
            .join("ipc-frames.jsonl");
        let mut frames: Vec<Value> = fs::read_to_string(&wire)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for frame in &mut frames {
            visit(&mut frame["frame"], &mut alter);
            let mut raw = serde_json::to_vec(&frame["frame"]).unwrap();
            if frame["direction"] != "relay_terminal" {
                raw.push(b'\n');
            }
            frame["bytes"] = raw.len().into();
            frame["sha256"] = format!("{:x}", Sha256::digest(&raw)).into();
            frame["utf8"] = String::from_utf8(raw).unwrap().into();
        }
        assert!(changes >= 3);
        let bytes = frames
            .iter()
            .map(|frame| serde_json::to_string(frame).unwrap() + "\n")
            .collect::<String>();
        fs::write(&wire, bytes).unwrap();
        sample["artifacts"]["ipc_frames"]["sha256"] = hash(&wire).into();
        let mut artifacts = sample["artifacts"].clone();
        artifacts.as_object_mut().unwrap().remove("records");
        for row in sample["observations"].as_array_mut().unwrap() {
            if row["kind"] == "actual_ipc_frames" {
                row["value"] = json!(frames);
            }
            if row["kind"] == "scenario_terminal" {
                row["value"]["artifacts"] = artifacts.clone();
            }
        }
        assert_eq!(snapshots(sample), original_snapshots);
        refresh_sample(&root, sample);
        write_index(&root, &samples);
        let output = cli(&["read"], &[&root]);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        if output.status.success() {
            accepted.push(scenario);
        } else {
            assert_eq!(result["status"], "invalid", "{scenario}: {result}");
        }
    }
    assert!(
        accepted.is_empty(),
        "contradictory session creation events accepted: {accepted:?}"
    );
}

#[test]
fn build_completeness_claims_require_the_measured_field_types() {
    for (field, value) in [
        ("build_input_completeness", json!(true)),
        ("build_input_completeness", json!(0)),
        ("build_input_completeness", json!({"status":"pending"})),
        ("build_input_completeness", Value::Null),
        ("source_verified_by_binary", json!("false")),
        ("source_verified_by_binary", json!(0)),
        ("source_verified_by_binary", Value::Null),
    ] {
        let root = copy();
        let mut binding = read_json(&root.join("meta.json"))["binding"].clone();
        binding["build"][field] = value;
        replace_binding(&root, &binding);
        rejected(&root);
    }
}

#[test]
fn missing_named_task_check_cannot_become_a_pass() {
    let mut report = ScenarioReport::new("custom", 1, json!({}), json!({}));
    report.check("cleanup", true, "stopped");
    report.task_checks.push("no-such-check".into());
    report.finish();
    assert_eq!(report.status, "unverified");
}

#[test]
fn summary_statistics_are_recomputed_from_raw_samples() {
    let root = copy();
    let mut summary = read_json(&root.join("summary.json"));
    summary["timing_summary"][0]["median"] = 0.into();
    write_json(&root.join("summary.json"), &summary);
    rejected(&root);
}

#[test]
fn listed_source_hashes_do_not_prove_complete_build_inputs() {
    let output = cli(&["compare"], &[baseline(), baseline()]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !output.status.success(),
        "listed inputs were incorrectly accepted as complete build evidence"
    );
    assert_eq!(value["status"], "pending");
}

/// A known failed task adds no missing-evidence reason. This fixture has no
/// build receipts, so its independent provenance gaps remain the same as the
/// passing control. Failure facts and measurements remain intact.
#[test]
fn known_failed_task_adds_no_comparison_gate_and_keeps_failure_evidence() {
    let failed = directory().join("known-failed");
    copy_tree(baseline(), &failed);
    let mut samples = index(&failed);
    let sample = &mut samples[0];
    let checks = sample["checks"].as_array_mut().unwrap();
    let check = checks
        .iter_mut()
        .find(|check| check["passed"] == true)
        .expect("baseline sample has a passing check");
    check["passed"] = false.into();
    let checks = sample["checks"].clone();
    sample["status"] = "failed".into();
    let terminal = sample["observations"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    assert_eq!(terminal["kind"], "scenario_terminal");
    terminal["value"]["status"] = "failed".into();
    terminal["value"]["checks"] = checks;
    refresh_sample(&failed, sample);
    write_index(&failed, &samples);
    let mut summary = read_json(&failed.join("summary.json"));
    summary["statuses"] = json!(
        samples
            .iter()
            .map(|sample| json!({
                "sample_id": sample["sample_id"],
                "status": sample["status"],
                "timing_ns": sample["timing_ns"],
                "failed_checks": sample["checks"].as_array().unwrap().iter()
                    .filter(|check| check["passed"] != true).collect::<Vec<_>>()
            }))
            .collect::<Vec<_>>()
    );
    summary["failed_samples"] = samples
        .iter()
        .filter(|sample| sample["status"] == "failed")
        .count()
        .into();
    summary["semantic_passed"] = false.into();
    write_json(&failed.join("summary.json"), &summary);
    let read = cli(&["read"], &[&failed]);
    assert!(
        read.status.success(),
        "known failure must remain structurally valid: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    let passing_output = cli(&["compare"], &[baseline(), baseline()]);
    let passing: Value = serde_json::from_slice(&passing_output.stdout).unwrap();
    let output = cli(&["compare"], &[baseline(), &failed]);
    let comparison: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(passing["status"], "pending");
    assert_eq!(output.status.success(), passing_output.status.success());
    assert_eq!(comparison["status"], passing["status"]);
    assert_eq!(comparison["reasons"], passing["reasons"]);
    assert_eq!(comparison["measurements"], passing["measurements"]);
    assert!(
        comparison["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reason| {
                !reason
                    .as_str()
                    .unwrap()
                    .contains("task has no passing result")
            })
    );
    let kept = read_json(&failed.join("summary.json"));
    assert_eq!(kept["failed_samples"], 1);
    assert_eq!(kept["semantic_passed"], false);
    assert!(comparison["measurements"]["left"].as_array().is_some());
    assert!(comparison["measurements"]["right"].as_array().is_some());
}

fn replace_binding(root: &Path, binding: &Value) {
    let mut meta = read_json(&root.join("meta.json"));
    meta["binding"] = binding.clone();
    write_json(&root.join("meta.json"), &meta);
    let mut summary = read_json(&root.join("summary.json"));
    summary["binding"] = binding.clone();
    summary["artifacts"]["meta_sha256"] = hash(&root.join("meta.json")).into();
    write_json(&root.join("summary.json"), &summary);
    let mut samples = index(root);
    for sample in &mut samples {
        sample["binding"] = binding.clone();
        sample["observations"][0]["value"]["binding"] = binding.clone();
        refresh_sample(root, sample);
    }
    write_index(root, &samples);
}

#[test]
fn a_source_verified_flag_cannot_promote_an_untraced_build() {
    let root = copy();
    let mut binding = read_json(&root.join("meta.json"))["binding"].clone();
    binding["build"]["source_verified_by_binary"] = true.into();
    replace_binding(&root, &binding);
    let mut summary = read_json(&root.join("summary.json"));
    summary["source_bound"] = true.into();
    write_json(&root.join("summary.json"), &summary);
    let output = cli(&["compare"], &[&root, &root]);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !output.status.success(),
        "a caller flag incorrectly promoted an incomplete build: {}",
        result["status"]
    );
    assert_eq!(result["status"], "invalid");
}

#[test]
fn a_declared_build_receipt_is_required_and_rechecked() {
    for damage in ["missing", "contradictory"] {
        let root = copy();
        let mut meta = read_json(&root.join("meta.json"));
        let path = root.join("build-receipt.json");
        if damage == "contradictory" {
            write_json(
                &path,
                &json!({"schema":"cayw.build-receipt.v1", "complete":true}),
            );
        }
        meta["artifacts"]["build_receipt"] = json!({"path":"build-receipt.json", "sha256":if path.exists() {hash(&path)} else {"0".repeat(64)}});
        write_json(&root.join("meta.json"), &meta);
        let mut summary = read_json(&root.join("summary.json"));
        summary["artifacts"]["meta_sha256"] = hash(&root.join("meta.json")).into();
        write_json(&root.join("summary.json"), &summary);
        rejected(&root);
    }
}

#[test]
fn build_receipts_are_explicit_inputs_and_invalid_ones_do_not_start_sampling() {
    for mode in ["sdk-ipc", "luau"] {
        let parent = directory();
        let receipt = parent.join("receipt.json");
        write_json(
            &receipt,
            &json!({"schema":"cayw.build-receipt.v1", "complete":true}),
        );
        let out = parent.join("run");
        let output = Command::new(env!("CARGO_BIN_EXE_cayw-verify-tool"))
            .args([mode, "--samples", "1", "--build-receipt"])
            .arg(&receipt)
            .arg("--out")
            .arg(&out)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            !out.exists(),
            "invalid source evidence must fail before output is allocated"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("unknown or repeated option"),
            "receipt option was not parsed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn changing_only_measured_startup_duration_does_not_change_comparison_conditions() {
    let other = directory().join("luau-independent");
    let run = cli(&["luau", "--samples", "1", "--out"], &[&other]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let result = cli(&["compare"], &[luau_baseline(), &other]);
    let comparison: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        comparison["status"], "pending",
        "actual timing observations are not experiment conditions: {}",
        comparison["reasons"]
    );
    assert!(
        !comparison["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "different measurement")
    );
}

#[test]
fn different_compilation_conditions_are_not_comparable() {
    for field in ["profile", "toolchain", "build_target", "opt_level"] {
        let root = copy();
        let mut binding = read_json(&root.join("meta.json"))["binding"].clone();
        binding["build"][field] = "different-compile-condition".into();
        replace_binding(&root, &binding);
        let output = cli(&["compare"], &[baseline(), &root]);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            result["status"], "incomparable",
            "known build condition {field} was ignored: {}",
            result["reasons"]
        );
    }
}

#[test]
fn all_repair_attempt_costs_are_recomputed_without_zeroing_unknown_usage() {
    let root = directory().join("repair-cost");
    copy_tree(luau_baseline(), &root);
    let samples = index(&root);
    let summary = read_json(&root.join("summary.json"));
    let repairs = summary["repair_summary"]
        .as_array()
        .expect("repair aggregate absent");
    assert_eq!(repairs.len(), 1);
    let repair = &repairs[0];
    assert_eq!(repair["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(repair["attempts"][0]["execution_status"], "error");
    assert_eq!(repair["attempts"][0]["error"]["kind"], "script");
    assert_eq!(repair["attempts"][1]["execution_status"], "success");
    let expected = samples
        .iter()
        .filter(|sample| {
            matches!(
                sample["scenario"].as_str(),
                Some("syntax_failure" | "syntax_repair_success")
            )
        })
        .map(|sample| sample["timing_ns"]["case_execute_ns"].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(repair["total_case_execute_ns"], expected);
    assert_eq!(repair["usage"]["status"], "not_measured");
    assert!(repair["usage"]["total_tokens"].is_null());
    assert_eq!(repair["cost"]["status"], "not_measured");
    assert!(repair["cost"]["amount"].is_null());
    let mut damaged = summary;
    damaged["repair_summary"][0]["total_case_execute_ns"] = 0.into();
    write_json(&root.join("summary.json"), &damaged);
    rejected(&root);
}

#[test]
fn repair_attempt_observations_cannot_change_or_omit_original_costs() {
    for damage in [
        "omit-attempt",
        "duplicate-attempt",
        "zero-duration",
        "invent-usage",
        "invent-cost",
    ] {
        let root = directory().join("repair-record");
        copy_tree(luau_baseline(), &root);
        let mut samples = index(&root);
        let sample = samples
            .iter_mut()
            .find(|sample| sample["scenario"] == "syntax_failure")
            .unwrap();
        let records = sample["observations"].as_array_mut().unwrap();
        let attempt = records
            .iter()
            .position(|row| row["kind"] == "repair_attempt")
            .unwrap();
        match damage {
            "omit-attempt" => {
                records.remove(attempt);
            }
            "duplicate-attempt" => {
                records.insert(attempt, records[attempt].clone());
            }
            "zero-duration" => records[attempt]["value"]["case_execute_ns"] = 0.into(),
            "invent-usage" => {
                records[attempt]["value"]["usage"] = json!({"status":"measured","total_tokens":0})
            }
            "invent-cost" => {
                records[attempt]["value"]["cost"] =
                    json!({"status":"measured","amount":0,"currency":"USD"})
            }
            _ => unreachable!(),
        }
        for (sequence, row) in records.iter_mut().enumerate() {
            row["sequence"] = (sequence + 1).into();
        }
        refresh_sample(&root, sample);
        write_index(&root, &samples);
        rejected(&root);
    }
}

#[test]
fn cold_luau_requires_its_original_process_receipt() {
    for damage in [
        "missing-receipt",
        "wrong-process",
        "cleanup-unknown",
        "output-altered",
    ] {
        let root = directory().join("cold-receipt");
        copy_tree(luau_baseline(), &root);
        let mut samples = index(&root);
        let sample = samples
            .iter_mut()
            .find(|sample| sample["scenario"] == "cold_process_first_success")
            .unwrap();
        let path = root
            .join("samples")
            .join(sample["sample_id"].as_str().unwrap())
            .join("child.json");
        if damage == "missing-receipt" {
            sample["artifacts"].as_object_mut().unwrap().remove("child");
            fs::remove_file(&path).unwrap();
        } else {
            let mut receipt = read_json(&path);
            match damage {
                "wrong-process" => receipt["process_id"] = 0.into(),
                "cleanup-unknown" => receipt["cleanup_confirmed"] = false.into(),
                "output-altered" => receipt["stdout"] = json!([]),
                _ => unreachable!(),
            }
            write_json(&path, &receipt);
            sample["artifacts"]["child"]["sha256"] = hash(&path).into();
        }
        let mut artifacts = sample["artifacts"].clone();
        artifacts.as_object_mut().unwrap().remove("records");
        sample["observations"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["value"]["artifacts"] = artifacts;
        refresh_sample(&root, sample);
        write_index(&root, &samples);
        rejected(&root);
    }
}

#[test]
fn missing_measurements_do_not_produce_a_comparable_result() {
    let root = copy();
    let mut summary = read_json(&root.join("summary.json"));
    summary.as_object_mut().unwrap().remove("timing_summary");
    write_json(&root.join("summary.json"), &summary);
    let result = cli(&["compare"], &[baseline(), &root]);
    assert!(!result.status.success());
    let value: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_ne!(value["status"], "comparable");
}

#[test]
fn sample_specification_must_match_meta_and_its_original_start_record() {
    let root = copy();
    let mut samples = index(&root);
    samples[0]["specification"]["prompt"] = "different task".into();
    samples[0]["specification"]["permissions"]["grants"] = json!([]);
    samples[0]["task_checks"] = json!(["not-a-real-check"]);
    store_sample(&root, &samples[0]);
    write_index(&root, &samples);
    rejected(&root);
}

#[test]
fn provider_request_journal_is_a_required_bound_attachment() {
    let root = copy();
    fs::remove_file(root.join("samples/0001/sdk-lifecycle/provider.jsonl")).unwrap();
    rejected(&root);
}

#[test]
fn complete_output_pages_are_revalidated_even_if_internal_hashes_are_updated() {
    fn make_unread(value: &mut Value) {
        match value {
            Value::Object(object) => {
                if object.contains_key("cell_id") && object.contains_key("has_more") {
                    object.insert("has_more".into(), true.into());
                }
                for value in object.values_mut() {
                    make_unread(value);
                }
            }
            Value::Array(array) => {
                for value in array {
                    make_unread(value);
                }
            }
            _ => {}
        }
    }
    let root = copy();
    let mut samples = index(&root);
    for sample in &mut samples {
        make_unread(sample);
        let directory = root
            .join("samples")
            .join(sample["sample_id"].as_str().unwrap());
        let content = sample["observations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| serde_json::to_string(value).unwrap() + "\n")
            .collect::<String>();
        fs::write(directory.join("records.jsonl"), content).unwrap();
        sample["artifacts"]["records"]["sha256"] = hash(&directory.join("records.jsonl")).into();
        store_sample(&root, sample);
    }
    write_index(&root, &samples);
    rejected(&root);
}

#[test]
fn tool_end_must_match_the_complete_original_call() {
    for field in ["arguments", "name"] {
        let root = copy();
        let mut samples = index(&root);
        let mut changed = 0;
        for sample in &mut samples {
            visit(sample, &mut |value| {
                if value["type"] == "tool_execution_end" {
                    value["call"][field] = if field == "name" {
                        json!("different-tool")
                    } else {
                        json!({"action":"exec","script":"return 'different task'"})
                    };
                    changed += 1;
                }
            });
            refresh_sample(&root, sample);
        }
        assert!(changed > 0);
        write_index(&root, &samples);
        rejected(&root);
    }
}

#[test]
fn tool_results_must_belong_to_the_original_attempt() {
    let root = copy();
    let mut samples = index(&root);
    let mut changed = 0;
    for sample in &mut samples {
        visit(sample, &mut |value| {
            if value["event"]["type"] == "agent"
                && value["event"]["event"]["type"] == "tool_execution_end"
            {
                value["attempt"]["attempt_id"] = "unrelated-attempt".into();
                changed += 1;
            }
        });
        refresh_sample(&root, sample);
    }
    assert!(changed > 0);
    write_index(&root, &samples);
    rejected(&root);
}

#[test]
fn provider_history_must_retain_the_executed_call_and_result() {
    let root = copy();
    let mut samples = index(&root);
    let sample = &mut samples[0];
    let path = root
        .join("samples")
        .join(sample["sample_id"].as_str().unwrap())
        .join("provider.jsonl");
    let mut journal: Vec<Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut changed = 0;
    for entry in &mut journal {
        visit(&mut entry["request"], &mut |value| {
            if value["type"] == "toolCall" {
                value["arguments"]["script"] = "return 'another task'".into();
                changed += 1;
            }
        });
        let request: cayw_ai::ModelRequest =
            serde_json::from_value(entry["request"].clone()).unwrap();
        entry["request_sha256"] = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&request).unwrap())
        )
        .into();
    }
    assert!(changed > 0);
    fs::write(
        &path,
        journal
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    sample["artifacts"]["provider"]["sha256"] = hash(&path).into();
    sample["observations"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap()["value"]["artifacts"]["provider"] = sample["artifacts"]["provider"].clone();
    refresh_sample(&root, sample);
    write_index(&root, &samples);
    rejected(&root);
}

#[test]
fn deleting_the_luau_attachment_declaration_cannot_skip_validation() {
    let root = directory().join("luau-copy");
    copy_tree(luau_baseline(), &root);
    let mut samples = index(&root);
    let sample = samples
        .iter_mut()
        .find(|sample| sample["scenario"] == "file_pipeline")
        .unwrap();
    sample["artifacts"].as_object_mut().unwrap().remove("luau");
    sample["observations"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap()["value"]["artifacts"]
        .as_object_mut()
        .unwrap()
        .remove("luau");
    fs::remove_file(
        root.join("samples")
            .join(sample["sample_id"].as_str().unwrap())
            .join("luau.json"),
    )
    .unwrap();
    refresh_sample(&root, sample);
    write_index(&root, &samples);
    rejected(&root);
}

#[test]
fn luau_and_sdk_reports_use_the_same_reader() {
    let sdk = cli(&["read"], &[baseline()]);
    assert!(sdk.status.success());
    let out = directory().join("luau");
    let run = cli(&["luau", "--samples", "1", "--out"], &[&out]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let read = cli(&["read"], &[&out]);
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stdout)
    );
    let mut samples = index(&out);
    assert_eq!(samples[0]["schema_version"], 2);
    samples[0]["artifacts"]["luau"]["sha256"] = "0".repeat(64).into();
    store_sample(&out, &samples[0]);
    write_index(&out, &samples);
    rejected(&out);
}

#[test]
fn event_reconnect_requires_its_cursor_wire_and_complete_original_suffix() {
    for damage in [
        "missing-cursor",
        "wrong-saved-cursor",
        "missing-frames",
        "missing-suffix",
        "shared-connection",
    ] {
        let root = directory().join("events-copy");
        copy_tree(event_baseline(), &root);
        let mut samples = index(&root);
        let sample = &mut samples[0];
        let sample_root = root
            .join("samples")
            .join(sample["sample_id"].as_str().unwrap());
        match damage {
            "missing-cursor" | "missing-frames" => {
                let key = if damage == "missing-cursor" {
                    "cursor"
                } else {
                    "ipc_frames"
                };
                let path = sample["artifacts"][key]["path"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                sample["artifacts"].as_object_mut().unwrap().remove(key);
                fs::remove_file(sample_root.join(path)).unwrap();
            }
            "wrong-saved-cursor" => {
                let path = sample_root.join("cursor.json");
                let mut saved = read_json(&path);
                saved["cursor"] = (saved["cursor"].as_u64().unwrap() + 1).into();
                write_json(&path, &saved);
                sample["artifacts"]["cursor"]["sha256"] = hash(&path).into();
            }
            "missing-suffix" => {
                sample["event_pages"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|page| page["stage"] != "event_connection_resumed");
                sample["observations"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|row| {
                        row["kind"] != "event_page"
                            || row["value"]["stage"] != "event_connection_resumed"
                    });
                for (sequence, row) in sample["observations"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .enumerate()
                {
                    row["sequence"] = (sequence + 1).into();
                }
            }
            "shared-connection" => {
                let path = sample_root.join("ipc-frames.jsonl");
                let mut frames: Vec<Value> = fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                let execution = frames
                    .iter()
                    .find(|frame| frame["frame"]["method"] == "session.prompt")
                    .unwrap()["connection"]
                    .clone();
                for frame in &mut frames {
                    frame["connection"] = execution.clone();
                }
                fs::write(
                    &path,
                    frames
                        .iter()
                        .map(|frame| serde_json::to_string(frame).unwrap() + "\n")
                        .collect::<String>(),
                )
                .unwrap();
                sample["artifacts"]["ipc_frames"]["sha256"] = hash(&path).into();
                sample["observations"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|row| row["kind"] == "actual_ipc_frames")
                    .unwrap()["value"] = json!(frames);
            }
            _ => unreachable!(),
        }
        let artifacts = sample["artifacts"].clone();
        sample["observations"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["value"]["artifacts"] = artifacts;
        sample["observations"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["value"]["artifacts"]
            .as_object_mut()
            .unwrap()
            .remove("records");
        refresh_sample(&root, sample);
        write_index(&root, &samples);
        rejected(&root);
    }
}

#[test]
fn fault_tasks_cannot_lose_their_original_event_content_or_terminal_state() {
    for (scenario, damage) in [
        ("ipc-reconnect", "attempt"),
        ("ipc-cancel", "task"),
        ("host-restart-before-dispatch", "task"),
        ("host-restart-unknown", "capability"),
    ] {
        let root = directory().join(format!("{scenario}-{damage}"));
        copy_tree(fault_baseline(), &root);
        let mut samples = index(&root);
        let sample = samples
            .iter_mut()
            .find(|sample| sample["scenario"] == scenario)
            .unwrap();
        let mut changed = 0;
        if damage == "task" {
            for row in sample["observations"].as_array_mut().unwrap() {
                if row["kind"] == "converged_tasks" || row["kind"] == "after_recover_tasks" {
                    row["value"][0]["state"] = "completed".into();
                    changed += 1;
                }
            }
        } else {
            visit(sample, &mut |value| {
                if value["cursor"].is_u64()
                    && value["event"].is_object()
                    && value["attempt"].is_object()
                {
                    if damage == "attempt" {
                        value["attempt"]["attempt_id"] = "different-original-attempt".into();
                        changed += 1;
                    } else if value["event"]["type"] == "attempt_recovered" {
                        value["event"]["capabilities"][0]["arguments"] =
                            json!({"query":"different-effect"});
                        changed += 1;
                    }
                }
            });
        }
        assert!(
            changed > 0,
            "{scenario}/{damage} did not alter an original record"
        );
        refresh_sample(&root, sample);
        write_index(&root, &samples);
        rejected(&root);
    }
}

#[test]
fn cold_and_restart_samples_require_original_child_and_fault_attachments() {
    for (scenario, key) in [
        ("cold-sdk", "cold_report"),
        ("cold-sdk", "child_exit"),
        ("host-restart-before-dispatch", "fault_marker"),
        ("host-restart-unknown", "fault_ready"),
        ("host-restart-unknown", "child_exit"),
    ] {
        let root = directory().join(format!("{scenario}-{key}"));
        copy_tree(fault_baseline(), &root);
        let mut samples = index(&root);
        let sample = samples
            .iter_mut()
            .find(|sample| sample["scenario"] == scenario)
            .unwrap();
        let path = sample["artifacts"][key]["path"]
            .as_str()
            .unwrap()
            .to_owned();
        sample["artifacts"].as_object_mut().unwrap().remove(key);
        sample["observations"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["value"]["artifacts"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        fs::remove_file(
            root.join("samples")
                .join(sample["sample_id"].as_str().unwrap())
                .join(path),
        )
        .unwrap();
        refresh_sample(&root, sample);
        write_index(&root, &samples);
        rejected(&root);
    }
}

#[test]
fn syntax_repair_must_keep_the_original_failure_and_same_workspace_pair() {
    for damage in ["previous-id", "workspace", "remove-original"] {
        let root = directory().join("repair-copy");
        copy_tree(luau_baseline(), &root);
        let mut samples = index(&root);
        let sample = samples
            .iter_mut()
            .find(|sample| sample["scenario"] == "syntax_repair_success")
            .unwrap();
        let path = root
            .join("samples")
            .join(sample["sample_id"].as_str().unwrap())
            .join("luau.json");
        let mut raw = read_json(&path);
        match damage {
            "previous-id" => raw["repair"]["previous_sample_id"] = "0001/another_failure".into(),
            "workspace" => raw["workspace"] = "another-workspace".into(),
            "remove-original" => {}
            _ => unreachable!(),
        }
        write_json(&path, &raw);
        sample["artifacts"]["luau"]["sha256"] = hash(&path).into();
        sample["observations"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["value"]["artifacts"]["luau"] = sample["artifacts"]["luau"].clone();
        refresh_sample(&root, sample);
        if damage == "remove-original" {
            samples.retain(|sample| sample["scenario"] != "syntax_failure");
            let mut summary = read_json(&root.join("summary.json"));
            summary["sample_count"] = samples.len().into();
            summary.as_object_mut().unwrap().remove("statuses");
            summary.as_object_mut().unwrap().remove("timing_summary");
            write_json(&root.join("summary.json"), &summary);
        }
        write_index(&root, &samples);
        rejected(&root);
    }
}

#[test]
fn attachments_cannot_escape_through_parent_paths_or_symlinks() {
    let root = copy();
    let mut samples = index(&root);
    samples[0]["artifacts"]["records"]["path"] = "../../../../outside".into();
    store_sample(&root, &samples[0]);
    write_index(&root, &samples);
    rejected(&root);
    let root = copy();
    let records = root.join("samples/0001/sdk-lifecycle/records.jsonl");
    let outside = root.parent().unwrap().join("outside.jsonl");
    fs::rename(&records, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &records).unwrap();
    rejected(&root);
}
