//! Public Runtime error contract: stable codes, typed causes and side-effect state.
//!
//! [`RuntimeError::code`] is the stable IPC/SDK discriminator. Callers must
//! branch on that code or on the typed accessors here, never on message text.
//! Unknown/reconciliation state is preserved through nested
//! [`RuntimeError::RecoveryRequired`] causes and cleanup errors; it never
//! becomes an ordinary known failure or an automatic retry.

use cayw_core::CoreError;
use cayw_ipc::RpcError;
use cayw_requests::RequestError;
use std::{fmt, io};

/// Classified storage failure.
///
/// Database contention is kept separate from disk I/O and capacity failures so
/// callers can decide behavior from typed state instead of message text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageFailure {
    /// The database or one of its tables is busy/locked. Side-effect state is separate.
    Busy,
    /// The database or an underlying file system is out of space/quota.
    Full,
    /// A system I/O, permission or open failure.
    Io,
    /// Stored data or the database image is malformed.
    Corrupt,
    /// Another SQLite failure that has no more specific category.
    Other,
}

impl StorageFailure {
    pub fn from_sqlite(error: &rusqlite::Error) -> Self {
        if let rusqlite::Error::FromSqlConversionFailure(_, _, source) = error
            && source.is::<serde_json::Error>()
        {
            return Self::Corrupt;
        }
        match error.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                Self::Busy
            }
            Some(rusqlite::ErrorCode::DiskFull) => Self::Full,
            Some(
                rusqlite::ErrorCode::SystemIoFailure
                | rusqlite::ErrorCode::ReadOnly
                | rusqlite::ErrorCode::CannotOpen
                | rusqlite::ErrorCode::FileLockingProtocolFailed
                | rusqlite::ErrorCode::NoLargeFileSupport,
            ) => Self::Io,
            Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
                Self::Corrupt
            }
            _ => Self::Other,
        }
    }

    pub fn from_io(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::StorageFull
            | io::ErrorKind::QuotaExceeded
            | io::ErrorKind::FileTooLarge => Self::Full,
            _ => Self::Io,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Busy => "storage_busy",
            Self::Full => "storage_full",
            Self::Io => "storage_io",
            Self::Corrupt => "storage_corrupt",
            Self::Other => "storage_error",
        }
    }

    fn unknown_code(self) -> &'static str {
        match self {
            Self::Busy => "storage_busy_outcome_unknown",
            Self::Full => "storage_full_outcome_unknown",
            Self::Io => "storage_io_outcome_unknown",
            Self::Corrupt => "storage_corrupt_outcome_unknown",
            Self::Other => "storage_error_outcome_unknown",
        }
    }
}

/// Typed classification for a worker that ended without returning its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkerFailure {
    /// The worker panicked. Its effects may already have happened.
    Panic,
    /// The worker task was cancelled before it could return a result.
    Cancelled,
    /// The worker ended for another reason and returned no result.
    Other,
}

/// Which worker terminal produced a recovery error. The code keeps the
/// variant identity: a lost join handle stays a lost worker even when the
/// join payload says the task panicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectWorkerFailure {
    Panicked,
    Lost,
}

/// The error and its [`std::error::Error::source`] chain are implemented by
/// hand instead of derived: `thiserror` would expose a boxed cause as the
/// concrete source type, which breaks typed downcasts of the recovery cause.
#[derive(Debug)]
pub enum RuntimeError {
    InvalidInput(String),
    Busy,
    QueueFull,
    Cancelled,
    Budget(&'static str),
    NeedsReconciliation,
    Frozen(String),
    StaleApproval,
    StaleCancellation,
    Core(CoreError),
    Journal(rusqlite::Error),
    Request(RequestError),
    Content(cayw_content::Error),
    Json(serde_json::Error),
    Agent(cayw_agent::AgentError),
    Workspace(String),
    Worker(String),
    WorkerPanicked(String),
    WorkerLost(tokio::task::JoinError),
    RecoveryRequired {
        cause: Box<RuntimeError>,
        cleanup: Option<Box<RuntimeError>>,
        outcome_unknown: bool,
    },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::Busy => write!(f, "session already has a queued or running task"),
            Self::QueueFull => write!(f, "runtime task queue is full"),
            Self::Cancelled => write!(f, "task cancelled before execution"),
            Self::Budget(budget) => write!(f, "task exceeded its {budget} budget"),
            Self::NeedsReconciliation => {
                write!(f, "session requires reconciliation before it can continue")
            }
            Self::Frozen(reason) => write!(f, "session is frozen: {reason}"),
            Self::StaleApproval => write!(f, "approval is no longer pending for an active attempt"),
            Self::StaleCancellation => {
                write!(
                    f,
                    "cancellation does not match the original task execution identity"
                )
            }
            Self::Core(error) => write!(f, "persistent session error: {error}"),
            Self::Journal(error) => write!(f, "runtime journal error: {error}"),
            Self::Request(error) => write!(f, "task request authority error: {error}"),
            Self::Content(error) => write!(f, "local content error: {error}"),
            Self::Json(error) => write!(f, "invalid serialized data: {error}"),
            Self::Agent(error) => write!(f, "agent error: {error}"),
            Self::Workspace(message) => write!(f, "workspace error: {message}"),
            Self::Worker(message) => write!(f, "runtime worker failed: {message}"),
            Self::WorkerPanicked(message) => {
                write!(
                    f,
                    "runtime worker panicked; effects may need reconciliation: {message}"
                )
            }
            Self::WorkerLost(error) => {
                write!(
                    f,
                    "runtime worker stopped unexpectedly; effects may need reconciliation: {error}"
                )
            }
            Self::RecoveryRequired { cause, cleanup, .. } => write!(
                f,
                "task requires explicit recovery: {cause}; cleanup error: {cleanup:?}"
            ),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Core(error) => Some(error),
            Self::Journal(error) => Some(error),
            Self::Request(error) => Some(error),
            Self::Content(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Agent(error) => Some(error),
            Self::WorkerLost(error) => Some(error),
            // The boxed cause is exposed as the concrete `RuntimeError`, so
            // callers can downcast the typed recovery cause.
            Self::RecoveryRequired { cause, .. } => Some(&**cause),
            _ => None,
        }
    }
}

impl From<CoreError> for RuntimeError {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}

impl From<rusqlite::Error> for RuntimeError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Journal(error)
    }
}

impl From<RequestError> for RuntimeError {
    fn from(error: RequestError) -> Self {
        Self::Request(error)
    }
}

impl From<cayw_content::Error> for RuntimeError {
    fn from(error: cayw_content::Error) -> Self {
        Self::Content(error)
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<cayw_agent::AgentError> for RuntimeError {
    fn from(error: cayw_agent::AgentError) -> Self {
        Self::Agent(error)
    }
}

impl From<tokio::task::JoinError> for RuntimeError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::WorkerLost(error)
    }
}

impl RuntimeError {
    /// Stable error code for IPC/SDK consumers. Do not infer behavior from the
    /// message text.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "invalid_params",
            Self::Busy => "busy",
            Self::QueueFull => "queue_full",
            Self::Cancelled => "cancelled",
            Self::Budget(_) => "budget_exceeded",
            Self::NeedsReconciliation => "needs_reconciliation_outcome_unknown",
            Self::Frozen(_) => "session_frozen",
            Self::StaleApproval => "stale_approval",
            Self::StaleCancellation => "stale_cancellation",
            Self::Core(error) => core_code(error),
            Self::Journal(error) => StorageFailure::from_sqlite(error).code(),
            Self::Request(error) => request_code(error),
            Self::Content(error) => content_code(error),
            Self::Workspace(_) => "workspace_error",
            // Serialized stored data is not an invalid client parameter.
            Self::Json(_) => "stored_data_corrupt",
            Self::Agent(error) => agent_code(error),
            Self::Worker(_) => "worker_failed",
            Self::WorkerPanicked(_) => "worker_panicked_outcome_unknown",
            Self::WorkerLost(_) => "worker_lost_outcome_unknown",
            Self::RecoveryRequired { .. } => match self.direct_worker_failure() {
                Some(DirectWorkerFailure::Panicked) => "worker_panicked_outcome_unknown",
                Some(DirectWorkerFailure::Lost) => "worker_lost_outcome_unknown",
                None if self.outcome_unknown() => "runtime_outcome_unknown",
                None => "recovery_required",
            },
        }
    }

    /// True when any effect may have happened without a reliable terminal.
    /// A nested cause or cleanup error keeps unknown state even when the
    /// wrapper flag was not set.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::NeedsReconciliation => true,
            Self::Core(CoreError::NeedsReconciliation) => true,
            Self::Request(
                RequestError::Clock
                | RequestError::Poisoned
                | RequestError::AuthorityStopped
                | RequestError::Database(_)
                | RequestError::Json(_)
                | RequestError::Io(_),
            ) => true,
            Self::Content(cayw_content::Error::CommitOutcomeUnknown(_)) => true,
            Self::Agent(cayw_agent::AgentError::UnresolvedToolEffects) => true,
            Self::WorkerPanicked(_) | Self::WorkerLost(_) => true,
            Self::RecoveryRequired {
                outcome_unknown,
                cause,
                cleanup,
            } => {
                *outcome_unknown
                    || cause.outcome_unknown()
                    || cleanup
                        .as_deref()
                        .is_some_and(RuntimeError::outcome_unknown)
            }
            _ => false,
        }
    }

    /// Typed storage classification, including nested causes and cleanup
    /// errors. `None` means this error is not a storage failure.
    pub fn storage_failure(&self) -> Option<StorageFailure> {
        match self {
            Self::Journal(error) => Some(StorageFailure::from_sqlite(error)),
            Self::Core(CoreError::Database(error)) => Some(StorageFailure::from_sqlite(error)),
            Self::Request(RequestError::Database(error)) => {
                Some(StorageFailure::from_sqlite(error))
            }
            Self::Request(RequestError::Io(error))
            | Self::Content(
                cayw_content::Error::Io(error) | cayw_content::Error::CommitOutcomeUnknown(error),
            ) => Some(StorageFailure::from_io(error)),
            Self::RecoveryRequired { cause, cleanup, .. } => cause
                .storage_failure()
                .or_else(|| cleanup.as_deref().and_then(RuntimeError::storage_failure)),
            _ => None,
        }
    }

    /// Typed worker classification, including nested causes and cleanup errors.
    pub fn worker_failure(&self) -> Option<WorkerFailure> {
        match self {
            Self::WorkerPanicked(_) => Some(WorkerFailure::Panic),
            Self::WorkerLost(error) if error.is_panic() => Some(WorkerFailure::Panic),
            Self::WorkerLost(error) if error.is_cancelled() => Some(WorkerFailure::Cancelled),
            Self::WorkerLost(_) => Some(WorkerFailure::Other),
            Self::RecoveryRequired { cause, cleanup, .. } => cause
                .worker_failure()
                .or_else(|| cleanup.as_deref().and_then(RuntimeError::worker_failure)),
            _ => None,
        }
    }

    /// The worker terminal that caused this recovery error. Cleanup failures
    /// are inspected separately so a cleanup panic does not hide the primary
    /// cause, and a lost join handle is not re-labelled as a caught panic.
    fn direct_worker_failure(&self) -> Option<DirectWorkerFailure> {
        match self {
            Self::WorkerPanicked(_) => Some(DirectWorkerFailure::Panicked),
            Self::WorkerLost(_) => Some(DirectWorkerFailure::Lost),
            Self::RecoveryRequired { cause, .. } => cause.direct_worker_failure(),
            _ => None,
        }
    }
}

fn core_code(error: &CoreError) -> &'static str {
    match error {
        CoreError::Database(error) => StorageFailure::from_sqlite(error).code(),
        CoreError::Json(_) | CoreError::CorruptStore(_) => "stored_data_corrupt",
        CoreError::NeedsReconciliation => "needs_reconciliation_outcome_unknown",
        _ => "session_error",
    }
}

fn content_code(error: &cayw_content::Error) -> &'static str {
    match error {
        cayw_content::Error::CommitOutcomeUnknown(_) => "content_commit_outcome_unknown",
        cayw_content::Error::Io(error) => StorageFailure::from_io(error).code(),
        cayw_content::Error::Encoding(_) | cayw_content::Error::Integrity => "stored_data_corrupt",
        _ => "content_error",
    }
}

fn agent_code(error: &cayw_agent::AgentError) -> &'static str {
    match error {
        cayw_agent::AgentError::UnresolvedToolEffects => "agent_outcome_unknown",
        _ => "agent_error",
    }
}

fn request_code(error: &RequestError) -> &'static str {
    match error {
        RequestError::InvalidInput(_) => "invalid_params",
        RequestError::Stale => "request_stale",
        RequestError::NotFound => "request_not_found",
        RequestError::Denied(_) => "request_denied",
        RequestError::Budget => "request_budget_exceeded",
        RequestError::PolicyMismatch => "request_policy_changed",
        RequestError::Clock => "request_clock_invalid_outcome_unknown",
        // Keep the existing authority-unavailable state while retaining its
        // concrete storage cause. The error alone does not prove no commit.
        RequestError::Database(error) => StorageFailure::from_sqlite(error).unknown_code(),
        RequestError::Json(_) => "stored_data_corrupt_outcome_unknown",
        RequestError::Io(error) => StorageFailure::from_io(error).unknown_code(),
        RequestError::Poisoned | RequestError::AuthorityStopped => {
            "request_authority_unavailable_outcome_unknown"
        }
    }
}

/// The IPC adapter uses the same stable classification as the SDK accessor.
/// Operation context (method and supplied IDs) is added by the RPC boundary.
impl From<RuntimeError> for RpcError {
    fn from(error: RuntimeError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite(code: rusqlite::ErrorCode) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: 0,
            },
            None,
        )
    }

    #[test]
    fn storage_failures_are_typed_and_distinguishable() {
        let busy = RuntimeError::Journal(sqlite(rusqlite::ErrorCode::DatabaseBusy));
        assert_eq!(busy.storage_failure(), Some(StorageFailure::Busy));
        assert_eq!(busy.code(), "storage_busy");
        assert!(!busy.outcome_unknown());

        let io = RuntimeError::Request(RequestError::Database(sqlite(
            rusqlite::ErrorCode::SystemIoFailure,
        )));
        assert_eq!(io.storage_failure(), Some(StorageFailure::Io));
        assert_eq!(io.code(), "storage_io_outcome_unknown");
        assert!(io.outcome_unknown());

        let full = RuntimeError::Core(CoreError::Database(sqlite(rusqlite::ErrorCode::DiskFull)));
        assert_eq!(full.storage_failure(), Some(StorageFailure::Full));
        assert_eq!(full.code(), "storage_full");

        let file_full = RuntimeError::Request(RequestError::Io(io::Error::from(
            io::ErrorKind::StorageFull,
        )));
        assert_eq!(file_full.storage_failure(), Some(StorageFailure::Full));
        assert_eq!(file_full.code(), "storage_full_outcome_unknown");
        assert!(cayw_ipc::ClientError::from(RpcError::from(file_full)).outcome_unknown);

        let request_busy = RuntimeError::Request(RequestError::Database(sqlite(
            rusqlite::ErrorCode::DatabaseBusy,
        )));
        assert_eq!(request_busy.code(), "storage_busy_outcome_unknown");
        assert_eq!(request_busy.storage_failure(), Some(StorageFailure::Busy));
        assert!(request_busy.outcome_unknown());
    }

    #[test]
    fn non_json_sql_conversion_does_not_claim_corrupt_json() {
        let error = rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(io::Error::other("non-JSON conversion fixture")),
        );
        assert_eq!(StorageFailure::from_sqlite(&error), StorageFailure::Other);
    }

    #[test]
    fn worker_and_agent_failures_have_distinct_codes() {
        assert_eq!(RuntimeError::Worker("known".into()).code(), "worker_failed");
        assert!(
            RuntimeError::Worker("known".into())
                .worker_failure()
                .is_none()
        );
        assert_eq!(
            RuntimeError::Agent(cayw_agent::AgentError::Busy).code(),
            "agent_error"
        );
        assert_eq!(
            RuntimeError::Agent(cayw_agent::AgentError::UnresolvedToolEffects).code(),
            "agent_outcome_unknown"
        );
        assert!(
            RuntimeError::Agent(cayw_agent::AgentError::UnresolvedToolEffects).outcome_unknown()
        );
        assert_eq!(
            RuntimeError::WorkerPanicked("panicked".into()).worker_failure(),
            Some(WorkerFailure::Panic)
        );
        assert!(RuntimeError::WorkerPanicked("panicked".into()).outcome_unknown());
        let recovery = RuntimeError::RecoveryRequired {
            cause: Box::new(RuntimeError::WorkerPanicked("panicked".into())),
            cleanup: None,
            outcome_unknown: true,
        };
        assert_eq!(recovery.code(), "worker_panicked_outcome_unknown");
    }

    #[tokio::test]
    async fn lost_worker_keeps_typed_unknown_state_and_source() {
        let join = tokio::spawn(async { panic!("worker fixture") })
            .await
            .expect_err("worker panicked");
        let lost = RuntimeError::RecoveryRequired {
            cause: Box::new(RuntimeError::WorkerLost(join)),
            cleanup: None,
            outcome_unknown: true,
        };
        assert_eq!(lost.code(), "worker_lost_outcome_unknown");
        assert!(lost.outcome_unknown());
        assert_eq!(lost.worker_failure(), Some(WorkerFailure::Panic));
        let cause = std::error::Error::source(&lost).expect("worker cause");
        let worker = cause
            .downcast_ref::<RuntimeError>()
            .expect("cause is the typed worker error");
        assert!(matches!(worker, RuntimeError::WorkerLost(_)));
        let join = std::error::Error::source(worker).expect("JoinError source");
        assert!(join.downcast_ref::<tokio::task::JoinError>().is_some());
    }

    #[test]
    fn nested_unknown_outranks_a_false_wrapper_flag() {
        let nested = RuntimeError::RecoveryRequired {
            cause: Box::new(RuntimeError::NeedsReconciliation),
            cleanup: None,
            outcome_unknown: false,
        };
        assert!(nested.outcome_unknown());
        assert_eq!(nested.code(), "runtime_outcome_unknown");

        let cleanup_unknown = RuntimeError::RecoveryRequired {
            cause: Box::new(RuntimeError::Worker("known".into())),
            cleanup: Some(Box::new(RuntimeError::WorkerPanicked(
                "cleanup panicked".into(),
            ))),
            outcome_unknown: false,
        };
        assert!(cleanup_unknown.outcome_unknown());
        assert_eq!(cleanup_unknown.code(), "runtime_outcome_unknown");

        let known = RuntimeError::RecoveryRequired {
            cause: Box::new(RuntimeError::Worker("known".into())),
            cleanup: None,
            outcome_unknown: false,
        };
        assert!(!known.outcome_unknown());
        assert_eq!(known.code(), "recovery_required");
    }

    #[test]
    fn source_chain_preserves_typed_cause() {
        let error = RuntimeError::Core(CoreError::Database(sqlite(
            rusqlite::ErrorCode::DatabaseBusy,
        )));
        let core = std::error::Error::source(&error).expect("core source");
        assert!(core.downcast_ref::<CoreError>().is_some());
        let sqlite = core.source().expect("sqlite source");
        assert!(sqlite.downcast_ref::<rusqlite::Error>().is_some());

        let recovery = RuntimeError::RecoveryRequired {
            cause: Box::new(RuntimeError::WorkerPanicked("panicked".into())),
            cleanup: Some(Box::new(RuntimeError::Workspace("cleanup failed".into()))),
            outcome_unknown: true,
        };
        assert!(
            std::error::Error::source(&recovery)
                .and_then(|source| source.downcast_ref::<RuntimeError>())
                .is_some()
        );
        let RuntimeError::RecoveryRequired {
            cleanup: Some(cleanup),
            ..
        } = &recovery
        else {
            panic!("cleanup error is preserved");
        };
        assert!(matches!(**cleanup, RuntimeError::Workspace(_)));
    }

    #[test]
    fn content_commit_unknown_survives_sdk_rpc_and_recovery() {
        let error = RuntimeError::Content(cayw_content::Error::CommitOutcomeUnknown(
            io::Error::from(io::ErrorKind::StorageFull),
        ));
        assert_eq!(error.code(), "content_commit_outcome_unknown");
        assert_eq!(error.storage_failure(), Some(StorageFailure::Full));
        assert!(error.outcome_unknown());
        let nested = RuntimeError::RecoveryRequired {
            cause: Box::new(RuntimeError::Worker("known".into())),
            cleanup: Some(Box::new(error)),
            outcome_unknown: false,
        };
        assert!(nested.outcome_unknown());
        let client = cayw_ipc::ClientError::from(RpcError::from(nested));
        assert_eq!(client.code, "runtime_outcome_unknown");
        assert!(client.outcome_unknown);
    }

    #[test]
    fn stored_json_corruption_is_not_invalid_client_params() {
        let error = RuntimeError::Json(
            serde_json::from_str::<serde_json::Value>("{").expect_err("invalid JSON fixture"),
        );
        assert_eq!(error.code(), "stored_data_corrupt");
        assert_ne!(error.code(), "invalid_params");
    }
}
