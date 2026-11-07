//! Serializable DTOs for F10 web-access first cut.

use serde::{Deserialize, Serialize};

/// Capability names for the unified web-access tool surface.
///
/// These are intentionally **not** `search.query`. Existing `search.query` remains
/// the host-owned local Workspace `fs.grep` bridge and must not be overloaded.
pub mod capabilities {
    pub const WEB_SEARCH: &str = "web.search";
    pub const WEB_FETCH: &str = "web.fetch";
    pub const WEB_EXTRACT: &str = "web.extract";

    /// Names this crate owns. Never includes `search.query`.
    pub const ALL: &[&str] = &[WEB_SEARCH, WEB_FETCH, WEB_EXTRACT];

    pub fn is_web_access(name: &str) -> bool {
        ALL.contains(&name)
    }

    pub fn is_local_search_query(name: &str) -> bool {
        name == "search.query"
    }
}

/// Kind of extracted / stored content body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Html,
    Text,
    PdfText,
    Json,
    BinaryUnsupported,
}

/// Fetched/extracted body metadata. With a Device sink, `id` is the formal
/// result handle; a standalone service owns only process-local references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentRef {
    pub id: String,
    pub kind: ContentKind,
    pub content_sha256: String,
    pub byte_len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// One citable source attached to a search hit or extracted document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// One web search hit before optional fetch/extract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchHit {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    pub rank: u32,
}

/// Result of `web.search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchResult {
    pub query: String,
    pub provider: String,
    pub hits: Vec<SearchHit>,
    pub citations: Vec<Citation>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_reason: Option<SearchEmptyReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<SearchEvidence>,
}

/// Structured response returned by a [`crate::provider::SearchProvider`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_reason: Option<SearchEmptyReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<SearchEvidence>,
}

impl SearchResponse {
    pub fn new(hits: Vec<SearchHit>) -> Self {
        Self {
            hits,
            empty_reason: None,
            evidence: None,
        }
    }

    pub fn empty(reason: SearchEmptyReason, evidence: Option<SearchEvidence>) -> Self {
        Self {
            hits: Vec::new(),
            empty_reason: Some(reason),
            evidence,
        }
    }
}

impl std::ops::Deref for SearchResponse {
    type Target = [SearchHit];
    fn deref(&self) -> &Self::Target {
        &self.hits
    }
}

/// Classification of empty search results.
///
/// Distinguishes true empty (query genuinely produced no results), challenges (anti-bot / CAPTCHA),
/// structural mismatch (markup layout drift), and HTTP errors.
/// When no HTTP original text is available, empty results must not be attributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchEmptyReason {
    /// 真空: Query genuinely produced zero results.
    TrueEmpty,
    /// 挑战: Anti-bot / CAPTCHA / rate-limiting challenge encountered.
    Challenge,
    /// 结构失配: Page returned without results and without matching known no-results/challenge patterns.
    StructuralMismatch,
    /// HTTP 错误: Transport or non-2xx HTTP status error.
    HttpError,
    /// Missing HTTP response text leaves the empty reason unattributed.
    Unattributed,
}

impl std::fmt::Display for SearchEmptyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TrueEmpty => write!(f, "true_empty"),
            Self::Challenge => write!(f, "challenge"),
            Self::StructuralMismatch => write!(f, "structural_mismatch"),
            Self::HttpError => write!(f, "http_error"),
            Self::Unattributed => write!(f, "unattributed"),
        }
    }
}

/// Bounded evidence and diagnostics attached to a search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// Bounded summary extracted from the response page (e.g. no-results or challenge message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_summary: Option<String>,
    /// Bounded slice of the raw HTTP response body text for evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_snippet: Option<String>,
}

/// Result of `web.fetch` (raw transport + stored body).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchResult {
    pub url: String,
    pub final_url: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    pub content: ContentRef,
    pub citation: Citation,
}

/// Result of `web.extract` (body / PDF text + citation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractResult {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub kind: ContentKind,
    /// Extracted plain text (HTML body text or PDF text stream).
    pub text: String,
    pub content: ContentRef,
    pub citation: Citation,
    pub truncated: bool,
}

/// Machine-readable offline / network failure class for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Offline,
    Dns,
    Timeout,
    HttpStatus,
    InvalidUrl,
    UnsupportedScheme,
    BodyTooLarge,
    ExtractFailed,
    ProviderUnavailable,
    Cancelled,
    NotFound,
    ContentStorage,
}

/// Structured diagnostic returned on failure (never silent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureDiagnostic {
    pub class: FailureClass,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// True when the failure is expected without network (fixture / offline mode).
    pub offline_mode: bool,
}
