//! F20 / SLICE Q — combo acceptance smoke (not a new scheduler).
//!
//! Tip-proven on this branch: Q1 Session SQLite reopen, Q2 cross-Session
//! isolation, Q3 `agent.run` child identity, Q4 process cleanup + settled
//! capability result reopen (unix), Q6 MCP resources list/read via CodeMode
//! + reopen. ContentStore reopen remains macOS-gated (#18 / Linux O_PATH).
//!
//! Still pending (not claimed): Slice A `result.release`, Q5 `bg.*`/gateway
//! ownership (SLICE P), F14 lazy/idle MCP lifecycle (SLICE R), Q7 WebUI/TUI/
//! auth IPC, Q8 multi-module same-tree seal. Ignored items must not be read
//! as F20 completion — see `scripts/run-f20-combo-smoke.sh` summary flags.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_core::CoreEvent;
use cayw_runtime::{
    LocalAgents, Runtime, RuntimeConfig, ScopedPermission, TaskState, fixture::script_response,
};
use futures::stream;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const WATCHDOG: Duration = Duration::from_secs(20);

struct ComboProvider {
    requests: Mutex<Vec<String>>,
}

impl ComboProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl Provider for ComboProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let prompt = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { .. } => Some(message.text()),
                _ => None,
            })
            .unwrap_or_default();
        self.requests.lock().unwrap().push(prompt.clone());

        if prompt == "child work" {
            let mut response = AssistantMessage::new(&request.model);
            response.content.push(Content::text("child result"));
            response.stop_reason = StopReason::Stop;
            response.usage.total_tokens = 4;
            return Box::pin(stream::iter([AssistantEvent::Done { message: response }]));
        }
        if prompt == "spawn child" {
            let message = script_response(
                &request,
                "return cayw.call('agent.run', {prompt='child work', label='F20Child'})",
                &format!("combo-{}", request.messages.len()),
            );
            return Box::pin(stream::iter([AssistantEvent::Done { message }]));
        }
        if prompt == "session a note" || prompt == "session b note" {
            let message = script_response(
                &request,
                &format!("return '{prompt}-ok'"),
                &format!("combo-{}", request.messages.len()),
            );
            return Box::pin(stream::iter([AssistantEvent::Done { message }]));
        }

        let message = script_response(
            &request,
            "return 1",
            &format!("combo-{}", request.messages.len()),
        );
        Box::pin(stream::iter([AssistantEvent::Done { message }]))
    }
}

fn settings() -> RuntimeConfig {
    RuntimeConfig {
        local_agents: Some(LocalAgents {
            max_children: 2,
            max_depth: 1,
            shared_total_tokens: 100_000,
            max_calls: 8,
            max_output_bytes: 64 * 1024,
            max_time: Duration::from_secs(10),
            permissions: vec![],
        }),
        max_concurrent_runs: 4,
        max_run_time: WATCHDOG,
        permissions: vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"child work","label":"F20Child"}),
        )],
        ..Default::default()
    }
}

fn open(temp: &tempfile::TempDir, provider: Arc<ComboProvider>) -> Runtime {
    let workspace = temp.path().join("workspace");
    let _ = std::fs::create_dir_all(&workspace);
    Runtime::open(
        temp.path().join("sessions.sqlite"),
        workspace,
        provider,
        Model::new("fixture", "f20-combo"),
        settings(),
    )
    .unwrap()
}

fn host_events(runtime: &Runtime, session: &str, kind: &str) -> Vec<Value> {
    runtime
        .events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            CoreEvent::Host {
                kind: ref event_kind,
                data,
            } if event_kind == kind => Some(data),
            _ => None,
        })
        .collect()
}

async fn run_prompt(runtime: &Runtime, session: &str, text: &str) {
    tokio::time::timeout(
        WATCHDOG,
        runtime.prompt(session, text, CancellationToken::new()),
    )
    .await
    .expect("prompt watchdog")
    .unwrap();
}

/// Q1 (session reopen) + Q2 (cross-session) + Q3 (child agent) on tip without ContentStore.
#[tokio::test]
async fn base_combo_session_reopen_isolation_and_child_agent() {
    let temp = tempfile::tempdir().unwrap();
    let provider = ComboProvider::new();
    let rt = open(&temp, provider.clone());

    let session_a = rt.create(Some("f20-a".into())).unwrap().id;
    let session_b = rt.create(Some("f20-b".into())).unwrap().id;

    run_prompt(&rt, &session_a, "session a note").await;
    run_prompt(&rt, &session_b, "session b note").await;

    let view_a = rt.get(&session_a).unwrap();
    let view_b = rt.get(&session_b).unwrap();
    assert!(
        view_a
            .messages
            .iter()
            .any(|message| message.text() == "session a note")
    );
    assert!(
        view_b
            .messages
            .iter()
            .any(|message| message.text() == "session b note")
    );
    assert!(
        view_a
            .messages
            .iter()
            .all(|message| message.text() != "session b note"),
        "Q2: session A must not carry B's user turn"
    );
    assert!(
        view_b
            .messages
            .iter()
            .all(|message| message.text() != "session a note"),
        "Q2: session B must not carry A's user turn"
    );

    run_prompt(&rt, &session_a, "spawn child").await;
    let parent_tasks = rt.tasks(&session_a).unwrap();
    assert!(
        parent_tasks
            .iter()
            .any(|task| task.state == TaskState::Completed)
    );
    let started = host_events(&rt, &session_a, "agent_child_started");
    assert_eq!(started.len(), 1, "Q3: one child start event");
    let child_session = started[0]["child_session"].as_str().unwrap().to_string();
    assert_ne!(child_session, session_a);
    assert_ne!(child_session, session_b);
    let child_tasks = rt.tasks(&child_session).unwrap();
    assert_eq!(child_tasks.len(), 1);
    assert_eq!(child_tasks[0].state, TaskState::Completed);
    let child_view = rt.get(&child_session).unwrap();
    assert!(
        child_view
            .messages
            .iter()
            .any(|message| message.text() == "child work")
    );
    assert!(
        child_view.messages.iter().all(|message| {
            message.text() != "session a note"
                && message.text() != "session b note"
                && message.text() != "spawn child"
        }),
        "Q3: parent/other prompts must not leak into the child session"
    );
    assert_eq!(
        host_events(&rt, &session_a, "agent_child_finished").len(),
        1
    );
    assert!(
        host_events(&rt, &session_b, "agent_child_started").is_empty(),
        "Q2: child start must stay on the parent session, not B"
    );

    drop(rt);

    // Q1: reopen same SQLite; both sessions and child identity survive.
    let rt = open(&temp, provider);
    let recovered_a = rt.get(&session_a).unwrap();
    let recovered_b = rt.get(&session_b).unwrap();
    assert!(
        recovered_a
            .messages
            .iter()
            .any(|message| message.text() == "session a note"),
        "Q1: session A survives reopen"
    );
    assert!(
        recovered_b
            .messages
            .iter()
            .any(|message| message.text() == "session b note"),
        "Q1: session B survives reopen"
    );
    assert!(
        recovered_a
            .messages
            .iter()
            .all(|message| message.text() != "session b note")
    );
    let child_after = rt.get(&child_session).unwrap();
    assert!(
        child_after
            .messages
            .iter()
            .any(|message| message.text() == "child work"),
        "Q1/Q3: child session identity remains after reopen"
    );
    assert_eq!(
        host_events(&rt, &session_a, "agent_child_started").len(),
        1,
        "parent host events remain after reopen"
    );
}

/// Q1 content variant — #18 ContentStore path; macOS only while Linux O_PATH/`EBADF` remains.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn base_combo_content_survives_reopen_on_macos() {
    use cayw_content::Limits;
    use cayw_runtime::ContentConfig;
    use std::path::Path;

    struct Script(String);
    impl Provider for Script {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            Box::pin(stream::iter([AssistantEvent::Done {
                message: script_response(
                    &request,
                    &self.0,
                    &format!("content-{}", request.messages.len()),
                ),
            }]))
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let put = json!({"name":"combo-note","text":"session-a","expected_version":0});
    let config = RuntimeConfig {
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "f20-combo-device".into(),
            limits: Limits {
                max_content_bytes: 16384,
                ..Default::default()
            },
        }),
        permissions: vec![ScopedPermission::exact("content.put", put)],
        ..Default::default()
    };
    let open_rt = |root: &Path, script: &str, config: RuntimeConfig| {
        Runtime::open(
            root.join("sessions.sqlite"),
            root.join("workspace"),
            Arc::new(Script(script.into())),
            Model::new("fixture", "f20-content"),
            config,
        )
        .unwrap()
    };
    let script =
        "return cayw.call('content.put',{name='combo-note',text='session-a',expected_version=0})";
    let rt = open_rt(&root, script, config.clone());
    let session_a = rt.create(None).unwrap().id;
    let session_b = rt.create(None).unwrap().id;
    run_prompt(&rt, &session_a, "store").await;
    assert_eq!(
        rt.content_inspect(&session_a, None, "combo-note")
            .unwrap()
            .unwrap()
            .text,
        "session-a"
    );
    assert!(
        rt.content_inspect(&session_b, None, "combo-note")
            .unwrap()
            .is_none()
    );
    drop(rt);
    let rt = open_rt(&root, "return 1", config);
    assert_eq!(
        rt.content_inspect(&session_a, None, "combo-note")
            .unwrap()
            .unwrap()
            .text,
        "session-a"
    );
    assert!(
        rt.content_inspect(&session_b, None, "combo-note")
            .unwrap()
            .is_none()
    );
}

/// Q4 (tip-feasible half) — real host process + settled capability result +
/// `processes_stopped` audit survive SQLite reopen. Does **not** claim Slice A
/// `result.release` (payload becomes unretrievable); that seal still needs A.
#[cfg(unix)]
#[tokio::test]
async fn process_cleanup_and_settled_result_survive_reopen() {
    use cayw_core::{CapabilityOutcome, CoreEvent as Ev};
    use cayw_host::{Profile, ProfileKind};
    use std::collections::BTreeMap;
    use std::path::Path;

    struct ProcessScript;
    impl Provider for ProcessScript {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let message = script_response(
                &request,
                "return cayw.call('test.run',{profile='f20-true'})",
                &format!("q4-{}", request.messages.len()),
            );
            Box::pin(stream::iter([AssistantEvent::Done { message }]))
        }
    }

    fn process_settings() -> RuntimeConfig {
        let mut config = RuntimeConfig {
            max_concurrent_runs: 1,
            max_run_time: WATCHDOG,
            ..Default::default()
        };
        config.codemode.timeout = Duration::from_secs(20);
        config.host.call_timeout = Duration::from_secs(20);
        config.host.profiles.insert(
            "f20-true".into(),
            Profile {
                kind: ProfileKind::Test,
                program: "/usr/bin/true".into(),
                args: vec![],
                env: BTreeMap::new(),
                timeout: Duration::from_secs(10),
                max_output_bytes: 4096,
                allow_filter: false,
                io: cayw_host::ProcessIo::Pipes,
            },
        );
        config.permissions = vec![ScopedPermission::exact(
            "test.run",
            json!({"profile":"f20-true"}),
        )];
        config
    }

    fn open_process(root: &Path) -> Runtime {
        Runtime::open(
            root.join("sessions.sqlite"),
            root.join("workspace"),
            Arc::new(ProcessScript),
            Model::new("fixture", "f20-q4-process"),
            process_settings(),
        )
        .unwrap()
    }

    fn settled_test_run(rt: &Runtime, session: &str) -> Value {
        rt.events(session, 0, 1000)
            .unwrap()
            .into_iter()
            .find_map(|record| match record.event {
                Ev::CapabilitySettled { call }
                    if call.name == "test.run"
                        && matches!(
                            &call.outcome,
                            Some(CapabilityOutcome::Completed { .. })
                        ) =>
                {
                    Some(serde_json::to_value(&call).unwrap())
                }
                _ => None,
            })
            .expect("Q4: test.run must settle Completed")
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let rt = open_process(&root);
    let session = rt.create(Some("f20-q4".into())).unwrap().id;
    run_prompt(&rt, &session, "run host process").await;
    let tasks = rt.tasks(&session).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::Completed);
    let settled_before = settled_test_run(&rt, &session);
    let stopped_before: Vec<_> = rt
        .audit(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect();
    assert_eq!(
        stopped_before.len(),
        1,
        "Q4: owned process cleanup must write processes_stopped"
    );
    assert_eq!(stopped_before[0].task_id.as_ref(), Some(&tasks[0].id));
    let task_id = tasks[0].id.clone();
    drop(rt);

    let rt = open_process(&root);
    let settled_after = settled_test_run(&rt, &session);
    assert_eq!(
        settled_after, settled_before,
        "Q4: settled capability result must survive SQLite reopen"
    );
    let stopped_after: Vec<_> = rt
        .audit(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect();
    assert_eq!(stopped_after.len(), 1);
    assert_eq!(stopped_after[0].task_id.as_ref(), Some(&task_id));
    assert_eq!(rt.task(&task_id).unwrap().state, TaskState::Completed);
    // Slice A `result.release` is still out of scope on this branch; do not claim it.
}

/// Q5 — `bg.*` + gateway OwnershipStore reopen still need SLICE P on tip.
#[tokio::test]
#[ignore = "pending SLICE P (bg.* + gateway ownership); not on this branch head"]
async fn pending_background_ownership_survives_reopen() {
    panic!("enable after feat/f13-background-tasks lands on dev");
}

/// Q6 (tip-feasible) — MCP resources list/read through CodeMode settle on the
/// same Session; capability + bridge dispatch survive SQLite reopen. Does **not**
/// claim F14 lazy-start / idle-stop lifecycle (still needs SLICE R).
#[tokio::test]
async fn mcp_resources_share_session_and_survive_reopen() {
    use async_trait::async_trait;
    use cayw_bridge::{
        AccessRequest, DispatchOutcome, Limits, Manifest, Provenance, Registry,
        RegistryPolicy, ResourceSpec, SourceHandle, SourceIdentity, SourceOperation, SourceReply,
        SourceRequest, Visibility,
    };
    use cayw_codemode::CapabilityError;
    use cayw_core::{
        CapabilityCallRecord, CapabilityOutcome, CoreEvent as Ev, SessionState,
    };
    use cayw_runtime::BridgeDispatchEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const URI: &str = "fixture://f20/manual";
    const MIME: &str = "text/markdown";
    const BODY: &str = "# F20 MCP fixture\n";

    struct Policy;
    #[async_trait]
    impl RegistryPolicy for Policy {
        fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
            true
        }
        async fn authorize(
            &self,
            _: &AccessRequest,
            _: CancellationToken,
        ) -> Result<(), CapabilityError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct State {
        requests: Mutex<Vec<SourceRequest>>,
        disconnected: AtomicUsize,
    }
    struct Source {
        state: Arc<State>,
    }
    #[async_trait]
    impl SourceHandle for Source {
        fn ownership_key(&self) -> &str {
            "f20-mcp-fixture"
        }
        async fn request(
            &self,
            request: SourceRequest,
            _: CancellationToken,
        ) -> Result<SourceReply, CapabilityError> {
            self.state.requests.lock().unwrap().push(request.clone());
            let value = match &request.operation {
                SourceOperation::ReadResource { uri } => {
                    assert_eq!(uri, URI);
                    json!({"uri":uri,"mime_type":MIME,"text":BODY})
                }
                other => panic!("unexpected source operation: {other:?}"),
            };
            Ok(SourceReply {
                key: request.key,
                value,
            })
        }
        fn disconnect(&self) {
            self.state.disconnected.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct Script(String);
    impl Provider for Script {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            Box::pin(stream::iter([AssistantEvent::Done {
                message: script_response(
                    &request,
                    &self.0,
                    &format!("q6-{}", request.messages.len()),
                ),
            }]))
        }
    }

    fn calls(rt: &Runtime, id: &str) -> Vec<CapabilityCallRecord> {
        rt.events(id, 0, 1000)
            .unwrap()
            .into_iter()
            .filter_map(|event| match event.event {
                Ev::CapabilitySettled { call } | Ev::CapabilityReconciled { call, .. } => {
                    Some(call)
                }
                _ => None,
            })
            .collect()
    }

    fn dispatches(rt: &Runtime, id: &str) -> Vec<BridgeDispatchEvent> {
        let mut cursor = 0;
        let mut output = Vec::new();
        loop {
            let page = rt.bridge_dispatches(id, cursor, 8).unwrap();
            if page.next_cursor == cursor {
                break;
            }
            cursor = page.next_cursor;
            output.extend(page.events);
        }
        output
    }

    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let state = Arc::new(State::default());
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry
        .register(
            SourceIdentity {
                id: "local".into(),
                version: "1.0".into(),
                origin: "f20 combo mcp fixture".into(),
            },
            Manifest {
                tools: Vec::new(),
                skills: Vec::new(),
                resources: vec![ResourceSpec {
                    uri: URI.into(),
                    description: "F20 fixture manual".into(),
                    mime_type: MIME.into(),
                }],
                search: false,
            },
            Box::new(Source {
                state: state.clone(),
            }),
        )
        .unwrap();
    let settings = RuntimeConfig {
        bridge: Some(registry.consumer()),
        max_run_time: WATCHDOG,
        permissions: vec![
            ScopedPermission::exact("mcp.resources.list", json!({})),
            ScopedPermission::exact(
                "mcp.resources.read",
                json!({"server":"local","uri":URI}),
            ),
        ],
        ..Default::default()
    };
    let script = format!(
        "local l=cayw.call('mcp.resources.list',{{}})\nlocal r=cayw.call('mcp.resources.read',{{server='local',uri='{URI}'}})\nreturn {{list=l,read=r}}"
    );
    let open_rt = |settings: RuntimeConfig, script: &str| {
        Runtime::open(
            temp.path().join("sessions.sqlite"),
            temp.path().join("workspace"),
            Arc::new(Script(script.into())),
            Model::new("fixture", "f20-q6-mcp"),
            settings,
        )
        .unwrap()
    };
    let rt = open_rt(settings.clone(), &script);
    let session = rt.create(Some("f20-q6".into())).unwrap().id;
    assert_eq!(
        rt.prompt(&session, "read mcp resource", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    {
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "Q6: only the read reaches the source");
    }
    let settled = calls(&rt, &session);
    assert_eq!(settled.len(), 2, "Q6: list + read settle on the session");
    assert!(
        settled
            .iter()
            .all(|call| matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })))
    );
    let records = dispatches(&rt, &session);
    assert!(
        records
            .iter()
            .any(|event| event.record.capability == "mcp.resources.read"
                && matches!(&event.outcome, Some(DispatchOutcome::Succeeded { value }) if value["value"]["text"] == BODY)),
        "Q6: durable bridge success for mcp.resources.read: {records:#?}"
    );
    let settled_json = serde_json::to_value(&settled).unwrap();
    let dispatch_json = serde_json::to_value(
        records
            .iter()
            .filter(|event| event.outcome.is_some())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    drop(rt);

    let rt = open_rt(settings, "return 1");
    let settled_after = calls(&rt, &session);
    assert_eq!(
        serde_json::to_value(&settled_after).unwrap(),
        settled_json,
        "Q6: MCP capability settlements survive SQLite reopen"
    );
    let records_after = dispatches(&rt, &session);
    let terminal_after: Vec<_> = records_after
        .iter()
        .filter(|event| event.outcome.is_some())
        .collect();
    assert_eq!(
        serde_json::to_value(&terminal_after).unwrap(),
        dispatch_json,
        "Q6: MCP bridge terminal dispatch survives SQLite reopen"
    );
    let _ = state.disconnected.load(Ordering::Acquire);
    // F14 lazy-start / idle-stop lifecycle remains on SLICE R; not claimed here.
}
