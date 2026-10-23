//! Synthetic replies try to declare completion while real Cells still owe work.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, Usage, UsageStatus,
};
use cayw_cells::CellPage;
use cayw_core::{CapabilityOutcome, CoreEvent};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::json;
use std::sync::{Arc, Mutex};

struct EarlyFinal(&'static str);
impl Provider for EarlyFinal {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        // Fixed synthetic counters keep this test focused on cell completion.
        message.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        if !request
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)))
        {
            message.stop_reason = StopReason::ToolUse;
            message.content.push(Content::ToolCall(ToolCall::new(
                "launch",
                "execute_code",
                json!({"action":"exec","script":self.0,"yield_time_ms":30000,"max_output_tokens":1024}),
            )));
        } else {
            message.stop_reason = StopReason::Stop;
            message.content.push(Content::text("All done"));
        }
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

async fn rejects_early_final(script: &'static str, reason: &str) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let runtime = Runtime::open(
        dir.path().join("sessions.db"),
        workspace,
        Arc::new(EarlyFinal(script)),
        Model::new("fixture", "early-final"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let id = runtime.create(None).unwrap().id;
    let result = runtime.prompt(&id, "run", CancellationToken::new()).await;
    assert!(
        result.is_err(),
        "an early final response was accepted: {result:?}"
    );
    assert!(result.unwrap_err().to_string().contains(reason));
    let tasks = runtime.tasks(&id).unwrap();
    assert_eq!(tasks[0].state, TaskState::Failed);
    let view = runtime.get(&id).unwrap();
    assert!(view.unresolved_calls.is_empty());
    assert!(view.unresolved_capabilities.is_empty());
    assert!(
        view.executions
            .iter()
            .all(|execution| execution.terminal.is_some())
    );
}

#[tokio::test]
async fn final_response_cannot_complete_a_yielded_vm() {
    rejects_early_final(
        "yield_control(); text('not yet')",
        "cell still requires wait",
    )
    .await;
}

#[tokio::test]
async fn final_response_cannot_discard_unread_output() {
    rejects_early_final(
        "text(string.rep('x', 2000)); return 7",
        "unread cell output",
    )
    .await;
}

#[tokio::test]
async fn nonzero_exit_cannot_be_reported_as_a_completed_task() {
    rejects_early_final("exit(7)", "cell exited with code 7").await;
}

struct Consume;
impl Provider for Consume {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "text('first'); yield_control(); text('second'); exit(0)",
            &format!("consume-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

#[tokio::test]
async fn explicitly_waited_and_consumed_zero_exit_can_complete() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime::open(
        dir.path().join("sessions.db"),
        dir.path(),
        Arc::new(Consume),
        Model::new("fixture", "consume"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let id = runtime.create(None).unwrap().id;
    runtime
        .prompt(&id, "run", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Completed);
}

#[derive(Clone, Copy)]
enum ReadMode {
    SkipFirst,
    Replay,
}

struct Paged {
    mode: ReadMode,
    pages: Mutex<Vec<CellPage>>,
}

impl Provider for Paged {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        // Fixed synthetic counters keep this test focused on cell completion.
        message.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        let mut pages = self.pages.lock().unwrap();
        let arguments = match request.messages.last() {
            Some(Message::ToolResult(result)) => {
                let page: CellPage = serde_json::from_value(
                    result
                        .structured_content
                        .clone()
                        .expect("a genuine cell page"),
                )
                .unwrap();
                pages.push(page.clone());
                if page.vm_alive || !page.status.is_terminal() {
                    Some(json!({
                        "action":"wait", "cell_id":page.cell_id, "cursor":0,
                        "continue_execution":false, "yield_time_ms":30000,
                        "max_output_tokens":1024,
                    }))
                } else {
                    let cursor = match self.mode {
                        ReadMode::SkipFirst if page.cursor == 0 => Some(1),
                        ReadMode::SkipFirst => None,
                        ReadMode::Replay => {
                            let full_pages =
                                pages.iter().filter(|page| !page.outputs.is_empty()).count();
                            (full_pages < 2).then_some(0)
                        }
                    };
                    cursor.map(|cursor| {
                        json!({
                            "action":"wait", "cell_id":page.cell_id, "cursor":cursor,
                            "continue_execution":false, "yield_time_ms":30000,
                            "max_output_tokens":8192,
                        })
                    })
                }
            }
            _ => Some(json!({
                "action":"exec",
                "script":"cayw.call('fs.write',{path='once.txt',content='once'}); text(string.rep('x',2000)); text('tail'); return 'return'",
                "yield_time_ms":30000, "max_output_tokens":1024,
            })),
        };
        if let Some(arguments) = arguments {
            message.stop_reason = StopReason::ToolUse;
            message.content.push(Content::ToolCall(ToolCall::new(
                format!("page-{}", pages.len()),
                "execute_code",
                arguments,
            )));
        } else {
            message.stop_reason = StopReason::Stop;
            message.content.push(Content::text("All pages consumed"));
        }
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

async fn paged_run(mode: ReadMode) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let provider = Arc::new(Paged {
        mode,
        pages: Mutex::default(),
    });
    let arguments = json!({"path":"once.txt","content":"once"});
    let runtime = Runtime::open(
        dir.path().join("sessions.db"),
        &workspace,
        provider.clone(),
        Model::new("fixture", "paged"),
        RuntimeConfig {
            permissions: vec![ScopedPermission::exact("fs.write", arguments.clone())],
            ..Default::default()
        },
    )
    .unwrap();
    let id = runtime.create(None).unwrap().id;
    let result = runtime.prompt(&id, "run", CancellationToken::new()).await;
    match mode {
        ReadMode::SkipFirst => {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("unread cell output")
            );
            assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Failed);
        }
        ReadMode::Replay => {
            result.unwrap();
            assert_eq!(runtime.tasks(&id).unwrap()[0].state, TaskState::Completed);
        }
    }
    assert_eq!(std::fs::read(workspace.join("once.txt")).unwrap(), b"once");
    let events = runtime.events(&id, 0, 1000).unwrap();
    let started = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilityStarted { call } if call.name == "fs.write" => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    let settled = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "fs.write" => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        started.len(),
        1,
        "waiting must not dispatch the write again"
    );
    assert_eq!(settled.len(), 1);
    assert_eq!(started[0].key, settled[0].key);
    assert_eq!(settled[0].arguments, arguments);
    assert!(matches!(
        settled[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    let view = runtime.get(&id).unwrap();
    assert_eq!(
        view.executions.len(),
        1,
        "waiting must not create another execution"
    );
    assert!(view.unresolved_calls.is_empty());
    assert!(view.unresolved_capabilities.is_empty());
    let pages = provider.pages.lock().unwrap();
    let first = &pages[0];
    assert!(first.outputs.is_empty());
    assert_eq!(first.cursor, 0);
    assert_eq!(first.next_cursor, 0);
    assert!(first.required_budget.unwrap() > 1024);
    assert!(pages.iter().all(|page| page.cell_id == first.cell_id));
    match mode {
        ReadMode::SkipFirst => {
            let last = pages.last().unwrap();
            assert_eq!(last.cursor, 1);
            assert_eq!(last.outputs.len(), 2);
            assert_eq!(last.next_cursor, 3);
            assert!(!last.has_more);
            assert!(
                !pages
                    .iter()
                    .any(|page| page.outputs.iter().any(|entry| entry.cursor == 1))
            );
        }
        ReadMode::Replay => {
            let full = pages
                .iter()
                .filter(|page| !page.outputs.is_empty())
                .collect::<Vec<_>>();
            assert_eq!(full.len(), 2);
            assert_eq!(full[0].outputs, full[1].outputs);
            assert_eq!(full[0].outputs.len(), 3);
            assert_eq!(full[0].cursor, 0);
            assert_eq!(full[0].next_cursor, 3);
            assert_eq!(full[1].next_cursor, 3);
            for (index, entry) in full[0].outputs.iter().enumerate() {
                assert_eq!(entry.cursor, index as u64 + 1);
            }
        }
    }
}

#[tokio::test]
async fn skipping_an_oversized_first_item_cannot_hide_unread_output() {
    paged_run(ReadMode::SkipFirst).await;
}

#[tokio::test]
async fn a_larger_page_and_repeated_wait_preserve_output_without_replaying_effects() {
    paged_run(ReadMode::Replay).await;
}
