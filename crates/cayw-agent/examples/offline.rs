use cayw_agent::{ai::*, *};
use futures::{StreamExt, stream};
use serde_json::json;
use std::sync::Arc;

struct OfflineModel;
impl Provider for OfflineModel {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut answer = AssistantMessage::new(&request.model);
        if let Some(Message::ToolResult(result)) = request.messages.last() {
            answer.content = vec![Content::text(format!(
                "2 + 3 = {}",
                content_text(&result.content)
            ))];
            answer.stop_reason = StopReason::Stop;
        } else {
            answer.content = vec![Content::ToolCall(ToolCall::new(
                "sum-1",
                "add",
                json!({"a":2,"b":3}),
            ))];
            answer.stop_reason = StopReason::ToolUse;
        }
        stream::iter([AssistantEvent::Done { message: answer }]).boxed()
    }
}

struct Add;
#[async_trait]
impl AgentTool for Add {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "add".into(),
            description: "Add two integers".into(),
            parameters: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false}),
        }
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        let a = context.arguments["a"].as_i64().ok_or("missing a")?;
        let b = context.arguments["b"].as_i64().ok_or("missing b")?;
        let sum = a.checked_add(b).ok_or("integer overflow")?;
        Ok(ToolResult {
            structured_content: Some(json!({"sum":sum})),
            ..ToolResult::text(sum.to_string())
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = Agent::new(Arc::new(OfflineModel), Model::new("offline", "scripted"));
    agent.set_tools(vec![Arc::new(Add)])?;
    agent.subscribe(|event, _| async move {
        match event {
            AgentEvent::ToolExecutionStart { call } => {
                println!("tool start: {} {}", call.name, call.arguments)
            }
            AgentEvent::ToolExecutionEnd { result, .. } => {
                println!("tool result: {}", content_text(&result.content))
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            } if message.stop_reason == StopReason::Stop => {
                println!("assistant: {}", content_text(&message.content))
            }
            _ => {}
        }
        Ok(())
    });
    let result = agent.prompt(Message::user("What is 2 + 3?")).await?;
    assert_eq!(result.last_message.stop_reason, StopReason::Stop);
    println!(
        "completed: {} messages, running={}",
        result.messages.len(),
        agent.snapshot().is_running
    );
    Ok(())
}
