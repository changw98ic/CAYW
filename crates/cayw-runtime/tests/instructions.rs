//! Synthetic provider; the durable Session, approvals and file capabilities are real.
use cayw_agent::AgentEvent;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, SystemMessage, current_system,
};
use cayw_core::{CapabilityOutcome, CoreEvent};
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, TaskState};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct RecordingProvider(Mutex<Vec<ModelRequest>>);
impl Provider for RecordingProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut requests = self.0.lock().unwrap();
        let call = format!("instruction-call-{}", requests.len());
        let message = cayw_runtime::fixture::script_response(
            &request,
            "local r=cayw.call('fs.read',{path='source.txt'}); return r",
            &call,
        );
        requests.push(request);
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn host_history(runtime: &Runtime, id: &str) {
    let store = runtime.store();
    let lease = store.begin_attempt(id, "synthetic-host").unwrap();
    let host = Message::System(SystemMessage {
        content: "Host policy: all file reads require an exact host decision.".into(),
        sections: [(
            "host-workspace".into(),
            Some("Only source.txt belongs to this task.".into()),
        )]
        .into(),
        ..Default::default()
    });
    let mut assistant = AssistantMessage::new(&Model::new("fixture", "instruction-test"));
    assistant.stop_reason = StopReason::Stop;
    assistant.content = vec![Content::text("Host policy installed.")];
    for event in [
        AgentEvent::AgentStart,
        AgentEvent::TurnStart,
        AgentEvent::MessageStart {
            message: host.clone(),
        },
        AgentEvent::MessageEnd { message: host },
        AgentEvent::MessageStart {
            message: Message::Assistant(assistant.clone()),
        },
        AgentEvent::MessageEnd {
            message: Message::Assistant(assistant.clone()),
        },
        AgentEvent::TurnEnd {
            message: assistant,
            tool_results: vec![],
        },
        AgentEvent::AgentEnd { messages: vec![] },
    ] {
        store.append_agent_event(&lease, &event).unwrap();
    }
    store.finish_attempt(&lease).unwrap();
}

// Exact instruction emitted by the sealed Issue 9 Runtime, not a substring match.
const LEGACY_RUNTIME_INSTRUCTION: &str = "Use execute_code with action=exec, script, yield_time_ms and max_output_tokens for bounded Luau cells. A timed reply can leave the VM running: use action=wait with its cell_id, next_cursor, yield_time_ms and max_output_tokens until status is terminal and has_more is false. wait resumes yield_control unless continue_execution=false. Use tool_search(query,limit) or ALL_TOOLS to discover names and toolDefinition(name) for needed schemas. Invoke capabilities with cayw.call(name, arguments). Helpers store/load retain versioned session values; text, image, audio and notify produce typed output. return a JSON value or cayw.emit(value). Discovery is not permission. Every exact invocation needs host authorization. Never approve your own request or replay uncertain effects. No ambient shell, network or filesystem APIs are available.";

#[tokio::test]
async fn legacy_runtime_content_is_canonicalized_without_rewriting_host_history() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("source.txt"), "synthetic-observed-nonce").unwrap();
    let provider = Arc::new(RecordingProvider::default());
    let open = || {
        Runtime::open(
            root.path().join("session.db"),
            &workspace,
            provider.clone(),
            Model::new("fixture", "instruction-test"),
            RuntimeConfig::default(),
        )
        .unwrap()
    };
    let runtime = open();
    let id = runtime.create(None).unwrap().id;
    host_history(&runtime, &id);
    let legacy = [
        Message::System(SystemMessage {
            content: LEGACY_RUNTIME_INSTRUCTION.into(),
            sections: [(
                "host-legacy-rule".into(),
                Some("Keep the original exact authorization.".into()),
            )]
            .into(),
            tools_removed: vec!["host-disabled-tool".into()],
            ..Default::default()
        }),
        Message::system(LEGACY_RUNTIME_INSTRUCTION),
        Message::system(
            "Use execute_code with action=exec only after the host has approved the exact call.",
        ),
        Message::user("Earlier host-controlled task."),
    ];
    let store = runtime.store();
    let lease = store.begin_attempt(&id, "synthetic-legacy-host").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    for message in legacy {
        store
            .append_agent_event(
                &lease,
                &AgentEvent::MessageStart {
                    message: message.clone(),
                },
            )
            .unwrap();
        store
            .append_agent_event(&lease, &AgentEvent::MessageEnd { message })
            .unwrap();
    }
    let mut assistant = AssistantMessage::new(&Model::new("fixture", "instruction-test"));
    assistant.stop_reason = StopReason::Stop;
    assistant.content = vec![Content::text("Earlier host-controlled task ended.")];
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageStart {
                message: Message::Assistant(assistant.clone()),
            },
        )
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::MessageEnd {
                message: Message::Assistant(assistant.clone()),
            },
        )
        .unwrap();
    store
        .append_agent_event(
            &lease,
            &AgentEvent::TurnEnd {
                message: assistant,
                tool_results: vec![],
            },
        )
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentEnd { messages: vec![] })
        .unwrap();
    store.finish_attempt(&lease).unwrap();
    let history = runtime.get(&id).unwrap().messages;
    let original_events = runtime.events(&id, 0, 1000).unwrap();
    drop(runtime);
    let runtime = open();
    let run = {
        let runtime = runtime.clone();
        let id = id.clone();
        tokio::spawn(async move {
            runtime
                .prompt(&id, "read after legacy reopen", CancellationToken::new())
                .await
        })
    };
    let approval = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(approval) = runtime.get(&id).unwrap().pending_approvals.first() {
                break approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime
        .approve(
            &id,
            &approval.id,
            ApprovalChoice::AllowOnce,
            "synthetic-host",
        )
        .unwrap();
    let view = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(view.messages.starts_with(&history));
    assert!(
        runtime
            .events(&id, 0, 1000)
            .unwrap()
            .starts_with(&original_events)
    );
    let requests = provider.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let system = current_system(&requests[0].messages);
    assert_eq!(
        system.prompt.matches(LEGACY_RUNTIME_INSTRUCTION).count(),
        1,
        "the exact legacy instruction and current named section must share one effective source"
    );
    assert!(
        system
            .prompt
            .contains("Host policy: all file reads require an exact host decision.")
    );
    assert!(
        system
            .prompt
            .contains("Only source.txt belongs to this task.")
    );
    assert!(
        system
            .prompt
            .contains("Keep the original exact authorization.")
    );
    assert!(system.prompt.contains(
        "Use execute_code with action=exec only after the host has approved the exact call."
    ));
    for (old, effective) in history.iter().zip(&requests[0].messages) {
        let mut expected = old.clone();
        if let Message::System(message) = &mut expected
            && message.content == LEGACY_RUNTIME_INSTRUCTION
        {
            message.content.clear();
            message.sections.insert(
                "cayw-runtime-codemode".into(),
                Some(LEGACY_RUNTIME_INSTRUCTION.into()),
            );
        }
        assert_eq!(
            &expected, effective,
            "only the exact legacy Runtime content may move into the reserved section"
        );
    }
    let Message::ToolResult(result) = requests[1].messages.last().unwrap() else {
        panic!("the original tool result must reach the provider");
    };
    assert!(cayw_ai::content_text(&result.content).contains("synthetic-observed-nonce"));
    assert!(requests[1].messages.iter().any(|message| matches!(message,
        Message::Assistant(reply) if reply.tool_calls().iter().any(|call| call.id == result.tool_call_id))));
}

#[tokio::test]
async fn second_user_turn_keeps_one_instruction_and_original_host_history() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("source.txt"), "synthetic-observed-nonce").unwrap();
    let provider = Arc::new(RecordingProvider::default());
    let runtime = Runtime::open(
        root.path().join("session.db"),
        &workspace,
        provider.clone(),
        Model::new("fixture", "instruction-test"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let id = runtime.create(None).unwrap().id;
    host_history(&runtime, &id);
    let host_prefix = runtime.get(&id).unwrap().messages;
    let mut first_history = Vec::new();
    for (round, text) in ["first observed read", "second observed read"]
        .into_iter()
        .enumerate()
    {
        let run = {
            let runtime = runtime.clone();
            let id = id.clone();
            tokio::spawn(async move { runtime.prompt(&id, text, CancellationToken::new()).await })
        };
        let approval = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(approval) = runtime.get(&id).unwrap().pending_approvals.first() {
                    break approval.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let events = runtime.events(&id, 0, 1000).unwrap();
        assert!(!events.iter().any(|record| matches!(
            &record.event,
            CoreEvent::CapabilityStarted { call } if call.task_id == approval.input.task_id
        )));
        runtime
            .approve(
                &id,
                &approval.id,
                ApprovalChoice::AllowOnce,
                "synthetic-host",
            )
            .unwrap();
        let view = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(view.messages.starts_with(&host_prefix));
        assert!(view.messages.starts_with(&first_history));
        let requests = provider.0.lock().unwrap();
        let request = &requests[round * 2];
        assert!(request.messages.starts_with(&host_prefix));
        assert!(request.messages.starts_with(&first_history));
        let system = current_system(&request.messages);
        assert!(
            system
                .prompt
                .contains("Host policy: all file reads require an exact host decision.")
        );
        assert!(
            system
                .prompt
                .contains("Only source.txt belongs to this task.")
        );
        assert_eq!(
            system
                .prompt
                .matches("Use execute_code with action=exec")
                .count(),
            1,
            "the second user turn must replace the runtime instruction instead of appending it"
        );
        assert!(system.tools.iter().any(|tool| tool.name == "execute_code"));
        let tool_request = &requests[round * 2 + 1];
        let Message::ToolResult(result) = tool_request.messages.last().unwrap() else {
            panic!("the actual file result must reach the provider");
        };
        assert!(cayw_ai::content_text(&result.content).contains("synthetic-observed-nonce"));
        assert!(tool_request.messages.iter().any(|message| matches!(
            message,
            Message::Assistant(reply) if reply.tool_calls().iter().any(|call| call.id == result.tool_call_id)
        )));
        drop(requests);
        first_history = view.messages;
    }
    let tasks = runtime.tasks(&id).unwrap();
    assert_eq!(tasks.len(), 2);
    assert!(tasks.iter().all(|task| task.state == TaskState::Completed));
    let events = runtime.events(&id, 0, 1000).unwrap();
    let settled = events.iter().filter(|record| matches!(
        &record.event,
        CoreEvent::CapabilitySettled { call } if matches!(call.outcome, Some(CapabilityOutcome::Completed { .. }))
    )).count();
    assert_eq!(settled, 2);
}
