//! Synthetic model replies; numeric preparation, Luau, file writes and storage are real.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall,
};
use cayw_core::{CoreEvent, SessionState};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct NumericArguments(AtomicUsize, bool);
impl Provider for NumericArguments {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        let turn = self.0.fetch_add(1, Ordering::SeqCst);
        message.stop_reason = StopReason::ToolUse;
        match turn {
            0 => message.content.push(Content::ToolCall(ToolCall::new(
                "launch", "execute_code", json!({
                    "action":"exec",
                    "script":"yield_control(); return cayw.call('fs.write', {path='numeric.txt', content='converted'})",
                    "yield_time_ms":"30000.0", "max_output_tokens":"8192"
                }),
            ))),
            1 => {
                let previous = request.messages.iter().rev().find_map(|message| match message {
                    Message::ToolResult(result) => Some(result),
                    _ => None,
                }).unwrap();
                assert!(!previous.is_error, "launch failed: {previous:?}");
                let page = previous.structured_content.as_ref().unwrap();
                assert_eq!(page["status"], "yielded");
                message.content.push(Content::ToolCall(ToolCall::new(
                    "resume", "execute_code", json!({
                        "action":"wait", "cell_id":page["cell_id"], "cursor":"0",
                        "yield_time_ms":"3e4", "max_output_tokens":"8192.0",
                        "continue_execution":true
                    }),
                )));
            }
            2 => {
                let previous = request.messages.iter().rev().find_map(|message| match message {
                    Message::ToolResult(result) => Some(result),
                    _ => None,
                }).unwrap();
                assert!(!previous.is_error, "wait failed: {previous:?}");
                assert_eq!(previous.structured_content.as_ref().unwrap()["status"], "completed");
                message.stop_reason = StopReason::Stop;
                message.content.push(Content::text("converted"));
            }
            _ => panic!("numeric conversion must not require a model repair turn"),
        }
        if self.1 {
            for part in &mut message.content {
                if let Content::ToolCall(call) = part {
                    for field in ["cursor", "yield_time_ms", "max_output_tokens"] {
                        if let Some(value) = call.arguments.get_mut(field)
                            && let Some(text) = value.as_str()
                        {
                            *value = serde_json::from_str(text).unwrap();
                        }
                    }
                }
            }
        }
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

#[tokio::test]
async fn numeric_strings_execute_and_wait_without_changing_original_history() {
    execute_and_wait(false).await;
}

#[tokio::test]
async fn integral_json_numbers_execute_and_wait_without_changing_original_history() {
    execute_and_wait(true).await;
}

async fn execute_and_wait(native_numbers: bool) {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let provider = Arc::new(NumericArguments(AtomicUsize::new(0), native_numbers));
    let settings = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"numeric.txt","content":"converted"}),
        )],
        ..Default::default()
    };
    let open = || {
        Runtime::open(
            directory.path().join("session.db"),
            &workspace,
            provider.clone(),
            Model::new("fixture", "numeric-arguments"),
            settings.clone(),
        )
        .unwrap()
    };
    let runtime = open();
    let id = runtime.create(None).unwrap().id;
    let completed = runtime
        .prompt(&id, "resume the cell and write", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.join("numeric.txt")).unwrap(),
        "converted"
    );
    assert_eq!(completed.state, SessionState::Idle);
    assert!(completed.unresolved_calls.is_empty());
    assert!(completed.unresolved_capabilities.is_empty());
    assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Completed);
    assert_eq!(provider.0.load(Ordering::SeqCst), 3);
    let raw_launch = completed
        .messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(message) => message.content.iter().find_map(|part| match part {
                Content::ToolCall(call) if call.id == "launch" => Some(&call.arguments),
                _ => None,
            }),
            _ => None,
        })
        .unwrap();
    if native_numbers {
        assert!(raw_launch["yield_time_ms"].is_f64());
        assert_eq!(raw_launch["yield_time_ms"], json!(30000.0));
    } else {
        assert_eq!(raw_launch["yield_time_ms"], "30000.0");
        assert_eq!(raw_launch["max_output_tokens"], "8192");
    }
    let raw_wait = completed
        .messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(message) => message.content.iter().find_map(|part| match part {
                Content::ToolCall(call) if call.id == "resume" => Some(call.arguments.clone()),
                _ => None,
            }),
            _ => None,
        })
        .unwrap();
    if native_numbers {
        assert_eq!(raw_wait["cursor"], json!(0));
        assert!(raw_wait["max_output_tokens"].is_f64());
        assert_eq!(raw_wait["max_output_tokens"], json!(8192.0));
    } else {
        assert_eq!(raw_wait["cursor"], "0");
        assert_eq!(raw_wait["max_output_tokens"], "8192.0");
    }
    let events = runtime.events(&id, 0, 1000).unwrap();
    let binding = events
        .iter()
        .find_map(|record| match &record.event {
            CoreEvent::ExecutionWaitRegistered { binding } => Some(binding),
            _ => None,
        })
        .unwrap();
    assert_eq!(binding.arguments, raw_wait);
    assert_eq!(binding.call_id, "resume");
    drop(runtime);
    let reopened = open();
    assert_eq!(reopened.get(&id).unwrap(), completed);
    assert_eq!(provider.0.load(Ordering::SeqCst), 3);
}
