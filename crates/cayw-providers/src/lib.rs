//! Explicit HTTP implementations of [`cayw_ai::Provider`].
//!
//! OpenAI Chat Completions remains in `cayw_ai::providers::OpenAiProvider`. This
//! crate implements Anthropic Messages, native OpenAI Responses and the Gemini
//! Developer API; it never loads credentials.

mod anthropic;
mod common;
mod gemini;
mod json;
mod responses;
mod transport;

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, ModelRequest, Provider,
    StopReason,
};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::sync::Arc;
use transport::{HttpConfig, Protocol};

/// Configuration errors deliberately exclude credentials and URLs.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ProviderConfigError(String);

/// Anthropic Messages (`base_url` is normally `https://api.anthropic.com/v1`).
#[derive(Clone)]
pub struct AnthropicProvider {
    config: HttpConfig,
}

impl AnthropicProvider {
    /// Appends `/messages` to an explicit API base URL. Uses API version 2023-06-01.
    pub fn new(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderConfigError> {
        Ok(Self {
            config: HttpConfig::new(base_url.as_ref(), api_key.into())?,
        })
    }

    pub fn standard(api_key: impl Into<String>) -> Result<Self, ProviderConfigError> {
        Self::new("https://api.anthropic.com/v1", api_key)
    }
}

impl Provider for AnthropicProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        transport::stream(self.config.clone(), Protocol::Anthropic, request, cancel)
    }
}

/// Native OpenAI Responses SSE (`base_url` normally ends in `/v1`).
#[derive(Clone)]
pub struct ResponsesProvider {
    config: HttpConfig,
}

impl ResponsesProvider {
    pub fn new(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderConfigError> {
        Ok(Self {
            config: HttpConfig::new(base_url.as_ref(), api_key.into())?,
        })
    }

    pub fn standard(api_key: impl Into<String>) -> Result<Self, ProviderConfigError> {
        Self::new("https://api.openai.com/v1", api_key)
    }
}

impl Provider for ResponsesProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        transport::stream(self.config.clone(), Protocol::Responses, request, cancel)
    }
}

/// Both Gemini REST methods use the same normalized event contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GeminiMode {
    #[default]
    StreamGenerateContent,
    GenerateContent,
}

/// Gemini Developer API (`base_url` normally ends in `/v1beta`).
#[derive(Clone)]
pub struct GeminiProvider {
    config: HttpConfig,
    mode: GeminiMode,
    gateway_routes: bool,
}

impl GeminiProvider {
    /// Appends `/models/{request.model.id}:streamGenerateContent?alt=sse`.
    pub fn new(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderConfigError> {
        Ok(Self {
            config: HttpConfig::new(base_url.as_ref(), api_key.into())?,
            mode: GeminiMode::default(),
            gateway_routes: false,
        })
    }

    pub fn standard(api_key: impl Into<String>) -> Result<Self, ProviderConfigError> {
        Self::new("https://generativelanguage.googleapis.com/v1beta", api_key)
    }

    /// Enables the host's explicit Gemini gateway routes.
    pub fn with_gateway_routes(mut self) -> Self {
        self.gateway_routes = true;
        self
    }

    /// `GenerateContent` buffers a bounded JSON response, then emits content events.
    pub fn with_mode(mut self, mode: GeminiMode) -> Self {
        self.mode = mode;
        self
    }
}

impl Provider for GeminiProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        transport::stream(
            self.config.clone(),
            Protocol::Gemini(self.mode, self.gateway_routes),
            request,
            cancel,
        )
    }
}

/// Exact, host-owned routing. Any `Arc<dyn Provider>`, including the existing
/// OpenAI provider, can be registered. Unknown names never fall back to a model.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Duplicate and blank names are errors, so configuration cannot silently
    /// replace an existing provider. Register aliases explicitly if needed.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        provider: Arc<dyn Provider>,
    ) -> Result<(), ProviderConfigError> {
        let name = name.into();
        if name.trim().is_empty() || self.providers.contains_key(&name) {
            return Err(ProviderConfigError(
                "Provider name is empty or already registered".into(),
            ));
        }
        self.providers.insert(name, provider);
        Ok(())
    }
}

impl Provider for ProviderRegistry {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        if let Some(provider) = self.providers.get(&request.model.provider) {
            return provider.stream(request, cancel);
        }
        async_stream::stream! {
            let message = AssistantMessage::new(&request.model);
            yield AssistantEvent::Start { message: message.clone() };
            let (reason, error) = if cancel.is_cancelled() {
                (StopReason::Aborted, "Request cancelled")
            } else {
                (StopReason::Error, "Unknown provider; register the requested provider explicitly")
            };
            yield AssistantEvent::Error { message: message.failure(reason, error) };
        }
        .boxed()
    }
}
