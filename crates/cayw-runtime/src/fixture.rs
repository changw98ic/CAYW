//! Explicit offline provider for demonstrations and repeatable integration tests.
//! It emits Luau; the actual Host performs the operations under approval.
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message,
    ModelRequest, Provider, StopReason, ToolCall, Usage, UsageStatus, current_system,
};
use cayw_cells::{CellPage, CellStatus};
use futures::stream;
use serde_json::json;
use uuid::Uuid;

pub const DEMO_SCRIPT: &str = "cayw.call('fs.write', {path='cayw-demo.txt', content='headless runtime demo'})\nlocal result = cayw.call('fs.read', {path='cayw-demo.txt'})\nreturn result";
pub const PLAN_DEMO_SCRIPT: &str = "cayw.call('plan.update', {expected_version=0, steps={{id='step-1', text='分析任务需求与工作区上下文', status='completed'}, {id='step-2', text='调用受控 CodeMode 能力执行文件读写', status='in_progress'}, {id='step-3', text='核验输出并完成任务总结', status='pending'}}})\ncayw.call('fs.write', {path='cayw-demo.txt', content='headless runtime demo'})\nlocal result = cayw.call('fs.read', {path='cayw-demo.txt'})\nreturn result";
pub const MARKER_READ_SCRIPT: &str =
    "local result = cayw.call('fs.read', {path='device-marker.txt'})\nreturn result";
pub const PLAN_MARKER_READ_SCRIPT: &str = "cayw.call('plan.update', {expected_version=0, steps={{id='step-1', text='分析任务需求与工作区上下文', status='completed'}, {id='step-2', text='调用受控 CodeMode 能力读取目标设备标识文件', status='in_progress'}, {id='step-3', text='核验输出并完成任务总结', status='pending'}}})\nlocal result = cayw.call('fs.read', {path='device-marker.txt'})\nreturn result";

/// Synthetic model response using the same exec/wait contract as a real model.
/// It reads every output page and waits for VM completion before reporting done.
/// The fixture never grants permission, performs an effect, or replays a cell.
/// Every response reports fixed synthetic usage of 4 input and 4 output tokens.
/// These counters exercise accounting; they are not tokenizer or billing data.
pub fn script_response(request: &ModelRequest, script: &str, call_id: &str) -> AssistantMessage {
    let mut message = AssistantMessage::new(&request.model);
    message.usage = Usage {
        input: 4,
        output: 4,
        total_tokens: 8,
        ..Default::default()
    };
    message.usage_status = UsageStatus::Reported;
    let declarations = current_system(&request.messages);
    let limits = declarations
        .tools
        .iter()
        .find(|definition| definition.name == "execute_code")
        .and_then(|definition| definition.parameters["oneOf"].as_array());
    let maximum = |action: &str, field: &str, fallback: u64| {
        limits
            .and_then(|schemas| {
                schemas
                    .iter()
                    .find(|schema| schema["properties"]["action"]["const"] == action)
            })
            .and_then(|schema| schema["properties"][field]["maximum"].as_u64())
            .unwrap_or(fallback)
    };
    let arguments = match request.messages.last() {
        Some(Message::ToolResult(result)) => {
            let page = result
                .structured_content
                .as_ref()
                .and_then(|value| serde_json::from_value::<CellPage>(value.clone()).ok());
            match page {
                Some(page)
                    if !result.outcome_unknown
                        && !matches!(
                            page.status,
                            CellStatus::OutcomeUnknown | CellStatus::Cancelled
                        )
                        && (!page.status.is_terminal() || page.has_more) =>
                {
                    json!({
                        "action":"wait", "cell_id":page.cell_id, "cursor":page.next_cursor,
                        "yield_time_ms":maximum("wait", "yield_time_ms", 30_000),
                        "max_output_tokens":maximum("wait", "max_output_tokens", 128*1024),
                    })
                }
                _ => {
                    // Include prior pages from this user turn so an early page
                    // cannot disappear from the final synthetic response.
                    let start = request
                        .messages
                        .iter()
                        .rposition(|message| matches!(message, Message::User { .. }))
                        .unwrap_or(0);
                    let text = request.messages[start..]
                        .iter()
                        .filter_map(|message| {
                            if let Message::ToolResult(result) = message {
                                Some(cayw_ai::content_text(&result.content))
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    message
                        .content
                        .push(Content::text(format!("Offline fixture: {text}")));
                    message.stop_reason = StopReason::Stop;
                    return message;
                }
            }
        }
        _ => json!({
            "action":"exec", "script":script,
            "yield_time_ms":maximum("exec", "yield_time_ms", 30_000),
            "max_output_tokens":maximum("exec", "max_output_tokens", 128*1024),
        }),
    };
    message.content.push(Content::ToolCall(ToolCall::new(
        call_id,
        "execute_code",
        arguments,
    )));
    message.stop_reason = StopReason::ToolUse;
    message
}

#[derive(Default)]
pub struct OfflineProvider {
    _private: (),
}
impl Provider for OfflineProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        if cancel.is_cancelled() {
            let message = AssistantMessage::new(&request.model)
                .failure(StopReason::Aborted, "offline fixture cancelled");
            return Box::pin(stream::iter(vec![AssistantEvent::Error { message }]));
        }
        let last_user_text = request
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content, .. } => Some(cayw_ai::content_text(content)),
                _ => None,
            })
            .unwrap_or_default();
        let is_plan = last_user_text.contains("[规划模式]");
        let is_marker = last_user_text.contains("device-marker.txt");
        let selected_script = match (is_plan, is_marker) {
            (true, true) => PLAN_MARKER_READ_SCRIPT,
            (true, false) => PLAN_DEMO_SCRIPT,
            (false, true) => MARKER_READ_SCRIPT,
            (false, false) => DEMO_SCRIPT,
        };
        let message = script_response(
            &request,
            selected_script,
            &format!("offline-{}", Uuid::new_v4()),
        );
        Box::pin(stream::iter(vec![AssistantEvent::Done { message }]))
    }
}
