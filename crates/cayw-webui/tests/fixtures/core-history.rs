//! SDK-only fixture for the browser's delayed projection regression.
//! Compile with the exact rlibs listed by cargo build --message-format=json.
//! It appends real Core events in a fresh, offline validation database.
use cayw_agent::AgentEvent;
use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
use cayw_core::{ReconciledOutcome, Reconciliation, SessionStore};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let store = SessionStore::open(&args[2])?;
    let id = &args[3];
    match args[1].as_str() {
        "unknown" => {
            let lease = store.begin_attempt(id, "webui-regression-host")?;
            store.append_agent_event(&lease, &AgentEvent::AgentStart)?;
            store.append_agent_event(&lease, &AgentEvent::TurnStart)?;
            let call = ToolCall::new(
                "browser-call-c1",
                "execute_code",
                json!({"script":"return true"}),
            );
            let mut assistant = AssistantMessage::new(&Model::new("fixture", "core-history"));
            assistant.stop_reason = StopReason::ToolUse;
            assistant.content.push(Content::ToolCall(call.clone()));
            let message = Message::Assistant(assistant);
            store.append_agent_event(
                &lease,
                &AgentEvent::MessageStart {
                    message: message.clone(),
                },
            )?;
            store.append_agent_event(&lease, &AgentEvent::MessageEnd { message })?;
            store.append_agent_event(
                &lease,
                &AgentEvent::ToolExecutionStart { call: call.clone() },
            )?;
            store.mark_tool_started(&lease, &call.id)?;
            // No executor was invoked. Recovery records an unknown boundary;
            // the explicit host reconciliation below supplies that evidence.
            store.recover(id)?;
        }
        "reconcile" => {
            store.reconcile(
                id,
                Reconciliation {
                    call_id: "browser-call-c1".into(),
                    actor: "webui-regression-host".into(),
                    evidence:
                        "Fixture crossed the durable start marker but never invoked an executor"
                            .into(),
                    outcome: ReconciledOutcome::NotExecuted {
                        reason: "No executor is present in this SDK fixture".into(),
                    },
                },
            )?;
        }
        _ => return Err("expected unknown or reconcile".into()),
    }
    println!("{}", serde_json::to_string(&store.snapshot(id)?)?);
    Ok(())
}
