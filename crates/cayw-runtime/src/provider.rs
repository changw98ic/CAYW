//! One explicit model-service configuration for a headless host.
use crate::RuntimeError;
use cayw_ai::{Model, Provider, providers::OpenAiProvider};
use cayw_providers::{AnthropicProvider, GeminiMode, GeminiProvider, ResponsesProvider};
use reqwest::{Url, header::HeaderValue};
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderService {
    Openai,
    Anthropic,
    Gemini,
    Magpie,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApi {
    ChatCompletions,
    Responses,
    Messages,
    StreamGenerateContent,
    GenerateContent,
}

/// Configuration contains the credential's environment variable name, never its value.
/// All fields are explicit. Loading and validating this value does not read credentials.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    service: ProviderService,
    api: ProviderApi,
    id: String,
    base_url: String,
    api_version: String,
    max_output_tokens: u64,
    api_key_env: String,
}

/// Host-selected adapter and model, ready for the existing `Runtime::open` interface.
pub struct ConfiguredProvider {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
}

fn invalid(reason: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("model configuration {reason}"))
}

impl ProviderConfig {
    /// Reject unsupported and contradictory settings before loading a key or making requests.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let (expected_version, path, official_host) = match (self.service, self.api) {
            (ProviderService::Openai, ProviderApi::ChatCompletions | ProviderApi::Responses) => {
                ("v1", "/v1", Some("api.openai.com"))
            }
            (ProviderService::Magpie, ProviderApi::ChatCompletions | ProviderApi::Responses) => {
                ("v1", "/v1", None)
            }
            (ProviderService::Magpie, ProviderApi::Messages) => ("2023-06-01", "/v1", None),
            (
                ProviderService::Magpie,
                ProviderApi::StreamGenerateContent | ProviderApi::GenerateContent,
            ) => ("v1beta", "/v1beta", None),
            (ProviderService::Anthropic, ProviderApi::Messages) => {
                ("2023-06-01", "/v1", Some("api.anthropic.com"))
            }
            (
                ProviderService::Gemini,
                ProviderApi::StreamGenerateContent | ProviderApi::GenerateContent,
            ) => (
                "v1beta",
                "/v1beta",
                Some("generativelanguage.googleapis.com"),
            ),
            _ => return Err(invalid("service and API do not match")),
        };
        if self.api_version != expected_version {
            return Err(invalid(
                "API version is unsupported for the selected adapter",
            ));
        }
        if self.max_output_tokens == 0 {
            return Err(invalid("max_output_tokens must be positive"));
        }
        if self.id.is_empty()
            || self.id.len() > 256
            || !self.id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
            })
        {
            return Err(invalid("model id must be an explicit model name"));
        }
        if matches!(
            self.api,
            ProviderApi::StreamGenerateContent | ProviderApi::GenerateContent
        ) {
            let id = self.id.strip_prefix("models/").unwrap_or(&self.id);
            let segments: Vec<_> = id.split('/').collect();
            let max_segments = if self.service == ProviderService::Magpie {
                2
            } else {
                1
            };
            if segments.len() > max_segments
                || segments.iter().any(|segment| {
                    segment.is_empty()
                        || matches!(*segment, "." | "..")
                        || !segment.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                        })
                })
            {
                return Err(invalid("Gemini model id has unsupported path segments"));
            }
        }
        if self.api_key_env.len() > 128
            || !self
                .api_key_env
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !self
                .api_key_env
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(invalid("api_key_env must be an environment variable name"));
        }
        let base = Url::parse(&self.base_url).map_err(|_| invalid("base_url is invalid"))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path().trim_end_matches('/') != path
        {
            return Err(invalid(
                "base_url must name the selected API root without credentials, query or fragment",
            ));
        }
        // Only numeric loopback addresses are local fixtures. Do not resolve DNS
        // to decide whether a non-official endpoint counts as an official service.
        let loopback = base.host_str().is_some_and(|host| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        if !loopback
            && (base.scheme() != "https"
                || official_host.is_some_and(|host| {
                    base.host_str() != Some(host) || base.port_or_known_default() != Some(443)
                }))
        {
            return Err(invalid(
                "base_url does not match the selected service; HTTP is allowed only on numeric loopback",
            ));
        }
        Ok(())
    }

    /// Read only the selected credential. This constructs a lazy adapter, without a model request.
    pub fn build(&self) -> Result<ConfiguredProvider, RuntimeError> {
        self.build_with_environment(|name| std::env::var(name).ok())
    }

    /// Use a host-owned credential lookup instead of reading the process environment.
    /// Validation precedes the single lookup; errors never include credential values.
    pub fn build_with_environment(
        &self,
        mut environment: impl FnMut(&str) -> Option<String>,
    ) -> Result<ConfiguredProvider, RuntimeError> {
        self.validate()?;
        let key = environment(&self.api_key_env)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                invalid(&format!(
                    "requires the credential named by {}",
                    self.api_key_env
                ))
            })?;
        HeaderValue::from_str(&key)
            .map_err(|_| invalid("credential is not a valid HTTP header value"))?;
        let provider: Arc<dyn Provider> = match self.api {
            ProviderApi::ChatCompletions => {
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .build()
                    .map_err(|_| invalid("cannot create the HTTP client"))?;
                Arc::new(
                    OpenAiProvider::new(&self.base_url, key)
                        .map_err(|_| invalid("cannot construct the selected adapter"))?
                        .with_client(client),
                )
            }
            ProviderApi::Responses => Arc::new(
                ResponsesProvider::new(&self.base_url, key)
                    .map_err(|_| invalid("cannot construct the selected adapter"))?,
            ),
            ProviderApi::Messages => Arc::new(
                AnthropicProvider::new(&self.base_url, key)
                    .map_err(|_| invalid("cannot construct the selected adapter"))?,
            ),
            ProviderApi::StreamGenerateContent | ProviderApi::GenerateContent => {
                let mut adapter = GeminiProvider::new(&self.base_url, key)
                    .map_err(|_| invalid("cannot construct the selected adapter"))?
                    .with_mode(if self.api == ProviderApi::GenerateContent {
                        GeminiMode::GenerateContent
                    } else {
                        GeminiMode::StreamGenerateContent
                    });
                if self.service == ProviderService::Magpie {
                    adapter = adapter.with_gateway_routes();
                }
                Arc::new(adapter)
            }
        };
        let name = match self.service {
            ProviderService::Openai => "openai",
            ProviderService::Anthropic => "anthropic",
            ProviderService::Gemini => "gemini",
            ProviderService::Magpie => "magpie",
        };
        let mut model = Model::new(name, &self.id);
        model.max_output_tokens = Some(self.max_output_tokens);
        Ok(ConfiguredProvider { provider, model })
    }
}
