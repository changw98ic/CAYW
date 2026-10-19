use async_trait::async_trait;
use cayw_agent::{Agent, AgentTool, ToolResult, ai::*};
use cayw_codemode::{CapabilityError, CapabilityHost, CodeMode, CodeModeTool, Limits};
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Host(AtomicUsize);
#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "write".into(),
            description: "fixture write".into(),
            parameters: json!({"type":"object"}),
        }]
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(CapabilityError::unknown("lost write acknowledgement"))
    }
}

struct ModelFixture {
    calls: AtomicUsize,
    observed_tools: Mutex<Vec<String>>,
}
impl Provider for ModelFixture {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let turn = self.calls.fetch_add(1, Ordering::SeqCst);
        *self.observed_tools.lock().unwrap() = current_system(&request.messages)
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let mut message = AssistantMessage::new(&request.model);
        if turn == 0 {
            message.content = vec![Content::ToolCall(ToolCall::new(
                "code-1",
                "execute_code",
                json!({"script":"cayw.call('write', {})"}),
            ))];
            message.stop_reason = StopReason::ToolUse;
        } else {
            message.content = vec![Content::text("unexpected continuation")];
            message.stop_reason = StopReason::Stop;
        }
        stream::iter(vec![AssistantEvent::Done { message }]).boxed()
    }
}

// Break caught: turning unknown write effects into a normal error would allow another model turn.
#[tokio::test]
async fn agent_sees_only_execute_code_and_freezes_on_unknown_effects() {
    let host = Arc::new(Host(AtomicUsize::new(0)));
    let code = CodeMode::new(host.clone(), Limits::default()).unwrap();
    let tool = Arc::new(CodeModeTool::new(code));
    let fixture = Arc::new(ModelFixture {
        calls: AtomicUsize::new(0),
        observed_tools: Mutex::new(vec![]),
    });
    let agent = Agent::new(fixture.clone(), Model::new("fixture", "offline"));
    agent.set_tools(vec![tool]).unwrap();
    let run = agent.prompt(Message::user("run one write")).await.unwrap();
    let result = run
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(result.is_error);
    assert!(result.outcome_unknown);
    assert_eq!(
        result.structured_content.as_ref().unwrap()["report"]["calls"][0]["status"],
        "outcome_unknown"
    );
    assert!(agent.snapshot().needs_reconciliation);
    assert_eq!(host.0.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *fixture.observed_tools.lock().unwrap(),
        vec!["execute_code"]
    );
}

#[test]
fn execute_code_arguments_have_one_strict_script_field() {
    let tool = CodeModeTool::new(
        CodeMode::new(Arc::new(Host(AtomicUsize::new(0))), Limits::default()).unwrap(),
    );
    let definition = tool.definition();
    let validator = ArgumentValidator::new(&definition.parameters).unwrap();
    assert!(validator.validate(&json!({"script":"return 1"})).is_ok());
    assert!(validator.validate(&json!({"script":1})).is_err());
    assert!(
        validator
            .validate(&json!({"script":"return 1", "allow":true}))
            .is_err()
    );
    assert!(validator.validate(&json!({})).is_err());
    let _: ToolResult = ToolResult::default();
}
