use crate::common::{self, Format, Result};
use cayw_ai::{
    AssistantEvent, AssistantMessage, CancellationToken, Content, Message, ModelRequest,
    StopReason, ToolCall, ToolDefinition, current_system,
};
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

static RESPONSE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(crate) fn request_body(request: &ModelRequest) -> Result<Value> {
    let mut prepared = common::prepare_request(request, Format::Gemini)?;
    let system = current_system(&request.messages);
    let mut contents = Vec::new();
    let mut pending = Vec::new();
    let mut outputs = BTreeMap::new();
    for (idx, message) in request.messages.iter().enumerate() {
        match message {
            Message::System(_) | Message::Custom { .. } => {}
            Message::User { .. } => {
                let parts = prepared
                    .user_parts
                    .remove(&idx)
                    .expect("validated user parts");
                common::append_turn(&mut contents, "user", "parts", parts);
            }
            Message::Assistant(assistant) => {
                let parts = history_parts(assistant)?;
                pending = assistant
                    .tool_calls()
                    .into_iter()
                    .zip(parts.iter().filter_map(|part| part.get("functionCall")))
                    .map(|(call, function)| {
                        (
                            call.id,
                            function
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        )
                    })
                    .collect();
                common::append_turn(&mut contents, "model", "parts", parts);
            }
            Message::ToolResult(result) => {
                let text = cayw_ai::content_text(&result.content);
                let output = result
                    .structured_content
                    .clone()
                    .unwrap_or_else(|| json!(text));
                let mut response = if result.is_error {
                    json!({"error":output})
                } else {
                    json!({"output":output})
                };
                if result.structured_content.is_some() && !text.is_empty() {
                    response["text"] = text.into();
                }
                outputs.insert(
                    result.tool_call_id.clone(),
                    json!({"name":result.tool_name,"response":response}),
                );
                if outputs.len() == pending.len() {
                    let parts = pending
                        .drain(..)
                        .map(|(id, wire_id)| {
                            let mut response = outputs.remove(&id).expect("validated tool pairing");
                            if let Some(wire_id) = wire_id {
                                response["id"] = wire_id.into();
                            }
                            json!({"functionResponse":response})
                        })
                        .collect();
                    common::append_turn(&mut contents, "user", "parts", parts);
                }
            }
        }
    }
    if contents.first().is_none_or(|m| m["role"] != "user") {
        return Err("Gemini history must start with a user turn".into());
    }
    let mut body = json!({"contents":contents,"generationConfig":{}});
    if !system.prompt.is_empty() {
        body["systemInstruction"] = json!({"parts":[{"text":system.prompt}]});
    }
    if !system.tools.is_empty() {
        body["tools"] = json!([{"functionDeclarations":system.tools.iter().map(|t| json!({"name":t.name,"description":t.description,"parametersJsonSchema":t.parameters})).collect::<Vec<_>>()}]);
    }
    if let Some(max) = request
        .options
        .max_tokens
        .or(request.model.max_output_tokens)
    {
        body["generationConfig"]["maxOutputTokens"] = max.into();
    }
    if let Some(t) = request.options.temperature {
        body["generationConfig"]["temperature"] = json!(t);
    }
    Ok(body)
}

// Signed parts must retain their wire boundaries. The local call IDs pair tool
// results without inventing provider IDs for calls that omitted them.
fn history_parts(assistant: &AssistantMessage) -> Result<Vec<Value>> {
    if assistant.provider_metadata.is_empty() {
        return assistant
            .content
            .iter()
            .map(|part| match part {
                Content::Text { text } => Ok(json!({"text":text})),
                Content::ToolCall(_) => {
                    Err("Gemini tool history requires its original response parts".to_owned())
                }
                _ => Err("Unsupported Gemini assistant history".to_owned()),
            })
            .collect();
    }
    if assistant.provider_metadata.len() != 1 {
        return Err("Unsupported Gemini history metadata".into());
    }
    let metadata = assistant
        .provider_metadata
        .get("gemini")
        .ok_or("Unsupported Gemini history metadata")?;
    let fields = common::object(metadata)?;
    if fields.len() != 2 || !fields.contains_key("parts") || !fields.contains_key("call_ids") {
        return Err("Invalid Gemini history metadata fields".into());
    }
    let parts = metadata["parts"]
        .as_array()
        .filter(|parts| !parts.is_empty())
        .ok_or("Invalid Gemini history parts")?;
    let call_ids = metadata["call_ids"]
        .as_array()
        .ok_or("Invalid Gemini history call IDs")?;
    let mut normalized = Vec::new();
    let mut text_index = None;
    let mut call_index = 0;
    let mut limits = common::ResponseLimits::default();
    for part in parts {
        limits.fragment()?;
        validate_part(part)?;
        if let Some(text) = part.get("text") {
            let text = text.as_str().expect("validated text");
            let index = *text_index.get_or_insert_with(|| {
                normalized.push(Content::text(""));
                normalized.len() - 1
            });
            if let Content::Text { text: previous } = &mut normalized[index] {
                previous.push_str(text);
            }
        } else {
            limits.tool_call()?;
            text_index = None;
            let function = &part["functionCall"];
            let id = call_ids
                .get(call_index)
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or("Missing or invalid Gemini history call ID")?;
            if function
                .get("id")
                .is_some_and(|wire_id| wire_id.as_str() != Some(id))
            {
                return Err("Gemini history call ID does not match its original part".into());
            }
            normalized.push(Content::ToolCall(ToolCall::new(
                id,
                common::string(function, "name")?,
                function.get("args").cloned().unwrap_or_else(|| json!({})),
            )));
            call_index += 1;
        }
    }
    if call_index != call_ids.len() || normalized != assistant.content {
        return Err("Gemini history metadata does not match assistant content".into());
    }
    Ok(parts.clone())
}

fn validate_part(part: &Value) -> Result<()> {
    let fields = common::object(part)?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "text" | "functionCall" | "thought" | "thoughtSignature"
        )
    }) {
        return Err("Unsupported Gemini response part".into());
    }
    if part
        .get("thought")
        .is_some_and(|value| value.as_bool() != Some(false))
    {
        return Err("Gemini thought content cannot be represented safely by this adapter".into());
    }
    if let Some(signature) = part.get("thoughtSignature") {
        let signature = signature
            .as_str()
            .filter(|signature| !signature.is_empty())
            .ok_or("Invalid Gemini thought signature")?;
        // Signatures are opaque base64 bytes. Validate the representation only;
        // authenticating the encrypted contents remains the provider's job.
        if !common::validate_standard_base64(signature) {
            return Err("Invalid Gemini thought signature".into());
        }
    }
    match (part.get("text"), part.get("functionCall")) {
        (Some(text), None) if text.is_string() => Ok(()),
        (None, Some(function)) => {
            let fields = common::object(function)?;
            if fields
                .keys()
                .any(|key| !matches!(key.as_str(), "id" | "name" | "args"))
            {
                return Err(
                    "Partial or nonstandard Gemini function arguments are unsupported".into(),
                );
            }
            common::string(function, "name")?;
            if function
                .get("id")
                .is_some_and(|id| id.as_str().is_none_or(|id| id.trim().is_empty()))
                || function.get("args").is_some_and(|args| !args.is_object())
            {
                return Err("Invalid Gemini function call ID or arguments".into());
            }
            Ok(())
        }
        _ => Err("Gemini part must contain text or a complete functionCall".into()),
    }
}

pub(crate) struct State {
    pub message: AssistantMessage,
    tools: BTreeMap<String, ToolDefinition>,
    seen_calls: BTreeSet<String>,
    prefix: u64,
    call_sequence: u64,
    text_index: Option<usize>,
    finish: Option<StopReason>,
    usage: common::UsageReport,
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
            seen_calls,
            prefix: RESPONSE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            call_sequence: 0,
            text_index: None,
            finish: None,
            usage: common::UsageReport::default(),
            limits: common::ResponseLimits::default(),
        }
    }

    /// Each poll produces at most one snapshot. The frame is retained as JSON,
    /// never as a collection of cumulative AssistantMessage copies.
    pub fn accept<'a>(
        &'a mut self,
        value: Value,
        cancel: &'a CancellationToken,
    ) -> BoxStream<'a, Result<AssistantEvent>> {
        async_stream::try_stream! {
            common::checkpoint(cancel).await?;
            common::object(&value)?;
            if value.get("error").is_some_and(|e| !e.is_null()) {
                Err("Gemini returned an API error".to_owned())?;
            }
            if let Some(feedback) = value.get("promptFeedback") {
                common::object(feedback)?;
                if feedback.get("blockReason").is_some_and(|v| !v.is_null() && v.as_str() != Some("BLOCK_REASON_UNSPECIFIED")) {
                    Err("Gemini blocked the prompt".to_owned())?;
                }
            }
            common::stable(&mut self.message.response_id, &value, "responseId")?;
            common::stable(&mut self.message.response_model, &value, "modelVersion")?;
            if let Some(usage) = value.get("usageMetadata").filter(|value| !value.is_null()) {
                self.usage(usage)?;
                yield AssistantEvent::UsageUpdate { partial: self.message.clone() };
            }
            let Some(candidates) = value.get("candidates") else {
                if value.get("usageMetadata").is_some() {
                    return;
                }
                Err("Gemini chunk has no candidates or usage".to_owned())?;
                unreachable!();
            };
            let candidates = candidates.as_array().ok_or("Invalid Gemini candidates")?;
            if candidates.is_empty() && value.get("usageMetadata").is_some() {
                return;
            }
            if candidates.len() != 1 {
                Err("Expected exactly one Gemini candidate".to_owned())?;
            }
            let candidate = &candidates[0];
            common::object(candidate)?;
            if let Some(index) = candidate.get("index") && index.as_u64() != Some(0) {
                Err("Unexpected Gemini candidate index".to_owned())?;
            }
            if self.finish.is_some() {
                Err("Gemini candidate arrived after finishReason".to_owned())?;
            }
            let mut has_parts = false;
            if let Some(content) = candidate.get("content") {
                common::object(content)?;
                if let Some(role) = content.get("role") && role.as_str() != Some("model") {
                    Err("Invalid Gemini response role".to_owned())?;
                }
                let parts = content.get("parts").and_then(Value::as_array).ok_or("Missing Gemini content parts")?;
                for part in parts {
                    common::checkpoint(cancel).await?;
                    self.limits.fragment()?;
                    validate_part(part)?;
                    match (part.get("text"), part.get("functionCall")) {
                        (Some(text), None) => {
                            self.remember_part(part, None);
                            let text = text.as_str().ok_or("Invalid Gemini text part")?;
                            let index = if let Some(index) = self.text_index {
                                index
                            } else {
                                let index = self.message.content.len();
                                self.message.content.push(Content::text(""));
                                self.text_index = Some(index);
                                yield AssistantEvent::TextStart { index, partial: self.message.clone() };
                                common::checkpoint(cancel).await?;
                                index
                            };
                            if !text.is_empty() {
                                yield common::text_delta(&mut self.message, index, text);
                            }
                        }
                        (None, Some(function)) => {
                            self.limits.tool_call()?;
                            if let Some(event) = self.end_text() {
                                yield event;
                                common::checkpoint(cancel).await?;
                            }
                            let name = common::string(function, "name")?;
                            let id = if let Some(id) = function.get("id") {
                                id.as_str().filter(|id| !id.trim().is_empty()).ok_or("Invalid Gemini function call ID")?.to_owned()
                            } else {
                                self.generated_id()
                            };
                            if !self.seen_calls.insert(id.clone()) {
                                Err("Duplicate Gemini function call ID".to_owned())?;
                            }
                            let arguments = function.get("args").cloned().unwrap_or_else(|| json!({}));
                            let call = ToolCall::new(id, name, arguments);
                            common::validate_call(&call, &self.tools)?;
                            common::check_cancel(cancel)?;
                            self.remember_part(part, Some(&call.id));
                            let index = self.message.content.len();
                            let delta = call.arguments.to_string();
                            self.message.content.push(Content::ToolCall(ToolCall::new(&call.id, &call.name, Value::Null)));
                            yield AssistantEvent::ToolCallStart { index, partial: self.message.clone() };
                            common::checkpoint(cancel).await?;
                            self.message.content[index] = Content::ToolCall(call);
                            yield AssistantEvent::ToolCallDelta { index, delta, partial: self.message.clone() };
                            common::checkpoint(cancel).await?;
                            yield AssistantEvent::ToolCallEnd { index, partial: self.message.clone() };
                        }
                        _ => Err("Gemini part must contain text or a complete functionCall".to_owned())?,
                    }
                    has_parts = true;
                }
            }
            common::checkpoint(cancel).await?;
            if let Some(reason) = candidate.get("finishReason").filter(|v| !v.is_null()) {
                let calls = self.message.content.iter().any(|part| matches!(part, Content::ToolCall(_)));
                let reason = match reason.as_str() {
                    Some("STOP") if calls => StopReason::ToolUse,
                    Some("STOP") => StopReason::Stop,
                    Some("MAX_TOKENS") if !calls => StopReason::Length,
                    _ => Err("Gemini returned an unsupported, blocked or incomplete finish reason".to_owned())?,
                };
                if let Some(event) = self.end_text() {
                    yield event;
                    common::checkpoint(cancel).await?;
                }
                self.finish = Some(reason);
                self.message.stop_reason = reason;
            } else if !has_parts {
                Err("Empty Gemini candidate has no finish reason".to_owned())?;
            }
        }.boxed()
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.finish.is_none() {
            return Err("Gemini response ended without finishReason".into());
        }
        if self.message.content.is_empty() {
            return Err("Gemini returned no supported content".into());
        }
        Ok(())
    }

    fn remember_part(&mut self, part: &Value, call_id: Option<&str>) {
        let metadata = self
            .message
            .provider_metadata
            .entry("gemini".to_owned())
            .or_insert_with(|| json!({"parts":[],"call_ids":[]}));
        metadata["parts"]
            .as_array_mut()
            .expect("constructed parts")
            .push(part.clone());
        if let Some(id) = call_id {
            metadata["call_ids"]
                .as_array_mut()
                .expect("constructed call IDs")
                .push(id.into());
        }
    }

    fn end_text(&mut self) -> Option<AssistantEvent> {
        self.text_index.take().map(|index| AssistantEvent::TextEnd {
            index,
            partial: self.message.clone(),
        })
    }

    fn generated_id(&mut self) -> String {
        loop {
            let id = format!("cayw_gemini_{}_{}", self.prefix, self.call_sequence);
            self.call_sequence = self.call_sequence.wrapping_add(1);
            if !self.seen_calls.contains(&id) {
                return id;
            }
        }
    }

    fn usage(&mut self, usage: &Value) -> Result<()> {
        self.usage.update(
            &mut self.message,
            usage,
            &[
                "promptTokenCount",
                "candidatesTokenCount",
                "cachedContentTokenCount",
                "thoughtsTokenCount",
                "toolUsePromptTokenCount",
                "totalTokenCount",
            ],
        )?;
        let prompt = self.usage.get("promptTokenCount");
        let candidates = self.usage.get("candidatesTokenCount").unwrap_or_default();
        self.message.usage.cache_read = self
            .usage
            .get("cachedContentTokenCount")
            .unwrap_or_default();
        self.message.usage.reasoning = self.usage.get("thoughtsTokenCount").unwrap_or_default();
        if prompt.is_some_and(|prompt| self.message.usage.cache_read > prompt) {
            return Err("Gemini cached tokens exceed prompt tokens".into());
        }
        let tool_prompt = self
            .usage
            .get("toolUsePromptTokenCount")
            .unwrap_or_default();
        self.message.usage.input = prompt
            .unwrap_or_default()
            .saturating_sub(self.message.usage.cache_read)
            .checked_add(tool_prompt)
            .ok_or("Gemini input usage overflows")?;
        self.message.usage.output = candidates
            .checked_add(self.message.usage.reasoning)
            .ok_or("Gemini output usage overflows")?;
        let calculated = prompt
            .unwrap_or_default()
            .checked_add(tool_prompt)
            .and_then(|total| total.checked_add(self.message.usage.output))
            .ok_or("Gemini usage total overflows")?;
        if let Some(total) = self.usage.get("totalTokenCount") {
            self.message.usage.total_tokens = total;
            if total < calculated
                || (prompt.is_some()
                    && self.usage.get("candidatesTokenCount").is_some()
                    && total != calculated)
            {
                return Err("Gemini total usage contradicts its counters".into());
            }
        } else {
            self.message.usage.total_tokens = calculated;
        }
        self.message.usage_status = self
            .usage
            .status("promptTokenCount", "candidatesTokenCount");
        Ok(())
    }
}
