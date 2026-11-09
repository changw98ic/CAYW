//! One explicit model-service configuration for a headless host.
use crate::RuntimeError;
use cayw_ai::{Model, Provider, providers::OpenAiProvider};
use cayw_providers::{AnthropicProvider, GeminiMode, GeminiProvider, ResponsesProvider};
use reqwest::{Url, header::HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

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

/// One model-service configuration. An optional credential is stored directly in
/// the same private local file. Absence explicitly selects a service with no authentication.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    service: ProviderService,
    api: ProviderApi,
    id: String,
    base_url: String,
    api_version: String,
    max_output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

/// Host-selected adapter and model, ready for the existing `Runtime::open` interface.
#[derive(Clone)]
pub struct ConfiguredProvider {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
}

fn invalid(reason: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("model configuration {reason}"))
}

impl ProviderConfig {
    /// Reject unsupported and contradictory settings before loading a key or making requests.
    ///
    /// `service` selects the wire protocol adapter. `base_url` is an explicit
    /// host choice: any deployment prefix, hostname and port are accepted so a
    /// host can select a gateway or a local fixture. Adapting the path does not
    /// mark that deployment as verified; see the provider README.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let expected_version = match (self.service, self.api) {
            (ProviderService::Openai, ProviderApi::ChatCompletions | ProviderApi::Responses) => {
                "v1"
            }
            (ProviderService::Magpie, ProviderApi::ChatCompletions | ProviderApi::Responses) => {
                "v1"
            }
            (ProviderService::Magpie, ProviderApi::Messages) => "2023-06-01",
            (
                ProviderService::Magpie,
                ProviderApi::StreamGenerateContent | ProviderApi::GenerateContent,
            ) => "v1beta",
            (ProviderService::Anthropic, ProviderApi::Messages) => "2023-06-01",
            (
                ProviderService::Gemini,
                ProviderApi::StreamGenerateContent | ProviderApi::GenerateContent,
            ) => "v1beta",
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
        if let Some(key) = &self.api_key
            && (key.trim().is_empty() || key.len() > 4096 || HeaderValue::from_str(key).is_err())
        {
            return Err(invalid(
                "api_key must be a nonempty HTTP header value of at most 4096 bytes",
            ));
        }
        let base = Url::parse(&self.base_url).map_err(|_| invalid("base_url is invalid"))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(invalid(
                "base_url must be an absolute HTTP(S) URL without credentials, query or fragment",
            ));
        }
        // The host explicitly selected this deployment. The adapter appends its
        // protocol suffix to the existing base path. Acceptance is not verification.
        Ok(())
    }

    /// The explicit model name selected by the host.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the private configuration contains a credential value.
    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// Apply an explicit settings update without exposing a stored credential to the caller.
    /// Omitting a key preserves it only while the credential destination is unchanged.
    pub fn merge_saved_api_key(
        &mut self,
        previous: Option<&Self>,
        clear_api_key: bool,
    ) -> Result<(), RuntimeError> {
        if clear_api_key {
            if self.api_key.is_some() {
                return Err(invalid("cannot set and clear api_key in the same update"));
            }
            return Ok(());
        }
        if self.api_key.is_some() {
            return Ok(());
        }
        let Some(previous) = previous.filter(|config| config.api_key.is_some()) else {
            return Ok(());
        };
        if self.service != previous.service
            || self.api != previous.api
            || self.base_url != previous.base_url
            || self.api_version != previous.api_version
        {
            return Err(invalid(
                "changed the credential destination; provide a new api_key or explicitly clear the saved key",
            ));
        }
        self.api_key.clone_from(&previous.api_key);
        Ok(())
    }

    /// A diagnostic/settings view that never returns the stored credential.
    pub fn public_view(&self) -> Value {
        json!({
            "service": self.service,
            "api": self.api,
            "id": self.id,
            "base_url": self.base_url,
            "api_version": self.api_version,
            "max_output_tokens": self.max_output_tokens,
            "authentication": if self.api_key.is_some() { "plain_file" } else { "none" },
        })
    }

    /// Construct a lazy adapter without making a model request. A missing key means
    /// this configured local service explicitly uses no authentication.
    pub fn build(&self) -> Result<ConfiguredProvider, RuntimeError> {
        self.validate()?;
        let provider: Arc<dyn Provider> = match self.api {
            ProviderApi::ChatCompletions => {
                // Keep redirects and automatic retries off. Environment proxy
                // discovery and TLS certificate verification stay at the reqwest default.
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .build()
                    .map_err(|_| invalid("cannot create the HTTP client"))?;
                let adapter = match &self.api_key {
                    Some(key) => OpenAiProvider::new(&self.base_url, key.clone()),
                    None => OpenAiProvider::without_auth(&self.base_url),
                }
                .map_err(|_| invalid("cannot construct the selected adapter"))?;
                Arc::new(adapter.with_client(client))
            }
            ProviderApi::Responses => Arc::new(
                match &self.api_key {
                    Some(key) => ResponsesProvider::new(&self.base_url, key.clone()),
                    None => ResponsesProvider::without_auth(&self.base_url),
                }
                .map_err(|_| invalid("cannot construct the selected adapter"))?,
            ),
            ProviderApi::Messages => Arc::new(
                match &self.api_key {
                    Some(key) => AnthropicProvider::new(&self.base_url, key.clone()),
                    None => AnthropicProvider::without_auth(&self.base_url),
                }
                .map_err(|_| invalid("cannot construct the selected adapter"))?,
            ),
            ProviderApi::StreamGenerateContent | ProviderApi::GenerateContent => {
                let mut adapter = match &self.api_key {
                    Some(key) => GeminiProvider::new(&self.base_url, key.clone()),
                    None => GeminiProvider::without_auth(&self.base_url),
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(base_url: &str, api_key: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            service: ProviderService::Openai,
            api: ProviderApi::ChatCompletions,
            id: "example-model".into(),
            base_url: base_url.into(),
            api_version: "v1".into(),
            max_output_tokens: 4096,
            api_key: api_key.map(str::to_owned),
        }
    }

    #[test]
    fn omitted_api_key_preserves_the_same_credential_destination() {
        let previous = profile("https://api.example/v1", Some("saved-secret"));
        let mut update = profile("https://api.example/v1", None);
        update.merge_saved_api_key(Some(&previous), false).unwrap();
        assert!(update.has_api_key());
        assert_eq!(update.public_view()["authentication"], "plain_file");
    }

    #[test]
    fn clearing_api_key_is_explicit_and_cannot_set_a_replacement() {
        let previous = profile("https://api.example/v1", Some("saved-secret"));
        let mut cleared = profile("https://api.example/v1", None);
        cleared.merge_saved_api_key(Some(&previous), true).unwrap();
        assert!(!cleared.has_api_key());

        let mut contradictory = profile("https://api.example/v1", Some("replacement"));
        assert!(
            contradictory
                .merge_saved_api_key(Some(&previous), true)
                .is_err()
        );
    }

    #[test]
    fn omitted_api_key_cannot_cross_adapter_or_version_boundaries() {
        let previous = profile("https://api.example/v1", Some("saved-secret"));
        let mut changed_api = profile("https://api.example/v1", None);
        changed_api.api = ProviderApi::Responses;
        assert!(
            changed_api
                .merge_saved_api_key(Some(&previous), false)
                .is_err()
        );
        assert!(!changed_api.has_api_key());
        let mut changed_version = profile("https://api.example/v1", None);
        changed_version.api_version = "v2".into();
        assert!(
            changed_version
                .merge_saved_api_key(Some(&previous), false)
                .is_err()
        );
        assert!(!changed_version.has_api_key());
    }

    #[test]
    fn omitted_api_key_cannot_move_a_saved_secret_to_another_endpoint() {
        let previous = profile("https://api.example/v1", Some("saved-secret"));
        let mut update = profile("https://other.example/v1", None);
        let error = update
            .merge_saved_api_key(Some(&previous), false)
            .unwrap_err();
        assert!(error.to_string().contains("credential destination"));
        assert!(!update.has_api_key());
    }
}
