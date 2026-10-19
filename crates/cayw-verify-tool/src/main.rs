mod cases;
mod fixtures;
mod runner;

use cayw_verify_tool::{
    measurement,
    provenance::{build_binding, platform},
    sdk,
};

use cases::{CASES, Case};
use fixtures::SOURCE;
use measurement::{elapsed_ns, process_peak_rss, sha256};
use runner::{Sample, empty_sample, run_case, sample_id};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitCode},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

type Result<T> = sdk::Result<T>;
const USAGE: &str = "cayw-verify-tool luau --out NEW_DIRECTORY --samples N [--workspace NEW_DIRECTORY]\nN must be 1..=1000; default workspace is OUT/workspace. All fixture inputs are offline synthetic.\nBuild with an explicit CAYW_SOURCE_MANIFEST_SHA256 to bind acceptance evidence.\nWithout it, measurements are unbound and cannot constitute acceptance evidence.";

struct Options {
    out: PathBuf,
    workspace: PathBuf,
    samples: u32,
}

fn main() -> ExitCode {
    let entry = Instant::now();
    match dispatch(entry) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("cayw-verify-tool: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(entry: Instant) -> Result<bool> {
    let mut args = env::args_os().skip(1);
    match args.next().as_deref().and_then(|value| value.to_str()) {
        Some("--help") => {
            println!("{USAGE}\n\n{}", sdk::USAGE);
            Ok(true)
        }
        Some("luau") => {
            let rest: Vec<_> = args.collect();
            if rest == [OsString::from("--help")] {
                println!("{USAGE}");
                return Ok(true);
            }
            let options = parse_options(rest)?;
            let runtime_start = Instant::now();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let runtime_setup_ns = elapsed_ns(runtime_start);
            let entry_to_runtime_ready_ns = elapsed_ns(entry);
            runtime.block_on(run(options, runtime_setup_ns, entry_to_runtime_ready_ns))
        }
        Some("sdk-ipc") => {
            let rest: Vec<_> = args.collect();
            if rest == [OsString::from("--help")] {
                println!("{}", sdk::USAGE);
                return Ok(true);
            }
            let options = sdk::parse_options(rest)?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(sdk::run(options))
        }
        Some(mode @ ("__sdk_cold" | "__sdk_host")) => {
            let rest: Vec<_> = args.collect();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(sdk::internal(mode, rest, entry))
        }
        Some("__cold_case") => internal_case(args.collect(), entry),
        _ => Err(USAGE.into()),
    }
}

fn parse_options(args: Vec<OsString>) -> Result<Options> {
    let mut out = None;
    let mut workspace = None;
    let mut samples = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("each option needs a value")?;
        match flag.to_str() {
            Some("--out") if out.is_none() => out = Some(absolute_path(Path::new(&value))?),
            Some("--workspace") if workspace.is_none() => {
                workspace = Some(absolute_path(Path::new(&value))?)
            }
            Some("--samples") if samples.is_none() => {
                let count: u32 = value
                    .to_str()
                    .ok_or("samples must be UTF-8 digits")?
                    .parse()?;
                if !(1..=1000).contains(&count) {
                    return Err("samples must be 1..=1000".into());
                }
                samples = Some(count);
            }
            _ => {
                return Err(
                    format!("unknown or repeated option: {}", flag.to_string_lossy()).into(),
                );
            }
        }
    }
    let out = out.ok_or("--out is required")?;
    let workspace = workspace.unwrap_or_else(|| out.join("workspace"));
    if out == workspace || out.starts_with(&workspace) {
        return Err(
            "workspace must be a separate new directory or a new directory below out".into(),
        );
    }
    Ok(Options {
        out,
        workspace,
        samples: samples.ok_or("--samples is required")?,
    })
}
fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err("directory path is empty".into());
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut absolute = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => return Err("directory paths must not contain '..'".into()),
            Component::CurDir => {}
            _ => absolute.push(component),
        }
    }
    Ok(absolute)
}
fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(format!(
            "directory already exists; refusing to overwrite: {}",
            path.display()
        )
        .into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
fn prepare(options: &Options) -> Result<()> {
    // Check both before creating either, including symlinks and empty directories.
    ensure_absent(&options.out)?;
    ensure_absent(&options.workspace)?;
    let out_parent = options.out.parent().ok_or("out needs a parent directory")?;
    if !out_parent.is_dir() {
        return Err("out parent must already be a directory".into());
    }
    if options.workspace.parent() != Some(options.out.as_path())
        && !options.workspace.parent().is_some_and(Path::is_dir)
    {
        return Err("workspace parent must exist or be out".into());
    }
    fs::create_dir(&options.out)?;
    fs::create_dir(&options.workspace)?;
    fs::create_dir(options.out.join("audits"))?;
    Ok(())
}
fn write_json_new(path: &Path, value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(bytes)
}

async fn run(
    options: Options,
    runtime_setup_ns: u64,
    entry_to_runtime_ready_ns: u64,
) -> Result<bool> {
    prepare(&options)?;
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let binary = env::current_exe()?;
    let meta = json!({
        "schema_version":1,"run_id":run_id,"source":SOURCE,
        "build":build_binding(),
        "executable":{"path":binary,"sha256":sha256(&fs::read(&binary)?)},
        "platform":platform(),
        "configuration":{"samples_per_case":options.samples,"fixed_cases":CASES.len(),
            "expected_sample_count":options.samples as usize * CASES.len(),"out":options.out,"workspace":options.workspace,
            "cases":CASES.iter().map(|case| json!({"case":case.name(),"config":runner::config_json(*case),
                "input_script_sha256":sha256(case.script().as_bytes())})).collect::<Vec<_>>()},
        "permission":{"source":SOURCE,"product_approval":false,
            "policy":"explicit exact-name and exact-argument grants only; default deny; real Host gate and durable audit"},
        "input":{"source":SOURCE,"model_calls":0,"network_calls":0,"fixtures":"fixed embedded scripts and UTF-8 strings; all case file contents use the real Host"},
        "measurement":{
            "duration_unit":"ns","clock":"std::time::Instant monotonic elapsed",
            "cold":"parent_spawn_to_exit_ns includes spawning current_exe, loader, parsing, Tokio setup, fixture audit creation, Host/catalog initialization, CodeMode setup, first execute (fresh Luau VM), serialization and child exit; OS file caches are not flushed",
            "warm":"same running parent process; new Host and CodeMode for each case. warm_process_first_success records first warmup execute and next execute. CodeMode creates a fresh Luau VM on every execute; no VM reuse is claimed",
            "host_setup":"Host::new including directory capability, schemas and runner initialization; excludes audit file creation",
            "codemode_setup":"CodeMode::new catalog/schema setup; no Lua VM exists yet",
            "vm_setup_only_ns":null,
            "first_execute":"first CodeMode::execute includes VM setup, compilation, running script and VM destruction; cannot split internal phases through the public API",
            "parent_runtime_setup_ns":runtime_setup_ns,"parent_entry_to_runtime_ready_ns":entry_to_runtime_ready_ns,
            "not_measured":["complete Agent/model latency","OS boot-to-entry time","per-task RSS","Monty comparison"],
            "rss":"getrusage(RUSAGE_SELF).ru_maxrss where supported (macOS bytes, Linux KiB); cumulative process lifetime high-water mark, not a task or allocation delta; otherwise null",
            "performance_thresholds":null,
            "unmeasured_phase":"null, including phases skipped by setup failures",
        },
        "artifacts":{"samples":"samples.jsonl","summary":"summary.json","audit":"audits/<iteration>-<case>.jsonl"},
        "acceptance":"unbound builds cannot be acceptance evidence; a bound run is limited to the recorded offline platform and fixture semantics",
    });
    let meta_bytes = write_json_new(&options.out.join("meta.json"), &meta)?;
    let mut samples_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(options.out.join("samples.jsonl"))?;
    let mut samples = Vec::with_capacity(options.samples as usize * CASES.len());
    let mut sample_digest = sha2::Sha256::default();
    use sha2::Digest;
    for iteration in 1..=options.samples {
        let iteration_root = options.workspace.join(format!("iteration-{iteration:04}"));
        let iteration_setup = fs::create_dir(&iteration_root);
        for case in CASES {
            let workspace = iteration_root.join(case.workspace_name());
            let setup = if let Err(error) = &iteration_setup {
                Err(io::Error::new(error.kind(), error.to_string()))
            } else if case == Case::SyntaxRepair {
                Ok(())
            } else {
                fs::create_dir(&workspace)
            };
            let audit_path = options
                .out
                .join("audits")
                .join(format!("{iteration:04}-{}.jsonl", case.name()));
            let mut sample = match setup {
                Err(error) => {
                    let mut sample = empty_sample(case, iteration, &workspace, &audit_path);
                    sample.fail("workspace_setup", error.to_string());
                    sample
                }
                Ok(()) if case == Case::Cold => cold_case(iteration, &workspace, &audit_path),
                Ok(()) => run_case(case, iteration, &workspace, &audit_path).await,
            };
            if case == Case::SyntaxRepair {
                let previous = samples.last().is_some_and(|previous: &Sample| {
                    previous.sample_id == sample_id(Case::SyntaxFailure, iteration)
                        && previous.semantic_passed
                        && previous
                            .error
                            .as_ref()
                            .is_some_and(|error| error.kind == "script")
                });
                sample.check("paired_after_observed_syntax_failure", previous,
                    "The immediately preceding sample is the actual syntax failure in this same iteration and workspace.");
            }
            let mut record = serde_json::to_value(&sample)?;
            record["run_id"] = run_id.clone().into();
            record["meta"] = "meta.json".into();
            let mut bytes = serde_json::to_vec(&record)?;
            bytes.push(b'\n');
            samples_file.write_all(&bytes)?;
            samples_file.sync_all()?;
            sample_digest.update(&bytes);
            samples.push(sample);
        }
    }
    let samples_sha256 = format!("{:x}", sample_digest.finalize());
    let passed = samples.iter().all(|sample| sample.semantic_passed);
    let bound = !env!("CAYW_VERIFY_TOOL_SOURCE_MANIFEST_SHA256").is_empty();
    let summary = summary(
        &samples,
        passed,
        bound,
        &run_id,
        &meta_bytes,
        &samples_sha256,
    );
    write_json_new(&options.out.join("summary.json"), &summary)?;
    println!(
        "{}",
        serde_json::to_string(&json!({"out":options.out,"sample_count":samples.len(),
        "semantic_passed":passed,"acceptance_eligible":passed && bound,"binding":build_binding()["status"]}))?
    );
    Ok(passed)
}

fn cold_case(iteration: u32, workspace: &Path, audit_path: &Path) -> Sample {
    let mut sample = empty_sample(Case::Cold, iteration, workspace, audit_path);
    let executable = match env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            sample.fail("child_executable", error.to_string());
            return sample;
        }
    };
    let start = Instant::now();
    let output = Command::new(executable)
        .arg("__cold_case")
        .arg(iteration.to_string())
        .arg(workspace)
        .arg(audit_path)
        .output();
    let duration = elapsed_ns(start);
    match output {
        Ok(output) => {
            match serde_json::from_slice::<Sample>(&output.stdout) {
                Ok(child) => sample = child,
                Err(error) => sample.fail(
                    "child_record",
                    format!(
                        "{error}; stderr: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ),
                ),
            }
            sample.check(
                "fresh_child_process",
                sample.process_id != std::process::id(),
                format!(
                    "parent pid {}; child pid {}",
                    std::process::id(),
                    sample.process_id
                ),
            );
            sample.check(
                "child_exit_status",
                output.status.success(),
                format!("{}", output.status),
            );
            if !output.stderr.is_empty() {
                sample.check(
                    "child_stderr_empty",
                    false,
                    String::from_utf8_lossy(&output.stderr),
                );
            }
        }
        Err(error) => sample.fail("child_spawn", error.to_string()),
    }
    sample.timing.parent_spawn_to_exit_ns = Some(duration);
    sample
}
fn internal_case(args: Vec<OsString>, entry: Instant) -> Result<bool> {
    if args.len() != 3 {
        return Err("internal cold case needs iteration, workspace, and audit path".into());
    }
    let iteration: u32 = args[0].to_str().ok_or("invalid iteration")?.parse()?;
    if !(1..=1000).contains(&iteration) {
        return Err("internal iteration must be 1..=1000".into());
    }
    let workspace = absolute_path(Path::new(&args[1]))?;
    let audit = absolute_path(Path::new(&args[2]))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut sample = runtime.block_on(run_case(Case::Cold, iteration, &workspace, &audit));
    sample.timing.child_entry_to_record_ns = Some(elapsed_ns(entry));
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &sample)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(sample.semantic_passed)
}

fn summary(
    samples: &[Sample],
    passed: bool,
    bound: bool,
    run_id: &str,
    meta_bytes: &[u8],
    samples_sha256: &str,
) -> Value {
    let mut grouped: BTreeMap<&str, Vec<&Sample>> = BTreeMap::new();
    for sample in samples {
        grouped.entry(&sample.case).or_default().push(sample);
    }
    let cases: Vec<_> = grouped.into_iter().map(|(case, records)| {
        let cold = case == Case::Cold.name();
        let mut durations: Vec<_> = records.iter().filter_map(|sample|
            if cold { sample.timing.parent_spawn_to_exit_ns } else { sample.timing.case_execute_ns }).collect();
        durations.sort_unstable();
        let percentile = |percent: usize| durations.get((durations.len() * percent).div_ceil(100).saturating_sub(1)).copied();
        json!({"case":case,"samples":records.len(),"semantic_passed":records.iter().all(|sample| sample.semantic_passed),
            "measurement":if cold { "parent_spawn_to_exit_ns" } else { "case_execute_ns" },
            "duration_ns":{"measured_count":durations.len(),"min":durations.first(),"median":percentile(50),"p95":percentile(95),"max":durations.last()},
            "errors_observed":records.iter().filter(|sample| sample.error.is_some()).count(),
            "outcome_unknown_observed":records.iter().filter(|sample| sample.report.outcome_unknown).count(),
            "failed_sample_ids":records.iter().filter(|sample| !sample.semantic_passed).map(|sample| &sample.sample_id).collect::<Vec<_>>()})
    }).collect();
    json!({"schema_version":1,"run_id":run_id,"source":SOURCE,
        "sample_count":samples.len(),"semantic_passed":passed,
        "failed_samples":samples.iter().filter(|sample| !sample.semantic_passed).count(),
        "binding":build_binding(),"acceptance_eligible":bound && passed,
        "acceptance_status":if !passed { "semantic_failure" } else if !bound { "unbound_not_acceptance" } else { "bound_offline_semantics_passed" },
        "artifacts":{"meta":{"path":"meta.json","sha256":sha256(meta_bytes)},
            "samples":{"path":"samples.jsonl","sha256":samples_sha256}},
        "cases":cases,"parent_process_peak_rss":process_peak_rss(),
        "scope":"Only the measured local platform and synthetic fixed cases; no real Provider or full Agent validation, no Monty comparison or fastest-runtime claim.",
        "performance_thresholds":null,
    })
}
