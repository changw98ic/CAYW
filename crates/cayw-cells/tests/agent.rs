mod support;
use async_trait::async_trait;
use cayw_agent::{Agent, AgentTool};
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, ToolDefinition,
};
use cayw_cells::*;
use cayw_codemode::{CapabilityError, CapabilityHost, Limits};
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use support::backend;
use tokio::sync::Semaphore;

struct Host(AtomicUsize);
#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "write".into(),
            description: "fixture host operation".into(),
            parameters: json!({"type":"object","additionalProperties":false}),
        }]
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"ok":true}))
    }
}
struct ProviderFixture {
    script: String,
    yield_time_ms: Option<u64>,
    terminal_status: CellStatus,
    turn: AtomicUsize,
    cell: Mutex<Option<String>>,
}
impl Provider for ProviderFixture {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let system = cayw_ai::current_system(&request.messages);
        assert_eq!(system.tools.len(), 1);
        assert_eq!(system.tools[0].name, "execute_code");
        let turn = self.turn.fetch_add(1, Ordering::SeqCst);
        let mut assistant = AssistantMessage::new(&request.model);
        let arguments = if turn == 0 {
            let mut arguments =
                json!({"action":"exec","script":self.script,"max_output_tokens":4096});
            if let Some(wait) = self.yield_time_ms {
                arguments["yield_time_ms"] = wait.into();
            }
            arguments
        } else {
            let result = request
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    Message::ToolResult(result) => Some(result),
                    _ => None,
                })
                .unwrap();
            let page: CellPage =
                serde_json::from_value(result.structured_content.clone().unwrap()).unwrap();
            *self.cell.lock().unwrap() = Some(page.cell_id.clone());
            if page.status.is_terminal() {
                assert_eq!(page.status, self.terminal_status, "{page:?}");
                assistant.stop_reason = StopReason::Stop;
                assistant
                    .content
                    .push(Content::text("completed fixture task"));
                return futures::stream::iter([AssistantEvent::Done { message: assistant }])
                    .boxed();
            }
            json!({"action":"wait","cell_id":page.cell_id,"cursor":page.next_cursor,"continue_execution":true,"yield_time_ms":10000,"max_output_tokens":4096})
        };
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content.push(Content::ToolCall(ToolCall::new(
            format!("call-{turn}"),
            "execute_code",
            arguments,
        )));
        futures::stream::iter([AssistantEvent::Done { message: assistant }]).boxed()
    }
}

#[tokio::test]
async fn agent_uses_one_entry_exec_then_wait_with_multiple_internal_host_operations() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host(AtomicUsize::new(0)));
    let limits = CellLimits::default();
    let manager = CellManager::new(
        CellIdentity {
            session_id: "session".into(),
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            permission_context: "fixture-gate".into(),
        },
        journal,
        backend(host.clone()),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        Limits::default(),
    )
    .unwrap();
    let provider = Arc::new(ProviderFixture {
        script: "yield_control(); cayw.call('write', {}); cayw.call('write', {}); text('complete')"
            .into(),
        yield_time_ms: Some(10000),
        terminal_status: CellStatus::Completed,
        turn: AtomicUsize::new(0),
        cell: Mutex::new(None),
    });
    let agent = Agent::new(provider.clone(), Model::new("fixture", "fixture"));
    let tool: Arc<dyn AgentTool> = Arc::new(CellTool::new(manager.clone()));
    agent.set_tools(vec![tool]).unwrap();
    let result = agent.prompt(Message::user("exercise cells")).await.unwrap();
    assert_eq!(result.last_message.stop_reason, StopReason::Stop);
    assert_eq!(host.0.load(Ordering::SeqCst), 2);
    assert!(provider.turn.load(Ordering::SeqCst) >= 3);
    assert_eq!(
        manager
            .record(provider.cell.lock().unwrap().as_ref().unwrap())
            .unwrap()
            .calls
            .len(),
        2
    );
    assert_eq!(
        manager
            .record(provider.cell.lock().unwrap().as_ref().unwrap())
            .unwrap()
            .parent_call_id,
        "call-0"
    );
    manager.close().await;
}

#[tokio::test]
async fn cell_exit_ends_only_the_cell_and_agent_produces_its_next_message() {
    let dir = tempfile::tempdir().unwrap();
    let limits = CellLimits::default();
    let host = Arc::new(Host(AtomicUsize::new(0)));
    let manager = CellManager::new(
        CellIdentity {
            session_id: "session".into(),
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            permission_context: "gate".into(),
        },
        Arc::new(SqliteJournal::open(dir.path().join("db")).unwrap()),
        backend(host.clone()),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        Limits::default(),
    )
    .unwrap();
    let provider = Arc::new(ProviderFixture {
        script: "exit(7); cayw.call('write', {})".into(),
        yield_time_ms: Some(10000),
        terminal_status: CellStatus::Exited,
        turn: AtomicUsize::new(0),
        cell: Mutex::new(None),
    });
    let agent = Agent::new(provider.clone(), Model::new("fixture", "fixture"));
    agent
        .set_tools(vec![Arc::new(CellTool::new(manager.clone()))])
        .unwrap();
    let result = agent.prompt(Message::user("exit one cell")).await.unwrap();
    assert_eq!(result.last_message.stop_reason, StopReason::Stop);
    assert!(provider.turn.load(Ordering::SeqCst) >= 2);
    assert_eq!(host.0.load(Ordering::SeqCst), 0);
    let cell = provider.cell.lock().unwrap().clone().unwrap();
    assert_eq!(manager.record(&cell).unwrap().parent_call_id, "call-0");
    manager.close().await;
}

#[tokio::test]
async fn omitted_wait_returns_short_operation_without_an_extra_model_round() {
    for max_yield_time in [None, Some(std::time::Duration::from_millis(500))] {
        let dir = tempfile::tempdir().unwrap();
        let limits = CellLimits {
            max_yield_time,
            ..Default::default()
        };
        let host = Arc::new(Host(AtomicUsize::new(0)));
        let manager = CellManager::new(
            CellIdentity {
                session_id: "session".into(),
                task_id: "task".into(),
                attempt_id: "attempt".into(),
                permission_context: "fixture-gate".into(),
            },
            Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap()),
            backend(host.clone()),
            ReadCoordinator::new(vec![], limits.clone()).unwrap(),
            limits,
            Limits::default(),
        )
        .unwrap();
        let provider = Arc::new(ProviderFixture {
            script: "return cayw.call('write', {})".into(),
            yield_time_ms: None,
            terminal_status: CellStatus::Completed,
            turn: AtomicUsize::new(0),
            cell: Mutex::new(None),
        });
        let agent = Agent::new(provider.clone(), Model::new("fixture", "fixture"));
        agent
            .set_tools(vec![Arc::new(CellTool::new(manager.clone()))])
            .unwrap();
        agent
            .prompt(Message::user("run short operation"))
            .await
            .unwrap();
        assert_eq!(host.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            provider.turn.load(Ordering::SeqCst),
            2,
            "exec result must arrive before the model's next turn"
        );
        manager.close().await;
    }
}

struct DelayedHost {
    entered: Semaphore,
    calls: AtomicUsize,
}

#[async_trait]
impl CapabilityHost for DelayedHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        Host(AtomicUsize::new(0)).capabilities()
    }

    async fn call(
        &self,
        _: &str,
        _: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => Ok(json!({"ok": true})),
            _ = cancel.cancelled() => Err(CapabilityError::new("cancelled before fixture write")),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn default_wait_passes_the_old_deadline_and_keeps_explicit_wakeups() {
    // Two seconds is simulated Host work that crosses the former one-second
    // default. These assertions measure model turns, not wall-clock speed.
    for (max_yield_time, explicit_wait, wake, expected_turns) in [
        (None, None, "complete", 2),
        (Some(Duration::from_secs(4)), None, "complete", 2),
        (None, Some(u64::MAX), "complete", 2),
        (None, Some(0), "complete", 3),
        (None, None, "permission", 3),
        (None, None, "cancel", 1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let limits = CellLimits {
            max_yield_time,
            ..Default::default()
        };
        let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
        let host = Arc::new(DelayedHost {
            entered: Semaphore::new(0),
            calls: AtomicUsize::new(0),
        });
        let manager = CellManager::new(
            CellIdentity {
                session_id: "session".into(),
                task_id: "task".into(),
                attempt_id: "attempt".into(),
                permission_context: "fixture-gate".into(),
            },
            journal.clone(),
            backend(host.clone()),
            ReadCoordinator::new(vec![], limits.clone()).unwrap(),
            limits,
            Limits::default(),
        )
        .unwrap();
        let provider = Arc::new(ProviderFixture {
            script: "return cayw.call('write', {})".into(),
            yield_time_ms: explicit_wait,
            terminal_status: CellStatus::Completed,
            turn: AtomicUsize::new(0),
            cell: Mutex::new(None),
        });
        let agent = Agent::new(provider.clone(), Model::new("fixture", "fixture"));
        let tool = Arc::new(CellTool::new(manager.clone()));
        if let Some(max) = max_yield_time {
            let definition = tool.definition();
            let field = &definition.parameters["oneOf"][0]["properties"]["yield_time_ms"];
            assert_eq!(field["default"], json!(max.as_millis() as u64));
            assert_eq!(field["maximum"], field["default"]);
        }
        agent.set_tools(vec![tool]).unwrap();
        let control = agent.control();
        let wake_manager = manager.clone();
        let wake_host = host.clone();
        let waker = tokio::spawn(async move {
            wake_host.entered.acquire().await.unwrap().forget();
            if wake == "permission" {
                let cells = journal
                    .records(wake_manager.identity(), wake_manager.limits())
                    .unwrap();
                wake_manager.notify_permission_wait(&cells[0].cell_id);
            } else if wake == "cancel" {
                control.abort();
            }
        });
        let result = agent
            .prompt(Message::user("run controlled operation"))
            .await
            .unwrap();
        waker.await.unwrap();
        assert_eq!(
            provider.turn.load(Ordering::SeqCst),
            expected_turns,
            "wake={wake}, max={max_yield_time:?}, explicit={explicit_wait:?}"
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        if wake == "cancel" {
            assert_eq!(result.last_message.stop_reason, StopReason::Aborted);
        } else {
            assert_eq!(result.last_message.stop_reason, StopReason::Stop);
        }
        manager.close().await;
    }
}
