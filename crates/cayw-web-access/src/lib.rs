//! Web search, URL fetch, and HTML/PDF text extraction for CAYW (F10 first cut).
//!
//! Provides a unified Provider/tool surface under **`web.search` / `web.fetch` /
//! `web.extract`**. Existing host `search.query` stays the local Workspace
//! `fs.grep` bridge and is explicitly refused by this crate.
//!
//! First cut (F10 / SLICE M):
//! - pluggable [`SearchProvider`] + [`HttpTransport`]
//! - offline [`FixtureTransport`] / [`FixtureSearch`] with diagnosable failures
//! - live [`ReqwestTransport`] for public HTTP(S) URLs
//! - HTML body text via `html5gum`; minimal PDF literal-string extraction
//! - in-process [`ContentStore`] + [`Citation`] / [`ContentRef`] for later tools
//!
//! Video understanding, multi-provider search fallback chains, Host/Bridge catalog
//! registration, and F01 result-handle alignment are out of scope for this slice.
//!
//! Design reference (not a code transplant):
//! [nicobailon/pi-web-access](https://github.com/nicobailon/pi-web-access).
//! See `docs/sources/f10-web-access.md`.
#![forbid(unsafe_code)]

mod content;
mod error;
mod extract;
mod provider;
mod service;
mod types;

pub use content::ContentStore;
pub use error::WebAccessError;
pub use extract::{extract_auto, extract_html, extract_pdf};
pub use provider::{
    BoxFuture, FixtureSearch, FixtureTransport, HttpTransport, RawResponse, ReqwestTransport,
    SearchProvider,
};
pub use service::{Limits, WebAccess, hit};
pub use types::*;

/// Wire / document protocol for this first-cut web-access surface.
pub const WEB_ACCESS_PROTOCOL_VERSION: u32 = 1;
