use cayw_verify_tool::{
    cases::{CASES, Case},
    runner::{Sample, revalidate, run_case},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-luau-measurements-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
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
fn declared(name: &str) -> Case {
    *CASES
        .iter()
        .find(|case| case.name() == name)
        .unwrap_or_else(|| panic!("the frozen Luau list is missing {name}"))
}
fn observed(case: Case) -> (Temporary, Sample) {
    let temp = Temporary::new();
    let workspace = temp.0.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let sample = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_case(case, 1, &workspace, &temp.0.join("audit.jsonl")));
    assert!(sample.semantic_passed, "{sample:#?}");
    (temp, sample)
}
fn rejected(sample: &Sample, mutate: impl FnOnce(&mut Value)) {
    let mut value = serde_json::to_value(sample).unwrap();
    mutate(&mut value);
    let mut changed: Sample = serde_json::from_value(value).unwrap();
    revalidate(&mut changed).unwrap();
    assert!(
        !changed.semantic_passed,
        "accepted altered execution: {changed:#?}"
    );
}

#[test]
fn calculation_uses_real_file_values_and_checks_the_computed_result() {
    let (temp, sample) = observed(declared("file_calculation"));
    assert_eq!(
        fs::read(temp.0.join("workspace/numbers.txt")).unwrap(),
        b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n"
    );
    assert_eq!(
        sample.report.return_value,
        json!({"even_sum":30,"even_count":5,"input_bytes":21})
    );
    assert_eq!(sample.report.calls.len(), 2);
    assert_eq!(sample.report.calls[1].name, "fs.read");
    assert_eq!(sample.permission_decisions.len(), 2);
    assert_eq!(sample.audit_events.len(), 4);
    rejected(&sample, |value| {
        value["report"]["return_value"]["even_sum"] = 31.into()
    });
    rejected(&sample, |value| value["report"]["calls"] = json!([]));
}

#[test]
fn complete_signatures_are_observed_in_the_executing_vm() {
    let (_temp, sample) = observed(declared("capability_signatures"));
    let value = serde_json::to_value(&sample).unwrap();
    let expected = &value["input"]["tool_signatures"];
    assert_eq!(expected.as_array().unwrap().len(), 4);
    assert_eq!(value["host_signatures"], *expected);
    assert_eq!(value["report"]["return_value"], *expected);
    for name in ["fs.list", "fs.read", "fs.stat", "fs.write"] {
        let definition = expected
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap();
        assert!(definition["description"].as_str().unwrap().len() > 10);
        assert_eq!(definition["parameters"]["additionalProperties"], false);
        assert_eq!(
            definition["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }
    rejected(&sample, |value| {
        value["report"]["return_value"][1]["parameters"]["additionalProperties"] = true.into()
    });
    rejected(&sample, |value| {
        value["host_signatures"][0]["description"] = "another signature".into();
        value["input"]["tool_signatures"] = value["host_signatures"].clone();
        value["report"]["return_value"] = value["host_signatures"].clone();
    });
}

fn exhausted(name: &str, kind: &str, calls: usize) -> Sample {
    let (temp, sample) = observed(declared(name));
    assert_eq!(sample.execution_status, "error");
    assert_eq!(sample.error.as_ref().unwrap().kind, kind);
    assert_eq!(sample.report.calls.len(), calls);
    assert_eq!(sample.permission_decisions.len(), calls);
    assert_eq!(sample.audit_events.len(), calls * 2);
    assert!(!sample.host_outcome_unknown && !sample.report.outcome_unknown);
    assert_eq!(
        fs::read(temp.0.join("workspace/before.txt")).unwrap(),
        b"completed before limit\n"
    );
    assert!(!temp.0.join("workspace/after.txt").exists());
    let value = serde_json::to_value(&sample).unwrap();
    assert_eq!(value["workspace_before"], json!([]));
    assert_eq!(value["workspace_after"].as_array().unwrap().len(), 1);
    assert_eq!(value["workspace_after"][0]["path"], "before.txt");
    assert_eq!(
        value["workspace_after"][0]["content"],
        "completed before limit\n"
    );
    assert_eq!(
        value["permission"]["grants"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["arguments"]["path"],
        "after.txt"
    );
    rejected(&sample, |value| value["error"]["kind"] = "script".into());
    rejected(&sample, |value| value["workspace_after"] = json!([]));
    sample
}

#[test]
fn output_limit_preserves_the_prior_emit_and_completed_file() {
    let sample = exhausted("output_budget", "output_limit", 1);
    assert_eq!(sample.report.outputs, vec![json!({"phase":"before_limit"})]);
    assert!(sample.report.output_bytes > 0);
    assert!(
        sample.report.output_bytes
            <= sample.config["codemode"]["output_bytes"].as_u64().unwrap() as usize
    );
    rejected(&sample, |value| value["report"]["outputs"] = json!([]));
}

#[test]
fn memory_limit_occurs_after_a_real_call_and_before_the_following_write() {
    let sample = exhausted("memory_budget", "memory_limit", 1);
    let script_bytes = sample.input["script"].as_str().unwrap().len();
    let cap = sample.config["codemode"]["memory_bytes"].as_u64().unwrap() as usize;
    assert!(
        script_bytes < cap / 4,
        "this must execute in the VM, not fail the source-size guard"
    );
    let peak = serde_json::to_value(sample.process_peak_rss.unwrap()).unwrap();
    assert!(peak["scope"].as_str().unwrap().contains("process lifetime"));
    assert_eq!(peak["unit"], "bytes");
}

#[test]
fn call_limit_blocks_the_third_dispatch_even_when_it_is_authorized() {
    let sample = exhausted("host_call_budget", "host_call_limit", 2);
    assert_eq!(sample.config["codemode"]["host_calls"], 2);
    assert_eq!(sample.permission["grants"].as_array().unwrap().len(), 4);
    rejected(&sample, |value| {
        value["config"]["codemode"]["host_calls"] = 64.into()
    });
}

#[test]
fn active_deadline_stops_the_loop_with_a_known_terminal_result() {
    let (_temp, sample) = observed(declared("active_timeout"));
    assert_eq!(sample.error.as_ref().unwrap().kind, "timeout");
    assert!(sample.report.calls.is_empty());
    assert!(!sample.host_outcome_unknown && !sample.report.outcome_unknown);
    assert_eq!(
        serde_json::to_value(&sample).unwrap()["workspace_after"],
        json!([])
    );
    rejected(&sample, |value| {
        value["execution_status"] = "success".into()
    });
}

#[test]
fn host_deadline_settles_the_same_started_call_and_keeps_unknown_effects() {
    let (_temp, sample) = observed(declared("timeout_pending_host"));
    assert_eq!(sample.error.as_ref().unwrap().kind, "timeout");
    assert_eq!(sample.report.calls.len(), 1);
    assert_eq!(sample.audit_events.len(), 2);
    assert_eq!(sample.audit_events[1].event, "settled");
    assert!(sample.report.outcome_unknown && sample.host_outcome_unknown);
    let observation = sample.cancellation.as_ref().unwrap();
    assert!(
        observation.entered && observation.stopped && observation.cancellation_observed_on_stop
    );
    assert!(!observation.active && !observation.cancellation_requested_after_entry);
    assert!(observation.settled_before_execute_return);
    assert_eq!(
        serde_json::to_value(&sample).unwrap()["caller_cancelled_after"],
        false
    );
    rejected(&sample, |value| {
        value["caller_cancelled_after"] = true.into()
    });
    rejected(&sample, |value| {
        value["host_outcome_unknown"] = false.into()
    });
    rejected(&sample, |value| value["workspace_host_after"] = json!([]));
}

#[test]
fn reader_revalidation_requires_the_frozen_script_permissions_and_limits() {
    let (_temp, sample) = observed(Case::Files);
    rejected(&sample, |value| {
        value["input"]["script"] = "return {answer = 42}".into();
        value["input"]["script_sha256"] =
            cayw_verify_tool::measurement::sha256(b"return {answer = 42}").into();
    });
    rejected(&sample, |value| value["permission"]["grants"] = json!([]));
    rejected(&sample, |value| {
        value["config"]["codemode"]["host_calls"] = 1.into()
    });
}

#[test]
fn completed_write_requires_its_original_recovery_state() {
    let (_temp, sample) = observed(Case::Files);
    let entries = sample.workspace_host_after.as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry["path"] == ".cayw-host-read-state")
    );
    rejected(&sample, |value| {
        value["workspace_host_after"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| entry["path"] != ".cayw-host-read-state");
    });
    rejected(&sample, |value| {
        let entry = value["workspace_host_after"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| entry["path"] == ".cayw-host-read-state")
            .unwrap();
        entry["content"] = "broken state".into();
        entry["bytes"] = "broken state".len().into();
        entry["sha256"] = cayw_verify_tool::measurement::sha256(b"broken state").into();
    });
}

#[test]
fn rejected_write_keeps_a_recorded_empty_workspace() {
    let (temp, sample) = observed(Case::GateDenial);
    assert_eq!(fs::read_dir(temp.0.join("workspace")).unwrap().count(), 0);
    let value = serde_json::to_value(&sample).unwrap();
    assert_eq!(value["workspace_before"], json!([]));
    assert_eq!(value["workspace_after"], json!([]));
    rejected(
        &sample,
        |value| {
            value["workspace_after"] =
                json!([{"path":"denied.txt","kind":"file","content":"unexpected"}])
        },
    );
}

fn bundle() -> &'static Path {
    static BUNDLE: OnceLock<PathBuf> = OnceLock::new();
    BUNDLE.get_or_init(|| {
        let temp = Temporary::new();
        let out = temp.0.join("bundle");
        let output = Command::new(env!("CARGO_BIN_EXE_cayw-verify-tool"))
            .args(["luau", "--samples", "1", "--out"])
            .arg(&out)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        cayw_verify_tool::evidence::Bundle::read(&out).unwrap();
        std::mem::forget(temp);
        out
    })
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn hash(path: &Path) -> String {
    cayw_verify_tool::measurement::sha256(&fs::read(path).unwrap())
}
fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}
fn altered_bundle(case: &str, mutate: impl FnOnce(&mut Value)) {
    let temp = Temporary::new();
    let root = temp.0.join("copy");
    copy_tree(bundle(), &root);
    let mut samples: Vec<Value> = fs::read_to_string(root.join("samples.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let sample = samples
        .iter_mut()
        .find(|sample| sample["scenario"] == case)
        .unwrap();
    let directory = root
        .join("samples")
        .join(sample["sample_id"].as_str().unwrap());
    let mut raw = read(&directory.join("luau.json"));
    mutate(&mut raw);
    write(&directory.join("luau.json"), &raw);
    sample["artifacts"]["luau"]["sha256"] = hash(&directory.join("luau.json")).into();
    let luau_attachment = sample["artifacts"]["luau"].clone();
    let terminal = sample["observations"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    terminal["value"]["artifacts"]["luau"] = luau_attachment;
    let observations = sample["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| serde_json::to_string(row).unwrap() + "\n")
        .collect::<String>();
    fs::write(directory.join("records.jsonl"), observations).unwrap();
    sample["artifacts"]["records"]["sha256"] = hash(&directory.join("records.jsonl")).into();
    let mut stored = sample.clone();
    stored.as_object_mut().unwrap().remove("meta");
    stored.as_object_mut().unwrap().remove("run_id");
    write(&directory.join("report.json"), &stored);
    fs::write(
        root.join("samples.jsonl"),
        samples
            .iter()
            .map(|sample| serde_json::to_string(sample).unwrap() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    let mut summary = read(&root.join("summary.json"));
    summary["artifacts"]["samples_sha256"] = hash(&root.join("samples.jsonl")).into();
    write(&root.join("summary.json"), &summary);
    let error = cayw_verify_tool::evidence::Bundle::read(&root)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Luau task does not satisfy its recorded result"),
        "unexpected outer-integrity rejection: {error}"
    );
}

#[test]
fn bundle_keeps_actual_file_attachments_and_rechecks_canonical_results() {
    let root = bundle();
    let bundle = cayw_verify_tool::evidence::Bundle::read(root).unwrap();
    assert_eq!(bundle.samples.len(), 18);
    let output = bundle
        .samples
        .iter()
        .find(|sample| sample["scenario"] == "output_budget")
        .unwrap();
    let attachment = &output["artifacts"]["workspace_0"];
    assert_eq!(attachment["path"], "workspace/before.txt");
    assert_eq!(
        fs::read(
            root.join("samples")
                .join(output["sample_id"].as_str().unwrap())
                .join("workspace/before.txt")
        )
        .unwrap(),
        b"completed before limit\n"
    );
    altered_bundle("file_calculation", |raw| {
        raw["report"]["return_value"]["even_sum"] = 31.into()
    });
    altered_bundle("capability_signatures", |raw| {
        raw["report"]["return_value"][0]["description"] = "unrelated signature".into()
    });
    altered_bundle("output_budget", |raw| raw["report"]["outputs"] = json!([]));
    altered_bundle("memory_budget", |raw| {
        raw["error"]["kind"] = "script".into()
    });
    altered_bundle("host_call_budget", |raw| {
        raw["report"]["calls"]
            .as_array_mut()
            .unwrap()
            .pop()
            .map(drop)
            .unwrap()
    });
    altered_bundle("active_timeout", |raw| {
        raw["execution_status"] = "success".into()
    });
    altered_bundle("timeout_pending_host", |raw| {
        raw["host_outcome_unknown"] = false.into()
    });
}
