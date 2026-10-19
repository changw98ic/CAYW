use crate::{
    cases::{Case, PIPELINE_CONTENT, REPAIR_CONTENT},
    fixtures::{
        AuditEvent, BlockingFixture, BlockingObservation, FixtureAudit, FixtureGate,
        PermissionRecord, SOURCE, lock,
    },
    measurement::{ProcessPeakRss, elapsed_ns, process_peak_rss, sha256},
};
use cayw_ai::CancellationToken;
use cayw_codemode::{CallStatus, CodeMode, CodeModeError, ErrorKind, ExecutionReport, Limits};
use cayw_host::{Host, HostConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Instant};

#[derive(Debug, Serialize, Deserialize)]
pub struct RecordedError {
    pub kind: String,
    pub message: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Timings {
    pub host_setup_ns: Option<u64>,
    pub codemode_setup_ns: Option<u64>,
    pub first_execute_ns: Option<u64>,
    pub warmup_execute_ns: Option<u64>,
    pub case_execute_ns: Option<u64>,
    pub child_entry_to_record_ns: Option<u64>,
    pub parent_spawn_to_exit_ns: Option<u64>,
    pub case_total_ns: Option<u64>,
    // The public CodeMode API does not expose a separate VM setup timer.
    pub vm_setup_only_ns: Option<u64>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct RepairPair {
    pub pair_id: String,
    pub attempt: u32,
    pub previous_sample_id: Option<String>,
    pub change: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Sample {
    pub schema_version: u32,
    pub sample_id: String,
    pub case: String,
    pub iteration: u32,
    pub source: String,
    pub process_id: u32,
    pub input: Value,
    pub permission: Value,
    pub config: Value,
    pub workspace: String,
    pub audit_path: String,
    pub execution_status: String,
    pub error: Option<RecordedError>,
    pub report: ExecutionReport,
    pub host_outcome_unknown: bool,
    pub host_audit_failure: Option<String>,
    pub permission_decisions: Vec<PermissionRecord>,
    pub audit_events: Vec<AuditEvent>,
    pub warmup: Option<ExecutionReport>,
    pub cancellation: Option<BlockingObservation>,
    pub repair: Option<RepairPair>,
    pub timing: Timings,
    pub process_peak_rss: Option<ProcessPeakRss>,
    pub semantic_checks: Vec<Check>,
    pub semantic_passed: bool,
}
impl Sample {
    pub fn check(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        self.semantic_checks.push(Check {
            name: name.into(),
            passed,
            detail: detail.into(),
        });
        self.semantic_passed = self.semantic_checks.iter().all(|check| check.passed);
    }
    pub fn fail(&mut self, kind: &str, message: impl Into<String>) {
        let message = message.into();
        self.execution_status = "error".into();
        self.error = Some(RecordedError {
            kind: kind.into(),
            message: message.clone(),
        });
        self.check(kind, false, message);
    }
}

pub fn limits_json(limits: &Limits) -> Value {
    json!({
        "memory_bytes":limits.memory_bytes,
        "instruction_checkpoint_budget":limits.instruction_budget,
        "timeout_ns":u64::try_from(limits.timeout.as_nanos()).unwrap_or(u64::MAX),
        "shutdown_grace_ns":u64::try_from(limits.shutdown_grace.as_nanos()).unwrap_or(u64::MAX),
        "host_calls":limits.host_calls,
        "output_bytes":limits.output_bytes,
    })
}
pub fn config_json(case: Case) -> Value {
    let host = HostConfig::default();
    json!({
        "codemode":limits_json(&case.limits()),
        "host":{
            "max_argument_bytes":host.max_argument_bytes,
            "max_result_bytes":host.max_result_bytes,
            "max_total_bytes":host.max_total_bytes,
            "max_calls":host.max_calls,
            "max_file_bytes":host.max_file_bytes,
            "max_list_entries":host.max_list_entries,
            "call_timeout_ns":u64::try_from(host.call_timeout.as_nanos()).unwrap_or(u64::MAX),
            "process_timeout_ns":u64::try_from(host.process_timeout.as_nanos()).unwrap_or(u64::MAX),
            "max_process_output_bytes":host.max_process_output_bytes,
            "data":host.data.map(|limits| json!({
                "input_bytes":limits.input_bytes,"output_bytes":limits.output_bytes,
                "max_depth":limits.max_depth,"max_nodes":limits.max_nodes,
                "max_rows":limits.max_rows,"max_diff_work":limits.max_diff_work,
            })),
            "workspace":host.workspace.as_ref().map(|limits| json!({
                "max_file_bytes":limits.max_file_bytes,"max_batch_bytes":limits.max_batch_bytes,
                "max_batch_operations":limits.max_batch_operations,"max_batches":limits.max_batches,
                "max_tree_entries":limits.max_tree_entries,"max_depth":limits.max_depth,
                "max_argument_bytes":limits.max_argument_bytes,"max_output_bytes":limits.max_output_bytes,
                "max_archive_bytes":limits.max_archive_bytes,"max_archive_entries":limits.max_archive_entries,
                "max_archive_expanded_bytes":limits.max_archive_expanded_bytes,
                "max_compression_ratio":limits.max_compression_ratio,
                "timeout_ns":u64::try_from(limits.timeout.as_nanos()).unwrap_or(u64::MAX),
                // This function records the unmodified HostConfig::default() used
                // below; it does not introspect an arbitrary SyntaxValidator trait.
                "syntax_validator":"cayw_workspace::JsonTomlValidator",
            })),
            "git_program":null,"profiles":{},
            "external":if case == Case::Cancel { json!({"source":SOURCE,"name":"blocking search.query fixture; no network"}) } else { Value::Null },
        },
        "runtime":"tokio current_thread",
        "warmup_script":if case == Case::Warm { Some("return {warmup = true}") } else { None },
    })
}
pub fn sample_id(case: Case, iteration: u32) -> String {
    format!("{iteration:04}/{}", case.name())
}
pub fn empty_sample(case: Case, iteration: u32, workspace: &Path, audit_path: &Path) -> Sample {
    let script = case.script();
    let grants: Vec<_> = case
        .grants()
        .into_iter()
        .map(|(capability, arguments)| json!({"capability":capability,"arguments":arguments}))
        .collect();
    let repair = match case {
        Case::SyntaxFailure | Case::SyntaxRepair => Some(RepairPair {
            pair_id: format!("{iteration:04}/syntax-repair"),
            attempt: if case == Case::SyntaxFailure { 1 } else { 2 },
            previous_sample_id: (case == Case::SyntaxRepair).then(|| sample_id(Case::SyntaxFailure, iteration)),
            change: (case == Case::SyntaxRepair).then(|| "Replace the incomplete `local read =` with `cayw.call(\"fs.read\", {path = \"repair.txt\"})`; execute the complete corrected script in the same workspace.".into()),
        }),
        _ => None,
    };
    Sample {
        schema_version: 1,
        sample_id: sample_id(case, iteration),
        case: case.name().into(),
        iteration,
        source: SOURCE.into(),
        process_id: std::process::id(),
        input: json!({"script_sha256":sha256(script.as_bytes()),"script":script,
        "fixture_files":[],"expected_error":case.expected_error(),
        "expected_output":match case {
            Case::Files => json!({"path":"pipeline.txt","content":PIPELINE_CONTENT,"sha256":sha256(PIPELINE_CONTENT.as_bytes())}),
            Case::SyntaxRepair => json!({"path":"repair.txt","content":REPAIR_CONTENT,"sha256":sha256(REPAIR_CONTENT.as_bytes())}),
            _ => Value::Null,
        }}),
        permission: json!({"source":SOURCE,"policy":"exact capability plus exact validated arguments; default deny",
            "grants":grants,"product_approval":false}),
        config: config_json(case),
        workspace: workspace.display().to_string(),
        audit_path: audit_path.display().to_string(),
        execution_status: "not_started".into(),
        error: None,
        report: ExecutionReport::default(),
        host_outcome_unknown: false,
        host_audit_failure: None,
        permission_decisions: vec![],
        audit_events: vec![],
        warmup: None,
        cancellation: None,
        repair,
        timing: Timings::default(),
        process_peak_rss: None,
        semantic_checks: vec![],
        semantic_passed: false,
    }
}

pub async fn run_case(case: Case, iteration: u32, workspace: &Path, audit_path: &Path) -> Sample {
    let start = Instant::now();
    let mut sample = empty_sample(case, iteration, workspace, audit_path);
    let gate = Arc::new(FixtureGate {
        grants: case.grants(),
        decisions: Default::default(),
    });
    let audit = match FixtureAudit::create(audit_path) {
        Ok(audit) => Arc::new(audit),
        Err(error) => {
            sample.fail("audit_setup", error.to_string());
            sample.timing.case_total_ns = Some(elapsed_ns(start));
            return sample;
        }
    };
    let blocker = Arc::new(BlockingFixture::default());
    let config = HostConfig {
        external: (case == Case::Cancel)
            .then(|| blocker.clone() as Arc<dyn cayw_host::ExternalHost>),
        ..HostConfig::default()
    };
    let setup = Instant::now();
    let host = match Host::new(workspace, gate.clone(), audit.clone(), config) {
        Ok(host) => Arc::new(host),
        Err(error) => {
            sample.fail("host_setup", error.message);
            sample.timing.host_setup_ns = Some(elapsed_ns(setup));
            sample.timing.case_total_ns = Some(elapsed_ns(start));
            return sample;
        }
    };
    sample.timing.host_setup_ns = Some(elapsed_ns(setup));
    let setup = Instant::now();
    let mode = match CodeMode::new(host.clone(), case.limits()) {
        Ok(mode) => mode,
        Err(error) => {
            sample.fail("codemode_setup", error.to_string());
            sample.timing.codemode_setup_ns = Some(elapsed_ns(setup));
            sample.timing.case_total_ns = Some(elapsed_ns(start));
            return sample;
        }
    };
    sample.timing.codemode_setup_ns = Some(elapsed_ns(setup));
    if case == Case::Warm {
        let first = Instant::now();
        match mode
            .execute("return {warmup = true}", CancellationToken::new())
            .await
        {
            Ok(report) => sample.warmup = Some(report),
            Err(error) => sample.check("warmup_success", false, error.to_string()),
        }
        sample.timing.warmup_execute_ns = Some(elapsed_ns(first));
        sample.timing.first_execute_ns = sample.timing.warmup_execute_ns;
    }
    let script = case.script();
    let execute = Instant::now();
    let result = if case == Case::Cancel {
        let cancel = CancellationToken::new();
        let mut cancel_start = None;
        let future = mode.execute(&script, cancel.clone());
        tokio::pin!(future);
        let result = tokio::select! {
            result = &mut future => result,
            () = blocker.entered.notified() => {
                {
                    let mut state = lock(&blocker.state);
                    state.cancellation_requested_after_entry = state.entered && state.active;
                }
                cancel_start = Some(Instant::now());
                cancel.cancel();
                future.await
            },
        };
        // CodeMode/Host must settle synchronously before the execute future returns.
        let mut observation = lock(&blocker.state).clone();
        observation.cancel_to_execute_return_ns = cancel_start.map(elapsed_ns);
        observation.settled_before_execute_return =
            audit.events().iter().any(|event| event.event == "settled");
        sample.cancellation = Some(observation);
        result
    } else {
        mode.execute(&script, CancellationToken::new()).await
    };
    sample.timing.case_execute_ns = Some(elapsed_ns(execute));
    if case != Case::Warm {
        sample.timing.first_execute_ns = sample.timing.case_execute_ns;
    }
    let kind = match result {
        Ok(report) => {
            sample.report = report;
            sample.execution_status = "success".into();
            None
        }
        Err(CodeModeError {
            kind,
            message,
            report,
        }) => {
            sample.report = *report;
            sample.execution_status = "error".into();
            sample.error = Some(RecordedError {
                kind: serde_json::to_value(kind)
                    .expect("serialize error kind")
                    .as_str()
                    .expect("kind is a string")
                    .into(),
                message,
            });
            Some(kind)
        }
    };
    sample.permission_decisions = lock(&gate.decisions).clone();
    sample.audit_events = audit.events();
    sample.host_outcome_unknown = host.outcome_unknown();
    sample.host_audit_failure = host.audit_failure();
    validate(&mut sample, case, kind);
    sample.timing.case_total_ns = Some(elapsed_ns(start));
    sample.process_peak_rss = process_peak_rss();
    sample
}

fn validate(sample: &mut Sample, case: Case, kind: Option<ErrorKind>) {
    sample.check(
        "expected_execution_result",
        kind == case.expected_error(),
        format!("expected {:?}; observed {kind:?}", case.expected_error()),
    );
    sample.check(
        "expected_call_count",
        sample.report.calls.len() == case.expected_calls(),
        format!(
            "expected {}; observed {}",
            case.expected_calls(),
            sample.report.calls.len()
        ),
    );
    sample.check(
        "no_audit_persistence_failure",
        sample.host_audit_failure.is_none(),
        format!("Host::audit_failure = {:?}", sample.host_audit_failure),
    );
    let expected = case.expected_operations();
    let calls_match = sample.report.calls.len() == expected.len()
        && sample.report.calls.iter().zip(&expected).enumerate().all(
            |(index, (call, (name, arguments)))| {
                call.sequence == index + 1
                    && call.name == *name
                    && call.arguments.as_ref() == Some(arguments)
            },
        );
    sample.check(
        "expected_call_sequence",
        calls_match,
        format!("Report sequence, names and exact arguments must match {expected:?}."),
    );
    let decision = if case == Case::GateDenial {
        "fixture_denial"
    } else {
        "fixture_grant"
    };
    let permissions_match =
        sample.permission_decisions.len() == expected.len()
            && sample.permission_decisions.iter().zip(&expected).all(
                |(record, (name, arguments))| {
                    record.source == SOURCE
                        && record.capability == *name
                        && record.arguments == *arguments
                        && record.decision == decision
                },
            );
    sample.check("permission_decisions_match_calls", permissions_match,
        format!("Expected {} actual {decision} decisions in call order, with exact capability and arguments; declared grants alone are not execution evidence.", expected.len()));

    let expected_status = match case {
        Case::Cancel => CallStatus::OutcomeUnknown,
        Case::GateDenial => CallStatus::Failed,
        _ => CallStatus::Succeeded,
    };
    let statuses_match = sample.report.calls.iter().all(|call| {
        call.status == expected_status
            && !call.result_omitted
            && match call.status {
                CallStatus::Succeeded => call.result.is_some() && call.error.is_none(),
                CallStatus::Failed | CallStatus::OutcomeUnknown => {
                    call.result.is_none()
                        && call.error.as_ref().is_some_and(|error| !error.is_empty())
                }
            }
    });
    sample.check("call_status_and_payload", statuses_match,
        format!("Expected {expected_status:?} with complete, consistent result/error evidence for each reported call."));

    let audited = if case == Case::GateDenial {
        &[][..]
    } else {
        expected.as_slice()
    };
    let audits_match = audit_matches_calls(sample, audited);
    sample.check("audit_started_settled_pairing", audits_match.is_ok(),
        audits_match.err().unwrap_or_else(|| "Every granted operation has its own ordered durable start/settled pair, exact arguments and matching report result/status; denied calls have no audit start.".into()));
    let unknown = sample
        .report
        .calls
        .iter()
        .any(|call| call.status == CallStatus::OutcomeUnknown);
    sample.check(
        "outcome_flags_match_calls",
        sample.report.outcome_unknown == unknown && sample.host_outcome_unknown == unknown,
        format!(
            "Unknown call status: {unknown}; report: {}; Host: {}.",
            sample.report.outcome_unknown, sample.host_outcome_unknown
        ),
    );
    if case == Case::Cancel {
        validate_cancel(sample);
    } else {
        sample.check(
            "known_outcome",
            !sample.report.outcome_unknown && !sample.host_outcome_unknown,
            "No unknown outcome is expected for this case.",
        );
    }
    match case {
        Case::Cold | Case::Warm => {
            sample.check("first_success_output", sample.report.return_value == json!({"answer":42})
                && sample.report.outputs == vec![json!({"phase":"first"})],
                "Actual Luau return is 42 and its emitted phase is first.");
            if case == Case::Warm {
                sample.check("warmup_success", sample.warmup.as_ref().is_some_and(|report|
                    report.return_value == json!({"warmup":true}) && report.calls.is_empty()),
                    "A measured first execute completed on the same CodeMode before the measured case execute; each uses a fresh VM.");
            }
        }
        Case::Files | Case::SyntaxRepair => {
            let content = if case == Case::Files { PIPELINE_CONTENT } else { REPAIR_CONTENT };
            let result = &sample.report.return_value;
            let correct = result["content"] == content && result["sha256"] == sha256(content.as_bytes())
                && result["same_hash"] == true
                && sample.report.calls.iter().all(|call| call.status == CallStatus::Succeeded)
                && sample.report.outputs.len() == 1;
            sample.check("actual_file_content_and_hash", correct,
                "The real Host read returns the exact written UTF-8 bytes and independently computed SHA-256.");
            if case == Case::Files {
                let result = &sample.report.return_value;
                sample.check("actual_list_and_stat", result["listed"] == true && result["kind"] == "file"
                    && result["bytes"] == content.len(),
                    "Real fs.list includes pipeline.txt, fs.stat reports file, and byte length matches UTF-8 input.");
            }
        }
        Case::SyntaxFailure => sample.check("syntax_has_no_effects", sample.audit_events.is_empty()
            && sample.permission_decisions.is_empty(), "Compilation failed before any Host call or permission request."),
        Case::LoopBudget => sample.check("checkpoint_budget_exhausted",
            sample.report.instruction_checkpoints > case.limits().instruction_budget,
            format!("observed checkpoints {}; budget {}",sample.report.instruction_checkpoints,case.limits().instruction_budget)),
        Case::GateDenial => sample.check("real_gate_denial", sample.permission_decisions.len() == 1
            && sample.permission_decisions[0].decision == "fixture_denial"
            && sample.audit_events.is_empty()
            && sample.report.calls.first().is_some_and(|call| call.status == CallStatus::Failed),
            "The advertised fs.write reached the real gate and was denied before an audit start or file effect."),
        Case::Sandbox => {
            let globals = ["os_absent","io_absent","process_absent","require_absent","loadfile_absent","dofile_absent","debug_absent"];
            sample.check("globals_unavailable", globals.iter().all(|name| sample.report.return_value[*name] == true),
                "Executed Luau confirms os/io/process/require/loadfile/dofile/debug are nil.");
            let names = sample.report.return_value["capabilities"].as_array();
            let expected = [
                "archive.list", "archive.read", "code.diff_hunks", "code.outline", "codec.decode", "codec.encode",
                "data.parse", "data.query", "data.serialize", "data.transform",
                "fs.batch_status", "fs.glob", "fs.grep", "fs.list", "fs.mkdir", "fs.move", "fs.patch",
                "fs.prepare_change_batch", "fs.read", "fs.recover_change_batch",
                "fs.remove", "fs.stat", "fs.submit_change_batch", "fs.tree", "fs.write",
                "hash.digest", "text.diff",
            ];
            sample.check("only_explicit_host_catalog", names.is_some_and(|names|
                names.len() == expected.len() && names.iter().zip(expected).all(|(name, expected)| name == expected)),
                "No shell, process profile, model provider, or external service is configured in the ordinary Host.");
        }
        Case::UnknownCapability | Case::Arguments => sample.check("rejected_before_host",
            sample.audit_events.is_empty() && sample.permission_decisions.is_empty(),
            "The invalid request never reached the Host or its permission gate."),
        Case::Cancel => {},
    }
}

/// These fixed scripts await one call at a time. The fixture sink commits a
/// start/settled pair for each granted operation in that same order. A count or
/// membership check alone cannot bind a settlement to the operation that ran.
fn audit_matches_calls(sample: &Sample, expected: &[(String, Value)]) -> Result<(), String> {
    if sample.audit_events.len() != expected.len() * 2 {
        return Err(format!(
            "Expected {} durable audit events; observed {}.",
            expected.len() * 2,
            sample.audit_events.len()
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    for (index, ((name, arguments), pair)) in expected
        .iter()
        .zip(sample.audit_events.chunks_exact(2))
        .enumerate()
    {
        let start = &pair[0];
        let settled = &pair[1];
        let Some(call) = sample.report.calls.get(index) else {
            return Err(format!(
                "Missing report call {} for durable audit pair.",
                index + 1
            ));
        };
        if start.source != SOURCE
            || start.event != "started"
            || start.call_id.is_empty()
            || !ids.insert(start.call_id.as_str())
            || start.capability.as_ref() != Some(name)
            || start.arguments.as_ref() != Some(arguments)
            || start.result.is_some()
            || start.error.is_some()
            || start.outcome_unknown
        {
            return Err(format!(
                "Call {} has a missing, duplicate, misplaced or mismatched durable start.",
                index + 1
            ));
        }
        if settled.source != SOURCE
            || settled.event != "settled"
            || settled.call_id != start.call_id
            || settled.capability.is_some()
            || settled.arguments.is_some()
            || call.sequence != index + 1
            || call.name != *name
            || call.arguments.as_ref() != Some(arguments)
            || call.result_omitted
            || settled.result != call.result
            || settled.error != call.error
            || settled.outcome_unknown != (call.status == CallStatus::OutcomeUnknown)
        {
            return Err(format!(
                "Call {} lacks its ordered durable settlement with matching report result/error and unknown status.",
                index + 1
            ));
        }
        let valid_payload = match call.status {
            CallStatus::Succeeded => {
                settled.result.is_some() && settled.error.is_none() && !settled.outcome_unknown
            }
            CallStatus::Failed | CallStatus::OutcomeUnknown => {
                settled.result.is_none()
                    && settled
                        .error
                        .as_ref()
                        .is_some_and(|error| !error.is_empty())
            }
        };
        if !valid_payload {
            return Err(format!(
                "Call {} settlement contradicts its reported status.",
                index + 1
            ));
        }
    }
    Ok(())
}

fn validate_cancel(sample: &mut Sample) {
    let cancelled = sample.cancellation.as_ref().is_some_and(|state| {
        state.entered
            && state.stopped
            && !state.active
            && state.cancellation_requested_after_entry
            && state.cancellation_observed_on_stop
            && state.settled_before_execute_return
            && state.cancel_to_execute_return_ns.is_some()
    });
    sample.check("cancel_started_host_synchronously_settled", cancelled,
        "The pending async fixture was entered before cancellation; it stopped and the real Host durably settled before CodeMode returned.");
    // An interrupted external call remains unknown even if the runtime drops the
    // Host future. Check unknown preservation separately from strict settlement.
    sample.check("conservative_external_unknown", sample.report.outcome_unknown
        && sample.host_outcome_unknown
        && sample.audit_events.iter().any(|event|
            matches!(event.event.as_str(), "settled" | "dropped") && event.outcome_unknown)
        && sample.report.calls.first().is_some_and(|call| call.status == CallStatus::OutcomeUnknown),
        "Keep the real Host's unknown flag; fixture knowledge is not a product reconciliation or permission decision.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);
    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cayw-harness-validator-{}-{}-{}",
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

    fn observed(case: Case) -> Sample {
        let temp = Temporary::new();
        let workspace = temp.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_case(case, 1, &workspace, &temp.0.join("audit.jsonl")))
    }

    fn assert_rejected(base: &Sample, case: Case, label: &str, mutate: impl FnOnce(&mut Sample)) {
        let mut sample: Sample =
            serde_json::from_value(serde_json::to_value(base).unwrap()).unwrap();
        sample.semantic_checks.clear();
        sample.semantic_passed = false;
        mutate(&mut sample);
        validate(&mut sample, case, case.expected_error());
        assert!(
            !sample.semantic_passed,
            "accepted {label}: {}",
            serde_json::to_string(&sample).unwrap()
        );
    }

    #[test]
    fn correct_file_report_without_permissions_or_audit_is_rejected() {
        let sample = observed(Case::Files);
        assert!(sample.semantic_passed, "{sample:?}");
        assert_eq!(sample.permission["grants"].as_array().unwrap().len(), 4);
        assert_rejected(
            &sample,
            Case::Files,
            "declared grants and successful report without execution evidence",
            |sample| {
                sample.permission_decisions.clear();
                sample.audit_events.clear();
            },
        );
    }

    #[test]
    fn file_call_evidence_must_match_the_expected_operation_in_order() {
        let sample = observed(Case::Files);
        assert!(sample.semantic_passed, "{sample:?}");
        type Corruption = (&'static str, fn(&mut Sample));
        let corruptions: &[Corruption] = &[
            ("swapped permission decisions", |s| {
                s.permission_decisions.swap(0, 1)
            }),
            ("duplicate permission decision", |s| {
                s.permission_decisions[1] = s.permission_decisions[0].clone()
            }),
            ("permission for another name", |s| {
                s.permission_decisions[0].capability = "fs.remove".into()
            }),
            ("permission for another path", |s| {
                s.permission_decisions[0].arguments["path"] = json!("other.txt")
            }),
            ("non-fixture permission source", |s| {
                s.permission_decisions[0].source = "unverified".into()
            }),
            ("denied call with a declared successful report", |s| {
                s.permission_decisions[0].decision = "fixture_denial".into()
            }),
            ("missing permission decision", |s| {
                s.permission_decisions.pop();
            }),
            ("extra permission decision", |s| {
                s.permission_decisions
                    .push(s.permission_decisions[0].clone())
            }),
            ("missing audit pair", |s| s.audit_events.truncate(6)),
            ("extra audit pair", |s| {
                s.audit_events.extend(s.audit_events[..2].to_vec())
            }),
            ("swapped audit operation pairs", |s| {
                s.audit_events.swap(0, 2);
                s.audit_events.swap(1, 3);
            }),
            ("cross-call settled IDs", |s| {
                s.audit_events[1].call_id = "call-0002".into();
                s.audit_events[3].call_id = "call-0001".into();
            }),
            ("duplicate audit ID", |s| {
                s.audit_events[2].call_id = "call-0001".into();
                s.audit_events[3].call_id = "call-0001".into();
            }),
            ("empty audit ID", |s| {
                s.audit_events[0].call_id.clear();
                s.audit_events[1].call_id.clear();
            }),
            ("settlement before start", |s| s.audit_events.swap(0, 1)),
            ("audit for another name", |s| {
                s.audit_events[0].capability = Some("fs.remove".into())
            }),
            ("audit for another path", |s| {
                s.audit_events[0].arguments.as_mut().unwrap()["path"] = json!("other.txt")
            }),
            ("non-fixture audit source", |s| {
                s.audit_events[1].source = "unverified".into()
            }),
            ("cross-call settled payloads", |s| {
                s.audit_events[1].result = s.audit_events[3].result.clone();
            }),
            ("audit failure contradicts successful report", |s| {
                s.audit_events[1].result = None;
                s.audit_events[1].error = Some("failed".into());
            }),
            ("audit unknown contradicts known report", |s| {
                s.audit_events[1].outcome_unknown = true
            }),
            ("start prematurely claims a result", |s| {
                s.audit_events[0].result = Some(json!({"bytes":23}))
            }),
            ("settlement claims different call arguments", |s| {
                s.audit_events[1].arguments = Some(json!({"path":"other.txt"}))
            }),
            ("report for another name", |s| {
                s.report.calls[0].name = "fs.remove".into()
            }),
            ("report for another path", |s| {
                s.report.calls[0].arguments.as_mut().unwrap()["path"] = json!("other.txt")
            }),
            ("report arguments omitted", |s| {
                s.report.calls[0].arguments = None
            }),
            ("duplicate report sequence", |s| {
                s.report.calls[1].sequence = 1
            }),
            ("swapped report calls", |s| s.report.calls.swap(0, 1)),
            ("report result disagrees with audit", |s| {
                s.report.calls[0].result = Some(json!({"bytes":0}))
            }),
            ("report result omitted", |s| {
                s.report.calls[0].result_omitted = true
            }),
            ("successful report has an error", |s| {
                s.report.calls[0].error = Some("failed".into())
            }),
            ("wrong report status", |s| {
                s.report.calls[0].status = CallStatus::Failed
            }),
            ("extra report call", |s| {
                s.report.calls.push(s.report.calls[0].clone())
            }),
            ("missing report call", |s| {
                s.report.calls.pop();
            }),
            ("all records agree on a different operation", |s| {
                s.report.calls[0].name = "fs.remove".into();
                s.permission_decisions[0].capability = "fs.remove".into();
                s.audit_events[0].capability = Some("fs.remove".into());
            }),
            ("all records agree on different arguments", |s| {
                let arguments = json!({"path":"other.txt","content":PIPELINE_CONTENT});
                s.report.calls[0].arguments = Some(arguments.clone());
                s.permission_decisions[0].arguments = arguments.clone();
                s.audit_events[0].arguments = Some(arguments);
            }),
        ];
        for (label, mutate) in corruptions {
            assert_rejected(&sample, Case::Files, label, mutate);
        }
    }

    #[test]
    fn denied_call_requires_exact_denial_and_no_started_audit() {
        let sample = observed(Case::GateDenial);
        assert!(sample.semantic_passed, "{sample:?}");
        assert_eq!(sample.report.calls.len(), 1);
        assert_eq!(sample.report.calls[0].status, CallStatus::Failed);
        assert_eq!(sample.permission_decisions[0].decision, "fixture_denial");
        assert!(sample.audit_events.is_empty());
        assert_rejected(
            &sample,
            Case::GateDenial,
            "denial for a different path",
            |s| {
                s.permission_decisions[0].arguments["path"] = json!("other.txt");
                s.report.calls[0].arguments.as_mut().unwrap()["path"] = json!("other.txt");
            },
        );
        assert_rejected(
            &sample,
            Case::GateDenial,
            "failed report claims a result",
            |s| {
                s.report.calls[0].result = Some(json!({"bytes":0}));
            },
        );
        assert_rejected(
            &sample,
            Case::GateDenial,
            "failed report omits failure evidence",
            |s| {
                s.report.calls[0].error = None;
            },
        );
        assert_rejected(
            &sample,
            Case::GateDenial,
            "missing actual permission denial",
            |s| {
                s.permission_decisions.clear();
            },
        );
    }

    #[test]
    fn syntax_failure_has_available_grants_but_no_executed_calls() {
        let sample = observed(Case::SyntaxFailure);
        assert!(sample.semantic_passed, "{sample:?}");
        assert_eq!(sample.permission["grants"].as_array().unwrap().len(), 2);
        assert!(sample.report.calls.is_empty());
        assert!(sample.permission_decisions.is_empty());
        assert!(sample.audit_events.is_empty());
    }

    #[test]
    fn sandbox_accepts_only_the_canonical_default_catalog() {
        let sample = observed(Case::Sandbox);
        assert!(sample.semantic_passed, "{sample:?}");
        assert_eq!(
            sample.report.return_value["capabilities"]
                .as_array()
                .unwrap()
                .len(),
            27
        );
        assert_rejected(
            &sample,
            Case::Sandbox,
            "an unexpected executable capability",
            |s| {
                s.report.return_value["capabilities"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("shell.exec"));
            },
        );
        assert_rejected(&sample, Case::Sandbox, "catalog order changed", |s| {
            s.report.return_value["capabilities"]
                .as_array_mut()
                .unwrap()
                .swap(0, 1);
        });
    }

    #[test]
    fn unknown_cancelled_call_still_requires_a_matching_durable_settlement() {
        // Synthetic validator input, not a measured cancellation result. The CLI
        // test separately requires the actual runtime to settle before returning.
        let mut sample = empty_sample(Case::Cancel, 1, Path::new("unused"), Path::new("unused"));
        let arguments = json!({"query":"cayw-harness.blocking-fixture"});
        let message = "external capability cancelled after admission";
        sample.execution_status = "error".into();
        sample.error = Some(RecordedError {
            kind: "cancelled".into(),
            message: "CodeMode execution cancelled".into(),
        });
        sample.report.calls.push(cayw_codemode::CapabilityCall {
            sequence: 1,
            name: "search.query".into(),
            arguments: Some(arguments.clone()),
            status: CallStatus::OutcomeUnknown,
            result: None,
            result_omitted: false,
            error: Some(message.into()),
        });
        sample.report.outcome_unknown = true;
        sample.host_outcome_unknown = true;
        sample.permission_decisions.push(PermissionRecord {
            source: SOURCE.into(),
            capability: "search.query".into(),
            arguments: arguments.clone(),
            decision: "fixture_grant".into(),
        });
        sample.audit_events = vec![
            AuditEvent {
                source: SOURCE.into(),
                event: "started".into(),
                call_id: "call-0001".into(),
                capability: Some("search.query".into()),
                arguments: Some(arguments),
                result: None,
                error: None,
                outcome_unknown: false,
            },
            AuditEvent {
                source: SOURCE.into(),
                event: "settled".into(),
                call_id: "call-0001".into(),
                capability: None,
                arguments: None,
                result: None,
                error: Some(message.into()),
                outcome_unknown: true,
            },
        ];
        sample.cancellation = Some(BlockingObservation {
            source: SOURCE.into(),
            entered: true,
            active: false,
            stopped: true,
            cancellation_observed_on_stop: true,
            cancellation_requested_after_entry: true,
            settled_before_execute_return: true,
            cancel_to_execute_return_ns: Some(1),
        });
        validate(&mut sample, Case::Cancel, Some(ErrorKind::Cancelled));
        assert!(sample.semantic_passed, "{sample:?}");
        type Corruption = (&'static str, fn(&mut Sample));
        let corruptions: &[Corruption] = &[
            ("cancelled report without grants or audit", |s| {
                s.permission_decisions.clear();
                s.audit_events.clear();
            }),
            ("started cancelled call without settlement", |s| {
                s.audit_events.truncate(1)
            }),
            ("dropped cancelled call with unknown preserved", |s| {
                s.audit_events[1].event = "dropped".into()
            }),
            ("settlement for a different cancelled call", |s| {
                s.audit_events[1].call_id = "call-0002".into()
            }),
            ("unknown report with a known settlement", |s| {
                s.audit_events[1].outcome_unknown = false
            }),
            ("unknown call with a known report flag", |s| {
                s.report.outcome_unknown = false
            }),
            ("unknown call with a known Host flag", |s| {
                s.host_outcome_unknown = false
            }),
            ("cancelled call reports known failure", |s| {
                s.report.calls[0].status = CallStatus::Failed
            }),
            (
                "settled cancelled call without a return-time observation",
                |s| {
                    s.cancellation
                        .as_mut()
                        .unwrap()
                        .settled_before_execute_return = false
                },
            ),
        ];
        for (label, mutate) in corruptions {
            assert_rejected(&sample, Case::Cancel, label, mutate);
        }
    }

    #[test]
    fn configuration_records_the_actual_cancellation_settlement_allowance() {
        let limits = Limits::default();
        let recorded = limits_json(&limits);
        assert_eq!(recorded["shutdown_grace_ns"], json!(2_000_000_000_u64));
        let shortened = Limits {
            shutdown_grace: std::time::Duration::from_nanos(1),
            ..limits
        };
        let changed = limits_json(&shortened);
        assert_eq!(changed["shutdown_grace_ns"], json!(1));
        assert_ne!(recorded, changed);
        assert_eq!(
            config_json(Case::Cancel)["codemode"]["shutdown_grace_ns"],
            recorded["shutdown_grace_ns"]
        );
    }

    #[test]
    fn configuration_records_default_data_and_workspace_budgets() {
        let config = config_json(Case::Files);
        assert_eq!(
            config["host"]["data"],
            json!({
                "input_bytes":1048576,"output_bytes":1048576,"max_depth":32,
                "max_nodes":100000,"max_rows":10000,"max_diff_work":1000000,
            })
        );
        assert_eq!(
            config["host"]["workspace"],
            json!({
                "max_file_bytes":1048576,"max_batch_bytes":16777216,"max_batch_operations":64,
                "max_batches":128,"max_tree_entries":4096,"max_depth":32,"max_argument_bytes":8388608,
                "max_output_bytes":8388608,"max_archive_bytes":16777216,"max_archive_entries":4096,
                "max_archive_expanded_bytes":16777216,"max_compression_ratio":100,"timeout_ns":30000000000_u64,
                "syntax_validator":"cayw_workspace::JsonTomlValidator",
            })
        );
    }
}
