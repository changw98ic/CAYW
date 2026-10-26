//! Host/Runtime wiring: options, builder, and shared [`WebAccess`] construction.

use std::sync::Arc;

use crate::brave::{BraveOrFallback, BraveSearch};
use crate::content::ContentStore;
use crate::ddg::DuckDuckGoHtmlSearch;
use crate::error::WebAccessError;
use crate::provider::{
    FixtureSearch, FixtureTransport, HttpTransport, ReqwestTransport, SearchProvider,
};
use crate::service::{Limits, WebAccess};
use crate::types::FailureClass;

/// Which search backend the host should construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchBackend {
    /// DuckDuckGo HTML results page — real search, no API key.
    #[default]
    DuckDuckGoHtml,
    /// In-memory fixture table only (deterministic / offline CI).
    Fixture,
    /// Brave Search when `BRAVE_SEARCH_API_KEY` is set; otherwise DuckDuckGo HTML.
    /// Empty Brave results or hard failures soft-fall back to DuckDuckGo HTML.
    BraveOrDuckDuckGo,
}

/// Cloneable host configuration for enabling web-access tools in the catalog.
#[derive(Debug, Clone)]
pub struct WebAccessOptions {
    pub limits: Limits,
    pub search: SearchBackend,
}

impl Default for WebAccessOptions {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            search: SearchBackend::DuckDuckGoHtml,
        }
    }
}

impl WebAccessOptions {
    pub fn fixture() -> Self {
        Self {
            limits: Limits::default(),
            search: SearchBackend::Fixture,
        }
    }

    /// Build a process-local [`WebAccess`] with in-memory [`ContentStore`].
    pub fn build(&self) -> Result<WebAccess, WebAccessError> {
        let transport: Arc<dyn HttpTransport> = match self.search {
            SearchBackend::Fixture => Arc::new(FixtureTransport::new()),
            _ => Arc::new(ReqwestTransport::new()?),
        };
        let search: Arc<dyn SearchProvider> = match self.search {
            SearchBackend::Fixture => Arc::new(FixtureSearch::new()),
            SearchBackend::DuckDuckGoHtml => {
                Arc::new(DuckDuckGoHtmlSearch::new(Arc::clone(&transport)))
            }
            SearchBackend::BraveOrDuckDuckGo => {
                let ddg: Arc<dyn SearchProvider> =
                    Arc::new(DuckDuckGoHtmlSearch::new(Arc::clone(&transport)));
                let brave = BraveSearch::from_env()?;
                Arc::new(BraveOrFallback::new(brave, ddg))
            }
        };
        Ok(WebAccess::new(
            transport,
            search,
            ContentStore::new(),
            self.limits.clone(),
        ))
    }
}

/// Map a web-access failure into a short host-facing message.
pub fn host_error_message(err: &WebAccessError) -> String {
    let d = err.diagnostic();
    match d.class {
        FailureClass::Offline => format!("web-access offline: {}", d.message),
        FailureClass::ProviderUnavailable => {
            format!("web-access provider unavailable: {}", d.message)
        }
        other => format!("web-access {other:?}: {}", d.message),
    }
}
