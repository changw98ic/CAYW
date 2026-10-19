//! Child processes belong to the fixture. Product code has no fault switches.
use super::{
    DEADLINE, NegativeControl, Result, Scenario, ScenarioContext, ScenarioReport,
    provider::{self, FixedProvider, effect_result, model, write_new},
    report::{contiguous_events, local_binding},
    scenarios::{self, BuiltinScenario},
};
use crate::measurement::{elapsed_ns, sha256};
use cayw_ai::CancellationToken;
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_runtime::{RuntimeError, SessionView, TaskState};
use serde_json::{Value, json};
use std::{env, ffi::OsString, fs, path::Path, process::Stdio, sync::Arc, time::Instant};
use tokio::process::{Child, Command};

pub async fn dispatch(mode: &str, args: Vec<OsString>, entry: Instant) -> Result<bool> {
    if args.len() != 3 {
        return Err("internal SDK mode needs root, iteration and control/fault".into());
    }
    let root = super::absolute(Path::new(&args[0]))?;
    let iteration: u32 = args[1].to_str().ok_or("invalid iteration")?.parse()?;
    if !(1..=1000).contains(&iteration) {
        return Err("iteration must be 1..=1000".into());
    }
    let flag = args[2].to_str().ok_or("invalid control/fault")?;
    match mode {
        "__sdk_cold" => {
            let control = match flag {
                "none" => NegativeControl::None,
                "wrong-script" => NegativeControl::WrongScript,
                _ => return Err("invalid cold control".into()),
            };
            let context = ScenarioContext {
                root,
                iteration,
                negative_control: control,
                binding: local_binding()?,
            };
            let mut report = ScenarioReport::new(
                "cold-sdk-child",
                iteration,
                BuiltinScenario::ColdSdk.specification(control),
                context.binding.clone(),
            );
            report.attach_records(&context.root.join("cold-records.jsonl"))?;
            if let Err(error) = scenarios::sdk_executions(&context, &mut report, 1).await {
                report.observe("runner_error", json!({"message":error.to_string()}))?;
                report.check("child_scenario_completed", false, error.to_string());
            }
            report
                .timing_ns
                .insert("child_entry_to_record".into(), elapsed_ns(entry));
            report.finish();
            report.persist_terminal()?;
            report.bind_records(&context.root.join("cold-records.jsonl"))?;
            report.artifacts["records"]["path"] = json!("cold-records.jsonl");
            write_new(&context.root.join("cold-report.json"), &report)?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(report.status == "passed")
        }
        "__sdk_host" => {
            let effect = match flag {
                "before-dispatch" => false,
                "unknown-effect" => true,
                _ => return Err("invalid host fault".into()),
            };
            host(&root, effect).await
        }
        _ => Err("unknown internal SDK mode".into()),
    }
}

pub async fn cold(context: &ScenarioContext, report: &mut ScenarioReport) -> Result<()> {
    let control = match context.negative_control {
        NegativeControl::WrongScript => "wrong-script",
        _ => "none",
    };
    let start = Instant::now();
    let output = tokio::time::timeout(
        DEADLINE * 3,
        Command::new(env::current_exe()?)
            .arg("__sdk_cold")
            .arg(&context.root)
            .arg(context.iteration.to_string())
            .arg(control)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    report
        .timing_ns
        .insert("parent_spawn_to_exit".into(), elapsed_ns(start));
    super::write_bytes_new(&context.root.join("cold.stdout"), &output.stdout)?;
    super::write_bytes_new(&context.root.join("cold.stderr"), &output.stderr)?;
    report.observe("child_process_terminal", json!({"exit_code":output.status.code(),
        "success":output.status.success(),"stdout_sha256":sha256(&output.stdout),"stderr_sha256":sha256(&output.stderr)}))?;
    let child: ScenarioReport =
        serde_json::from_slice(&fs::read(context.root.join("cold-report.json"))?)?;
    report.observe("cold_child_report", &child)?;
    report.check(
        "cold_fresh_process",
        child.process_id != std::process::id(),
        "Cold measurement must use an independent process",
    );
    report.check(
        "cold_child_uses_same_build",
        child.binding == context.binding,
        "The fresh process retains the same executable hash, protocol versions and caller-declared build binding",
    );
    report.check(
        "cold_child_semantics",
        child.status == "passed" && output.status.success() && output.stderr.is_empty(),
        "Child exit, semantic checks and diagnostic stream must all agree",
    );
    for check in child.checks {
        report.check(&format!("child_{}", check.name), check.passed, check.detail);
    }
    for (phase, value) in child.timing_ns {
        report.timing_ns.insert(format!("child_{phase}"), value);
    }
    for page in child.event_pages {
        report.event_page(page)?;
    }
    Ok(())
}

async fn host(root: &Path, effect: bool) -> Result<bool> {
    scenarios::initialize(root)?;
    let script = if effect {
        provider::EFFECT_SCRIPT
    } else {
        provider::PIPELINE
    };
    let provider = Arc::new(FixedProvider::new(
        script,
        !effect,
        Some(root.join("provider.jsonl")),
    ));
    let runtime = scenarios::open(root, provider.clone(), effect)?;
    let session = runtime.create(Some("Offline restart fixture".into()))?;
    write_new(
        &root.join("host.json"),
        &json!({"process_id":std::process::id(),"session_id":session.id,
        "fault":if effect {"unknown-effect"} else {"before-dispatch"},"binding":local_binding()?,"model":model()}),
    )?;
    let run = runtime.prompt(&session.id, provider::PROMPT, CancellationToken::new());
    tokio::pin!(run);
    if !effect {
        tokio::select! {
            () = provider.entered.cancelled() => {},
            result = &mut run => return Err(format!("host prompt ended before gate: {result:?}").into()),
        }
        write_new(
            &root.join("before-dispatch.json"),
            &json!({"process_id":std::process::id(),
            "provider_calls":provider.calls(),"session":runtime.get(&session.id)?,"tasks":runtime.tasks(&session.id)?}),
        )?;
    }
    let result = run.await;
    write_new(
        &root.join("unexpected-terminal.json"),
        &json!({"result":result.as_ref().map_err(ToString::to_string)}),
    )?;
    Err("fixture host completed before parent terminated it".into())
}

async fn wait_file(child: &mut Child, path: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("fixture child exited before fault point: {status}").into());
        }
        if path.is_file() {
            // The writer fsyncs before waiting. Require valid, complete JSON too.
            if fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some()
            {
                return Ok(());
            }
        }
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return Err("fixture fault point watchdog expired".into()),
            () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {},
        }
    }
}

pub async fn restart(
    context: &ScenarioContext,
    report: &mut ScenarioReport,
    effect: bool,
) -> Result<()> {
    let point = context.root.join(if effect {
        "effect.json"
    } else {
        "before-dispatch.json"
    });
    let mut child = Command::new(env::current_exe()?)
        .arg("__sdk_host")
        .arg(&context.root)
        .arg(context.iteration.to_string())
        .arg(if effect {
            "unknown-effect"
        } else {
            "before-dispatch"
        })
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let child_pid = child.id();
    let ready_point = if effect {
        context.root.join("effect-ready.json")
    } else {
        point.clone()
    };
    if let Err(error) = wait_file(&mut child, &ready_point).await {
        let _ = child.start_kill();
        let output = child.wait_with_output().await?;
        super::write_bytes_new(&context.root.join("host.stdout"), &output.stdout)?;
        super::write_bytes_new(&context.root.join("host.stderr"), &output.stderr)?;
        return Err(error);
    }
    let marker_bytes = fs::read(&point)?;
    let marker: Value = serde_json::from_slice(&marker_bytes)?;
    let ready: Value = serde_json::from_slice(&fs::read(context.root.join("host.json"))?)?;
    let id = ready["session_id"]
        .as_str()
        .ok_or("host session identity absent")?;
    report.observe("host_identity", &ready)?;
    report.observe(
        "fault_point_readiness",
        serde_json::from_slice::<Value>(&fs::read(&ready_point)?)?,
    )?;
    report.observe(
        "fault_point_durable_marker",
        json!({"path":point,"sha256":sha256(&marker_bytes),"value":marker}),
    )?;
    report.check(
        "original_host_identity",
        child_pid.map(u64::from) == ready["process_id"].as_u64()
            && child_pid != Some(std::process::id()),
        "Observed process must be the exact child owned by this scenario",
    );
    let provider = Arc::new(FixedProvider::new(
        provider::PIPELINE,
        false,
        Some(context.root.join("reopened-provider.jsonl")),
    ));
    let observer = scenarios::open(&context.root, provider.clone(), effect)?;
    let before = observer.get(id)?;
    let before_tasks = observer.tasks(id)?;
    let original_task = before_tasks
        .first()
        .ok_or("fault point has no original durable task")?
        .clone();
    let before_events = scenarios::capture(&observer, id, "before_kill", report)?;
    let original_calls: Vec<_> = before_events
        .iter()
        .filter_map(|record| {
            if let CoreEvent::CapabilityStarted { call } = &record.event {
                Some(call.clone())
            } else {
                None
            }
        })
        .collect();
    report.check(
        "original_attempt_running",
        before.attempt.is_some()
            && before_tasks.len() == 1
            && before_tasks[0].state == TaskState::Running,
        "Kill must occur while one original durable task owns an active attempt",
    );
    if effect {
        let expected_scope = serde_json::to_string(&provider::external_arguments())?;
        let exact_authority = original_calls.first().is_some_and(|call| {
            call.authorization
                .as_ref()
                .and_then(|value| {
                    serde_json::from_value::<cayw_core::CapabilityAuthorization>(value.clone()).ok()
                })
                .is_some_and(|authority| {
                    authority.target == call.key
                        && authority.task_id == call.task_id
                        && authority.capability == "search.query"
                        && authority.decision == cayw_core::ApprovalDecision::Approve
                        && authority.actor == "runtime-config"
                        && authority.scope == expected_scope
                })
        });
        report.check("effect_after_original_durable_start", marker==effect_result() && original_calls.len()==1
            && original_calls[0].name=="search.query" && original_calls[0].arguments==provider::external_arguments()
            && exact_authority && original_calls[0].outcome.is_none(),
            "One capability with exact persisted fixture authority precedes the synced local effect; no settlement exists yet");
    } else {
        report.check(
            "before_dispatch_has_no_started_effect",
            original_calls.is_empty()
                && before.executions.is_empty()
                && !context.root.join("workspace/result.txt").exists(),
            "Provider gate prevents any code or Host dispatch before termination",
        );
    }
    drop(observer);
    report.observe("fault_injected",json!({"operation":"Child::start_kill then wait_with_output", "process_id":child_pid,
        "point":if effect {"after durable synthetic local effect"} else {"before script dispatch"}}))?;
    let killed = Instant::now();
    child.start_kill()?;
    let output = tokio::time::timeout(DEADLINE, child.wait_with_output()).await??;
    report
        .timing_ns
        .insert("kill_to_confirmed_exit".into(), elapsed_ns(killed));
    super::write_bytes_new(&context.root.join("host.stdout"), &output.stdout)?;
    super::write_bytes_new(&context.root.join("host.stderr"), &output.stderr)?;
    report.observe(
        "host_exit",
        json!({"exit_code":output.status.code(),"success":output.status.success(),
        "stdout_sha256":sha256(&output.stdout),"stderr_sha256":sha256(&output.stderr)}),
    )?;
    report.check(
        "killed_host_exited_before_recovery",
        !output.status.success() && output.stdout.is_empty() && output.stderr.is_empty(),
        "The original host has exited; explicit recovery cannot race its old owner",
    );
    let provider_log = fs::read(context.root.join("provider.jsonl"))?;
    let start = Instant::now();
    let runtime = scenarios::open(&context.root, provider.clone(), effect)?;
    report
        .timing_ns
        .insert("reopen_after_kill".into(), elapsed_ns(start));
    let opened = runtime.get(id)?;
    report.observe("reopened_before_recovery", &opened)?;
    report.check("open_does_not_recover_or_replay", opened==before && runtime.tasks(id)?==before_tasks && provider.calls()==0,
        "Opening preserves the exact interrupted state and never runs a Provider or repairs implicitly");
    let start = Instant::now();
    let recovered = runtime.recover(id)?;
    report
        .timing_ns
        .insert("explicit_recovery".into(), elapsed_ns(start));
    let events = scenarios::capture(&runtime, id, "after_recover", report)?;
    report.check(
        "recovery_retains_contiguous_original_events",
        contiguous_events(id, 0, &events, recovered.cursor) && events.starts_with(&before_events),
        "Recovery appends facts while preserving every original event and call identity",
    );
    let tasks = runtime.tasks(id)?;
    report.check(
        "recovery_fences_original_task",
        tasks.len() == 1
            && tasks[0].id == original_task.id
            && tasks[0].state == TaskState::Interrupted
            && tasks[0].fence > original_task.fence,
        "The same task is interrupted and fenced; no replacement task is created",
    );
    let resumed = tokio::time::timeout(
        DEADLINE,
        runtime.resume(&original_task.id, CancellationToken::new()),
    )
    .await?;
    report.observe(
        "resume_original_interrupted_task",
        json!({"result":resumed.as_ref().map_err(ToString::to_string)}),
    )?;
    report.check(
        "interrupted_task_cannot_replay",
        resumed.is_err() && provider.calls() == 0,
        "Explicit resume of an interrupted task is rejected before a Provider request",
    );
    if effect {
        check_unknown(&recovered, &original_calls, report);
        let denied = tokio::time::timeout(
            DEADLINE,
            runtime.prompt(id, provider::PROMPT, CancellationToken::new()),
        )
        .await?;
        report.observe(
            "prompt_frozen_unknown",
            json!({"result":denied.as_ref().map_err(ToString::to_string)}),
        )?;
        report.check(
            "unknown_blocks_new_work",
            matches!(denied, Err(RuntimeError::NeedsReconciliation))
                && runtime.tasks(id)?.len() == 1
                && provider.calls() == 0,
            "Unknown capability outcome freezes admission before any new task or Provider stream",
        );
        report.check("durable_effect_not_repeated",fs::read(&point)?==marker_bytes,
            "The exact one marker survives kill, reopen, recover and refused replay; fixture uses create_new for any second invocation");
        drop(runtime);
        let again = scenarios::open(&context.root, provider.clone(), true)?;
        let view = again.get(id)?;
        report.observe("second_reopen_still_unknown", &view)?;
        report.check("unknown_survives_another_reopen",view==recovered && provider.calls()==0,
            "Reopen cannot clear unknown outcomes; no host reconciliation is fabricated by this fixture");
    } else {
        report.check("before_dispatch_recovery_is_known",scenarios::clean(&recovered) && recovered.executions.is_empty()
            && !context.root.join("workspace/result.txt").exists(),"No capability started, so explicit recovery reaches known idle without executing the queued script");
    }
    report.check(
        "no_provider_replay_across_recovery",
        provider.calls() == 0
            && fs::read(context.root.join("provider.jsonl"))? == provider_log
            && !context.root.join("reopened-provider.jsonl").exists(),
        "Original raw Provider journal is unchanged and reopened Provider was never called",
    );
    Ok(())
}

fn check_unknown(
    view: &SessionView,
    original: &[cayw_core::CapabilityCallRecord],
    report: &mut ScenarioReport,
) {
    let unknown = &view.unresolved_capabilities;
    report.check("recovered_effect_is_unknown_and_frozen",view.state==SessionState::NeedsReconciliation
        && view.attempt.is_none() && unknown.len()==1 && original.len()==1
        && unknown[0].key==original[0].key && unknown[0].lease==original[0].lease
        && unknown[0].task_id==original[0].task_id && unknown[0].source_hash==original[0].source_hash
        && unknown[0].parent_call_id==original[0].parent_call_id && unknown[0].arguments==original[0].arguments
        && unknown[0].authorization==original[0].authorization
        && matches!(unknown[0].outcome,Some(CapabilityOutcome::OutcomeUnknown{..})),
        "Recovered unknown retains the original capability, complete lease, task, source, parent and authorization; session remains unavailable for execution");
}
