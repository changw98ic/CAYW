use async_trait::async_trait;
use cayw_verify_tool::{
    child::ManagedChild,
    sdk::{self, NegativeControl, Options, Scenario, ScenarioContext, ScenarioReport},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn directory(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "cw-child-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&path).unwrap();
    path
}

fn command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "fixture_child", "--ignored", "--nocapture"])
        .env("CAYW_TEST_CHILD_MODE", mode);
    command
}

#[test]
#[ignore = "subprocess fixture used by the child ownership tests"]
fn fixture_child() {
    let mode = std::env::var("CAYW_TEST_CHILD_MODE").unwrap();
    if mode == "failure" {
        std::process::exit(7);
    }
    if mode == "normal" {
        std::io::stdout().write_all(&[b'o'; 64 * 1024]).unwrap();
        std::io::stderr().write_all(&[b'e'; 64 * 1024]).unwrap();
        return;
    }
    let ready = std::env::var_os("CAYW_TEST_CHILD_READY").unwrap();
    fs::write(ready, b"ready").unwrap();
    loop {
        std::io::stdout().write_all(&[b'x'; 8192]).unwrap();
        std::io::stderr().write_all(&[b'y'; 8192]).unwrap();
    }
}

fn receipt(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn original_owner_drains_both_streams_and_retains_only_bounded_output() {
    let root = directory("streams");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut child = ManagedChild::spawn(
            command("normal"),
            Duration::from_secs(10),
            4096,
            root.join("normal.json"),
        )
        .unwrap();
        let output = child.wait().await.unwrap();
        assert!(output.success && output.cleanup_confirmed && !output.timed_out);
        assert_eq!(output.stdout.len(), 4096);
        assert_eq!(output.stderr.len(), 4096);
        assert!(output.stdout_bytes >= 64 * 1024 && output.stderr_bytes >= 64 * 1024);
        assert_eq!(
            receipt(&root.join("normal.json"))["process_id"],
            output.process_id
        );
        let mut failed = ManagedChild::spawn(
            command("failure"),
            Duration::from_secs(10),
            4096,
            root.join("failure.json"),
        )
        .unwrap();
        let output = failed.wait().await.unwrap();
        assert!(!output.success && output.cleanup_confirmed);
        assert_eq!(output.exit_code, Some(7));
    });
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dropping_the_enclosing_future_joins_its_original_child() {
    let root = directory("future");
    let ready = root.join("ready");
    let record = root.join("drop.json");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut command = command("stream");
        command.env("CAYW_TEST_CHILD_READY", &ready);
        let mut future = Box::pin(async {
            let mut child =
                ManagedChild::spawn(command, Duration::from_secs(10), 4096, record.clone())?;
            child.wait().await
        });
        assert!(futures::poll!(&mut future).is_pending());
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(future);
        let output = receipt(&record);
        assert_eq!(output["cleanup_confirmed"], true);
        assert_eq!(output["stopped"], true);
        assert_eq!(output["success"], false);
        assert_eq!(output["timed_out"], false);
        assert!(output["stdout"].as_array().unwrap().len() <= 4096);
        assert!(output["stderr"].as_array().unwrap().len() <= 4096);
    });
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cold_luau_receipt_identifies_the_original_child_and_complete_output() {
    let root = directory("luau");
    let out = root.join("run");
    let output = Command::new(env!("CARGO_BIN_EXE_cayw-verify-tool"))
        .args(["luau", "--samples", "1", "--out"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let samples: Vec<Value> = fs::read_to_string(out.join("samples.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let cold = samples
        .iter()
        .find(|sample| sample["scenario"] == "cold_process_first_success")
        .unwrap();
    let path = out.join("audits/0001-cold_process_first_success.child.json");
    assert!(
        path.is_file(),
        "cold execution must retain its original owned child receipt"
    );
    let owner = receipt(&path);
    assert_eq!(owner["process_id"], cold["process_id"]);
    assert_eq!(owner["cleanup_confirmed"], true);
    assert_eq!(owner["success"], true);
    assert_eq!(owner["timed_out"], false);
    assert_eq!(owner["stderr_bytes"], 0);
    fs::remove_dir_all(root).unwrap();
}

struct CleanupFailure;
#[async_trait]
impl Scenario for CleanupFailure {
    fn name(&self) -> &str {
        "cleanup-failure"
    }
    fn specification(&self, _: NegativeControl) -> Value {
        json!({"fixture":"original child receipt cannot be persisted"})
    }
    async fn execute(&self, context: &ScenarioContext, _: &mut ScenarioReport) -> sdk::Result<()> {
        let mut child = ManagedChild::spawn(
            command("normal"),
            Duration::from_secs(10),
            4096,
            context.root.join("missing-parent/exit.json"),
        )?;
        child.wait().await?;
        Err("receipt persistence unexpectedly succeeded".into())
    }
}
struct LaterSample;
#[async_trait]
impl Scenario for LaterSample {
    fn name(&self) -> &str {
        "later-sample"
    }
    fn specification(&self, _: NegativeControl) -> Value {
        json!({"fixture":"must not execute"})
    }
    async fn execute(
        &self,
        context: &ScenarioContext,
        report: &mut ScenarioReport,
    ) -> sdk::Result<()> {
        fs::write(context.root.join("executed"), b"unexpected")?;
        report.task_check("effect", true, "This sample should never run");
        Ok(())
    }
}

#[test]
fn unconfirmed_cleanup_stops_all_later_scenarios() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cleanup_failure_runner",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "isolates the deliberately persistent cleanup-unknown state"]
fn cleanup_failure_runner() {
    let root = directory("unknown");
    let out = root.join("run");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(sdk::run_scenarios(
        Options {
            out: out.clone(),
            samples: 3,
            scenarios: vec![],
            build_receipt: None,
            negative_control: NegativeControl::None,
        },
        &[&CleanupFailure, &LaterSample],
    ));
    assert!(matches!(result, Ok(false)), "{result:?}");
    assert!(
        !out.join("samples/0001/later-sample/executed").exists(),
        "unconfirmed cleanup must prevent the next actual scenario from running"
    );
    let summary = receipt(&out.join("summary.json"));
    assert_eq!(summary["sample_count"], 1);
    assert_eq!(summary["sampling_stopped"], true);
    assert_eq!(summary["semantic_passed"], false);
    assert!(!out.join("samples/0002").exists());
    let retry = runtime.block_on(sdk::run_scenarios(
        Options {
            out: root.join("retry"),
            samples: 1,
            scenarios: vec![],
            build_receipt: None,
            negative_control: NegativeControl::None,
        },
        &[&LaterSample],
    ));
    assert!(
        retry.is_err(),
        "a new run cannot clear the unknown cleanup state"
    );
    assert!(!root.join("retry").exists());
    fs::remove_dir_all(root).unwrap();
}
