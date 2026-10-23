#![cfg(target_os = "macos")]

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, Usage, UsageStatus,
};
use cayw_cells::{CellPage, CellStatus};
use cayw_core::{CapabilityOutcome, CoreEvent, EventRecord, ExecutionOutcome, SessionState};
use cayw_host::{ProcessIo, Profile, ProfileKind};
use cayw_requests::{OperationScope, RequestInput, RequestRecord, RequestState, ValidityReason};
use cayw_runtime::{
    RequestPolicy, Runtime, RuntimeConfig, RuntimeError, ScopedPermission, TaskIntent, TaskRecord,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

const OPERATION: &str = "project-test";
const PROFILE: &str = "project";
const OTHER_PROFILE: &str = "other-project";

struct Project {
    root: PathBuf,
    _temporary: Option<tempfile::TempDir>,
}
impl Project {
    fn new(label: &str) -> Self {
        let parent = std::env::var_os("CAYW_REQUEST_BOUNDARY_EVIDENCE_DIR");
        let temporary = if let Some(parent) = &parent {
            fs::create_dir_all(parent).unwrap();
            tempfile::Builder::new()
                .prefix(&format!("{label}-"))
                .tempdir_in(parent)
                .unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let (root, temporary) = if parent.is_some() {
            (temporary.keep(), None)
        } else {
            (temporary.path().to_owned(), Some(temporary))
        };
        let project = Self {
            root,
            _temporary: temporary,
        };
        fs::create_dir(project.workspace()).unwrap();
        for profile in [PROFILE, OTHER_PROFILE] {
            fs::create_dir(project.profile_root(profile)).unwrap();
            fs::write(project.profile_root(profile).join("release"), "may exit").unwrap();
        }
        project
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn profile_root(&self, profile: &str) -> PathBuf {
        self.workspace().join(profile)
    }
    fn markers(&self) -> Vec<Value> {
        [
            self.profile_root(PROFILE).join("leaf.pid"),
            self.profile_root(OTHER_PROFILE).join("leaf.pid"),
            self.workspace().join("generic-execution-marker"),
        ]
        .into_iter()
        .map(|path| json!({"path":path,"exists":path.try_exists().unwrap()}))
        .collect()
    }
    fn no_effects(&self) {
        for marker in self.markers() {
            assert_eq!(marker["exists"], false, "unexpected real effect: {marker}");
        }
    }
    fn save(&self, name: &str, value: &impl Serialize) {
        let path = self.root.join(format!("{name}.json"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        serde_json::to_writer_pretty(&mut file, value).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
        eprintln!("REQUEST_BOUNDARY_EVIDENCE {}", path.display());
    }
    fn snapshot(&self, stage: &str, rt: &Runtime, task: &TaskRecord, request_id: &str) {
        self.save(
            stage,
            &json!({
                "task":rt.task(&task.id).unwrap(),
                "session":rt.get(&task.session_id).unwrap(),
                "core":events(rt,&task.session_id),
                "audit":rt.audit(&task.session_id,0,1000).unwrap(),
                "requests":rt.request_events(&task.id,0,64).unwrap(),
                "request":rt.request_status(&task.id,request_id).unwrap(),
                "request_task":rt.request_task_state(&task.id).unwrap(),
                "effect_observations":self.markers(),
                "running":rt.health().unwrap().running,
            }),
        );
    }
}

// Each failed call gets a new cell. A Luau error in one cell cannot hide any
// later input, and none of the cells catches or suppresses a VM failure.
struct Scripts {
    scripts: Vec<String>,
    next_cell: AtomicUsize,
    rounds: AtomicUsize,
    resume: Arc<Semaphore>,
}
impl Scripts {
    fn next(&self, request: &ModelRequest) -> AssistantMessage {
        let index = self.next_cell.fetch_add(1, Ordering::AcqRel);
        let mut message = AssistantMessage::new(&request.model);
        // Fixed synthetic counters describe this successful model response only.
        message.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        if let Some(script) = self.scripts.get(index) {
            message.content.push(Content::ToolCall(ToolCall::new(
                format!("boundary-cell-{index}"),
                "execute_code",
                json!({"action":"exec","script":script,
                    "yield_time_ms":1000,"max_output_tokens":128*1024}),
            )));
            message.stop_reason = StopReason::ToolUse;
        } else {
            message
                .content
                .push(Content::text("All boundary cells were attempted"));
            message.stop_reason = StopReason::Stop;
        }
        message
    }
    fn response(&self, request: &ModelRequest) -> AssistantMessage {
        match request.messages.last() {
            Some(Message::ToolResult(result)) => {
                let page = result
                    .structured_content
                    .as_ref()
                    .and_then(|value| serde_json::from_value::<CellPage>(value.clone()).ok());
                match page {
                    Some(page)
                        if !result.outcome_unknown
                            && !page.outcome_unknown
                            && !matches!(
                                page.status,
                                CellStatus::Cancelled | CellStatus::OutcomeUnknown
                            ) =>
                    {
                        if !page.status.is_terminal() || page.has_more {
                            cayw_runtime::fixture::script_response(
                                request,
                                "",
                                &format!("boundary-wait-{}", request.messages.len()),
                            )
                        } else {
                            self.next(request)
                        }
                    }
                    _ => AssistantMessage::new(&request.model).failure(
                        StopReason::Error,
                        "Boundary fixture requires a known terminal cell before the next input",
                    ),
                }
            }
            _ => self.next(request),
        }
    }
}
impl Provider for Scripts {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let message = self.response(&request);
        let pause = (self.rounds.fetch_add(1, Ordering::AcqRel) == 1).then(|| self.resume.clone());
        Box::pin(async_stream::stream! {
            if let Some(pause) = pause {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    permit = pause.acquire_owned() => permit.unwrap().forget(),
                }
            }
            yield AssistantEvent::Done { message };
        })
    }
}

#[derive(Clone, Serialize)]
#[serde(tag = "boundary", rename_all = "snake_case")]
enum Refusal {
    ExactGrant,
    Schema { field: String },
    MissingCapability,
}
#[derive(Clone, Serialize)]
struct Attempt {
    label: String,
    name: String,
    arguments: Value,
    refusal: Refusal,
}
impl Attempt {
    fn script(&self) -> String {
        format!(
            "return cayw.call({},{})",
            lua(&json!(self.name)),
            lua(&self.arguments)
        )
    }
}
fn lua(value: &Value) -> String {
    match value {
        Value::Null => "cayw.null".into(),
        Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_string(value).unwrap()
        }
        Value::Array(values) => format!(
            "cayw.array({{{}}})",
            values.iter().map(lua).collect::<Vec<_>>().join(",")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("[{}]={}", lua(&json!(key)), lua(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}
fn request_input(operation: &str, arguments: Value) -> RequestInput {
    RequestInput {
        purpose: "run the user-selected local project test".into(),
        gap: "the configured process requires an exact task request grant".into(),
        operation_id: operation.into(),
        capability: "test.run".into(),
        arguments,
    }
}
fn settings(project: &Project) -> RuntimeConfig {
    let mut config = RuntimeConfig {
        max_run_time: Duration::from_secs(45),
        ..Default::default()
    };
    for name in [PROFILE, OTHER_PROFILE] {
        config.host.profiles.insert(
            name.into(),
            Profile {
                kind: ProfileKind::Test,
                program: env!("CARGO_BIN_EXE_cayw-runtime-process-fixture").into(),
                args: vec![
                    "leaf".into(),
                    project.profile_root(name).to_string_lossy().into_owned(),
                ],
                env: BTreeMap::new(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 4096,
                allow_filter: true,
                io: ProcessIo::Pipes,
            },
        );
    }
    config.permissions = vec![
        ScopedPermission::exact("test.run", json!({"profile":PROFILE})),
        ScopedPermission::exact("test.run", json!({"profile":OTHER_PROFILE})),
        ScopedPermission::exact("test.run", json!({"profile":PROFILE,"filter":"leaf"})),
    ];
    config.request_policy = Some(RequestPolicy {
        permission_policy_id: "issue8-request-boundaries".into(),
        operations: vec![OperationScope::exact(
            OPERATION,
            "test.run",
            json!({"profile":PROFILE}),
            "host-selected-project",
        )],
    });
    config
}
fn open(
    project: &Project,
    input: &RequestInput,
    attempts: &[Attempt],
    resume: Arc<Semaphore>,
) -> Runtime {
    let request = serde_json::to_value(input).unwrap();
    let first = format!(
        "local request=cayw.call('capability.request',{}); cayw.emit(request); yield_control(); return request",
        lua(&request)
    );
    let scripts = std::iter::once(first)
        .chain(attempts.iter().map(Attempt::script))
        .collect::<Vec<_>>();
    project.save("input", &json!({
        "request":input,"attempts":attempts,"scripts":scripts,
        "provider":"fixed local fixture","workspace":project.workspace(),
        "program":env!("CARGO_BIN_EXE_cayw-runtime-process-fixture"),
        "profiles":([PROFILE,OTHER_PROFILE].map(|name| json!({
            "id":name,"args":["leaf",project.profile_root(name)],"env":{},
            "allow_filter":true,"release_exists":project.profile_root(name).join("release").try_exists().unwrap(),
        }))),
        "selected_operation":{"id":OPERATION,"capability":"test.run","arguments":{"profile":PROFILE}},
        "ordinary_exact_permissions":[{"profile":PROFILE},{"profile":OTHER_PROFILE},{"profile":PROFILE,"filter":"leaf"}],
        "effect_observations":project.markers(),
    }));
    Runtime::open(
        project.root.join("session.db"),
        project.workspace(),
        Arc::new(Scripts {
            scripts,
            next_cell: AtomicUsize::new(0),
            rounds: AtomicUsize::new(0),
            resume,
        }),
        Model::new("fixture", "request-boundaries"),
        settings(project),
    )
    .unwrap()
}
fn events(rt: &Runtime, session: &str) -> Vec<EventRecord> {
    let mut collected = Vec::new();
    let mut after = 0;
    loop {
        let page = rt.events(session, after, 1000).unwrap();
        let Some(last) = page.last() else {
            return collected;
        };
        after = last.cursor;
        collected.extend(page);
    }
}
async fn recorded_request(rt: &Runtime, task: &TaskRecord) -> RequestRecord {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match rt.request_events(&task.id, 0, 64) {
                Ok(events) => {
                    for event in events.into_iter().rev() {
                        if let Some(id) = event.request_id {
                            return rt.request_status(&task.id, &id).unwrap();
                        }
                    }
                }
                Err(RuntimeError::InvalidInput(reason))
                    if reason == "task has no host request authority" => {}
                Err(error) => panic!("cannot read the original request: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "request was not recorded: {error}; task={:?}; events={:?}",
            rt.task(&task.id),
            events(rt, &task.session_id)
        )
    })
}
fn verify_attempts(project: &Project, rt: &Runtime, task: &TaskRecord, attempts: &[Attempt]) {
    let events = events(rt, &task.session_id);
    let executions = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::ExecutionStarted { execution } => Some(execution),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        executions.len(),
        attempts.len() + 1,
        "a failed cell hid a later test input"
    );
    assert_eq!(
        executions
            .iter()
            .map(|execution| &execution.execution_id)
            .collect::<BTreeSet<_>>()
            .len(),
        executions.len()
    );
    for (index, attempt) in attempts.iter().enumerate() {
        let parent = format!("boundary-cell-{}", index + 1);
        let execution = executions
            .iter()
            .find(|execution| execution.parent_call_id == parent)
            .expect("attempt did not get its own real cell");
        assert_eq!(execution.task_id, task.id);
        let terminal = events
            .iter()
            .find_map(|event| match &event.event {
                CoreEvent::ExecutionFinished {
                    execution: finished,
                } if finished.execution_id == execution.execution_id => Some(finished),
                _ => None,
            })
            .expect("boundary cell has no terminal");
        let error = match &terminal.terminal {
            Some(ExecutionOutcome::Failed { error }) => error,
            other => panic!(
                "{} was not rejected by the actual execution: {other:?}",
                attempt.label
            ),
        };
        let calls = events
            .iter()
            .filter_map(|event| match &event.event {
                CoreEvent::CapabilitySettled { call }
                    if call.key.execution_id == execution.execution_id =>
                {
                    Some(call)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!events.iter().any(|event| matches!(&event.event,
            CoreEvent::CapabilityStarted { call } if call.key.execution_id == execution.execution_id
        )), "{} crossed the actual Host execution boundary", attempt.label);
        match &attempt.refusal {
            Refusal::ExactGrant => {
                assert_eq!(
                    calls.len(),
                    1,
                    "{} never reached the real Runtime",
                    attempt.label
                );
                let call = calls[0];
                assert_eq!(call.name, attempt.name);
                assert_eq!(call.arguments, attempt.arguments);
                assert_eq!(call.task_id, task.id);
                assert_eq!(call.lease, execution.lease);
                assert!(
                    matches!(&call.outcome, Some(CapabilityOutcome::NotExecuted { reason }) if reason.contains("no active host grant for these exact arguments")),
                    "{}: {call:?}",
                    attempt.label
                );
                assert!(
                    error.contains("no active host grant for these exact arguments"),
                    "{}: {error}",
                    attempt.label
                );
            }
            Refusal::Schema { field } => {
                assert!(
                    calls.is_empty(),
                    "schema-refused arguments reached the Runtime backend"
                );
                assert!(
                    error.to_lowercase().contains("additional") && error.contains(field),
                    "{}: {error}",
                    attempt.label
                );
            }
            Refusal::MissingCapability => {
                assert!(
                    calls.is_empty(),
                    "an unregistered generic tool reached the Runtime backend"
                );
                assert!(
                    error.contains(&format!("Unknown capability: {}", attempt.name)),
                    "{}: {error}",
                    attempt.label
                );
            }
        }
        assert!(!events.iter().any(|event| matches!(&event.event,
            CoreEvent::CapabilityPlanned { call } if call.key.execution_id == execution.execution_id && !matches!(attempt.refusal, Refusal::ExactGrant)
        )), "a directory/schema refusal must not invent a Core subcall");
    }
    assert!(!events.iter().any(|event| matches!(&event.event,
        CoreEvent::Host { kind, data } if kind == "task_request_admitted" && data["name"] == "test.run"
    )));
    assert_eq!(rt.get(&task.session_id).unwrap().state, SessionState::Idle);
    assert!(
        rt.get(&task.session_id)
            .unwrap()
            .pending_approvals
            .is_empty()
    );
    assert_eq!(rt.health().unwrap().running, 0);
    project.no_effects();
}
async fn run(project: &Project, input: RequestInput, attempts: Vec<Attempt>, grant: bool) {
    let resume = Arc::new(Semaphore::new(0));
    let rt = open(project, &input, &attempts, resume.clone());
    let session = rt.create(None).unwrap().id;
    let task = rt
        .enqueue_with_intent(
            &session,
            "Exercise only the exact host-selected project test boundary",
            TaskIntent::ExplicitProjectTestOrScript {
                user_instruction: "Run the configured project profile without changing its scope"
                    .into(),
            },
        )
        .unwrap();
    let worker = {
        let runtime = rt.clone();
        let task_id = task.id.clone();
        tokio::spawn(async move { runtime.resume(&task_id, CancellationToken::new()).await })
    };
    let request = recorded_request(&rt, &task).await;
    assert_eq!(request.input, input);
    assert_eq!(request.identity.task_id, task.id);
    assert_eq!(request.identity.session_id, session);
    project.snapshot("before-decision", &rt, &task, &request.id);
    project.no_effects();
    if grant {
        assert_eq!(request.state, RequestState::AwaitingEvaluation);
        assert_eq!(request.valid_request_count, 1);
        let ticket = rt
            .request_evaluate(&task.id, &request.id, "boundary-host")
            .unwrap();
        let granted = rt
            .request_approve(
                &task.id,
                &ticket,
                Duration::from_secs(90),
                "only the exact unchanged project call",
            )
            .unwrap();
        assert_eq!(granted.state, RequestState::Granted);
        assert_eq!(granted.id, request.id);
        assert_eq!(granted.identity, request.identity);
    } else {
        assert_eq!(request.state, RequestState::Invalid);
        assert_eq!(request.valid_request_count, 0);
        let reason = if input.operation_id == OPERATION {
            ValidityReason::ScopeMismatch
        } else {
            ValidityReason::UnsupportedOperation
        };
        assert_eq!(request.validity_reasons, vec![reason]);
        let evaluation = rt.request_evaluate(&task.id, &request.id, "boundary-host");
        project.save(
            "invalid-evaluation",
            &json!({
                "task_id":task.id,"request_id":request.id,
                "evaluation_error":evaluation.as_ref().err().map(ToString::to_string),
            }),
        );
        assert!(
            matches!(
                evaluation,
                Err(RuntimeError::Request(cayw_requests::RequestError::Stale))
            ),
            "an invalid request must be refused for its state, not an unrelated error"
        );
        let unchanged = rt.request_status(&task.id, &request.id).unwrap();
        assert_eq!(unchanged.state, RequestState::Invalid);
        assert!(unchanged.grant_expires_at_ms.is_none());
        assert!(unchanged.decision.is_none());
    }
    project.snapshot("decision-before-attempts", &rt, &task, &request.id);
    project.no_effects();
    resume.add_permits(1);
    let finished = tokio::time::timeout(Duration::from_secs(60), worker).await;
    project.save("worker-result", &json!({"result":format!("{finished:?}")}));
    // Save raw observations before asserting the expected refusals, so an actual
    // unexpected start or effect is retained in an explicitly requested directory.
    project.snapshot("after-attempts", &rt, &task, &request.id);
    finished.unwrap().unwrap().unwrap();
    verify_attempts(project, &rt, &task, &attempts);
    assert!(matches!(
        rt.request_task_state(&task.id).unwrap(),
        cayw_requests::TaskState::Ended { .. }
    ));
    if !grant {
        let final_request = rt.request_status(&task.id, &request.id).unwrap();
        assert!(final_request.grant_expires_at_ms.is_none());
        assert!(
            !rt.request_events(&task.id, 0, 64)
                .unwrap()
                .iter()
                .any(|event| event.kind == "evaluation_decided")
        );
    }
}
fn runtime_attempts() -> Vec<Attempt> {
    vec![
        // Matching only the capability name would wrongly admit this other real
        // profile; the exact ordinary permission makes that fallback observable.
        Attempt {
            label: "another-configured-profile".into(),
            name: "test.run".into(),
            arguments: json!({"profile":OTHER_PROFILE}),
            refusal: Refusal::ExactGrant,
        },
        // Dropping filter from argument identity would silently widen the grant.
        // Both the Host profile and its ordinary exact permission allow this filter.
        Attempt {
            label: "filter-was-not-in-the-approved-arguments".into(),
            name: "test.run".into(),
            arguments: json!({"profile":PROFILE,"filter":"leaf"}),
            refusal: Refusal::ExactGrant,
        },
    ]
}

// Falling back from a changed request scope to an ordinary exact preauthorization
// would run a real configured leaf process and leave one of the two PID markers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_grant_cannot_switch_profile_or_add_a_filter_despite_ordinary_permissions() {
    let project = Project::new("granted-exact-scope");
    run(
        &project,
        request_input(OPERATION, json!({"profile":PROFILE})),
        runtime_attempts(),
        true,
    )
    .await;
}

// Removing additionalProperties or adding a generic execution capability changes
// the actual Failed-cell reason/boundary even when the task grant still refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_grant_does_not_admit_agent_process_fields_or_generic_execution_names() {
    let project = Project::new("granted-schema-and-catalog");
    let mut attempts = Vec::new();
    for (field, value) in [
        // Accepting path would let model input enter a Host-selected process scope.
        (
            "path",
            json!(project.workspace().join("generic-execution-marker")),
        ),
        // Accepting args would let the model replace or extend the trusted argv.
        ("args", json!(["leaf", project.profile_root(OTHER_PROFILE)])),
        // Accepting program would make the executable a model-selected value.
        (
            "program",
            json!(env!("CARGO_BIN_EXE_cayw-runtime-process-fixture")),
        ),
        // Accepting env would permit a model-selected process environment.
        ("env", json!({"CAYW_BOUNDARY":"model-selected"})),
        // Accepting cwd would permit a model-selected process root.
        ("cwd", json!(project.profile_root(OTHER_PROFILE))),
    ] {
        let mut arguments = json!({"profile":PROFILE});
        arguments[field] = value;
        attempts.push(Attempt {
            label: format!("extra-{field}"),
            name: "test.run".into(),
            arguments,
            refusal: Refusal::Schema {
                field: field.into(),
            },
        });
    }
    for name in [
        // A dotted shell alias must not introduce general command execution.
        "shell.run",
        // A dotted Node alias must not expose an unrestricted interpreter.
        "node.run",
        // A dotted Python alias must not expose an unrestricted interpreter.
        "python.run",
        // A conventional Bash name must not bypass the registered Host catalog.
        "Bash",
        // A conventional Shell name must not bypass the registered Host catalog.
        "Shell",
        // A conventional Node name must not bypass the registered Host catalog.
        "Node",
        // A conventional Python name must not bypass the registered Host catalog.
        "Python",
    ] {
        attempts.push(Attempt {
            label: format!("unregistered-{name}"),
            name: name.into(),
            arguments: json!({
                "program":env!("CARGO_BIN_EXE_cayw-runtime-process-fixture"),
                "args":["leaf",project.profile_root(OTHER_PROFILE)],
                "env":{},"cwd":project.workspace(),
                "command":format!("touch '{}'",project.workspace().join("generic-execution-marker").display()),
            }),
            refusal: Refusal::MissingCapability,
        });
    }
    run(
        &project,
        request_input(OPERATION, json!({"profile":PROFILE})),
        attempts,
        true,
    )
    .await;
}

// Treating an unsupported operation ID or changed arguments as eligible would
// create an evaluation/grant; bypassing the refusal via ordinary permission
// would cross Core Started or leave the real leaf marker in its selected root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_operation_ids_and_arguments_never_get_a_grant_or_an_ordinary_permission_fallback()
{
    let mut cases = vec![(
        "unsupported-operation-id".to_owned(),
        "not-selected".to_owned(),
        json!({"profile":PROFILE}),
    )];
    cases.extend(
        runtime_attempts()
            .into_iter()
            .map(|attempt| (attempt.label, OPERATION.into(), attempt.arguments)),
    );
    for (label, operation, arguments) in cases {
        let project = Project::new(&format!("invalid-request-{label}"));
        let attempt = Attempt {
            label,
            name: "test.run".into(),
            arguments: arguments.clone(),
            refusal: Refusal::ExactGrant,
        };
        run(
            &project,
            request_input(&operation, arguments),
            vec![attempt],
            false,
        )
        .await;
    }
}
