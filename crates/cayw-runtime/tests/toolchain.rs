//! Issue 5 host-configured toolchain profiles through the real Runtime.
//!
//! Every profile program, argument and environment entry is chosen by the host.
//! The tests drive the five existing profile capabilities (`build.run`,
//! `test.run`, `check.run`, `lint.run`, `format.run`) through the real
//! Runtime/Cells/Host/Device path with exact, per-argument grants:
//!
//! - all five profiles run real macOS processes and their stdout, exit status,
//!   Core `Planned`/`Started`/`Settled` records and execution terminal agree;
//! - agent-supplied argv, env, program or a wrong profile kind is a known
//!   refusal that never crosses the execution boundary or launches a process;
//! - an explicit Deny starts no process and leaves no workspace effect;
//! - cancelling a running profile process settles the unknown effect and the
//!   original supervisor reaps the process before the run returns.
//!
//! The tests use `/usr/bin/printenv`, `/usr/bin/false` and the crate's existing
//! `cayw-runtime-process-fixture` binary. No assertion depends on a timing
//! upper bound.

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityStage, CoreEvent, ExecutionOutcome,
    ExecutionState, SessionState,
};
use cayw_host::{HostConfig, ProcessIo, Profile, ProfileKind};
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;

const PRINTENV: &str = "/usr/bin/printenv";
const FALSE: &str = "/usr/bin/false";
const FIXTURE: &str = env!("CARGO_BIN_EXE_cayw-runtime-process-fixture");
/// Read by `/usr/bin/printenv`; its value lives only in the host profile.
const TAG: &str = "CAYW_PROFILE_TAG";

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            &self.0,
            &format!("tool-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

fn workspace(dir: &Path) -> PathBuf {
    dir.join("workspace")
}

fn open(
    dir: &Path,
    script: String,
    profiles: BTreeMap<String, Profile>,
    permissions: Vec<ScopedPermission>,
) -> Runtime {
    Runtime::open(
        dir.join("session.db"),
        workspace(dir),
        Arc::new(Script(script)),
        Model::new("fixture", "toolchain"),
        RuntimeConfig {
            host: HostConfig {
                profiles,
                ..Default::default()
            },
            permissions,
            ..Default::default()
        },
    )
    .unwrap()
}

/// A profile whose only behavior is to print one host-selected environment value.
fn printenv_profile(kind: ProfileKind, tag: &str) -> Profile {
    Profile {
        kind,
        program: PathBuf::from(PRINTENV),
        args: vec![TAG.into()],
        env: BTreeMap::from([(TAG.into(), tag.into())]),
        timeout: Duration::from_secs(10),
        max_output_bytes: 4096,
        allow_filter: false,
        io: ProcessIo::Pipes,
    }
}

/// A real process that records its PID before waiting for a host-controlled release.
fn leaf_profile(kind: ProfileKind, root: &Path) -> Profile {
    Profile {
        kind,
        program: PathBuf::from(FIXTURE),
        args: vec!["leaf".into(), root.to_string_lossy().into_owned()],
        env: BTreeMap::new(),
        timeout: Duration::from_secs(20),
        max_output_bytes: 4096,
        allow_filter: false,
        io: ProcessIo::Pipes,
    }
}

fn grant(name: &str, profile: &str) -> ScopedPermission {
    ScopedPermission::exact(name, json!({"profile":profile}))
}

fn capability_events(
    rt: &Runtime,
    session: &str,
) -> (
    Vec<CapabilityCallRecord>,
    Vec<CapabilityCallRecord>,
    Vec<CapabilityCallRecord>,
) {
    let mut planned = Vec::new();
    let mut started = Vec::new();
    let mut settled = Vec::new();
    for record in rt.events(session, 0, 1000).unwrap() {
        match record.event {
            CoreEvent::CapabilityPlanned { call } => planned.push(call),
            CoreEvent::CapabilityStarted { call } => started.push(call),
            CoreEvent::CapabilitySettled { call } => settled.push(call),
            _ => {}
        }
    }
    for list in [&mut planned, &mut started, &mut settled] {
        list.sort_by_key(|call| call.key.clone());
    }
    (planned, started, settled)
}

fn process_status(pid: &str) -> (bool, String) {
    match std::process::Command::new("/bin/ps")
        .args(["-p", pid])
        .output()
    {
        Ok(output) => (
            output.status.success(),
            format!(
                "status={:?} stdout={:?} stderr={:?}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        ),
        Err(error) => (false, format!("ps failed: {error}")),
    }
}

fn process_alive(pid: &str) -> bool {
    process_status(pid).0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_configured_profiles_run_through_the_real_runtime() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(dir.path())).unwrap();
    let profiles = BTreeMap::from([
        (
            "build".into(),
            printenv_profile(ProfileKind::Build, "build-ok"),
        ),
        (
            "test".into(),
            printenv_profile(ProfileKind::Test, "test-ok"),
        ),
        (
            "check".into(),
            printenv_profile(ProfileKind::Check, "check-ok"),
        ),
        (
            "lint".into(),
            printenv_profile(ProfileKind::Lint, "lint-ok"),
        ),
        (
            "format".into(),
            printenv_profile(ProfileKind::Format, "format-ok"),
        ),
    ]);
    let rt = open(
        dir.path(),
        r#"cayw.call('build.run',{profile='build'})
cayw.call('test.run',{profile='test'})
cayw.call('check.run',{profile='check'})
cayw.call('lint.run',{profile='lint'})
cayw.call('format.run',{profile='format'})
return 'configured profiles ran'"#
            .into(),
        profiles,
        vec![
            grant("build.run", "build"),
            grant("test.run", "test"),
            grant("check.run", "check"),
            grant("lint.run", "lint"),
            grant("format.run", "format"),
        ],
    );
    let id = rt.create(None).unwrap().id;
    let view = timeout(
        Duration::from_secs(20),
        rt.prompt(
            &id,
            "run the configured toolchain",
            CancellationToken::new(),
        ),
    )
    .await
    .expect("the five configured profiles must finish")
    .expect("the offline provider and host calls must succeed");

    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_capabilities.is_empty());
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);

    // The original execution owner has one terminal record for this cell.
    assert_eq!(view.executions.len(), 1);
    let execution = &view.executions[0];
    assert_eq!(execution.state, ExecutionState::Completed);
    assert!(matches!(
        execution.terminal,
        Some(ExecutionOutcome::Completed { .. })
    ));

    let (planned, started, settled) = capability_events(&rt, &id);
    assert_eq!(planned.len(), 5);
    assert_eq!(
        started.len(),
        5,
        "all five profiles must cross the launch boundary"
    );
    assert_eq!(settled.len(), 5);

    for (capability, profile, tag) in [
        ("build.run", "build", "build-ok"),
        ("test.run", "test", "test-ok"),
        ("check.run", "check", "check-ok"),
        ("lint.run", "lint", "lint-ok"),
        ("format.run", "format", "format-ok"),
    ] {
        let planned = planned
            .iter()
            .find(|call| call.name == capability)
            .unwrap_or_else(|| panic!("{capability} was never planned"));
        let started = started
            .iter()
            .find(|call| call.name == capability)
            .unwrap_or_else(|| panic!("{capability} never started a process"));
        let settled = settled
            .iter()
            .find(|call| call.name == capability)
            .unwrap_or_else(|| panic!("{capability} was never settled"));
        assert_eq!(planned.key, started.key);
        assert_eq!(planned.key, settled.key);
        // The agent supplied only the profile name; every executable argument and
        // environment entry remains the host's fixed profile content.
        assert_eq!(planned.arguments, json!({"profile":profile}));
        assert_eq!(planned.stage, CapabilityStage::Planned);
        assert_eq!(started.stage, CapabilityStage::Started);
        assert_eq!(settled.stage, CapabilityStage::Settled);
        assert!(
            started.authorization.is_some(),
            "{capability} must retain its exact authorization proof"
        );

        match settled
            .outcome
            .as_ref()
            .expect("settled call has an outcome")
        {
            CapabilityOutcome::Completed { result } => {
                assert_eq!(result["success"], true, "{capability}: {result}");
                assert_eq!(result["exit_code"], 0, "{capability}: {result}");
                assert_eq!(result["termination"], "exited");
                assert_eq!(result["output_complete"], true);
                assert_eq!(result["stderr"], "");
                // `/usr/bin/printenv` printed exactly the host profile value.
                assert_eq!(result["stdout"], format!("{tag}\n"), "{capability}");
            }
            other => panic!("{capability} must complete, got {other:?}"),
        }
    }
}

/// One refused attempt: the capability the script names, its exact Lua arguments,
/// the exact grant that isolates the host check, a label for failures, and
/// whether the strict catalog schema already rejects the shape before planning.
struct Refusal {
    capability: &'static str,
    lua_arguments: &'static str,
    arguments: Value,
    label: &'static str,
    schema_refused: bool,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_argv_env_or_program_and_wrong_profile_kinds_are_known_refusals() {
    let refusals = [
        Refusal {
            capability: "build.run",
            lua_arguments: "{profile='build',args={'injected'}}",
            arguments: json!({"profile":"build","args":["injected"]}),
            label: "agent argv",
            schema_refused: true,
        },
        Refusal {
            capability: "build.run",
            lua_arguments: "{profile='build',env={HOME='injected'}}",
            arguments: json!({"profile":"build","env":{"HOME":"injected"}}),
            label: "agent environment",
            schema_refused: true,
        },
        Refusal {
            capability: "build.run",
            lua_arguments: "{profile='build',program='/bin/sh'}",
            arguments: json!({"profile":"build","program":"/bin/sh"}),
            label: "agent program",
            schema_refused: true,
        },
        Refusal {
            capability: "build.run",
            lua_arguments: "{profile='test'}",
            arguments: json!({"profile":"test"}),
            label: "test profile passed to build.run",
            schema_refused: true,
        },
        Refusal {
            capability: "test.run",
            lua_arguments: "{profile='build'}",
            arguments: json!({"profile":"build"}),
            label: "build profile passed to test.run",
            schema_refused: true,
        },
        Refusal {
            capability: "check.run",
            lua_arguments: "{profile='missing'}",
            arguments: json!({"profile":"missing"}),
            label: "unknown profile name",
            schema_refused: true,
        },
        Refusal {
            capability: "test.run",
            lua_arguments: "{profile='test',filter='--ignored'}",
            arguments: json!({"profile":"test","filter":"--ignored"}),
            label: "filter without allow_filter",
            schema_refused: false,
        },
    ];

    for refusal in refusals {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace(dir.path())).unwrap();
        let control = workspace(dir.path()).join("control");
        std::fs::create_dir(&control).unwrap();
        // If any refused attempt launched the profile, this real process would
        // record its PID here.
        std::fs::write(control.join("release"), b"release").unwrap();
        let rt = open(
            dir.path(),
            format!(
                "return cayw.call('{}',{})",
                refusal.capability, refusal.lua_arguments
            ),
            BTreeMap::from([
                ("build".into(), leaf_profile(ProfileKind::Build, &control)),
                (
                    "test".into(),
                    printenv_profile(ProfileKind::Test, "test-ok"),
                ),
                (
                    "check".into(),
                    printenv_profile(ProfileKind::Check, "check-ok"),
                ),
            ]),
            // Grant the exact attempted arguments so only the host's own profile
            // contract can refuse it; the runtime never waits for an approval.
            vec![ScopedPermission::exact(
                refusal.capability,
                refusal.arguments.clone(),
            )],
        );
        let id = rt.create(None).unwrap().id;
        let _ = timeout(
            Duration::from_secs(20),
            rt.prompt(&id, "attempt an injected profile", CancellationToken::new()),
        )
        .await
        .unwrap_or_else(|_| panic!("{}: the run never reached a terminal", refusal.label));

        let view = rt.get(&id).unwrap();
        assert_eq!(
            view.state,
            SessionState::Idle,
            "{}: a known refusal must not require reconciliation",
            refusal.label
        );
        assert!(
            view.unresolved_capabilities.is_empty(),
            "{}: no capability may remain unresolved",
            refusal.label
        );
        assert!(
            !matches!(rt.tasks(&id).unwrap()[0].state, TaskState::OutcomeUnknown),
            "{}: a refusal must not be an unknown effect",
            refusal.label
        );
        assert!(
            !control.join("leaf.pid").exists(),
            "{}: no process may be launched",
            refusal.label
        );

        let (planned, started, settled) = capability_events(&rt, &id);
        assert!(
            started.is_empty(),
            "{}: a refused call must not cross the execution boundary",
            refusal.label
        );
        assert!(
            view.messages.iter().any(|message| matches!(
                message,
                cayw_ai::Message::ToolResult(result) if result.is_error
            )),
            "{}: the agent must receive a known error result",
            refusal.label
        );
        if refusal.schema_refused {
            // The strict catalog schema rejects the agent-supplied shape before
            // any Core capability row is planned or the host is consulted.
            assert!(
                planned.is_empty(),
                "{}: an invalid shape must not be planned",
                refusal.label
            );
            assert!(settled.is_empty(), "{}: nothing to settle", refusal.label);
        } else {
            assert_eq!(planned.len(), 1, "{}", refusal.label);
            assert_eq!(settled.len(), 1, "{}", refusal.label);
            let settled = &settled[0];
            assert_eq!(settled.name, refusal.capability);
            assert_eq!(settled.arguments, refusal.arguments);
            assert_eq!(settled.stage, CapabilityStage::Settled);
            match settled
                .outcome
                .as_ref()
                .expect("settled call has an outcome")
            {
                CapabilityOutcome::NotExecuted { reason } => {
                    assert!(!reason.is_empty(), "{}: reason", refusal.label);
                }
                other => panic!(
                    "{}: a refused call must be NotExecuted, got {other:?}",
                    refusal.label
                ),
            }
        }
    }

    // Control: the same profile does launch a real process when the agent
    // supplies exactly the host-fixed profile name and nothing else.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(dir.path())).unwrap();
    let control = workspace(dir.path()).join("control");
    std::fs::create_dir(&control).unwrap();
    std::fs::write(control.join("release"), b"release").unwrap();
    let rt = open(
        dir.path(),
        "return cayw.call('build.run',{profile='build'})".into(),
        BTreeMap::from([("build".into(), leaf_profile(ProfileKind::Build, &control))]),
        vec![grant("build.run", "build")],
    );
    let id = rt.create(None).unwrap().id;
    let view = timeout(
        Duration::from_secs(20),
        rt.prompt(&id, "run the exact profile", CancellationToken::new()),
    )
    .await
    .expect("the control profile must finish")
    .expect("the control call is authorized");
    assert_eq!(view.state, SessionState::Idle);
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    let (planned, started, settled) = capability_events(&rt, &id);
    assert_eq!(planned.len(), 1);
    assert_eq!(started.len(), 1);
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].arguments, json!({"profile":"build"}));
    match settled[0].outcome.as_ref().expect("outcome") {
        CapabilityOutcome::Completed { result } => {
            assert_eq!(result["success"], true);
            assert_eq!(result["exit_code"], 0);
        }
        other => panic!("the control call must complete, got {other:?}"),
    }
    let pid = std::fs::read_to_string(control.join("leaf.pid"))
        .expect("the authorized control process must record its PID")
        .trim()
        .to_string();
    assert!(!pid.is_empty());
    assert!(
        !process_alive(&pid),
        "the control process must have exited before the run returned"
    );
    let mut entries: Vec<_> = std::fs::read_dir(&control)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, vec!["leaf.pid", "release"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_profile_permission_starts_no_process_and_leaves_no_effect() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(dir.path())).unwrap();
    let denied = workspace(dir.path()).join("denied");
    std::fs::create_dir(&denied).unwrap();
    let rt = open(
        dir.path(),
        "return cayw.call('build.run',{profile='denied'})".into(),
        BTreeMap::from([("denied".into(), leaf_profile(ProfileKind::Build, &denied))]),
        vec![],
    );
    let id = rt.create(None).unwrap().id;
    let run = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "run the denied profile", CancellationToken::new())
                .await
        })
    };

    let approval = timeout(Duration::from_secs(15), async {
        loop {
            if let Some(approval) = rt.get(&id).unwrap().pending_approvals.into_iter().next() {
                return approval;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the exact profile call must request host permission");
    assert_eq!(approval.input.capability, "build.run");
    assert_eq!(approval.input.task_id, rt.tasks(&id).unwrap()[0].id);
    assert_eq!(
        approval.input.scope,
        serde_json::to_string(&json!({"profile":"denied"})).unwrap()
    );

    rt.approve(&id, &approval.id, ApprovalChoice::Deny, "fixture-operator")
        .unwrap();
    let view = timeout(Duration::from_secs(20), run)
        .await
        .expect("a denied call must not block the run")
        .expect("the worker must not panic")
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_capabilities.is_empty());
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    assert!(
        !denied.join("leaf.pid").exists(),
        "a denied profile must never launch its program"
    );

    let (planned, started, settled) = capability_events(&rt, &id);
    assert_eq!(planned.len(), 1);
    assert!(
        started.is_empty(),
        "no process may cross the execution boundary after a denial"
    );
    assert_eq!(settled.len(), 1);
    let settled = &settled[0];
    assert_eq!(settled.name, "build.run");
    assert_eq!(settled.arguments, json!({"profile":"denied"}));
    assert!(
        settled.authorization.is_none(),
        "a call that never started has no execution authorization"
    );
    match settled
        .outcome
        .as_ref()
        .expect("settled call has an outcome")
    {
        CapabilityOutcome::NotExecuted { reason } => {
            assert!(
                reason.contains("denied"),
                "the denial reason must be durable: {reason}"
            );
        }
        other => panic!("a denied call must be NotExecuted, got {other:?}"),
    }

    let decided = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|record| match record.event {
            CoreEvent::Host { kind, data } if kind == "approval_scope_decided" => Some(data),
            _ => None,
        })
        .expect("the host records its exact approval decision");
    assert_eq!(decided["approval_id"], approval.id);
    assert_eq!(decided["choice"], "deny");
    assert_eq!(decided["actor"], "fixture-operator");
    assert_eq!(decided["scope"], approval.input.scope);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nonzero_profile_exit_is_a_confirmed_result_not_an_unknown_effect() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(dir.path())).unwrap();
    let rt = open(
        dir.path(),
        "return cayw.call('check.run',{profile='failure'})".into(),
        BTreeMap::from([(
            "failure".into(),
            Profile {
                kind: ProfileKind::Check,
                program: PathBuf::from(FALSE),
                args: vec![],
                env: BTreeMap::new(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 4096,
                allow_filter: false,
                io: ProcessIo::Pipes,
            },
        )]),
        vec![grant("check.run", "failure")],
    );
    let id = rt.create(None).unwrap().id;
    let view = timeout(
        Duration::from_secs(20),
        rt.prompt(&id, "run the failing profile", CancellationToken::new()),
    )
    .await
    .expect("a nonzero exit must reach a terminal")
    .expect("the provider completes normally");
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_capabilities.is_empty());
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::Completed);
    assert_eq!(view.executions.len(), 1);
    assert_eq!(view.executions[0].state, ExecutionState::Completed);

    let (_planned, started, settled) = capability_events(&rt, &id);
    assert_eq!(started.len(), 1, "the profile process really ran");
    assert_eq!(settled.len(), 1);
    match settled[0].outcome.as_ref().expect("outcome") {
        // A normal nonzero exit is a confirmed terminal result: the process
        // ended, the exit status is known, and no reconciliation is required.
        CapabilityOutcome::Completed { result } => {
            assert_eq!(result["success"], false);
            assert_eq!(result["exit_code"], 1);
            assert_eq!(result["termination"], "exited");
            assert_eq!(result["output_complete"], true);
            assert_eq!(result["stdout"], "");
            assert_eq!(result["stderr"], "");
        }
        other => panic!("a confirmed nonzero exit must complete, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_running_profile_settles_unknown_and_reaps_the_process() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(dir.path())).unwrap();
    let waiter = workspace(dir.path()).join("waiter");
    std::fs::create_dir(&waiter).unwrap();
    let rt = open(
        dir.path(),
        "return cayw.call('test.run',{profile='waiter'})".into(),
        BTreeMap::from([("waiter".into(), leaf_profile(ProfileKind::Test, &waiter))]),
        vec![grant("test.run", "waiter")],
    );
    let id = rt.create(None).unwrap().id;
    let admitted = rt.enqueue(&id, "run the long profile").unwrap();
    let run = {
        let rt = rt.clone();
        let task_id = admitted.id.clone();
        tokio::spawn(async move { rt.resume(&task_id, CancellationToken::new()).await })
    };

    let pid_file = waiter.join("leaf.pid");
    // `fs::write` creates the file before it writes the PID, so wait for the
    // complete marker content rather than for the path to exist.
    let pid = timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                let pid = text.trim();
                if !pid.is_empty() {
                    return pid.to_string();
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the profile process must record its PID before cancellation");
    let (alive, detail) = process_status(&pid);
    assert!(alive, "the fixture process {pid} must be running: {detail}");

    let target = rt.task(&admitted.id).unwrap().cancel_target();
    rt.cancel(&target).unwrap();
    let outcome = timeout(Duration::from_secs(20), run)
        .await
        .expect("cancellation must reach the original run")
        .expect("the worker must not panic");
    // A cancelled run may report Cancelled to this caller; the durable facts
    // below are what the next recovery step must rely on.
    let _ = outcome;

    let (alive, detail) = process_status(&pid);
    assert!(
        !alive,
        "the original supervisor must reap {pid} before the run returns: {detail}"
    );

    let view = rt.get(&id).unwrap();
    assert_eq!(view.state, SessionState::NeedsReconciliation);
    assert_eq!(rt.tasks(&id).unwrap()[0].state, TaskState::OutcomeUnknown);
    assert_eq!(view.executions.len(), 1);
    let execution = &view.executions[0];
    assert!(
        !matches!(
            execution.state,
            ExecutionState::Running | ExecutionState::Yielded
        ),
        "the execution owner must have a terminal record: {execution:?}"
    );
    assert!(execution.terminal.is_some());

    let (_planned, started, settled) = capability_events(&rt, &id);
    assert_eq!(started.len(), 1);
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].name, "test.run");
    match settled[0].outcome.as_ref().expect("outcome") {
        CapabilityOutcome::OutcomeUnknown { error } => {
            assert!(
                error.to_lowercase().contains("cancel"),
                "the unknown terminal must name the stop cause: {error}"
            );
        }
        other => panic!("a cancelled started process must be unknown, got {other:?}"),
    }
}
