use super::sse::SseDecoder;
use crate::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message,
    ModelRequest, Provider, StopReason, ThinkingLevel, ToolCall, current_system,
    parse_tool_arguments,
};
use futures::StreamExt;
use reqwest::{Client, Url};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ProviderConfigError(String);

/// OpenAI Chat Completions adapter. Configuration is explicit; no credentials are loaded.
/// The default client never retries or follows redirects. A supplied client owns its policies.
#[derive(Clone)]
pub struct OpenAiProvider {
    client: Client,
    endpoint: Url,
    api_key: String,
}

impl OpenAiProvider {
    pub fn new(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderConfigError> {
        let mut endpoint = Url::parse(base_url.as_ref())
            .map_err(|_| ProviderConfigError("Invalid provider base URL".into()))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(ProviderConfigError(
                "Provider URL must be HTTP(S), without credentials, query or fragment".into(),
            ));
        }
        let path = format!("{}/chat/completions", endpoint.path().trim_end_matches('/'));
        endpoint.set_path(&path);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|error| {
                ProviderConfigError(format!(
                    "Cannot create HTTP client: {}",
                    error.without_url()
                ))
            })?;
        Ok(Self {
            client,
            endpoint,
            api_key: api_key.into(),
        })
    }

    /// A caller-supplied client must not retry side-effecting requests implicitly.
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }
}

impl Provider for OpenAiProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let provider = self.clone();
        async_stream::stream! {
            let started = Instant::now();
            let mut state = ResponseState::new(&request);
            yield AssistantEvent::Start { message: state.message.clone() };
            let response: Result<reqwest::Response, String> = async {
                if cancel.is_cancelled() { return Err("Request cancelled".into()); }
                let body = build_request(&request)?;
                let mut builder = provider.client.post(provider.endpoint).json(&body);
                if !provider.api_key.is_empty() { builder = builder.bearer_auth(provider.api_key); }
                let response = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err("Request cancelled".into()),
                    response = builder.send() => response.map_err(|error| format!("HTTP request failed: {}", error.without_url()))?,
                };
                if !response.status().is_success() {
                    return Err(format!("Provider returned HTTP {}", response.status()));
                }
                let content_type = response.headers().get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()).unwrap_or("");
                if !content_type.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("text/event-stream") {
                    return Err("Provider did not return text/event-stream".into());
                }
                Ok(response)
            }.await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    let reason = if cancel.is_cancelled() { StopReason::Aborted } else { StopReason::Error };
                    state.message = state.message.failure(reason, error);
                    state.message.duration_ms = Some(started.elapsed().as_millis() as u64);
                    yield AssistantEvent::Error { message: state.message };
                    return;
                }
            };
            let mut body = response.bytes_stream();
            let mut decoder = SseDecoder::new();
            let mut failure = None;
            'body: loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        failure = Some("Request cancelled".to_owned());
                        break;
                    }
                    next = body.next() => next,
                };
                let (events, eof) = match next {
                    Some(Ok(bytes)) => (decoder.push(&bytes), false),
                    Some(Err(error)) => {
                        failure = Some(format!("Response stream failed: {}", error.without_url()));
                        break;
                    }
                    None => (decoder.finish(), true),
                };
                let events = match events {
                    Ok(events) => events,
                    Err(error) => { failure = Some(error); break; }
                };
                for event in events {
                    if cancel.is_cancelled() {
                        failure = Some("Request cancelled".to_owned());
                        break 'body;
                    }
                    match state.accept(&event) {
                        Ok(updates) => for update in updates {
                            if cancel.is_cancelled() {
                                failure = Some("Request cancelled".to_owned());
                                break 'body;
                            }
                            yield update;
                        },
                        Err(error) => { failure = Some(error); break 'body; }
                    }
                }
                if state.done { break; }
                if eof {
                    failure = Some("Response stream ended before [DONE]".into());
                    break;
                }
            }
            if failure.is_none() && cancel.is_cancelled() {
                failure = Some("Request cancelled".to_owned());
            }
            if let Some(error) = failure {
                let reason = if cancel.is_cancelled() { StopReason::Aborted } else { StopReason::Error };
                state.message = state.message.failure(reason, error);
                state.message.duration_ms = Some(started.elapsed().as_millis() as u64);
                yield AssistantEvent::Error { message: state.message };
            } else {
                state.message.duration_ms = Some(started.elapsed().as_millis() as u64);
                yield AssistantEvent::Done { message: state.message };
            }
        }.boxed()
    }
}

fn build_request(request: &ModelRequest) -> Result<Value, String> {
    if request.model.id.trim().is_empty() {
        return Err("Model ID must not be empty".into());
    }
    if request
        .options
        .temperature
        .is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value))
    {
        return Err("Temperature must be finite and between 0 and 2".into());
    }
    let system = current_system(&request.messages);
    let mut messages = Vec::new();
    if !system.prompt.is_empty() {
        messages.push(json!({"role":"system","content":system.prompt}));
    }
    let mut pending = BTreeMap::<String, String>::new();
    let mut seen = BTreeSet::new();
    for message in &request.messages {
        match message {
            Message::System(_) | Message::Custom { .. } => {}
            Message::User { content, .. } => {
                require_results(&pending)?;
                messages.push(json!({"role":"user","content":input_content(content, request.model.supports_images)?}));
            }
            Message::Assistant(assistant) => {
                if assistant.stop_reason.is_failure() {
                    continue;
                }
                require_results(&pending)?;
                if assistant.stop_reason == StopReason::Pending {
                    return Err("Pending assistant message cannot be replayed".into());
                }
                let mut text = Vec::new();
                let mut calls = Vec::new();
                for part in &assistant.content {
                    match part {
                        Content::Text { text: value } => text.push(value.as_str()),
                        Content::Thinking { .. } => {}
                        Content::Image { .. } => {
                            return Err(
                                "Assistant image output is unsupported by Chat Completions history"
                                    .into(),
                            );
                        }
                        Content::ToolCall(call) => {
                            if call.id.is_empty()
                                || call.name.is_empty()
                                || !call.arguments.is_object()
                            {
                                return Err(
                                    "History contains an invalid tool call id, name or arguments"
                                        .into(),
                                );
                            }
                            if !seen.insert(call.id.clone()) {
                                return Err("History contains a duplicate tool call id".into());
                            }
                            pending.insert(call.id.clone(), call.name.clone());
                            calls.push(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}}));
                        }
                    }
                }
                let mut wire = json!({"role":"assistant","content":if text.is_empty() { Value::Null } else { json!(text.join("\n")) }});
                if !calls.is_empty() {
                    wire["tool_calls"] = calls.into();
                }
                messages.push(wire);
            }
            Message::ToolResult(result) => {
                if result.outcome_unknown {
                    return Err(
                        "Tool outcome is unknown; reconcile effects before another request".into(),
                    );
                }
                let Some(name) = pending.remove(&result.tool_call_id) else {
                    return Err("History contains an unmatched or duplicate tool result".into());
                };
                if name != result.tool_name {
                    return Err("Tool result name does not match its tool call".into());
                }
                // Chat Completions tool messages accept text only. Image results retain an explicit placeholder.
                let content = result.content.iter().map(|part| match part {
                    Content::Text { text } => Ok(json!({"type":"text","text":text})),
                    Content::Image { mime_type, .. } => Ok(json!({"type":"text","text":format!("[Tool image omitted: {mime_type}]")})),
                    _ => Err("Tool result contains unsupported content".to_owned()),
                }).collect::<Result<Vec<_>, _>>()?;
                messages.push(
                    json!({"role":"tool","tool_call_id":result.tool_call_id,"content":content}),
                );
            }
        }
    }
    require_results(&pending)?;
    if messages.iter().all(|message| message["role"] == "system") {
        return Err("Request needs at least one conversational message".into());
    }
    let mut body = json!({"model":request.model.id,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
    if !system.tools.is_empty() {
        body["tools"] = system.tools.iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.parameters}})).collect::<Vec<_>>().into();
    }
    if let Some(max_tokens) = request
        .options
        .max_tokens
        .or(request.model.max_output_tokens)
    {
        if max_tokens == 0 {
            return Err("Maximum output tokens must be greater than zero".into());
        }
        body["max_completion_tokens"] = max_tokens.into();
    }
    if let Some(temperature) = request.options.temperature {
        body["temperature"] = json!(temperature);
    }
    if request.options.thinking_level != ThinkingLevel::Off {
        body["reasoning_effort"] = serde_json::to_value(request.options.thinking_level)
            .map_err(|error| error.to_string())?;
    }
    Ok(body)
}

fn require_results(pending: &BTreeMap<String, String>) -> Result<(), String> {
    if pending.is_empty() {
        Ok(())
    } else {
        Err("Missing tool result for an assistant tool call".into())
    }
}

fn input_content(content: &[Content], images: bool) -> Result<Vec<Value>, String> {
    content.iter().map(|part| match part {
        Content::Text { text } => Ok(json!({"type":"text","text":text})),
        Content::Image { data, mime_type } if images => Ok(json!({"type":"image_url","image_url":{"url":format!("data:{mime_type};base64,{data}")}})),
        Content::Image { mime_type, .. } => Ok(json!({"type":"text","text":format!("[Image omitted: {mime_type}]")})),
        _ => Err("User messages may only contain text or images".into()),
    }).collect()
}

struct PendingCall {
    content_index: usize,
    arguments: String,
}

struct ResponseState {
    message: AssistantMessage,
    calls: BTreeMap<u64, PendingCall>,
    deferred_calls: BTreeMap<u64, Vec<Value>>,
    deferred_bytes: usize,
    prompt_tokens: u64,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    finish: Option<StopReason>,
    done: bool,
}

impl ResponseState {
    fn new(request: &ModelRequest) -> Self {
        Self {
            message: AssistantMessage::new(&request.model),
            calls: BTreeMap::new(),
            deferred_calls: BTreeMap::new(),
            deferred_bytes: 0,
            prompt_tokens: 0,
            text_index: None,
            thinking_index: None,
            finish: None,
            done: false,
        }
    }

    fn accept(&mut self, data: &str) -> Result<Vec<AssistantEvent>, String> {
        if self.done {
            return Err("Unexpected data after [DONE]".into());
        }
        if data.trim() == "[DONE]" {
            if self.finish.is_none() {
                return Err("Stream ended without finish_reason".into());
            }
            self.done = true;
            return Ok(Vec::new());
        }
        let chunk: Value =
            serde_json::from_str(data).map_err(|error| format!("Invalid stream JSON: {error}"))?;
        let object = chunk.as_object().ok_or("Stream JSON must be an object")?;
        if let Some(error) = object.get("error").filter(|value| !value.is_null()) {
            return Err(format!("Provider stream error: {error}"));
        }
        if let Some(id) = optional_string(object, "id")? {
            stable_identity(&mut self.message.response_id, id, "id")?;
        }
        if let Some(model) = optional_string(object, "model")? {
            stable_identity(&mut self.message.response_model, model, "model")?;
        }
        if let Some(usage) = object.get("usage").filter(|value| !value.is_null()) {
            self.usage(usage)?;
        }
        let choices = object
            .get("choices")
            .and_then(Value::as_array)
            .ok_or("Stream chunk is missing choices array")?;
        if choices.is_empty() {
            if object.get("usage").is_none_or(Value::is_null) {
                return Err("Empty choices chunk is missing usage".into());
            }
            return Ok(Vec::new());
        }
        if choices.len() != 1 || choices[0].get("index").and_then(Value::as_u64) != Some(0) {
            return Err("Only one choice at index 0 is supported".into());
        }
        let choice = choices[0].as_object().ok_or("Invalid choice")?;
        let delta = choice
            .get("delta")
            .and_then(Value::as_object)
            .ok_or("Choice is missing delta object")?;
        if self.finish.is_some() {
            return Err("Received choice after finish_reason".into());
        }
        for key in ["function_call", "audio"] {
            if delta.get(key).is_some_and(|value| !value.is_null()) {
                return Err(format!("Unsupported stream delta: {key}"));
            }
        }
        if delta
            .get("refusal")
            .is_some_and(|value| !value.is_null() && value != "")
        {
            return Err("Provider returned a refusal".into());
        }
        if let Some(role) = optional_string(delta, "role")?
            && role != "assistant"
        {
            return Err("Stream role must be assistant".into());
        }
        let mut events = Vec::new();
        if let Some(text) = optional_string(delta, "content")?
            && !text.is_empty()
        {
            self.text(text, false, &mut events);
        }
        for key in ["reasoning_content", "reasoning", "reasoning_text"] {
            if let Some(text) = optional_string(delta, key)?
                && !text.is_empty()
            {
                self.text(text, true, &mut events);
                break;
            }
        }
        if let Some(calls) = delta.get("tool_calls").filter(|value| !value.is_null()) {
            for call in calls
                .as_array()
                .ok_or("tool_calls delta must be an array")?
            {
                self.tool(call, &mut events)?;
            }
        }
        if let Some(reason) = optional_string(choice, "finish_reason")? {
            let reason = match reason {
                "stop" => StopReason::Stop,
                "length" => StopReason::Length,
                "tool_calls" => StopReason::ToolUse,
                reason => return Err(format!("Unsupported finish_reason: {reason}")),
            };
            if reason == StopReason::ToolUse && self.calls.is_empty() {
                return Err("tool_calls finish_reason without tool calls".into());
            }
            if reason == StopReason::Stop && !self.calls.is_empty() {
                return Err("Tool calls require a tool_calls or length finish_reason".into());
            }
            self.finalize_blocks(&mut events)?;
            self.finish = Some(reason);
            self.message.stop_reason = reason;
        }
        Ok(events)
    }

    fn text(&mut self, delta: &str, thinking: bool, events: &mut Vec<AssistantEvent>) {
        let slot = if thinking {
            &mut self.thinking_index
        } else {
            &mut self.text_index
        };
        let index = if let Some(index) = *slot {
            index
        } else {
            let index = self.message.content.len();
            self.message.content.push(if thinking {
                Content::Thinking {
                    thinking: String::new(),
                    signature: None,
                }
            } else {
                Content::text("")
            });
            *slot = Some(index);
            events.push(if thinking {
                AssistantEvent::ThinkingStart {
                    index,
                    partial: self.message.clone(),
                }
            } else {
                AssistantEvent::TextStart {
                    index,
                    partial: self.message.clone(),
                }
            });
            index
        };
        match &mut self.message.content[index] {
            Content::Text { text } => text.push_str(delta),
            Content::Thinking { thinking, .. } => thinking.push_str(delta),
            _ => unreachable!(),
        }
        events.push(if thinking {
            AssistantEvent::ThinkingDelta {
                index,
                delta: delta.to_owned(),
                partial: self.message.clone(),
            }
        } else {
            AssistantEvent::TextDelta {
                index,
                delta: delta.to_owned(),
                partial: self.message.clone(),
            }
        });
    }

    fn tool(&mut self, value: &Value, events: &mut Vec<AssistantEvent>) -> Result<(), String> {
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .ok_or("Tool call delta needs an index")?;
        if index > 1024 {
            return Err("Tool call index exceeds limit".into());
        }
        if index > self.calls.len() as u64 {
            let size = value.to_string().len();
            self.deferred_bytes += size;
            if self.deferred_bytes > 8 * 1024 * 1024 {
                return Err("Deferred tool deltas exceed 8 MiB limit".into());
            }
            self.deferred_calls
                .entry(index)
                .or_default()
                .push(value.clone());
            return Ok(());
        }
        self.apply_tool(value, events)?;
        while let Some(buffered) = self.deferred_calls.remove(&(self.calls.len() as u64)) {
            for value in buffered {
                self.deferred_bytes -= value.to_string().len();
                self.apply_tool(&value, events)?;
            }
        }
        Ok(())
    }

    fn apply_tool(
        &mut self,
        value: &Value,
        events: &mut Vec<AssistantEvent>,
    ) -> Result<(), String> {
        let value = value
            .as_object()
            .ok_or("Tool call delta must be an object")?;
        let source_index = value
            .get("index")
            .and_then(Value::as_u64)
            .ok_or("Tool call delta needs an index")?;
        if source_index > 1024 {
            return Err("Tool call index exceeds limit".into());
        }
        if let Some(kind) = optional_string(value, "type")?
            && kind != "function"
        {
            return Err("Unsupported tool call type".into());
        }
        let id = optional_string(value, "id")?;
        let function = match value.get("function") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_object()
                    .ok_or("Tool call function must be an object")?,
            ),
        };
        let name = function
            .map(|value| optional_string(value, "name"))
            .transpose()?
            .flatten();
        let arguments = function
            .map(|value| optional_string(value, "arguments"))
            .transpose()?
            .flatten();
        let is_new = !self.calls.contains_key(&source_index);
        let pending = self.calls.entry(source_index).or_insert_with(|| {
            let content_index = self.message.content.len();
            self.message
                .content
                .push(Content::ToolCall(ToolCall::new("", "", json!({}))));
            PendingCall {
                content_index,
                arguments: String::new(),
            }
        });
        let Content::ToolCall(call) = &mut self.message.content[pending.content_index] else {
            unreachable!()
        };
        if let Some(id) = id {
            if !call.id.is_empty() && call.id != id {
                return Err("Tool call id changed during streaming".into());
            }
            call.id = id.to_owned();
        }
        if let Some(name) = name {
            call.name.push_str(name);
        }
        if is_new {
            events.push(AssistantEvent::ToolCallStart {
                index: pending.content_index,
                partial: self.message.clone(),
            });
        }
        if let Some(arguments) = arguments {
            pending.arguments.push_str(arguments);
            if pending.arguments.len() > 8 * 1024 * 1024 {
                return Err("Tool arguments exceed 8 MiB limit".into());
            }
            let Content::ToolCall(call) = &mut self.message.content[pending.content_index] else {
                unreachable!()
            };
            call.arguments = serde_json::from_str(&pending.arguments)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            events.push(AssistantEvent::ToolCallDelta {
                index: pending.content_index,
                delta: arguments.to_owned(),
                partial: self.message.clone(),
            });
        }
        Ok(())
    }

    fn finalize_blocks(&mut self, events: &mut Vec<AssistantEvent>) -> Result<(), String> {
        if !self.deferred_calls.is_empty() {
            return Err("Tool call indices are missing preceding calls".into());
        }
        let mut ids = BTreeSet::new();
        for pending in self.calls.values() {
            let Content::ToolCall(call) = &mut self.message.content[pending.content_index] else {
                unreachable!()
            };
            if call.id.is_empty() {
                return Err("Tool call is missing id".into());
            }
            if call.name.is_empty() {
                return Err("Tool call is missing name".into());
            }
            if !ids.insert(call.id.clone()) {
                return Err("Duplicate tool call id".into());
            }
            match parse_tool_arguments(&pending.arguments) {
                Ok(arguments) => call.arguments = arguments,
                Err(error) => {
                    call.arguments = json!({});
                    call.arguments_error = Some(error.to_string());
                }
            }
        }
        for index in 0..self.message.content.len() {
            events.push(match self.message.content[index] {
                Content::Text { .. } => AssistantEvent::TextEnd {
                    index,
                    partial: self.message.clone(),
                },
                Content::Thinking { .. } => AssistantEvent::ThinkingEnd {
                    index,
                    partial: self.message.clone(),
                },
                Content::ToolCall(_) => AssistantEvent::ToolCallEnd {
                    index,
                    partial: self.message.clone(),
                },
                Content::Image { .. } => unreachable!(),
            });
        }
        Ok(())
    }

    fn usage(&mut self, value: &Value) -> Result<(), String> {
        let object = value.as_object().ok_or("usage must be an object")?;
        let input = optional_u64(object, "prompt_tokens")?;
        let output = optional_u64(object, "completion_tokens")?;
        if let Some(details) = object
            .get("prompt_tokens_details")
            .filter(|value| !value.is_null())
        {
            let details = details
                .as_object()
                .ok_or("prompt_tokens_details must be an object")?;
            if let Some(tokens) = optional_u64(details, "cached_tokens")? {
                self.message.usage.cache_read = tokens;
            }
            if let Some(tokens) = optional_u64(details, "cache_write_tokens")? {
                self.message.usage.cache_write = tokens;
            }
        }
        if let Some(details) = object
            .get("completion_tokens_details")
            .filter(|value| !value.is_null())
        {
            let details = details
                .as_object()
                .ok_or("completion_tokens_details must be an object")?;
            if let Some(tokens) = optional_u64(details, "reasoning_tokens")? {
                self.message.usage.reasoning = tokens;
            }
        }
        if let Some(input) = input {
            self.prompt_tokens = input;
        }
        self.message.usage.input = self
            .prompt_tokens
            .saturating_sub(self.message.usage.cache_read)
            .saturating_sub(self.message.usage.cache_write);
        if let Some(output) = output {
            self.message.usage.output = output;
        }
        self.message.usage.update_total();
        Ok(())
    }
}

fn stable_identity(slot: &mut Option<String>, value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("Response {field} must not be empty"));
    }
    match slot {
        Some(previous) if previous != value => {
            Err(format!("Response {field} changed during streaming"))
        }
        Some(_) => Ok(()),
        None => {
            *slot = Some(value.to_owned());
            Ok(())
        }
    }
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        _ => Err(format!("{key} must be a string")),
    }
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be an unsigned integer")),
    }
}
