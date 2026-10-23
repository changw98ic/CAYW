use base64::Engine;
use cayw_ai::{
    ArgumentValidator, AssistantEvent, AssistantMessage, CancellationToken, Content, Message,
    ModelRequest, StopReason, ThinkingLevel, ToolCall, ToolDefinition, ToolResultMessage,
    UsageStatus, current_system,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) type Result<T> = std::result::Result<T, String>;

// Byte limits alone cannot bound work or the number of full-message snapshots.
// Count content fragments (including empty ones) and calls over the whole reply.
const MAX_FRAGMENTS: usize = 4096;
const MAX_TOOL_CALLS: usize = 128;

#[derive(Default)]
pub(crate) struct ResponseLimits {
    fragments: usize,
    calls: usize,
}

impl ResponseLimits {
    pub fn fragment(&mut self) -> Result<()> {
        if self.fragments == MAX_FRAGMENTS {
            return Err("Provider response exceeds the content fragment limit".into());
        }
        self.fragments += 1;
        Ok(())
    }

    pub fn tool_call(&mut self) -> Result<()> {
        if self.calls == MAX_TOOL_CALLS {
            return Err("Provider response exceeds the tool call limit".into());
        }
        self.calls += 1;
        Ok(())
    }
}

pub(crate) fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err("Request cancelled".into())
    } else {
        Ok(())
    }
}

// Even an eager consumer must let a same-thread cancellation task run. One
// checkpoint per bounded parsing/event step also avoids a ready-stream hot loop.
pub(crate) async fn checkpoint(cancel: &CancellationToken) -> Result<()> {
    check_cancel(cancel)?;
    tokio::task::yield_now().await;
    check_cancel(cancel)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Anthropic,
    Gemini,
    Responses,
}

pub(crate) fn validate_request(request: &ModelRequest, format: Format) -> Result<()> {
    if request.model.id.trim().is_empty() {
        return Err("Model ID must not be empty".into());
    }
    if request.options.thinking_level != ThinkingLevel::Off {
        return Err("Thinking options are unsupported by this adapter".into());
    }
    if request
        .options
        .max_tokens
        .or(request.model.max_output_tokens)
        == Some(0)
    {
        return Err("Maximum output tokens must be positive".into());
    }
    let max_temperature = if format == Format::Anthropic {
        1.0
    } else {
        2.0
    };
    if request
        .options
        .temperature
        .is_some_and(|t| !t.is_finite() || !(0.0..=max_temperature).contains(&t))
    {
        return Err("Temperature is outside the provider's supported range".into());
    }
    // Reconcile unknown outcomes even when another malformed message occurs earlier.
    if request
        .messages
        .iter()
        .any(|message| matches!(message, Message::ToolResult(result) if result.outcome_unknown))
    {
        return Err("Tool outcome is unknown; reconcile effects before another request".into());
    }
    let mut pending = BTreeMap::<String, String>::new();
    let mut seen = BTreeSet::new();
    let mut definitions = BTreeMap::<String, ToolDefinition>::new();
    let mut has_conversation = false;
    let mut awaiting_model = false;
    for message in &request.messages {
        match message {
            Message::Custom { .. } => {}
            Message::System(system) => {
                for name in &system.tools_removed {
                    definitions.remove(name);
                }
                let mut added = BTreeSet::new();
                for tool in &system.tools_added {
                    validate_definition(tool, format)?;
                    if !added.insert(&tool.name) {
                        return Err("Duplicate tool declaration in a system message".into());
                    }
                    definitions.insert(tool.name.clone(), tool.clone());
                }
            }
            Message::User { content, .. } => {
                require_results(&pending)?;
                input_parts(content, request.model.supports_images, format)?;
                has_conversation = true;
                awaiting_model = true;
            }
            Message::Assistant(assistant) => {
                require_results(&pending)?;
                if !awaiting_model {
                    return Err(
                        "Assistant history needs a preceding user or tool-result turn".into(),
                    );
                }
                awaiting_model = false;
                let metadata_key = match format {
                    Format::Anthropic => Some("anthropic"),
                    Format::Gemini => Some("gemini"),
                    Format::Responses => Some("openai_responses"),
                };
                if assistant
                    .provider_metadata
                    .keys()
                    .any(|key| Some(key.as_str()) != metadata_key)
                {
                    return Err("Assistant replay metadata belongs to another protocol".into());
                }
                if matches!(
                    assistant.stop_reason,
                    StopReason::Pending | StopReason::Error | StopReason::Aborted
                ) {
                    return Err("Incomplete or failed assistant history cannot be replayed".into());
                }
                if assistant.content.is_empty() {
                    return Err("Assistant history must contain content".into());
                }
                for part in &assistant.content {
                    match part {
                        Content::Text { .. } => {}
                        Content::ToolCall(call) => {
                            validate_call(call, &definitions)?;
                            if !seen.insert(call.id.clone()) {
                                return Err("Duplicate tool call ID in history".into());
                            }
                            pending.insert(call.id.clone(), call.name.clone());
                        }
                        Content::Thinking { .. } => {
                            return Err("Thinking and signature history is unsupported".into());
                        }
                        Content::Image { .. } => {
                            return Err("Assistant image history is unsupported".into());
                        }
                    }
                }
                if !pending.is_empty() && assistant.stop_reason != StopReason::ToolUse {
                    return Err("Assistant tool calls require a tool-use stop reason".into());
                }
                if pending.is_empty() && assistant.stop_reason == StopReason::ToolUse {
                    return Err("Tool-use history has no tool calls".into());
                }
                has_conversation = true;
            }
            Message::ToolResult(result) => {
                let name = pending
                    .remove(&result.tool_call_id)
                    .ok_or("Unmatched or duplicate tool result")?;
                if name != result.tool_name {
                    return Err("Tool result name does not match its call".into());
                }
                tool_output(result)?;
                has_conversation = true;
                awaiting_model = true;
            }
        }
    }
    require_results(&pending)?;
    if !has_conversation {
        return Err("Request needs conversational messages".into());
    }
    if !awaiting_model {
        return Err("Request must end with a user or paired tool-result turn".into());
    }
    Ok(())
}

fn require_results(pending: &BTreeMap<String, String>) -> Result<()> {
    if pending.is_empty() {
        Ok(())
    } else {
        Err("Missing result for a tool call".into())
    }
}

fn validate_definition(tool: &ToolDefinition, format: Format) -> Result<()> {
    let max_len = if format == Format::Gemini { 128 } else { 64 };
    if tool.name.is_empty()
        || tool.name.len() > max_len
        || !tool
            .name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        return Err("Tool name has unsupported characters or length".into());
    }
    if !tool.parameters.is_object() || tool.parameters["type"] != "object" {
        return Err("Tool schema must declare an object".into());
    }
    ArgumentValidator::new(&tool.parameters)
        .map_err(|_| "Invalid or external tool schema".to_owned())?;
    Ok(())
}

pub(crate) fn validate_call(
    call: &ToolCall,
    definitions: &BTreeMap<String, ToolDefinition>,
) -> Result<()> {
    if call.id.trim().is_empty()
        || call.name.trim().is_empty()
        || call.arguments_error.is_some()
        || !call.arguments.is_object()
    {
        return Err("Invalid tool call ID, name or arguments".into());
    }
    let tool = definitions
        .get(&call.name)
        .ok_or("Response or history calls an undeclared tool")?;
    ArgumentValidator::new(&tool.parameters)
        .map_err(|_| "Invalid tool schema".to_owned())?
        .validate(&call.arguments)
        .map_err(|_| "Tool arguments do not satisfy the declared schema".to_owned())
}

pub(crate) fn active_tools(request: &ModelRequest) -> BTreeMap<String, ToolDefinition> {
    current_system(&request.messages)
        .tools
        .into_iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect()
}

pub(crate) fn input_parts(content: &[Content], images: bool, format: Format) -> Result<Vec<Value>> {
    if content.is_empty() {
        return Err("User content must not be empty".into());
    }
    content.iter().map(|part| match part {
        Content::Text { text } => Ok(match format {
            Format::Anthropic => json!({"type":"text","text":text}),
            Format::Gemini => json!({"text":text}),
            Format::Responses => json!({"type":"input_text","text":text}),
        }),
        Content::Image { data, mime_type } => {
            if !images { return Err("The requested model is not declared to support user images".into()); }
            if !matches!(mime_type.as_str(), "image/png" | "image/jpeg" | "image/webp" | "image/gif") || (format == Format::Gemini && mime_type == "image/gif") {
                return Err("Unsupported user image MIME type".into());
            }
            if data.is_empty() || base64::engine::general_purpose::STANDARD.decode(data).is_err() {
                return Err("User image data must be nonempty standard base64".into());
            }
            Ok(match format {
                Format::Anthropic => json!({"type":"image","source":{"type":"base64","media_type":mime_type,"data":data}}),
                Format::Gemini => json!({"inlineData":{"mimeType":mime_type,"data":data}}),
                Format::Responses => json!({"type":"input_image","image_url":format!("data:{mime_type};base64,{data}")}),
            })
        }
        _ => Err("User content supports text and explicitly enabled images only".into()),
    }).collect()
}

pub(crate) fn tool_output(result: &ToolResultMessage) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for part in &result.content {
        if let Content::Text { text } = part {
            output.push(json!({"type":"text","text":text}));
        } else {
            return Err("Tool results support text and structured JSON only".into());
        }
    }
    if let Some(structured) = &result.structured_content {
        output.push(json!({"type":"text","text":structured.to_string()}));
    }
    Ok(output)
}

pub(crate) fn append_turn(turns: &mut Vec<Value>, role: &str, field: &str, parts: Vec<Value>) {
    if let Some(previous) = turns.last_mut().filter(|v| v["role"] == role) {
        previous[field]
            .as_array_mut()
            .expect("constructed array")
            .extend(parts);
    } else {
        let mut turn = json!({"role":role});
        turn[field] = parts.into();
        turns.push(turn);
    }
}

pub(crate) fn object(value: &Value) -> Result<&serde_json::Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| "Protocol JSON must be an object".into())
}

pub(crate) fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing or invalid protocol string".into())
}

pub(crate) fn stable(slot: &mut Option<String>, value: &Value, field: &str) -> Result<()> {
    if let Some(value) = value.get(field).filter(|v| !v.is_null()) {
        let text = value
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or("Invalid response identity")?;
        if slot.as_ref().is_some_and(|old| old != text) {
            return Err("Response identity changed during a stream".into());
        }
        *slot = Some(text.to_owned());
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct UsageReport {
    fields: BTreeMap<&'static str, u64>,
}

impl UsageReport {
    pub fn update(
        &mut self,
        message: &mut AssistantMessage,
        value: &Value,
        fields: &[&'static str],
    ) -> Result<()> {
        if value.is_null() {
            return Err("Missing provider usage report".into());
        }
        message.usage_status = UsageStatus::Invalid;
        object(value)?;
        for &field in fields {
            if let Some(value) = value.get(field) {
                message.usage_fields.insert(field.to_owned(), value.clone());
            }
        }
        let mut updated = self.fields.clone();
        for &field in fields {
            if let Some(value) = value.get(field).filter(|value| !value.is_null()) {
                let value = value.as_u64().ok_or("Invalid nonnegative usage counter")?;
                if self
                    .fields
                    .get(field)
                    .is_some_and(|previous| value < *previous)
                {
                    return Err("Cumulative usage counter decreased".into());
                }
                updated.insert(field, value);
            }
        }
        self.fields = updated;
        Ok(())
    }

    pub fn get(&self, field: &str) -> Option<u64> {
        self.fields.get(field).copied()
    }

    pub fn status(&self, input: &str, output: &str) -> UsageStatus {
        if self.get(input).is_some() && self.get(output).is_some() {
            UsageStatus::Reported
        } else if self.fields.is_empty() {
            UsageStatus::Unknown
        } else {
            UsageStatus::Partial
        }
    }
}

pub(crate) fn text_delta(
    message: &mut AssistantMessage,
    index: usize,
    delta: &str,
) -> AssistantEvent {
    if let Content::Text { text } = &mut message.content[index] {
        text.push_str(delta);
    }
    AssistantEvent::TextDelta {
        index,
        delta: delta.to_owned(),
        partial: message.clone(),
    }
}
