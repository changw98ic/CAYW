//! Run with: cargo run -p cayw-core --example headless -- <new-or-existing-db-file>
//! Uses a local fixture provider and no model service or credentials.
use cayw_agent::{Agent, AgentEvent};
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_core::{SessionState, SessionStore};
use std::sync::Arc;

struct OfflineProvider;
impl Provider for OfflineProvider {
    fn stream(&self, request: ModelRequest, _cancel: CancellationToken) -> AssistantStream {
        let mut assistant = AssistantMessage::new(&request.model);
        assistant.content = vec![Content::text("This response came from an offline fixture.")];
        assistant.stop_reason = StopReason::Stop;
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message: assistant,
        }]))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("Provide a SQLite database file path")?;
    let store = SessionStore::open(&path)?;
    let session = store.create_session("Headless Rust API example")?;
    let lease = store.begin_attempt(&session.id, "offline-example")?;
    let agent = Agent::new(Arc::new(OfflineProvider), Model::new("fixture", "offline"));
    let listener_store = store.clone();
    let listener_lease = lease.clone();
    agent.subscribe(move |event: AgentEvent, _| {
        let store = listener_store.clone();
        let lease = listener_lease.clone();
        async move {
            // A production async host can move this synchronous write to a blocking task.
            store
                .append_agent_event(&lease, &event)
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    });
    if let Err(error) = agent
        .prompt(Message::user("Demonstrate a durable headless run"))
        .await
    {
        store.recover(&session.id)?;
        return Err(error.into());
    }
    store.finish_attempt(&lease)?;
    drop(store);
    let reopened = SessionStore::open(path)?;
    let persisted = reopened.snapshot(&session.id)?;
    assert_eq!(persisted.state, SessionState::Idle);
    assert!(
        persisted
            .messages
            .iter()
            .any(|message| matches!(message, Message::Assistant(_)))
    );
    println!("{}", serde_json::to_string_pretty(&persisted)?);
    Ok(())
}
