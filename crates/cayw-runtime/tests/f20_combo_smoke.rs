//! F20 / SLICE Q — combo acceptance base smoke (not a new scheduler).
//!
//! Tip-ready path: same-field `agent.run` child identity, cross-Session
//! isolation, and Session SQLite reopen. Local ContentStore reopen is a
//! separate macOS-gated check (#18 path); Linux ContentStore still hits the
//! known O_PATH/`EBADF` sync gap and is not claimed here.
//!
//! Pending A/P/F14 gates stay `#[ignore]` until those slices land on `dev`.
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

/// Q4 — waits for SLICE A result handles + process cleanup alignment.
#[tokio::test]
#[ignore = "pending SLICE A (result.release) + process cleanup combo seal"]
async fn pending_result_release_aligns_with_process_cleanup() {
    panic!("enable after feat/f01-f03-result-handles lands on dev");
}

/// Q5 — waits for SLICE P background + OwnershipStore reopen.
#[tokio::test]
#[ignore = "pending SLICE P (bg.* + gateway ownership) combo seal"]
async fn pending_background_ownership_survives_reopen() {
    panic!("enable after feat/f13-background-tasks lands on dev");
}

/// Q6 — waits for SLICE R/F14 MCP lifecycle sharing session results.
#[tokio::test]
#[ignore = "pending SLICE R/F14 MCP lifecycle + A result path"]
async fn pending_mcp_lifecycle_shares_session_results() {
    panic!("enable after feat/f14-mcp-lifecycle lands on dev");
}
