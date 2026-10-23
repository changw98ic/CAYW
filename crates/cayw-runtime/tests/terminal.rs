//! Real Runtime -> Cell -> Host terminal path. Every host process is a real
//! child selected by application configuration; the synthetic model only reads
//! the session ID returned by the Host and never supplies a program or argv.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{ApprovalRequest, CapabilityOutcome, CoreEvent, SessionState};
use cayw_host::{ProcessIo, Profile, ProfileKind, TerminalConfig, TerminalLimits};
use cayw_runtime::{
    ApprovalChoice, Runtime, RuntimeConfig, RuntimeError, ScopedPermission, TaskState,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;

const WATCHDOG: Duration = Duration::from_secs(20);

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            self.0,
            &format!("terminal-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn limits() -> TerminalLimits {
    TerminalLimits {
        max_sessions: 2,
        max_sessions_total: 8,
        max_input_bytes_per_write: 64,
        max_input_bytes_per_session: 256,
        max_output_bytes_per_session: 4096,
        max_read_bytes: 64,
        max_wait: Duration::from_secs(10),
    }
}
fn profile(program: impl Into<PathBuf>, args: &[String]) -> Profile {
    Profile {
        kind: ProfileKind::Check,
        program: program.into(),
        args: args.to_vec(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 4096,
        allow_filter: false,
        io: ProcessIo::Pipes,
    }
}
fn runtime_config(profile: Profile, permissions: Vec<ScopedPermission>) -> RuntimeConfig {
    RuntimeConfig {
        max_run_time: Duration::from_secs(15),
        codemode: cayw_codemode::Limits {
            timeout: Duration::from_secs(10),
            ..Default::default()
        },
        host: cayw_host::HostConfig {
            call_timeout: Duration::from_secs(10),
            terminal_handoffs: Some(TerminalConfig {
                profiles: BTreeMap::from([("terminal".into(), profile)]),
                limits: limits(),
            }),
            ..Default::default()
        },
        permissions,
        ..Default::default()
    }
}
fn open(root: &Path, provider: Arc<dyn Provider>, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        root.join("session.sqlite"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "terminal-path"),
        config,
    )
    .unwrap()
}
fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    root
}
async fn pending(rt: &Runtime, id: &str) -> ApprovalRequest {
    timeout(WATCHDOG, async {
        loop {
            if let Some(approval) = rt.get(id).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("terminal approval did not appear")
}
async fn ready(path: &Path) -> u32 {
    timeout(WATCHDOG, async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                return text.trim().parse().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("terminal fixture did not publish its PID")
}
fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}
fn fixture() -> PathBuf {
    env!("CARGO_BIN_EXE_cayw-runtime-process-fixture").into()
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_terminal_success_reads_the_actual_host_returned_session() {
    let root = root();
    let provider = Arc::new(Script(
        "local t = cayw.call('exec_command',{profile='terminal'})\n\
         local w = cayw.call('write_stdin',{session_id=t.session_id,action='wait',wait_ms=10000,request_id='wait-1'})\n\
         local output = '' local eof = false local cursor = 0\n\
         for i = 1, 100 do\n\
           local r = cayw.call('write_stdin',{session_id=t.session_id,action='read',cursor=cursor,max_bytes=64,request_id='read-'..i})\n\
           output = output .. r.text cursor = r.next_cursor eof = r.eof\n\
           if eof then break end\n\
         end\n\
         return {session=t.session_id,state=w.status.state,output=output,eof=eof,cursor=cursor}",
    ));
    let config = runtime_config(
        profile("/usr/bin/printf", &["terminal-ok\n".to_string()]),
        vec![ScopedPermission::exact(
            "exec_command",
            json!({"profile":"terminal"}),
        )],
    );
    let rt = open(root.path(), provider, config);
    let id = rt.create(None).unwrap().id;
    let run = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "run the configured terminal", CancellationToken::new())
                .await
        })
    };
    let mut session_ids = std::collections::BTreeSet::new();
    while !run.is_finished() {
        if let Some(approval) = rt.get(&id).unwrap().pending_approvals.first() {
            assert_eq!(approval.input.capability, "write_stdin");
            let scope: Value = serde_json::from_str(&approval.input.scope).unwrap();
            let session = scope["session_id"].as_str().expect("session id in scope");
            assert!(session.starts_with("term-"), "{session}");
            assert_ne!(session, "*");
            session_ids.insert(session.to_string());
            match rt.approve(
                &id,
                &approval.id,
                ApprovalChoice::AllowOnce,
                "terminal-test",
            ) {
                Ok(_) | Err(RuntimeError::StaleApproval) => {}
                Err(error) => panic!("approval failed: {error}"),
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let view = timeout(WATCHDOG, run)
        .await
        .unwrap()
        .unwrap()
        .expect("all terminal output was read and the process exited cleanly");
    assert_eq!(view.state, SessionState::Idle);
    assert_eq!(
        session_ids.len(),
        1,
        "the model reused its actual session ID"
    );
    let mut returned = None;
    let mut read_bytes = 0u64;
    for record in rt.events(&id, 0, 1000).unwrap() {
        if let CoreEvent::CapabilitySettled { call } = record.event {
            match (call.name.as_str(), call.outcome) {
                ("exec_command", Some(CapabilityOutcome::Completed { result })) => {
                    returned = result["session_id"].as_str().map(str::to_string);
                }
                ("write_stdin", Some(CapabilityOutcome::Completed { result }))
                    if call.arguments["action"] == "read" =>
                {
                    assert_eq!(
                        call.arguments["session_id"].as_str(),
                        returned.as_deref(),
                        "the read addressed the Host-returned session ID"
                    );
                    read_bytes += result["data_bytes"].as_u64().unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    let returned = returned.expect("exec_command returned its Host-generated session ID");
    assert_eq!(session_ids, std::collections::BTreeSet::from([returned]));
    assert!(
        read_bytes > 0,
        "the read returned the terminal's real output"
    );
    let task = rt.tasks(&id).unwrap().remove(0);
    assert_eq!(task.state, TaskState::Completed);
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_terminal_live_session_blocks_early_task_completion_and_reaps_the_process() {
    let root = root();
    let provider = Arc::new(Script(
        "local t = cayw.call('exec_command',{profile='terminal'})\nreturn t.session_id",
    ));
    let config = runtime_config(
        profile(
            fixture(),
            &["leaf".into(), root.path().to_string_lossy().into_owned()],
        ),
        vec![ScopedPermission::exact(
            "exec_command",
            json!({"profile":"terminal"}),
        )],
    );
    let rt = open(root.path(), provider, config);
    let id = rt.create(None).unwrap().id;
    let run = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(
                &id,
                "start a terminal and finish early",
                CancellationToken::new(),
            )
            .await
        })
    };
    // The fixture writes this PID and then waits for a release file the test
    // never creates, so its absence after the prompt proves Runtime cleanup.
    let pid = ready(&root.path().join("leaf.pid")).await;
    let error = timeout(WATCHDOG, run)
        .await
        .unwrap()
        .unwrap()
        .expect_err("a live terminal must not permit task completion");
    assert!(
        matches!(error, RuntimeError::RecoveryRequired { .. }),
        "{error}"
    );
    assert!(
        error.to_string().contains("still running") || error.to_string().contains("terminal"),
        "{error}"
    );
    let task = rt.tasks(&id).unwrap().remove(0);
    assert_ne!(task.state, TaskState::Completed);
    assert!(
        !alive(pid),
        "the live terminal's original OS process was reaped"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_terminal_permission_denial_never_starts_the_configured_program() {
    let root = root();
    let provider = Arc::new(Script(
        "local t = cayw.call('exec_command',{profile='terminal'})\nreturn t.session_id",
    ));
    let config = runtime_config(
        profile(
            fixture(),
            &["leaf".into(), root.path().to_string_lossy().into_owned()],
        ),
        vec![],
    );
    let rt = open(root.path(), provider, config);
    let id = rt.create(None).unwrap().id;
    let run = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "request a denied terminal", CancellationToken::new())
                .await
        })
    };
    let approval = pending(&rt, &id).await;
    assert_eq!(approval.input.capability, "exec_command");
    let scope: Value = serde_json::from_str(&approval.input.scope).unwrap();
    assert_eq!(scope, json!({"profile":"terminal"}));
    rt.approve(&id, &approval.id, ApprovalChoice::Deny, "terminal-test")
        .unwrap();
    let view = timeout(WATCHDOG, run)
        .await
        .unwrap()
        .unwrap()
        .expect("the known permission denial is surfaced to the model");
    assert_eq!(view.state, SessionState::Idle);
    assert!(
        !root.path().join("leaf.pid").exists(),
        "a denied terminal program must never start"
    );
    let task = rt.tasks(&id).unwrap().remove(0);
    assert_eq!(task.state, TaskState::Completed);
    let mut denied = false;
    let mut started = false;
    for record in rt.events(&id, 0, 1000).unwrap() {
        match record.event {
            CoreEvent::CapabilityStarted { call } if call.name == "exec_command" => {
                started = true;
            }
            CoreEvent::CapabilitySettled { call } if call.name == "exec_command" => {
                if let Some(CapabilityOutcome::NotExecuted { reason }) = call.outcome {
                    assert!(reason.contains("permission denied"), "{reason}");
                    denied = true;
                }
            }
            _ => {}
        }
    }
    assert!(denied, "the denied call must settle as not executed");
    assert!(
        !started,
        "permission denial cannot cross the execution boundary"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_cancel_reaps_the_original_terminal_os_process() {
    let root = root();
    let provider = Arc::new(Script(
        "local t = cayw.call('exec_command',{profile='terminal'})\n\
         cayw.call('write_stdin',{session_id=t.session_id,action='wait',wait_ms=10000,request_id='wait-1'})\n\
         return t.session_id",
    ));
    let config = runtime_config(
        profile(
            fixture(),
            &["leaf".into(), root.path().to_string_lossy().into_owned()],
        ),
        vec![ScopedPermission::exact(
            "exec_command",
            json!({"profile":"terminal"}),
        )],
    );
    let rt = open(root.path(), provider, config);
    let peer = rt.clone();
    let id = rt.create(None).unwrap().id;
    let run = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "cancel the live terminal", CancellationToken::new())
                .await
        })
    };
    let pid = ready(&root.path().join("leaf.pid")).await;
    assert!(alive(pid));
    // The pending wait approval proves the model addressed the actual session
    // returned by the Host rather than a host-wide permission wildcard.
    let approval = pending(&rt, &id).await;
    assert_eq!(approval.input.capability, "write_stdin");
    let scope: Value = serde_json::from_str(&approval.input.scope).unwrap();
    assert!(scope["session_id"].as_str().unwrap().starts_with("term-"));
    let target = peer.task(&approval.input.task_id).unwrap().cancel_target();
    assert!(peer.cancel(&target).unwrap());
    let result = timeout(WATCHDOG, run).await.unwrap().unwrap();
    assert!(matches!(result, Err(RuntimeError::RecoveryRequired { .. })));
    let task = rt.tasks(&id).unwrap().remove(0);
    assert_ne!(task.state, TaskState::Completed);
    assert!(
        !alive(pid),
        "Runtime cancellation must clean the original terminal OS process"
    );
}
