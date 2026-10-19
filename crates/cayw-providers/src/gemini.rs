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
    common::validate_request(request, Format::Gemini)?;
    let system = current_system(&request.messages);
    let mut contents = Vec::new();
    for message in &request.messages {
        match message {
            Message::System(_) | Message::Custom { .. } => {}
            Message::User { content, .. } => common::append_turn(
                &mut contents,
                "user",
                "parts",
                common::input_parts(content, request.model.supports_images, Format::Gemini)?,
            ),
            Message::Assistant(assistant) => {
                let parts = assistant.content.iter().map(|part| match part {
                    Content::Text { text } => Ok(json!({"text":text})),
                    Content::ToolCall(call) => Ok(json!({"functionCall":{"id":call.id,"name":call.name,"args":call.arguments}})),
                    _ => Err("Unsupported Gemini assistant history".to_owned()),
                }).collect::<Result<Vec<_>>>()?;
                common::append_turn(&mut contents, "model", "parts", parts);
            }
            Message::ToolResult(result) => {
                common::tool_output(result)?;
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
                common::append_turn(
                    &mut contents,
                    "user",
                    "parts",
                    vec![
                        json!({"functionResponse":{"id":result.tool_call_id,"name":result.tool_name,"response":response}}),
                    ],
                );
            }
        }
    }
    if contents.first().is_none_or(|m| m["role"] != "user") {
        return Err("Gemini history must start with a user turn".into());
    }
    let mut body = json!({"contents":contents,"generationConfig":{"candidateCount":1}});
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

pub(crate) struct State {
    pub message: AssistantMessage,
    tools: BTreeMap<String, ToolDefinition>,
    seen_calls: BTreeSet<String>,
    prefix: u64,
    call_sequence: u64,
    text_index: Option<usize>,
    finish: Option<StopReason>,
    prompt_tokens: u64,
    candidate_tokens: u64,
    total: Option<u64>,
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
            prompt_tokens: 0,
            candidate_tokens: 0,
            total: None,
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
            if let Some(usage) = value.get("usageMetadata") {
                self.usage(usage)?;
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
                    let fields = common::object(part)?;
                    if part.get("thoughtSignature").is_some_and(|v| !v.is_null())
                        || part.get("thought").is_some_and(|v| v.as_bool() != Some(false))
                    {
                        Err("Gemini thought and signature content cannot be represented safely by this adapter".to_owned())?;
                    }
                    if fields.keys().any(|key| !matches!(key.as_str(), "text" | "functionCall" | "thought")) {
                        Err("Unsupported Gemini response part".to_owned())?;
                    }
                    match (part.get("text"), part.get("functionCall")) {
                        (Some(text), None) => {
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
                            common::object(function)?;
                            if function.as_object().expect("checked object").keys().any(|key| !matches!(key.as_str(), "id" | "name" | "args")) {
                                Err("Partial or nonstandard Gemini function arguments are unsupported".to_owned())?;
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
        common::object(usage)?;
        common::counter(usage, "promptTokenCount", &mut self.prompt_tokens)?;
        common::counter(usage, "candidatesTokenCount", &mut self.candidate_tokens)?;
        common::counter(
            usage,
            "cachedContentTokenCount",
            &mut self.message.usage.cache_read,
        )?;
        common::counter(
            usage,
            "thoughtsTokenCount",
            &mut self.message.usage.reasoning,
        )?;
        if self.message.usage.cache_read > self.prompt_tokens {
            return Err("Gemini cached tokens exceed prompt tokens".into());
        }
        self.message.usage.input = self.prompt_tokens - self.message.usage.cache_read;
        self.message.usage.output = self
            .candidate_tokens
            .saturating_add(self.message.usage.reasoning);
        if let Some(total) = usage.get("totalTokenCount") {
            self.total = Some(total.as_u64().ok_or("Invalid Gemini total token count")?);
        }
        self.message.usage.update_total();
        if let Some(total) = self.total {
            self.message.usage.total_tokens = total;
        }
        Ok(())
    }
}
