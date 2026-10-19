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
use support::backend;

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
            json!({"action":"exec","script":self.script,"yield_time_ms":10000,"max_output_tokens":4096})
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
    assert!(!agent.snapshot().needs_reconciliation);
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
    assert!(!agent.snapshot().needs_reconciliation);
    let cell = provider.cell.lock().unwrap().clone().unwrap();
    assert_eq!(manager.record(&cell).unwrap().parent_call_id, "call-0");
    manager.close().await;
}
