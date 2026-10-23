//! Run an existing compiled project test through task requests and the real Host.
//! The Provider and host decisions are fixed local fixtures, not a real model.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_requests::{RequestRecord, RequestState};
use cayw_runtime::{HeadlessConfig, Runtime, SessionView, TaskIntent, TaskRecord};
use serde_json::{Value, json};
use std::{
    error::Error,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore};

type Result<T = ()> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const TOKEN: &str = "public-project-test-fixture-token";
const SCRIPT: &str = "local r=cayw.call('capability.request',{purpose='run the project argument-validation test',gap='the fixed project test requires a task grant',operation_id='project-contracts',capability='test.run',arguments={profile='project-contracts'}}); cayw.emit(r); yield_control(); return cayw.call('test.run',{profile='project-contracts'})";
const TEST: &str = "validation_is_strict_and_local_only";

fn require(condition: bool, message: &str) -> Result {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message).into())
    }
}
fn save(path: &Path, value: &impl serde::Serialize) -> Result {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}
fn hash(path: &Path) -> Result<String> {
    let output = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path)
        .env_clear()
        .output()?;
    require(
        output.status.success(),
        "cannot hash the named project artifact",
    )?;
    let text = std::str::from_utf8(&output.stdout)?;
    let digest = text.split_whitespace().next().unwrap_or("");
    require(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "project artifact digest is invalid",
    )?;
    Ok(digest.into())
}
struct FixedProjectRequest {
    pause: Arc<Semaphore>,
    rounds: AtomicUsize,
}
impl Provider for FixedProjectRequest {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            SCRIPT,
            &format!("project-test-{}", request.messages.len()),
        );
        let pause = (self.rounds.fetch_add(1, Ordering::AcqRel) == 1).then(|| self.pause.clone());
        Box::pin(async_stream::stream! {
            if let Some(pause) = pause { pause.acquire_owned().await.unwrap().forget(); }
            yield AssistantEvent::Done { message };
        })
    }
}
fn configuration(program: &Path) -> Value {
    json!({
        "budgets":{"max_run_time_ms":60000,"max_turns":16,"max_capability_calls":8,
            "host_wait_ms":30000,"host_call_timeout_ms":30000},
        "profiles":{"project-contracts":{"kind":"test","program":program,
            "args":[TEST,"--exact","--nocapture"],"env":{},"timeout_ms":20000,
            "max_output_bytes":65536,"allow_filter":false}},
        "permissions":[{"capability":"test.run","arguments":{"profile":"project-contracts"}}],
        "requests":{"permission_policy_id":"issue8-project-contracts-v1",
            "operations":[{"id":"project-contracts","capability":"test.run",
                "arguments":{"profile":"project-contracts"},"permission_binding":"compiled-project-test"}],
            "limits":{"request_lifetime_ms":45000,"evaluation_timeout_ms":15000,
                "max_grant_lifetime_ms":30000}}
    })
}
async fn awaiting(rt: &Runtime, task: &str) -> Result<RequestRecord> {
    Ok(tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            // The task has not created its authority before the worker starts.
            if let Ok(events) = rt.request_events(task, 0, 64) {
                for event in events.into_iter().rev() {
                    if let Some(id) = event.request_id {
                        let request = rt.request_status(task, &id)?;
                        if request.state == RequestState::AwaitingEvaluation {
                            return Ok::<_, cayw_runtime::RuntimeError>(request);
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??)
}
async fn run_case(mode: &str, root: &Path, program: &Path, output: &Path) -> Result<Value> {
    std::fs::create_dir(output)?;
    let workspace = output.join("workspace");
    std::fs::create_dir(&workspace)?;
    save(
        &output.join("workspace.json"),
        &json!({"source_root":root,"runtime_workspace":workspace,
            "test_kind":"compiled project contract test with no source-directory input"}),
    )?;
    let input = configuration(program);
    save(&output.join("configuration.json"), &input)?;
    let config = serde_json::from_value::<HeadlessConfig>(input)?.into_runtime_config()?;
    let pause = Arc::new(Semaphore::new(0));
    let rt = Runtime::open(
        output.join("session.db"),
        &workspace,
        Arc::new(FixedProjectRequest {
            pause: pause.clone(),
            rounds: AtomicUsize::new(0),
        }),
        Model::new("fixture", "exact-project-test"),
        config.clone(),
    )?;
    let session = rt.create(Some(format!("Issue 8 actual project test via {mode}")))?;
    let intent = TaskIntent::ExplicitProjectTestOrScript {
        user_instruction: format!("Run the compiled project test {TEST} with exact arguments"),
    };
    let mut connection = None;
    let mut server = None;
    let stop = CancellationToken::new();
    let task: TaskRecord = if mode == "ipc" {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        server = Some(tokio::spawn(serve(
            listener,
            Arc::new(rt.clone()),
            ServerConfig::new(TOKEN),
            stop.clone(),
        )));
        let client = Client::connect(address, TOKEN).await?;
        let task = serde_json::from_value(
            client
                .request(
                    "task.enqueue_with_intent",
                    json!({
                        "session_id":session.id,"text":format!("Run {TEST}"),"intent":intent,
                    }),
                )
                .await?,
        )?;
        connection = Some(client);
        task
    } else {
        rt.enqueue_with_intent(&session.id, &format!("Run {TEST}"), intent)?
    };
    save(&output.join("task-admitted.json"), &task)?;
    let worker = if let Some(client) = &connection {
        let client = client.clone();
        let id = task.id.clone();
        tokio::spawn(async move {
            let value = client.request("task.resume", json!({"task_id":id})).await?;
            Ok::<SessionView, Box<dyn Error + Send + Sync>>(serde_json::from_value(value)?)
        })
    } else {
        let runtime = rt.clone();
        let id = task.id.clone();
        tokio::spawn(async move { Ok(runtime.resume(&id, CancellationToken::new()).await?) })
    };
    let request = awaiting(&rt, &task.id).await?;
    save(&output.join("request-before.json"), &request)?;
    let before = rt.events(&session.id, 0, 1000)?;
    save(&output.join("core-before.json"), &before)?;
    require(
        !before.iter().any(|event| {
            matches!(&event.event,
        CoreEvent::CapabilityStarted { call } if call.name == "test.run")
        }),
        "project test started before the host decision",
    )?;
    require(
        request.valid_request_count == 1,
        "explicit project intent did not enable first evaluation",
    )?;
    let granted: RequestRecord = if let Some(client) = &connection {
        serde_json::from_value(
            client
                .request(
                    "request.decide",
                    json!({
                        "task_id":task.id,"request_id":request.id,
                        "decision":{"kind":"allow","lifetime_ms":30000},
                        "reason":"fixture host reviewed the exact compiled project test",
                    }),
                )
                .await?,
        )?
    } else {
        let ticket = rt.request_evaluate(&task.id, &request.id, "fixture-project-host")?;
        rt.request_approve(
            &task.id,
            &ticket,
            Duration::from_secs(30),
            "fixture host reviewed the exact compiled project test",
        )?
    };
    save(&output.join("request-granted.json"), &granted)?;
    require(
        granted.id == request.id
            && granted.identity == request.identity
            && granted.state == RequestState::Granted,
        "host decision did not grant the original exact request",
    )?;
    pause.add_permits(1);
    let view = tokio::time::timeout(Duration::from_secs(60), worker).await???;
    let events = rt.events(&session.id, 0, 1000)?;
    let audit = rt.audit(&session.id, 0, 1000)?;
    let requests = rt.request_events(&task.id, 0, 64)?;
    save(&output.join("core-events.json"), &events)?;
    save(&output.join("host-audit.json"), &audit)?;
    save(&output.join("request-events.json"), &requests)?;
    save(&output.join("session-final.json"), &view)?;
    save(&output.join("task-final.json"), &rt.task(&task.id)?)?;
    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "test.run" => Some(call),
            _ => None,
        })
        .collect();
    require(
        calls.len() == 1,
        "expected exactly one settled project process",
    )?;
    let call = calls[0];
    require(
        call.task_id == task.id
            && call.lease.session_id == session.id
            && call.lease.attempt_id == request.identity.attempt_id,
        "project result lost its original task identity",
    )?;
    let starts: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilityStarted { call: started } if started.name == "test.run" => {
                Some((event.cursor, started))
            }
            _ => None,
        })
        .collect();
    require(
        starts.len() == 1,
        "expected one original project process start",
    )?;
    let (start_cursor, started) = starts[0];
    require(
        started.key == call.key
            && started.lease == call.lease
            && started.arguments == call.arguments
            && started.task_id == call.task_id
            && started.parent_call_id == call.parent_call_id
            && started.source_hash == call.source_hash,
        "project process start and terminal do not pair",
    )?;
    let settled_cursor = events
        .iter()
        .find_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call: settled } if settled.key == call.key => {
                Some(event.cursor)
            }
            _ => None,
        })
        .ok_or_else(|| io::Error::other("project terminal event is absent"))?;
    require(
        start_cursor < settled_cursor,
        "project process terminal preceded its start",
    )?;
    let receipts: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::Host { kind, data }
                if kind == "task_request_admitted" && data["name"] == "test.run" =>
            {
                Some((event.cursor, data))
            }
            _ => None,
        })
        .collect();
    require(
        receipts.len() == 1,
        "expected one original task request admission receipt",
    )?;
    let (receipt_cursor, receipt) = receipts[0];
    require(
        receipt_cursor < start_cursor
            && receipt["key"] == serde_json::to_value(&call.key)?
            && receipt["receipt"]["request_id"] == request.id
            && receipt["receipt"]["identity"] == serde_json::to_value(&request.identity)?
            && receipt["receipt"]["operation"]["arguments"] == call.arguments
            && receipt["receipt"]["catalog_fingerprint"] == request.catalog_fingerprint,
        "project admission receipt does not bind its original request and Core call",
    )?;
    let result = match &call.outcome {
        Some(CapabilityOutcome::Completed { result }) => result,
        _ => return Err(io::Error::other("project test has no known completed result").into()),
    };
    save(&output.join("actual-project-result.json"), result)?;
    require(
        result["success"] == true
            && result["exit_code"] == 0
            && result["termination"] == "exited"
            && result["output_complete"] == true,
        "compiled project test failed or output was incomplete",
    )?;
    let stdout = result["stdout"].as_str().unwrap_or("");
    require(
        stdout.contains(TEST) && stdout.contains("1 passed; 0 failed"),
        "actual project test output is missing its exact test or result",
    )?;
    require(
        rt.task(&task.id)?.state == cayw_runtime::TaskState::Completed,
        "project task did not complete",
    )?;
    require(
        matches!(
            rt.request_task_state(&task.id)?,
            cayw_requests::TaskState::Ended { .. }
        ),
        "project request authority did not end",
    )?;
    let final_cursor = view.cursor;
    let task_id = task.id.clone();
    let session_id = session.id.clone();
    if let Some(client) = connection {
        client.close().await;
    }
    stop.cancel();
    if let Some(server) = server {
        server.await??;
    }
    drop(rt);
    let reopened = Runtime::open(
        output.join("session.db"),
        &workspace,
        Arc::new(FixedProjectRequest {
            pause,
            rounds: AtomicUsize::new(0),
        }),
        Model::new("fixture", "exact-project-test"),
        config,
    )?;
    require(
        reopened.get(&session_id)?.cursor == final_cursor,
        "reopen unexpectedly changed project history",
    )?;
    require(
        reopened.health()?.running == 0,
        "reopen restarted the project task",
    )?;
    save(
        &output.join("reopened-task.json"),
        &reopened.task(&task_id)?,
    )?;
    Ok(
        json!({"mode":mode,"session_id":session_id,"task_id":task_id,
        "request_id":request.id,"call":call.key,"attempt":call.lease,
        "source_hash":call.source_hash,"test":TEST,"actual_result":"actual-project-result.json"}),
    )
}
#[tokio::main]
async fn main() -> Result {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    require(
        args.len() == 3,
        "usage: task_project_test ABS_PROJECT_ROOT ABS_PROJECT_PROGRAM_BUILD_JSON NEW_EVIDENCE_DIR",
    )?;
    let root = PathBuf::from(&args[0]).canonicalize()?;
    let build_path = PathBuf::from(&args[1]).canonicalize()?;
    let provenance: Value = serde_json::from_reader(File::open(&build_path)?)?;
    let program = PathBuf::from(
        provenance["program"]
            .as_str()
            .ok_or_else(|| io::Error::other("program build receipt has no program"))?,
    )
    .canonicalize()?;
    let output = PathBuf::from(&args[2]);
    require(
        output.is_absolute() && program.is_file(),
        "program and new output path must be absolute",
    )?;
    let source = root.join("crates/cayw-ai/tests/contracts.rs");
    let receipt_path = PathBuf::from(
        provenance["build_receipt"]
            .as_str()
            .ok_or_else(|| io::Error::other("program build receipt has no command receipt"))?,
    );
    let command: Value = serde_json::from_reader(File::open(&receipt_path)?)?;
    let log = PathBuf::from(
        command["log"]
            .as_str()
            .ok_or_else(|| io::Error::other("build command has no raw log"))?,
    );
    require(
        provenance["schema"] == "cayw.issue8.project-program-build.v1"
            && provenance["program_sha256"] == hash(&program)?
            && provenance["compiler_target_source"] == source.to_string_lossy().as_ref()
            && provenance["compiler_target_source_sha256"] == hash(&source)?
            && provenance["build_receipt_sha256"] == hash(&receipt_path)?
            && command["exit_code"] == 0
            && command["source_unchanged_during_command"] == true
            && command["log_sha256"] == hash(&log)?
            && provenance["build_log_sha256"] == command["log_sha256"]
            && provenance["source_inventory_sha256"] == command["source_inventory_sha256"],
        "program does not match its unchanged-source build receipt",
    )?;
    let raw_log = std::fs::read_to_string(&log)?;
    let artifact_matches = raw_log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|artifact| artifact == &provenance["compiler_artifact"])
        .count();
    require(
        artifact_matches == 1,
        "program compiler artifact is absent or repeated in the raw build log",
    )?;
    let preserved_path = PathBuf::from(
        provenance["preserved_artifact_path"]
            .as_str()
            .ok_or_else(|| io::Error::other("build has no same-command preserved artifact"))?,
    );
    let preserved: Value = serde_json::from_reader(File::open(&preserved_path)?)?;
    require(
        preserved == provenance["preserved_artifact"]
            && provenance["preserved_artifact_sha256"] == hash(&preserved_path)?
            && preserved["compiler_artifact"] == provenance["compiler_artifact"]
            && preserved["program"] == program.to_string_lossy().as_ref()
            && preserved["program_sha256"] == provenance["program_sha256"]
            && raw_log
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|entry| entry == &preserved)
                .count()
                == 1,
        "program was not preserved with its digest inside the original build command",
    )?;
    std::fs::create_dir(&output)?;
    let build = json!({"provider":"fixed local fixture","host_decision":"synthetic trusted fixture",
        "program":program,"program_sha256":hash(&program)?,"args":[TEST,"--exact","--nocapture"],
        "source_root":root,"test_source":source,"test_source_sha256":hash(&source)?,
        "build_provenance":provenance,"build_provenance_path":build_path,"build_provenance_sha256":hash(&build_path)?,
        "example_sha256":hash(&root.join("crates/cayw-runtime/examples/task_project_test.rs"))?});
    save(&output.join("input-build.json"), &build)?;
    let mut cases = Vec::new();
    for mode in ["sdk", "ipc"] {
        cases.push(run_case(mode, &root, &program, &output.join(mode)).await?);
    }
    let summary = json!({"schema":"cayw.issue8.actual-project-test.v1","cases":cases,
        "evidence_kind":"actual compiled project test with synthetic Provider and host decisions"});
    save(&output.join("result.json"), &summary)?;
    File::open(&output)?.sync_all()?;
    println!("{summary}");
    Ok(())
}
