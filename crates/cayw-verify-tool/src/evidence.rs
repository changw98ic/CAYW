//! One evidence writer and reader for SDK, IPC and direct Luau measurements.
use crate::{
    measurement::sha256,
    sdk::{Result, ScenarioReport},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

const MAX_ATTACHMENT: u64 = 64 * 1024 * 1024;

use crate::provenance::build_receipt::{
    BuildReceipt, ReceiptVerification, verify_current_receipt, verify_receipt_binding,
};

/// Read an explicitly supplied receipt and check it against this running program
/// before a runner allocates its output or starts any measured work.
pub fn load_build_receipt(path: Option<&Path>) -> Result<Option<BuildReceipt>> {
    let receipt = path
        .map(|path| -> Result<BuildReceipt> {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            };
            let parent = absolute.parent().ok_or("build receipt parent absent")?;
            let name = absolute
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("build receipt filename absent")?;
            Ok(serde_json::from_slice(&attachment(parent, name)?)?)
        })
        .transpose()?;
    verify_current_receipt(receipt.as_ref())?;
    Ok(receipt)
}

fn check_source_claims(binding: &Value) -> Result<()> {
    if binding["build"]["source_verified_by_binary"] != false
        || binding["build"]["build_input_completeness"] != "pending"
    {
        return Err("a source flag cannot establish complete build inputs".into());
    }
    Ok(())
}

fn verify_bundle_build(root: &Path, meta: &Value) -> Result<ReceiptVerification> {
    check_source_claims(&meta["binding"])?;
    let receipt: Option<BuildReceipt> = meta["artifacts"]
        .get("build_receipt")
        .map(|descriptor| -> Result<BuildReceipt> {
            Ok(serde_json::from_slice(&bound_file(root, descriptor)?)?)
        })
        .transpose()?;
    let checked = verify_receipt_binding(
        receipt.as_ref(),
        meta["binding"]["executable"]["sha256"]
            .as_str()
            .unwrap_or(""),
        &meta["binding"]["build"],
    )?;
    if meta["build_input_verification"] != serde_json::to_value(&checked)? {
        return Err("build input verification contradicts its recorded inputs".into());
    }
    Ok(checked)
}

pub fn attachment(root: &Path, relative: &str) -> Result<Vec<u8>> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("attachment must be a normal relative path".into());
    }
    let root = root.canonicalize()?;
    let mut checked = root.clone();
    for component in path.components() {
        checked.push(component);
        if fs::symlink_metadata(&checked)?.file_type().is_symlink() {
            return Err("attachment symlinks are forbidden".into());
        }
    }
    let metadata = fs::metadata(&checked)?;
    if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT {
        return Err("attachment is not a bounded regular file".into());
    }
    Ok(fs::read(checked)?)
}
fn json_file(root: &Path, name: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&attachment(root, name)?)?)
}
fn bound_file(root: &Path, descriptor: &Value) -> Result<Vec<u8>> {
    let bytes = attachment(
        root,
        descriptor["path"]
            .as_str()
            .ok_or("attachment path absent")?,
    )?;
    if descriptor["sha256"].as_str() != Some(sha256(&bytes).as_str()) {
        return Err("attachment digest mismatch".into());
    }
    Ok(bytes)
}
fn write_new(root: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(name))?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    if file.metadata()?.len() > MAX_ATTACHMENT {
        return Err(format!(
            "{name} exceeds the reader's {MAX_ATTACHMENT}-byte limit; raw evidence retained"
        )
        .into());
    }
    Ok(())
}

/// Derived values are recomputed by both the writer and reader. Timings are
/// grouped by scenario so unlike operations never become one benchmark.
fn derived(samples: &[Value]) -> Result<Value> {
    let mut phases = BTreeMap::<(&str, &str), Vec<u64>>::new();
    for sample in samples {
        let scenario = sample["scenario"].as_str().ok_or("scenario absent")?;
        for (phase, value) in sample["timing_ns"]
            .as_object()
            .ok_or("raw timings absent")?
        {
            phases
                .entry((scenario, phase))
                .or_default()
                .push(value.as_u64().ok_or("invalid duration")?);
        }
    }
    let timings: Vec<Value> = phases
        .into_iter()
        .map(|((scenario, phase), mut values)| {
            values.sort_unstable();
            let percentile = |percent: usize| {
                values
                    .get((values.len() * percent).div_ceil(100).saturating_sub(1))
                    .copied()
            };
            json!({"scenario":scenario,"phase":phase,"unit":"ns","measured_count":values.len(),
            "min":values.first(),"median":percentile(50),"p95":percentile(95),"max":values.last()})
        })
        .collect();
    let mut repairs = BTreeMap::<(u64, String), Vec<Value>>::new();
    for sample in samples {
        for observation in sample["observations"]
            .as_array()
            .ok_or("observations absent")?
        {
            if observation["kind"] != "repair_attempt" {
                continue;
            }
            let attempt = &observation["value"];
            let iteration = attempt["iteration"]
                .as_u64()
                .ok_or("repair iteration absent")?;
            let pair = attempt["repair"]["pair_id"]
                .as_str()
                .ok_or("repair pair absent")?;
            repairs
                .entry((iteration, pair.to_owned()))
                .or_default()
                .push(attempt.clone());
        }
    }
    let repair_summary = repairs.into_iter().map(|((iteration,pair_id),attempts)| -> Result<Value> {
        let total = |field: &str| -> Result<Option<u64>> {
            let mut sum = Some(0_u64);
            for attempt in &attempts {
                sum = match (sum,attempt[field].as_u64()) {
                    (Some(a),Some(b)) => Some(a.checked_add(b).ok_or("repair duration overflow")?),
                    _ => None,
                };
            }
            Ok(sum)
        };
        Ok(json!({"pair_id":pair_id,"iteration":iteration,
            "total_case_execute_ns":total("case_execute_ns")?,"total_case_total_ns":total("case_total_ns")?,
            "usage":{"status":"not_measured","total_tokens":null},
            "cost":{"status":"not_measured","amount":null,"currency":null},"attempts":attempts}))
    }).collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"sample_count":samples.len(),"semantic_passed":!samples.is_empty() && samples.iter().all(|s|s["status"]=="passed"),
        "repair_summary":repair_summary,
        "failed_samples":samples.iter().filter(|s|s["status"]=="failed").count(),
        "unsupported_samples":samples.iter().filter(|s|s["status"]=="unsupported").count(),
        "unverified_samples":samples.iter().filter(|s|s["status"]=="unverified").count(),
        "statuses":samples.iter().map(|s|json!({"sample_id":s["sample_id"],"status":s["status"],"timing_ns":s["timing_ns"],
            "failed_checks":s["checks"].as_array().map(|checks|checks.iter().filter(|c|c["passed"]!=true).collect::<Vec<_>>())})).collect::<Vec<_>>(),
        "timing_summary":timings}),
    )
}

pub struct BundleWriter {
    root: PathBuf,
    meta: Value,
    index: File,
    samples: Vec<Value>,
}
impl BundleWriter {
    /// The caller allocates a new root. All evidence files use create_new.
    pub fn new(root: &Path, mut meta: Value, receipt: Option<&BuildReceipt>) -> Result<Self> {
        if meta["schema_version"] != 2 || !meta["scenarios"].is_array() {
            return Err("writer needs the current evidence contract".into());
        }
        check_source_claims(&meta["binding"])?;
        let checked = verify_current_receipt(receipt)?;
        if meta["binding"]["build"] != crate::provenance::build_binding()
            || meta["binding"]["executable"] != crate::provenance::executable()?
        {
            return Err("writer binding differs from its actual running program".into());
        }
        if let Some(receipt) = receipt {
            write_new(root, "build-receipt.json", receipt)?;
            meta["artifacts"]["build_receipt"] = json!({"path":"build-receipt.json","sha256":sha256(&attachment(root,"build-receipt.json")?)});
        }
        meta["build_input_verification"] = serde_json::to_value(&checked)?;
        write_new(root, "meta.json", &meta)?;
        fs::create_dir(root.join("samples"))?;
        let index = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("samples.jsonl"))?;
        Ok(Self {
            root: root.to_path_buf(),
            meta,
            index,
            samples: vec![],
        })
    }
    pub fn sample_root(&self, scenario: &str, iteration: u32) -> PathBuf {
        self.root
            .join("samples")
            .join(format!("{iteration:04}/{scenario}"))
    }
    pub fn start_sample(&self, scenario: &str, iteration: u32) -> Result<ScenarioReport> {
        if scenario.is_empty()
            || !scenario
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || iteration == 0
            || self.meta["samples_per_scenario"]
                .as_u64()
                .is_none_or(|n| u64::from(iteration) > n)
        {
            return Err("invalid scenario or iteration".into());
        }
        let spec = self.meta["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == scenario)
            .ok_or("undeclared scenario")?;
        let root = self.sample_root(scenario, iteration);
        if !root.parent().ok_or("sample parent absent")?.exists() {
            fs::create_dir(root.parent().unwrap())?;
        }
        fs::create_dir(&root)?;
        let mut report = ScenarioReport::new(
            scenario,
            iteration,
            spec["specification"].clone(),
            self.meta["binding"].clone(),
        );
        report.attach_records(&root.join("records.jsonl"))?;
        Ok(report)
    }
    pub fn complete_sample(&mut self, mut sample: ScenarioReport) -> Result<()> {
        let root = self.sample_root(&sample.scenario, sample.iteration);
        sample.finish();
        sample.persist_terminal()?;
        sample.bind_records(&root.join("records.jsonl"))?;
        write_new(&root, "report.json", &sample)?;
        for descriptor in sample
            .artifacts
            .as_object()
            .ok_or("artifacts absent")?
            .values()
        {
            bound_file(&root, descriptor)?;
        }
        let mut value = serde_json::to_value(sample)?;
        value["run_id"] = self.meta["run_id"].clone();
        value["meta"] = "meta.json".into();
        let mut encoded = serde_json::to_vec(&value)?;
        encoded.push(b'\n');
        if self
            .index
            .metadata()?
            .len()
            .checked_add(encoded.len() as u64)
            .is_none_or(|bytes| bytes > MAX_ATTACHMENT)
        {
            return Err(format!(
                "samples.jsonl exceeds the reader's {MAX_ATTACHMENT}-byte limit; sampling stopped and raw evidence retained"
            )
            .into());
        }
        self.index.write_all(&encoded)?;
        self.index.sync_all()?;
        self.samples.push(value);
        Ok(())
    }
    pub fn finish(self, sampling_stopped: bool) -> Result<bool> {
        let mut summary = derived(&self.samples)?;
        let passed = summary["semantic_passed"] == true && !sampling_stopped;
        summary["semantic_passed"] = passed.into();
        summary["schema_version"] = 2.into();
        summary["run_id"] = self.meta["run_id"].clone();
        summary["source"] = self.meta["source"].clone();
        summary["binding"] = self.meta["binding"].clone();
        summary["sampling_stopped"] = sampling_stopped.into();
        summary["issue_2_complete"] = false.into();
        summary["real_provider"] = self.meta["real_provider"].clone();
        summary["source_bound"] = false.into();
        summary["self_verified_source"] = false.into();
        summary["build_input_verification"] =
            serde_json::to_value(verify_bundle_build(&self.root, &self.meta)?)?;
        summary["artifacts"] = json!({"meta_sha256":sha256(&fs::read(self.root.join("meta.json"))?),
            "samples_sha256":sha256(&fs::read(self.root.join("samples.jsonl"))?)});
        summary["performance_thresholds"] = Value::Null;
        write_new(&self.root, "summary.json", &summary)?;
        println!(
            "{}",
            json!({"out":self.root,"sample_count":self.samples.len(),"semantic_passed":passed,"issue_2_complete":false})
        );
        Ok(passed)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub root: PathBuf,
    pub meta: Value,
    pub summary: Value,
    pub samples: Vec<Value>,
}
impl Bundle {
    pub fn read(root: &Path) -> Result<Self> {
        let meta_bytes = attachment(root, "meta.json")?;
        let sample_bytes = attachment(root, "samples.jsonl")?;
        let meta: Value = serde_json::from_slice(&meta_bytes)?;
        let summary = json_file(root, "summary.json")?;
        if meta["schema_version"] != 2 || summary["schema_version"] != 2 {
            return Err("only evidence schema v2 is accepted".into());
        }
        if summary["run_id"] != meta["run_id"]
            || summary["binding"] != meta["binding"]
            || summary["source"] != meta["source"]
            || summary["artifacts"]["meta_sha256"] != sha256(&meta_bytes)
            || summary["artifacts"]["samples_sha256"] != sha256(&sample_bytes)
        {
            return Err("bundle provenance or index digest contradicts the raw input".into());
        }
        let build = verify_bundle_build(root, &meta)?;
        if summary["source_bound"] != false
            || summary["self_verified_source"] != false
            || summary["build_input_verification"] != serde_json::to_value(build)?
        {
            return Err("summary source claims contradict the build evidence".into());
        }
        let samples: Vec<Value> = std::str::from_utf8(&sample_bytes)?
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        if samples.is_empty() || summary["sample_count"] != samples.len() {
            return Err("sample count is absent or contradicts the samples".into());
        }
        let mut ids = BTreeSet::new();
        let specifications = meta["scenarios"]
            .as_array()
            .ok_or("scenario declarations absent")?;
        let mut declared = BTreeSet::new();
        for spec in specifications {
            if !declared.insert(spec["name"].as_str().ok_or("scenario name absent")?) {
                return Err("duplicate scenario declaration".into());
            }
        }
        for sample in &samples {
            let parsed: ScenarioReport = serde_json::from_value(sample.clone())?;
            let spec = specifications
                .iter()
                .find(|s| s["name"] == parsed.scenario)
                .ok_or("sample has no declaration")?;
            if parsed.schema_version != 2
                || sample["run_id"] != meta["run_id"]
                || sample["meta"] != "meta.json"
                || sample["source"] != meta["source"]
                || sample["binding"] != meta["binding"]
                || parsed.specification != spec["specification"]
                || parsed.iteration == 0
                || parsed.sample_id != format!("{:04}/{}", parsed.iteration, parsed.scenario)
                || !ids.insert(parsed.sample_id.clone())
            {
                return Err("sample identity or specification contradicts the bundle".into());
            }
            let directory = format!("samples/{}", parsed.sample_id);
            let stored = json_file(root, &format!("{directory}/report.json"))?;
            let mut indexed = sample.clone();
            indexed
                .as_object_mut()
                .ok_or("sample must be object")?
                .remove("run_id");
            indexed.as_object_mut().unwrap().remove("meta");
            if stored != indexed {
                return Err("sample index and report disagree".into());
            }
            let raw_root = root.join(&directory);
            let raw = bound_file(&raw_root, &parsed.artifacts["records"])?;
            let observations: Vec<Value> = std::str::from_utf8(&raw)?
                .lines()
                .map(serde_json::from_str)
                .collect::<std::result::Result<_, _>>()?;
            if sample["observations"].as_array() != Some(&observations)
                || observations
                    .iter()
                    .enumerate()
                    .any(|(i, v)| v["sequence"] != i + 1)
            {
                return Err("raw observations disagree or are not contiguous".into());
            }
            let start = observations.first().ok_or("scenario start absent")?;
            if start["kind"] != "scenario_start"
                || start["value"]["sample_id"] != sample["sample_id"]
                || start["value"]["specification"] != parsed.specification
                || start["value"]["binding"] != parsed.binding
            {
                return Err("raw start contradicts the sample specification".into());
            }
            let event_pages: Vec<_> = observations
                .iter()
                .filter(|o| o["kind"] == "event_page")
                .map(|o| o["value"].clone())
                .collect();
            if sample["event_pages"].as_array() != Some(&event_pages) {
                return Err("event content differs from its raw records".into());
            }
            let terminal = observations.last().ok_or("terminal observation absent")?;
            let mut artifacts = parsed.artifacts.clone();
            artifacts
                .as_object_mut()
                .ok_or("artifacts absent")?
                .remove("records");
            if terminal["kind"] != "scenario_terminal"
                || terminal["value"]["status"] != sample["status"]
                || terminal["value"]["checks"] != sample["checks"]
                || terminal["value"]["task_checks"] != sample["task_checks"]
                || terminal["value"]["timing_ns"] != sample["timing_ns"]
                || terminal["value"]["process_peak_rss"] != sample["process_peak_rss"]
                || terminal["value"]["artifacts"] != artifacts
            {
                return Err("raw terminal contradicts report".into());
            }
            if !matches!(
                parsed.status.as_str(),
                "passed" | "failed" | "unverified" | "unsupported"
            ) {
                return Err("unknown sample status".into());
            }
            if parsed.status == "passed"
                && (!parsed.has_task_checks() || parsed.checks.iter().any(|c| !c.passed))
            {
                return Err("passed sample has no actual task checks or has failed checks".into());
            }
            for descriptor in parsed.artifacts.as_object().unwrap().values() {
                bound_file(&raw_root, descriptor)?;
            }
            crate::sdk::validate_recorded_sample(&parsed, &raw_root)?;
        }
        validate_repair_pairs(root, &samples)?;
        let expected = derived(&samples)?;
        for (key, value) in expected.as_object().unwrap() {
            if let Some(stored) = summary.get(key)
                && stored != value
            {
                return Err(format!("summary {key} contradicts raw samples").into());
            }
        }
        Ok(Self {
            root: root.canonicalize()?,
            meta,
            summary,
            samples,
        })
    }
}

fn validate_repair_pairs(root: &Path, samples: &[Value]) -> Result<()> {
    for (index, sample) in samples
        .iter()
        .enumerate()
        .filter(|(_, sample)| sample["scenario"] == "syntax_repair_success")
    {
        let previous = index
            .checked_sub(1)
            .and_then(|index| samples.get(index))
            .ok_or("repair has no original failed sample")?;
        if previous["scenario"] != "syntax_failure" || previous["iteration"] != sample["iteration"]
        {
            return Err("repair is not immediately paired with its original failure".into());
        }
        let load = |value: &Value| -> Result<crate::runner::Sample> {
            let directory = root.join("samples").join(
                value["sample_id"]
                    .as_str()
                    .ok_or("sample identity absent")?,
            );
            Ok(serde_json::from_slice(&bound_file(
                &directory,
                &value["artifacts"]["luau"],
            )?)?)
        };
        let failed = load(previous)?;
        let repaired = load(sample)?;
        for (raw, case) in [
            (&failed, crate::cases::Case::SyntaxFailure),
            (&repaired, crate::cases::Case::SyntaxRepair),
        ] {
            let expected = crate::runner::empty_sample(
                case,
                raw.iteration,
                Path::new(&raw.workspace),
                Path::new(&raw.audit_path),
            );
            if serde_json::to_value(&raw.repair)? != serde_json::to_value(&expected.repair)? {
                return Err(
                    "repair attempt lost its original pair identity or recorded change".into(),
                );
            }
        }
        if failed.workspace != repaired.workspace
            || failed.execution_status != "error"
            || failed
                .error
                .as_ref()
                .is_none_or(|error| error.kind != "script")
            || repaired.execution_status != "success"
            || repaired.error.is_some()
            || failed.iteration != repaired.iteration
        {
            return Err(
                "repair no longer follows the original syntax failure in the same workspace".into(),
            );
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Comparison {
    pub status: String,
    pub reasons: Vec<String>,
    pub left_binding: Value,
    pub right_binding: Value,
    pub measurements: Value,
}
/// Known differences reject comparison even when other evidence is pending.
pub fn compare(left: &Bundle, right: &Bundle) -> Comparison {
    let mut reasons = vec![];
    let mut missing = vec![];
    for key in ["source", "scenarios", "measurement"] {
        if left.meta[key].is_null() || right.meta[key].is_null() {
            missing.push(format!("missing {key}"));
        } else if left.meta[key] != right.meta[key] {
            reasons.push(format!("different {key}"));
        }
    }
    for key in ["platform", "protocol"] {
        if left.meta["binding"][key].is_null() || right.meta["binding"][key].is_null() {
            missing.push(format!("missing {key}"));
        } else if left.meta["binding"][key] != right.meta["binding"][key] {
            reasons.push(format!("different {key}"));
        }
    }
    for key in ["profile", "toolchain", "build_target", "opt_level"] {
        let a = &left.meta["binding"]["build"][key];
        let b = &right.meta["binding"]["build"][key];
        if a.is_null() || b.is_null() {
            missing.push(format!("missing build {key}"));
        } else if a != b {
            reasons.push(format!("different build {key}"));
        }
    }
    for (side, bundle) in [("left", left), ("right", right)] {
        match verify_bundle_build(&bundle.root, &bundle.meta) {
            Ok(checked) => {
                missing.extend(checked.gaps.into_iter().map(|gap| format!("{side} {gap}")))
            }
            Err(error) => {
                return Comparison {
                    status: "invalid".into(),
                    reasons: vec![format!("{side} build evidence: {error}")],
                    left_binding: left.meta["binding"].clone(),
                    right_binding: right.meta["binding"].clone(),
                    measurements: Value::Null,
                };
            }
        }
        if bundle.samples.iter().any(|s| s["status"] != "passed") {
            missing.push(format!("{side} task has no passing result"));
        }
        for key in [
            "timing_summary",
            "statuses",
            "failed_samples",
            "unsupported_samples",
            "unverified_samples",
        ] {
            if bundle.summary.get(key).is_none() {
                missing.push(format!("{side} missing {key}"));
            }
        }
        let count = bundle.meta["samples_per_scenario"].as_u64();
        if count.is_none_or(|n| n < 3) {
            missing.push(format!(
                "{side} needs at least three repetitions per scenario"
            ));
        }
        if let (Some(count), Some(scenarios)) = (count, bundle.meta["scenarios"].as_array()) {
            for scenario in scenarios {
                for iteration in 1..=count {
                    if !bundle
                        .samples
                        .iter()
                        .any(|s| s["scenario"] == scenario["name"] && s["iteration"] == iteration)
                    {
                        missing.push(format!("{side} incomplete scenario repetitions"));
                        break;
                    }
                }
            }
        }
        if bundle
            .samples
            .iter()
            .any(|s| s["timing_ns"].as_object().is_none_or(|t| t.is_empty()))
        {
            missing.push(format!("{side} raw measurements absent"));
        }
    }
    let status = if !reasons.is_empty() {
        "incomparable"
    } else if !missing.is_empty() {
        "pending"
    } else {
        "comparable"
    };
    reasons.extend(missing);
    Comparison {
        status: status.into(),
        reasons,
        left_binding: left.meta["binding"].clone(),
        right_binding: right.meta["binding"].clone(),
        measurements: json!({"left":left.summary["timing_summary"],"right":right.summary["timing_summary"],"thresholds":null}),
    }
}
