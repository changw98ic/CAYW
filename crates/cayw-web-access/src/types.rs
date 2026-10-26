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

/// Stable in-process reference to fetched / extracted bytes or text.
///
/// First cut keeps bodies in an in-memory store. Aligning with F01 result handles
/// / `cayw-content` is a follow-up merge concern, not required here.
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
