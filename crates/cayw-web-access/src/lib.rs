//! Web search, URL fetch, and HTML/PDF text extraction for CAYW (F10).
//!
//! Provides a unified Provider/tool surface under **`web.search` / `web.fetch` /
//! `web.extract`**. Existing host `search.query` stays the local Workspace
//! `fs.grep` bridge and is explicitly refused by this crate.
//!
//! ## Capabilities (this wave)
//! - pluggable [`SearchProvider`] + [`HttpTransport`]
//! - offline [`FixtureTransport`] / [`FixtureSearch`] with diagnosable failures
//! - live [`ReqwestTransport`] for public HTTP(S) URLs
//! - real search: [`DuckDuckGoHtmlSearch`] (no API key); optional [`BraveSearch`]
//!   via `BRAVE_SEARCH_API_KEY` with DuckDuckGo fallback ([`SearchBackend::BraveOrDuckDuckGo`])
//! - HTML body text via `html5gum`
//! - **PDF**: minimal parenthesis / literal-string scan only — **not** a full PDF
//!   engine (no font decoding, ToUnicode, xref repair, or layout)
//! - **Video**: unsupported (no frame / AV understanding)
//! - process-local [`ContentStore`] + [`Citation`] / [`ContentRef`] held for the
//!   [`WebAccess`] lifetime (host keeps `Arc<WebAccess>` so the model can reuse refs)
//! - [`definitions`] for Host/Runtime catalog registration
//!
//! Design reference (not a code transplant):
//! [nicobailon/pi-web-access](https://github.com/nicobailon/pi-web-access).
//! See `docs/sources/f10-web-access.md`.
#![forbid(unsafe_code)]

mod brave;
mod catalog;
mod content;
mod ddg;
mod error;
mod extract;
mod host;
mod provider;
mod service;
mod types;

pub use brave::{BRAVE_API_KEY_ENV, BraveOrFallback, BraveSearch};
pub use catalog::definitions;
pub use content::{ContentSink, ContentStore};
pub use ddg::{
    DuckDuckGoHtmlSearch, extract_ddg_hits, parse_duckduckgo_html, parse_duckduckgo_response,
    resolve_ddg_href,
};
pub use error::WebAccessError;
pub use extract::{extract_auto, extract_html, extract_pdf};
pub use host::{SearchBackend, WebAccessOptions, host_error_message};
pub use provider::{
    BoxFuture, FixtureSearch, FixtureTransport, HttpTransport, RawResponse, ReqwestTransport,
    SearchProvider,
};
pub use service::{Limits, WebAccess, hit};
pub use types::*;

/// Wire / document protocol for this web-access surface.
pub const WEB_ACCESS_PROTOCOL_VERSION: u32 = 1;
