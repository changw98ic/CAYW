//! Orchestrates search / fetch / extract into citable results.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::content::ContentStore;
use crate::error::WebAccessError;
use crate::extract::extract_auto;
use crate::provider::{HttpTransport, SearchProvider};
use crate::types::{
    Citation, ContentKind, ExtractResult, FailureClass, FetchResult, SearchHit, SearchResult,
    capabilities,
};

/// Bounds for one WebAccess service instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_body_bytes: usize,
    pub max_extract_chars: usize,
    pub default_search_limit: u32,
    pub max_search_limit: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_bytes: 2 * 1024 * 1024,
            max_extract_chars: 100_000,
            default_search_limit: 5,
            max_search_limit: 20,
        }
    }
}

/// Host-facing web-access facade. Does **not** implement or alias `search.query`.
pub struct WebAccess {
    transport: Arc<dyn HttpTransport>,
    search: Arc<dyn SearchProvider>,
    store: ContentStore,
    limits: Limits,
}

impl WebAccess {
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        search: Arc<dyn SearchProvider>,
        store: ContentStore,
        limits: Limits,
    ) -> Self {
        Self {
            transport,
            search,
            store,
            limits,
        }
    }

    pub fn with_fixtures(
        transport: impl HttpTransport + 'static,
        search: impl SearchProvider + 'static,
    ) -> Self {
        Self::new(
            Arc::new(transport),
            Arc::new(search),
            ContentStore::new(),
            Limits::default(),
        )
    }

    pub fn store(&self) -> &ContentStore {
        &self.store
    }

    pub fn with_sink(&self, sink: Arc<dyn crate::content::ContentSink>) -> Self {
        Self {
            transport: Arc::clone(&self.transport),
            search: Arc::clone(&self.search),
            store: ContentStore::with_sink(sink),
            limits: self.limits.clone(),
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Reject accidental use of the local-grep capability name.
    pub fn reject_local_search_query(name: &str) -> Result<(), WebAccessError> {
        if capabilities::is_local_search_query(name) {
            return Err(WebAccessError::new(
                FailureClass::InvalidUrl,
                "web-access refuses search.query; that name is reserved for local Workspace fs.grep — use web.search / web.fetch / web.extract",
            ));
        }
        Ok(())
    }

    pub async fn search(
        &self,
        query: &str,
        limit: Option<u32>,
    ) -> Result<SearchResult, WebAccessError> {
        let q = query.trim();
        if q.is_empty() || q.len() > 8192 {
            return Err(WebAccessError::new(
                FailureClass::InvalidUrl,
                "search query must be 1..=8192 bytes",
            ));
        }
        let limit = limit
            .unwrap_or(self.limits.default_search_limit)
            .clamp(1, self.limits.max_search_limit);
        let hits = self.search.search(q, limit).await?;
        let truncated = hits.len() as u32 >= limit;
        let citations = hits
            .iter()
            .map(|h| Citation {
                url: h.url.clone(),
                title: h.title.clone(),
                snippet: h.snippet.clone(),
                content_ref: None,
                provider: Some(self.search.name().into()),
            })
            .collect();
        Ok(SearchResult {
            query: q.to_string(),
            provider: self.search.name().to_string(),
            hits,
            citations,
            truncated,
        })
    }

    pub async fn fetch(&self, url: &str) -> Result<FetchResult, WebAccessError> {
        validate_http_url(url)?;
        let raw = self.transport.get(url, self.limits.max_body_bytes).await?;
        let kind = mime_kind(raw.mime_type.as_deref(), &raw.body);
        let text = match kind {
            ContentKind::BinaryUnsupported => None,
            _ => std::str::from_utf8(&raw.body).ok().map(str::to_string),
        };
        let content = if let Some(t) = text {
            self.store.put_text(
                kind,
                t,
                raw.mime_type.clone(),
                Some(raw.final_url.clone()),
                None,
            )
        } else {
            self.store.put_bytes(
                kind,
                raw.body.clone(),
                None,
                raw.mime_type.clone(),
                Some(raw.final_url.clone()),
                None,
            )
        }?;
        let citation = Citation {
            url: raw.final_url.clone(),
            title: None,
            snippet: None,
            content_ref: Some(content.id.clone()),
            provider: Some("fetch".into()),
        };
        Ok(FetchResult {
            url: url.to_string(),
            final_url: raw.final_url,
            status: raw.status,
            mime_type: raw.mime_type,
            content,
            citation,
        })
    }

    /// Fetch (if needed via URL) or extract from an already-stored content ref.
    pub async fn extract_url(&self, url: &str) -> Result<ExtractResult, WebAccessError> {
        validate_http_url(url)?;
        let raw = self.transport.get(url, self.limits.max_body_bytes).await?;
        self.extract_bytes(url, &raw.final_url, raw.mime_type.as_deref(), &raw.body)
    }

    pub fn extract_bytes(
        &self,
        request_url: &str,
        final_url: &str,
        mime: Option<&str>,
        bytes: &[u8],
    ) -> Result<ExtractResult, WebAccessError> {
        let (kind, title, text, truncated) =
            extract_auto(mime, bytes, self.limits.max_extract_chars)?;
        let content = self.store.put_text(
            kind,
            text.clone(),
            Some("text/plain; charset=utf-8".into()),
            Some(final_url.to_string()),
            title.clone(),
        )?;
        let snippet = text.chars().take(240).collect::<String>();
        let citation = Citation {
            url: final_url.to_string(),
            title: title.clone(),
            snippet: Some(snippet),
            content_ref: Some(content.id.clone()),
            provider: Some("extract".into()),
        };
        Ok(ExtractResult {
            url: request_url.to_string(),
            title,
            kind,
            text,
            content,
            citation,
            truncated,
        })
    }

    /// Fetch + extract, then return citation whose `content_ref` can feed later tools.
    pub async fn fetch_and_extract(&self, url: &str) -> Result<ExtractResult, WebAccessError> {
        self.extract_url(url).await
    }

    /// Dispatch by capability name. Refuses `search.query`.
    pub async fn call(
        &self,
        capability: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, WebAccessError> {
        Self::reject_local_search_query(capability)?;
        if !capabilities::is_web_access(capability) {
            return Err(WebAccessError::new(
                FailureClass::InvalidUrl,
                format!("unknown web-access capability: {capability}"),
            ));
        }
        match capability {
            capabilities::WEB_SEARCH => {
                let query = arguments
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        WebAccessError::new(FailureClass::InvalidUrl, "web.search requires query")
                    })?;
                let limit = arguments
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let result = self.search(query, limit).await?;
                Ok(serde_json::to_value(result).expect("serialize SearchResult"))
            }
            capabilities::WEB_FETCH => {
                let url = arguments
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        WebAccessError::new(FailureClass::InvalidUrl, "web.fetch requires url")
                    })?;
                let result = self.fetch(url).await?;
                if let Some(mut view) = self.store.get_handle_view(&result.content.id) {
                    view["url"] = serde_json::json!(result.url);
                    view["final_url"] = serde_json::json!(result.final_url);
                    view["status"] = serde_json::json!(result.status);
                    view["mime_type"] = serde_json::json!(result.mime_type);
                    view["kind"] = serde_json::json!(result.content.kind);
                    view["citation"] = serde_json::json!(result.citation);
                    return Ok(view);
                }
                Ok(serde_json::to_value(result).expect("serialize FetchResult"))
            }
            capabilities::WEB_EXTRACT => {
                let url = arguments
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        WebAccessError::new(FailureClass::InvalidUrl, "web.extract requires url")
                    })?;
                let result = self.extract_url(url).await?;
                if let Some(mut view) = self.store.get_handle_view(&result.content.id) {
                    view["url"] = serde_json::json!(result.url);
                    view["title"] = serde_json::json!(result.title);
                    view["kind"] = serde_json::json!(result.kind);
                    view["citation"] = serde_json::json!(result.citation);
                    view["truncated"] = serde_json::json!(result.truncated);
                    return Ok(view);
                }
                Ok(serde_json::to_value(result).expect("serialize ExtractResult"))
            }
            _ => unreachable!(),
        }
    }
}

fn validate_http_url(url: &str) -> Result<(), WebAccessError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        WebAccessError::new(FailureClass::InvalidUrl, format!("invalid URL: {e}")).with_url(url)
    })?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        other => Err(WebAccessError::new(
            FailureClass::UnsupportedScheme,
            format!("unsupported URL scheme {other}"),
        )
        .with_url(url)),
    }
}

fn mime_kind(mime: Option<&str>, bytes: &[u8]) -> ContentKind {
    let m = mime.map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    if m.contains("pdf") || bytes.starts_with(b"%PDF-") {
        ContentKind::PdfText
    } else if m.contains("html") || m.contains("xml") {
        ContentKind::Html
    } else if m.contains("json") {
        ContentKind::Json
    } else if m.starts_with("text/") || m.is_empty() {
        ContentKind::Text
    } else {
        ContentKind::BinaryUnsupported
    }
}

/// Helper for tests / demos: build a SearchHit.
pub fn hit(rank: u32, url: &str, title: &str, snippet: &str) -> SearchHit {
    SearchHit {
        url: url.into(),
        title: Some(title.into()),
        snippet: Some(snippet.into()),
        rank,
    }
}
