//! Unified Provider surface for search / fetch / extract.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use crate::error::WebAccessError;
use crate::types::{FailureClass, SearchHit};

/// Async result alias used by provider futures.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Raw HTTP response returned by a transport.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub final_url: String,
    pub mime_type: Option<String>,
    pub body: Vec<u8>,
}

/// Pluggable HTTP transport. Offline fixtures implement this without sockets.
pub trait HttpTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<RawResponse, WebAccessError>>;
}

/// Pluggable web search backend.
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, WebAccessError>>;
}

/// In-memory fixture transport: URL → canned body. Missing URLs fail as Offline.
#[derive(Debug, Clone)]
pub struct FixtureTransport {
    pages: HashMap<String, RawResponse>,
    pub offline: bool,
}

impl Default for FixtureTransport {
    fn default() -> Self {
        let mut transport = Self {
            pages: HashMap::new(),
            offline: false,
        };
        transport.pages.insert(
            "https://example.com/".into(),
            RawResponse {
                status: 200,
                final_url: "https://example.com/".into(),
                mime_type: Some("text/html; charset=utf-8".into()),
                body: b"<!doctype html><html><head><title>Example Domain</title></head><body><h1>Example Domain</h1><p>This domain is for use in documentation examples.</p></body></html>".to_vec(),
            },
        );
        transport
    }
}

impl FixtureTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_html(mut self, url: impl Into<String>, html: impl Into<String>) -> Self {
        let url = url.into();
        let body = html.into().into_bytes();
        self.pages.insert(
            url.clone(),
            RawResponse {
                status: 200,
                final_url: url,
                mime_type: Some("text/html; charset=utf-8".into()),
                body,
            },
        );
        self
    }

    pub fn insert_bytes(
        mut self,
        url: impl Into<String>,
        mime: impl Into<String>,
        body: Vec<u8>,
    ) -> Self {
        let url = url.into();
        self.pages.insert(
            url.clone(),
            RawResponse {
                status: 200,
                final_url: url,
                mime_type: Some(mime.into()),
                body,
            },
        );
        self
    }

    pub fn offline(mut self) -> Self {
        self.offline = true;
        self
    }
}

impl HttpTransport for FixtureTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<RawResponse, WebAccessError>> {
        Box::pin(async move {
            if self.offline {
                return Err(
                    WebAccessError::offline("fixture transport is in offline mode")
                        .with_url(url)
                        .with_provider("fixture"),
                );
            }
            let Some(page) = self.pages.get(url) else {
                return Err(WebAccessError::new(
                    FailureClass::NotFound,
                    format!("no fixture for URL {url}"),
                )
                .with_url(url)
                .with_provider("fixture"));
            };
            if page.body.len() > max_bytes {
                return Err(WebAccessError::new(
                    FailureClass::BodyTooLarge,
                    format!("body {} exceeds max_bytes {max_bytes}", page.body.len()),
                )
                .with_url(url));
            }
            Ok(page.clone())
        })
    }
}

/// Fixture search provider with a fixed hit table keyed by query substring.
#[derive(Debug, Default, Clone)]
pub struct FixtureSearch {
    hits: HashMap<String, Vec<SearchHit>>,
    pub offline: bool,
}

impl FixtureSearch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(mut self, query: impl Into<String>, hits: Vec<SearchHit>) -> Self {
        self.hits.insert(query.into(), hits);
        self
    }

    pub fn offline(mut self) -> Self {
        self.offline = true;
        self
    }
}

impl SearchProvider for FixtureSearch {
    fn name(&self) -> &str {
        "fixture"
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, WebAccessError>> {
        Box::pin(async move {
            if self.offline {
                return Err(
                    WebAccessError::offline("fixture search provider is offline")
                        .with_provider("fixture"),
                );
            }
            let mut hits = self
                .hits
                .get(query)
                .cloned()
                .or_else(|| {
                    self.hits.iter().find_map(|(k, v)| {
                        if query.contains(k) || k.contains(query) {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                })
                .unwrap_or_default();
            if hits.len() as u32 > limit {
                hits.truncate(limit as usize);
            }
            Ok(hits)
        })
    }
}

/// Live `reqwest` transport (used when the host opts into network).
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, WebAccessError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("cayw-web-access/0.1 (F10 first cut)")
            .build()
            .map_err(|e| {
                WebAccessError::new(
                    FailureClass::ProviderUnavailable,
                    format!("failed to build HTTP client: {e}"),
                )
                .with_provider("reqwest")
            })?;
        Ok(Self { client })
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new().expect("reqwest client")
    }
}

impl HttpTransport for ReqwestTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<RawResponse, WebAccessError>> {
        Box::pin(async move {
            let parsed = url::Url::parse(url).map_err(|e| {
                WebAccessError::new(FailureClass::InvalidUrl, format!("invalid URL: {e}"))
                    .with_url(url)
            })?;
            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                return Err(WebAccessError::new(
                    FailureClass::UnsupportedScheme,
                    format!("unsupported URL scheme {}", parsed.scheme()),
                )
                .with_url(url));
            }
            let response = self.client.get(parsed.clone()).send().await.map_err(|e| {
                let class = if e.is_timeout() {
                    FailureClass::Timeout
                } else if e.is_connect() {
                    FailureClass::Dns
                } else if e.is_request() {
                    FailureClass::InvalidUrl
                } else {
                    FailureClass::Offline
                };
                WebAccessError::new(class, format!("HTTP GET failed: {e}"))
                    .with_url(url)
                    .with_provider("reqwest")
            })?;
            let status = response.status().as_u16();
            let final_url = response.url().to_string();
            let mime_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = response.bytes().await.map_err(|e| {
                WebAccessError::new(FailureClass::Offline, format!("failed to read body: {e}"))
                    .with_url(url)
                    .with_provider("reqwest")
            })?;
            if body.len() > max_bytes {
                return Err(WebAccessError::new(
                    FailureClass::BodyTooLarge,
                    format!("body {} exceeds max_bytes {max_bytes}", body.len()),
                )
                .with_url(url)
                .with_status(status));
            }
            if !(200..300).contains(&status) {
                return Err(WebAccessError::new(
                    FailureClass::HttpStatus,
                    format!("HTTP status {status}"),
                )
                .with_url(final_url.clone())
                .with_status(status)
                .with_provider("reqwest"));
            }
            Ok(RawResponse {
                status,
                final_url,
                mime_type,
                body: body.to_vec(),
            })
        })
    }
}
