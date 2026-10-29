use crate::common::{self, Format, Result};
use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, ModelRequest,
    StopReason, ToolCall, ToolDefinition, current_system,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn request_body(request: &ModelRequest) -> Result<Value> {
    let mut prepared = common::prepare_request(request, Format::Anthropic)?;
    let max_tokens = request
        .options
        .max_tokens
        .or(request.model.max_output_tokens)
        .ok_or("Anthropic requires an explicit positive maximum output token count")?;
    let system = current_system(&request.messages);
    let mut messages = Vec::new();
    for (idx, message) in request.messages.iter().enumerate() {
        match message {
            Message::System(_) | Message::Custom { .. } => {}
            Message::User { .. } => {
                let parts = prepared
                    .user_parts
                    .remove(&idx)
                    .expect("validated user parts");
                common::append_turn(&mut messages, "user", "content", parts);
            }
            Message::Assistant(assistant) => {
                let content = history_content(assistant)?;
                common::append_turn(&mut messages, "assistant", "content", content);
            }
            Message::ToolResult(result) => {
                let tool_output = prepared
                    .tool_outputs
                    .remove(&idx)
                    .expect("validated tool output");
                common::append_turn(
                    &mut messages,
                    "user",
                    "content",
                    vec![
                        json!({"type":"tool_result","tool_use_id":result.tool_call_id,"content":tool_output,"is_error":result.is_error}),
                    ],
                );
            }
        }
    }
    if messages.first().is_none_or(|m| m["role"] != "user") {
        return Err("Anthropic history must start with a user turn".into());
    }
    let mut body =
        json!({"model":request.model.id,"messages":messages,"max_tokens":max_tokens,"stream":true});
    if !system.prompt.is_empty() {
        body["system"] = system.prompt.into();
    }
    if !system.tools.is_empty() {
        body["tools"] = system
            .tools
            .iter()
            .map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.parameters}))
            .collect::<Vec<_>>()
            .into();
    }
    if let Some(t) = request.options.temperature {
        body["temperature"] = json!(t);
    }
    Ok(body)
}

// Keep signed blocks out of normalized answer content. Replay uses the original
// wire block order and checks that text and calls still match the saved message.
fn history_content(assistant: &AssistantMessage) -> Result<Vec<Value>> {
    if assistant.provider_metadata.is_empty() {
        return assistant
            .content
            .iter()
            .map(|part| match part {
                Content::Text { text } => Ok(json!({"type":"text","text":text})),
                Content::ToolCall(call) => Ok(json!({
                    "type":"tool_use",
                    "id":call.id,
                    "name":call.name,
                    "input":send_arguments(call.arguments.clone()),
                })),
                _ => Err("Unsupported Anthropic assistant history".to_owned()),
            })
            .collect();
    }
    if assistant.provider_metadata.len() != 1 {
        return Err("Unsupported Anthropic history metadata".into());
    }
    let metadata = assistant
        .provider_metadata
        .get("anthropic")
        .ok_or("Unsupported Anthropic history metadata")?;
    let fields = common::object(metadata)?;
    if fields.len() != 1 || !fields.contains_key("content") {
        return Err("Invalid Anthropic history metadata fields".into());
    }
    let content = metadata["content"]
        .as_array()
        .filter(|content| !content.is_empty())
        .ok_or("Invalid Anthropic history content")?;
    let mut normalized = Vec::new();
    let mut limits = common::ResponseLimits::default();
    for block in content {
        limits.fragment()?;
        validate_block(block, true)?;
        match common::string(block, "type")? {
            "text" => normalized.push(Content::text(common::string(block, "text")?)),
            "tool_use" => {
                limits.tool_call()?;
                normalized.push(Content::ToolCall(ToolCall::new(
                    common::string(block, "id")?,
                    common::string(block, "name")?,
                    block["input"].clone(),
                )));
            }
            "thinking" | "redacted_thinking" => {}
            _ => unreachable!("validated block type"),
        }
    }
    // Argument validity is checked at the request boundary. Metadata must match
    // the actual wire content, which has no local error-description field.
    if normalized.len() != assistant.content.len()
        || !normalized
            .iter()
            .zip(&assistant.content)
            .all(|(wire, saved)| match (wire, saved) {
                (Content::ToolCall(wire), Content::ToolCall(saved)) => {
                    wire.id == saved.id
                        && wire.name == saved.name
                        && wire.arguments == saved.arguments
                }
                _ => wire == saved,
            })
    {
        return Err("Anthropic history metadata does not match assistant content".into());
    }
    // Messages requires an object input. Project only the copy that is sent;
    // the persisted wire block keeps the original malformed value unchanged.
    let mut sent = content.clone();
    for block in &mut sent {
        if block["type"] == "tool_use" {
            let arguments = block["input"].take();
            block["input"] = send_arguments(arguments);
        }
    }
    Ok(sent)
}

fn send_arguments(arguments: Value) -> Value {
    if arguments.is_object() {
        arguments
    } else {
        json!({"invalid_tool_arguments": arguments})
    }
}

fn validate_block(block: &Value, complete: bool) -> Result<()> {
    let fields = common::object(block)?;
    let allowed: &[&str] = match common::string(block, "type")? {
        "text" => {
            common::string(block, "text")?;
            &["type", "text"]
        }
        "tool_use" => {
            for field in ["id", "name"] {
                if common::string(block, field)?.trim().is_empty() {
                    return Err("Invalid Anthropic tool identity".into());
                }
            }
            // Arguments are model output, not protocol identity. Keep any value
            // here so malformed input can be reported back instead of ending the
            // stream; the stop event classifies it.
            block.get("input").ok_or("Missing Anthropic tool input")?;
            if let Some(caller) = block.get("caller") {
                let fields = common::object(caller)?;
                if fields.len() != 1 || caller["type"] != "direct" {
                    return Err("Server-side Anthropic tool callers are unsupported".into());
                }
            }
            &["type", "id", "name", "input", "caller"]
        }
        "thinking" => {
            common::string(block, "thinking")?;
            let signature = common::string(block, "signature")?;
            if complete && signature.is_empty() {
                return Err("Completed Anthropic thinking requires its signature".into());
            }
            &["type", "thinking", "signature"]
        }
        "redacted_thinking" => {
            if common::string(block, "data")?.is_empty() {
                return Err("Invalid Anthropic redacted thinking data".into());
            }
            &["type", "data"]
        }
        _ => return Err("Unsupported Anthropic content block".into()),
    };
    if fields
        .keys()
        .any(|field| !allowed.contains(&field.as_str()))
    {
        return Err("Unsupported Anthropic content block fields".into());
    }
    Ok(())
}

enum Block {
    Text {
        index: usize,
    },
    Tool {
        index: usize,
        initial: Value,
        json: String,
    },
    Thinking {
        signature_started: bool,
    },
    RedactedThinking,
}

pub(crate) struct State {
    pub message: AssistantMessage,
    tools: BTreeMap<String, ToolDefinition>,
    blocks: BTreeMap<u64, Block>,
    seen_blocks: BTreeSet<u64>,
    seen_calls: BTreeSet<String>,
    started: bool,
    stopped: bool,
    finish: Option<StopReason>,
    limits: common::ResponseLimits,
    usage: common::UsageReport,
}

impl State {
    pub fn new(request: &ModelRequest) -> Self {
        let seen_calls = request
            .messages
            .iter()
            .flat_map(|m| match m {
                Message::Assistant(a) => a.tool_calls().into_iter().map(|c| c.id).collect(),
                _ => Vec::new(),
            })
            .collect();
        Self {
            message: AssistantMessage::new(&request.model),
            tools: common::active_tools(request),
            blocks: BTreeMap::new(),
            seen_blocks: BTreeSet::new(),
            seen_calls,
            started: false,
            stopped: false,
            finish: None,
            limits: common::ResponseLimits::default(),
            usage: common::UsageReport::default(),
        }
    }

    pub fn done(&self) -> bool {
        self.stopped
    }

    pub async fn accept(
        &mut self,
        value: Value,
        event: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Vec<AssistantEvent>> {
        common::object(&value)?;
        if self.stopped {
            return Err("Unexpected data after Anthropic message_stop".into());
        }
        let kind = common::string(&value, "type")?;
        if event.is_some_and(|event| event != kind) {
            return Err("SSE event name does not match its JSON type".into());
        }
        if kind == "error" {
            return Err("Anthropic returned a stream error".into());
        }
        if kind == "ping" {
            return Ok(Vec::new());
        }
        if kind == "message_start" {
            if self.started {
                return Err("Duplicate Anthropic message_start".into());
            }
            let message = value
                .get("message")
                .ok_or("Missing message_start message")?;
            common::object(message)?;
            if message["role"] != "assistant"
                || message["type"] != "message"
                || message["content"].as_array().is_none_or(|v| !v.is_empty())
            {
                return Err("Invalid Anthropic message_start".into());
            }
            common::stable(&mut self.message.response_id, message, "id")?;
            common::stable(&mut self.message.response_model, message, "model")?;
            if self.message.response_id.is_none() || self.message.response_model.is_none() {
                return Err("Missing Anthropic response identity".into());
            }
            self.usage(message.get("usage").ok_or("Missing Anthropic usage")?)?;
            self.started = true;
            return Ok(vec![AssistantEvent::UsageUpdate {
                partial: self.message.clone(),
            }]);
        }
        if !self.started {
            return Err("Anthropic event arrived before message_start".into());
        }
        if self.finish.is_some() && !matches!(kind, "message_stop" | "message_delta") {
            return Err("Unexpected Anthropic event after stop reason".into());
        }
        let mut events = Vec::new();
        match kind {
            "content_block_start" => {
                self.limits.fragment()?;
                let wire_index = index(&value)?;
                if wire_index != self.seen_blocks.len() as u64
                    || !self.seen_blocks.insert(wire_index)
                    || !self.blocks.is_empty()
                {
                    return Err(
                        "Out-of-order, duplicate or overlapping Anthropic content block".into(),
                    );
                }
                let block = value.get("content_block").ok_or("Missing content block")?;
                validate_block(block, false)?;
                stored_content(&mut self.message).push(block.clone());
                let index = self.message.content.len();
                match common::string(block, "type")? {
                    "text" => {
                        let text = common::string(block, "text")?.to_owned();
                        self.message.content.push(Content::text(""));
                        self.blocks.insert(wire_index, Block::Text { index });
                        events.push(AssistantEvent::TextStart {
                            index,
                            partial: self.message.clone(),
                        });
                        if !text.is_empty() {
                            events.push(common::text_delta(&mut self.message, index, &text));
                        }
                    }
                    "tool_use" => {
                        self.limits.tool_call()?;
                        let id = common::string(block, "id")?;
                        let name = common::string(block, "name")?;
                        if id.trim().is_empty()
                            || !self.seen_calls.insert(id.to_owned())
                            || !self.tools.contains_key(name)
                        {
                            return Err("Invalid, reused or undeclared Anthropic tool call".into());
                        }
                        let initial = block
                            .get("input")
                            .ok_or("Missing Anthropic tool input")?
                            .clone();
                        self.message.content.push(Content::ToolCall(ToolCall::new(
                            id,
                            name,
                            Value::Null,
                        )));
                        self.blocks.insert(
                            wire_index,
                            Block::Tool {
                                index,
                                initial,
                                json: String::new(),
                            },
                        );
                        events.push(AssistantEvent::ToolCallStart {
                            index,
                            partial: self.message.clone(),
                        });
                    }
                    "thinking" => {
                        self.blocks.insert(
                            wire_index,
                            Block::Thinking {
                                signature_started: !common::string(block, "signature")?.is_empty(),
                            },
                        );
                    }
                    "redacted_thinking" => {
                        self.blocks.insert(wire_index, Block::RedactedThinking);
                    }
                    _ => unreachable!("validated block type"),
                }
            }
            "content_block_delta" => {
                self.limits.fragment()?;
                let wire_index = index(&value)?;
                let delta = value.get("delta").ok_or("Missing content delta")?;
                let block = self
                    .blocks
                    .get_mut(&wire_index)
                    .ok_or("Delta for an unopened content block")?;
                match (block, common::string(delta, "type")?) {
                    (Block::Text { index }, "text_delta") => {
                        let text = common::string(delta, "text")?;
                        if let Value::String(previous) =
                            &mut stored_content(&mut self.message)[wire_index as usize]["text"]
                        {
                            previous.push_str(text);
                        }
                        events.push(common::text_delta(&mut self.message, *index, text));
                    }
                    (
                        Block::Tool {
                            index,
                            json,
                            initial,
                        },
                        "input_json_delta",
                    ) => {
                        if !initial.is_object()
                            || initial.as_object().is_some_and(|o| !o.is_empty())
                        {
                            return Err(
                                "Cannot combine complete tool input with JSON deltas".into()
                            );
                        }
                        let fragment = common::string(delta, "partial_json")?;
                        json.push_str(fragment);
                        events.push(AssistantEvent::ToolCallDelta {
                            index: *index,
                            delta: fragment.to_owned(),
                            partial: self.message.clone(),
                        });
                    }
                    (Block::Thinking { signature_started }, "thinking_delta") => {
                        if *signature_started {
                            return Err("Anthropic thinking changed after its signature".into());
                        }
                        let text = common::string(delta, "thinking")?;
                        if let Value::String(previous) =
                            &mut stored_content(&mut self.message)[wire_index as usize]["thinking"]
                        {
                            previous.push_str(text);
                        }
                    }
                    (Block::Thinking { signature_started }, "signature_delta") => {
                        let signature = common::string(delta, "signature")?;
                        // Signature deltas replace rather than append, as in the
                        // native Messages accumulator. The value stays opaque.
                        stored_content(&mut self.message)[wire_index as usize]["signature"] =
                            signature.into();
                        *signature_started = true;
                    }
                    _ => return Err("Content delta does not match its block type".into()),
                }
            }
            "content_block_stop" => {
                let wire_index = index(&value)?;
                let block = self
                    .blocks
                    .remove(&wire_index)
                    .ok_or("Stop for an unopened content block")?;
                match block {
                    Block::Text { index } => events.push(AssistantEvent::TextEnd {
                        index,
                        partial: self.message.clone(),
                    }),
                    Block::Tool {
                        index,
                        initial,
                        json,
                    } => {
                        let arguments = if json.is_empty() {
                            initial
                        } else {
                            match crate::json::parse_response(&json, cancel).await {
                                Ok(value) => value,
                                // Keep syntax errors verbatim; cancellation and
                                // parser resource limits still stop the response.
                                Err(
                                    crate::json::ParseError::InvalidSyntax
                                    | crate::json::ParseError::TrailingData,
                                ) => Value::String(json),
                                Err(error) => return Err(error.into()),
                            }
                        };
                        let schema = {
                            let Content::ToolCall(call) = &self.message.content[index] else {
                                unreachable!()
                            };
                            self.tools
                                .get(&call.name)
                                .ok_or("Response calls an undeclared tool")?
                                .parameters
                                .clone()
                        };
                        let Content::ToolCall(call) = &mut self.message.content[index] else {
                            unreachable!()
                        };
                        call.arguments = arguments;
                        call.arguments_error = common::arguments_error(&call.arguments, &schema)?;
                        let arguments = call.arguments.clone();
                        stored_content(&mut self.message)[wire_index as usize]["input"] = arguments;
                        events.push(AssistantEvent::ToolCallEnd {
                            index,
                            partial: self.message.clone(),
                        });
                    }
                    Block::Thinking { .. } | Block::RedactedThinking => {}
                }
                validate_block(
                    &stored_content(&mut self.message)[wire_index as usize],
                    true,
                )?;
            }
            "message_delta" => {
                if !self.blocks.is_empty() {
                    return Err("Anthropic finish has unclosed content blocks".into());
                }
                let delta = value.get("delta").ok_or("Missing message delta")?;
                common::object(delta)?;
                if let Some(reason) = delta.get("stop_reason").filter(|v| !v.is_null()) {
                    let reason = match reason.as_str() {
                        Some("end_turn" | "stop_sequence") => StopReason::Stop,
                        Some("max_tokens" | "model_context_window_exceeded") => StopReason::Length,
                        Some("tool_use") => StopReason::ToolUse,
                        // pause_turn requires server tool support.
                        _ => return Err("Unsupported Anthropic stop reason".into()),
                    };
                    let has_calls = self
                        .message
                        .content
                        .iter()
                        .any(|p| matches!(p, Content::ToolCall(_)));
                    if has_calls != (reason == StopReason::ToolUse) {
                        return Err("Anthropic stop reason does not match its tool calls".into());
                    }
                    if self.finish.is_some_and(|old| old != reason) {
                        return Err("Anthropic stop reason changed during a stream".into());
                    }
                    self.finish = Some(reason);
                    self.message.stop_reason = reason;
                }
                if let Some(usage) = value.get("usage") {
                    self.usage(usage)?;
                    events.push(AssistantEvent::UsageUpdate {
                        partial: self.message.clone(),
                    });
                }
            }
            "message_stop" => {
                if self.finish.is_none() || !self.blocks.is_empty() {
                    return Err("Anthropic ended without a complete stop reason".into());
                }
                self.stopped = true;
            }
            _ => return Err("Unsupported Anthropic stream event".into()),
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<()> {
        if !self.stopped {
            Err("Anthropic stream ended before message_stop".into())
        } else if self.message.content.is_empty() {
            Err("Anthropic returned no supported content".into())
        } else {
            history_content(&self.message)?;
            Ok(())
        }
    }

    fn usage(&mut self, usage: &Value) -> Result<()> {
        self.usage.update(
            &mut self.message,
            usage,
            &[
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ],
        )?;
        let u = &mut self.message.usage;
        u.input = self.usage.get("input_tokens").unwrap_or_default();
        u.output = self.usage.get("output_tokens").unwrap_or_default();
        u.cache_read = self
            .usage
            .get("cache_read_input_tokens")
            .unwrap_or_default();
        u.cache_write = self
            .usage
            .get("cache_creation_input_tokens")
            .unwrap_or_default();
        u.total_tokens = u
            .input
            .checked_add(u.output)
            .and_then(|total| total.checked_add(u.cache_read))
            .and_then(|total| total.checked_add(u.cache_write))
            .ok_or("Anthropic usage total overflows")?;
        self.message.usage_status = self.usage.status("input_tokens", "output_tokens");
        Ok(())
    }
}

fn stored_content(message: &mut AssistantMessage) -> &mut Vec<Value> {
    message
        .provider_metadata
        .entry("anthropic".into())
        .or_insert_with(|| json!({"content":[]}))["content"]
        .as_array_mut()
        .expect("constructed Anthropic content")
}

fn index(value: &Value) -> Result<u64> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Invalid content block index".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_ai::{Model, StreamOptions, SystemMessage};

    #[tokio::test]
    async fn cancellation_during_tool_json_parsing_is_not_an_argument_error() {
        let request = ModelRequest {
            model: Model::new("anthropic", "synthetic-model"),
            messages: vec![Message::System(SystemMessage {
                tools_added: vec![ToolDefinition {
                    name: "fixture".into(),
                    description: "Synthetic tool".into(),
                    parameters: json!({"type":"object"}),
                }],
                ..Default::default()
            })],
            options: StreamOptions::default(),
        };
        let cancel = CancellationToken::new();
        let mut state = State::new(&request);
        for event in [
            json!({"type":"message_start","message":{"id":"cancel-fixture","model":"synthetic-model","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"cancel-call","name":"fixture","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":format!("[{}0]", "0,".repeat(1024))}}),
        ] {
            state.accept(event, None, &cancel).await.unwrap();
        }
        let stop = state.accept(
            json!({"type":"content_block_stop","index":0}),
            None,
            &cancel,
        );
        tokio::pin!(stop);
        assert!(futures::poll!(&mut stop).is_pending());
        cancel.cancel();
        assert_eq!(stop.await.unwrap_err(), "Request cancelled");
    }
}
