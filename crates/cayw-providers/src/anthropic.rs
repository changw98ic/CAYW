use crate::common::{self, Format, Result};
use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, ModelRequest,
    StopReason, ToolCall, ToolDefinition, current_system,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn request_body(request: &ModelRequest) -> Result<Value> {
    common::validate_request(request, Format::Anthropic)?;
    let max_tokens = request
        .options
        .max_tokens
        .or(request.model.max_output_tokens)
        .ok_or("Anthropic requires an explicit positive maximum output token count")?;
    let system = current_system(&request.messages);
    let mut messages = Vec::new();
    for message in &request.messages {
        match message {
            Message::System(_) | Message::Custom { .. } => {}
            Message::User { content, .. } => common::append_turn(
                &mut messages,
                "user",
                "content",
                common::input_parts(content, request.model.supports_images, Format::Anthropic)?,
            ),
            Message::Assistant(assistant) => {
                let content = assistant.content.iter().map(|part| match part {
                    Content::Text { text } => Ok(json!({"type":"text","text":text})),
                    Content::ToolCall(call) => Ok(json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments})),
                    _ => Err("Unsupported assistant content".to_owned()),
                }).collect::<Result<Vec<_>>>()?;
                common::append_turn(&mut messages, "assistant", "content", content);
            }
            Message::ToolResult(result) => common::append_turn(
                &mut messages,
                "user",
                "content",
                vec![
                    json!({"type":"tool_result","tool_use_id":result.tool_call_id,"content":common::tool_output(result)?,"is_error":result.is_error}),
                ],
            ),
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

enum Block {
    Text {
        index: usize,
    },
    Tool {
        index: usize,
        initial: Value,
        json: String,
    },
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
            return Ok(Vec::new());
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
                if !self.seen_blocks.insert(wire_index) || !self.blocks.is_empty() {
                    return Err("Duplicate or overlapping Anthropic content block".into());
                }
                let block = value.get("content_block").ok_or("Missing content block")?;
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
                            .filter(|v| v.is_object())
                            .ok_or("Tool input must be an object")?
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
                    _ => return Err(
                        "Anthropic thinking, signatures and non-text/tool blocks are unsupported"
                            .into(),
                    ),
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
                    (Block::Text { index }, "text_delta") => events.push(common::text_delta(
                        &mut self.message,
                        *index,
                        common::string(delta, "text")?,
                    )),
                    (
                        Block::Tool {
                            index,
                            json,
                            initial,
                        },
                        "input_json_delta",
                    ) => {
                        if initial.as_object().is_some_and(|o| !o.is_empty()) {
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
                    _ => return Err("Content delta does not match its block type".into()),
                }
            }
            "content_block_stop" => {
                let block = self
                    .blocks
                    .remove(&index(&value)?)
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
                            crate::json::parse_response(&json, cancel).await?
                        };
                        let Content::ToolCall(call) = &mut self.message.content[index] else {
                            unreachable!()
                        };
                        call.arguments = arguments;
                        common::validate_call(call, &self.tools)?;
                        events.push(AssistantEvent::ToolCallEnd {
                            index,
                            partial: self.message.clone(),
                        });
                    }
                }
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
            Ok(())
        }
    }

    fn usage(&mut self, usage: &Value) -> Result<()> {
        common::object(usage)?;
        let u = &mut self.message.usage;
        common::counter(usage, "input_tokens", &mut u.input)?;
        common::counter(usage, "output_tokens", &mut u.output)?;
        common::counter(usage, "cache_read_input_tokens", &mut u.cache_read)?;
        common::counter(usage, "cache_creation_input_tokens", &mut u.cache_write)?;
        u.update_total();
        Ok(())
    }
}

fn index(value: &Value) -> Result<u64> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Invalid content block index".into())
}
