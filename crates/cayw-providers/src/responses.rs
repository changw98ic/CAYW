use crate::common::{self, Format, Result};
use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, ModelRequest,
    StopReason, ToolCall, ToolDefinition, Usage, UsageStatus, current_system,
};
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

// A single tool argument string may arrive as many bounded deltas. This cap is
// independent of the stream-wide byte limit and is checked before appending.
const MAX_ARGUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_REPLAY_DEPTH: usize = 128;

pub(crate) fn request_body(request: &ModelRequest) -> Result<Value> {
    common::validate_request(request, Format::Responses)?;
    let system = current_system(&request.messages);
    let mut input = Vec::new();
    for message in &request.messages {
        match message {
            Message::System(_) | Message::Custom { .. } => {}
            Message::User { content, .. } => input.push(json!({
                "role": "user",
                "content": common::input_parts(content, request.model.supports_images, Format::Responses)?,
            })),
            // The original output items are required replay material. Normalized
            // text or calls alone cannot reconstruct reasoning identities.
            Message::Assistant(assistant) => input.extend(history_items(assistant)?),
            Message::ToolResult(result) => {
                let output = common::tool_output(result)?
                    .into_iter()
                    .map(|part| {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                        json!({"type":"input_text","text":text})
                    })
                    .collect::<Vec<_>>();
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": result.tool_call_id,
                    "output": output,
                }));
            }
        }
    }
    let mut body = json!({
        "model": request.model.id,
        "input": input,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    if !system.prompt.is_empty() {
        body["instructions"] = system.prompt.into();
    }
    if !system.tools.is_empty() {
        body["tools"] = system
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                    "strict": false,
                })
            })
            .collect::<Vec<_>>()
            .into();
    }
    if let Some(max) = request
        .options
        .max_tokens
        .or(request.model.max_output_tokens)
    {
        body["max_output_tokens"] = max.into();
    }
    if let Some(temperature) = request.options.temperature {
        body["temperature"] = json!(temperature);
    }
    Ok(body)
}

fn history_items(assistant: &AssistantMessage) -> Result<Vec<Value>> {
    if assistant.provider_metadata.len() != 1 {
        return Err("Missing or conflicting Responses replay metadata".into());
    }
    let metadata = assistant
        .provider_metadata
        .get("openai_responses")
        .ok_or("Missing Responses replay metadata")?;
    let fields = common::object(metadata)?;
    if fields.len() != 1 || !fields.contains_key("output") {
        return Err("Invalid Responses replay metadata fields".into());
    }
    let output = metadata["output"]
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or("Invalid Responses replay output")?;
    let mut normalized = Vec::new();
    let mut seen_items = BTreeSet::new();
    let mut seen_calls = BTreeSet::new();
    let mut limits = common::ResponseLimits::default();
    for item in output {
        limits.fragment()?;
        common::object(item)?;
        let id = common::string(item, "id")?;
        if id.trim().is_empty() {
            return Err("Invalid Responses replay item ID".into());
        }
        if !seen_items.insert(id.to_owned()) {
            return Err("Duplicate Responses replay item ID".into());
        }
        match common::string(item, "type")? {
            "reasoning" => {
                item.get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|encrypted| !encrypted.is_empty())
                    .ok_or("Responses reasoning replay requires encrypted content")?;
            }
            "message" => {
                if common::string(item, "role")? != "assistant" {
                    return Err("Invalid Responses replay message role".into());
                }
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .filter(|parts| !parts.is_empty())
                    .ok_or("Invalid Responses replay message content")?;
                for part in content {
                    common::object(part)?;
                    if common::string(part, "type")? != "output_text" {
                        return Err("Unsupported Responses replay message part".into());
                    }
                    let text = common::string(part, "text")?;
                    normalized.push(Content::text(text));
                }
            }
            "function_call" => {
                limits.tool_call()?;
                let call_id = common::string(item, "call_id")?;
                if call_id.trim().is_empty() || !seen_calls.insert(call_id.to_owned()) {
                    return Err("Invalid or duplicate Responses replay call ID".into());
                }
                let name = common::string(item, "name")?;
                let arguments = parse_arguments(common::string(item, "arguments")?)?;
                normalized.push(Content::ToolCall(ToolCall::new(call_id, name, arguments)));
            }
            _ => return Err("Unsupported Responses replay item type".into()),
        }
    }
    if normalized != assistant.content {
        return Err("Responses replay metadata does not match assistant content".into());
    }
    Ok(output.clone())
}

pub(crate) struct State {
    pub message: AssistantMessage,
    tools: BTreeMap<String, ToolDefinition>,
    seen_calls: BTreeSet<String>,
    created: bool,
    completed: bool,
    items: Vec<Item>,
    limits: common::ResponseLimits,
}

struct Item {
    id: String,
    done: Option<Value>,
    kind: ItemKind,
}

enum ItemKind {
    Message {
        parts: Vec<TextPart>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        buffer: String,
        content_index: usize,
        arguments_done: bool,
    },
    Reasoning {
        parts: Vec<ReasoningPart>,
        unannounced_text: bool,
    },
}

struct TextPart {
    content_index: usize,
    text: String,
    text_done: bool,
    part_done: bool,
}

struct ReasoningPart {
    text: String,
    text_done: bool,
    part_done: bool,
}

impl State {
    pub fn new(request: &ModelRequest) -> Self {
        let seen_calls = request
            .messages
            .iter()
            .flat_map(|message| match message {
                Message::Assistant(assistant) => assistant
                    .tool_calls()
                    .into_iter()
                    .map(|call| call.id)
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        Self {
            message: AssistantMessage::new(&request.model),
            tools: common::active_tools(request),
            seen_calls,
            created: false,
            completed: false,
            items: Vec::new(),
            limits: common::ResponseLimits::default(),
        }
    }

    pub fn accept<'a>(
        &'a mut self,
        value: Value,
        event: Option<&'a str>,
        cancel: &'a CancellationToken,
    ) -> BoxStream<'a, Result<AssistantEvent>> {
        async_stream::try_stream! {
            common::checkpoint(cancel).await?;
            let updates = self.handle(&value, event)?;
            for update in updates {
                common::checkpoint(cancel).await?;
                yield update;
            }
        }
        .boxed()
    }

    pub fn done(&self) -> bool {
        self.completed
    }

    pub fn finish(&mut self) -> Result<()> {
        if !self.completed {
            return Err("Responses stream ended without a completed event".into());
        }
        if self.message.content.is_empty() {
            return Err("Responses returned no supported content".into());
        }
        Ok(())
    }

    fn require_created(&self) -> Result<()> {
        if self.created {
            Ok(())
        } else {
            Err("Responses event arrived before response.created".into())
        }
    }

    fn handle(&mut self, value: &Value, event: Option<&str>) -> Result<Vec<AssistantEvent>> {
        common::object(value)?;
        let event_type = common::string(value, "type")?;
        if let Some(name) = event.filter(|name| !name.is_empty())
            && name != event_type
        {
            return Err("SSE event name does not match its payload".into());
        }
        let mut events = Vec::new();
        match event_type {
            "response.created" => self.created(value)?,
            "response.in_progress" => {
                self.require_created()?;
                self.stable_identity(response(value)?)?;
            }
            "response.output_item.added" => self.output_item_added(value, &mut events)?,
            "response.content_part.added" => self.content_part_added(value, &mut events)?,
            "response.output_text.delta" => self.output_text_delta(value, &mut events)?,
            "response.output_text.done" => self.output_text_done(value)?,
            "response.reasoning_text.delta" => self.reasoning_text_delta(value)?,
            "response.reasoning_text.done" => self.reasoning_text_done(value)?,
            "response.content_part.done" => self.content_part_done(value, &mut events)?,
            "response.function_call_arguments.delta" => self.arguments_delta(value, &mut events)?,
            "response.function_call_arguments.done" => self.arguments_done(value)?,
            "response.output_item.done" => self.output_item_done(value, &mut events)?,
            "response.completed" => self.completed(value, &mut events)?,
            "response.failed" => {
                self.terminal_response(value, "failed")?;
                return Err("Provider response failed".into());
            }
            "response.incomplete" => {
                self.terminal_response(value, "incomplete")?;
                return Err("Provider response was incomplete".into());
            }
            "error" | "response.error" => return Err("Provider returned an error event".into()),
            "response.refusal.delta" | "response.refusal.done" => {
                return Err("Responses refusal content is unsupported".into());
            }
            // Reasoning summaries are retained only through the original
            // reasoning item. They are never turned into visible thinking text.
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.output_text.annotation.added" => self.require_created()?,
            _ => self.require_created()?,
        }
        Ok(events)
    }

    fn created(&mut self, value: &Value) -> Result<()> {
        if self.created {
            return Err("Duplicate response.created event".into());
        }
        let response = response(value)?;
        if response.get("object").and_then(Value::as_str) != Some("response") {
            return Err("Invalid Responses response object".into());
        }
        let id = common::string(response, "id")?;
        if id.trim().is_empty() {
            return Err("Invalid Responses response ID".into());
        }
        let model = common::string(response, "model")?;
        if model.trim().is_empty() {
            return Err("Invalid Responses model identity".into());
        }
        if common::string(response, "status")? != "in_progress" {
            return Err("Invalid initial Responses status".into());
        }
        let output = response
            .get("output")
            .and_then(Value::as_array)
            .ok_or("Missing initial Responses output")?;
        if !output.is_empty() {
            return Err("Initial Responses output must be empty".into());
        }
        self.stable_identity(response)?;
        self.created = true;
        Ok(())
    }

    fn stable_identity(&mut self, response: &Value) -> Result<()> {
        common::stable(&mut self.message.response_id, response, "id")?;
        common::stable(&mut self.message.response_model, response, "model")
    }

    fn output_item_added(&mut self, value: &Value, events: &mut Vec<AssistantEvent>) -> Result<()> {
        self.require_created()?;
        let index = usize_field(value, "output_index")?;
        if index != self.items.len() {
            return Err("Responses output items must be added in order".into());
        }
        let item = value.get("item").ok_or("Missing Responses output item")?;
        common::object(item)?;
        let id = common::string(item, "id")?;
        if id.trim().is_empty() {
            return Err("Invalid Responses output item ID".into());
        }
        if self.items.iter().any(|item| item.id == id) {
            return Err("Duplicate Responses output item ID".into());
        }
        match common::string(item, "type")? {
            "message" => {
                if common::string(item, "role")? != "assistant" {
                    return Err("Invalid Responses message role".into());
                }
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or("Missing Responses message content")?;
                if !content.is_empty() {
                    return Err("An in-progress Responses message must have empty content".into());
                }
                self.items.push(Item {
                    id: id.to_owned(),
                    done: None,
                    kind: ItemKind::Message { parts: Vec::new() },
                });
            }
            "function_call" => {
                let call_id = common::string(item, "call_id")?;
                if call_id.trim().is_empty() || !self.seen_calls.insert(call_id.to_owned()) {
                    return Err("Invalid or duplicate Responses function call ID".into());
                }
                let name = common::string(item, "name")?;
                if !self.tools.contains_key(name) {
                    return Err("Response calls an undeclared tool".into());
                }
                match item.get("arguments") {
                    None => {}
                    Some(Value::String(arguments)) if arguments.is_empty() => {}
                    _ => {
                        return Err(
                            "Partial function arguments in an added item are unsupported".into(),
                        );
                    }
                }
                self.limits.tool_call()?;
                let content_index = self.message.content.len();
                self.message.content.push(Content::ToolCall(ToolCall::new(
                    call_id,
                    name,
                    json!({}),
                )));
                events.push(AssistantEvent::ToolCallStart {
                    index: content_index,
                    partial: self.message.clone(),
                });
                self.items.push(Item {
                    id: id.to_owned(),
                    done: None,
                    kind: ItemKind::FunctionCall {
                        call_id: call_id.to_owned(),
                        name: name.to_owned(),
                        buffer: String::new(),
                        content_index,
                        arguments_done: false,
                    },
                });
            }
            "reasoning" => self.items.push(Item {
                id: id.to_owned(),
                done: None,
                kind: ItemKind::Reasoning {
                    parts: Vec::new(),
                    unannounced_text: false,
                },
            }),
            _ => return Err("Unsupported Responses output item type".into()),
        }
        Ok(())
    }

    fn content_part_added(
        &mut self,
        value: &Value,
        events: &mut Vec<AssistantEvent>,
    ) -> Result<()> {
        self.require_created()?;
        let index = usize_field(value, "output_index")?;
        let item_id = common::string(value, "item_id")?;
        let part_index = usize_field(value, "content_index")?;
        let part = value.get("part").ok_or("Missing Responses content part")?;
        common::object(part)?;
        let part_type = common::string(part, "type")?;
        let text = common::string(part, "text")?;
        let item = self
            .items
            .get_mut(index)
            .ok_or("Responses content part references an unknown output item")?;
        if item.id != item_id {
            return Err("Responses content part item ID does not match its output index".into());
        }
        match &mut item.kind {
            ItemKind::Message { parts } => {
                if part_type != "output_text" {
                    return Err("Unsupported Responses content part for a message item".into());
                }
                if part_index != parts.len() {
                    return Err("Responses content parts must be added in order".into());
                }
                self.limits.fragment()?;
                let content_index = self.message.content.len();
                self.message.content.push(Content::text(text));
                parts.push(TextPart {
                    content_index,
                    text: text.to_owned(),
                    text_done: false,
                    part_done: false,
                });
                events.push(AssistantEvent::TextStart {
                    index: content_index,
                    partial: self.message.clone(),
                });
            }
            ItemKind::Reasoning {
                parts,
                unannounced_text,
            } => {
                if part_type != "reasoning_text" {
                    return Err("Unsupported Responses content part for a reasoning item".into());
                }
                if *unannounced_text {
                    return Err(
                        "Responses reasoning content part was added after its text streamed".into(),
                    );
                }
                if part_index != parts.len() {
                    return Err("Responses reasoning content parts must be added in order".into());
                }
                self.limits.fragment()?;
                parts.push(ReasoningPart {
                    text: text.to_owned(),
                    text_done: false,
                    part_done: false,
                });
            }
            ItemKind::FunctionCall { .. } => {
                return Err("Responses content part belongs to a function call item".into());
            }
        }
        Ok(())
    }

    fn output_text_delta(&mut self, value: &Value, events: &mut Vec<AssistantEvent>) -> Result<()> {
        self.require_created()?;
        let index = usize_field(value, "output_index")?;
        let item_id = common::string(value, "item_id")?;
        let part_index = usize_field(value, "content_index")?;
        let delta = common::string(value, "delta")?;
        let content_index = {
            let item = self
                .items
                .get_mut(index)
                .ok_or("Responses text delta references an unknown output item")?;
            if item.id != item_id {
                return Err("Responses text delta item ID does not match its output index".into());
            }
            let ItemKind::Message { parts } = &mut item.kind else {
                return Err("Responses text delta belongs to a non-message item".into());
            };
            let part = parts
                .get_mut(part_index)
                .ok_or("Responses text delta references an unknown content part")?;
            if part.text_done || part.part_done {
                return Err("Responses text delta arrived after its done event".into());
            }
            part.text.push_str(delta);
            part.content_index
        };
        self.limits.fragment()?;
        events.push(common::text_delta(&mut self.message, content_index, delta));
        Ok(())
    }

    fn output_text_done(&mut self, value: &Value) -> Result<()> {
        self.require_created()?;
        let item = self.message_item(value, "text done")?;
        let part_index = usize_field(value, "content_index")?;
        let text = common::string(value, "text")?;
        let ItemKind::Message { parts } = &mut item.kind else {
            unreachable!("checked message item");
        };
        let part = parts
            .get_mut(part_index)
            .ok_or("Responses text done references an unknown content part")?;
        if part.text_done || part.part_done {
            return Err("Duplicate Responses text done event".into());
        }
        if part.text != text {
            return Err("Responses text done event does not match its deltas".into());
        }
        part.text_done = true;
        Ok(())
    }

    fn reasoning_text_delta(&mut self, value: &Value) -> Result<()> {
        self.require_created()?;
        let index = usize_field(value, "output_index")?;
        let item_id = common::string(value, "item_id")?;
        let part_index = usize_field(value, "content_index")?;
        let delta = common::string(value, "delta")?;
        let item = self
            .items
            .get_mut(index)
            .ok_or("Responses reasoning text delta references an unknown output item")?;
        if item.id != item_id {
            return Err(
                "Responses reasoning text delta item ID does not match its output index".into(),
            );
        }
        let ItemKind::Reasoning {
            parts,
            unannounced_text,
        } = &mut item.kind
        else {
            return Err("Responses reasoning text delta belongs to a non-reasoning item".into());
        };
        if let Some(part) = parts.get_mut(part_index) {
            if part.text_done || part.part_done {
                return Err("Responses reasoning text delta arrived after its done event".into());
            }
            part.text.push_str(delta);
        } else if parts.is_empty() {
            *unannounced_text = true;
        } else {
            return Err("Responses reasoning text delta references an unknown content part".into());
        }
        Ok(())
    }

    fn reasoning_text_done(&mut self, value: &Value) -> Result<()> {
        self.require_created()?;
        let index = usize_field(value, "output_index")?;
        let item_id = common::string(value, "item_id")?;
        let part_index = usize_field(value, "content_index")?;
        let text = common::string(value, "text")?;
        let item = self
            .items
            .get_mut(index)
            .ok_or("Responses reasoning text done references an unknown output item")?;
        if item.id != item_id {
            return Err(
                "Responses reasoning text done item ID does not match its output index".into(),
            );
        }
        let ItemKind::Reasoning {
            parts,
            unannounced_text,
        } = &mut item.kind
        else {
            return Err("Responses reasoning text done belongs to a non-reasoning item".into());
        };
        if let Some(part) = parts.get_mut(part_index) {
            if part.text_done || part.part_done {
                return Err("Duplicate Responses reasoning text done event".into());
            }
            if part.text != text {
                return Err("Responses reasoning text done event does not match its deltas".into());
            }
            part.text_done = true;
        } else if parts.is_empty() {
            *unannounced_text = true;
        } else {
            return Err("Responses reasoning text done references an unknown content part".into());
        }
        Ok(())
    }

    fn content_part_done(&mut self, value: &Value, events: &mut Vec<AssistantEvent>) -> Result<()> {
        self.require_created()?;
        let index = usize_field(value, "output_index")?;
        let item_id = common::string(value, "item_id")?;
        let part_index = usize_field(value, "content_index")?;
        let part = value.get("part").ok_or("Missing Responses content part")?;
        common::object(part)?;
        let part_type = common::string(part, "type")?;
        let text = common::string(part, "text")?;
        let mut text_end = None;
        {
            let item = self
                .items
                .get_mut(index)
                .ok_or("Responses content part done references an unknown output item")?;
            if item.id != item_id {
                return Err(
                    "Responses content part done item ID does not match its output index".into(),
                );
            }
            match &mut item.kind {
                ItemKind::Message { parts } => {
                    if part_type != "output_text" {
                        return Err("Unsupported Responses content part for a message item".into());
                    }
                    let part_state = parts
                        .get_mut(part_index)
                        .ok_or("Responses content part done references an unknown content part")?;
                    if part_state.part_done {
                        return Err("Duplicate Responses content part done event".into());
                    }
                    if !part_state.text_done {
                        return Err(
                            "Responses content part done arrived before its text done".into()
                        );
                    }
                    if part_state.text != text {
                        return Err("Responses content part done does not match its deltas".into());
                    }
                    part_state.part_done = true;
                    text_end = Some(part_state.content_index);
                }
                ItemKind::Reasoning { parts, .. } => {
                    if part_type != "reasoning_text" {
                        return Err(
                            "Unsupported Responses content part for a reasoning item".into()
                        );
                    }
                    let part_state = parts.get_mut(part_index).ok_or(
                        "Responses reasoning content part done references an unknown content part",
                    )?;
                    if part_state.part_done {
                        return Err("Duplicate Responses reasoning content part done event".into());
                    }
                    if part_state.text != text {
                        return Err(
                            "Responses reasoning content part done does not match its deltas"
                                .into(),
                        );
                    }
                    part_state.part_done = true;
                }
                ItemKind::FunctionCall { .. } => {
                    return Err(
                        "Responses content part done belongs to a function call item".into(),
                    );
                }
            }
        }
        if let Some(index) = text_end {
            events.push(AssistantEvent::TextEnd {
                index,
                partial: self.message.clone(),
            });
        }
        Ok(())
    }

    fn arguments_delta(&mut self, value: &Value, events: &mut Vec<AssistantEvent>) -> Result<()> {
        self.require_created()?;
        let index = usize_field(value, "output_index")?;
        let item_id = common::string(value, "item_id")?;
        let delta = common::string(value, "delta")?;
        let (content_index, partial) = {
            let item = self
                .items
                .get_mut(index)
                .ok_or("Responses argument delta references an unknown output item")?;
            if item.id != item_id {
                return Err(
                    "Responses argument delta item ID does not match its output index".into(),
                );
            }
            let ItemKind::FunctionCall {
                buffer,
                content_index,
                arguments_done,
                ..
            } = &mut item.kind
            else {
                return Err("Responses argument delta belongs to a non-call item".into());
            };
            if *arguments_done {
                return Err("Responses argument delta arrived after its done event".into());
            }
            if buffer.len().saturating_add(delta.len()) > MAX_ARGUMENT_BYTES {
                return Err("Responses tool arguments exceed the size limit".into());
            }
            buffer.push_str(delta);
            (*content_index, self.message.clone())
        };
        self.limits.fragment()?;
        events.push(AssistantEvent::ToolCallDelta {
            index: content_index,
            delta: delta.to_owned(),
            partial,
        });
        Ok(())
    }

    fn arguments_done(&mut self, value: &Value) -> Result<()> {
        self.require_created()?;
        let index = usize_field(value, "output_index")?;
        let item_id = common::string(value, "item_id")?;
        let arguments = common::string(value, "arguments")?;
        let item = self
            .items
            .get_mut(index)
            .ok_or("Responses argument done references an unknown output item")?;
        if item.id != item_id {
            return Err("Responses argument done item ID does not match its output index".into());
        }
        let ItemKind::FunctionCall {
            buffer,
            arguments_done,
            ..
        } = &mut item.kind
        else {
            return Err("Responses argument done belongs to a non-call item".into());
        };
        if *arguments_done {
            return Err("Duplicate Responses argument done event".into());
        }
        if buffer != arguments {
            return Err("Responses argument done does not match its deltas".into());
        }
        *arguments_done = true;
        Ok(())
    }

    fn output_item_done(&mut self, value: &Value, events: &mut Vec<AssistantEvent>) -> Result<()> {
        self.require_created()?;
        let index = usize_field(value, "output_index")?;
        let item_value = value.get("item").ok_or("Missing Responses done item")?;
        common::object(item_value)?;
        let id = common::string(item_value, "id")?;
        let item_type = common::string(item_value, "type")?;
        let item = self
            .items
            .get_mut(index)
            .ok_or("Responses done item references an unknown output index")?;
        if item.id != id {
            return Err("Responses done item ID does not match its output index".into());
        }
        if item.done.is_some() {
            return Err("Duplicate Responses output item done event".into());
        }
        let mut ended_call = None;
        match (&item.kind, item_type) {
            (ItemKind::Message { parts }, "message") => {
                if common::string(item_value, "role")? != "assistant" {
                    return Err("Invalid Responses done message role".into());
                }
                let content = item_value
                    .get("content")
                    .and_then(Value::as_array)
                    .filter(|content| !content.is_empty())
                    .ok_or("Invalid Responses done message content")?;
                if content.len() != parts.len() {
                    return Err("Responses done message content does not match its parts".into());
                }
                for (part, raw) in parts.iter().zip(content) {
                    common::object(raw)?;
                    if common::string(raw, "type")? != "output_text" {
                        return Err("Unsupported Responses done message part".into());
                    }
                    if !part.text_done || !part.part_done {
                        return Err("Responses message item ended before its content part".into());
                    }
                    if common::string(raw, "text")? != part.text {
                        return Err("Responses done message text does not match its deltas".into());
                    }
                }
            }
            (
                ItemKind::FunctionCall {
                    call_id,
                    name,
                    buffer,
                    content_index,
                    arguments_done,
                },
                "function_call",
            ) => {
                if !*arguments_done {
                    return Err("Responses call item ended before its argument done event".into());
                }
                if common::string(item_value, "call_id")? != call_id
                    || common::string(item_value, "name")? != name
                    || common::string(item_value, "arguments")? != buffer
                {
                    return Err("Responses done call does not match its deltas".into());
                }
                let arguments = parse_arguments(buffer)?;
                let call = ToolCall::new(call_id.clone(), name.clone(), arguments);
                common::validate_call(&call, &self.tools)?;
                ended_call = Some((*content_index, call));
            }
            (ItemKind::Reasoning { parts, .. }, "reasoning") => {
                item_value
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|encrypted| !encrypted.is_empty())
                    .ok_or("Responses reasoning item has no encrypted content")?;
                let content = item_value.get("content");
                if parts.is_empty() {
                    if let Some(content) = content {
                        let content = content
                            .as_array()
                            .ok_or("Invalid Responses done reasoning content")?;
                        for raw in content {
                            common::object(raw)?;
                            if common::string(raw, "type")? != "reasoning_text" {
                                return Err("Unsupported Responses done reasoning part".into());
                            }
                            common::string(raw, "text")?;
                        }
                    }
                } else {
                    let content = content
                        .and_then(Value::as_array)
                        .ok_or("Invalid Responses done reasoning content")?;
                    if content.len() != parts.len() {
                        return Err(
                            "Responses done reasoning content does not match its parts".into()
                        );
                    }
                    for (part, raw) in parts.iter().zip(content) {
                        common::object(raw)?;
                        if common::string(raw, "type")? != "reasoning_text" {
                            return Err("Unsupported Responses done reasoning part".into());
                        }
                        if !part.part_done {
                            return Err(
                                "Responses reasoning item ended before its content part".into()
                            );
                        }
                        if common::string(raw, "text")? != part.text {
                            return Err(
                                "Responses done reasoning text does not match its deltas".into()
                            );
                        }
                    }
                }
            }
            _ => return Err("Responses done item type does not match its added item".into()),
        }
        item.done = Some(item_value.clone());
        if let Some((content_index, call)) = ended_call {
            match self.message.content.get(content_index) {
                Some(Content::ToolCall(placeholder))
                    if placeholder.id == call.id && placeholder.name == call.name => {}
                _ => return Err("Responses call does not match its normalized placeholder".into()),
            }
            self.message.content[content_index] = Content::ToolCall(call);
            events.push(AssistantEvent::ToolCallEnd {
                index: content_index,
                partial: self.message.clone(),
            });
        }
        Ok(())
    }

    fn terminal_response(&mut self, value: &Value, status: &str) -> Result<()> {
        self.require_created()?;
        let response = response(value)?;
        if common::string(response, "status")? != status {
            return Err("Responses terminal status does not match its event".into());
        }
        self.stable_identity(response)?;
        if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
            self.usage(usage)?;
        }
        Ok(())
    }

    fn completed(&mut self, value: &Value, events: &mut Vec<AssistantEvent>) -> Result<()> {
        if self.completed {
            return Err("Duplicate response.completed event".into());
        }
        self.require_created()?;
        let response = response(value)?;
        if common::string(response, "status")? != "completed" {
            return Err("Responses completed status is invalid".into());
        }
        if response.get("error").is_some_and(|error| !error.is_null()) {
            return Err("Responses completed event carries an error".into());
        }
        self.stable_identity(response)?;
        if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
            self.usage(usage)?;
            events.push(AssistantEvent::UsageUpdate {
                partial: self.message.clone(),
            });
        }
        let output = response
            .get("output")
            .and_then(Value::as_array)
            .ok_or("Missing Responses completed output")?;
        if output.len() != self.items.len() {
            return Err("Responses completed output does not match its items".into());
        }
        let mut normalized = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            let done = item
                .done
                .as_ref()
                .ok_or("Responses output item has no done event")?;
            if done != &output[index] {
                return Err("Responses completed output does not match item done events".into());
            }
            match &item.kind {
                ItemKind::Message { parts } => {
                    for part in parts {
                        normalized.push(Content::text(part.text.clone()));
                    }
                }
                ItemKind::FunctionCall {
                    call_id,
                    name,
                    buffer,
                    arguments_done,
                    ..
                } => {
                    if !arguments_done {
                        return Err("Responses call has no argument done event".into());
                    }
                    normalized.push(Content::ToolCall(ToolCall::new(
                        call_id.clone(),
                        name.clone(),
                        parse_arguments(buffer)?,
                    )));
                }
                ItemKind::Reasoning { .. } => {}
            }
        }
        if normalized.is_empty() || normalized != self.message.content {
            return Err("Responses completed output does not match normalized content".into());
        }
        let has_calls = normalized
            .iter()
            .any(|part| matches!(part, Content::ToolCall(_)));
        self.message.stop_reason = if has_calls {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        self.message
            .provider_metadata
            .insert("openai_responses".into(), json!({"output": output.clone()}));
        self.completed = true;
        Ok(())
    }

    fn message_item<'a>(&'a mut self, value: &Value, what: &str) -> Result<&'a mut Item> {
        let index = usize_field(value, "output_index")?;
        let item_id = common::string(value, "item_id")?;
        let item = self
            .items
            .get_mut(index)
            .ok_or_else(|| format!("Responses {what} references an unknown output item"))?;
        if item.id != item_id {
            return Err(format!(
                "Responses {what} item ID does not match its output index"
            ));
        }
        if !matches!(&item.kind, ItemKind::Message { .. }) {
            return Err(format!("Responses {what} belongs to a non-message item"));
        }
        Ok(item)
    }

    fn usage(&mut self, usage: &Value) -> Result<()> {
        let fields = usage.as_object().ok_or("Invalid Responses usage report")?;
        self.message.usage_fields = fields
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        self.message.usage_status = UsageStatus::Invalid;
        let input = counter(fields, "input_tokens")?;
        let output = counter(fields, "output_tokens")?;
        let total = counter(fields, "total_tokens")?;
        let cached = detail_counter(fields, "input_tokens_details", "cached_tokens")?;
        let reasoning = detail_counter(fields, "output_tokens_details", "reasoning_tokens")?;
        let cache_write = detail_counter(fields, "input_tokens_details", "cache_write_tokens")?;
        if cached.is_some() && input.is_none() {
            return Err("Responses cached tokens have no input total".into());
        }
        if cache_write.is_some() && input.is_none() {
            return Err("Responses cache write tokens have no input total".into());
        }
        if reasoning.is_some() && output.is_none() {
            return Err("Responses reasoning tokens have no output total".into());
        }
        let cache_total = cached
            .unwrap_or(0)
            .checked_add(cache_write.unwrap_or(0))
            .ok_or("Responses cache usage overflows")?;
        if let Some(input) = input
            && cache_total > input
        {
            return Err("Responses cached tokens exceed input tokens".into());
        }
        if let (Some(reasoning), Some(output)) = (reasoning, output)
            && reasoning > output
        {
            return Err("Responses reasoning tokens exceed output tokens".into());
        }
        let calculated = match (input, output) {
            (Some(input), Some(output)) => Some(
                input
                    .checked_add(output)
                    .ok_or("Responses usage total overflows")?,
            ),
            _ => None,
        };
        if let (Some(calculated), Some(total)) = (calculated, total)
            && total != calculated
        {
            return Err("Responses total usage contradicts its counters".into());
        }
        self.message.usage = Usage {
            input: input.map_or(0, |input| input - cache_total),
            output: output.unwrap_or(0),
            cache_read: cached.unwrap_or(0),
            cache_write: cache_write.unwrap_or(0),
            reasoning: reasoning.unwrap_or(0),
            total_tokens: total.or(calculated).unwrap_or(0),
        };
        self.message.usage_status = if fields.is_empty() {
            UsageStatus::Unknown
        } else if input.is_some() && output.is_some() {
            UsageStatus::Reported
        } else {
            UsageStatus::Partial
        };
        Ok(())
    }
}

fn response(value: &Value) -> Result<&Value> {
    let response = value
        .get("response")
        .ok_or("Missing Responses response object")?;
    common::object(response)?;
    Ok(response)
}

fn usize_field(value: &Value, field: &str) -> Result<usize> {
    let raw = value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or("Missing or invalid Responses event index")?;
    usize::try_from(raw).map_err(|_| "Responses event index is too large".into())
}

fn counter(fields: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u64>> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| "Invalid Responses usage counter".into()),
    }
}

fn detail_counter(
    fields: &serde_json::Map<String, Value>,
    object: &str,
    key: &str,
) -> Result<Option<u64>> {
    match fields.get(object) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let nested = value.as_object().ok_or("Invalid Responses usage details")?;
            match nested.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(value) => value
                    .as_u64()
                    .map(Some)
                    .ok_or_else(|| "Invalid Responses usage detail counter".into()),
            }
        }
    }
}

fn parse_arguments(text: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(text)
        .map_err(|_| "Invalid JSON in Responses tool arguments".to_owned())?;
    ensure_no_duplicate_keys(text.as_bytes())?;
    if !value.is_object() {
        return Err("Responses tool arguments must be a JSON object".into());
    }
    Ok(value)
}

// serde_json keeps the last duplicate field instead of rejecting it. This
// bounded scanner checks object keys after syntax validation, including escaped
// keys and nested objects, without adding another dependency.
fn ensure_no_duplicate_keys(input: &[u8]) -> Result<()> {
    let mut index = 0;
    skip_ws(input, &mut index);
    scan_json_value(input, &mut index, 0)?;
    skip_ws(input, &mut index);
    if index != input.len() {
        return Err("Invalid JSON in Responses tool arguments".into());
    }
    Ok(())
}

fn skip_ws(input: &[u8], index: &mut usize) {
    while let Some(byte) = input.get(*index)
        && matches!(*byte, b' ' | b'\t' | b'\n' | b'\r')
    {
        *index += 1;
    }
}

fn scan_json_value(input: &[u8], index: &mut usize, depth: usize) -> Result<()> {
    if depth > MAX_REPLAY_DEPTH {
        return Err("Responses tool arguments exceed the nesting limit".into());
    }
    skip_ws(input, index);
    match input.get(*index) {
        Some(b'{') => {
            *index += 1;
            skip_ws(input, index);
            if input.get(*index) == Some(&b'}') {
                *index += 1;
                return Ok(());
            }
            let mut keys = BTreeSet::new();
            loop {
                skip_ws(input, index);
                if input.get(*index) != Some(&b'"') {
                    return Err("Invalid JSON in Responses tool arguments".into());
                }
                let key = scan_json_string(input, index)?;
                if !keys.insert(key) {
                    return Err("Duplicate JSON key in Responses tool arguments".into());
                }
                skip_ws(input, index);
                if input.get(*index) != Some(&b':') {
                    return Err("Invalid JSON in Responses tool arguments".into());
                }
                *index += 1;
                scan_json_value(input, index, depth + 1)?;
                skip_ws(input, index);
                match input.get(*index) {
                    Some(b',') => *index += 1,
                    Some(b'}') => {
                        *index += 1;
                        break;
                    }
                    _ => return Err("Invalid JSON in Responses tool arguments".into()),
                }
            }
        }
        Some(b'[') => {
            *index += 1;
            skip_ws(input, index);
            if input.get(*index) == Some(&b']') {
                *index += 1;
                return Ok(());
            }
            loop {
                scan_json_value(input, index, depth + 1)?;
                skip_ws(input, index);
                match input.get(*index) {
                    Some(b',') => *index += 1,
                    Some(b']') => {
                        *index += 1;
                        break;
                    }
                    _ => return Err("Invalid JSON in Responses tool arguments".into()),
                }
            }
        }
        Some(b'"') => {
            scan_json_string(input, index)?;
        }
        Some(_) => {
            let start = *index;
            while let Some(byte) = input.get(*index) {
                if matches!(*byte, b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r') {
                    break;
                }
                *index += 1;
            }
            if *index == start {
                return Err("Invalid JSON in Responses tool arguments".into());
            }
        }
        None => return Err("Invalid JSON in Responses tool arguments".into()),
    }
    Ok(())
}

fn scan_json_string(input: &[u8], index: &mut usize) -> Result<String> {
    if input.get(*index) != Some(&b'"') {
        return Err("Invalid JSON in Responses tool arguments".into());
    }
    *index += 1;
    let mut out = String::new();
    let mut chunk = *index;
    while let Some(&byte) = input.get(*index) {
        match byte {
            b'"' => {
                out.push_str(
                    std::str::from_utf8(&input[chunk..*index])
                        .map_err(|_| "Invalid UTF-8 in Responses tool arguments")?,
                );
                *index += 1;
                return Ok(out);
            }
            b'\\' => {
                out.push_str(
                    std::str::from_utf8(&input[chunk..*index])
                        .map_err(|_| "Invalid UTF-8 in Responses tool arguments")?,
                );
                *index += 1;
                let escape = *input
                    .get(*index)
                    .ok_or("Invalid JSON escape in Responses tool arguments")?;
                *index += 1;
                match escape {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = scan_hex4(input, index)?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            if input.get(*index) != Some(&b'\\')
                                || input.get(*index + 1) != Some(&b'u')
                            {
                                return Err(
                                    "Invalid JSON surrogate pair in Responses tool arguments"
                                        .into(),
                                );
                            }
                            *index += 2;
                            let low = scan_hex4(input, index)?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err(
                                    "Invalid JSON surrogate pair in Responses tool arguments"
                                        .into(),
                                );
                            }
                            let code =
                                0x10000 + (((code - 0xD800) as u32) << 10) + (low - 0xDC00) as u32;
                            out.push(
                                char::from_u32(code)
                                    .ok_or("Invalid JSON code point in Responses tool arguments")?,
                            );
                        } else if (0xDC00..=0xDFFF).contains(&code) {
                            return Err(
                                "Invalid JSON surrogate pair in Responses tool arguments".into()
                            );
                        } else {
                            out.push(
                                char::from_u32(code as u32)
                                    .ok_or("Invalid JSON code point in Responses tool arguments")?,
                            );
                        }
                    }
                    _ => return Err("Invalid JSON escape in Responses tool arguments".into()),
                }
                chunk = *index;
            }
            _ => *index += 1,
        }
    }
    Err("Invalid JSON in Responses tool arguments".into())
}

fn scan_hex4(input: &[u8], index: &mut usize) -> Result<u16> {
    let end = index
        .checked_add(4)
        .ok_or("Invalid JSON escape in Responses tool arguments")?;
    let digits = input
        .get(*index..end)
        .ok_or("Invalid JSON escape in Responses tool arguments")?;
    let digits = std::str::from_utf8(digits)
        .map_err(|_| "Invalid JSON escape in Responses tool arguments")?;
    let value = u16::from_str_radix(digits, 16)
        .map_err(|_| "Invalid JSON escape in Responses tool arguments")?;
    *index = end;
    Ok(value)
}
