use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("SQLite operation failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("JSON data is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("The session store mutex is poisoned")]
    StorePoisoned,
    #[error("Unsupported session schema {found}; this build requires {expected}")]
    SchemaVersion { found: u32, expected: u32 },
    #[error("The file belongs to another application or has an unrecognized schema")]
    ForeignDatabase,
    #[error("Unsupported protocol version {found}; this build requires {expected}")]
    ProtocolVersion { found: u32, expected: u32 },
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error("This session already has an active attempt")]
    Busy,
    #[error("The attempt lease is no longer current")]
    StaleLease,
    #[error("Tool effects require host reconciliation")]
    NeedsReconciliation,
    #[error("An approval is pending; it does not authorize execution")]
    PendingApproval,
    #[error("The host denied this operation")]
    ApprovalDenied,
    #[error("Unknown tool call: {0}")]
    CallNotFound(String),
    #[error("Unknown approval: {0}")]
    ApprovalNotFound(String),
    #[error("Unknown execution: {0}")]
    ExecutionNotFound(String),
    #[error("Unknown capability call: {0:?}")]
    CapabilityNotFound(crate::CapabilityCallKey),
    #[error("Version 1 storage requires explicit host migration: {0}")]
    MigrationRequired(String),
    #[error("Invalid event: {0}")]
    InvalidEvent(String),
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    #[error("Event page size {limit} must be between 1 and {max}")]
    InvalidLimit { limit: usize, max: usize },
    #[error("Event cursor {after} is beyond the current cursor {current}")]
    FutureCursor { after: u64, current: u64 },
    #[error("The persistent cursor or attempt fence has exhausted its integer range")]
    CounterExhausted,
    #[error("Persistent session data is inconsistent: {0}")]
    CorruptStore(String),
}
