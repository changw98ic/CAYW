//! Optional Brave Search API backend (env-gated key).

use std::sync::Arc;

use crate::error::WebAccessError;
use crate::provider::{BoxFuture, SearchProvider};
use crate::types::{FailureClass, SearchHit};

const PROVIDER: &str = "brave";
const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
/// Environment variable for the Brave Search subscription token.
pub const BRAVE_API_KEY_ENV: &str = "BRAVE_SEARCH_API_KEY";

/// Brave web search when a subscription token is available.
///
/// The HTTP transport must attach the `X-Subscription-Token` header. Prefer
/// [`BraveSearch::from_env`] which builds a dedicated reqwest client, or use
/// [`crate::host::build_search`] which selects Brave only when the env key is set.
pub struct BraveSearch {
    client: reqwest::Client,
    api_key: String,
    max_fetch_bytes: usize,
}

impl BraveSearch {
    pub fn new(api_key: impl Into<String>) -> Result<Self, WebAccessError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("cayw-web-access/0.1 (F10)")
            .build()
            .map_err(|e| {
                WebAccessError::new(
                    FailureClass::ProviderUnavailable,
                    format!("brave client: {e}"),
                )
                .with_provider(PROVIDER)
            })?;
        Ok(Self {
            client,
            api_key: api_key.into(),
            max_fetch_bytes: 2 * 1024 * 1024,
        })
    }

    /// Returns `Ok(None)` when the env key is unset/empty (caller should fall back).
    pub fn from_env() -> Result<Option<Self>, WebAccessError> {
        match std::env::var(BRAVE_API_KEY_ENV) {
            Ok(key) if !key.trim().is_empty() => Ok(Some(Self::new(key.trim())?)),
            _ => Ok(None),
        }
    }
}

impl SearchProvider for BraveSearch {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, WebAccessError>> {
        Box::pin(async move {
            let mut url = url::Url::parse(ENDPOINT).map_err(|e| {
                WebAccessError::new(FailureClass::InvalidUrl, format!("brave endpoint: {e}"))
                    .with_provider(PROVIDER)
            })?;
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("count", &limit.clamp(1, 20).to_string());
            let response = self
                .client
                .get(url)
                .header("X-Subscription-Token", &self.api_key)
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|e| {
                    WebAccessError::new(FailureClass::Offline, format!("brave GET failed: {e}"))
                        .with_provider(PROVIDER)
                })?;
            let status = response.status().as_u16();
            let body = response.bytes().await.map_err(|e| {
                WebAccessError::new(FailureClass::Offline, format!("brave body: {e}"))
                    .with_provider(PROVIDER)
            })?;
            if body.len() > self.max_fetch_bytes {
                return Err(WebAccessError::new(
                    FailureClass::BodyTooLarge,
                    format!("brave body {} exceeds max", body.len()),
                )
                .with_provider(PROVIDER)
                .with_status(status));
            }
            if !(200..300).contains(&status) {
                return Err(WebAccessError::new(
                    FailureClass::HttpStatus,
                    format!("brave HTTP {status}"),
                )
                .with_provider(PROVIDER)
                .with_status(status));
            }
            parse_brave_json(&body, limit)
        })
    }
}

fn parse_brave_json(bytes: &[u8], limit: u32) -> Result<Vec<SearchHit>, WebAccessError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        WebAccessError::new(FailureClass::ExtractFailed, format!("brave JSON: {e}"))
            .with_provider(PROVIDER)
    })?;
    let mut hits = Vec::new();
    let results = value
        .pointer("/web/results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for (i, item) in results.into_iter().enumerate() {
        if hits.len() as u32 >= limit {
            break;
        }
        let url = item
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        hits.push(SearchHit {
            url,
            title: item
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            snippet: item
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            rank: (i as u32) + 1,
        });
    }
    Ok(hits)
}

/// Prefer Brave when configured; otherwise use the provided fallback (typically DDG HTML).
pub struct BraveOrFallback {
    brave: Option<BraveSearch>,
    fallback: Arc<dyn SearchProvider>,
}

impl BraveOrFallback {
    pub fn new(brave: Option<BraveSearch>, fallback: Arc<dyn SearchProvider>) -> Self {
        Self { brave, fallback }
    }
}

impl SearchProvider for BraveOrFallback {
    fn name(&self) -> &str {
        if self.brave.is_some() {
            PROVIDER
        } else {
            self.fallback.name()
        }
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, WebAccessError>> {
        Box::pin(async move {
            if let Some(brave) = &self.brave {
                match brave.search(query, limit).await {
                    Ok(hits) if !hits.is_empty() => return Ok(hits),
                    Ok(_) => { /* empty → fallback */ }
                    Err(_) => { /* soft-fallback to DDG/fixture */ }
                }
            }
            self.fallback.search(query, limit).await
        })
    }
}
