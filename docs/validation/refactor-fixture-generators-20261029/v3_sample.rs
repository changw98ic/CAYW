//! Real Core v3 storage sample.
//!
//! `generate_v3_sample` creates a complete session through the public
//! `SessionStore` + `Agent` API and writes it under `tests/fixtures`. Run it on
//! the pre-move baseline (see the workflow report) and check the resulting file
//! in. The reader copies the sample, opens it, reads the event log and calls the
//! explicit recovery entry. Opening a v3 sample must not migrate or rewrite it.
//!
//! `EventRecord.timestamp` (Unix milliseconds) is the persisted time field; the
//! reader asserts it is populated and monotonic instead of comparing wall-clock
//! values.

use cayw_agent::{
    Agent, AgentHooks, AgentTool, BeforeToolDecision, ExecutionMode, ToolContext, ToolHookContext,
    ToolResult, async_trait,
};
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, ToolDefinition,
};
use cayw_core::{AttemptLease, CoreEvent, SessionState, SessionStore};
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

fn sample_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3-sample.sqlite")
}

struct OfflineProvider {
    requests: Mutex<usize>,
}

impl Provider for OfflineProvider {
    fn stream(&self, request: ModelRequest, _cancel: CancellationToken) -> AssistantStream {
        let mut requests = self.requests.lock().unwrap();
        let mut message = AssistantMessage::new(&request.model);
        if *requests == 0 {
            message.stop_reason = StopReason::ToolUse;
            message.content = vec![Content::ToolCall(ToolCall::new(
                "sample-call-1",
                "echo",
                json!({"text": "hi"}),
            ))];
        } else {
            message.stop_reason = StopReason::Stop;
            message.content = vec![Content::text("Finished the v3 sample")];
        }
        *requests += 1;
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

struct EchoTool;

#[async_trait]
impl AgentTool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Echo the supplied text".into(),
            parameters: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
        }
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        Ok(ToolResult::text(
            context.arguments["text"].as_str().unwrap_or_default().to_owned(),
        ))
    }
}

struct StartHook {
    store: SessionStore,
    lease: AttemptLease,
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
            .map_err(|error| error.to_string())?;
        Ok(BeforeToolDecision::default())
    }
}

#[tokio::test]
#[ignore = "generator: run against pre-move baseline, then check in the sample"]
async fn generate_v3_sample() {
    let path = sample_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Remove any previous file and its WAL sidecars so the shape is reproducible.
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let store = SessionStore::open(&path).unwrap();
    let id = store.create_session("v3 sample session").unwrap().id;
    let lease = store.begin_attempt(&id, "sample-worker").unwrap();
    let agent = Agent::new(
        Arc::new(OfflineProvider {
            requests: Mutex::new(0),
        }),
        Model::new("fixture", "offline"),
    );
    agent.set_tools(vec![Arc::new(EchoTool)]).unwrap();
    agent.set_execution_mode(ExecutionMode::Sequential).unwrap();
    agent
        .set_hooks(Arc::new(StartHook {
            store: store.clone(),
            lease: lease.clone(),
        }))
        .unwrap();
    let recording = store.clone();
    let recording_lease = lease.clone();
    agent.subscribe(move |event, _| {
        let store = recording.clone();
        let lease = recording_lease.clone();
        async move {
            store
                .append_agent_event(&lease, &event)
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    });
    agent
        .prompt(Message::user("Run the v3 sample"))
        .await
        .unwrap();
    store.finish_attempt(&lease).unwrap();
    assert_eq!(store.snapshot(&id).unwrap().state, SessionState::Idle);
    drop(agent);
    // Dropping the last connection checkpoints WAL into the sample file.
    drop(store);
    assert!(path.exists());
}

#[test]
fn real_v3_sample_opens_reads_and_recovers() {
    let source = sample_path();
    assert!(
        source.exists(),
        "v3 sample is missing; run generate_v3_sample against pre-move baseline code"
    );
    let directory = std::env::temp_dir().join(format!("cayw-v3-sample-{}", Uuid::new_v4()));
    fs::create_dir_all(&directory).unwrap();
    let copy = directory.join("sample.sqlite");
    fs::copy(&source, &copy).unwrap();
    let store = SessionStore::open(&copy).unwrap();
    let sessions = store.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    let id = sessions[0].id.clone();
    let snapshot = store.snapshot(&id).unwrap();
    assert_eq!(snapshot.state, SessionState::Idle);
    let events = store.events(&id, 0, 1000).unwrap();
    assert!(events.len() >= 8, "sample should contain a complete log");
    assert!(events.iter().all(|record| record.protocol_version == 3));
    assert!(events.iter().all(|record| record.timestamp > 0));
    assert!(
        events
            .windows(2)
            .all(|window| window[0].timestamp <= window[1].timestamp)
    );
    assert!(
        events
            .iter()
            .any(|record| matches!(record.event, CoreEvent::SessionCreated { .. }))
    );
    assert!(
        events
            .iter()
            .any(|record| matches!(record.event, CoreEvent::AttemptStarted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|record| matches!(record.event, CoreEvent::Agent { .. }))
    );
    assert!(
        events
            .iter()
            .any(|record| matches!(record.event, CoreEvent::ToolStarted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|record| matches!(record.event, CoreEvent::AttemptFinished { .. }))
    );
    // A settled idle session is unchanged by the explicit recovery entry; a v3
    // sample is read, not migrated or rewritten.
    let recovered = store.recover(&id).unwrap();
    assert_eq!(recovered, snapshot);
    assert_eq!(store.events(&id, 0, 1000).unwrap().len(), events.len());
    drop(store);
    let _ = fs::remove_dir_all(directory);
}
