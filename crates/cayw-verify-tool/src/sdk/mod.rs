//! Reusable public-SDK and actual-IPC validation with deterministic offline faults.
mod children;
mod provider;
mod report;
mod scenarios;
mod transport;

use crate::measurement::sha256;
use async_trait::async_trait;
pub use report::{Check, EventPage, Observation, ScenarioReport, contiguous_events};
pub use scenarios::{BuiltinScenario, NegativeControl};
use serde_json::{Value, json};
use std::{
    env,
    error::Error,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
pub const DEADLINE: Duration = Duration::from_secs(30);
pub const USAGE: &str = "cayw-verify-tool sdk-ipc --out NEW_DIRECTORY --samples N [--scenario NAME] [--negative-control wrong-script|skip-cursor]\nNames: cold-sdk, sdk-lifecycle, ipc-lifecycle, ipc-reconnect, ipc-cancel, host-restart-before-dispatch, host-restart-unknown.\nOffline synthetic inputs only. Loopback TCP is used for IPC; no remote services or real model calls.\nOutput directory must not exist. N is 1..=1000. A source digest is caller-declared, never self-verified.";

/// A new scenario supplies concrete inputs, permission policy and fault points,
/// executes the public API, and appends raw observations and semantic checks.
#[async_trait]
pub trait Scenario: Send + Sync {
    fn name(&self) -> &str;
    fn specification(&self, control: NegativeControl) -> Value;
    async fn execute(&self, context: &ScenarioContext, report: &mut ScenarioReport) -> Result<()>;
}
#[derive(Clone)]
pub struct ScenarioContext {
    pub root: PathBuf,
    pub iteration: u32,
    pub negative_control: NegativeControl,
    pub binding: Value,
}
#[derive(Debug)]
pub struct Options {
    pub out: PathBuf,
    pub samples: u32,
    pub scenarios: Vec<BuiltinScenario>,
    pub negative_control: NegativeControl,
}
fn absolute(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err("output path is empty".into());
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => return Err("output paths must not contain '..'".into()),
            Component::CurDir => {}
            _ => result.push(component),
        }
    }
    Ok(result)
}
pub fn parse_options(args: Vec<OsString>) -> Result<Options> {
    let (mut out, mut samples, mut scenario, mut control) = (None, None, None, None);
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("each sdk-ipc option needs a value")?;
        match flag.to_str() {
            Some("--out") if out.is_none() => out = Some(absolute(Path::new(&value))?),
            Some("--samples") if samples.is_none() => {
                let n: u32 = value.to_str().ok_or("samples must be digits")?.parse()?;
                if !(1..=1000).contains(&n) {
                    return Err("samples must be 1..=1000".into());
                }
                samples = Some(n);
            }
            Some("--scenario") if scenario.is_none() => {
                scenario = Some(BuiltinScenario::parse(
                    value.to_str().ok_or("scenario must be UTF-8")?,
                )?)
            }
            Some("--negative-control") if control.is_none() => {
                control = Some(NegativeControl::parse(
                    value.to_str().ok_or("control must be UTF-8")?,
                )?)
            }
            _ => {
                return Err(
                    format!("unknown or repeated option: {}", flag.to_string_lossy()).into(),
                );
            }
        }
    }
    let negative_control = control.unwrap_or_default();
    let scenarios = scenario
        .map(|s| vec![s])
        .unwrap_or_else(|| BuiltinScenario::ALL.to_vec());
    if !scenarios
        .iter()
        .any(|scenario| negative_control.applies(*scenario))
    {
        return Err("negative control has no applicable selected scenario".into());
    }
    Ok(Options {
        out: out.ok_or("--out is required")?,
        samples: samples.ok_or("--samples is required")?,
        scenarios,
        negative_control,
    })
}
fn create_root(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            return Err(format!(
                "directory already exists; refusing to overwrite: {}",
                path.display()
            )
            .into());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if !path.parent().is_some_and(Path::is_dir) {
        return Err("output parent must already be a directory".into());
    }
    fs::create_dir(path)?;
    Ok(())
}

/// Runs custom scenarios through the same durable raw report format. Directories
/// are allocated once, never reused. Failed samples remain on disk.
pub async fn run_scenarios(options: Options, scenarios: &[&dyn Scenario]) -> Result<bool> {
    if !(1..=1000).contains(&options.samples) || scenarios.is_empty() {
        return Err("run needs 1..=1000 samples and at least one scenario".into());
    }
    let mut names = std::collections::BTreeSet::new();
    for scenario in scenarios {
        let name = scenario.name();
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || !names.insert(name)
        {
            return Err(
                "scenario names must be unique ASCII letters, digits, hyphens or underscores"
                    .into(),
            );
        }
    }
    let out = absolute(&options.out)?;
    let options = Options { out, ..options };
    create_root(&options.out)?;
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let binding = report::local_binding()?;
    let meta = json!({"schema_version":1,"run_id":run_id,"source":"offline synthetic", "binding":binding,
        "samples_per_scenario":options.samples,"out":options.out,
        "scenarios":scenarios.iter().map(|s|json!({"name":s.name(),"specification":s.specification(options.negative_control)})).collect::<Vec<_>>(),
        "measurement":{"clock":"std::time::Instant monotonic elapsed","duration_unit":"ns","sampling":"serial fixed scenarios, one independent directory per sample; cold uses a fresh process; warm uses the same Runtime",
            "cold":"parent spawn through child exit; includes loader, setup, first SDK prompt and serialization; OS caches not flushed",
            "first_execution":"first Runtime::prompt through durable completion, including Agent/Cells/CodeMode/Host", "orchestration":"fixed Provider executes real Luau write/read through Runtime",
            "cancellation":"after fixture Provider entered, through durable task terminal and idle; no fixed latency assertion",
            "resource":"getrusage lifetime peak RSS on macOS/Linux, bytes versus KiB; null means unsupported/unobserved; no per-task delta",
            "performance_thresholds":null,"real_task_success":null,"real_repair_cost":null},
        "real_provider":{"status":"unverified","calls":0,"credentials_read":false},
        "issue_2_complete":false,"network":{"remote_calls":0,"transport":"authenticated loopback TCP only"},
        "scenario_watchdog_seconds":180,
        "platform_scope":"Only this recorded OS/architecture was executed. Other platforms remain unverified.",
        "artifacts":{"samples":"samples.jsonl","summary":"summary.json","workspaces":"samples/<iteration>/<scenario>/"},
        "source_evidence":"caller digest and observed executable hash; no caller digest is promoted to self-verified source",
        "instrumentation":"SDK observations and IPC relay frames are synchronously fsynced inside measured spans; these durations include evidence-writing overhead and are not bare-runtime benchmarks",
    });
    provider::write_new(&options.out.join("meta.json"), &meta)?;
    fs::create_dir(options.out.join("samples"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(options.out.join("samples.jsonl"))?;
    let mut records = Vec::new();
    for iteration in 1..=options.samples {
        let root = options.out.join("samples").join(format!("{iteration:04}"));
        fs::create_dir(&root)?;
        for scenario in scenarios {
            let context = ScenarioContext {
                root: root.join(scenario.name()),
                iteration,
                negative_control: options.negative_control,
                binding: binding.clone(),
            };
            fs::create_dir(&context.root)?;
            let mut sample = ScenarioReport::new(
                scenario.name(),
                iteration,
                scenario.specification(options.negative_control),
                binding.clone(),
            );
            sample.attach_records(&context.root.join("records.jsonl"))?;
            let outcome =
                tokio::time::timeout(DEADLINE * 6, scenario.execute(&context, &mut sample)).await;
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    sample.observe("runner_error", json!({"message":error.to_string()}))?;
                    sample.check(
                        "scenario_reached_expected_terminal",
                        false,
                        error.to_string(),
                    );
                }
                Err(error) => {
                    sample.observe("runner_watchdog", json!({"message":error.to_string()}))?;
                    sample.check(
                        "scenario_reached_expected_terminal",
                        false,
                        "Scenario liveness watchdog expired",
                    );
                }
            }
            sample.finish();
            sample.persist_terminal()?;
            sample.bind_records(&context.root.join("records.jsonl"))?;
            let mut value = serde_json::to_value(&sample)?;
            value["run_id"] = json!(run_id);
            value["meta"] = json!("meta.json");
            let mut bytes = serde_json::to_vec(&value)?;
            bytes.push(b'\n');
            file.write_all(&bytes)?;
            file.sync_all()?;
            provider::write_new(&context.root.join("report.json"), &sample)?;
            records.push(sample);
        }
    }
    let passed = records.iter().all(|s| s.status == "passed");
    let source_bound = !env!("CAYW_VERIFY_TOOL_SOURCE_MANIFEST_SHA256").is_empty();
    let platforms:[Value;3]=["macos","windows","linux"].map(|os|json!({"os":os,"status":if os==env::consts::OS { if passed { "observed_passed" } else { "observed_failed_or_unsupported" } } else { "unverified" }}));
    let mut phases = std::collections::BTreeMap::<&str, Vec<u64>>::new();
    for record in &records {
        for (phase, value) in &record.timing_ns {
            phases.entry(phase).or_default().push(*value);
        }
    }
    let timing_summary:Vec<Value>=phases.into_iter().map(|(phase,mut values)| {
        values.sort_unstable();
        let percentile=|percent:usize|values.get((values.len()*percent).div_ceil(100).saturating_sub(1)).copied();
        json!({"phase":phase,"unit":"ns","measured_count":values.len(),"min":values.first(),"median":percentile(50),"p95":percentile(95),"max":values.last()})
    }).collect();
    let summary = json!({"schema_version":1,"run_id":run_id,"source":"offline synthetic","sample_count":records.len(),
        "semantic_passed":passed,"failed_samples":records.iter().filter(|s|s.status=="failed").count(),
        "unsupported_samples":records.iter().filter(|s|s.status=="unsupported").count(),
        "unverified_samples":records.iter().filter(|s|s.status=="unverified").count(),
        "source_bound":source_bound,"self_verified_source":false,
        "issue_2_complete":false,"real_provider":"unverified","platforms":platforms,
        "bound_offline_semantics_passed":passed&&source_bound,
        "statuses":records.iter().map(|s|json!({"sample_id":s.sample_id,"status":s.status,"timing_ns":s.timing_ns,
            "failed_checks":s.checks.iter().filter(|c|!c.passed).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "artifacts":{"meta_sha256":sha256(&fs::read(options.out.join("meta.json"))?),"samples_sha256":sha256(&fs::read(options.out.join("samples.jsonl"))?)},
        "binding":binding,"timing_summary":timing_summary,"performance_thresholds":null});
    provider::write_new(&options.out.join("summary.json"), &summary)?;
    println!(
        "{}",
        serde_json::to_string(
            &json!({"out":options.out,"sample_count":records.len(),"semantic_passed":passed,"issue_2_complete":false})
        )?
    );
    Ok(passed)
}
pub(crate) fn write_bytes_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub async fn run(options: Options) -> Result<bool> {
    let builtins = options.scenarios.clone();
    let scenarios: Vec<&dyn Scenario> = builtins.iter().map(|s| s as &dyn Scenario).collect();
    run_scenarios(options, &scenarios).await
}
pub async fn internal(mode: &str, args: Vec<OsString>, entry: Instant) -> Result<bool> {
    children::dispatch(mode, args, entry).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_rejects_repeated_unsafe_and_inapplicable_options() {
        for args in [
            vec!["--out", "../escape", "--samples", "1"],
            vec!["--out", "x", "--samples", "0"],
            vec!["--out", "x", "--samples", "1", "--out", "y"],
            vec![
                "--out",
                "x",
                "--samples",
                "1",
                "--scenario",
                "ipc-cancel",
                "--negative-control",
                "wrong-script",
            ],
        ] {
            assert!(parse_options(args.into_iter().map(OsString::from).collect()).is_err());
        }
    }
}
