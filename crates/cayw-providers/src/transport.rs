use crate::common::{self, Result};
use crate::{GeminiMode, ProviderConfigError, anthropic, gemini, responses};
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, ModelRequest, StopReason,
};
use futures::{StreamExt, stream::BoxStream};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, Url};
use std::time::Instant;

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_FRAMES: usize = 8192;
const DECODE_BATCH_BYTES: usize = 4096;

#[derive(Clone)]
pub(crate) struct HttpConfig {
    client: Client,
    base: Url,
    key: HeaderValue,
}

impl HttpConfig {
    pub fn new(base: &str, key: String) -> std::result::Result<Self, ProviderConfigError> {
        let base = Url::parse(base)
            .map_err(|_| ProviderConfigError("Invalid provider base URL".into()))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ProviderConfigError(
                "Base URL must be HTTP(S), without credentials, query or fragment".into(),
            ));
        }
        if key.trim().is_empty() {
            return Err(ProviderConfigError(
                "An explicit API key is required".into(),
            ));
        }
        let mut key = HeaderValue::from_str(&key)
            .map_err(|_| ProviderConfigError("API key is not a valid HTTP header value".into()))?;
        key.set_sensitive(true);
        // Keep redirects and automatic retries disabled. Environment proxy
        // discovery and TLS certificate verification stay at the reqwest default.
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| ProviderConfigError("Cannot create provider HTTP client".into()))?;
        Ok(Self { client, base, key })
    }

    fn endpoint(&self, protocol: Protocol, request: &ModelRequest) -> Result<Url> {
        let mut endpoint = self.base.clone();
        let suffix = match protocol {
            Protocol::Anthropic => "messages".to_owned(),
            Protocol::Responses => "responses".to_owned(),
            Protocol::Gemini(mode, gateway_routes) => {
                let id = gemini_model(&request.model.id, gateway_routes)?;
                let method = if mode == GeminiMode::GenerateContent {
                    "generateContent"
                } else {
                    "streamGenerateContent"
                };
                format!("models/{id}:{method}")
            }
        };
        endpoint.set_path(&format!(
            "{}/{}",
            endpoint.path().trim_end_matches('/'),
            suffix
        ));
        if matches!(
            protocol,
            Protocol::Gemini(GeminiMode::StreamGenerateContent, _)
        ) {
            endpoint.set_query(Some("alt=sse"));
        }
        Ok(endpoint)
    }
}

// The default Gemini route is one model segment, optionally prefixed by
// `models/`. Explicit gateway routing additionally permits a safe two-segment
// `provider/model` path; neither form accepts empty, `.` or `..` segments.
fn gemini_model(id: &str, gateway_routes: bool) -> Result<&str> {
    let id = id.strip_prefix("models/").unwrap_or(id);
    let segments = id.split('/').collect::<Vec<_>>();
    let limit = if gateway_routes { 2 } else { 1 };
    if segments.len() > limit
        || segments.iter().any(|segment| {
            segment.is_empty()
                || matches!(*segment, "." | "..")
                || !segment
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        })
    {
        return Err(if gateway_routes {
            "Gemini model ID must be a model name or safe provider/model route".into()
        } else {
            "Gemini model ID must be a model name or models/name".into()
        });
    }
    Ok(id)
}

#[derive(Clone, Copy)]
pub(crate) enum Protocol {
    Anthropic,
    Responses,
    Gemini(GeminiMode, bool),
}

// The protocol state owns normalized messages, not a network task. Dropping the
// returned stream drops send/read futures and sockets; nothing is spawned.
enum State {
    Anthropic(anthropic::State),
    Responses(responses::State),
    Gemini(gemini::State),
}

impl State {
    fn new(protocol: Protocol, request: &ModelRequest) -> Self {
        match protocol {
            Protocol::Anthropic => Self::Anthropic(anthropic::State::new(request)),
            Protocol::Responses => Self::Responses(responses::State::new(request)),
            Protocol::Gemini(..) => Self::Gemini(gemini::State::new(request)),
        }
    }
    fn message(&self) -> &AssistantMessage {
        match self {
            Self::Anthropic(s) => &s.message,
            Self::Responses(s) => &s.message,
            Self::Gemini(s) => &s.message,
        }
    }
    fn accept<'a>(
        &'a mut self,
        data: &'a str,
        event: Option<&'a str>,
        cancel: &'a CancellationToken,
    ) -> BoxStream<'a, Result<AssistantEvent>> {
        async_stream::try_stream! {
            let value = crate::json::parse_response(data, cancel).await?;
            match self {
                // One Anthropic wire event produces at most two snapshots.
                Self::Anthropic(state) => {
                    common::checkpoint(cancel).await?;
                    for update in state.accept(value, event, cancel).await? {
                        common::checkpoint(cancel).await?;
                        yield update;
                    }
                }
                Self::Responses(state) => {
                    let mut updates = state.accept(value, event, cancel);
                    while let Some(update) = updates.next().await {
                        yield update?;
                    }
                }
                Self::Gemini(state) => {
                    let mut updates = state.accept(value, cancel);
                    while let Some(update) = updates.next().await {
                        yield update?;
                    }
                }
            }
        }
        .boxed()
    }
    fn done(&self) -> bool {
        match self {
            Self::Anthropic(s) => s.done(),
            Self::Responses(s) => s.done(),
            Self::Gemini(_) => false,
        }
    }
    fn finish(&mut self) -> Result<()> {
        match self {
            Self::Anthropic(s) => s.finish(),
            Self::Responses(s) => s.finish(),
            Self::Gemini(s) => s.finish(),
        }
    }
}

pub(crate) fn stream(
    config: HttpConfig,
    protocol: Protocol,
    request: ModelRequest,
    cancel: CancellationToken,
) -> AssistantStream {
    async_stream::stream! {
        let started = Instant::now();
        let mut state = State::new(protocol, &request);
        yield AssistantEvent::Start { message: state.message().clone() };
        let response: Result<reqwest::Response> = async {
            if cancel.is_cancelled() { return Err("Request cancelled".into()); }
            let body = match protocol {
                Protocol::Anthropic => anthropic::request_body(&request)?,
                Protocol::Responses => responses::request_body(&request)?,
                Protocol::Gemini(..) => gemini::request_body(&request)?,
            };
            let endpoint = config.endpoint(protocol, &request)?;
            let json = matches!(protocol, Protocol::Gemini(GeminiMode::GenerateContent, _));
            let mut builder = config.client.post(endpoint).header(ACCEPT, if json { "application/json" } else { "text/event-stream" }).json(&body);
            builder = match protocol {
                Protocol::Anthropic => builder.header("x-api-key", config.key.clone()).header("anthropic-version", "2023-06-01"),
                Protocol::Responses => {
                    let mut bearer = Vec::with_capacity(7 + config.key.as_bytes().len());
                    bearer.extend_from_slice(b"Bearer ");
                    bearer.extend_from_slice(config.key.as_bytes());
                    let mut bearer = HeaderValue::from_bytes(&bearer).map_err(|_| "Invalid provider authorization header".to_owned())?;
                    bearer.set_sensitive(true);
                    builder.header(AUTHORIZATION, bearer)
                }
                Protocol::Gemini(..) => builder.header("x-goog-api-key", config.key.clone()),
            };
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err("Request cancelled".into()),
                response = builder.send() => response.map_err(|_| "Provider HTTP request failed")?,
            };
            if !response.status().is_success() {
                // Never return remote error bodies: a gateway may echo keys or URLs.
                return Err(format!("Provider returned HTTP {}", response.status().as_u16()));
            }
            let actual = response.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").split(';').next().unwrap_or("").trim();
            let expected = if json { "application/json" } else { "text/event-stream" };
            if !actual.eq_ignore_ascii_case(expected) { return Err("Provider returned an unexpected content type".into()); }
            Ok(response)
        }.await;
        let mut failure = None;
        if let Ok(response) = response {
            let mut body = response.bytes_stream();
            let mut decoder = SseDecoder::default();
            let mut json_body = Vec::new();
            let is_json = matches!(protocol, Protocol::Gemini(GeminiMode::GenerateContent, _));
            let mut received = 0usize;
            let mut frame_count = 0usize;
            'body: loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => { failure = Some("Request cancelled".to_owned()); break; }
                    next = body.next() => next,
                };
                let eof = next.is_none();
                let frames = match next {
                    Some(Err(_)) => { failure = Some("Provider response stream failed".into()); break; }
                    Some(Ok(bytes)) => {
                        if is_json {
                            received = received.saturating_add(bytes.len());
                            if received > MAX_JSON_BYTES { failure = Some("Provider response exceeds the size limit".into()); break; }
                            json_body.extend_from_slice(&bytes);
                            continue;
                        }
                        // Consume one frame at a time. Protocol completion must
                        // be observed before decoding or counting trailing bytes.
                        let mut offset = 0;
                        while offset < bytes.len() {
                            if let Err(error) = common::checkpoint(&cancel).await { failure = Some(error); break 'body; }
                            let count = (bytes.len() - offset)
                                .min(DECODE_BATCH_BYTES)
                                .min(MAX_RESPONSE_BYTES.saturating_sub(received) + 1);
                            let (consumed, frame) = match decoder.push(&bytes[offset..offset + count]) {
                                Ok(decoded) => decoded,
                                Err(error) => { failure = Some(error); break 'body; }
                            };
                            received += consumed;
                            offset += consumed;
                            if received > MAX_RESPONSE_BYTES { failure = Some("Provider response exceeds the size limit".into()); break 'body; }
                            if let Some(frame) = frame {
                                frame_count += 1;
                                if frame_count > MAX_RESPONSE_FRAMES { failure = Some("Provider response exceeds the frame limit".into()); break 'body; }
                                let mut updates = state.accept(&frame.data, frame.event.as_deref(), &cancel);
                                while let Some(update) = updates.next().await {
                                    match update {
                                        Ok(update) => {
                                            if cancel.is_cancelled() { failure = Some("Request cancelled".into()); break 'body; }
                                            yield update;
                                        }
                                        Err(error) => { failure = Some(error); break 'body; }
                                    }
                                }
                                drop(updates);
                                if state.done() { break 'body; }
                            }
                        }
                        continue;
                    }
                    None if is_json => {
                        match std::str::from_utf8(&json_body) {
                            Ok(data) => Ok(vec![Frame { data: data.to_owned(), event: None }]),
                            Err(_) => Err("Invalid UTF-8 in provider JSON".into()),
                        }
                    }
                    None => decoder.finish(),
                };
                let frames = match frames { Ok(frames) => frames, Err(error) => { failure = Some(error); break; } };
                for frame in frames {
                    frame_count += 1;
                    if frame_count > MAX_RESPONSE_FRAMES { failure = Some("Provider response exceeds the frame limit".into()); break 'body; }
                    if let Err(error) = common::checkpoint(&cancel).await { failure = Some(error); break 'body; }
                    let mut updates = state.accept(&frame.data, frame.event.as_deref(), &cancel);
                    while let Some(update) = updates.next().await {
                        match update {
                            Ok(update) => {
                                if cancel.is_cancelled() { failure = Some("Request cancelled".into()); break 'body; }
                                yield update;
                            }
                            Err(error) => { failure = Some(error); break 'body; }
                        }
                    }
                }
                if state.done() || eof { break; }
            }
            // Release the HTTP response before yielding a terminal event, even
            // when a consumer retains the stream without polling it again.
            drop(body);
            if failure.is_none() && let Err(error) = state.finish() { failure = Some(error); }
        } else if let Err(error) = response {
            failure = Some(error);
        }
        if cancel.is_cancelled() { failure = Some("Request cancelled".into()); }
        let mut message = state.message().clone();
        message.duration_ms = Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        if let Some(error) = failure {
            let reason = if cancel.is_cancelled() { StopReason::Aborted } else { StopReason::Error };
            yield AssistantEvent::Error { message: message.failure(reason, error) };
        } else {
            yield AssistantEvent::Done { message };
        }
    }.boxed()
}

struct Frame {
    data: String,
    event: Option<String>,
}

/// Byte framing happens before UTF-8 decoding. CR, LF, CRLF, comments, a BOM,
/// multiline data and a final unterminated frame all have deterministic handling.
#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    data: Vec<String>,
    event: Option<String>,
    skip_lf: bool,
    bytes: usize,
    lines: usize,
}

impl SseDecoder {
    fn push(&mut self, input: &[u8]) -> Result<(usize, Option<Frame>)> {
        for (index, &byte) in input.iter().enumerate() {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.bytes += 1;
            if self.bytes > MAX_EVENT_BYTES {
                return Err("SSE event exceeds the 8 MiB limit".into());
            }
            let frame = match byte {
                b'\r' => {
                    let frame = self.end_line()?;
                    self.skip_lf = true;
                    frame
                }
                b'\n' => self.end_line()?,
                _ => {
                    self.line.push(byte);
                    None
                }
            };
            if let Some(frame) = frame {
                return Ok((index + 1, Some(frame)));
            }
        }
        Ok((input.len(), None))
    }
    fn finish(&mut self) -> Result<Vec<Frame>> {
        let mut frames = Vec::new();
        if !self.line.is_empty()
            && let Some(frame) = self.end_line()?
        {
            frames.push(frame);
        }
        if let Some(frame) = self.dispatch() {
            frames.push(frame);
        }
        Ok(frames)
    }
    fn end_line(&mut self) -> Result<Option<Frame>> {
        let line = std::mem::take(&mut self.line);
        let mut line = std::str::from_utf8(&line).map_err(|_| "Invalid UTF-8 in SSE")?;
        if self.lines == 0 {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        self.lines = self.lines.saturating_add(1);
        if line.is_empty() {
            self.bytes = 0;
            return Ok(self.dispatch());
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => self.data.push(value.to_owned()),
                "event" => self.event = (!value.is_empty()).then(|| value.to_owned()),
                _ => {}
            }
        }
        Ok(None)
    }
    fn dispatch(&mut self) -> Option<Frame> {
        let event = self.event.take();
        (!self.data.is_empty()).then(|| Frame {
            data: std::mem::take(&mut self.data).join("\n"),
            event,
        })
    }
}
