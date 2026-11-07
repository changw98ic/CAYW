use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const ID_BYTES: usize = 128;
pub(crate) const TEXT_BYTES: usize = 2048;
pub(crate) const ARG_BYTES: usize = 64 * 1024;
pub(crate) const CATALOG_BYTES: usize = 512 * 1024;
pub(crate) const RECORD_BYTES: usize = ARG_BYTES + 32 * 1024;
// Covers 64 retained attempt IDs plus maximally escaped intent/end reasons.
pub(crate) const TASK_BYTES: usize = CATALOG_BYTES + 32 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error("invalid request input: {0}")]
    InvalidInput(String),
    #[error("request state changed, is terminal, or belongs to another attempt")]
    Stale,
    #[error("task or request was not found for this identity")]
    NotFound,
    #[error("request denied: {0}")]
    Denied(String),
    #[error("request storage or count budget is exhausted")]
    Budget,
    #[error("request store configuration or host policy does not match")]
    PolicyMismatch,
    #[error("request clock is outside the supported range")]
    Clock,
    #[error("request store lock is poisoned")]
    Poisoned,
    #[error(
        "request authority stopped after persistence became unavailable; host reconciliation and replacement are required"
    )]
    AuthorityStopped,
    #[error("request persistence failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("request JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("request workspace failed: {0}")]
    Io(#[from] io::Error),
}
pub type Result<T> = std::result::Result<T, RequestError>;

/// Identity of one file in the original Requests storage. Paths alone cannot
/// distinguish a replacement database or authority sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityFileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityTaskInspection {
    pub identity: TaskIdentity,
    pub state: TaskState,
}

/// Host-only observation. Inspection does not advance time, grant permission,
/// or release the authority stop. The digest binds the original stored facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityInspection {
    pub authority_id: String,
    pub inspection_id: String,
    pub tasks: Vec<AuthorityTaskInspection>,
    pub database: AuthorityFileIdentity,
    pub sidecar: AuthorityFileIdentity,
    pub marker: Vec<u8>,
    pub stopped: bool,
    pub last_now_ms: u64,
}

/// Trusted host proof owner. Implementations must retain every original Task
/// execution lock, Cells stopped scope, and confirmed Device cleanup through
/// Drop. Returning task names alone is not proof. No unit/blanket implementation
/// is provided, and this trait has no model-facing deserialization path.
pub trait AuthorityRecoveryGuard {
    fn verified_tasks(&self) -> &[TaskIdentity];
}

#[derive(Debug, Clone)]
pub struct AuthorityRepairRequest {
    pub inspection: AuthorityInspection,
    pub repair_id: String,
    pub actor: HostActor,
    pub evidence: String,
}
impl AuthorityRepairRequest {
    pub fn new(
        inspection: AuthorityInspection,
        repair_id: impl Into<String>,
        actor: HostActor,
        evidence: impl Into<String>,
    ) -> Result<Self> {
        let repair_id = repair_id.into();
        let evidence = evidence.into();
        identifier(&repair_id)?;
        text(&evidence)?;
        Ok(Self {
            inspection,
            repair_id,
            actor,
            evidence,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityRepairReceipt {
    pub repair_id: String,
    pub previous_authority_id: String,
    pub authority_id: String,
    pub inspection_id: String,
}

/// All fields come from the host's active execution lease, never tool arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIdentity {
    pub session_id: String,
    pub task_id: String,
    pub attempt_id: String,
}
impl TaskIdentity {
    pub fn new(
        session: impl Into<String>,
        task: impl Into<String>,
        attempt: impl Into<String>,
    ) -> Result<Self> {
        let identity = Self {
            session_id: session.into(),
            task_id: task.into(),
            attempt_id: attempt.into(),
        };
        identity.validate()?;
        Ok(identity)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        identifier(&self.session_id)?;
        identifier(&self.task_id)?;
        identifier(&self.attempt_id)
    }
}

/// Trusted application identity. Deliberately not deserializable from model JSON.
#[derive(Debug, Clone)]
pub struct HostActor(String);
impl HostActor {
    pub fn new(actor: impl Into<String>) -> Result<Self> {
        let actor = actor.into();
        identifier(&actor)?;
        Ok(Self(actor))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// First-round evaluation requires this host-recorded user instruction AND an
/// exact build.run/test.run profile in the existing host catalog. The canonical
/// definition lives in `cayw-queue`, which owns durable task identity.
pub use cayw_queue::TaskIntent;
pub(crate) fn validate_intent(intent: &TaskIntent) -> Result<()> {
    if let TaskIntent::ExplicitProjectTestOrScript { user_instruction } = intent {
        text(user_instruction)?;
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Completed,
    Failed,
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskState {
    Active,
    /// Admissions stop immediately; already-started calls retain their audit
    /// settlement path while Runtime cancels/drains the same Host futures.
    Closing {
        actor: String,
        reason: String,
        at_ms: u64,
    },
    Ended {
        outcome: TaskOutcome,
        actor: String,
        reason: String,
        at_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestInput {
    pub purpose: String,
    pub gap: String,
    pub operation_id: String,
    pub capability: String,
    pub arguments: Value,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidityReason {
    ExactHostOperation,
    PurposeMissing,
    ConcreteGapMissing,
    UnsupportedOperation,
    ScopeMismatch,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    Invalid,
    /// Structurally valid and below the repeat threshold. This is a hint for
    /// the host/model; a trusted host may evaluate it immediately.
    Requested,
    AwaitingEvaluation,
    Evaluating,
    Granted,
    Denied,
    EvaluatorUnavailable,
    Revoked,
    Expired,
    Superseded,
    TaskEnded,
}
impl RequestState {
    pub(crate) fn live(self) -> bool {
        matches!(
            self,
            Self::Requested | Self::AwaitingEvaluation | Self::Evaluating | Self::Granted
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Approved,
    Denied,
    EvaluatorUnavailable,
    Revoked,
    Expired,
    Superseded,
    TaskEnded,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecord {
    pub kind: DecisionKind,
    pub actor: String,
    pub reason: String,
    pub at_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRecord {
    pub id: String,
    pub identity: TaskIdentity,
    pub input: RequestInput,
    pub state: RequestState,
    pub validity_reasons: Vec<ValidityReason>,
    pub valid_request_count: u32,
    pub catalog_fingerprint: String,
    pub created_at_ms: u64,
    pub request_expires_at_ms: u64,
    pub evaluator: Option<String>,
    pub evaluation_deadline_ms: Option<u64>,
    pub grant_expires_at_ms: Option<u64>,
    pub decision: Option<DecisionRecord>,
}

/// Exact host-issued admission evidence. Only RequestStore::admit can construct
/// it: no Deserialize, public fields, or model-facing issuance path exists.
/// Recheck this SAME receipt after permission/audit waits; it cannot follow a
/// newer grant. It is not a substitute for upstream Host/Device permissions.
///
/// ```compile_fail
/// let _: cayw_requests::AuthorizationReceipt = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// let _ = cayw_requests::AuthorizationReceipt {};
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct AuthorizationReceipt {
    pub(crate) identity: TaskIdentity,
    pub(crate) request_id: String,
    pub(crate) catalog_fingerprint: String,
    pub(crate) scope: crate::OperationScope,
    pub(crate) grant_expires_at_ms: u64,
    pub(crate) decision: DecisionRecord,
}
impl AuthorizationReceipt {
    pub fn identity(&self) -> &TaskIdentity {
        &self.identity
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn catalog_fingerprint(&self) -> &str {
        &self.catalog_fingerprint
    }
    pub fn scope(&self) -> &crate::OperationScope {
        &self.scope
    }
    pub fn grant_expires_at_ms(&self) -> u64 {
        self.grant_expires_at_ms
    }
    pub fn decision(&self) -> &DecisionRecord {
        &self.decision
    }
}

/// Only evaluate() can issue this ticket. No Deserialize, public fields, model
/// callback, or boolean approval path exists. Tickets are single-use and leased.
#[derive(Debug, Clone)]
pub struct EvaluationTicket {
    pub(crate) identity: TaskIdentity,
    pub(crate) request_id: String,
    pub(crate) nonce: String,
    pub(crate) actor: String,
}
impl EvaluationTicket {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn evaluator(&self) -> &str {
        &self.actor
    }
}

/// The repeat threshold only selects the informational state before evaluation
/// (`requested` versus `awaiting_evaluation`). It never withholds evaluation
/// from a structurally valid request and never grants by itself.
/// [`crate::RequestStore::adjust_config`] durably replaces operational limits
/// for a live store; a trusted host may also open with
/// [`crate::RequestStore::open_with_trusted_adjustment`] to adopt and persist
/// limits changed since the previous process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestConfig {
    pub repeated_valid_threshold: u32,
    pub request_lifetime_ms: u64,
    pub evaluation_timeout_ms: u64,
    pub max_grant_lifetime_ms: u64,
    /// Maximum number of unfinished (Active/Closing) tasks. Finished tasks keep
    /// their tombstones but stop counting against this operational limit.
    pub max_tasks: u32,
    /// Across all attempts; old rows and terminal task tombstones are retained.
    pub max_requests_per_task: u32,
    pub max_attempts_per_task: u32,
    /// Lifetime event-row budget. Event rows are immutable audit facts and are
    /// never released.
    pub max_events: u64,
    /// Operational state reservations of unfinished tasks and a repair awaiting
    /// activation. Task reservations return at `end_task`; a repair reservation
    /// returns in its activation commit. Retained audit records are not charged
    /// again.
    pub max_state_bytes: u64,
    pub max_result_bytes: usize,
    pub sqlite_busy_timeout_ms: u64,
}
impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            repeated_valid_threshold: 2,
            request_lifetime_ms: 15 * 60 * 1000,
            evaluation_timeout_ms: 60 * 1000,
            max_grant_lifetime_ms: 15 * 60 * 1000,
            max_tasks: 128,
            max_requests_per_task: 64,
            max_attempts_per_task: 16,
            max_events: 100_000,
            max_state_bytes: 128 * 1024 * 1024,
            max_result_bytes: 1024 * 1024,
            sqlite_busy_timeout_ms: 50,
        }
    }
}
impl RequestConfig {
    /// Check persisted authority limits without opening storage or issuing a grant.
    pub fn validate(&self) -> Result<()> {
        if self.repeated_valid_threshold == 0
            || self.repeated_valid_threshold > 1000
            || self.max_tasks == 0
            || self.max_tasks > 10_000
            || self.max_requests_per_task == 0
            || self.max_requests_per_task > 1000
            || self.max_attempts_per_task == 0
            || self.max_attempts_per_task > 64
            || self.max_events == 0
            || self.max_events > 10_000_000
            || self.max_state_bytes < 2 * TASK_BYTES as u64
            || self.max_state_bytes > 4 * 1024 * 1024 * 1024
            || self.max_result_bytes < TASK_BYTES + RECORD_BYTES
            || self.max_result_bytes > 4 * 1024 * 1024
            || self.sqlite_busy_timeout_ms > 1000
            || [
                self.request_lifetime_ms,
                self.evaluation_timeout_ms,
                self.max_grant_lifetime_ms,
            ]
            .iter()
            .any(|ms| *ms == 0 || *ms > 7 * 24 * 60 * 60 * 1000)
        {
            return Err(RequestError::InvalidInput(
                "configuration limits must be positive and bounded".into(),
            ));
        }
        Ok(())
    }
}

/// Immutable audit fact for one trusted operational-limit adjustment. History
/// rows are never updated or deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigAdjustment {
    pub at_ms: u64,
    pub actor: String,
    pub reason: String,
    pub previous: RequestConfig,
    pub next: RequestConfig,
}

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> Result<u64>;
}
#[derive(Debug, Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> Result<u64> {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RequestError::Clock)?
            .as_millis();
        let ms = u64::try_from(ms).map_err(|_| RequestError::Clock)?;
        if ms > i64::MAX as u64 {
            return Err(RequestError::Clock);
        }
        Ok(ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEvent {
    pub sequence: u64,
    pub at_ms: u64,
    pub task_id: String,
    pub request_id: Option<String>,
    pub kind: String,
    pub actor: String,
    pub payload: Value,
}

pub(crate) fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > ID_BYTES
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        return Err(RequestError::InvalidInput(
            "IDs must contain 1..128 ASCII letters, digits, '.', ':', '_' or '-'".into(),
        ));
    }
    Ok(())
}
pub(crate) fn text(value: &str) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > TEXT_BYTES
        || value
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(RequestError::InvalidInput(
            "reason or instruction must contain 1..2048 bytes without control characters".into(),
        ));
    }
    Ok(())
}
pub(crate) fn checked_deadline(now: u64, lifetime: u64) -> Result<u64> {
    now.checked_add(lifetime)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(RequestError::Clock)
}
pub(crate) fn bounded_json<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(io::Error::other("JSON byte limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| RequestError::InvalidInput("JSON byte limit exceeded".into()))?;
    Ok(writer.bytes)
}
pub(crate) fn argument_bounds(value: &Value) -> Result<()> {
    fn walk(value: &Value, depth: usize, left: &mut usize) -> Result<()> {
        if depth > 32 || *left == 0 {
            return Err(RequestError::InvalidInput(
                "JSON depth or node limit exceeded".into(),
            ));
        }
        *left -= 1;
        match value {
            Value::Array(values) => {
                for child in values {
                    walk(child, depth + 1, left)?;
                }
            }
            Value::Object(values) => {
                for child in values.values() {
                    walk(child, depth + 1, left)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(value, 0, &mut 4096)?;
    bounded_json(value, ARG_BYTES)?;
    if !value.is_object() {
        return Err(RequestError::InvalidInput(
            "arguments must be an object".into(),
        ));
    }
    Ok(())
}
pub(crate) fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted: std::collections::BTreeMap<_, _> = object
                .iter()
                .map(|(k, v)| (k.clone(), canonical(v)))
                .collect();
            serde_json::to_value(sorted).expect("JSON values serialize")
        }
        Value::Array(array) => Value::Array(array.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}
