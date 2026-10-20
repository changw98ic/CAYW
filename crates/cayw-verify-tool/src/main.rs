use cayw_verify_tool::{
    OFFLINE_SOURCE as SOURCE, cases, evidence::BundleWriter, measurement, runner, sdk,
};

use cases::{CASES, Case};
use measurement::{elapsed_ns, sha256};
use runner::{Sample, empty_sample, run_case, sample_id};
use serde::Serialize;
use serde_json::json;
use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitCode},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

type Result<T> = sdk::Result<T>;
const USAGE: &str = "cayw-verify-tool luau --out NEW_DIRECTORY --samples N [--workspace NEW_DIRECTORY] [--build-receipt FILE]\ncayw-verify-tool read BUNDLE_DIRECTORY\ncayw-verify-tool compare LEFT_DIRECTORY RIGHT_DIRECTORY\nN must be 1..=1000; default workspace is OUT/workspace. All fixture inputs are offline synthetic.\nBuild with CAYW_SOURCE_MANIFEST_PATH to measure explicitly listed source files. An optional post-build receipt checks recorded Cargo inputs and this executable.\nBuild input completeness remains pending; a source digest alone cannot establish it.";

struct Options {
    out: PathBuf,
    workspace: PathBuf,
    build_receipt: Option<PathBuf>,
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
        Some("__child_fixture") => {
            let mode = args.next().ok_or("child fixture mode")?;
            if mode == "normal" {
                std::io::stdout().write_all(b"fixture\n")?;
                Ok(true)
            } else if mode == "failure" {
                Ok(false)
            } else {
                let data = [b'x'; 8192];
                loop {
                    std::io::stdout().write_all(&data)?;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        Some("child-probe") => {
            let root = PathBuf::from(args.next().ok_or("child-probe EXISTING_DIRECTORY")?);
            if args.next().is_some() {
                return Err("child-probe takes one directory".into());
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    for mode in ["normal", "failure", "timeout", "drop"] {
                        let mut command = Command::new(env::current_exe()?);
                        command.arg("__child_fixture").arg(mode);
                        let mut child = cayw_verify_tool::child::ManagedChild::spawn(
                            command,
                            std::time::Duration::from_secs(1),
                            4096,
                            root.join(format!("{mode}.json")),
                        )?;
                        if mode == "drop" {
                            let future = child.wait();
                            tokio::pin!(future);
                            assert!(futures::poll!(&mut future).is_pending());
                        } else {
                            child.wait().await?;
                        }
                        drop(child);
                    }
                    Ok(!cayw_verify_tool::child::cleanup_unknown())
                })
        }
        Some("read") => {
            let root = args.next().ok_or("read BUNDLE_DIRECTORY")?;
            if args.next().is_some() {
                return Err("read takes one directory".into());
            }
            match cayw_verify_tool::evidence::Bundle::read(Path::new(&root)) {
                Ok(bundle) => {
                    println!(
                        "{}",
                        serde_json::to_string(&json!({"status":"valid","summary":bundle.summary}))?
                    );
                    Ok(true)
                }
                Err(error) => {
                    println!("{}", json!({"status":"invalid","reason":error.to_string()}));
                    Ok(false)
                }
            }
        }
        Some("compare") => {
            let left = args
                .next()
                .ok_or("compare LEFT_DIRECTORY RIGHT_DIRECTORY")?;
            let right = args
                .next()
                .ok_or("compare LEFT_DIRECTORY RIGHT_DIRECTORY")?;
            if args.next().is_some() {
                return Err("compare takes two directories".into());
            }
            match (
                cayw_verify_tool::evidence::Bundle::read(Path::new(&left)),
                cayw_verify_tool::evidence::Bundle::read(Path::new(&right)),
            ) {
                (Ok(left), Ok(right)) => {
                    let result = cayw_verify_tool::evidence::compare(&left, &right);
                    println!("{}", serde_json::to_string(&result)?);
                    Ok(result.status == "comparable")
                }
                (left, right) => {
                    println!(
                        "{}",
                        json!({"status":"invalid","left":left.err().map(|e|e.to_string()),"right":right.err().map(|e|e.to_string())})
                    );
                    Ok(false)
                }
            }
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
    let mut build_receipt = None;
    let mut samples = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("each option needs a value")?;
        match flag.to_str() {
            Some("--out") if out.is_none() => out = Some(absolute_path(Path::new(&value))?),
            Some("--build-receipt") if build_receipt.is_none() => {
                build_receipt = Some(absolute_path(Path::new(&value))?)
            }
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
        build_receipt,
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
    if cayw_verify_tool::child::cleanup_unknown() {
        return Err("previous child cleanup unconfirmed; sampling stopped".into());
    }
    let receipt = cayw_verify_tool::evidence::load_build_receipt(options.build_receipt.as_deref())?;
    prepare(&options)?;
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let binding = sdk::local_binding()?;
    let specifications: Vec<_> = CASES.iter().map(|case| {
        let sample = empty_sample(*case, 1, Path::new("<workspace>"), Path::new("<audit>"));
        json!({"name":case.name(),"specification":{"input":sample.input,"permissions":sample.permission,"configuration":sample.config}})
    }).collect();
    let meta = json!({
        "schema_version":2,"run_id":run_id,"source":SOURCE,"binding":binding,
        "samples_per_scenario":options.samples,"scenarios":specifications,
        "real_provider":{"status":"unverified","calls":0,"credentials_read":false},
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
            "not_measured":["complete Agent/model latency","OS boot-to-entry time","per-task RSS","Monty comparison"],
            "rss":"getrusage(RUSAGE_SELF).ru_maxrss where supported (macOS bytes, Linux KiB); cumulative process lifetime high-water mark, not a task or allocation delta; otherwise null",
            "performance_thresholds":null,
            "unmeasured_phase":"null, including phases skipped by setup failures",
        },
        "run_observations":{"parent_runtime_setup_ns":runtime_setup_ns,"parent_entry_to_runtime_ready_ns":entry_to_runtime_ready_ns},
        "artifacts":{"samples":"samples.jsonl","summary":"summary.json","audit":"audits/<iteration>-<case>.jsonl"},
        "acceptance":"offline task checks are limited to this recorded macOS host and fixture; source and optional build receipt retain remaining completeness gaps; no run declares whole-Issue acceptance",
    });
    let mut writer = BundleWriter::new(&options.out, meta, receipt.as_ref())?;
    let mut samples = Vec::with_capacity(options.samples as usize * CASES.len());
    let mut sampling_stopped = false;
    'sampling: for iteration in 1..=options.samples {
        let iteration_root = options.workspace.join(format!("iteration-{iteration:04}"));
        let iteration_setup = fs::create_dir(&iteration_root);
        for case in CASES {
            let mut report = writer.start_sample(case.name(), iteration)?;
            let sample_root = writer.sample_root(case.name(), iteration);
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
                Ok(()) if case == Case::Cold => cold_case(iteration, &workspace, &audit_path).await,
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
            report.process_id = sample.process_id;
            if let Some(repair) = sample.repair_record() {
                report.observe("repair_attempt", repair)?;
            }
            for check in &sample.semantic_checks {
                report.task_check(&check.name, check.passed, &check.detail);
            }
            for (phase, value) in serde_json::to_value(&sample.timing)?.as_object().unwrap() {
                if let Some(value) = value.as_u64() {
                    report.timing_ns.insert(phase.clone(), value);
                }
            }
            if cayw_verify_tool::child::cleanup_unknown() {
                report.check(
                    "original_child_cleanup_confirmed",
                    false,
                    "Cleanup or its durable receipt is unconfirmed",
                );
                sampling_stopped = true;
            }
            write_json_new(&sample_root.join("luau.json"), &sample)?;
            report.bind_attachment("luau", &sample_root, "luau.json")?;
            if audit_path.is_file() {
                let bytes = fs::read(&audit_path)?;
                fs::write(sample_root.join("audit.jsonl"), bytes)?;
                report.bind_attachment("audit", &sample_root, "audit.jsonl")?;
            }
            if case == Case::Cold && audit_path.with_extension("child.json").is_file() {
                fs::copy(
                    audit_path.with_extension("child.json"),
                    sample_root.join("child.json"),
                )?;
                report.bind_attachment("child", &sample_root, "child.json")?;
            }
            writer.complete_sample(report)?;
            samples.push(sample);
            if sampling_stopped {
                break 'sampling;
            }
        }
    }
    writer.finish(sampling_stopped)
}

async fn cold_case(iteration: u32, workspace: &Path, audit_path: &Path) -> Sample {
    let mut sample = empty_sample(Case::Cold, iteration, workspace, audit_path);
    let executable = match env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            sample.fail("child_executable", error.to_string());
            return sample;
        }
    };
    let start = Instant::now();
    let mut command = Command::new(executable);
    command
        .arg("__cold_case")
        .arg(iteration.to_string())
        .arg(workspace)
        .arg(audit_path);
    let output = match cayw_verify_tool::child::ManagedChild::spawn(
        command,
        sdk::DEADLINE * 3,
        1024 * 1024,
        audit_path.with_extension("child.json"),
    ) {
        Ok(mut child) => child.wait().await,
        Err(error) => Err(error),
    };
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
                sample.process_id != std::process::id() && sample.process_id == output.process_id,
                format!(
                    "parent pid {}; child pid {}",
                    std::process::id(),
                    sample.process_id
                ),
            );
            sample.check(
                "child_exit_status",
                output.success && !output.timed_out && output.cleanup_confirmed,
                format!(
                    "exit code {:?}; timed out {}",
                    output.exit_code, output.timed_out
                ),
            );
            if output.stderr_bytes != 0 {
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
