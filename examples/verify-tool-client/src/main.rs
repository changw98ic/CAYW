//! An independent public-API consumer; no verifier or Runtime implementation is copied.
use async_trait::async_trait;
use cayw_runtime::{Runtime, RuntimeConfig, fixture::OfflineProvider};
use cayw_verify_tool::{
    evidence::{Bundle, compare},
    provenance::{
        build_receipt::{
            BuildContext, SourceSnapshot, capture_receipt, snapshot_sources, verify_current_receipt,
        },
        executable,
    },
    sdk::{
        NegativeControl, Options, Result, Scenario, ScenarioContext, ScenarioReport, run_scenarios,
    },
};
use serde_json::{Value, json};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Instant,
};

struct CreateReopen {
    wrong_result: bool,
}

#[async_trait]
impl Scenario for CreateReopen {
    fn name(&self) -> &str {
        "client-create-reopen"
    }
    fn specification(&self, _: NegativeControl) -> Value {
        json!({"input":{"label":"Independent verifyTool client","provider":"offline fixture, no calls"},
            "permissions":{"grants":[],"product_user_approval":false},
            "operation":"create a Runtime session, drop Runtime, reopen and query the same session",
            "control":if self.wrong_result {"wrong-result"}else{"none"}})
    }
    async fn execute(&self, context: &ScenarioContext, report: &mut ScenarioReport) -> Result<()> {
        let workspace = context.root.join("workspace");
        fs::create_dir(&workspace)?;
        let database = context.root.join("session.sqlite");
        let open = || {
            Runtime::open(
                &database,
                &workspace,
                Arc::new(OfflineProvider::default()),
                cayw_ai::Model::new("offline-synthetic", "create-reopen"),
                RuntimeConfig::default(),
            )
        };
        let started = Instant::now();
        let runtime = open()?;
        let created = runtime.create(Some("Independent verifyTool client".into()))?;
        report.observe("created_session", &created)?;
        report.timing_ns.insert(
            "create".into(),
            cayw_verify_tool::measurement::elapsed_ns(started),
        );
        drop(runtime);
        let started = Instant::now();
        let runtime = open()?;
        let reopened = runtime.get(&created.id)?;
        let tasks = runtime.tasks(&created.id)?;
        let events = runtime.events(&created.id, 0, 100)?;
        report.timing_ns.insert(
            "reopen_query".into(),
            cayw_verify_tool::measurement::elapsed_ns(started),
        );
        report.observe("reopened_session", &reopened)?;
        let expected_label = if self.wrong_result {
            "Deliberately wrong expected label"
        } else {
            "Independent verifyTool client"
        };
        let actual = json!({"created":created,"reopened":reopened,"tasks":tasks,"events":events,
            "expected_label":expected_label,"source":"offline synthetic public Runtime task"});
        write_new(&context.root.join("result.json"), &actual)?;
        let saved: Value = read_json(&context.root.join("result.json"))?;
        report.bind_attachment("task_result", &context.root, "result.json")?;
        report.task_check(
            "persisted_session_matches",
            created == reopened,
            "The same session ID and full session snapshot survive closing and reopening Runtime",
        );
        report.task_check(
            "expected_label_and_idle_state",
            reopened.label == expected_label
                && reopened.messages.is_empty()
                && reopened.attempt.is_none()
                && tasks.is_empty(),
            "The reopened session has the expected label, no messages, active attempt or task",
        );
        report.task_check(
            "persisted_result_attachment_matches",
            saved == actual && !events.is_empty(),
            "The saved result contains the actual snapshots, task list and durable creation events",
        );
        Ok(())
    }
}

fn write_new(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn path(args: &mut impl Iterator<Item = std::ffi::OsString>, label: &str) -> Result<PathBuf> {
    Ok(args
        .next()
        .ok_or_else(|| format!("missing {label}"))?
        .into())
}
fn end(args: &mut impl Iterator<Item = std::ffi::OsString>) -> Result<()> {
    if args.next().is_some() {
        Err("unexpected extra argument".into())
    } else {
        Ok(())
    }
}

async fn execute() -> Result<ExitCode> {
    let mut args = env::args_os().skip(1);
    let command = args
        .next()
        .ok_or("commands: run, wrong-result, read, compare, snapshot, capture")?;
    match command.to_str().ok_or("command must be UTF-8")? {
        "run" | "wrong-result" => {
            let out = path(&mut args,"NEW_OUT")?;
            let build_receipt = args.next().map(PathBuf::from);
            end(&mut args)?;
            let passed = run_scenarios(Options { out:out.clone(), build_receipt, samples:3,
                scenarios:vec![], negative_control:NegativeControl::None },
                &[&CreateReopen { wrong_result:command == "wrong-result" }]).await?;
            let bundle = Bundle::read(&out)?;
            println!("{}",serde_json::to_string_pretty(&json!({"semantic_passed":passed,
                "sample_count":bundle.samples.len(),"build_input_verification":bundle.meta["build_input_verification"]}))?);
            Ok(if passed {ExitCode::SUCCESS}else{ExitCode::FAILURE})
        }
        "read" => {
            let root = path(&mut args,"OUT")?; end(&mut args)?;
            let bundle = Bundle::read(&root)?;
            println!("{}",serde_json::to_string_pretty(&json!({"valid":true,"summary":bundle.summary}))?);
            Ok(ExitCode::SUCCESS)
        }
        "compare" => {
            let left = path(&mut args,"LEFT")?; let right = path(&mut args,"RIGHT")?; end(&mut args)?;
            let result = compare(&Bundle::read(&left)?,&Bundle::read(&right)?);
            println!("{}",serde_json::to_string_pretty(&result)?);
            Ok(if result.status == "comparable" {ExitCode::SUCCESS}else{ExitCode::FAILURE})
        }
        "snapshot" => {
            let root = path(&mut args,"SOURCE_ROOT")?;
            let paths_json = path(&mut args,"PATHS_JSON")?;
            let out = path(&mut args,"NEW_SNAPSHOT_JSON")?; end(&mut args)?;
            let paths: Vec<PathBuf> = read_json(&paths_json)?;
            let snapshot = snapshot_sources(&root,&paths)?;
            write_new(&out,&snapshot)?;
            Ok(ExitCode::SUCCESS)
        }
        "capture" => {
            let context: BuildContext = read_json(&path(&mut args,"CONTEXT_JSON")?)?;
            let before: SourceSnapshot = read_json(&path(&mut args,"BEFORE_JSON")?)?;
            let cargo_stdout = path(&mut args,"CARGO_JSONL")?;
            let out = path(&mut args,"NEW_RECEIPT_JSON")?;
            let dep_infos = args.map(PathBuf::from).collect::<Vec<_>>();
            if dep_infos.is_empty() { return Err("capture needs at least one explicit public .d file".into()); }
            let current = executable()?;
            let executable = Path::new(current["path"].as_str().ok_or("current executable path absent")?);
            let receipt = capture_receipt(context,before,&cargo_stdout,&dep_infos,executable)?;
            let checked = verify_current_receipt(Some(&receipt))?;
            write_new(&out,&receipt)?;
            println!("{}",serde_json::to_string_pretty(&checked)?);
            Ok(ExitCode::SUCCESS)
        }
        _ => Err("commands: run NEW_OUT [RECEIPT], wrong-result NEW_OUT [RECEIPT], read OUT, compare LEFT RIGHT, snapshot ROOT PATHS_JSON NEW_JSON, capture CONTEXT BEFORE CARGO_JSONL NEW_RECEIPT DEP_INFO...".into()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match execute().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
