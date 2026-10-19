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
            "cayw-verify-tool-test-{}-{}-{}",
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
fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cayw-verify-tool"))
}
fn run(out: &Path, workspace: &Path, samples: &str) -> Output {
    command()
        .arg("luau")
        .arg("--out")
        .arg(out)
        .arg("--workspace")
        .arg(workspace)
        .arg("--samples")
        .arg(samples)
        .output()
        .unwrap()
}
fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn records(out: &Path) -> Vec<Value> {
    fs::read_to_string(out.join("samples.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn case<'a>(records: &'a [Value], name: &str, iteration: u64) -> &'a Value {
    records
        .iter()
        .find(|record| record["case"] == name && record["iteration"] == iteration)
        .unwrap()
}

#[test]
fn binary_records_real_host_results_failures_repairs_and_cancellation() {
    let temp = Temporary::new();
    let out = temp.0.join("out");
    let workspace = out.join("workspace");
    let output = run(&out, &workspace, "2");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let meta = read_json(&out.join("meta.json"));
    let summary = read_json(&out.join("summary.json"));
    let samples = records(&out);
    assert_eq!(samples.len(), 22);
    assert_eq!(summary["sample_count"], 22);
    let all_passed = samples
        .iter()
        .all(|record| record["semantic_passed"] == true);
    assert_eq!(summary["semantic_passed"], all_passed);
    assert_eq!(output.status.success(), all_passed);
    if !all_passed {
        assert!(!output.status.success());
        assert_eq!(summary["acceptance_eligible"], false);
        assert_eq!(summary["acceptance_status"], "semantic_failure");
    }
    assert_eq!(
        summary["failed_samples"],
        samples
            .iter()
            .filter(|record| record["semantic_passed"] != true)
            .count()
    );
    assert_eq!(meta["configuration"]["samples_per_case"], 2);
    assert_eq!(meta["source"], "offline synthetic");
    assert_eq!(meta["permission"]["product_approval"], false);
    assert_eq!(meta["input"]["model_calls"], 0);
    assert_eq!(meta["input"]["network_calls"], 0);
    assert_eq!(meta["platform"]["os"], std::env::consts::OS);
    assert_eq!(meta["platform"]["arch"], std::env::consts::ARCH);
    assert_eq!(
        meta["build"]["toolchain"],
        env!("CAYW_VERIFY_TOOL_RUSTC_VERSION")
    );
    assert!(
        meta["build"]["toolchain"]
            .as_str()
            .unwrap()
            .starts_with("rustc ")
    );
    assert_eq!(
        meta["build"]["build_target"],
        env!("CAYW_VERIFY_TOOL_BUILD_TARGET")
    );
    assert_eq!(
        meta["executable"]["sha256"],
        digest(&fs::read(env!("CARGO_BIN_EXE_cayw-verify-tool")).unwrap())
    );
    assert_eq!(
        summary["artifacts"]["meta"]["sha256"],
        digest(&fs::read(out.join("meta.json")).unwrap())
    );
    assert_eq!(
        summary["artifacts"]["samples"]["sha256"],
        digest(&fs::read(out.join("samples.jsonl")).unwrap())
    );
    let bound = env!("CAYW_VERIFY_TOOL_SOURCE_MANIFEST_SHA256");
    if bound.is_empty() {
        assert_eq!(meta["build"]["status"], "unbound");
        assert!(meta["build"]["source_manifest_sha256"].is_null());
        assert_eq!(
            summary["acceptance_status"],
            if all_passed {
                "unbound_not_acceptance"
            } else {
                "semantic_failure"
            }
        );
        assert_eq!(summary["acceptance_eligible"], false);
    } else {
        assert_eq!(meta["build"]["status"], "explicit_build_input");
        assert_eq!(meta["build"]["source_manifest_sha256"], bound);
        assert_eq!(summary["acceptance_eligible"], all_passed);
    }
    for record in &samples {
        assert_eq!(record["run_id"], meta["run_id"]);
        assert_eq!(record["meta"], "meta.json");
        assert_eq!(record["source"], "offline synthetic");
        if record["case"] != "cancel_pending_host" {
            assert_eq!(record["semantic_passed"], true, "{}", record);
        } else {
            let checks = record["semantic_checks"].as_array().unwrap();
            assert!(
                checks
                    .iter()
                    .any(|check| check["name"] == "cancel_started_host_synchronously_settled")
            );
            assert_eq!(
                record["semantic_passed"],
                checks.iter().all(|check| check["passed"] == true)
            );
        }
        assert_eq!(
            record["input"]["script_sha256"],
            digest(record["input"]["script"].as_str().unwrap().as_bytes())
        );
        assert!(record["timing"]["case_execute_ns"].is_u64());
        assert!(record["timing"]["case_total_ns"].is_u64());
        assert!(record["timing"]["vm_setup_only_ns"].is_null());
        let audit = fs::read_to_string(record["audit_path"].as_str().unwrap()).unwrap();
        let events: Vec<Value> = audit
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events, *record["audit_events"].as_array().unwrap());
    }
    for iteration in 1..=2 {
        let root = workspace.join(format!("iteration-{iteration:04}"));
        let files = case(&samples, "file_pipeline", iteration);
        let content = "offline CAYW\n第二行\n";
        assert_eq!(
            fs::read_to_string(root.join("file_pipeline/pipeline.txt")).unwrap(),
            content
        );
        assert_eq!(files["report"]["return_value"]["content"], content);
        assert_eq!(
            files["report"]["return_value"]["sha256"],
            digest(content.as_bytes())
        );
        assert_eq!(files["report"]["return_value"]["listed"], true);
        assert_eq!(files["report"]["calls"].as_array().unwrap().len(), 4);
        assert_eq!(files["permission_decisions"].as_array().unwrap().len(), 4);
        assert_eq!(files["audit_events"].as_array().unwrap().len(), 8);
        let failed = case(&samples, "syntax_failure", iteration);
        let repaired = case(&samples, "syntax_repair_success", iteration);
        assert_eq!(failed["execution_status"], "error");
        assert_eq!(failed["error"]["kind"], "script");
        assert_eq!(failed["repair"]["attempt"], 1);
        assert!(failed["report"]["calls"].as_array().unwrap().is_empty());
        assert_eq!(repaired["execution_status"], "success");
        assert_eq!(repaired["repair"]["attempt"], 2);
        assert_eq!(repaired["repair"]["pair_id"], failed["repair"]["pair_id"]);
        assert_eq!(
            repaired["repair"]["previous_sample_id"],
            failed["sample_id"]
        );
        assert_eq!(repaired["workspace"], failed["workspace"]);
        assert_eq!(
            fs::read_to_string(root.join("syntax-repair/repair.txt")).unwrap(),
            "repaired offline fixture\n"
        );
        let cancelled = case(&samples, "cancel_pending_host", iteration);
        assert_eq!(cancelled["error"]["kind"], "cancelled");
        assert_eq!(cancelled["report"]["outcome_unknown"], true);
        assert_eq!(cancelled["host_outcome_unknown"], true);
        for name in [
            "entered",
            "stopped",
            "cancellation_requested_after_entry",
            "cancellation_observed_on_stop",
        ] {
            assert_eq!(cancelled["cancellation"][name], true, "{name}: {cancelled}");
        }
        assert_eq!(cancelled["cancellation"]["active"], false);
        assert!(cancelled["cancellation"]["cancel_to_execute_return_ns"].is_u64());
        assert_eq!(cancelled["audit_events"].as_array().unwrap().len(), 2);
        let terminal = &cancelled["audit_events"][1];
        assert_eq!(
            terminal["event"], "settled",
            "runtime returned before durable settlement: {cancelled}"
        );
        assert_eq!(
            cancelled["cancellation"]["settled_before_execute_return"],
            true
        );
        assert_eq!(terminal["outcome_unknown"], true);
        for check_name in [
            "cancel_started_host_synchronously_settled",
            "audit_started_settled_pairing",
            "permission_decisions_match_calls",
            "call_status_and_payload",
            "outcome_flags_match_calls",
        ] {
            let check = cancelled["semantic_checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|check| check["name"] == check_name)
                .unwrap();
            assert_eq!(check["passed"], true, "{check_name}: {cancelled}");
        }
        assert_eq!(cancelled["semantic_passed"], true, "{cancelled}");
        let looped = case(&samples, "loop_budget", iteration);
        assert_eq!(looped["error"]["kind"], "instruction_limit");
        assert!(
            looped["report"]["instruction_checkpoints"]
                .as_u64()
                .unwrap()
                > 2_000
        );
        let denied = case(&samples, "host_permission_denied", iteration);
        assert_eq!(
            denied["permission_decisions"][0]["decision"],
            "fixture_denial"
        );
        assert!(!root.join("host_permission_denied/denied.txt").exists());
        let sandbox = case(&samples, "sandbox_globals", iteration);
        assert_eq!(sandbox["report"]["return_value"]["os_absent"], true);
        assert_eq!(sandbox["report"]["return_value"]["io_absent"], true);
        let cold = case(&samples, "cold_process_first_success", iteration);
        let warm = case(&samples, "warm_process_first_success", iteration);
        assert_ne!(cold["process_id"], warm["process_id"]);
        assert!(cold["timing"]["parent_spawn_to_exit_ns"].is_u64());
        assert!(cold["timing"]["child_entry_to_record_ns"].is_u64());
        assert!(warm["timing"]["warmup_execute_ns"].is_u64());
        assert_eq!(warm["warmup"]["return_value"]["warmup"], true);
    }
}

#[test]
fn existing_out_or_workspace_is_rejected_without_overwrite() {
    let temp = Temporary::new();
    let out = temp.0.join("existing");
    fs::create_dir(&out).unwrap();
    fs::write(out.join("sentinel"), "keep").unwrap();
    let workspace = temp.0.join("new-workspace");
    let output = run(&out, &workspace, "1");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
    assert_eq!(fs::read_to_string(out.join("sentinel")).unwrap(), "keep");
    assert!(!workspace.exists());
    let new_out = temp.0.join("new-out");
    let output = run(&new_out, &out, "1");
    assert!(!output.status.success());
    assert!(!new_out.exists());
}

#[test]
fn sample_limits_duplicate_flags_and_missing_values_are_rejected() {
    let temp = Temporary::new();
    for count in ["0", "1001", "-1", "NaN"] {
        let out = temp.0.join(format!("out-{count}"));
        assert!(!run(&out, &temp.0.join("workspace"), count).status.success());
        assert!(!out.exists());
    }
    let output = command().args(["luau", "--out"]).output().unwrap();
    assert!(!output.status.success());
    let output = command()
        .args(["luau", "--out", "one", "--out", "two", "--samples", "1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn runtime_environment_cannot_forge_build_source_binding_and_default_workspace_works() {
    let temp = Temporary::new();
    let out = temp.0.join("out");
    let output = command()
        .arg("luau")
        .arg("--out")
        .arg(&out)
        .args(["--samples", "1"])
        .env("CAYW_SOURCE_MANIFEST_SHA256", "f".repeat(64))
        .output()
        .unwrap();
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = read_json(&out.join("summary.json"));
    assert_eq!(output.status.success(), summary["semantic_passed"] == true);
    let meta = read_json(&out.join("meta.json"));
    let bound = env!("CAYW_VERIFY_TOOL_SOURCE_MANIFEST_SHA256");
    if bound.is_empty() {
        assert!(meta["build"]["source_manifest_sha256"].is_null());
        assert_eq!(
            read_json(&out.join("summary.json"))["acceptance_eligible"],
            false
        );
    } else {
        assert_eq!(meta["build"]["source_manifest_sha256"], bound);
    }
    assert_eq!(records(&out).len(), 11);
    assert!(
        out.join("workspace/iteration-0001/file_pipeline/pipeline.txt")
            .is_file()
    );
}
