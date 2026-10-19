use cayw_agent::{
    Agent, AgentHooks, AgentTool, BeforeToolDecision, ExecutionMode, ToolContext, ToolHookContext,
    ToolResult, async_trait,
};
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, ToolDefinition,
};
use cayw_core::{CoreEvent, SessionState, SessionStore};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use uuid::Uuid;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("cayw-core-agent-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn open(&self) -> SessionStore {
        SessionStore::open(self.0.join("sessions.sqlite")).unwrap()
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct OfflineProvider {
    unknown: bool,
    requests: Mutex<Vec<ModelRequest>>,
}
impl Provider for OfflineProvider {
    fn stream(&self, request: ModelRequest, _cancel: CancellationToken) -> AssistantStream {
        let mut requests = self.requests.lock().unwrap();
        let first = requests.is_empty();
        let mut message = AssistantMessage::new(&request.model);
        if first {
            message.stop_reason = StopReason::ToolUse;
            message.content = vec![Content::ToolCall(ToolCall::new(
                "first",
                "record",
                json!({"unknown":self.unknown}),
            ))];
            if self.unknown {
                message.content.push(Content::ToolCall(ToolCall::new(
                    "second",
                    "record",
                    json!({"unknown":false}),
                )));
            }
        } else {
            message.stop_reason = StopReason::Stop;
            message.content = vec![Content::text("Finished the offline fixture")];
        }
        requests.push(request);
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

struct RecordTool {
    executions: Arc<AtomicUsize>,
}
#[async_trait]
impl AgentTool for RecordTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "record".into(),
            description: "Offline counter fixture".into(),
            parameters: json!({"type":"object","properties":{"unknown":{"type":"boolean"}},"required":["unknown"],"additionalProperties":false}),
        }
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        if context.arguments["unknown"] == true {
            Ok(ToolResult {
                outcome_unknown: true,
                ..ToolResult::error("Fixture executor lost its confirmation")
            })
        } else {
            Ok(ToolResult::text("Recorded the fixture"))
        }
    }
}
struct StartHook {
    store: SessionStore,
    lease: cayw_core::AttemptLease,
}
#[async_trait]
impl AgentHooks for StartHook {
    async fn before_tool_call(
        &self,
        context: ToolHookContext,
        _cancel: CancellationToken,
    ) -> Result<BeforeToolDecision, String> {
        self.store
            .mark_tool_started(&self.lease, &context.call.id)
            .map_err(|e| e.to_string())?;
        Ok(BeforeToolDecision::default())
    }
}
fn build_agent(
    store: &SessionStore,
    lease: &cayw_core::AttemptLease,
    unknown: bool,
) -> (Agent, Arc<OfflineProvider>, Arc<AtomicUsize>) {
    let provider = Arc::new(OfflineProvider {
        unknown,
        requests: Mutex::new(vec![]),
    });
    let agent = Agent::new(provider.clone(), Model::new("fixture", "offline"));
    let executions = Arc::new(AtomicUsize::new(0));
    agent
        .set_tools(vec![Arc::new(RecordTool {
            executions: executions.clone(),
        })])
        .unwrap();
    agent.set_execution_mode(ExecutionMode::Sequential).unwrap();
    agent
        .set_hooks(Arc::new(StartHook {
            store: store.clone(),
            lease: lease.clone(),
        }))
        .unwrap();
    let store = store.clone();
    let lease = lease.clone();
    agent.subscribe(move |event, _| {
        let store = store.clone();
        let lease = lease.clone();
        async move {
            store
                .append_agent_event(&lease, &event)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    });
    (agent, provider, executions)
}

// Exercises real Agent events and hooks, including separate result-message timestamps.
#[tokio::test]
async fn real_agent_fixture_round_trip_persists_one_paired_result() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("actual event flow").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    let (agent, provider, executions) = build_agent(&store, &lease, false);
    agent.prompt(Message::user("Run offline")).await.unwrap();
    store.finish_attempt(&lease).unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(
        |m| matches!(m, Message::ToolResult(r) if r.tool_call_id == "first" && !r.outcome_unknown)
    ));
    drop(requests);
    let before = store.snapshot(&id).unwrap();
    assert_eq!(before.state, SessionState::Idle);
    assert!(before.unresolved_calls.is_empty());
    assert_eq!(
        before
            .messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResult(_)))
            .count(),
        1
    );
    drop(agent);
    drop(store);
    let store = database.open();
    assert_eq!(store.snapshot(&id).unwrap(), before);
}

// An unknown first result must not prevent later declared calls from recording
// nonexecution errors, nor prevent the Agent's final error turn from settling.
#[tokio::test]
async fn unknown_sequential_tool_stops_execution_and_settles_remaining_declared_calls() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("unknown event flow").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    let (agent, provider, executions) = build_agent(&store, &lease, true);
    agent.prompt(Message::user("Run offline")).await.unwrap();
    store.finish_attempt(&lease).unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let snapshot = store.snapshot(&id).unwrap();
    assert_eq!(snapshot.state, SessionState::NeedsReconciliation);
    assert!(snapshot.attempt.is_none());
    assert_eq!(snapshot.unresolved_calls.len(), 1);
    let results = snapshot
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .map(|r| r.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(results[0].outcome_unknown);
    assert!(results[1].is_error);
    assert!(!results[1].outcome_unknown);
    assert!(results[1].duration_ms.is_none());
    assert!(store.events(&id, 0, 100).unwrap().iter().any(|r| matches!(&r.event, CoreEvent::Agent { event } if matches!(event.as_ref(), cayw_agent::AgentEvent::AgentEnd { .. }))));
}
