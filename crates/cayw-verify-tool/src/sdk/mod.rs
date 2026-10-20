//! Reusable public-SDK and actual-IPC validation with deterministic offline faults.
mod access_scenarios;
mod access_validation;
mod children;
mod fault_validation;
mod provider;
mod report;
mod scenarios;
mod transport;
mod validation;
pub use report::local_binding;
pub(crate) use validation::validate_recorded_sample;

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
pub const USAGE: &str = "cayw-verify-tool sdk-ipc --out NEW_DIRECTORY --samples N [--scenario NAME] [--build-receipt FILE] [--negative-control wrong-script|skip-cursor]\nNames: cold-sdk, sdk-lifecycle, ipc-lifecycle, ipc-approval, ipc-rejections, ipc-event-reconnect, ipc-reconnect, ipc-cancel, host-restart-before-dispatch, host-restart-unknown.\nOffline synthetic inputs only. Loopback TCP is used for IPC; no remote services or real model calls.\nOutput directory must not exist. N is 1..=1000. Build-time listed file measurements and optional Cargo receipts retain untraced inputs as pending.";

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
    pub build_receipt: Option<PathBuf>,
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
    let (mut out, mut samples, mut scenario, mut control, mut build_receipt) =
        (None, None, None, None, None);
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("each sdk-ipc option needs a value")?;
        match flag.to_str() {
            Some("--out") if out.is_none() => out = Some(absolute(Path::new(&value))?),
            Some("--build-receipt") if build_receipt.is_none() => {
                build_receipt = Some(absolute(Path::new(&value))?)
            }
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
        build_receipt,
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
    if crate::child::cleanup_unknown() {
        return Err("previous child cleanup unconfirmed; sampling stopped".into());
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
    let receipt = crate::evidence::load_build_receipt(options.build_receipt.as_deref())?;
    create_root(&options.out)?;
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let binding = report::local_binding()?;
    let meta = json!({"schema_version":2,"run_id":run_id,"source":"offline synthetic", "binding":binding,
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
        "source_evidence":"build-time listed file measurements, executable hash and optional Cargo receipt; untraced build inputs remain pending",
        "instrumentation":"SDK observations and IPC relay frames are synchronously fsynced inside measured spans; these durations include evidence-writing overhead and are not bare-runtime benchmarks",
    });
    let mut writer = crate::evidence::BundleWriter::new(&options.out, meta, receipt.as_ref())?;
    let mut sampling_stopped = false;
    'sampling: for iteration in 1..=options.samples {
        for scenario in scenarios {
            let context = ScenarioContext {
                root: writer.sample_root(scenario.name(), iteration),
                iteration,
                negative_control: options.negative_control,
                binding: binding.clone(),
            };
            let mut sample = writer.start_sample(scenario.name(), iteration)?;
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
            if crate::child::cleanup_unknown() {
                sample.check("original_child_cleanup_confirmed", false,
                    "Child cleanup or its durable receipt is unconfirmed; no further samples may run");
                sampling_stopped = true;
            }
            if context.root.join("provider.jsonl").is_file() {
                sample.bind_attachment("provider", &context.root, "provider.jsonl")?;
            }
            writer.complete_sample(sample)?;
            if sampling_stopped {
                break 'sampling;
            }
        }
    }
    writer.finish(sampling_stopped)
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
