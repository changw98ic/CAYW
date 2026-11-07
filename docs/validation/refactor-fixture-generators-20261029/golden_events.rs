//! Fixed historical JSON for the cross-component `AgentEvent` / `ToolResult`
//! contracts.
//!
//! Generate this fixture from pre-move baseline code and never regenerate it
//! after the relocation: it is the guard that proves moving the definitions kept
//! the serialized bytes unchanged. All timestamps are explicit so the golden
//! bytes do not depend on the wall clock.

use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{
    AssistantEvent, AssistantMessage, Content, Message, StopReason, ToolCall, ToolResultMessage,
    Usage, UsageStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{fs, path::PathBuf};

#[derive(Deserialize, Serialize)]
struct Golden {
    events: Vec<AgentEvent>,
    tool_results: Vec<ToolResult>,
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_events.golden.json")
}

fn usage() -> Usage {
    Usage {
        input: 11,
        output: 7,
        cache_read: 2,
        cache_write: 1,
        reasoning: 3,
        total_tokens: 21,
    }
}

fn assistant(content: Vec<Content>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        provider: "fixture".into(),
        model: "fixture-model".into(),
        response_id: Some("response-1".into()),
        response_model: Some("fixture-model-2026-01-01".into()),
        usage: usage(),
        usage_status: UsageStatus::Reported,
        usage_fields: Default::default(),
        provider_metadata: Default::default(),
        stop_reason,
        error_message: None,
        timestamp: 1_700_000_000_000,
        duration_ms: Some(5),
    }
}

fn fixtures() -> Golden {
    let call = ToolCall::new("call-1", "echo", json!({"text": "hi"}));
    let user = Message::User {
        content: vec![Content::text("hello")],
        timestamp: 1_700_000_000_000,
    };
    let reply = assistant(vec![Content::text("done")], StopReason::Stop);
    let streaming = assistant(vec![Content::text("hi")], StopReason::ToolUse);
    let completed = ToolResult {
        content: vec![Content::text("hi")],
        details: Some(json!({"trace": "abc123"})),
        structured_content: Some(json!({"value": 1})),
        usage: Some(usage()),
        is_error: false,
        terminate: true,
        outcome_unknown: false,
    };
    let unknown = ToolResult {
        content: vec![Content::text("effect may have occurred")],
        details: None,
        structured_content: None,
        usage: None,
        is_error: true,
        terminate: false,
        outcome_unknown: true,
    };
    let result_message = ToolResultMessage {
        tool_call_id: "call-1".into(),
        tool_name: "echo".into(),
        content: vec![Content::text("hi")],
        details: Some(json!({"trace": "abc123"})),
        structured_content: Some(json!({"value": 1})),
        usage: Some(usage()),
        is_error: false,
        outcome_unknown: false,
        timestamp: 1_700_000_000_001,
        duration_ms: Some(4),
    };
    let events = vec![
        AgentEvent::AgentStart,
        AgentEvent::AgentEnd {
            messages: vec![user.clone(), Message::Assistant(reply)],
        },
        AgentEvent::TurnStart,
        AgentEvent::TurnEnd {
            message: streaming.clone(),
            tool_results: vec![result_message],
        },
        AgentEvent::MessageStart { message: user },
        AgentEvent::MessageUpdate {
            message: streaming.clone(),
            event: Box::new(AssistantEvent::TextDelta {
                index: 0,
                delta: "hi".into(),
                partial: streaming.clone(),
            }),
        },
        AgentEvent::MessageEnd {
            message: Message::Assistant(streaming),
        },
        AgentEvent::ToolExecutionStart { call: call.clone() },
        AgentEvent::ToolExecutionUpdate {
            call: call.clone(),
            result: completed.clone(),
        },
        AgentEvent::ToolExecutionEnd {
            call,
            result: unknown.clone(),
            duration_ms: Some(4),
        },
    ];
    Golden {
        events,
        tool_results: vec![completed, unknown],
    }
}

fn rendered() -> String {
    serde_json::to_string_pretty(&fixtures()).unwrap() + "\n"
}

#[test]
#[ignore = "generator: run against pre-move baseline; never regenerate after the move"]
fn generate_golden_events() {
    let path = fixture_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, rendered()).unwrap();
}

#[test]
fn golden_agent_event_bytes_are_stable() {
    let expected = fs::read_to_string(fixture_path()).expect(
        "golden fixture is missing; run generate_golden_events against pre-move baseline code",
    );
    assert_eq!(rendered(), expected);
    let parsed: Golden = serde_json::from_str(&expected).unwrap();
    assert_eq!(parsed.events.len(), 10);
    assert_eq!(parsed.tool_results.len(), 2);
    assert!(parsed.tool_results.iter().any(|result| result.outcome_unknown));
}

// A renamed required field must not deserialize. This proves the fixture pins
// field names instead of only validating shapes that happen to line up.
#[test]
fn renamed_field_is_rejected_by_the_fixture() {
    let original = fs::read_to_string(fixture_path()).expect(
        "golden fixture is missing; run generate_golden_events against pre-move baseline code",
    );
    assert!(serde_json::from_str::<Golden>(&original).is_ok());
    let renamed = original.replace("\"is_error\"", "\"isError\"");
    assert_ne!(renamed, original);
    assert!(
        serde_json::from_str::<Golden>(&renamed).is_err(),
        "renaming a required field must be rejected"
    );
}
