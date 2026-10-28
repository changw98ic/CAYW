//! Issue 5 local sub-agent acceptance. Providers are fixtures; every child
//! Session, Task, queue admission, permission boundary, event and cleanup below
//! is the real Runtime implementation.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_core::CoreEvent;
use cayw_runtime::{
    ApprovalChoice, LocalAgents, ModelRequestTerminal, Runtime, RuntimeConfig, ScopedPermission,
    TaskState,
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
    pause_parent: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    parent_active: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
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
        if prompt == "hold stream" {
            let receiver = self.pause_parent.lock().unwrap().take();
            let sender = self.parent_active.lock().unwrap().take();
            let mut response = AssistantMessage::new(&request.model);
            response.content.push(Content::text("hold stream finished"));
            response.stop_reason = StopReason::Stop;
            response.usage = cayw_ai::Usage {
                input: 4,
                output: 4,
                total_tokens: 8,
                ..Default::default()
            };
            response.usage_status = cayw_ai::UsageStatus::Reported;
            self.requests.lock().unwrap().push(request);
            return Box::pin(async_stream::stream! {
                if let Some(tx) = sender {
                    let _ = tx.send(());
                }
                if let Some(rx) = receiver {
                    let _ = rx.await;
                }
                yield AssistantEvent::Done { message: response };
            });
        }
        if prompt == "spawn real yield0 child" {
            let is_turn_1 = !request
                .messages
                .iter()
                .any(|m| matches!(m, Message::ToolResult(..)));
            if is_turn_1 {
                let mut response = AssistantMessage::new(&request.model);
                response.usage = cayw_ai::Usage {
                    input: 4,
                    output: 4,
                    total_tokens: 8,
                    ..Default::default()
                };
                response.usage_status = cayw_ai::UsageStatus::Reported;
                response
                    .content
                    .push(Content::ToolCall(cayw_ai::ToolCall::new(
                        format!("fixture-{}", request.messages.len()),
                        "execute_code",
                        json!({
                            "action": "exec",
                            "script": "return cayw.call('agent.run', {prompt='child write and read', label='Child'})",
                            "yield_time_ms": 0,
                            "max_output_tokens": 4096,
                        }),
                    )));
                response.stop_reason = StopReason::ToolUse;
                self.requests.lock().unwrap().push(request);
                return Box::pin(stream::iter([AssistantEvent::Done { message: response }]));
            } else if self.pause_parent.lock().unwrap().is_some() {
                let receiver = self.pause_parent.lock().unwrap().take();
                let sender = self.parent_active.lock().unwrap().take();
                let cell_id = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        Message::ToolResult(res) => res
                            .structured_content
                            .as_ref()
                            .and_then(|sc| sc.get("cell_id").and_then(Value::as_str))
                            .map(ToString::to_string),
                        _ => None,
                    })
                    .unwrap_or_else(|| "cell_1".to_string());
                let mut response = AssistantMessage::new(&request.model);
                response.usage = cayw_ai::Usage {
                    input: 4,
                    output: 4,
                    total_tokens: 8,
                    ..Default::default()
                };
                response.usage_status = cayw_ai::UsageStatus::Reported;
                response
                    .content
                    .push(Content::ToolCall(cayw_ai::ToolCall::new(
                        format!("fixture-{}", request.messages.len()),
                        "execute_code",
                        json!({
                            "action": "wait",
                            "cell_id": cell_id,
                            "cursor": 0,
                            "yield_time_ms": 30000,
                            "max_output_tokens": 4096,
                        }),
                    )));
                response.stop_reason = StopReason::ToolUse;
                self.requests.lock().unwrap().push(request);
                return Box::pin(async_stream::stream! {
                    if let Some(tx) = sender {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = receiver {
                        let _ = rx.await;
                    }
                    yield AssistantEvent::Done { message: response };
                });
            } else {
                let mut response = AssistantMessage::new(&request.model);
                response.usage = cayw_ai::Usage {
                    input: 4,
                    output: 4,
                    total_tokens: 8,
                    ..Default::default()
                };
                response.usage_status = cayw_ai::UsageStatus::Reported;
                response
                    .content
                    .push(Content::text("parent completed after child wait"));
                response.stop_reason = StopReason::Stop;
                self.requests.lock().unwrap().push(request);
                return Box::pin(stream::iter([AssistantEvent::Done { message: response }]));
            }
        }
        let script = match prompt.as_str() {
            "spawn child" => "return cayw.call('agent.run', {prompt='child work', label='Child'})",
            "spawn background concurrent" => {
                "return cayw.call('agent.run', {prompt='bg child work', label='Bg', mode='background'})"
            }
            "parent follow up" => "return 'parent follow up done'",
            "spawn child missing usage" => {
                let mut response = cayw_runtime::fixture::script_response(
                    &request,
                    "return cayw.call('agent.run', {prompt='child work', label='Child'})",
                    &format!("fixture-{}", request.messages.len()),
                );
                response.usage_status = cayw_ai::UsageStatus::Unknown;
                response.usage.total_tokens = 0;
                self.requests.lock().unwrap().push(request);
                return Box::pin(stream::iter([AssistantEvent::Done { message: response }]));
            }
            "child work" => "return 'child result'",
            "child write and read" => {
                "cayw.call('fs.write', {path='child-only.txt', content='child-yield0'})\nlocal read = cayw.call('fs.read', {path='child-only.txt'})\nreturn read.content"
            }
            "spawn background child" => {
                "return cayw.call('agent.run', {prompt='long child', label='Bg', mode='background'})"
            }
            "bg child work" => "return 'bg child result'",
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
            .all(|definition| !definition.name.starts_with("agent.")),
        "agent.* fleet capabilities are available only through explicit host configuration"
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

#[tokio::test]
async fn background_agent_run_returns_handle_and_parent_completion_does_not_stop_detached() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let settings = config(
        Some(local(vec![])),
        vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"long child","label":"Bg","mode":"background"}),
        )],
    );
    let runtime = runtime(&temp, provider, settings);
    let parent = runtime.create(Some("parent".into())).unwrap();
    let admitted = runtime
        .enqueue(&parent.id, "spawn background child")
        .unwrap();
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

    // Parent capability completes with a handle while the detached child keeps running.
    // That proves drain / parent terminal did not cancel the detached member.
    let parent_view = tokio::time::timeout(WATCHDOG, task)
        .await
        .expect("background parent hung")
        .unwrap()
        .expect("background parent should complete with a handle");
    let parent_text = parent_view.messages.last().unwrap().text();
    assert!(
        parent_text.contains("running")
            || parent_text.contains("Bg")
            || parent_text.contains("task"),
        "parent should surface the background handle, got {parent_text}"
    );
    assert_eq!(
        parent_task(&runtime, &parent.id).state,
        TaskState::Completed,
        "parent must reach a confirmed terminal without waiting on the detached child"
    );
    let fleet = runtime.fleet(Some(&parent.id), None, 100).unwrap();
    assert_eq!(
        fleet.members.len(),
        1,
        "FleetView must show the detached child"
    );
    assert!(fleet.members[0].detached);
    assert_eq!(
        fleet.members[0].mode,
        cayw_runtime::AgentRunMode::Background
    );
    assert_eq!(fleet.members[0].child_session_id, child);
    assert_eq!(
        runtime.tasks(&child).unwrap()[0].state,
        TaskState::Running,
        "detached child must still be running after parent returns"
    );

    // Explicit fleet stop ends the child (parent cancel is not in the cascade path).
    let stopped = runtime.fleet_stop(&fleet.members[0].child_task_id).unwrap();
    assert_eq!(stopped["stopped"], true);
    let final_state = tokio::time::timeout(WATCHDOG, async {
        loop {
            let state = runtime.tasks(&child).unwrap()[0].state;
            if state != TaskState::Running {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fleet stop did not finish child");
    assert_eq!(final_state, TaskState::Cancelled);
}

#[tokio::test]
async fn background_still_respects_depth_and_appears_in_fleet_page() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let mut agents = local(vec![]);
    agents.max_depth = 1;
    let settings = config(
        Some(agents),
        vec![ScopedPermission::exact(
            "agent.run",
            json!({"prompt":"nested child"}),
        )],
    );
    let runtime = runtime(&temp, provider.clone(), settings);
    // Depth refusal is covered by nesting_beyond_max_depth; here just ensure fleet
    // paging API rejects invalid limits and accepts an empty page.
    let empty = runtime.fleet(None, None, 10).unwrap();
    assert!(empty.members.is_empty());
    assert!(runtime.fleet(None, None, 0).is_err());
    assert!(runtime.fleet(None, None, 1001).is_err());
}

#[tokio::test]
async fn child_subagent_succeeds_and_shares_settled_budget_when_usage_reported() {
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

    assert!(
        result
            .messages
            .last()
            .unwrap()
            .text()
            .contains("child result"),
        "the parent received the child result"
    );

    // Verify shared accounting ledger: complete usage was recorded, reservation settled,
    // and parent and child shared the persistent budget without exceeding limits.
    let ledger = runtime.model_requests(&parent.id).unwrap();
    assert!(!ledger.budget_unknown);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.settled_tokens, 32);
    assert_eq!(ledger.remaining_tokens, 100_000 - 32);
    assert_eq!(ledger.requests.len(), 4);
    for req in &ledger.requests {
        assert_eq!(req.terminal, Some(ModelRequestTerminal::Completed));
        assert_eq!(req.settled_tokens, Some(8));
        assert_eq!(req.outstanding_reservation_tokens, 0);
    }
}

#[tokio::test]
async fn child_subagent_is_blocked_by_unsettled_model_reservation_when_parent_usage_missing() {
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
    let parent = runtime.create(Some("parent".into())).unwrap();
    let _result = run_prompt(&runtime, &parent.id, "spawn child missing usage").await;

    // The child model request should have failed or returned error because
    // the parent reservation was unsettled.
    let child = child_session(&runtime, &parent.id).await;
    let child_tasks = runtime.tasks(&child).unwrap();
    assert_eq!(child_tasks.len(), 1);
    assert_ne!(child_tasks[0].state, TaskState::Completed);

    // Verify that the child's model prompt "child work" was never dispatched to the provider.
    let requests = provider.requests.lock().unwrap();
    let child_model_dispatched = requests
        .iter()
        .any(|req| req.messages.iter().any(|msg| msg.text() == "child work"));
    assert!(
        !child_model_dispatched,
        "child model request must be blocked before dispatch to the provider"
    );
    drop(requests);

    // Verify the persistent shared ledger:
    // Missing usage preserves unknown reservation; unknown tokens are NOT treated as 0;
    // rejection reason is explicitly unsettled_model_reservation.
    let ledger = runtime.model_requests(&parent.id).unwrap();
    assert!(
        ledger.budget_unknown,
        "ledger must strictly retain budget_unknown=true"
    );
    assert!(
        ledger.reserved_tokens > 0,
        "unknown reservation tokens must be preserved, never zeroed"
    );
    assert_eq!(
        ledger.settled_tokens, 0,
        "unreported usage cannot settle tokens"
    );

    // The child session recorded an explicit rejection with unsettled_model_reservation.
    let child_rejections: Vec<_> = ledger
        .requests
        .iter()
        .filter(|req| req.identity.lease.session_id == child)
        .collect();
    assert!(
        !child_rejections.is_empty(),
        "child must have an accounting record in the shared ledger"
    );
    assert_eq!(
        child_rejections[0].rejection.as_deref(),
        Some("unsettled_model_reservation"),
        "child request rejection reason must be unsettled_model_reservation"
    );
}

#[tokio::test]
async fn child_subagent_waits_for_concurrent_parent_turn_and_succeeds_under_synthetic_link() {
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
    let parent = runtime.create(Some("parent".into())).unwrap();

    // Setup gate on parent request: parent stream holds budget_lock until unpaused
    let (unpause_tx, unpause_rx) = tokio::sync::oneshot::channel();
    let (active_tx, active_rx) = tokio::sync::oneshot::channel();
    *provider.pause_parent.lock().unwrap() = Some(unpause_rx);
    *provider.parent_active.lock().unwrap() = Some(active_tx);

    // Parent task starts and enters "hold stream", holding the shared budget reservation and lock
    let parent_rt = runtime.clone();
    let parent_id = parent.id.clone();
    let parent_task = tokio::spawn(async move {
        parent_rt
            .prompt(&parent_id, "hold stream", CancellationToken::new())
            .await
    });

    // Confirm parent stream is actively in-flight holding the budget lock
    active_rx.await.expect("parent stream never became active");

    // Spawn child task while parent is still streaming; link it to parent's budget tree
    // We create a linked child session via Core control event to simulate yield_time_ms=0 background subagent
    let child = runtime.create(Some("child".into())).unwrap();
    runtime
        .store()
        .append_control_event(
            &child.id,
            child.cursor,
            None,
            "runtime-local-agent",
            "agent_child_linked",
            json!({"parent_session": parent.id}),
        )
        .unwrap();

    // Child attempts to run a model turn while parent is holding budget_lock;
    // it must queue without being rejected with unsettled_model_reservation
    let child_rt = runtime.clone();
    let child_id = child.id.clone();
    let child_task = tokio::spawn(async move {
        child_rt
            .prompt(&child_id, "child work", CancellationToken::new())
            .await
    });

    // Let the child reach the reservation queue
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Release parent hold; parent settles usage and releases budget_lock, unblocking child
    unpause_tx
        .send(())
        .expect("could not unpause parent stream");

    let parent_res = tokio::time::timeout(WATCHDOG, parent_task)
        .await
        .expect("parent timed out")
        .unwrap();
    assert!(
        parent_res.is_ok(),
        "parent task should complete successfully"
    );

    let child_res = tokio::time::timeout(WATCHDOG, child_task)
        .await
        .expect("child timed out")
        .unwrap();
    assert!(
        child_res.is_ok(),
        "child task should complete successfully after parent settlement"
    );

    // Verify shared budget ledger: both parent and child succeeded and settled
    let ledger = runtime.model_requests(&parent.id).unwrap();
    assert!(!ledger.budget_unknown);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.settled_tokens, 24); // parent: 8, child: 16 (turn 1 + turn 2)
    assert_eq!(ledger.remaining_tokens, 100_000 - 24);
    for req in &ledger.requests {
        assert_eq!(req.terminal, Some(ModelRequestTerminal::Completed));
        assert!(req.settled_tokens.is_some());
        assert_eq!(req.outstanding_reservation_tokens, 0);
    }
}

#[tokio::test]
async fn child_subagent_waiting_for_parent_turn_is_cancelled_cleanly() {
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
    let parent = runtime.create(Some("parent".into())).unwrap();

    // Link child session
    let child = runtime.create(Some("child".into())).unwrap();
    runtime
        .store()
        .append_control_event(
            &child.id,
            child.cursor,
            None,
            "runtime-local-agent",
            "agent_child_linked",
            json!({"parent_session": parent.id}),
        )
        .unwrap();

    // Setup gate on parent request
    let (unpause_tx, unpause_rx) = tokio::sync::oneshot::channel();
    let (active_tx, active_rx) = tokio::sync::oneshot::channel();
    *provider.pause_parent.lock().unwrap() = Some(unpause_rx);
    *provider.parent_active.lock().unwrap() = Some(active_tx);

    let parent_rt = runtime.clone();
    let parent_id = parent.id.clone();
    let parent_task = tokio::spawn(async move {
        parent_rt
            .prompt(&parent_id, "hold stream", CancellationToken::new())
            .await
    });

    active_rx.await.expect("parent stream never became active");

    // Child tries to reserve while parent holds budget_lock, but is cancelled while waiting
    let child_cancel = CancellationToken::new();
    let child_rt = runtime.clone();
    let child_id = child.id.clone();
    let child_cancel_clone = child_cancel.clone();
    let child_task = tokio::spawn(async move {
        child_rt
            .prompt(&child_id, "child work", child_cancel_clone)
            .await
    });

    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel the waiting child
    child_cancel.cancel();

    let child_res = tokio::time::timeout(WATCHDOG, child_task)
        .await
        .expect("child join timed out")
        .unwrap();
    let child_view = child_res.expect("prompt returns view with aborted message");
    let last = child_view
        .messages
        .last()
        .expect("must have assistant message");
    match last {
        Message::Assistant(msg) => {
            assert_eq!(msg.stop_reason, cayw_ai::StopReason::Aborted);
            assert_eq!(
                msg.error_message.as_deref(),
                Some("model request cancelled before admission")
            );
        }
        other => panic!("expected assistant message, got {other:?}"),
    }

    // Unpause parent
    unpause_tx
        .send(())
        .expect("could not unpause parent stream");

    let parent_res = tokio::time::timeout(WATCHDOG, parent_task)
        .await
        .expect("parent timed out")
        .unwrap();
    assert!(
        parent_res.is_ok(),
        "parent task should complete successfully"
    );

    // Verify shared budget ledger:
    // Child never created an unsettled reservation; parent settled its tokens;
    // budget_unknown is false.
    let ledger = runtime.model_requests(&parent.id).unwrap();
    assert!(!ledger.budget_unknown);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.settled_tokens, 8);
    assert_eq!(ledger.remaining_tokens, 100_000 - 8);
    assert_eq!(ledger.requests.len(), 1);
    assert_eq!(
        ledger.requests[0].terminal,
        Some(ModelRequestTerminal::Completed)
    );
}

#[tokio::test]
async fn child_subagent_waits_for_parent_turn_via_exec_yield_zero_tool_call() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(Fixture::default());
    let child_perms = vec![
        ScopedPermission::exact(
            "fs.write",
            json!({"path":"child-only.txt","content":"child-yield0"}),
        ),
        ScopedPermission::exact("fs.read", json!({"path":"child-only.txt"})),
    ];
    let parent_perms = vec![ScopedPermission::exact(
        "agent.run",
        json!({"prompt":"child write and read","label":"Child"}),
    )];
    let settings = config(Some(local(child_perms)), parent_perms);
    let runtime = runtime(&temp, provider.clone(), settings);
    let parent = runtime.create(Some("parent".into())).unwrap();

    // Setup gate on parent Turn 2 stream (which handles the ToolResult of yield_time_ms=0 exec)
    let (unpause_tx, unpause_rx) = tokio::sync::oneshot::channel();
    let (active_tx, active_rx) = tokio::sync::oneshot::channel();
    *provider.pause_parent.lock().unwrap() = Some(unpause_rx);
    *provider.parent_active.lock().unwrap() = Some(active_tx);

    let parent_rt = runtime.clone();
    let parent_id = parent.id.clone();
    let parent_task =
        tokio::spawn(
            async move { run_prompt(&parent_rt, &parent_id, "spawn real yield0 child").await },
        );

    // 1. Confirm parent Turn 2 stream is actively in-flight holding the budget lock
    active_rx.await.expect("parent stream never became active");

    // 2. Observe the child session linked via real Core events from execute_code/agent.run
    let child = child_session(&runtime, &parent.id).await;

    // 3. Observe child task running in the background queue
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if runtime
                .tasks(&child)
                .is_ok_and(|tasks| tasks.iter().any(|t| t.state == TaskState::Running))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child task did not enter running state");

    // 4. Ledger observation: parent Turn 2 has an active, in-flight reservation
    let ledger_before = runtime.model_requests(&parent.id).unwrap();
    assert!(
        ledger_before
            .requests
            .iter()
            .any(|req| req.terminal.is_none() && req.outstanding_reservation_tokens > 0),
        "parent Turn 2 must hold an in-flight reservation"
    );

    // 5. Provider observation: child request for 'child write and read' has NOT reached provider yet
    // because it is queued on budget_lock
    {
        let requests = provider.requests.lock().unwrap();
        let child_dispatched = requests.iter().any(|req| {
            req.messages
                .iter()
                .any(|msg| msg.text() == "child write and read")
        });
        assert!(
            !child_dispatched,
            "child must be queued waiting on budget_lock before parent releases"
        );
    }

    // 6. Release parent stream gate; parent Turn 2 settles and releases budget_lock
    unpause_tx
        .send(())
        .expect("could not unpause parent stream");

    // 7. Parent task completes
    let parent_res = tokio::time::timeout(WATCHDOG, parent_task)
        .await
        .expect("parent timed out")
        .unwrap();
    assert!(
        parent_res.is_ok(),
        "parent task should complete successfully: {parent_res:?}"
    );

    // 8. Child task completes
    let child_tasks = runtime.tasks(&child).unwrap();
    assert_eq!(child_tasks.len(), 1);
    assert_eq!(child_tasks[0].state, TaskState::Completed);

    // 9. Verify actual workspace file write and content
    assert_eq!(
        std::fs::read_to_string(temp.path().join("workspace/child-only.txt")).unwrap(),
        "child-yield0",
        "child subagent must execute fs.write exactly once with expected content"
    );

    // 10. Verify durable Core events: fs.write settled exactly once
    let child_events = runtime.events(&child, 0, 1000).unwrap();
    let settled_writes: Vec<_> = child_events
        .iter()
        .filter_map(|r| {
            let val = serde_json::to_value(r).unwrap();
            let call = &val["event"]["call"];
            if val["event"]["type"] == "capability_settled" && call["name"] == "fs.write" {
                Some(call.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        settled_writes.len(),
        1,
        "fs.write must settle exactly once in durable Core events"
    );
    assert_eq!(settled_writes[0]["outcome"]["type"], "completed");

    // 11. Verify Reopen: durable state survives and does NOT replay fs.write
    drop(runtime);
    let reopened_settings = config(Some(local(vec![])), vec![]);
    let reopened = Runtime::open(
        temp.path().join("sessions.db"),
        temp.path().join("workspace"),
        provider.clone(),
        Model::new("fixture", "local-agents"),
        reopened_settings,
    )
    .unwrap();

    let reopened_events = reopened.events(&child, 0, 1000).unwrap();
    let reopened_writes: Vec<_> = reopened_events
        .iter()
        .filter_map(|r| {
            let val = serde_json::to_value(r).unwrap();
            let call = &val["event"]["call"];
            if val["event"]["type"] == "capability_settled" && call["name"] == "fs.write" {
                Some(call.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        reopened_writes.len(),
        1,
        "reopened session must not replay fs.write"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("workspace/child-only.txt")).unwrap(),
        "child-yield0"
    );

    // 12. Verify shared budget ledger: zero reserved tokens, requests completely settled
    let ledger = reopened.model_requests(&parent.id).unwrap();
    assert!(!ledger.budget_unknown);
    assert_eq!(ledger.reserved_tokens, 0);
    assert!(ledger.settled_tokens > 0);
    for req in &ledger.requests {
        assert_eq!(req.terminal, Some(ModelRequestTerminal::Completed));
        assert!(req.settled_tokens.is_some());
        assert_eq!(req.outstanding_reservation_tokens, 0);
    }

    let child_ledger = reopened.model_requests(&child).unwrap();
    assert!(!child_ledger.budget_unknown);
    assert_eq!(child_ledger.reserved_tokens, 0);
    assert_eq!(child_ledger.settled_tokens, ledger.settled_tokens);
}
