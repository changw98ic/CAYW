//! F20 / SLICE Q — combo acceptance smoke (not a new scheduler).
//!
//! Tip-proven on this branch: Q1 Session SQLite reopen, Q2 cross-Session
//! isolation, Q3 `agent.run` child identity, Q4 process cleanup + settled
//! capability result reopen (unix), Q6 MCP resources list/read via CodeMode
//! + reopen, and Q6 real MCP stdio process start/stop/cleanup (unix).
//! Tip-tree multi-capability combo (session+child+process+MCP stdio) exercises
//! the strongest same-tree path without merging #37–#54. ContentStore reopen
//! remains macOS-gated (#18 / Linux O_PATH).
//!
//! Still pending (not claimed): Slice A `result.release` (#40 — absent on tip),
//! Q5 `bg.*`/gateway ownership (SLICE P / #45), F14 ManagedStdio lazy/idle
//! (SLICE R / #41), Q7 WebUI/TUI/auth IPC, Q8 multi-module same-tree seal
//! (needs A+P+R merges; tip combo is not that seal). Ignored items must not
//! be read as F20 completion — see `scripts/run-f20-combo-smoke.sh`.
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
/// `result.release`: tip has no `results.rs` / `result.*` tools (#40 not merged).
/// ContentStore alone cannot soft-release a result handle on this tree.
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
                        && matches!(&call.outcome, Some(CapabilityOutcome::Completed { .. })) =>
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
    // Slice A `result.release` is absent on tip (#40); see
    // `result_release_not_registered_on_tip` for the honest absence check.
}

/// Q5 — `bg.*` + gateway OwnershipStore reopen still need SLICE P on tip.
/// Do **not** un-ignore: tip has no `background.rs` / `bg.*` tools (#45).
#[tokio::test]
#[ignore = "pending SLICE P (bg.* + gateway ownership); not on this branch head"]
async fn pending_background_ownership_survives_reopen() {
    panic!("enable after feat/f13-background-tasks lands on dev");
}

/// Q4 honesty — `result.release` is not registered on tip (SLICE A / #40).
/// This is an absence check, not a release seal. Do not read as Q4 complete.
#[tokio::test]
async fn result_release_not_registered_on_tip() {
    struct Script;
    impl Provider for Script {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            Box::pin(stream::iter([AssistantEvent::Done {
                message: script_response(
                    &request,
                    "return cayw.call('result.release',{handle='demo'})",
                    &format!("q4-absent-{}", request.messages.len()),
                ),
            }]))
        }
    }

    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        temp.path().join("sessions.sqlite"),
        temp.path().join("workspace"),
        Arc::new(Script),
        Model::new("fixture", "f20-q4-absent"),
        RuntimeConfig {
            max_run_time: WATCHDOG,
            // Even with an exact permission, tip Runtime has no result.* catalog entry.
            permissions: vec![ScopedPermission::exact(
                "result.release",
                json!({"handle":"demo"}),
            )],
            ..Default::default()
        },
    )
    .unwrap();
    let session = rt.create(Some("f20-q4-absent".into())).unwrap().id;
    run_prompt(&rt, &session, "try result.release").await;
    let view = rt.get(&session).unwrap();
    let encoded = serde_json::to_string(&view.messages).unwrap();
    assert!(
        encoded.contains("Unknown capability: result.release"),
        "Q4: tip must reject result.release as unknown (SLICE A / #40 absent): {encoded}"
    );
    let settled_release = rt
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .any(|record| match record.event {
            CoreEvent::CapabilitySettled { call }
                if call.name == "result.release"
                    && matches!(
                        &call.outcome,
                        Some(cayw_core::CapabilityOutcome::Completed { .. })
                    ) =>
            {
                true
            }
            _ => false,
        });
    assert!(
        !settled_release,
        "Q4: result.release must not settle Completed on tip without #40"
    );
}

/// Q6 (tip-feasible) — MCP resources list/read through CodeMode settle on the
/// same Session; capability + bridge dispatch survive SQLite reopen. Does **not**
/// claim F14 lazy-start / idle-stop lifecycle (still needs SLICE R).
#[tokio::test]
async fn mcp_resources_share_session_and_survive_reopen() {
    use async_trait::async_trait;
    use cayw_bridge::{
        AccessRequest, DispatchOutcome, Limits, Manifest, Provenance, Registry, RegistryPolicy,
        ResourceSpec, SourceHandle, SourceIdentity, SourceOperation, SourceReply, SourceRequest,
        Visibility,
    };
    use cayw_codemode::CapabilityError;
    use cayw_core::{CapabilityCallRecord, CapabilityOutcome, CoreEvent as Ev, SessionState};
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
            ScopedPermission::exact("mcp.resources.read", json!({"server":"local","uri":URI})),
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
    // F14 ManagedStdio lazy-start / idle-stop remains on SLICE R; not claimed here.
}

/// Q6 (tip process half) — real MCP stdio child: spawn → mcp.call/read →
/// Registry disconnect → wait_closed/reaped, then Session settlements reopen.
/// Advances beyond in-memory SourceHandle fixtures. Still **not** F14
/// ManagedStdio lazy/idle recycle (SLICE R / #41).
#[cfg(unix)]
#[tokio::test]
async fn mcp_stdio_process_lifecycle_cleanup_and_session_reopen() {
    use async_trait::async_trait;
    use cayw_bridge::{
        AccessRequest, DispatchOutcome, Limits, Provenance, Registry, RegistryPolicy,
        SourceIdentity, Visibility,
    };
    use cayw_codemode::CapabilityError;
    use cayw_core::{CapabilityCallRecord, CapabilityOutcome, CoreEvent as Ev, SessionState};
    use cayw_runtime::BridgeDispatchEvent;
    use cayw_sources::{StdioConfig, StdioSource};
    use std::path::Path;

    const URI: &str = "fixture://local/addition";
    const BODY_PREFIX: &str = "# Local addition";

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

    struct Script(String);
    impl Provider for Script {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            Box::pin(stream::iter([AssistantEvent::Done {
                message: script_response(
                    &request,
                    &self.0,
                    &format!("q6-stdio-{}", request.messages.len()),
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
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let prepared = StdioSource::connect(
        StdioConfig::new(
            Path::new(env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture")),
            &root,
        ),
        CancellationToken::new(),
    )
    .await
    .expect("Q6: real MCP stdio fixture must spawn");
    assert_eq!(prepared.server_info().name, "cayw-local-fixture");
    let control = prepared.control();
    let before = control.snapshot();
    assert_eq!(before.spawned, 1, "Q6: stdio child spawned");
    assert!(!before.reaped);
    assert!(!before.cleanup_complete);
    let server_pid = before.pid;
    let (manifest, handle) = prepared.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry
        .register(
            SourceIdentity {
                id: "local".into(),
                version: "2".into(),
                origin: "f20 combo real MCP stdio".into(),
            },
            manifest,
            handle,
        )
        .unwrap();

    let settings = RuntimeConfig {
        bridge: Some(registry.consumer()),
        max_run_time: WATCHDOG,
        permissions: vec![
            ScopedPermission::exact("mcp.resources.list", json!({"server":"local"})),
            ScopedPermission::exact("mcp.resources.read", json!({"server":"local","uri":URI})),
            ScopedPermission::exact(
                "mcp.call",
                json!({"server":"local","tool":"add","arguments":{"a":17,"b":25}}),
            ),
        ],
        ..Default::default()
    };
    let script = format!(
        "local l=cayw.call('mcp.resources.list',{{server='local'}})\n\
         local r=cayw.call('mcp.resources.read',{{server='local',uri='{URI}'}})\n\
         local t=cayw.call('mcp.call',{{server='local',tool='add',arguments={{a=17,b=25}}}})\n\
         return {{list=l,read=r,sum=t.value.sum,pid=t.value.server_pid}}"
    );
    let open_rt = |settings: RuntimeConfig, script: &str| {
        Runtime::open(
            root.join("sessions.sqlite"),
            root.join("workspace"),
            Arc::new(Script(script.into())),
            Model::new("fixture", "f20-q6-stdio"),
            settings,
        )
        .unwrap()
    };

    let rt = open_rt(settings.clone(), &script);
    let session = rt.create(Some("f20-q6-stdio".into())).unwrap().id;
    assert_eq!(
        rt.prompt(&session, "drive real mcp stdio", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    let settled = calls(&rt, &session);
    assert!(
        settled.len() >= 3,
        "Q6: list+read+call should settle: {settled:#?}"
    );
    assert!(
        settled
            .iter()
            .all(|call| { matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })) }),
        "Q6: MCP stdio capabilities must complete"
    );
    let after_call = control.snapshot();
    assert_eq!(after_call.pid, server_pid);
    assert_eq!(after_call.tool_calls, 1, "Q6: real tools/call hit child");
    assert_eq!(
        after_call.resource_reads, 1,
        "Q6: real resources/read hit child"
    );
    let records = dispatches(&rt, &session);
    assert!(
        records.iter().any(|event| {
            event.record.capability == "mcp.call"
                && matches!(
                    &event.outcome,
                    Some(DispatchOutcome::Succeeded { value })
                        if value["value"]["sum"] == 42
                )
        }),
        "Q6: mcp.call bridge success: {records:#?}"
    );
    assert!(
        records.iter().any(|event| {
            event.record.capability == "mcp.resources.read"
                && matches!(
                    &event.outcome,
                    Some(DispatchOutcome::Succeeded { value })
                        if value["value"]["text"]
                            .as_str()
                            .is_some_and(|text| text.starts_with(BODY_PREFIX))
                )
        }),
        "Q6: resources.read bridge success: {records:#?}"
    );
    let settled_json = serde_json::to_value(&settled).unwrap();
    let dispatch_json = serde_json::to_value(
        records
            .iter()
            .filter(|event| event.outcome.is_some())
            .collect::<Vec<_>>(),
    )
    .unwrap();

    registry.disconnect("local").unwrap();
    tokio::time::timeout(Duration::from_secs(10), control.wait_closed())
        .await
        .expect("Q6: stdio cleanup watchdog");
    let cleaned = control.snapshot();
    assert!(
        cleaned.reaped,
        "Q6: MCP child must be reaped after disconnect"
    );
    assert!(
        cleaned.cleanup_complete,
        "Q6: MCP stdio cleanup_complete after disconnect"
    );
    assert_eq!(cleaned.disconnected, 1);
    drop(rt);

    let rt = open_rt(settings, "return 1");
    let settled_after = calls(&rt, &session);
    assert_eq!(
        serde_json::to_value(&settled_after).unwrap(),
        settled_json,
        "Q6: real MCP settlements survive SQLite reopen after process cleanup"
    );
    let records_after = dispatches(&rt, &session);
    let terminal_after: Vec<_> = records_after
        .iter()
        .filter(|event| event.outcome.is_some())
        .collect();
    assert_eq!(
        serde_json::to_value(&terminal_after).unwrap(),
        dispatch_json,
        "Q6: real MCP bridge terminals survive reopen after process cleanup"
    );
    // Still not claiming ManagedStdio LifecycleMode::Lazy idle recycle (#41).
}

/// Tip-tree strongest combo (not Q8 seal): one Session runs host process
/// cleanup + real MCP stdio lifecycle, then SQLite reopen.
///
/// Note: `agent.run` child + Bridge MCP in the **same** Runtime currently fails
/// the post-tool model-request reservation on this tip (`model request was
/// refused before provider invocation`) after a successful `agent.run` cell.
/// Q3 child coverage stays in `base_combo_session_reopen_isolation_and_child_agent`
/// (no Bridge). Cross-PR deps for a true Q8 seal remain A/P/R — not merged here.
#[cfg(unix)]
#[tokio::test]
async fn tip_tree_session_child_process_and_mcp_stdio_combo() {
    use async_trait::async_trait;
    use cayw_bridge::{
        AccessRequest, Limits, Provenance, Registry, RegistryPolicy, SourceIdentity, Visibility,
    };
    use cayw_codemode::CapabilityError;
    use cayw_core::{CapabilityOutcome, CoreEvent as Ev};
    use cayw_host::{Profile, ProfileKind};
    use cayw_sources::{StdioConfig, StdioSource};
    use std::collections::BTreeMap;
    use std::path::Path;

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

    struct ComboScript {
        phase: Mutex<String>,
    }
    impl Provider for ComboScript {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            if matches!(request.messages.last(), Some(Message::ToolResult(_))) {
                let phase = self.phase.lock().unwrap().clone();
                let script = match phase.as_str() {
                    "run host process" => "return cayw.call('test.run',{profile='f20-true'})",
                    "drive mcp" => {
                        "return cayw.call('mcp.call',{server='local',tool='add',arguments={a=2,b=3}})"
                    }
                    _ => "return 1",
                };
                return Box::pin(stream::iter([AssistantEvent::Done {
                    message: script_response(
                        &request,
                        script,
                        &format!("tip-combo-cont-{}", request.messages.len()),
                    ),
                }]));
            }
            let prompt = request
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::User { .. } => Some(message.text()),
                    _ => None,
                })
                .unwrap_or_default();
            *self.phase.lock().unwrap() = prompt.clone();
            let script = match prompt.as_str() {
                "run host process" => "return cayw.call('test.run',{profile='f20-true'})",
                "drive mcp" => {
                    "return cayw.call('mcp.call',{server='local',tool='add',arguments={a=2,b=3}})"
                }
                _ => "return 1",
            };
            Box::pin(stream::iter([AssistantEvent::Done {
                message: script_response(
                    &request,
                    script,
                    &format!("tip-combo-{}", request.messages.len()),
                ),
            }]))
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();

    let prepared = StdioSource::connect(
        StdioConfig::new(
            Path::new(env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture")),
            &root,
        ),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let control = prepared.control();
    let (manifest, handle) = prepared.into_parts();
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry
        .register(
            SourceIdentity {
                id: "local".into(),
                version: "2".into(),
                origin: "f20 tip-tree combo MCP".into(),
            },
            manifest,
            handle,
        )
        .unwrap();

    let mut config = RuntimeConfig {
        bridge: Some(registry.consumer()),
        max_concurrent_runs: 4,
        max_run_time: WATCHDOG,
        permissions: vec![
            ScopedPermission::exact("test.run", json!({"profile":"f20-true"})),
            ScopedPermission::exact(
                "mcp.call",
                json!({"server":"local","tool":"add","arguments":{"a":2,"b":3}}),
            ),
        ],
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

    let provider = Arc::new(ComboScript {
        phase: Mutex::new(String::new()),
    });
    let open_rt = |config: RuntimeConfig, provider: Arc<ComboScript>| {
        Runtime::open(
            root.join("sessions.sqlite"),
            root.join("workspace"),
            provider,
            Model::new("fixture", "f20-tip-combo"),
            config,
        )
        .unwrap()
    };

    let rt = open_rt(config.clone(), provider.clone());
    let session = rt.create(Some("f20-tip-combo".into())).unwrap().id;

    run_prompt(&rt, &session, "run host process").await;
    let stopped: Vec<_> = rt
        .audit(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect();
    assert_eq!(stopped.len(), 1, "tip combo: host process cleaned up");
    assert!(
        rt.events(&session, 0, 1000)
            .unwrap()
            .into_iter()
            .any(|record| match record.event {
                Ev::CapabilitySettled { call }
                    if call.name == "test.run"
                        && matches!(&call.outcome, Some(CapabilityOutcome::Completed { .. })) =>
                {
                    true
                }
                _ => false,
            }),
        "tip combo: test.run settled"
    );

    run_prompt(&rt, &session, "drive mcp").await;
    assert_eq!(control.snapshot().tool_calls, 1, "tip combo: MCP tool ran");
    assert!(
        rt.events(&session, 0, 1000)
            .unwrap()
            .into_iter()
            .any(|record| match record.event {
                Ev::CapabilitySettled { call }
                    if call.name == "mcp.call"
                        && matches!(&call.outcome, Some(CapabilityOutcome::Completed { .. })) =>
                {
                    true
                }
                _ => false,
            }),
        "tip combo: mcp.call settled"
    );

    registry.disconnect("local").unwrap();
    tokio::time::timeout(Duration::from_secs(10), control.wait_closed())
        .await
        .expect("tip combo: mcp cleanup watchdog");
    assert!(control.snapshot().reaped, "tip combo: MCP reaped");
    drop(rt);

    let rt = open_rt(config, provider);
    assert!(
        rt.get(&session)
            .unwrap()
            .messages
            .iter()
            .any(|message| message.text() == "run host process"),
        "tip combo: session survives reopen"
    );
    let stopped_after: Vec<_> = rt
        .audit(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect();
    assert_eq!(
        stopped_after.len(),
        1,
        "tip combo: processes_stopped survives reopen"
    );
    assert!(
        rt.events(&session, 0, 1000)
            .unwrap()
            .into_iter()
            .any(|record| match record.event {
                Ev::CapabilitySettled { call } if call.name == "mcp.call" => true,
                _ => false,
            }),
        "tip combo: mcp.call settlement survives reopen"
    );
    // Not Q8 seal: missing result.release / bg.* / F14 ManagedStdio; child+Bridge
    // same-Runtime multi-turn remains a tip interaction gap (see doc comment).
}
