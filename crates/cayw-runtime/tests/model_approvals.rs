//! Offline model fixtures decide through CodeMode; no human approval is injected.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, Usage, UsageStatus,
};
use cayw_cells::CellPage;
use cayw_requests::OperationScope;
use cayw_runtime::{ApprovalMode, RequestPolicy, Runtime, RuntimeConfig};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Fixture {
    script: String,
    choice: &'static str,
    state: Mutex<(usize, Option<CellPage>)>,
}

impl Provider for Fixture {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut state = self.state.lock().unwrap();
        let step = state.0;
        state.0 += 1;
        let mut message = AssistantMessage::new(&request.model);
        message.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        let args = match step {
            0 => Some(json!({
                "action": "exec",
                "script": self.script,
                "yield_time_ms": 1000,
                "max_output_tokens": 8192
            })),
            1 => {
                let page = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        Message::ToolResult(result) => result
                            .structured_content
                            .as_ref()
                            .and_then(|value| serde_json::from_value::<CellPage>(value.clone()).ok()),
                        _ => None,
                    })
                    .expect("original cell page");
                assert!(page.vm_alive, "original call must still await permission");
                state.1 = Some(page);
                Some(json!({
                    "action": "exec",
                    "script": format!(
                        "local pending = cayw.call('approval.pending', {{}}); assert(#pending == 1); return cayw.call('approval.decide', {{approval_id=pending[1].id, decision='{}'}})",
                        self.choice
                    ),
                    "yield_time_ms": 1000,
                    "max_output_tokens": 8192
                }))
            }
            2 => {
                let page = state.1.as_ref().unwrap();
                Some(json!({
                    "action": "wait",
                    "cell_id": page.cell_id,
                    "cursor": page.next_cursor,
                    "yield_time_ms": 1000,
                    "max_output_tokens": 8192
                }))
            }
            _ => None,
        };
        if let Some(args) = args {
            message.content = vec![Content::ToolCall(ToolCall::new(
                format!("fixture-{step}"),
                "execute_code",
                args,
            ))];
            message.stop_reason = StopReason::ToolUse;
        } else {
            message.content = vec![Content::text("complete")];
            message.stop_reason = StopReason::Stop;
        }
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

async fn run(choice: &'static str, request_authority: bool) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = if request_authority {
        "local request=cayw.call('capability.request',{purpose='write the result',gap='write requires request authority',operation_id='write-result',capability='fs.write',arguments={path='result.txt',content='model approved'}}); local decision=cayw.call('capability.decide',{request_id=request.id,decision='allow',reason='perform the requested write',lifetime_ms=10000}); assert(decision.evaluator == 'model-auto'); return cayw.call('fs.write',{path='result.txt',content='model approved'})"
    } else {
        "return cayw.call('fs.write',{path='result.txt',content='model approved'})"
    };
    let config = RuntimeConfig {
        approval_mode: ApprovalMode::ModelAuto,
        max_run_time: Some(Duration::from_secs(15)),
        request_policy: request_authority.then(|| RequestPolicy {
            permission_policy_id: "model-auto-fixture".into(),
            operations: vec![OperationScope::exact(
                "write-result",
                "fs.write",
                json!({"path": "result.txt", "content": "model approved"}),
                "write-policy",
            )],
        }),
        ..Default::default()
    };
    let provider = Arc::new(Fixture {
        script: script.into(),
        choice,
        state: Mutex::new((0, None)),
    });
    let runtime = Runtime::open(
        root.path().join("runtime.db"),
        &workspace,
        provider,
        Model::new("fixture", "offline"),
        config,
    )
    .unwrap();
    let session = runtime.create(None).unwrap().id;
    tokio::time::timeout(
        Duration::from_secs(20),
        runtime.prompt(&session, "write result", CancellationToken::new()),
    )
    .await
    .expect("approval tools cannot wait for their own approval")
    .unwrap();
    assert!(runtime.get(&session).unwrap().pending_approvals.is_empty());
    if choice == "deny" {
        assert!(!workspace.join("result.txt").exists());
    } else {
        assert_eq!(
            std::fs::read_to_string(workspace.join("result.txt")).unwrap(),
            "model approved"
        );
    }
    let events = serde_json::to_string(&runtime.events(&session, 0, 1000).unwrap()).unwrap();
    assert!(events.contains("model-auto"));
    assert!(events.contains("approval_scope_decided"));
    assert!(!events.contains("rpc-user"));
    if request_authority {
        let task = runtime.tasks(&session).unwrap().remove(0);
        let events = serde_json::to_string(&runtime.request_events(&task.id, 0, 64).unwrap()).unwrap();
        assert!(events.contains("model-auto"));
        assert!(events.contains("evaluation_started"));
    }
}

#[tokio::test]
async fn model_allows_original_waiting_cell_without_human_or_approval_recursion() {
    run("allow_once", false).await;
}

#[tokio::test]
async fn model_denies_original_waiting_cell_without_effect() {
    run("deny", false).await;
}

#[tokio::test]
async fn model_decides_request_authority_and_upstream_approval_separately() {
    run("allow_once", true).await;
}

#[test]
fn sdk_defaults_to_explicit_approval() {
    assert_eq!(RuntimeConfig::default().approval_mode, ApprovalMode::Explicit);
}
