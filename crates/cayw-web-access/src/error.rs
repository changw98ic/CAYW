//! Error type for F10 web-access.

use thiserror::Error;

use crate::types::{FailureClass, FailureDiagnostic};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{diagnostic}")]
pub struct WebAccessError {
    pub diagnostic: FailureDiagnostic,
    pub outcome_unknown: bool,
}

impl std::fmt::Display for FailureDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.class, self.message)
    }
}

impl WebAccessError {
    pub fn new(class: FailureClass, message: impl Into<String>) -> Self {
        Self {
            diagnostic: FailureDiagnostic {
                class,
                message: message.into(),
                url: None,
                provider: None,
                http_status: None,
                offline_mode: class == FailureClass::Offline,
            },
            outcome_unknown: false,
        }
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.diagnostic.url = Some(url.into());
        self
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.diagnostic.provider = Some(provider.into());
        self
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.diagnostic.http_status = Some(status);
        self
    }

    pub fn offline(message: impl Into<String>) -> Self {
        Self::new(FailureClass::Offline, message)
    }

    pub fn diagnostic(&self) -> &FailureDiagnostic {
        &self.diagnostic
    }
}
