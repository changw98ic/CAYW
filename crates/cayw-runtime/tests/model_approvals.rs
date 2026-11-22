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
    decision_delay: Duration,
    wait_before_deciding: bool,
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
        let action_step = step - usize::from(self.wait_before_deciding && step >= 2);
        let args = match action_step {
            0 => Some(json!({
                "action": "exec",
                "script": self.script,
                "yield_time_ms": 30000,
                "max_output_tokens": 8192
            })),
            1 => {
                let result = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        Message::ToolResult(result) => Some(result),
                        _ => None,
                    })
                    .unwrap();
                assert!(result.content.iter().any(|content| matches!(content,
                    Content::Text { text } if text.contains("approval.pending") && text.contains("approval.decide")
                )), "a waiting tool result must tell the model how to decide pending approvals");
                let page = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        Message::ToolResult(result) => {
                            result.structured_content.as_ref().and_then(|value| {
                                serde_json::from_value::<CellPage>(value.clone()).ok()
                            })
                        }
                        _ => None,
                    })
                    .expect("original cell page");
                assert!(page.vm_alive, "original call must still await permission");
                if self.wait_before_deciding && step == 1 {
                    // No yield deadline and no unread output to wake this wait.
                    Some(
                        json!({"action":"wait", "cell_id":page.cell_id, "cursor":page.next_cursor}),
                    )
                } else {
                    state.1 = Some(page);
                    let decision = format!(
                        "cayw.call('approval.decide', {{approval_id=pending[1].id, decision='{}'}})",
                        self.choice
                    );
                    let statement = if self.choice == "deny" {
                        format!("text({decision})")
                    } else {
                        decision
                    };
                    Some(json!({
                        "action": "exec",
                        "script": format!(
                            "local pending = cayw.call('approval.pending', {{}}); assert(#pending == 1); {statement}"
                        ),
                        "max_output_tokens": 8192
                    }))
                }
            }
            2 if self.choice == "deny" => {
                let page = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        Message::ToolResult(result) if result.tool_call_id == "fixture-1" => Some(
                            serde_json::from_value::<CellPage>(
                                result.structured_content.clone().unwrap(),
                            )
                            .unwrap(),
                        ),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(
                    page.outputs.len(),
                    1,
                    "the decision must already be visible"
                );
                Some(json!({"action":"wait", "cell_id":page.cell_id, "cursor":page.next_cursor}))
            }
            step if step == 2 + usize::from(self.choice == "deny") => {
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
        let delay = if step == 1 {
            self.decision_delay
        } else {
            Duration::ZERO
        };
        Box::pin(futures::stream::once(async move {
            tokio::time::sleep(delay).await;
            AssistantEvent::Done { message }
        }))
    }
}

async fn run(
    choice: &'static str,
    request_authority: bool,
    decision_delay: Duration,
    wait_before_deciding: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = if request_authority {
        "local request=cayw.call('capability.request',{purpose='write the result',gap='write requires request authority',operation_id='write-result',capability='fs.write',arguments={path='result.txt',content='model approved'}}); local decision=cayw.call('capability.decide',{request_id=request.id,decision='allow',reason='perform the requested write',lifetime_ms=10000}); assert(decision.evaluator == 'model-auto'); return cayw.call('fs.write',{path='result.txt',content='model approved'})"
    } else {
        "return cayw.call('fs.write',{path='result.txt',content='model approved'})"
    };
    let mut config = RuntimeConfig {
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
    if !decision_delay.is_zero() {
        config.host.call_timeout = Some(Duration::from_secs(2));
    }
    let provider = Arc::new(Fixture {
        script: script.into(),
        choice,
        decision_delay,
        wait_before_deciding,
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
        let records = runtime.events(&session, 0, 1000).unwrap();
        let original = records
            .iter()
            .find_map(|record| match &record.event {
                cayw_core::CoreEvent::CapabilitySettled { call }
                    if call.name == "approval.decide" =>
                {
                    match call.outcome.as_ref().unwrap() {
                        cayw_core::CapabilityOutcome::Completed { result } => Some(result),
                        outcome => panic!("decision did not commit: {outcome:?}"),
                    }
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(original["decision"]["decision"], "deny");
        assert_eq!(original["decision"]["actor"], "model-auto");
        let view = runtime.get(&session).unwrap();
        let page_for = |call_id| {
            view.messages
                .iter()
                .find_map(|message| match message {
                    Message::ToolResult(result) if result.tool_call_id == call_id => Some((
                        result.is_error,
                        serde_json::from_value::<CellPage>(
                            result.structured_content.clone().unwrap(),
                        )
                        .unwrap(),
                    )),
                    _ => None,
                })
                .unwrap()
        };
        let (_, decision) = page_for("fixture-1");
        let (is_error, terminal) = page_for("fixture-2");
        assert!(is_error);
        assert_eq!(terminal.cell_id, decision.cell_id);
        assert_eq!(terminal.status, cayw_cells::CellStatus::Failed);
        assert_eq!(
            terminal.error.as_deref(),
            Some("Capability: invalid request: invalid type: map, expected a string")
        );
        assert!(terminal.outputs.is_empty());
        assert_eq!(decision.outputs.len(), 1);
        assert_eq!(
            &decision.outputs[0].output,
            &cayw_cells::CellOutput::Json {
                value: original.clone()
            }
        );
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
        let events =
            serde_json::to_string(&runtime.request_events(&task.id, 0, 64).unwrap()).unwrap();
        assert!(events.contains("model-auto"));
        assert!(events.contains("evaluation_started"));
    }
}

#[tokio::test]
async fn model_allows_original_waiting_cell_without_human_or_approval_recursion() {
    run("allow_once", false, Duration::ZERO, false).await;
}

#[tokio::test]
async fn repeated_wait_returns_control_to_the_model_until_it_decides() {
    run("allow_once", false, Duration::ZERO, true).await;
}

#[tokio::test]
async fn denied_approval_receipt_survives_a_later_output_type_error() {
    run("deny", false, Duration::ZERO, false).await;
}

#[tokio::test]
async fn model_decides_request_authority_and_upstream_approval_separately() {
    run("allow_once", true, Duration::ZERO, false).await;
}

#[test]
fn sdk_defaults_to_explicit_approval() {
    assert_eq!(
        RuntimeConfig::default().approval_mode,
        ApprovalMode::Explicit
    );
}

#[tokio::test]
async fn model_decision_after_execution_budget_still_runs_original_call_once() {
    run("allow_once", false, Duration::from_secs(3), false).await;
}
