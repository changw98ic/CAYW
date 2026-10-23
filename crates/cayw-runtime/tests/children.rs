//! Issue 5 local sub-agent acceptance. Providers are fixtures; every child
//! Session, Task, queue admission, permission boundary, event and cleanup below
//! is the real Runtime implementation.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_core::CoreEvent;
use cayw_runtime::{
    ApprovalChoice, LocalAgents, Runtime, RuntimeConfig, ScopedPermission, TaskState,
};
use futures::stream;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const WATCHDOG: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Fixture {
    requests: Mutex<Vec<ModelRequest>>,
    /// The `stall child` provider blocks on this channel, simulating a model
    /// call that cannot observe cancellation. The test releases it only after
    /// observing the parent's unconfirmed cleanup.
    stall: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}
impl Fixture {
    fn scripts(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl Provider for Fixture {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let prompt = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { .. } => Some(message.text()),
                _ => None,
            })
            .unwrap_or_default();
        if prompt == "long child" {
            let message = request.clone();
            return Box::pin(async_stream::stream! {
                cancel.cancelled().await;
                let mut response = AssistantMessage::new(&message.model);
                response.stop_reason = StopReason::Aborted;
                response
                    .content
                    .push(Content::text("long child observed cancellation"));
                yield AssistantEvent::Done { message: response };
            });
        }
        if prompt == "stall child" {
            if let Some(release) = self.stall.lock().unwrap().take() {
                // A synchronous block here cannot observe the child's
                // cancellation token. The timeout only prevents a failed test
                // from blocking its worker forever.
                let _ = release.recv_timeout(Duration::from_secs(10));
            }
            let mut response = AssistantMessage::new(&request.model);
            response.content.push(Content::text("stall child finished"));
            response.stop_reason = StopReason::Stop;
            response.usage.total_tokens = 8;
            self.requests.lock().unwrap().push(request);
            return Box::pin(stream::iter([AssistantEvent::Done { message: response }]));
        }
        let script = match prompt.as_str() {
            "spawn child" => "return cayw.call('agent.run', {prompt='child work', label='Child'})",
            "child work" => "return 'child result'",
            "spawn long child" => "return cayw.call('agent.run', {prompt='long child'})",
            "spawn stall child" => "return cayw.call('agent.run', {prompt='stall child'})",
            "spawn nested" => "return cayw.call('agent.run', {prompt='nested child'})",
            "nested child" => "return cayw.call('agent.run', {prompt='grandchild'})",
            "spawn child write" => "return cayw.call('agent.run', {prompt='child write'})",
            "spawn unauthorized" => {
                "return cayw.call('agent.run', {prompt='child work', permissions={write=true}})"
            }
            "child write" => {
                "cayw.call('fs.write', {path='child-only.txt', content='child'})\nreturn 'child wrote'"
            }
            "spawn long output" => "return cayw.call('agent.run', {prompt='long output'})",
            "long output" if !matches!(request.messages.last(), Some(Message::ToolResult(_))) => {
                // A literal keeps this independent of which Luau string helpers
                // the sandbox exposes; only the child Runtime output is long.
                return {
                    let script = format!("return '{}'", "x".repeat(4096));
                    let mut response = cayw_runtime::fixture::script_response(
                        &request,
                        &script,
                        &format!("fixture-{}", request.messages.len()),
                    );
                    response.usage.total_tokens = 8;
                    self.requests.lock().unwrap().push(request);
                    Box::pin(stream::iter([AssistantEvent::Done { message: response }]))
                };
            }
            "long output" => "return 'short after long output'",
            other => panic!("unexpected fixture prompt: {other}"),
        };
        let mut response = cayw_runtime::fixture::script_response(
            &request,
            script,
            &format!("fixture-{}", request.messages.len()),
        );
        response.usage.total_tokens = 8;
        self.requests.lock().unwrap().push(request);
        Box::pin(stream::iter([AssistantEvent::Done { message: response }]))
    }
}

fn local(permissions: Vec<ScopedPermission>) -> LocalAgents {
    LocalAgents {
        max_children: 4,
        max_depth: 1,
        shared_total_tokens: 100_000,
        max_calls: 8,
        max_output_bytes: 64 * 1024,
        max_time: Duration::from_secs(10),
        permissions,
    }
}

fn config(local_agents: Option<LocalAgents>, permissions: Vec<ScopedPermission>) -> RuntimeConfig {
    RuntimeConfig {
        max_concurrent_runs: 4,
        max_run_time: WATCHDOG,
        permissions,
        local_agents,
        ..Default::default()
    }
}

fn runtime(temp: &tempfile::TempDir, provider: Arc<Fixture>, config: RuntimeConfig) -> Runtime {
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        provider,
        Model::new("fixture", "local-agents"),
        config,
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

async fn child_session(runtime: &Runtime, parent: &str) -> String {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if let Some(event) = host_events(runtime, parent, "agent_child_started")
                .into_iter()
                .next()
            {
                return event["child_session"].as_str().unwrap().to_string();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("parent never recorded a child session")
}

async fn pending(runtime: &Runtime, session: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if let Some(pending) = runtime.get(session).unwrap().pending_approvals.first() {
                return pending.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child never requested approval")
}

async fn run_prompt(
    runtime: &Runtime,
    session: &str,
    text: &str,
) -> Result<cayw_runtime::SessionView, cayw_runtime::RuntimeError> {
    tokio::time::timeout(
        WATCHDOG,
        runtime.prompt(session, text, CancellationToken::new()),
    )
    .await
    .expect("runtime prompt did not finish")
}

fn parent_task(runtime: &Runtime, session: &str) -> cayw_runtime::TaskRecord {
    runtime
        .tasks(session)
        .unwrap()
        .into_iter()
        .next()
        .expect("session has a task")
}

#[tokio::test]
async fn agent_run_starts_a_real_child_session_and_task_and_records_identity() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let settings = config(
        Some(local(vec![])),
        vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"child work","label":"Child"}),
        )],
    );
    let runtime = runtime(&temp, provider.clone(), settings);
    assert!(
        runtime
            .capabilities()
            .unwrap()
            .iter()
            .any(|definition| definition.name == "agent.run"),
        "an explicitly enabled host must see the agent.run schema"
    );
    let parent = runtime.create(Some("parent".into())).unwrap();
    let result = run_prompt(&runtime, &parent.id, "spawn child")
        .await
        .unwrap();

    let parent_task = parent_task(&runtime, &parent.id);
    assert_eq!(parent_task.state, TaskState::Completed);
    let child = child_session(&runtime, &parent.id).await;
    let child_tasks = runtime.tasks(&child).unwrap();
    assert_eq!(child_tasks.len(), 1);
    assert_eq!(child_tasks[0].state, TaskState::Completed);
    assert_ne!(child_tasks[0].id, parent_task.id);
    assert_ne!(child_tasks[0].session_id, parent.id);

    let child_view = runtime.get(&child).unwrap();
    assert!(
        child_view
            .messages
            .iter()
            .any(|message| message.text() == "child work"),
        "the child must own its own user turn"
    );
    assert!(
        child_view
            .messages
            .iter()
            .all(|message| message.text() != "spawn child"),
        "the parent prompt must not be replayed into the child"
    );
    assert!(
        result
            .messages
            .last()
            .unwrap()
            .text()
            .contains("child result"),
        "the parent's Luau call must receive the child's own terminal output"
    );

    let started = host_events(&runtime, &parent.id, "agent_child_started");
    let finished = host_events(&runtime, &parent.id, "agent_child_finished");
    assert_eq!(started.len(), 1);
    assert_eq!(finished.len(), 1);
    assert_eq!(started[0]["child_session"], child);
    assert_eq!(started[0]["child_task"], child_tasks[0].id);
    assert_eq!(started[0]["parent_task"], parent_task.id);
    assert_eq!(started[0]["depth"], json!(1));
    assert_eq!(finished[0]["state"], "completed");
    assert_eq!(
        host_events(&runtime, &child, "agent_child_linked").len(),
        1,
        "the child log must persist its parent identity independently"
    );
    assert!(
        provider.scripts() >= 4,
        "parent and child must both use the provider"
    );
}

#[tokio::test]
async fn concurrency_one_rejects_the_child_immediately_and_never_deadlocks() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let mut settings = config(
        Some(local(vec![])),
        vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"child work","label":"Child"}),
        )],
    );
    settings.max_concurrent_runs = 1;
    let runtime = runtime(&temp, provider, settings);
    let parent = runtime.create(Some("parent".into())).unwrap();
    let result = tokio::time::timeout(
        WATCHDOG,
        runtime.prompt(&parent.id, "spawn child", CancellationToken::new()),
    )
    .await
    .expect("a full slot must fail fast instead of waiting for a child")
    .unwrap();

    assert_eq!(
        parent_task(&runtime, &parent.id).state,
        TaskState::Completed
    );
    assert_eq!(
        runtime.list().unwrap().len(),
        1,
        "a preflight capacity refusal must not create an orphan child session"
    );
    assert!(
        result
            .messages
            .last()
            .unwrap()
            .text()
            .contains("queue is full"),
        "the model must receive a known refusal: {}",
        result.messages.last().unwrap().text()
    );
    assert!(host_events(&runtime, &parent.id, "agent_child_started").is_empty());
}

#[tokio::test]
async fn child_permissions_are_independent_and_cannot_inherit_parent_grants() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let settings = config(
        Some(local(vec![])),
        vec![
            ScopedPermission::exact("agent.run", json!({"prompt":"child write"})),
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"child-only.txt","content":"child"}),
            ),
        ],
    );
    let runtime = runtime(&temp, provider, settings);
    let parent = runtime.create(Some("parent".into())).unwrap();
    let task_rt = runtime.clone();
    let task_id = parent.id.clone();
    let task = tokio::spawn(async move {
        task_rt
            .prompt(&task_id, "spawn child write", CancellationToken::new())
            .await
    });
    let child = child_session(&runtime, &parent.id).await;
    let approval = pending(&runtime, &child).await;
    assert_eq!(approval.session_id, child);
    assert_eq!(approval.input.capability, "fs.write");
    runtime
        .approve(&child, &approval.id, ApprovalChoice::Deny, "test-user")
        .unwrap();
    tokio::time::timeout(WATCHDOG, task)
        .await
        .expect("parent did not finish after the child approval")
        .unwrap()
        .unwrap();

    assert_eq!(
        parent_task(&runtime, &parent.id).state,
        TaskState::Completed
    );
    assert!(
        !temp.path().join("workspace/child-only.txt").exists(),
        "the parent's exact fs.write grant must not authorize the child"
    );
    let grants = runtime.grants(&child).unwrap();
    assert!(
        grants
            .iter()
            .any(|grant| grant.capability == "fs.write" && grant.state == "denied"),
        "the denial belongs to the child's own grant ledger"
    );
    assert!(runtime.grants(&parent.id).unwrap().is_empty());
}

#[tokio::test]
async fn nesting_beyond_max_depth_is_refused_before_any_grandchild_task() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let settings = config(
        Some(local(vec![])),
        vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"nested child"}),
        )],
    );
    let runtime = runtime(&temp, provider, settings);
    let parent = runtime.create(Some("parent".into())).unwrap();
    let result = run_prompt(&runtime, &parent.id, "spawn nested")
        .await
        .unwrap();

    assert_eq!(runtime.list().unwrap().len(), 2, "no grandchild session");
    let child = child_session(&runtime, &parent.id).await;
    assert!(host_events(&runtime, &child, "agent_child_started").is_empty());
    assert!(
        result
            .messages
            .last()
            .unwrap()
            .text()
            .contains("depth limit reached"),
        "the child must report the depth refusal: {}",
        result.messages.last().unwrap().text()
    );
}

#[tokio::test]
async fn parent_cancellation_reaches_the_child_and_waits_for_its_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let settings = config(
        Some(local(vec![])),
        vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"long child"}),
        )],
    );
    let runtime = runtime(&temp, provider, settings);
    let parent = runtime.create(Some("parent".into())).unwrap();
    let admitted = runtime.enqueue(&parent.id, "spawn long child").unwrap();
    let task_rt = runtime.clone();
    let task_id = admitted.id.clone();
    let task =
        tokio::spawn(async move { task_rt.resume(&task_id, CancellationToken::new()).await });
    let child = child_session(&runtime, &parent.id).await;
    tokio::time::timeout(WATCHDOG, async {
        while runtime.tasks(&child).unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    runtime
        .cancel(&runtime.task(&admitted.id).unwrap().cancel_target())
        .unwrap();
    let outcome = tokio::time::timeout(WATCHDOG, task)
        .await
        .expect("parent cancellation hung")
        .unwrap();
    // The public shape of a cancelled run may be an error or a session view that
    // requires reconciliation. The durable task state below is authoritative and
    // proves that the parent did not complete.
    let _ = outcome;

    let child_task = runtime.tasks(&child).unwrap().remove(0);
    assert_eq!(child_task.state, TaskState::Cancelled);
    let parent_task = parent_task(&runtime, &parent.id);
    assert_ne!(
        parent_task.state,
        TaskState::Completed,
        "a cancelled parent must not claim the child completed"
    );
    assert_eq!(runtime.health().unwrap().running, 0);
    assert_eq!(runtime.health().unwrap().queued, 0);
    let finished = host_events(&runtime, &parent.id, "agent_child_finished");
    assert_eq!(finished.len(), 1, "cleanup must be recorded exactly once");
    assert_eq!(finished[0]["state"], "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconfirmed_child_cleanup_keeps_the_handle_slot_and_never_claims_finished() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let (release, blocked) = std::sync::mpsc::channel();
    *provider.stall.lock().unwrap() = Some(blocked);
    let mut agents = local(vec![]);
    // The child provider blocks synchronously and cannot observe cancellation,
    // so the parent's join and terminal both stay unconfirmed until release.
    agents.max_time = Duration::from_secs(1);
    let runtime = runtime(
        &temp,
        provider,
        config(
            Some(agents),
            vec![ScopedPermission::exact(
                "agent.run",
                json!({"prompt":"stall child"}),
            )],
        ),
    );
    let parent = runtime.create(Some("parent".into())).unwrap();
    let task_rt = runtime.clone();
    let task_id = parent.id.clone();
    let task = tokio::spawn(async move {
        task_rt
            .prompt(&task_id, "spawn stall child", CancellationToken::new())
            .await
    });
    let child = child_session(&runtime, &parent.id).await;
    let outcome = tokio::time::timeout(WATCHDOG, task)
        .await
        .expect("unconfirmed cleanup hung")
        .unwrap();
    assert!(
        outcome.is_err(),
        "a parent whose child cleanup is unconfirmed must not report success"
    );

    assert_eq!(
        runtime.tasks(&child).unwrap().remove(0).state,
        TaskState::Running,
        "the blocked worker must still be owned, not detached before the bound"
    );
    let blocked_parent = parent_task(&runtime, &parent.id).state;
    assert!(
        matches!(
            blocked_parent,
            TaskState::Draining | TaskState::OutcomeUnknown
        ),
        "the parent slot must stay blocked: {blocked_parent:?}"
    );
    assert!(
        runtime.health().unwrap().running >= 1,
        "unconfirmed cleanup must retain admission capacity"
    );
    assert!(
        host_events(&runtime, &parent.id, "agent_child_finished").is_empty(),
        "an unconfirmed child must not be recorded as finished"
    );

    release.send(()).unwrap();
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let state = runtime.tasks(&child).unwrap().remove(0).state;
            if !matches!(
                state,
                TaskState::Queued | TaskState::Running | TaskState::Draining
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the retained child worker never reached a terminal");

    assert_eq!(
        parent_task(&runtime, &parent.id).state,
        blocked_parent,
        "the parent cannot silently recover when the retained child later ends"
    );
    assert!(
        host_events(&runtime, &parent.id, "agent_child_finished").is_empty(),
        "no later path may invent finish evidence after unconfirmed cleanup"
    );
}

#[tokio::test]
async fn shared_token_budget_is_consumed_by_the_child_not_reset() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    // 8 tokens per model reply. Parent script reply 8, child script 8 and child
    // final 8 put the tree at 24; the parent's own 16 stays inside the limit.
    let mut agents = local(vec![]);
    agents.shared_total_tokens = 24;
    let settings = config(
        Some(agents),
        vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"child work","label":"Child"}),
        )],
    );
    let runtime = runtime(&temp, provider, settings);
    let parent = runtime.create(Some("parent".into())).unwrap();
    let _ = run_prompt(&runtime, &parent.id, "spawn child").await;
    let parent_task = parent_task(&runtime, &parent.id);
    assert_ne!(
        parent_task.state,
        TaskState::Completed,
        "the child's tokens must remain counted against the parent's tree"
    );
}

#[tokio::test]
async fn child_result_is_bounded_by_max_output_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let mut agents = local(vec![]);
    agents.max_output_bytes = 1024;
    let settings = config(
        Some(agents),
        vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"long output"}),
        )],
    );
    let runtime = runtime(&temp, provider, settings);
    let parent = runtime.create(Some("parent".into())).unwrap();
    let result = run_prompt(&runtime, &parent.id, "spawn long output")
        .await
        .unwrap();
    assert_eq!(
        parent_task(&runtime, &parent.id).state,
        TaskState::Completed
    );
    let text = result.messages.last().unwrap().text();
    assert!(
        !text.contains(&"x".repeat(2048)),
        "the 4 KiB child output must be truncated below the 1 KiB result budget"
    );
}

#[tokio::test]
async fn model_arguments_cannot_add_permissions_or_other_authority() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let runtime = runtime(
        &temp,
        provider,
        config(
            Some(local(vec![])),
            vec![ScopedPermission::exact(
                "agent.run",
                json!({"prompt":"child work","permissions":{"write":true}}),
            )],
        ),
    );
    let parent = runtime.create(Some("parent".into())).unwrap();
    let result = run_prompt(&runtime, &parent.id, "spawn unauthorized")
        .await
        .unwrap();
    assert_eq!(
        parent_task(&runtime, &parent.id).state,
        TaskState::Completed
    );
    assert_eq!(
        runtime.list().unwrap().len(),
        1,
        "an undeclared field must be refused before any child session exists"
    );
    assert!(host_events(&runtime, &parent.id, "agent_child_started").is_empty());
    assert!(
        !result
            .messages
            .last()
            .unwrap()
            .text()
            .contains("child result"),
        "the model cannot widen its own capability arguments into a child run"
    );
}

#[tokio::test]
async fn disabled_configuration_removes_agent_run_from_the_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let runtime = runtime(&temp, provider, config(None, vec![]));
    assert!(
        runtime
            .capabilities()
            .unwrap()
            .iter()
            .all(|definition| definition.name != "agent.run"),
        "agent.run is available only through explicit host configuration"
    );
    let parent = runtime.create(None).unwrap();
    let result = run_prompt(&runtime, &parent.id, "spawn child")
        .await
        .unwrap();
    assert_eq!(
        runtime.list().unwrap().len(),
        1,
        "no child session may start"
    );
    assert!(host_events(&runtime, &parent.id, "agent_child_started").is_empty());
    assert!(
        !result
            .messages
            .last()
            .unwrap()
            .text()
            .contains("child result"),
        "the disabled capability must not reach a child provider"
    );
}
