use super::sse::SseDecoder;
use crate::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message,
    ModelRequest, Provider, StopReason, ThinkingLevel, ToolCall, UsageStatus, current_system,
    json::{checkpoint, parse_response},
};
use base64::{engine::general_purpose::STANDARD, read::DecoderReader};
use futures::{StreamExt, stream::BoxStream};
use reqwest::{Client, Url};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;
use thiserror::Error;

// The supported response range is finite across the entire HTTP response, even
// when comments, empty content or deferred tool deltas produce no output events.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_FRAMES: usize = 8192;
const MAX_FRAGMENTS: usize = 4096;
const MAX_TOOL_CALLS: usize = 128;
const MAX_CONTENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_SNAPSHOT_WORK: usize = 64 * 1024 * 1024;
const TOOL_ARGUMENT_BATCH_BYTES: usize = 256;
const DECODE_BATCH_BYTES: usize = 4096;

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
            let mut received = 0usize;
            let mut frames = 0usize;
            'body: loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        failure = Some("Request cancelled".to_owned());
                        break;
                    }
                    next = body.next() => next,
                };
                match next {
                    Some(Ok(bytes)) => {
                        let mut offset = 0;
                        while offset < bytes.len() {
                            if let Err(error) = checkpoint(&cancel).await {
                                if failure.is_none() {
                                    failure = Some(error.to_string());
                                }
                                break 'body;
                            }
                            // Count only bytes consumed before [DONE]. Never
                            // inspect a later frame just because it shares the
                            // same network chunk with the terminal frame.
                            let allowance = MAX_RESPONSE_BYTES.saturating_sub(received);
                            if allowance == 0 {
                                if failure.is_none() {
                                    failure = Some("Provider response exceeds the 64 MiB limit".into());
                                }
                                break 'body;
                            }
                            let end = bytes.len().min(offset + DECODE_BATCH_BYTES.min(allowance));
                            let (used, frame) = match decoder.push_frame(&bytes[offset..end], &cancel).await {
                                Ok(decoded) => decoded,
                                Err(error) => {
                                    if failure.is_none() {
                                        failure = Some(error);
                                    }
                                    break 'body;
                                }
                            };
                            offset += used;
                            received += used;
                            if let Some(frame) = frame {
                                frames += 1;
                                if frames > MAX_RESPONSE_FRAMES {
                                    if failure.is_none() {
                                        failure = Some("Provider response exceeds the frame limit".into());
                                    }
                                    break 'body;
                                }
                                if let Err(error) = checkpoint(&cancel).await {
                                    if failure.is_none() {
                                        failure = Some(error.to_string());
                                    }
                                    break 'body;
                                }
                                if failure.is_some() {
                                    if let Err(error) = state.drain_frame(&frame, &cancel).await {
                                        if failure.is_none() {
                                            failure = Some(error);
                                        }
                                        break 'body;
                                    }
                                } else {
                                    let mut updates = state.accept(&frame, &cancel);
                                    while let Some(update) = updates.next().await {
                                        match update {
                                            Ok(update) => {
                                                if cancel.is_cancelled() {
                                                    failure = Some("Request cancelled".into());
                                                    break 'body;
                                                }
                                                yield update;
                                            }
                                            Err(error) => {
                                                failure = Some(error);
                                                break;
                                            }
                                        }
                                    }
                                    drop(updates);
                                    if failure.is_some() && !state.finish_phase {
                                        break 'body;
                                    }
                                }
                                if state.done { break 'body; }
                            }
                        }
                    }
                    Some(Err(error)) => {
                        if failure.is_none() {
                            failure = Some(format!("Response stream failed: {}", error.without_url()));
                        }
                        break;
                    }
                    None => {
                        let frame = match decoder.finish(&cancel).await {
                            Ok(frame) => frame,
                            Err(error) => {
                                if failure.is_none() {
                                    failure = Some(error);
                                }
                                break;
                            }
                        };
                        if let Some(frame) = frame {
                            frames += 1;
                            if frames > MAX_RESPONSE_FRAMES {
                                if failure.is_none() {
                                    failure = Some("Provider response exceeds the frame limit".into());
                                }
                                break;
                            }
                            if let Err(error) = checkpoint(&cancel).await {
                                if failure.is_none() {
                                    failure = Some(error.to_string());
                                }
                                break;
                            }
                            if failure.is_some() {
                                if let Err(error) = state.drain_frame(&frame, &cancel).await {
                                    if failure.is_none() {
                                        failure = Some(error);
                                    }
                                    break;
                                }
                            } else {
                                let mut updates = state.accept(&frame, &cancel);
                                while let Some(update) = updates.next().await {
                                    match update {
                                        Ok(update) => {
                                            if cancel.is_cancelled() {
                                                failure = Some("Request cancelled".into());
                                                break 'body;
                                            }
                                            yield update;
                                        }
                                        Err(error) => {
                                            failure = Some(error);
                                            break;
                                        }
                                    }
                                }
                                drop(updates);
                                if failure.is_some() && !state.finish_phase {
                                    break;
                                }
                            }
                        }
                        if failure.is_none() && !state.done {
                            failure = Some("Response stream ended before [DONE]".into());
                        }
                        break;
                    }
                }
            }
            // A consumer may retain the stream after its terminal event.
            drop(body);
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
    let mut awaiting_model = false;
    for message in &request.messages {
        match message {
            Message::System(_) | Message::Custom { .. } => {}
            Message::User { content, .. } => {
                require_results(&pending)?;
                if content.is_empty() {
                    return Err("User messages must contain content".into());
                }
                messages.push(json!({"role":"user","content":input_content(content, request.model.supports_images)?}));
                awaiting_model = true;
            }
            Message::Assistant(assistant) => {
                if !assistant.provider_metadata.is_empty() {
                    return Err("Chat Completions cannot replay another protocol's metadata".into());
                }
                require_results(&pending)?;
                if !awaiting_model {
                    return Err(
                        "Assistant history needs a preceding user or tool-result turn".into(),
                    );
                }
                awaiting_model = false;
                if assistant.stop_reason == StopReason::Pending
                    || assistant.stop_reason.is_failure()
                {
                    return Err("Incomplete or failed assistant history cannot be replayed".into());
                }
                if assistant.content.is_empty() {
                    return Err("Assistant history must contain content".into());
                }
                let mut text = Vec::new();
                let mut thinking = String::new();
                let mut has_thinking = false;
                let mut calls = Vec::new();
                for part in &assistant.content {
                    match part {
                        Content::Text { text: value } => text.push(value.as_str()),
                        Content::Thinking {
                            thinking: value,
                            signature,
                        } => {
                            if signature.is_some() || request.model.provider != "magpie" {
                                return Err("Thinking history is unsupported by this Chat Completions configuration".into());
                            }
                            has_thinking = true;
                            thinking.push_str(value);
                        }
                        Content::Image { .. } => {
                            return Err(
                                "Assistant image output is unsupported by Chat Completions history"
                                    .into(),
                            );
                        }
                        Content::ToolCall(call) => {
                            if call.id.trim().is_empty()
                                || call.name.trim().is_empty()
                                || !call.arguments.is_object()
                                || call.arguments_error.is_some()
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
                if !calls.is_empty() && assistant.stop_reason != StopReason::ToolUse {
                    return Err("Assistant tool calls require a tool-use stop reason".into());
                }
                if calls.is_empty() && assistant.stop_reason == StopReason::ToolUse {
                    return Err("Tool-use history has no tool calls".into());
                }
                let mut wire = json!({"role":"assistant","content":if text.is_empty() { Value::Null } else { json!(text.join("\n")) }});
                if !calls.is_empty() {
                    wire["tool_calls"] = calls.into();
                }
                if has_thinking {
                    wire["reasoning_content"] = thinking.into();
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
                let mut content = result
                    .content
                    .iter()
                    .map(|part| match part {
                        Content::Text { text } => Ok(json!({"type":"text","text":text})),
                        _ => Err("Tool results support text and structured JSON only".to_owned()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if let Some(structured) = &result.structured_content {
                    content.push(json!({"type":"text","text":structured.to_string()}));
                }
                messages.push(
                    json!({"role":"tool","tool_call_id":result.tool_call_id,"content":content}),
                );
                awaiting_model = true;
            }
        }
    }
    require_results(&pending)?;
    if !awaiting_model {
        return Err("Request must end with a user or paired tool-result turn".into());
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
        Content::Image { data, mime_type } => {
            if !images {
                return Err("User images require an explicitly enabled model capability".into());
            }
            if !matches!(mime_type.as_str(), "image/png" | "image/jpeg" | "image/webp" | "image/gif") {
                return Err("Unsupported user image MIME type".into());
            }
            if data.is_empty() || std::io::copy(
                &mut DecoderReader::new(data.as_bytes(), &STANDARD),
                &mut std::io::sink(),
            ).is_err() {
                return Err("User image data must be nonempty standard base64".into());
            }
            Ok(json!({"type":"image_url","image_url":{"url":format!("data:{mime_type};base64,{data}")}}))
        }
        _ => Err("User messages may only contain text or images".into()),
    }).collect()
}

struct PendingCall {
    content_index: usize,
    arguments: String,
    emitted_bytes: usize,
    parsed_weight: usize,
}

struct DeferredCall {
    value: Value,
    weight: usize,
}

struct ResponseState {
    message: AssistantMessage,
    calls: BTreeMap<u64, PendingCall>,
    deferred_calls: BTreeMap<u64, Vec<DeferredCall>>,
    deferred_bytes: usize,
    fragments: usize,
    content_bytes: usize,
    message_content_bytes: usize,
    retained_weight: usize,
    arguments_weight: usize,
    snapshot_work: usize,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    finish: Option<StopReason>,
    finish_phase: bool,
    done: bool,
}

impl ResponseState {
    fn new(request: &ModelRequest) -> Self {
        Self {
            message: AssistantMessage::new(&request.model),
            calls: BTreeMap::new(),
            deferred_calls: BTreeMap::new(),
            deferred_bytes: 0,
            fragments: 0,
            content_bytes: 0,
            message_content_bytes: 0,
            retained_weight: std::mem::size_of::<AssistantMessage>()
                .saturating_add(request.model.provider.len())
                .saturating_add(request.model.id.len()),
            arguments_weight: 0,
            snapshot_work: 0,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            text_index: None,
            thinking_index: None,
            finish: None,
            finish_phase: false,
            done: false,
        }
    }

    async fn accept_header(
        &mut self,
        object: &Map<String, Value>,
        cancel: &CancellationToken,
    ) -> Result<bool, String> {
        checkpoint(cancel).await?;
        if object.get("error").is_some_and(|value| !value.is_null()) {
            return Err("Provider returned a stream error".into());
        }
        if let Some(id) = optional_string(object, "id")? {
            let weight = if self.message.response_id.is_none() {
                id.len()
            } else {
                0
            };
            if self.retained_weight.saturating_add(weight) > MAX_RESPONSE_BYTES {
                return Err("Provider response exceeds the byte limit".into());
            }
            self.retained_weight += weight;
            stable_identity(&mut self.message.response_id, id, "id")?;
        }
        if let Some(model) = optional_string(object, "model")? {
            let weight = if self.message.response_model.is_none() {
                model.len()
            } else {
                0
            };
            if self.retained_weight.saturating_add(weight) > MAX_RESPONSE_BYTES {
                return Err("Provider response exceeds the byte limit".into());
            }
            self.retained_weight += weight;
            stable_identity(&mut self.message.response_model, model, "model")?;
        }
        if let Some(usage) = object.get("usage").filter(|value| !value.is_null()) {
            let weight = value_weight(usage, cancel).await?;
            if self.retained_weight.saturating_add(weight) > MAX_RESPONSE_BYTES {
                return Err("Provider response exceeds the byte limit".into());
            }
            self.retained_weight += weight;
            self.usage(usage)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn drain_frame(&mut self, data: &str, cancel: &CancellationToken) -> Result<(), String> {
        checkpoint(cancel).await?;
        if self.done {
            return Err("Unexpected data after [DONE]".into());
        }
        if done_frame(data, cancel).await? {
            self.done = true;
            return Ok(());
        }
        let chunk = parse_response(data, cancel).await?;
        let object = chunk.as_object().ok_or("Stream JSON must be an object")?;
        self.accept_header(object, cancel).await?;
        Ok(())
    }

    fn accept<'a>(
        &'a mut self,
        data: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxStream<'a, Result<AssistantEvent, String>> {
        async_stream::try_stream! {
            checkpoint(cancel).await?;
            self.finish_phase = false;
            if self.done { Err("Unexpected data after [DONE]")?; }
            if done_frame(data, cancel).await? {
                if self.finish.is_none() { Err("Stream ended without finish_reason")?; }
                self.done = true;
            } else {
                let chunk = parse_response(data, cancel).await?;
                let object = chunk.as_object().ok_or("Stream JSON must be an object")?;
                if self.accept_header(object, cancel).await? {
                    let partial = self.snapshot(0, cancel).await?;
                    yield AssistantEvent::UsageUpdate { partial };
                }
                let choices = object.get("choices").and_then(Value::as_array)
                    .ok_or("Stream chunk is missing choices array")?;
                if choices.is_empty() {
                    if object.get("usage").is_none_or(Value::is_null) {
                        Err("Empty choices chunk is missing usage")?;
                    }
                } else {
                    if choices.len() != 1 || choices[0].get("index").and_then(Value::as_u64) != Some(0) {
                        Err("Only one choice at index 0 is supported")?;
                    }
                    let choice = choices[0].as_object().ok_or("Invalid choice")?;
                    let delta = choice.get("delta").and_then(Value::as_object)
                        .ok_or("Choice is missing delta object")?;
                    if self.finish.is_some() { Err("Received choice after finish_reason")?; }
                    for key in ["function_call", "audio"] {
                        if delta.get(key).is_some_and(|value| !value.is_null()) {
                            Err(format!("Unsupported stream delta: {key}"))?;
                        }
                    }
                    if delta.get("refusal").is_some_and(|value| !value.is_null() && value != "") {
                        Err("Provider returned a refusal")?;
                    }
                    if optional_string(delta, "role")?.is_some_and(|role| role != "assistant") {
                        Err("Stream role must be assistant")?;
                    }
                    // Reject oversized frames before creating content, deferred
                    // copies, or any batch of full-message snapshots.
                    self.preflight(delta, cancel).await?;
                    if let Some(text) = optional_string(delta, "content")?.filter(|text| !text.is_empty()) {
                        let mut updates = self.text(text, false, cancel);
                        while let Some(update) = updates.next().await { yield update?; }
                    }
                    for key in ["reasoning_content", "reasoning", "reasoning_text"] {
                        if let Some(text) = optional_string(delta, key)?.filter(|text| !text.is_empty()) {
                            let mut updates = self.text(text, true, cancel);
                            while let Some(update) = updates.next().await { yield update?; }
                            break;
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").filter(|value| !value.is_null()) {
                        for call in calls.as_array().ok_or("tool_calls delta must be an array")? {
                            let mut updates = self.tool(call, cancel);
                            while let Some(update) = updates.next().await { yield update?; }
                        }
                    }
                    if let Some(reason) = optional_string(choice, "finish_reason")? {
                        self.finish_phase = true;
                        let reason = match reason {
                            "stop" => StopReason::Stop,
                            "length" => StopReason::Length,
                            "tool_calls" => StopReason::ToolUse,
                            reason => Err(format!("Unsupported finish_reason: {reason}"))?,
                        };
                        if reason == StopReason::ToolUse && self.calls.is_empty() {
                            Err("tool_calls finish_reason without tool calls")?;
                        }
                        if reason == StopReason::Stop && !self.calls.is_empty() {
                            Err("Tool calls require a tool_calls or length finish_reason")?;
                        }
                        let mut updates = self.finalize_blocks(reason, cancel);
                        while let Some(update) = updates.next().await { yield update?; }
                        drop(updates);
                        self.finish = Some(reason);
                        self.message.stop_reason = reason;
                    }
                }
            }
        }.boxed()
    }

    async fn preflight(
        &mut self,
        delta: &Map<String, Value>,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let mut fragments = self.fragments;
        let mut bytes = self.content_bytes;
        for key in [
            "content",
            "reasoning_content",
            "reasoning",
            "reasoning_text",
        ] {
            if let Some(text) = optional_string(delta, key)? {
                fragments = fragments.saturating_add(1);
                bytes = bytes.saturating_add(text.len());
            }
        }
        let mut indices = self
            .calls
            .keys()
            .chain(self.deferred_calls.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        if let Some(calls) = delta.get("tool_calls").filter(|value| !value.is_null()) {
            let calls = calls
                .as_array()
                .ok_or("tool_calls delta must be an array")?;
            fragments = fragments.saturating_add(calls.len());
            if fragments > MAX_FRAGMENTS {
                return Err("Provider response exceeds the content fragment limit".into());
            }
            for (step, call) in calls.iter().enumerate() {
                if step.is_multiple_of(64) {
                    checkpoint(cancel).await?;
                }
                let call = call
                    .as_object()
                    .ok_or("Tool call delta must be an object")?;
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or("Tool call delta needs an index")?;
                if index >= MAX_TOOL_CALLS as u64 {
                    return Err("Provider response exceeds the tool call limit".into());
                }
                indices.insert(index);
                if indices.len() > MAX_TOOL_CALLS {
                    return Err("Provider response exceeds the tool call limit".into());
                }
                if let Some(id) = optional_string(call, "id")? {
                    bytes = bytes.saturating_add(id.len());
                }
                if let Some(function) = call.get("function").filter(|value| !value.is_null()) {
                    let function = function
                        .as_object()
                        .ok_or("Tool call function must be an object")?;
                    for key in ["name", "arguments"] {
                        if let Some(value) = optional_string(function, key)? {
                            bytes = bytes.saturating_add(value.len());
                        }
                    }
                }
            }
        }
        if fragments > MAX_FRAGMENTS {
            return Err("Provider response exceeds the content fragment limit".into());
        }
        if bytes > MAX_CONTENT_BYTES {
            return Err("Provider content exceeds the 8 MiB limit".into());
        }
        self.fragments = fragments;
        self.content_bytes = bytes;
        Ok(())
    }

    async fn snapshot(
        &mut self,
        extra_bytes: usize,
        cancel: &CancellationToken,
    ) -> Result<AssistantMessage, String> {
        checkpoint(cancel).await?;
        let weight = self
            .retained_weight
            .saturating_add(self.message_content_bytes)
            .saturating_add(self.arguments_weight)
            .saturating_add(extra_bytes);
        if weight > MAX_SNAPSHOT_WORK.saturating_sub(self.snapshot_work) {
            return Err("Provider response exceeds the snapshot work limit".into());
        }
        self.snapshot_work += weight;
        Ok(self.message.clone())
    }

    fn text<'a>(
        &'a mut self,
        delta: &'a str,
        thinking: bool,
        cancel: &'a CancellationToken,
    ) -> BoxStream<'a, Result<AssistantEvent, String>> {
        async_stream::try_stream! {
            checkpoint(cancel).await?;
            let slot = if thinking { &mut self.thinking_index } else { &mut self.text_index };
            let index = if let Some(index) = *slot {
                index
            } else {
                let index = self.message.content.len();
                self.message.content.push(if thinking {
                    Content::Thinking { thinking: String::new(), signature: None }
                } else { Content::text("") });
                *slot = Some(index);
                self.retained_weight += std::mem::size_of::<Content>();
                let partial = self.snapshot(0, cancel).await?;
                yield if thinking {
                    AssistantEvent::ThinkingStart { index, partial }
                } else { AssistantEvent::TextStart { index, partial } };
                index
            };
            checkpoint(cancel).await?;
            match &mut self.message.content[index] {
                Content::Text { text } => text.push_str(delta),
                Content::Thinking { thinking, .. } => thinking.push_str(delta),
                _ => unreachable!(),
            }
            self.message_content_bytes = self.message_content_bytes.saturating_add(delta.len());
            let partial = self.snapshot(delta.len(), cancel).await?;
            yield if thinking {
                AssistantEvent::ThinkingDelta { index, delta: delta.to_owned(), partial }
            } else { AssistantEvent::TextDelta { index, delta: delta.to_owned(), partial } };
        }
        .boxed()
    }

    fn tool<'a>(
        &'a mut self,
        value: &'a Value,
        cancel: &'a CancellationToken,
    ) -> BoxStream<'a, Result<AssistantEvent, String>> {
        async_stream::try_stream! {
            checkpoint(cancel).await?;
            let index = value.get("index").and_then(Value::as_u64).ok_or("Tool call delta needs an index")?;
            if index > self.calls.len() as u64 {
                let weight = value_weight(value, cancel).await?;
                if weight > MAX_CONTENT_BYTES.saturating_sub(self.deferred_bytes) {
                    Err("Deferred tool deltas exceed 8 MiB limit")?;
                }
                self.deferred_bytes += weight;
                self.deferred_calls.entry(index).or_default().push(DeferredCall { value: value.clone(), weight });
            } else {
                {
                    let mut updates = self.apply_tool(value, cancel);
                    while let Some(update) = updates.next().await { yield update?; }
                }
                while let Some(buffered) = self.deferred_calls.remove(&(self.calls.len() as u64)) {
                    for deferred in buffered {
                        checkpoint(cancel).await?;
                        self.deferred_bytes -= deferred.weight;
                        let mut updates = self.apply_tool(&deferred.value, cancel);
                        while let Some(update) = updates.next().await { yield update?; }
                    }
                }
            }
        }.boxed()
    }

    fn apply_tool<'a>(
        &'a mut self,
        value: &'a Value,
        cancel: &'a CancellationToken,
    ) -> BoxStream<'a, Result<AssistantEvent, String>> {
        async_stream::try_stream! {
            checkpoint(cancel).await?;
            let value = value.as_object().ok_or("Tool call delta must be an object")?;
            let source_index = value.get("index").and_then(Value::as_u64).ok_or("Tool call delta needs an index")?;
            if optional_string(value, "type")?.is_some_and(|kind| kind != "function") {
                Err("Unsupported tool call type")?;
            }
            let id = optional_string(value, "id")?;
            let function = value.get("function").filter(|value| !value.is_null())
                .map(|value| value.as_object().ok_or("Tool call function must be an object")).transpose()?;
            let name = function.map(|value| optional_string(value, "name")).transpose()?.flatten();
            let arguments = function.map(|value| optional_string(value, "arguments")).transpose()?.flatten();
            let is_new = !self.calls.contains_key(&source_index);
            let pending = self.calls.entry(source_index).or_insert_with(|| {
                let content_index = self.message.content.len();
                self.message.content.push(Content::ToolCall(ToolCall::new("", "", json!({}))));
                self.retained_weight += std::mem::size_of::<Content>();
                let parsed_weight = std::mem::size_of::<Value>() + 96;
                self.arguments_weight = self.arguments_weight.saturating_add(parsed_weight);
                PendingCall { content_index, arguments: String::new(), emitted_bytes: 0, parsed_weight }
            });
            let index = pending.content_index;
            let Content::ToolCall(call) = &mut self.message.content[index] else { unreachable!() };
            if let Some(id) = id {
                if !call.id.is_empty() && call.id != id { Err("Tool call id changed during streaming")?; }
                self.message_content_bytes = self.message_content_bytes.saturating_add(id.len().saturating_sub(call.id.len()));
                call.id = id.to_owned();
            }
            if let Some(name) = name {
                self.message_content_bytes = self.message_content_bytes.saturating_add(name.len());
                call.name.push_str(name);
            }
            if is_new {
                let partial = self.snapshot(0, cancel).await?;
                yield AssistantEvent::ToolCallStart { index, partial };
            }
            if let Some(arguments) = arguments {
                checkpoint(cancel).await?;
                let pending = self.calls.get_mut(&source_index).expect("call exists");
                pending.arguments.push_str(arguments);
                if pending.arguments.len().saturating_sub(pending.emitted_bytes) >= TOOL_ARGUMENT_BATCH_BYTES {
                    yield self.argument_delta(source_index, cancel).await?;
                }
            }
        }.boxed()
    }

    async fn argument_delta(
        &mut self,
        source_index: u64,
        cancel: &CancellationToken,
    ) -> Result<AssistantEvent, String> {
        self.replace_arguments(source_index, false, cancel).await?;
        let pending = self.calls.get(&source_index).expect("call exists");
        let index = pending.content_index;
        let emitted_bytes = pending.arguments.len();
        let delta_len = emitted_bytes - pending.emitted_bytes;
        // Pending wire arguments are not part of AssistantMessage. Batch small
        // fragments so a retained thinking block is not cloned for each byte.
        let partial = self.snapshot(delta_len, cancel).await?;
        let pending = self.calls.get_mut(&source_index).expect("call exists");
        let delta = pending.arguments[pending.emitted_bytes..].to_owned();
        pending.emitted_bytes = emitted_bytes;
        Ok(AssistantEvent::ToolCallDelta {
            index,
            delta,
            partial,
        })
    }

    async fn replace_arguments(
        &mut self,
        source_index: u64,
        finalizing: bool,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, String> {
        let pending = self.calls.get(&source_index).expect("call exists");
        let parsed =
            if pending.arguments.is_empty() || only_whitespace(&pending.arguments, cancel).await? {
                Ok(json!({}))
            } else {
                parse_response(&pending.arguments, cancel)
                    .await
                    .map_err(|error| format!("Invalid tool JSON: {error}"))
                    .and_then(|value| {
                        if value.is_object() {
                            Ok(value)
                        } else {
                            Err("Tool arguments must be a JSON object".into())
                        }
                    })
            };
        crate::json::check_cancel(cancel)?;
        let (arguments, error) = match parsed {
            Ok(arguments) => (arguments, None),
            Err(error) => (json!({}), Some(error)),
        };
        let weight = value_weight(&arguments, cancel).await?;
        let pending = self.calls.get_mut(&source_index).expect("call exists");
        self.arguments_weight = self
            .arguments_weight
            .saturating_sub(pending.parsed_weight)
            .saturating_add(weight);
        pending.parsed_weight = weight;
        let Content::ToolCall(call) = &mut self.message.content[pending.content_index] else {
            unreachable!()
        };
        call.arguments = arguments;
        if finalizing {
            call.arguments_error = error.clone();
        }
        Ok(error)
    }

    fn finalize_blocks<'a>(
        &'a mut self,
        reason: StopReason,
        cancel: &'a CancellationToken,
    ) -> BoxStream<'a, Result<AssistantEvent, String>> {
        async_stream::try_stream! {
            if !self.deferred_calls.is_empty() { Err("Tool call indices are missing preceding calls")?; }
            let mut ids = BTreeSet::new();
            // At most 128 scalar indices; no message or JSON tree snapshots.
            let indices = self.calls.keys().copied().collect::<Vec<_>>();
            for source_index in indices {
                checkpoint(cancel).await?;
                let pending = &self.calls[&source_index];
                if pending.emitted_bytes < pending.arguments.len() {
                    yield self.argument_delta(source_index, cancel).await?;
                }
                let pending = &self.calls[&source_index];
                let Content::ToolCall(call) = &self.message.content[pending.content_index] else { unreachable!() };
                if call.id.is_empty() { Err("Tool call is missing id")?; }
                if call.name.is_empty() { Err("Tool call is missing name")?; }
                if !ids.insert(call.id.clone()) { Err("Duplicate tool call id")?; }
                if let Some(error) = self.replace_arguments(source_index, true, cancel).await? {
                    self.retained_weight += error.len();
                    if reason != StopReason::Length { Err(error)?; }
                }
            }
            for index in 0..self.message.content.len() {
                let partial = self.snapshot(0, cancel).await?;
                yield match self.message.content[index] {
                    Content::Text { .. } => AssistantEvent::TextEnd { index, partial },
                    Content::Thinking { .. } => AssistantEvent::ThinkingEnd { index, partial },
                    Content::ToolCall(_) => AssistantEvent::ToolCallEnd { index, partial },
                    Content::Image { .. } => unreachable!(),
                };
            }
        }.boxed()
    }

    fn usage(&mut self, value: &Value) -> Result<(), String> {
        let previous_status = self.message.usage_status;
        self.message.usage_status = UsageStatus::Invalid;
        let object = value.as_object().ok_or("usage must be an object")?;
        for key in [
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "prompt_tokens_details",
            "completion_tokens_details",
        ] {
            if let Some(value) = object.get(key) {
                if let (Some(previous), Some(current)) = (
                    self.message
                        .usage_fields
                        .get_mut(key)
                        .and_then(Value::as_object_mut),
                    value.as_object(),
                ) {
                    previous.extend(current.clone());
                } else {
                    self.message
                        .usage_fields
                        .insert(key.to_owned(), value.clone());
                }
            }
        }
        let input = optional_u64(object, "prompt_tokens")?;
        let output = optional_u64(object, "completion_tokens")?;
        let total = optional_u64(object, "total_tokens")?;
        for (previous, current) in [
            (self.prompt_tokens, input),
            (self.completion_tokens, output),
            (self.total_tokens, total),
        ] {
            if let (Some(previous), Some(current)) = (previous, current)
                && current < previous
            {
                return Err("Cumulative usage counter decreased".into());
            }
        }
        let mut details_reported = false;
        if let Some(details) = object
            .get("prompt_tokens_details")
            .filter(|value| !value.is_null())
        {
            let details = details
                .as_object()
                .ok_or("prompt_tokens_details must be an object")?;
            if let Some(tokens) = optional_u64(details, "cached_tokens")? {
                if tokens < self.message.usage.cache_read {
                    return Err("Cumulative cache usage decreased".into());
                }
                self.message.usage.cache_read = tokens;
                details_reported = true;
            }
            if let Some(tokens) = optional_u64(details, "cache_write_tokens")? {
                if tokens < self.message.usage.cache_write {
                    return Err("Cumulative cache usage decreased".into());
                }
                self.message.usage.cache_write = tokens;
                details_reported = true;
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
                if tokens < self.message.usage.reasoning {
                    return Err("Cumulative reasoning usage decreased".into());
                }
                self.message.usage.reasoning = tokens;
                details_reported = true;
            }
        }
        if let Some(input) = input {
            self.prompt_tokens = Some(input);
        }
        self.message.usage.input = self
            .prompt_tokens
            .unwrap_or_default()
            .saturating_sub(self.message.usage.cache_read)
            .saturating_sub(self.message.usage.cache_write);
        if let Some(output) = output {
            self.completion_tokens = Some(output);
            self.message.usage.output = output;
        }
        if let Some(total) = total {
            self.total_tokens = Some(total);
        }
        let cache = self
            .message
            .usage
            .cache_read
            .checked_add(self.message.usage.cache_write)
            .ok_or("Cache usage overflows")?;
        if self.prompt_tokens.is_some_and(|prompt| cache > prompt) {
            return Err("Cached usage exceeds prompt tokens".into());
        }
        if self
            .completion_tokens
            .is_some_and(|output| self.message.usage.reasoning > output)
        {
            return Err("Reasoning usage exceeds completion tokens".into());
        }
        let known_input = self.prompt_tokens.unwrap_or(cache);
        let calculated = known_input
            .checked_add(self.message.usage.output)
            .ok_or("Usage total overflows")?;
        let complete = self.prompt_tokens.is_some() && self.completion_tokens.is_some();
        self.message.usage.total_tokens = self.total_tokens.unwrap_or(calculated);
        if let Some(total) = self.total_tokens
            && (total < calculated || (complete && total != calculated))
        {
            return Err("Total usage contradicts its counters".into());
        }
        self.message.usage_status = if complete {
            UsageStatus::Reported
        } else if self.prompt_tokens.is_some()
            || self.completion_tokens.is_some()
            || self.total_tokens.is_some()
            || details_reported
            || previous_status == UsageStatus::Partial
        {
            UsageStatus::Partial
        } else {
            UsageStatus::Unknown
        };
        Ok(())
    }
}

async fn only_whitespace(input: &str, cancel: &CancellationToken) -> Result<bool, String> {
    let mut next_checkpoint = 0;
    for (step, character) in input.char_indices() {
        if step >= next_checkpoint {
            checkpoint(cancel).await?;
            next_checkpoint = step.saturating_add(DECODE_BATCH_BYTES);
        }
        if !character.is_whitespace() {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn done_frame(input: &str, cancel: &CancellationToken) -> Result<bool, String> {
    let mut start = None;
    let mut next_checkpoint = 0;
    for (step, character) in input.char_indices() {
        if step >= next_checkpoint {
            checkpoint(cancel).await?;
            next_checkpoint = step.saturating_add(DECODE_BATCH_BYTES);
        }
        if !character.is_whitespace() {
            start = Some(step);
            break;
        }
    }
    let Some(start) = start else {
        return Ok(false);
    };
    let tail = &input[start..];
    Ok(tail.starts_with("[DONE]") && only_whitespace(&tail[6..], cancel).await?)
}

// Iterate with a depth-sized stack; never serialize a JSON tree just to count
// its size. The charge includes strings, values, and conservative map/array
// overhead so a large tree of empty objects also consumes snapshot work.
async fn value_weight(value: &Value, cancel: &CancellationToken) -> Result<usize, String> {
    enum Visit<'a> {
        Value(&'a Value),
        Array(std::slice::Iter<'a, Value>),
        Object(serde_json::map::Iter<'a>),
    }
    let mut stack = vec![Visit::Value(value)];
    let mut weight = 0usize;
    let mut steps = 0usize;
    while let Some(visit) = stack.pop() {
        steps += 1;
        if steps.is_multiple_of(64) {
            checkpoint(cancel).await?;
        }
        match visit {
            Visit::Value(value) => {
                weight = weight.saturating_add(std::mem::size_of::<Value>() + 96);
                match value {
                    Value::String(text) => weight = weight.saturating_add(text.len()),
                    Value::Array(values) => stack.push(Visit::Array(values.iter())),
                    Value::Object(fields) => stack.push(Visit::Object(fields.iter())),
                    _ => {}
                }
            }
            Visit::Array(mut values) => {
                if let Some(value) = values.next() {
                    stack.push(Visit::Array(values));
                    stack.push(Visit::Value(value));
                }
            }
            Visit::Object(mut fields) => {
                if let Some((key, value)) = fields.next() {
                    weight = weight.saturating_add(key.len());
                    stack.push(Visit::Object(fields));
                    stack.push(Visit::Value(value));
                }
            }
        }
    }
    crate::json::check_cancel(cancel)?;
    Ok(weight)
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
