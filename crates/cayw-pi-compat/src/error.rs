use thiserror::Error;

/// Errors from the optional Pi→CAYW compat host.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PiCompatError {
    #[error("pi compat host disabled")]
    Disabled,
    #[error("plugin not loaded: {0}")]
    NotLoaded(String),
    #[error("plugin already loaded: {0}")]
    AlreadyLoaded(String),
    #[error("surface incompatible: {0}")]
    Incompatible(String),
    #[error("surface unavailable: {0}")]
    Unavailable(String),
    #[error("plugin call failed: {0}")]
    Failed(String),
    #[error("plugin call cancelled with a known outcome")]
    Cancelled,
    #[error("plugin call timed out with a known outcome")]
    Timeout,
    #[error("plugin outcome unknown: {0}")]
    OutcomeUnknown(String),
}

impl PiCompatError {
    pub fn outcome_unknown(&self) -> bool {
        matches!(self, Self::OutcomeUnknown(_))
    }
}
