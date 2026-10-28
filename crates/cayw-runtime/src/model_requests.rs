//! Durable model accounting. No tokenizer or price is assumed. A request holds
//! the budget lineage's entire remainder until a complete reported terminal
//! commits, including a known model failure, or a cancellation is durably
//! recorded before dispatch. A failed task can have known token consumption;
//! cancellation, missing or invalid usage, and crash uncertainty retain the hold.
//! Only one unsettled request can run in a session tree. A new task, child, fork
//! or reopen never resets consumption or unknown holds. The effective cap is
//! the trusted host's current configuration; records keep their original limits.
use crate::{Inner, Runtime, RuntimeError, execution::RunContext};
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, ModelRequest, Provider,
    StopReason, Usage, UsageStatus,
};
use cayw_core::{AttemptLease, CoreEvent, EventRecord, SessionStore};
use futures::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const KIND: &str = "model_request";
const RECONCILIATION_KIND: &str = "model_request_reconciliation";
const MAX_REQUESTS: usize = 4096;
const MAX_OBSERVATIONS: usize = 65_536;
const MAX_LEDGER_EVENTS: usize = 262_144;
const MAX_LABEL_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReservationPolicy {
    /// No trusted bound for input tokens exists. Output max_tokens is not one.
    EntireSessionRemainder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequestTerminal {
    Completed,
    Failed,
    Cancelled,
    /// Cancellation was durably observed before the dispatch marker was
    /// committed. No Provider call can have happened, so this request's own
    /// token hold is released with a known zero consumption. It is never valid
    /// for a request that recorded `Started`.
    CancelledBeforeDispatch,
    Dropped,
    MissingTerminal,
    RecordingFailed,
    InvalidUsage,
    BudgetExceeded,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCostStatus {
    /// No trusted price or monetary settlement is available. None is not zero.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestIdentity {
    pub request_id: String,
    pub lease: AttemptLease,
    pub task_id: String,
    pub budget_session_id: String,
    /// One-based model request within this task attempt, not a user-turn count.
    pub turn: u64,
    pub provider: String,
    pub model: String,
}

/// Exact original accounting facts. Reconciliation never rewrites these cursors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestOrigin {
    pub identity: ModelRequestIdentity,
    pub reservation_tokens: u64,
    pub reservation_cursor: Option<u64>,
    pub started_cursor: Option<u64>,
    pub terminal: Option<ModelRequestTerminal>,
    pub terminal_cursor: Option<u64>,
    pub last_cursor: u64,
}

impl From<&ModelRequestRecord> for ModelRequestOrigin {
    fn from(record: &ModelRequestRecord) -> Self {
        Self {
            identity: record.identity.clone(),
            reservation_tokens: record.reservation_tokens,
            reservation_cursor: record.reservation_cursor,
            started_cursor: record.started_cursor,
            terminal: record.terminal,
            terminal_cursor: record.terminal_cursor,
            last_cursor: record.last_cursor,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelRequestReconciledOutcome {
    /// The trusted host proved this exact request never reached the service.
    /// This does not assert a monetary price or authorize another model call.
    NotDispatched { reason: String },
    /// A trusted complete response capture proves this request's final usage.
    /// The report is separate evidence, not a replacement for the original
    /// failed, cancelled or interrupted execution or permission to replay it.
    ReportedUsage { report: ModelUsageObservation },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestReconciliation {
    pub expected: ModelRequestOrigin,
    pub actor: String,
    pub evidence: String,
    pub outcome: ModelRequestReconciledOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestReconciliationRecord {
    pub cursor: u64,
    pub reconciliation: ModelRequestReconciliation,
}

/// Trusted host code must authorize the actor and verify evidence for the exact
/// original request. For ReportedUsage it must bind the complete response capture,
/// request/response identities, original Agent event prefix and protocol counters
/// to the supplied final report, including for cancellation or Drop. Local zero
/// defaults, an incomplete capture or actor/evidence text alone prove nothing.
/// No default verifier is installed. Implementations run synchronously under the
/// lifecycle fence and must not call Runtime lifecycle methods or wait for an
/// active execution.
pub trait ModelRequestReconciliationVerifier: Send + Sync {
    fn verify(&self, request: &ModelRequestReconciliation) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUsageObservation {
    pub event: String,
    pub usage_status: UsageStatus,
    pub usage: Usage,
    /// Protocol counter fields only. Malformed strings/containers are redacted;
    /// numeric values, including rejected negative/fractional values, survive.
    pub usage_fields: BTreeMap<String, Value>,
    pub response_id: Option<String>,
    pub response_model: Option<String>,
    pub stop_reason: StopReason,
    pub metadata_redacted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestObservation {
    pub cursor: u64,
    pub sequence: u64,
    pub observation: ModelUsageObservation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestRecord {
    pub identity: ModelRequestIdentity,
    pub policy: ModelReservationPolicy,
    pub token_limit: u64,
    pub reservation_tokens: u64,
    /// Only caps output. No trusted input-token bound is available.
    pub output_max_tokens: Option<u64>,
    pub input_token_bound: Option<u64>,
    pub reservation_cursor: Option<u64>,
    pub started_cursor: Option<u64>,
    pub last_cursor: u64,
    pub terminal: Option<ModelRequestTerminal>,
    pub terminal_cursor: Option<u64>,
    pub settled_tokens: Option<u64>,
    pub outstanding_reservation_tokens: u64,
    pub observations: Vec<ModelRequestObservation>,
    /// A separate host decision; original terminal and observation cursors stay intact.
    pub reconciliation: Option<ModelRequestReconciliationRecord>,
    pub rejection: Option<String>,
    pub cost_status: ModelCostStatus,
    pub cost_amount: Option<String>,
    pub cost_currency: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestLedger {
    /// Includes every related root/child/fork record, across tasks and attempts.
    pub budget_session_id: String,
    pub policy: ModelReservationPolicy,
    pub token_limit: u64,
    pub settled_tokens: u64,
    pub reserved_tokens: u64,
    pub remaining_tokens: u64,
    pub budget_unknown: bool,
    pub overflowed: bool,
    pub requests: Vec<ModelRequestRecord>,
    pub cost_status: ModelCostStatus,
    pub cost_amount: Option<String>,
    pub cost_currency: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case", deny_unknown_fields)]
enum Entry {
    Reserved {
        identity: ModelRequestIdentity,
        policy: ModelReservationPolicy,
        tokens: u64,
        token_limit: u64,
        output_max_tokens: u64,
    },
    Started {
        request_id: String,
        reservation_cursor: u64,
        previous_cursor: u64,
    },
    Observed {
        request_id: String,
        reservation_cursor: u64,
        previous_cursor: u64,
        observation: ModelUsageObservation,
    },
    Finished {
        request_id: String,
        reservation_cursor: u64,
        previous_cursor: u64,
        terminal: ModelRequestTerminal,
        settled_tokens: Option<u64>,
        last_received: Option<ModelUsageObservation>,
    },
    Rejected {
        identity: ModelRequestIdentity,
        token_limit: u64,
        reason: String,
    },
}

fn corrupt() -> RuntimeError {
    RuntimeError::Worker("model request ledger is inconsistent; keep its reservations and inspect the original Core events".into())
}

fn lineage(id: &str, links: &BTreeMap<String, String>) -> Result<String, RuntimeError> {
    let mut seen = BTreeSet::new();
    let mut current = id;
    while let Some(parent) = links.get(current) {
        if !seen.insert(current) || seen.len() > MAX_REQUESTS {
            return Err(corrupt());
        }
        current = parent;
    }
    Ok(current.into())
}

fn initial(
    identity: ModelRequestIdentity,
    token_limit: u64,
    tokens: u64,
    cursor: u64,
    rejected: bool,
) -> ModelRequestRecord {
    ModelRequestRecord {
        identity,
        policy: ModelReservationPolicy::EntireSessionRemainder,
        token_limit,
        reservation_tokens: tokens,
        output_max_tokens: None,
        input_token_bound: None,
        reservation_cursor: (!rejected).then_some(cursor),
        started_cursor: None,
        last_cursor: cursor,
        terminal: rejected.then_some(ModelRequestTerminal::Rejected),
        terminal_cursor: rejected.then_some(cursor),
        settled_tokens: None,
        outstanding_reservation_tokens: tokens,
        observations: vec![],
        reconciliation: None,
        rejection: None,
        cost_status: ModelCostStatus::Unknown,
        cost_amount: None,
        cost_currency: None,
    }
}

fn apply(
    records: &mut BTreeMap<String, ModelRequestRecord>,
    event: &EventRecord,
    entry: Entry,
) -> Result<(), RuntimeError> {
    match entry {
        Entry::Reserved {
            identity,
            policy: ModelReservationPolicy::EntireSessionRemainder,
            tokens,
            token_limit,
            output_max_tokens,
        } => {
            if tokens == 0
                || tokens > token_limit
                || output_max_tokens == 0
                || output_max_tokens > tokens
            {
                return Err(corrupt());
            }
            validate_identity(&identity, event)?;
            let id = identity.request_id.clone();
            let mut record = initial(identity, token_limit, tokens, event.cursor, false);
            record.output_max_tokens = Some(output_max_tokens);
            if records.insert(id, record).is_some() {
                return Err(corrupt());
            }
        }
        Entry::Rejected {
            identity,
            token_limit,
            reason,
        } => {
            validate_identity(&identity, event)?;
            let id = identity.request_id.clone();
            let mut record = initial(identity, token_limit, 0, event.cursor, true);
            record.rejection = Some(reason);
            if records.insert(id, record).is_some() {
                return Err(corrupt());
            }
        }
        other => {
            let (id, reservation_cursor, previous_cursor) = match &other {
                Entry::Started {
                    request_id,
                    reservation_cursor,
                    previous_cursor,
                }
                | Entry::Observed {
                    request_id,
                    reservation_cursor,
                    previous_cursor,
                    ..
                }
                | Entry::Finished {
                    request_id,
                    reservation_cursor,
                    previous_cursor,
                    ..
                } => (request_id, *reservation_cursor, *previous_cursor),
                _ => unreachable!(),
            };
            let record = records.get_mut(id).ok_or_else(corrupt)?;
            validate_identity(&record.identity, event)?;
            if record.terminal.is_some()
                || record.reconciliation.is_some()
                || record.reservation_cursor != Some(reservation_cursor)
                || record.last_cursor != previous_cursor
            {
                return Err(corrupt());
            }
            match other {
                Entry::Started { .. } => {
                    if record.started_cursor.replace(event.cursor).is_some() {
                        return Err(corrupt());
                    }
                }
                Entry::Observed { observation, .. } => {
                    if record.started_cursor.is_none()
                        || record.observations.len() >= MAX_OBSERVATIONS
                    {
                        return Err(corrupt());
                    }
                    record.observations.push(ModelRequestObservation {
                        cursor: event.cursor,
                        sequence: record.observations.len() as u64 + 1,
                        observation,
                    });
                }
                Entry::Finished {
                    terminal,
                    settled_tokens,
                    last_received,
                    ..
                } => {
                    if terminal == ModelRequestTerminal::CancelledBeforeDispatch {
                        // The reserve committed but dispatch admission never did,
                        // so no Provider call can have happened. Release exactly
                        // this hold with known zero consumption. A Started
                        // marker, usage evidence or any other amount is
                        // inconsistent and must not be interpreted as a release.
                        if record.started_cursor.is_some()
                            || settled_tokens != Some(0)
                            || last_received.is_some()
                        {
                            return Err(corrupt());
                        }
                        record.outstanding_reservation_tokens = 0;
                        record.settled_tokens = Some(0);
                    } else {
                        if let Some(observation) = last_received
                            && record
                                .observations
                                .last()
                                .is_none_or(|last| last.observation != observation)
                        {
                            if record.observations.len() >= MAX_OBSERVATIONS {
                                return Err(corrupt());
                            }
                            record.observations.push(ModelRequestObservation {
                                cursor: event.cursor,
                                sequence: record.observations.len() as u64 + 1,
                                observation,
                            });
                        }
                        if terminal == ModelRequestTerminal::Rejected {
                            return Err(corrupt());
                        }
                        if let Some(tokens) = settled_tokens {
                            if record.started_cursor.is_none() {
                                return Err(corrupt());
                            }
                            let last = record.observations.last().ok_or_else(corrupt)?;
                            if terminal_tokens(terminal, &last.observation) != Some(tokens)
                                || tokens > record.reservation_tokens
                            {
                                return Err(corrupt());
                            }
                            record.outstanding_reservation_tokens = 0;
                            record.settled_tokens = Some(tokens);
                        }
                    }
                    record.terminal = Some(terminal);
                    record.terminal_cursor = Some(event.cursor);
                }
                _ => unreachable!(),
            }
            record.last_cursor = event.cursor;
        }
    }
    if records.len() > MAX_REQUESTS {
        return Err(corrupt());
    }
    Ok(())
}

fn validate_identity(
    identity: &ModelRequestIdentity,
    event: &EventRecord,
) -> Result<(), RuntimeError> {
    if event.session_id != identity.lease.session_id
        || event.attempt.as_ref() != Some(&identity.lease)
        || identity.turn == 0
        || identity.request_id.is_empty()
        || identity.task_id.is_empty()
        || identity.budget_session_id.is_empty()
    {
        return Err(corrupt());
    }
    Ok(())
}

struct Projection {
    ledger: ModelRequestLedger,
    events: usize,
    /// Unfinished reservations own a terminal slot even while another branch is
    /// refused. This slot cannot be consumed by observations or Rejected rows.
    pending_terminals: usize,
}
impl Projection {
    fn room_with_limit(&self, additional: usize, event_limit: usize) -> bool {
        self.events
            .checked_add(self.pending_terminals)
            .and_then(|used| used.checked_add(additional))
            .is_some_and(|used| used <= event_limit)
    }
}

fn lineage_links(store: &SessionStore) -> Result<BTreeMap<String, String>, RuntimeError> {
    let sessions = store.list_sessions()?;
    let mut links = BTreeMap::new();
    for snapshot in &sessions {
        for (event_type, host_kind) in [
            ("session_forked", None),
            ("host", Some("agent_child_linked")),
        ] {
            visit_selected(store, snapshot, event_type, host_kind, |event| {
                let parent = match &event.event {
                    CoreEvent::SessionForked {
                        source_session_id, ..
                    } => Some(source_session_id.as_str()),
                    CoreEvent::Host { kind, data } if kind == "agent_child_linked" => Some(
                        data.get("decision").unwrap_or(data)["parent_session"]
                            .as_str()
                            .ok_or_else(corrupt)?,
                    ),
                    _ => None,
                };
                if let Some(parent) = parent
                    && links.insert(snapshot.id.clone(), parent.into()).is_some()
                {
                    return Err(corrupt());
                }
                Ok(())
            })?;
        }
    }
    Ok(links)
}

pub(crate) fn lineage_root(store: &SessionStore, session: &str) -> Result<String, RuntimeError> {
    store.snapshot(session)?;
    let links = lineage_links(store)?;
    lineage(session, &links)
}

/// Discover child/fork links before selecting records. Unrelated roots do not
/// share request/event capacity, and their request payloads are not projected.
fn project(
    store: &SessionStore,
    session: &str,
    configured_limit: u64,
) -> Result<Projection, RuntimeError> {
    store.snapshot(session)?;
    let sessions = store.list_sessions()?;
    let links = lineage_links(store)?;
    let root = lineage(session, &links)?;
    let mut records = BTreeMap::new();
    let mut ledger_events = 0;
    for snapshot in &sessions {
        if lineage(&snapshot.id, &links)? != root {
            continue;
        }
        visit_selected(store, snapshot, "host", Some(KIND), |event| {
            if let CoreEvent::Host { kind, data } = &event.event
                && kind == KIND
            {
                ledger_events += 1;
                if ledger_events > MAX_LEDGER_EVENTS {
                    return Err(corrupt());
                }
                apply(
                    &mut records,
                    event,
                    serde_json::from_value(data.clone()).map_err(|_| corrupt())?,
                )?;
            }
            Ok(())
        })?;
    }
    // Host decisions use an idle control event, not the original attempt lease.
    // Project all original facts first so decisions can only bind their final,
    // unchanged request cursor. Decisions never become ordinary Finished rows.
    for snapshot in &sessions {
        if lineage(&snapshot.id, &links)? != root {
            continue;
        }
        visit_selected(
            store,
            snapshot,
            "host",
            Some(RECONCILIATION_KIND),
            |event| {
                ledger_events += 1;
                if ledger_events > MAX_LEDGER_EVENTS {
                    return Err(corrupt());
                }
                apply_reconciliation(store, &mut records, event)
            },
        )?;
    }
    let pending_terminals = records
        .values()
        .filter(|record| {
            record.reservation_cursor.is_some()
                && record.terminal.is_none()
                && record.reconciliation.is_none()
        })
        .count();
    let mut requests = Vec::new();
    // The effective cap is the current trusted host configuration for this
    // session tree. Historical records keep their own `token_limit` as audit
    // facts, but they cannot permanently pin an earlier, smaller allocation:
    // consumption and unknown holds are carried forward separately.
    let token_limit = configured_limit;
    let mut settled_tokens = 0_u64;
    let mut reserved_tokens = 0_u64;
    let mut overflowed = false;
    for mut record in records.into_values() {
        let canonical = lineage(&record.identity.lease.session_id, &links)?;
        if canonical != record.identity.budget_session_id {
            return Err(corrupt());
        }
        if canonical != root {
            continue;
        }
        // Journals written before the explicit release terminal recorded a
        // provably pre-dispatch cancellation as `Cancelled` without a `Started`
        // event or any observation. Interpret those with the canonical release
        // so an old session tree is not permanently stuck. The original Core
        // events are not modified.
        if record.terminal == Some(ModelRequestTerminal::Cancelled)
            && record.started_cursor.is_none()
            && record.settled_tokens.is_none()
            && record.observations.is_empty()
        {
            record.outstanding_reservation_tokens = 0;
            record.settled_tokens = Some(0);
        }
        let settled = record.settled_tokens.unwrap_or(0);
        overflowed |= settled_tokens.checked_add(settled).is_none()
            || reserved_tokens
                .checked_add(record.outstanding_reservation_tokens)
                .is_none();
        settled_tokens = settled_tokens.saturating_add(settled);
        reserved_tokens = reserved_tokens.saturating_add(record.outstanding_reservation_tokens);
        requests.push(record);
    }
    requests.sort_by(|a, b| {
        (&a.identity.lease.session_id, a.last_cursor)
            .cmp(&(&b.identity.lease.session_id, b.last_cursor))
    });
    overflowed |= settled_tokens.checked_add(reserved_tokens).is_none();
    let occupied = settled_tokens.saturating_add(reserved_tokens);
    Ok(Projection {
        ledger: ModelRequestLedger {
            budget_session_id: root,
            policy: ModelReservationPolicy::EntireSessionRemainder,
            token_limit,
            settled_tokens,
            reserved_tokens,
            remaining_tokens: token_limit.saturating_sub(occupied),
            budget_unknown: reserved_tokens != 0,
            overflowed,
            requests,
            cost_status: ModelCostStatus::Unknown,
            cost_amount: None,
            cost_currency: None,
        },
        events: ledger_events,
        pending_terminals,
    })
}

fn visit_selected(
    store: &SessionStore,
    snapshot: &cayw_core::SessionSnapshot,
    event_type: &str,
    host_kind: Option<&str>,
    mut accept: impl FnMut(&EventRecord) -> Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    let mut cursor = 0;
    while cursor < snapshot.cursor {
        let page = store.events_by_kind(&snapshot.id, cursor, 1000, event_type, host_kind)?;
        if page.is_empty() {
            break;
        }
        for event in page {
            if event.cursor > snapshot.cursor {
                return Ok(());
            }
            cursor = event.cursor;
            accept(&event)?;
        }
    }
    Ok(())
}

pub(crate) fn visit(
    store: &SessionStore,
    snapshot: &cayw_core::SessionSnapshot,
    mut accept: impl FnMut(&EventRecord) -> Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    let mut cursor = 0;
    while cursor < snapshot.cursor {
        let page = store.events(&snapshot.id, cursor, 1000)?;
        if page.is_empty() {
            return Err(corrupt());
        }
        for event in page
            .into_iter()
            .take_while(|event| event.cursor <= snapshot.cursor)
        {
            cursor = event.cursor;
            accept(&event)?;
        }
    }
    Ok(())
}

fn read(
    store: &SessionStore,
    session: &str,
    configured_limit: u64,
) -> Result<ModelRequestLedger, RuntimeError> {
    Ok(project(store, session, configured_limit)?.ledger)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconciliationDecision {
    actor: String,
    decision: ModelRequestReconciliation,
}

fn validate_reconciliation(input: &ModelRequestReconciliation) -> Result<(), RuntimeError> {
    if [(&input.actor, 256), (&input.evidence, 64 * 1024)]
        .into_iter()
        .any(|(text, limit)| text.trim().is_empty() || text.len() > limit)
    {
        return Err(RuntimeError::InvalidInput(
            "model request reconciliation requires bounded actor and evidence".into(),
        ));
    }
    match &input.outcome {
        ModelRequestReconciledOutcome::NotDispatched { reason }
            if reason.trim().is_empty() || reason.len() > 4096 =>
        {
            Err(RuntimeError::InvalidInput(
                "model request non-dispatch requires a bounded reason".into(),
            ))
        }
        ModelRequestReconciledOutcome::ReportedUsage { report } => validate_report(report),
        _ => Ok(()),
    }
}

fn invalid_report() -> RuntimeError {
    RuntimeError::InvalidInput(
        "model request reported usage must be complete, consistent and match its original response and limits".into(),
    )
}

fn valid_response_label(value: &Option<String>) -> bool {
    value.as_ref().is_some_and(|text| {
        !text.trim().is_empty()
            && text.len() <= MAX_LABEL_BYTES
            && !text.chars().any(char::is_control)
    })
}

/// The report retains the same nested counter shape as provider observations.
/// Only bounded nonnegative counters are accepted; price and opaque data belong
/// in the trusted evidence, not in this accounting decision.
fn valid_report_fields(fields: &BTreeMap<String, Value>) -> bool {
    fields.len() <= FIELDS.len() + 2
        && fields.iter().all(|(key, value)| {
            let details: &[&str] = match key.as_str() {
                "prompt_tokens_details" => &[
                    "cached_tokens",
                    "cache_write_tokens",
                    "audio_tokens",
                    "cache_creation_input_tokens",
                ],
                "input_tokens_details" => &["cached_tokens", "cache_write_tokens", "audio_tokens"],
                "completion_tokens_details" | "output_tokens_details" => &[
                    "reasoning_tokens",
                    "accepted_prediction_tokens",
                    "rejected_prediction_tokens",
                    "audio_tokens",
                ],
                _ => return FIELDS.contains(&key.as_str()) && value.as_u64().is_some(),
            };
            value.as_object().is_some_and(|object| {
                object.len() <= details.len()
                    && object.iter().all(|(key, value)| {
                        details.contains(&key.as_str())
                            && value.as_u64().is_some()
                            // The Chat adapter retains this counter but does not
                            // use it as an alias for cache_write_tokens.
                            && (key != "cache_creation_input_tokens"
                                || value.as_u64() == Some(0))
                    })
            })
        })
}

fn report_counter(fields: &BTreeMap<String, Value>, key: &str) -> Option<u64> {
    fields.get(key)?.as_u64()
}

fn report_detail(fields: &BTreeMap<String, Value>, object: &str, key: &str) -> u64 {
    fields
        .get(object)
        .and_then(|value| value.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Normalize the supported protocol counters with checked arithmetic. The
/// trusted verifier must additionally prove that these are the exact final
/// counters from the complete capture, rather than a local default or estimate.
fn reported_counters(fields: &BTreeMap<String, Value>) -> Option<Usage> {
    let mut usage = Usage::default();
    let chat = fields.contains_key("prompt_tokens") || fields.contains_key("completion_tokens");
    let tokens = fields.contains_key("input_tokens") || fields.contains_key("output_tokens");
    let gemini =
        fields.contains_key("promptTokenCount") || fields.contains_key("candidatesTokenCount");
    if usize::from(chat) + usize::from(tokens) + usize::from(gemini) != 1 {
        return None;
    }
    let allowed: &[&str] = if chat {
        &[
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "prompt_tokens_details",
            "completion_tokens_details",
        ]
    } else if gemini {
        &[
            "promptTokenCount",
            "candidatesTokenCount",
            "cachedContentTokenCount",
            "thoughtsTokenCount",
            "toolUsePromptTokenCount",
            "totalTokenCount",
        ]
    } else {
        &[
            "input_tokens",
            "output_tokens",
            "total_tokens",
            "input_tokens_details",
            "output_tokens_details",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
    };
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return None;
    }
    if chat {
        let prompt = report_counter(fields, "prompt_tokens")?;
        usage.output = report_counter(fields, "completion_tokens")?;
        usage.cache_read = report_detail(fields, "prompt_tokens_details", "cached_tokens");
        usage.cache_write = report_detail(fields, "prompt_tokens_details", "cache_write_tokens");
        usage.reasoning = report_detail(fields, "completion_tokens_details", "reasoning_tokens");
        usage.input = prompt
            .checked_sub(usage.cache_read)?
            .checked_sub(usage.cache_write)?;
    } else if gemini {
        let prompt = report_counter(fields, "promptTokenCount")?;
        usage.cache_read = report_counter(fields, "cachedContentTokenCount").unwrap_or(0);
        usage.reasoning = report_counter(fields, "thoughtsTokenCount").unwrap_or(0);
        usage.input = prompt
            .checked_sub(usage.cache_read)?
            .checked_add(report_counter(fields, "toolUsePromptTokenCount").unwrap_or(0))?;
        usage.output =
            report_counter(fields, "candidatesTokenCount")?.checked_add(usage.reasoning)?;
    } else {
        usage.input = report_counter(fields, "input_tokens")?;
        usage.output = report_counter(fields, "output_tokens")?;
        if fields.contains_key("cache_read_input_tokens")
            || fields.contains_key("cache_creation_input_tokens")
        {
            if fields.contains_key("input_tokens_details")
                || fields.contains_key("output_tokens_details")
            {
                return None;
            }
            usage.cache_read = report_counter(fields, "cache_read_input_tokens").unwrap_or(0);
            usage.cache_write = report_counter(fields, "cache_creation_input_tokens").unwrap_or(0);
        } else {
            usage.cache_read = report_detail(fields, "input_tokens_details", "cached_tokens");
            usage.cache_write = report_detail(fields, "input_tokens_details", "cache_write_tokens");
            usage.reasoning = report_detail(fields, "output_tokens_details", "reasoning_tokens");
            usage.input = usage
                .input
                .checked_sub(usage.cache_read)?
                .checked_sub(usage.cache_write)?;
        }
    }
    usage.total_tokens = usage
        .input
        .checked_add(usage.output)?
        .checked_add(usage.cache_read)?
        .checked_add(usage.cache_write)?;
    if usage.reasoning > usage.output
        || report_counter(fields, "total_tokens").is_some_and(|total| total != usage.total_tokens)
        || report_counter(fields, "totalTokenCount")
            .is_some_and(|total| total != usage.total_tokens)
    {
        return None;
    }
    Some(usage)
}

fn validate_report(report: &ModelUsageObservation) -> Result<(), RuntimeError> {
    let terminal = matches!(
        (report.event.as_str(), report.stop_reason),
        (
            "done",
            StopReason::Stop | StopReason::Length | StopReason::ToolUse
        ) | ("error", StopReason::Error)
    );
    if !terminal
        || report.usage_status != UsageStatus::Reported
        || report.metadata_redacted
        || !valid_response_label(&report.response_id)
        || !valid_response_label(&report.response_model)
        || checked_tokens(&report.usage).is_none()
        || !valid_report_fields(&report.usage_fields)
        || reported_counters(&report.usage_fields).as_ref() != Some(&report.usage)
    {
        return Err(invalid_report());
    }
    Ok(())
}

fn no_dispatch_observation(observation: &ModelUsageObservation) -> bool {
    matches!(observation.event.as_str(), "start" | "error")
        && observation.usage_status == UsageStatus::Unknown
        && observation.usage == Usage::default()
        && observation.usage_fields.is_empty()
        && observation.response_id.is_none()
        && observation.response_model.is_none()
        && !observation.metadata_redacted
        && matches!(
            observation.stop_reason,
            StopReason::Pending | StopReason::Error | StopReason::Aborted
        )
}

fn unreconciled_hold(record: &ModelRequestRecord) -> Result<(), RuntimeError> {
    if record.reconciliation.is_some()
        || record.settled_tokens.is_some()
        || record.reservation_cursor.is_none()
        || record.reservation_tokens == 0
        || record.outstanding_reservation_tokens != record.reservation_tokens
        || matches!(
            record.terminal,
            Some(
                ModelRequestTerminal::Completed
                    | ModelRequestTerminal::CancelledBeforeDispatch
                    | ModelRequestTerminal::Rejected
            )
        )
        // These old pre-dispatch cancellation facts already project as settled.
        || (record.terminal == Some(ModelRequestTerminal::Cancelled)
            && record.started_cursor.is_none()
            && record.observations.is_empty())
    {
        return Err(RuntimeError::InvalidInput(
            "model request reconciliation contradicts its original settlement or reservation"
                .into(),
        ));
    }
    Ok(())
}

fn no_dispatch_message(message: &AssistantMessage) -> bool {
    message.content.is_empty()
        && message.provider_metadata.is_empty()
        && message.usage_status == UsageStatus::Unknown
        && message.usage == Usage::default()
        && message.usage_fields.is_empty()
        && message.response_id.is_none()
        && message.response_model.is_none()
        && matches!(
            message.stop_reason,
            StopReason::Pending | StopReason::Error | StopReason::Aborted
        )
}

/// Accounting observations deliberately omit content. Check the original Agent
/// events too, through this request's MessageEnd and before the next model turn.
fn verify_no_response(
    store: &SessionStore,
    record: &ModelRequestRecord,
) -> Result<(), RuntimeError> {
    let snapshot = store.snapshot(&record.identity.lease.session_id)?;
    let mut next_request = false;
    visit_selected(store, &snapshot, "agent", None, |event| {
        if next_request
            || event.cursor <= record.reservation_cursor.ok_or_else(corrupt)?
            || event.attempt.as_ref() != Some(&record.identity.lease)
        {
            return Ok(());
        }
        let CoreEvent::Agent { event } = &event.event else {
            return Ok(());
        };
        use cayw_agent::AgentEvent;
        let message = match event.as_ref() {
            AgentEvent::MessageStart {
                message: cayw_ai::Message::Assistant(_),
            } => {
                next_request = true;
                None
            }
            AgentEvent::MessageUpdate { message, .. }
            | AgentEvent::TurnEnd { message, .. }
            | AgentEvent::MessageEnd {
                message: cayw_ai::Message::Assistant(message),
            } => Some(message),
            _ => None,
        };
        if message.is_some_and(|message| !no_dispatch_message(message)) {
            return Err(RuntimeError::InvalidInput(
                "model request non-dispatch contradicts its original Agent response".into(),
            ));
        }
        Ok(())
    })
}

fn usage_prefix(previous: &Usage, report: &Usage) -> bool {
    let input = |usage: &Usage| {
        usage
            .input
            .checked_add(usage.cache_read)?
            .checked_add(usage.cache_write)
    };
    input(previous)
        .zip(input(report))
        .is_some_and(|(previous, report)| previous <= report)
        && previous.output <= report.output
        && previous.cache_read <= report.cache_read
        && previous.cache_write <= report.cache_write
        && previous.reasoning <= report.reasoning
        && previous.total_tokens <= report.total_tokens
}

fn counter_prefix(previous: &Value, report: Option<&Value>) -> bool {
    match previous {
        Value::Null => true,
        Value::Number(number) => number
            .as_u64()
            .zip(report.and_then(Value::as_u64))
            .is_some_and(|(previous, report)| previous <= report),
        Value::Object(previous) => report.and_then(Value::as_object).is_some_and(|report| {
            previous
                .iter()
                .all(|(key, value)| counter_prefix(value, report.get(key)))
        }),
        _ => false,
    }
}

fn verify_reported_observation(
    previous: &ModelUsageObservation,
    report: &ModelUsageObservation,
) -> Result<(), RuntimeError> {
    if previous.metadata_redacted
        || previous.usage_status == UsageStatus::Invalid
        || (previous.usage_status == UsageStatus::Reported
            && checked_tokens(&previous.usage).is_none())
        || previous
            .response_id
            .as_ref()
            .is_some_and(|id| Some(id) != report.response_id.as_ref())
        || previous
            .response_model
            .as_ref()
            .is_some_and(|model| Some(model) != report.response_model.as_ref())
        || (matches!(
            previous.stop_reason,
            StopReason::Stop | StopReason::Length | StopReason::ToolUse
        ) && previous.stop_reason != report.stop_reason)
        || !usage_prefix(&previous.usage, &report.usage)
        || previous
            .usage_fields
            .iter()
            .any(|(key, value)| !counter_prefix(value, report.usage_fields.get(key)))
    {
        return Err(invalid_report());
    }
    Ok(())
}

fn verify_reported_response(
    store: &SessionStore,
    record: &ModelRequestRecord,
    report: &ModelUsageObservation,
) -> Result<(), RuntimeError> {
    let snapshot = store.snapshot(&record.identity.lease.session_id)?;
    let mut next_request = false;
    visit_selected(store, &snapshot, "agent", None, |event| {
        if next_request
            || event.cursor <= record.reservation_cursor.ok_or_else(corrupt)?
            || event.attempt.as_ref() != Some(&record.identity.lease)
        {
            return Ok(());
        }
        let CoreEvent::Agent { event } = &event.event else {
            return Ok(());
        };
        use cayw_agent::AgentEvent;
        let message = match event.as_ref() {
            AgentEvent::MessageStart {
                message: cayw_ai::Message::Assistant(_),
            } => {
                next_request = true;
                return Ok(());
            }
            AgentEvent::MessageUpdate { message, event } => {
                if message != event.message() {
                    return Err(invalid_report());
                }
                message
            }
            AgentEvent::TurnEnd { message, .. }
            | AgentEvent::MessageEnd {
                message: cayw_ai::Message::Assistant(message),
            } => message,
            _ => return Ok(()),
        };
        if message.provider != record.identity.provider || message.model != record.identity.model {
            return Err(invalid_report());
        }
        verify_reported_observation(&message_observation("usage_update", message), report)
    })
}

fn reconciliation_tokens(
    store: &SessionStore,
    record: &ModelRequestRecord,
    outcome: &ModelRequestReconciledOutcome,
) -> Result<u64, RuntimeError> {
    unreconciled_hold(record)?;
    match outcome {
        ModelRequestReconciledOutcome::NotDispatched { .. } => {
            if record
                .observations
                .iter()
                .any(|entry| !no_dispatch_observation(&entry.observation))
            {
                return Err(RuntimeError::InvalidInput(
                    "model request non-dispatch contradicts its original usage or response observations".into(),
                ));
            }
            verify_no_response(store, record)?;
            Ok(0)
        }
        ModelRequestReconciledOutcome::ReportedUsage { report } => {
            validate_report(report)?;
            let tokens = checked_tokens(&report.usage).ok_or_else(invalid_report)?;
            if record.started_cursor.is_none()
                || tokens > record.reservation_tokens
                || tokens > record.token_limit
                || record
                    .output_max_tokens
                    .is_none_or(|limit| report.usage.output > limit)
            {
                return Err(invalid_report());
            }
            for entry in &record.observations {
                verify_reported_observation(&entry.observation, report)?;
            }
            verify_reported_response(store, record, report)?;
            Ok(tokens)
        }
    }
}

fn apply_reconciliation(
    store: &SessionStore,
    records: &mut BTreeMap<String, ModelRequestRecord>,
    event: &EventRecord,
) -> Result<(), RuntimeError> {
    let CoreEvent::Host { kind, data } = &event.event else {
        return Err(corrupt());
    };
    let decision: ReconciliationDecision =
        serde_json::from_value(data.clone()).map_err(|_| corrupt())?;
    let input = decision.decision;
    let record = records
        .get_mut(&input.expected.identity.request_id)
        .ok_or_else(corrupt)?;
    if kind != RECONCILIATION_KIND
        || decision.actor != input.actor
        || event.attempt.is_some()
        || event.session_id != record.identity.lease.session_id
        || event.cursor <= record.last_cursor
        || input.expected != ModelRequestOrigin::from(&*record)
    {
        return Err(corrupt());
    }
    validate_reconciliation(&input).map_err(|_| corrupt())?;
    let tokens = reconciliation_tokens(store, record, &input.outcome).map_err(|_| corrupt())?;
    record.settled_tokens = Some(tokens);
    record.outstanding_reservation_tokens = 0;
    record.reconciliation = Some(ModelRequestReconciliationRecord {
        cursor: event.cursor,
        reconciliation: input,
    });
    Ok(())
}

impl Runtime {
    /// Append a trusted decision for one stopped, unsettled model request.
    /// This releases only that request's token hold. It never resumes a task,
    /// replaces original history, clears other unknowns or reports a price.
    pub async fn reconcile_model_request(
        &self,
        input: ModelRequestReconciliation,
    ) -> Result<ModelRequestReconciliationRecord, RuntimeError> {
        validate_reconciliation(&input)?;
        let session = &input.expected.identity.lease.session_id;
        let budget = {
            let _coordinate = self.inner.coordination.enter()?;
            lineage_root(&self.inner.store, session)?
        };
        let _budget = self
            .inner
            .budget_lock(&budget)
            .try_lock_owned()
            .map_err(|_| RuntimeError::Busy)?;
        let _coordinate = self.inner.coordination.enter()?;
        if lineage_root(&self.inner.store, session)? != budget
            || input.expected.identity.budget_session_id != budget
        {
            return Err(RuntimeError::InvalidInput(
                "model request reconciliation changed its budget identity".into(),
            ));
        }
        let projection = project(&self.inner.store, session, self.model_token_limit())?;
        let record = projection
            .ledger
            .requests
            .iter()
            .find(|record| record.identity.request_id == input.expected.identity.request_id)
            .ok_or_else(|| RuntimeError::InvalidInput("original model request is absent".into()))?;
        if input.expected != ModelRequestOrigin::from(record) {
            return Err(RuntimeError::InvalidInput(
                "model request reconciliation changed its original identity or cursors".into(),
            ));
        }
        if let Some(existing) = &record.reconciliation {
            return if existing.reconciliation == input {
                Ok(existing.clone())
            } else {
                Err(RuntimeError::InvalidInput(
                    "model request already has a different reconciliation".into(),
                ))
            };
        }
        unreconciled_hold(record)?;
        let _owners = self.stopped_model_budget(&budget)?;
        let snapshot = self.inner.store.snapshot(session)?;
        let task = self.inner.queue.get(&record.identity.task_id)?;
        if task.session_id != *session
            || task.attempt_id.as_deref() != Some(&record.identity.lease.attempt_id)
            || task.worker_id.as_deref() != Some(&record.identity.lease.worker_id)
        {
            return Err(RuntimeError::InvalidInput(
                "model request reconciliation differs from its original Task".into(),
            ));
        }
        reconciliation_tokens(&self.inner.store, record, &input.outcome)?;
        if !projection.room_with_limit(1, MAX_LEDGER_EVENTS) {
            return Err(RuntimeError::Budget("model accounting event capacity"));
        }
        let verifier = self
            .inner
            .config
            .model_request_reconciliation_verifier
            .as_ref()
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "model request reconciliation requires a trusted host verifier".into(),
                )
            })?;
        let data = serde_json::to_value(&input)?;
        if serde_json::to_vec(&json!({"actor": input.actor, "decision": data}))?.len()
            > self.inner.config.max_event_bytes
        {
            return Err(RuntimeError::Budget("model accounting event byte"));
        }
        verifier.verify(&input).map_err(|_| {
            RuntimeError::InvalidInput(
                "trusted host verifier rejected model request reconciliation".into(),
            )
        })?;
        let event = self.inner.store.append_control_event(
            session,
            snapshot.cursor,
            None,
            &input.actor,
            RECONCILIATION_KIND,
            data,
        )?;
        self.inner.changed.notify_waiters();
        Ok(ModelRequestReconciliationRecord {
            cursor: event.cursor,
            reconciliation: input,
        })
    }

    /// Keep every execution owner in this budget tree stopped until the decision
    /// commits. Queue records and file locks cover independent Runtime instances.
    fn stopped_model_budget(&self, budget: &str) -> Result<Vec<std::fs::File>, RuntimeError> {
        let links = lineage_links(&self.inner.store)?;
        let mut owners = Vec::new();
        for snapshot in self.inner.store.list_sessions()? {
            if lineage(&snapshot.id, &links)? != budget {
                continue;
            }
            if snapshot.attempt.is_some()
                || crate::lock(&self.inner.active).contains_key(&snapshot.id)
            {
                return Err(RuntimeError::Busy);
            }
            for task in self.inner.queue.list(&snapshot.id)? {
                if task.holds_slot
                    || matches!(
                        task.state,
                        crate::TaskState::Running
                            | crate::TaskState::WaitingApproval
                            | crate::TaskState::Draining
                    )
                    || (task.state == crate::TaskState::Queued && task.worker_id.is_some())
                {
                    return Err(RuntimeError::Busy);
                }
                owners.push(self.inner.coordination.task(&task.id)?);
            }
        }
        Ok(owners)
    }

    fn model_token_limit(&self) -> u64 {
        self.inner.config.local_agents.as_ref().map_or(
            self.inner.config.max_total_tokens,
            |agents| {
                agents
                    .shared_total_tokens
                    .min(self.inner.config.max_total_tokens)
            },
        )
    }

    /// Read-only ledger for the complete persistent session budget lineage.
    /// Missing prices stay unknown even when reported tokens have settled.
    pub fn model_requests(&self, session: &str) -> Result<ModelRequestLedger, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        read(&self.inner.store, session, self.model_token_limit())
    }
}

fn append(inner: &Inner, lease: &AttemptLease, entry: &Entry) -> Result<u64, RuntimeError> {
    let data = serde_json::to_value(entry)?;
    if serde_json::to_vec(&data)?.len() > inner.config.max_event_bytes {
        return Err(RuntimeError::Budget("model accounting event byte"));
    }
    let event = inner.store.append_host_event(lease, KIND, data)?;
    inner.changed.notify_waiters();
    Ok(event.cursor)
}

fn checked_tokens(usage: &Usage) -> Option<u64> {
    let total = usage
        .input
        .checked_add(usage.output)?
        .checked_add(usage.cache_read)?
        .checked_add(usage.cache_write)?;
    (total == usage.total_tokens && usage.reasoning <= usage.output).then_some(total)
}

fn terminal_tokens(
    terminal: ModelRequestTerminal,
    observation: &ModelUsageObservation,
) -> Option<u64> {
    let terminal_matches = match terminal {
        ModelRequestTerminal::Completed => {
            observation.event == "done"
                && matches!(
                    observation.stop_reason,
                    StopReason::Stop | StopReason::Length | StopReason::ToolUse
                )
        }
        ModelRequestTerminal::Failed => {
            observation.event == "error" && observation.stop_reason == StopReason::Error
        }
        _ => false,
    };
    if terminal_matches && observation.usage_status == UsageStatus::Reported {
        checked_tokens(&observation.usage)
    } else {
        None
    }
}

fn label(text: &str) -> (String, bool) {
    let mut result = String::new();
    let mut redacted = false;
    for character in text.chars() {
        if character.is_control() || result.len() + character.len_utf8() > MAX_LABEL_BYTES {
            redacted = true;
            continue;
        }
        result.push(character);
    }
    (result, redacted)
}

const FIELDS: &[&str] = &[
    "prompt_tokens",
    "completion_tokens",
    "total_tokens",
    "prompt_tokens_details",
    "completion_tokens_details",
    "cached_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "accepted_prediction_tokens",
    "rejected_prediction_tokens",
    "audio_tokens",
    "input_tokens",
    "output_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
    "promptTokenCount",
    "candidatesTokenCount",
    "cachedContentTokenCount",
    "thoughtsTokenCount",
    "toolUsePromptTokenCount",
    "totalTokenCount",
];
fn counter_value(value: &Value, depth: usize, redacted: &mut bool) -> Value {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        Value::Object(object) if depth < 2 && object.len() <= 32 => {
            let mut result = serde_json::Map::new();
            for (key, value) in object {
                if FIELDS.contains(&key.as_str()) {
                    result.insert(key.clone(), counter_value(value, depth + 1, redacted));
                } else {
                    *redacted = true;
                }
            }
            Value::Object(result)
        }
        _ => {
            *redacted = true;
            let kind = match value {
                Value::String(_) => "string",
                Value::Array(_) => "array",
                _ => "object",
            };
            json!({"redacted_type":kind})
        }
    }
}

fn observation(event: &AssistantEvent) -> ModelUsageObservation {
    let name = match event {
        AssistantEvent::Start { .. } => "start",
        AssistantEvent::TextStart { .. } => "text_start",
        AssistantEvent::TextDelta { .. } => "text_delta",
        AssistantEvent::TextEnd { .. } => "text_end",
        AssistantEvent::ThinkingStart { .. } => "thinking_start",
        AssistantEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantEvent::ToolCallStart { .. } => "tool_call_start",
        AssistantEvent::ToolCallDelta { .. } => "tool_call_delta",
        AssistantEvent::ToolCallEnd { .. } => "tool_call_end",
        AssistantEvent::UsageUpdate { .. } => "usage_update",
        AssistantEvent::Done { .. } => "done",
        AssistantEvent::Error { .. } => "error",
    };
    message_observation(name, event.message())
}

fn message_observation(event: &str, message: &AssistantMessage) -> ModelUsageObservation {
    let mut metadata_redacted = false;
    let mut usage_fields = BTreeMap::new();
    for (key, value) in &message.usage_fields {
        if FIELDS.contains(&key.as_str()) {
            usage_fields.insert(key.clone(), counter_value(value, 0, &mut metadata_redacted));
        } else {
            metadata_redacted = true;
        }
    }
    let mut response_label = |value: &Option<String>| {
        value.as_ref().map(|value| {
            let (value, redacted) = label(value);
            metadata_redacted |= redacted;
            value
        })
    };
    let response_id = response_label(&message.response_id);
    let response_model = response_label(&message.response_model);
    ModelUsageObservation {
        event: event.into(),
        usage_status: message.usage_status,
        usage: message.usage.clone(),
        usage_fields,
        response_id,
        response_model,
        stop_reason: message.stop_reason,
        metadata_redacted,
    }
}

pub(crate) struct AccountedProvider {
    context: Arc<RunContext>,
    turn: AtomicU64,
}
impl AccountedProvider {
    pub(crate) fn new(context: Arc<RunContext>) -> Self {
        Self {
            context,
            turn: AtomicU64::new(0),
        }
    }
}

struct RequestGuard {
    context: Arc<RunContext>,
    id: String,
    reservation: u64,
    cursor: u64,
    tokens: u64,
    event_limit: usize,
    observations: usize,
    started: bool,
    finished: bool,
    recording_failed: bool,
    last_received: Option<ModelUsageObservation>,
    last_persisted: Option<ModelUsageObservation>,
    _permit: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl RequestGuard {
    async fn reserve(
        context: Arc<RunContext>,
        request: &mut ModelRequest,
        id: String,
        turn: u64,
        cancel: &CancellationToken,
    ) -> Result<Self, RuntimeError> {
        context.check(&context.cancel)?;
        if context.cancel.is_cancelled() || cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if context
            .inner
            .queue
            .model_cancelled(&context.task, &context.lease)?
        {
            return Err(RuntimeError::Cancelled);
        }
        let budget_session_id = lineage_root(&context.inner.store, &context.lease.session_id)?;
        let budget_lock = context.inner.budget_lock(&budget_session_id);
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            _ = context.cancel.cancelled() => return Err(RuntimeError::Cancelled),
            permit = budget_lock.lock_owned() => permit,
        };
        context.check(&context.cancel)?;
        if context.cancel.is_cancelled() || cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if context
            .inner
            .queue
            .model_cancelled(&context.task, &context.lease)?
        {
            return Err(RuntimeError::Cancelled);
        }
        Self::reserve_with_permit(context, request, id, turn, MAX_LEDGER_EVENTS, permit)
    }

    #[cfg(test)]
    fn reserve_with_limit(
        context: Arc<RunContext>,
        request: &mut ModelRequest,
        id: String,
        turn: u64,
        event_limit: usize,
    ) -> Result<Self, RuntimeError> {
        let budget_session_id = lineage_root(&context.inner.store, &context.lease.session_id)?;
        let budget_lock = context.inner.budget_lock(&budget_session_id);
        let permit = budget_lock
            .try_lock_owned()
            .map_err(|_| RuntimeError::Busy)?;
        Self::reserve_with_permit(context, request, id, turn, event_limit, permit)
    }

    #[cfg(test)]
    fn reserve_test(
        context: Arc<RunContext>,
        request: &mut ModelRequest,
        id: String,
        turn: u64,
    ) -> Result<Self, RuntimeError> {
        Self::reserve_with_limit(context, request, id, turn, MAX_LEDGER_EVENTS)
    }

    fn reserve_with_permit(
        context: Arc<RunContext>,
        request: &mut ModelRequest,
        id: String,
        turn: u64,
        event_limit: usize,
        permit: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<Self, RuntimeError> {
        context.check(&context.cancel)?;
        let _coordinate = context.inner.coordination.enter()?;
        if context
            .inner
            .queue
            .model_cancelled(&context.task, &context.lease)?
            || context.cancel.is_cancelled()
        {
            return Err(RuntimeError::Cancelled);
        }
        context.inner.store.validate_lease(&context.lease)?;
        if request.options.max_tokens == Some(0) || request.model.max_output_tokens == Some(0) {
            return Err(RuntimeError::InvalidInput(
                "model output token limit must be positive".into(),
            ));
        }
        let projection = project(
            &context.inner.store,
            &context.lease.session_id,
            context.scope.token_limit(),
        )?;
        if projection.ledger.requests.len() >= MAX_REQUESTS {
            return Err(RuntimeError::Budget("model request history capacity"));
        }
        let has_rejection_room = projection.room_with_limit(1, event_limit);
        // Reserved + Started + at least one observation + a protected Finished.
        let has_request_room = projection.room_with_limit(4, event_limit);
        let ledger = projection.ledger;
        context.scope.tokens().store(
            ledger.settled_tokens.saturating_add(ledger.reserved_tokens),
            Ordering::Release,
        );
        let identity = ModelRequestIdentity {
            request_id: id.clone(),
            lease: context.lease.clone(),
            task_id: context.task.id.clone(),
            budget_session_id: ledger.budget_session_id,
            turn,
            provider: request.model.provider.clone(),
            model: request.model.id.clone(),
        };
        let invalid_identity = [&identity.provider, &identity.model]
            .into_iter()
            .any(|text| {
                text.is_empty()
                    || text.len() > MAX_LABEL_BYTES
                    || text.chars().any(char::is_control)
            });
        if invalid_identity {
            return Err(RuntimeError::InvalidInput(
                "model identity exceeds accounting bounds".into(),
            ));
        }
        let reason = if ledger.budget_unknown {
            Some("unsettled_model_reservation")
        } else if ledger.overflowed || ledger.remaining_tokens == 0 {
            Some("model_token_budget_exhausted")
        } else {
            None
        };
        if let Some(reason) = reason {
            if !has_rejection_room {
                return Err(RuntimeError::Budget("model accounting event capacity"));
            }
            append(
                &context.inner,
                &context.lease,
                &Entry::Rejected {
                    identity,
                    token_limit: ledger.token_limit,
                    reason: reason.into(),
                },
            )?;
            return Err(RuntimeError::Budget("model token reservation"));
        }
        if !has_request_room {
            return Err(RuntimeError::Budget("model accounting event capacity"));
        }
        request.messages = Runtime {
            inner: context.inner.clone(),
        }
        .model_input_messages(&context.lease.session_id, &request.messages)?;
        request.options.max_tokens = Some(
            request
                .options
                .max_tokens
                .or(request.model.max_output_tokens)
                .unwrap_or(ledger.remaining_tokens)
                .min(ledger.remaining_tokens),
        );
        let cursor = append(
            &context.inner,
            &context.lease,
            &Entry::Reserved {
                identity,
                policy: ModelReservationPolicy::EntireSessionRemainder,
                tokens: ledger.remaining_tokens,
                token_limit: ledger.token_limit,
                output_max_tokens: request.options.max_tokens.expect("set above"),
            },
        )?;
        context
            .scope
            .tokens()
            .store(ledger.token_limit, Ordering::Release);
        drop(_coordinate);
        Ok(Self {
            context,
            id,
            reservation: cursor,
            cursor,
            tokens: ledger.remaining_tokens,
            event_limit,
            observations: 0,
            started: false,
            finished: false,
            recording_failed: false,
            last_received: None,
            last_persisted: None,
            _permit: Some(permit),
        })
    }
    fn record(&mut self, mut entry: Entry) -> Result<Option<u64>, RuntimeError> {
        let result = (|| {
            let _coordinate = self.context.inner.coordination.enter()?;
            // Peer cancellation and this commit use the same process-shared
            // fence. A local CancellationToken is not the durable authority.
            let cancelled = self
                .context
                .inner
                .queue
                .model_cancelled(&self.context.task, &self.context.lease)?;
            if cancelled {
                self.context.cancel.cancel();
            }
            if let Entry::Observed { observation, .. } = &entry
                && !self.recording_failed
                && !matches!(observation.event.as_str(), "done" | "error")
                && self.last_persisted.as_ref() == Some(observation)
            {
                // Agent events retain every text fragment. Repeated accounting
                // metadata needs no new row, but still checks durable ownership.
                self.context
                    .inner
                    .store
                    .validate_lease(&self.context.lease)?;
                return Ok(None);
            }
            if matches!(entry, Entry::Started { .. }) {
                if cancelled || self.context.cancel.is_cancelled() {
                    return Err(RuntimeError::Cancelled);
                }
                self.context.inner.queue.check(&self.context.task)?;
            }
            let settled = match &mut entry {
                Entry::Finished {
                    terminal,
                    settled_tokens,
                    ..
                } => {
                    // A pre-dispatch release is itself the durable record of the
                    // cancellation. It is only legal before the dispatch marker
                    // and must not be downgraded back into an unsettled hold.
                    let releasing =
                        *terminal == ModelRequestTerminal::CancelledBeforeDispatch && !self.started;
                    if *terminal == ModelRequestTerminal::CancelledBeforeDispatch && !releasing {
                        // Never let a post-dispatch call claim a pre-dispatch
                        // release; retain it conservatively instead.
                        *terminal = ModelRequestTerminal::Cancelled;
                        *settled_tokens = None;
                    } else if (cancelled || self.context.cancel.is_cancelled()) && !releasing {
                        *terminal = ModelRequestTerminal::Cancelled;
                        *settled_tokens = None;
                    }
                    *settled_tokens
                }
                _ => None,
            };
            let projection = project(
                &self.context.inner.store,
                &self.context.lease.session_id,
                self.context.scope.token_limit(),
            )?;
            // Finished consumes its already-protected slot. Every other event
            // must leave all outstanding terminal slots available.
            if !projection.room_with_limit(
                usize::from(!matches!(entry, Entry::Finished { .. })),
                self.event_limit,
            ) {
                return Err(RuntimeError::Budget("model accounting event capacity"));
            }
            let cursor = append(&self.context.inner, &self.context.lease, &entry)?;
            self.cursor = cursor;
            if let Entry::Observed { observation, .. } = entry {
                self.last_persisted = Some(observation);
            }
            Ok(settled)
        })();
        if result
            .as_ref()
            .is_err_and(|error| !matches!(error, RuntimeError::Cancelled))
        {
            self.recording_failed = true;
        }
        result
    }
    fn start(&mut self) -> Result<(), RuntimeError> {
        // This synchronous commit is dispatch admission. A later cancel cannot
        // undo an admitted external request, but it can never settle its hold.
        self.record(Entry::Started {
            request_id: self.id.clone(),
            reservation_cursor: self.reservation,
            previous_cursor: self.cursor,
        })?;
        self.started = true;
        Ok(())
    }
    /// Releases the reservation of a request whose durable cancellation was
    /// observed before the dispatch marker. Only valid before `start()`.
    fn release_cancelled_before_dispatch(&mut self) -> Result<(), RuntimeError> {
        if self.started {
            return Err(RuntimeError::Worker(
                "post-dispatch cancellation cannot release model reservation".into(),
            ));
        }
        self.finish(ModelRequestTerminal::CancelledBeforeDispatch, Some(0))
    }
    fn observe(
        &mut self,
        event: &AssistantEvent,
    ) -> Result<Option<ModelRequestTerminal>, RuntimeError> {
        let mut value = observation(event);
        let invalid = value.usage_status == UsageStatus::Invalid
            || (value.usage_status == UsageStatus::Reported
                && checked_tokens(&value.usage).is_none());
        if invalid {
            value.usage_status = UsageStatus::Invalid;
        }
        let exceeds = value.usage_status == UsageStatus::Reported
            && checked_tokens(&value.usage).is_some_and(|tokens| tokens > self.tokens);
        self.last_received = Some(value.clone());
        // Keep the latest received snapshot for the reserved terminal slot even
        // when another observation cannot be committed within the fixed bound.
        if self.observations >= MAX_OBSERVATIONS - 1 {
            return Err(RuntimeError::Budget("model accounting observation"));
        }
        self.record(Entry::Observed {
            request_id: self.id.clone(),
            reservation_cursor: self.reservation,
            previous_cursor: self.cursor,
            observation: value,
        })?;
        self.observations += 1;
        Ok(if invalid {
            Some(ModelRequestTerminal::InvalidUsage)
        } else if exceeds {
            Some(ModelRequestTerminal::BudgetExceeded)
        } else {
            None
        })
    }
    fn finish(
        &mut self,
        terminal: ModelRequestTerminal,
        settled_tokens: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let settled_tokens = self.record(Entry::Finished {
            request_id: self.id.clone(),
            reservation_cursor: self.reservation,
            previous_cursor: self.cursor,
            terminal,
            settled_tokens,
            last_received: self.last_received.clone(),
        })?;
        self.finished = true;
        if let Some(tokens) = settled_tokens {
            // The durable terminal is already committed. Update only the shared
            // in-memory hold; do not turn an unrelated later read failure into
            // an apparent failed settlement or charge the same request twice.
            let _ = self.context.scope.tokens().fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |occupied| occupied.checked_sub(self.tokens)?.checked_add(tokens),
            );
            debug_assert!(tokens <= self.tokens);
        }
        self._permit.take();
        Ok(())
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.finished {
            // A guard dropped after cancellation but before the dispatch marker
            // provably never reached a Provider. A failed durable read stays
            // held: its state is unresolved.
            let durably_cancelled = self.context.cancel.is_cancelled()
                || matches!(
                    self.context
                        .inner
                        .queue
                        .model_cancelled(&self.context.task, &self.context.lease),
                    Ok(true)
                );
            let released_before_dispatch =
                !self.started && !self.recording_failed && durably_cancelled;
            let terminal = if self.recording_failed {
                ModelRequestTerminal::RecordingFailed
            } else if released_before_dispatch {
                ModelRequestTerminal::CancelledBeforeDispatch
            } else if durably_cancelled {
                ModelRequestTerminal::Cancelled
            } else {
                ModelRequestTerminal::Dropped
            };
            let settled = released_before_dispatch.then_some(0);
            if self.finish(terminal, settled).is_err() {
                self.context.model_recording_failed();
            }
        }
        self._permit.take();
    }
}

fn error_message(message: &AssistantMessage, reason: &'static str) -> AssistantEvent {
    AssistantEvent::Error {
        message: message.clone().failure(StopReason::Error, reason),
    }
}
impl Provider for AccountedProvider {
    fn stream(&self, mut request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let id = uuid::Uuid::new_v4().to_string();
        let turn = self.turn.fetch_add(1, Ordering::AcqRel) + 1;
        let context = self.context.clone();
        let stream:BoxStream<'static,AssistantEvent>=async_stream::stream! {
            let mut partial = AssistantMessage::new(&request.model);
            let mut activity = match crate::scheduler::Activity::model(&context, &cancel).await {
                Ok(activity) => activity,
                Err(error) => {
                    if !matches!(error, RuntimeError::Cancelled | RuntimeError::Budget(_)) {
                        context.persistence_failed(error);
                    }
                    yield error_message(&partial, "model request has no scheduler admission");
                    return;
                }
            };
            let mut guard = match RequestGuard::reserve(context.clone(), &mut request, id, turn, &cancel).await {
                Ok(guard) => guard,
                Err(error) => {
                    if !matches!(error, RuntimeError::Budget(_) | RuntimeError::Cancelled | RuntimeError::InvalidInput(_)) {
                        context.model_recording_failed();
                    }
                    if matches!(error, RuntimeError::Cancelled) {
                        yield AssistantEvent::Error {
                            message: partial.failure(
                                StopReason::Aborted,
                                "model request cancelled before admission",
                            ),
                        };
                    } else {
                        yield error_message(&partial, "model request was refused before provider invocation; inspect its persistent ledger and budget");
                    }
                    return;
                }
            };
            // `start()` checks the durable queue fence and the owner token at
            // commit time. A per-request cancellation observed before the
            // marker is equally provably undispatched.
            if cancel.is_cancelled() {
                if guard.release_cancelled_before_dispatch().is_err() {
                    context.model_recording_failed();
                    yield AssistantEvent::Error {message:partial.failure(StopReason::Aborted,"model request cancelled before dispatch admission; its unused reservation could not be released")};
                } else {
                    yield AssistantEvent::Error {message:partial.failure(StopReason::Aborted,"model request cancelled before dispatch admission; unused reservation released")};
                }
                return;
            }
            if let Err(error) = guard.start() {
                if matches!(error,RuntimeError::Cancelled) {
                    if guard.release_cancelled_before_dispatch().is_err() {
                        context.model_recording_failed();
                        yield AssistantEvent::Error {message:partial.failure(StopReason::Aborted,"model request cancelled before dispatch admission; its unused reservation could not be released")};
                    } else {
                        yield AssistantEvent::Error {message:partial.failure(StopReason::Aborted,"model request cancelled before dispatch admission; unused reservation released")};
                    }
                } else {
                    context.model_recording_failed();
                    let _ = guard.finish(ModelRequestTerminal::RecordingFailed,None);
                    yield error_message(&partial,"model request recording failed before provider invocation");
                }
                return;
            }
            // Everything before `start()` is provably undispatched and releases
            // its own hold there. Once the durable `Started` marker exists, the
            // request is treated as possibly dispatched: a later cancellation
            // aborts the adapter through `provider_cancel`, but it must keep the
            // hold because a crash at this boundary is indistinguishable.
            let provider_cancel=cancel.child_token();
            let _cancel_on_drop=provider_cancel.clone().drop_guard();
            let mut upstream=context.inner.provider.stream(request.clone(),provider_cancel.clone());
            while let Some(event)=upstream.next().await {
                partial=event.message().clone();
                let invalid=match guard.observe(&event) {
                    Ok(invalid)=>invalid,
                    Err(_)=>{
                        context.model_recording_failed();
                        let _ = guard.finish(ModelRequestTerminal::RecordingFailed,None);
                        provider_cancel.cancel();
                        drop(upstream);
                        yield error_message(&partial,"model event recording failed; its reservation remains unsettled");
                        return;
                    }
                };
                if let Some(terminal)=invalid {
                    if terminal==ModelRequestTerminal::InvalidUsage {partial.usage_status=UsageStatus::Invalid;}
                    if guard.finish(terminal,None).is_err() { context.model_recording_failed(); }
                    provider_cancel.cancel();
                    drop(upstream);
                    yield error_message(&partial,"model usage is invalid or exceeds its reservation; reservation retained");
                    return;
                }
                if event.is_terminal() {
                    let message=event.message();
                    let successful=matches!(event,AssistantEvent::Done{..}) && matches!(message.stop_reason,StopReason::Stop|StopReason::Length|StopReason::ToolUse) && !cancel.is_cancelled() && !context.cancel.is_cancelled();
                    let terminal=if successful {ModelRequestTerminal::Completed} else if message.stop_reason==StopReason::Aborted||cancel.is_cancelled()||context.cancel.is_cancelled() {ModelRequestTerminal::Cancelled} else {ModelRequestTerminal::Failed};
                    let settlement = guard.last_received.as_ref().and_then(|last| terminal_tokens(terminal, last));
                    if guard.finish(terminal,settlement).is_err() {
                        context.model_recording_failed();
                        let _ = guard.finish(ModelRequestTerminal::RecordingFailed,None);
                        provider_cancel.cancel();
                        drop(upstream);
                        yield error_message(&partial,"model terminal recording failed; its reservation remains unsettled");
                        return;
                    }
                    drop(upstream);
                    if let Err(error) = activity.finish() {
                        context.persistence_failed(error);
                        yield error_message(&partial,"model completion could not update scheduler activity");
                        return;
                    }
                    if context.cancel.is_cancelled() || cancel.is_cancelled() {
                        yield AssistantEvent::Error {message:partial.failure(StopReason::Aborted,"model request cancelled; late response cannot release its reservation")};
                    } else {
                        yield event;
                    }
                    return;
                }
                yield event;
            }
            if guard.finish(ModelRequestTerminal::MissingTerminal,None).is_err() {context.model_recording_failed();}
            yield error_message(&partial,"model stream ended without a terminal; its reservation remains unsettled");
        }.boxed();
        stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RuntimeConfig, queue::TaskLease};
    use cayw_ai::{Model, StreamOptions};

    struct UnreachableProvider;
    impl Provider for UnreachableProvider {
        fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
            panic!("provider must not run");
        }
    }
    fn context(root: &std::path::Path) -> (Runtime, Arc<RunContext>, ModelRequest) {
        std::fs::create_dir(root.join("workspace")).unwrap();
        let model = Model::new("fixture", "accounting-guard");
        let runtime = Runtime::open(
            root.join("session.db"),
            root.join("workspace"),
            Arc::new(UnreachableProvider),
            model.clone(),
            RuntimeConfig {
                max_total_tokens: 1000,
                ..Default::default()
            },
        )
        .unwrap();
        let session = runtime.create(None).unwrap();
        let task = runtime.enqueue(&session.id, "guard fixture").unwrap();
        let worker = runtime.inner.queue.claim(&task.id).unwrap();
        assert!(runtime.inner.queue.admit(&worker, 4).unwrap());
        let lease = runtime
            .inner
            .store
            .begin_attempt(&session.id, &worker.worker)
            .unwrap();
        runtime
            .inner
            .queue
            .attach(&worker, &lease.attempt_id)
            .unwrap();
        let context = Arc::new(RunContext::model_fixture(
            runtime.inner.clone(),
            lease,
            TaskLease {
                id: worker.id,
                worker: worker.worker,
                fence: worker.fence,
            },
        ));
        let request = ModelRequest {
            model,
            messages: vec![cayw_ai::Message::user("guard fixture")],
            options: StreamOptions::default(),
        };
        (runtime, context, request)
    }

    fn delta(message: &AssistantMessage) -> AssistantEvent {
        AssistantEvent::TextDelta {
            index: 0,
            delta: "synthetic fragment".into(),
            partial: message.clone(),
        }
    }

    #[test]
    fn dropped_request_needs_a_trusted_complete_report_before_its_hold_can_settle() {
        struct CompleteCapture;
        impl ModelRequestReconciliationVerifier for CompleteCapture {
            fn verify(&self, input: &ModelRequestReconciliation) -> Result<(), String> {
                if input.evidence == "complete synthetic capture" {
                    Ok(())
                } else {
                    Err("capture is incomplete".into())
                }
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        let mut message = AssistantMessage::new(&request.model);
        message.response_id = Some("drop-response".into());
        message.response_model = Some("drop-model".into());
        guard.observe(&delta(&message)).unwrap();
        drop(guard);
        let session = &context.lease.session_id;
        let original = runtime.model_requests(session).unwrap().requests[0].clone();
        assert_eq!(original.terminal, Some(ModelRequestTerminal::Dropped));
        runtime
            .inner
            .queue
            .finish(
                &context.task,
                crate::queue::TaskTerminal::Failed,
                Some("synthetic dropped execution".into()),
                |_| Ok(()),
            )
            .unwrap();
        runtime.store().finish_attempt(&context.lease).unwrap();
        let peer = Runtime::open(
            temp.path().join("session.db"),
            temp.path().join("workspace"),
            Arc::new(UnreachableProvider),
            request.model.clone(),
            RuntimeConfig {
                max_total_tokens: 1000,
                model_request_reconciliation_verifier: Some(Arc::new(CompleteCapture)),
                ..Default::default()
            },
        )
        .unwrap();
        let report = ModelUsageObservation {
            event: "done".into(),
            usage_status: UsageStatus::Reported,
            usage: Usage {
                input: 4,
                output: 2,
                total_tokens: 6,
                ..Default::default()
            },
            usage_fields: BTreeMap::from([
                ("input_tokens".into(), json!(4)),
                ("output_tokens".into(), json!(2)),
            ]),
            response_id: message.response_id,
            response_model: message.response_model,
            stop_reason: StopReason::Stop,
            metadata_redacted: false,
        };
        let mut input = ModelRequestReconciliation {
            expected: ModelRequestOrigin::from(&original),
            actor: "fixture-host".into(),
            evidence: "incomplete synthetic capture".into(),
            outcome: ModelRequestReconciledOutcome::ReportedUsage { report },
        };
        assert!(futures::executor::block_on(peer.reconcile_model_request(input.clone())).is_err());
        assert_eq!(peer.model_requests(session).unwrap().requests[0], original);
        input.evidence = "complete synthetic capture".into();
        let before = runtime.store().snapshot(session).unwrap();
        let task = runtime.task(&context.task.id).unwrap();
        let result = futures::executor::block_on(peer.reconcile_model_request(input)).unwrap();
        let after = runtime.model_requests(session).unwrap();
        let mut expected = original;
        expected.settled_tokens = Some(6);
        expected.outstanding_reservation_tokens = 0;
        expected.reconciliation = Some(result.clone());
        assert_eq!(after.requests, vec![expected]);
        assert_eq!(after.settled_tokens, 6);
        assert_eq!(after.reserved_tokens, 0);
        assert_eq!(runtime.task(&context.task.id).unwrap(), task);
        let mut expected_snapshot = before;
        expected_snapshot.cursor = result.cursor;
        assert_eq!(
            runtime.store().snapshot(session).unwrap(),
            expected_snapshot
        );
    }

    #[test]
    fn consecutive_equal_observations_keep_every_metadata_change_and_return() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        let mut message = AssistantMessage::new(&request.model);
        let mut events = vec![delta(&message)];
        events.push(AssistantEvent::TextEnd {
            index: 0,
            partial: message.clone(),
        });
        // A -> B -> A must retain the second A.
        events.push(delta(&message));
        message.usage_status = UsageStatus::Partial;
        events.push(delta(&message));
        message.usage.input = 4;
        message.usage.total_tokens = 4;
        events.push(delta(&message));
        message.usage_fields.insert("input_tokens".into(), json!(4));
        events.push(delta(&message));
        message.response_id = Some("original-response".into());
        events.push(delta(&message));
        message.response_model = Some("original-model".into());
        events.push(delta(&message));
        message.stop_reason = StopReason::Length;
        events.push(delta(&message));
        message.usage_fields.insert("unrecognized".into(), json!(1));
        events.push(delta(&message));
        let expected = events.iter().map(observation).collect::<Vec<_>>();
        assert!(expected.windows(2).all(|pair| pair[0] != pair[1]));
        for event in &events {
            assert_eq!(guard.observe(event).unwrap(), None);
            let cursor = runtime
                .store()
                .snapshot(&context.lease.session_id)
                .unwrap()
                .cursor;
            assert_eq!(guard.observe(event).unwrap(), None);
            assert_eq!(
                runtime
                    .store()
                    .snapshot(&context.lease.session_id)
                    .unwrap()
                    .cursor,
                cursor
            );
        }
        assert_eq!(guard.observations, events.len() * 2);
        guard
            .finish(ModelRequestTerminal::MissingTerminal, None)
            .unwrap();
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(
            ledger.requests[0]
                .observations
                .iter()
                .map(|entry| entry.observation.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(ledger.reserved_tokens, 1000);
        assert_eq!(ledger.settled_tokens, 0);
        assert!(ledger.budget_unknown);
        let reopened = SessionStore::open(temp.path().join("session.db")).unwrap();
        assert_eq!(
            read(&reopened, &context.lease.session_id, 1000).unwrap(),
            ledger
        );
    }

    #[test]
    fn equal_terminal_observations_are_never_coalesced() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        let mut message = AssistantMessage::new(&request.model);
        message.stop_reason = StopReason::Stop;
        let done = AssistantEvent::Done {
            message: message.clone(),
        };
        let error = AssistantEvent::Error {
            message: message.failure(StopReason::Error, "synthetic failure"),
        };
        for event in [&done, &done, &error, &error] {
            assert_eq!(guard.observe(event).unwrap(), None);
        }
        guard.finish(ModelRequestTerminal::Failed, None).unwrap();
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(
            ledger.requests[0]
                .observations
                .iter()
                .map(|entry| entry.observation.event.as_str())
                .collect::<Vec<_>>(),
            ["done", "done", "error", "error"]
        );
        assert_eq!(ledger.reserved_tokens, 1000);
    }

    #[test]
    fn equal_observation_observes_peer_cancel_before_releasing_its_hold() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let peer = Runtime::open(
            temp.path().join("session.db"),
            temp.path().join("workspace"),
            Arc::new(UnreachableProvider),
            request.model.clone(),
            RuntimeConfig {
                max_total_tokens: 1000,
                ..Default::default()
            },
        )
        .unwrap();
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        let mut message = AssistantMessage::new(&request.model);
        message.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        let event = delta(&message);
        assert_eq!(guard.observe(&event).unwrap(), None);
        assert!(
            peer.cancel(&peer.task(&context.task.id).unwrap().cancel_target())
                .unwrap()
        );
        assert!(!context.cancel.is_cancelled());
        assert_eq!(guard.observe(&event).unwrap(), None);
        assert!(context.cancel.is_cancelled());
        assert!(!guard.recording_failed);
        guard
            .finish(ModelRequestTerminal::Completed, Some(8))
            .unwrap();
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(ledger.requests[0].observations.len(), 1);
        assert_eq!(ledger.requests[0].identity.lease, context.lease);
        assert_eq!(
            ledger.requests[0].terminal,
            Some(ModelRequestTerminal::Cancelled)
        );
        assert_eq!(ledger.requests[0].settled_tokens, None);
        assert_eq!(ledger.reserved_tokens, 1000);
        assert_eq!(ledger.cost_status, ModelCostStatus::Unknown);
        assert!(ledger.cost_amount.is_none() && ledger.cost_currency.is_none());
    }

    #[test]
    fn equal_observation_rejects_a_recovered_core_lease() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        let event = delta(&AssistantMessage::new(&request.model));
        guard.observe(&event).unwrap();
        runtime.store().recover(&context.lease.session_id).unwrap();
        let recovered = runtime.store().snapshot(&context.lease.session_id).unwrap();
        assert!(!context.cancel.is_cancelled());
        assert!(guard.observe(&event).is_err());
        assert!(guard.recording_failed);
        drop(guard);
        assert!(crate::lock(&context.model_recording_error).is_some());
        assert!(!context.unknown.load(Ordering::Acquire));
        assert_eq!(
            runtime.store().snapshot(&context.lease.session_id).unwrap(),
            recovered
        );
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(ledger.requests[0].observations.len(), 1);
        assert_eq!(ledger.requests[0].terminal, None);
        assert_eq!(ledger.reserved_tokens, 1000);
    }

    #[test]
    fn equal_observations_still_consume_the_received_observation_limit() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        let mut message = AssistantMessage::new(&request.model);
        guard.observe(&delta(&message)).unwrap();
        let before = runtime.model_requests(&context.lease.session_id).unwrap();
        // Synthetic in-memory boundary; this is not 65,536 provider requests.
        guard.observations = MAX_OBSERVATIONS - 2;
        assert_eq!(guard.observe(&delta(&message)).unwrap(), None);
        assert_eq!(guard.observations, MAX_OBSERVATIONS - 1);
        assert_eq!(
            runtime.model_requests(&context.lease.session_id).unwrap(),
            before
        );
        message.response_id = Some("last-received-at-bound".into());
        assert!(matches!(
            guard.observe(&delta(&message)),
            Err(RuntimeError::Budget("model accounting observation"))
        ));
        guard
            .finish(ModelRequestTerminal::RecordingFailed, None)
            .unwrap();
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(ledger.requests[0].observations.len(), 2);
        assert_eq!(
            ledger.requests[0].observations[1].observation.response_id,
            message.response_id
        );
        assert_eq!(
            ledger.requests[0].terminal_cursor,
            Some(ledger.requests[0].observations[1].cursor)
        );
        assert_eq!(ledger.reserved_tokens, 1000);
    }

    #[test]
    fn recording_failure_cannot_be_hidden_by_an_equal_previous_observation() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        let message = AssistantMessage::new(&request.model);
        let first = delta(&message);
        guard.observe(&first).unwrap();
        let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
        db.execute_batch("CREATE TRIGGER observed_fixture_failure BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='model_request' AND json_extract(NEW.record,'$.event.data.record')='observed' BEGIN SELECT RAISE(ABORT,'observation write fixture'); END;").unwrap();
        let mut changed = message.clone();
        changed.response_id = Some("failed-write".into());
        assert!(guard.observe(&delta(&changed)).is_err());
        assert!(guard.recording_failed);
        assert!(guard.observe(&first).is_err());
        guard
            .finish(ModelRequestTerminal::RecordingFailed, None)
            .unwrap();
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(ledger.requests[0].observations.len(), 1);
        assert_eq!(
            ledger.requests[0].terminal,
            Some(ModelRequestTerminal::RecordingFailed)
        );
        assert_eq!(ledger.reserved_tokens, 1000);
        assert!(!context.unknown.load(Ordering::Acquire));
    }

    #[test]
    fn equal_invalid_and_excess_usage_is_still_rejected() {
        for (usage, expected) in [
            (
                Usage {
                    input: 4,
                    output: 4,
                    total_tokens: 1,
                    ..Default::default()
                },
                ModelRequestTerminal::InvalidUsage,
            ),
            (
                Usage {
                    input: 1001,
                    total_tokens: 1001,
                    ..Default::default()
                },
                ModelRequestTerminal::BudgetExceeded,
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (runtime, context, mut request) = context(temp.path());
            let mut guard = RequestGuard::reserve_test(
                context.clone(),
                &mut request,
                uuid::Uuid::new_v4().to_string(),
                1,
            )
            .unwrap();
            guard.start().unwrap();
            let mut message = AssistantMessage::new(&request.model);
            message.usage = usage;
            message.usage_status = UsageStatus::Reported;
            let event = delta(&message);
            assert_eq!(guard.observe(&event).unwrap(), Some(expected));
            assert_eq!(guard.observe(&event).unwrap(), Some(expected));
            guard.finish(expected, None).unwrap();
            let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
            assert_eq!(ledger.requests[0].observations.len(), 1);
            assert_eq!(ledger.requests[0].terminal, Some(expected));
            assert_eq!(ledger.reserved_tokens, 1000);
        }
    }

    #[test]
    fn stale_lease_drop_preserves_the_original_reservation_without_tool_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let id = uuid::Uuid::new_v4().to_string();
        let mut guard =
            RequestGuard::reserve_test(context.clone(), &mut request, id.clone(), 1).unwrap();
        guard.start().unwrap();
        runtime.store().recover(&context.lease.session_id).unwrap();
        let recovered = runtime.store().snapshot(&context.lease.session_id).unwrap();
        drop(guard);
        assert!(crate::lock(&context.model_recording_error).is_some());
        assert!(!context.unknown.load(Ordering::Acquire));
        assert_eq!(
            runtime.store().snapshot(&context.lease.session_id).unwrap(),
            recovered
        );
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(ledger.requests[0].identity.request_id, id);
        assert_eq!(ledger.requests[0].terminal, None);
        assert!(ledger.budget_unknown);
        assert_eq!(ledger.reserved_tokens, 1000);
        // The effective cap is the trusted configured value (C18). Reopen with
        // the same allocation to compare the durable ledger itself.
        let store = SessionStore::open(temp.path().join("session.db")).unwrap();
        assert_eq!(
            read(&store, &context.lease.session_id, 1000).unwrap(),
            ledger
        );
    }
    #[test]
    fn committed_peer_cancel_before_started_releases_only_its_unused_hold() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let peer = Runtime::open(
            temp.path().join("session.db"),
            temp.path().join("workspace"),
            Arc::new(UnreachableProvider),
            request.model.clone(),
            RuntimeConfig {
                max_total_tokens: 1000,
                ..Default::default()
            },
        )
        .unwrap();
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        assert!(!context.cancel.is_cancelled());
        assert!(
            peer.cancel(&peer.task(&context.task.id).unwrap().cancel_target())
                .unwrap()
        );
        assert!(peer.task(&context.task.id).unwrap().cancel_requested);
        assert!(!context.cancel.is_cancelled());
        assert!(
            matches!(guard.start(), Err(RuntimeError::Cancelled)),
            "persistent cancel must prevent Started even while the owner token is false"
        );
        assert!(!guard.started);
        guard.release_cancelled_before_dispatch().unwrap();
        assert!(guard.finished);
        drop(guard);
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        let record = &ledger.requests[0];
        assert_eq!(record.started_cursor, None);
        assert_eq!(
            record.terminal,
            Some(ModelRequestTerminal::CancelledBeforeDispatch)
        );
        assert_eq!(
            record.settled_tokens,
            Some(0),
            "a request that never crossed the dispatch marker consumed no tokens"
        );
        assert_eq!(record.outstanding_reservation_tokens, 0);
        assert_eq!(ledger.reserved_tokens, 0);
        assert_eq!(ledger.settled_tokens, 0);
        assert_eq!(
            ledger.remaining_tokens, 1000,
            "the same lineage can admit its next model request"
        );
        assert!(!ledger.budget_unknown);
        assert!(!context.unknown.load(Ordering::Acquire));
        let store = SessionStore::open(temp.path().join("session.db")).unwrap();
        assert_eq!(
            read(&store, &context.lease.session_id, 1000).unwrap(),
            ledger,
            "the release must survive reopening the durable ledger"
        );
    }

    #[test]
    fn reservation_without_a_terminal_or_dispatch_marker_is_still_held() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, request) = context(temp.path());
        // Simulate a crash after the durable reservation and before any
        // terminal. Without a `Started` marker the request is not dispatched,
        // but the crash itself is unresolved uncertainty: keep the hold.
        let identity = ModelRequestIdentity {
            request_id: uuid::Uuid::new_v4().to_string(),
            lease: context.lease.clone(),
            task_id: context.task.id.clone(),
            budget_session_id: context.lease.session_id.clone(),
            turn: 1,
            provider: request.model.provider.clone(),
            model: request.model.id.clone(),
        };
        append(
            &context.inner,
            &context.lease,
            &Entry::Reserved {
                identity,
                policy: ModelReservationPolicy::EntireSessionRemainder,
                tokens: 1000,
                token_limit: 1000,
                output_max_tokens: 16,
            },
        )
        .unwrap();
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(ledger.requests[0].started_cursor, None);
        assert_eq!(ledger.requests[0].terminal, None);
        assert_eq!(ledger.reserved_tokens, 1000);
        assert!(ledger.budget_unknown);
        let store = SessionStore::open(temp.path().join("session.db")).unwrap();
        assert_eq!(
            read(&store, &context.lease.session_id, 1000).unwrap(),
            ledger
        );
    }

    #[test]
    fn legacy_pre_dispatch_cancellation_projects_as_a_released_hold() {
        let temp = tempfile::tempdir().unwrap();
        let (_runtime, context, request) = context(temp.path());
        let id = uuid::Uuid::new_v4().to_string();
        let identity = ModelRequestIdentity {
            request_id: id.clone(),
            lease: context.lease.clone(),
            task_id: context.task.id.clone(),
            budget_session_id: context.lease.session_id.clone(),
            turn: 1,
            provider: request.model.provider.clone(),
            model: request.model.id.clone(),
        };
        let reservation = append(
            &context.inner,
            &context.lease,
            &Entry::Reserved {
                identity,
                policy: ModelReservationPolicy::EntireSessionRemainder,
                tokens: 1000,
                token_limit: 1000,
                output_max_tokens: 16,
            },
        )
        .unwrap();
        // This is the exact shape the old implementation persisted for a
        // cancellation that never crossed the dispatch marker.
        append(
            &context.inner,
            &context.lease,
            &Entry::Finished {
                request_id: id,
                reservation_cursor: reservation,
                previous_cursor: reservation,
                terminal: ModelRequestTerminal::Cancelled,
                settled_tokens: None,
                last_received: None,
            },
        )
        .unwrap();
        let store = SessionStore::open(temp.path().join("session.db")).unwrap();
        let ledger = read(&store, &context.lease.session_id, 1000).unwrap();
        assert_eq!(ledger.reserved_tokens, 0);
        assert_eq!(ledger.remaining_tokens, 1000);
        assert!(!ledger.budget_unknown);
        assert_eq!(
            ledger.requests[0].terminal,
            Some(ModelRequestTerminal::Cancelled),
            "the original recorded terminal is preserved"
        );
        assert_eq!(ledger.requests[0].started_cursor, None);
        assert_eq!(ledger.requests[0].settled_tokens, Some(0));
        assert_eq!(ledger.requests[0].outstanding_reservation_tokens, 0);
    }

    #[test]
    fn release_is_refused_after_the_dispatch_marker() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        assert!(matches!(
            guard.release_cancelled_before_dispatch(),
            Err(RuntimeError::Worker(_))
        ));
        assert!(guard.started && !guard.finished);
        guard.finish(ModelRequestTerminal::Cancelled, None).unwrap();
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(ledger.reserved_tokens, 1000);
        assert_eq!(
            ledger.requests[0].terminal,
            Some(ModelRequestTerminal::Cancelled)
        );
        assert!(ledger.budget_unknown);
    }

    #[test]
    fn recorded_dispatch_marker_rejects_a_pre_dispatch_release_event() {
        let temp = tempfile::tempdir().unwrap();
        let (_runtime, context, request) = context(temp.path());
        let id = uuid::Uuid::new_v4().to_string();
        let identity = ModelRequestIdentity {
            request_id: id.clone(),
            lease: context.lease.clone(),
            task_id: context.task.id.clone(),
            budget_session_id: context.lease.session_id.clone(),
            turn: 1,
            provider: request.model.provider.clone(),
            model: request.model.id.clone(),
        };
        let reservation = append(
            &context.inner,
            &context.lease,
            &Entry::Reserved {
                identity,
                policy: ModelReservationPolicy::EntireSessionRemainder,
                tokens: 1000,
                token_limit: 1000,
                output_max_tokens: 16,
            },
        )
        .unwrap();
        let started = append(
            &context.inner,
            &context.lease,
            &Entry::Started {
                request_id: id.clone(),
                reservation_cursor: reservation,
                previous_cursor: reservation,
            },
        )
        .unwrap();
        append(
            &context.inner,
            &context.lease,
            &Entry::Finished {
                request_id: id,
                reservation_cursor: reservation,
                previous_cursor: started,
                terminal: ModelRequestTerminal::CancelledBeforeDispatch,
                settled_tokens: Some(0),
                last_received: None,
            },
        )
        .unwrap();
        let store = SessionStore::open(temp.path().join("session.db")).unwrap();
        assert!(
            read(&store, &context.lease.session_id, 1000).is_err(),
            "a request with a durable Started marker must never release as pre-dispatch"
        );
    }

    #[test]
    fn trusted_configuration_increase_raises_the_same_session_limit_after_settlement() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let session = context.lease.session_id.clone();
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        let mut message = AssistantMessage::new(&request.model);
        message.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        message.stop_reason = StopReason::Stop;
        guard.observe(&AssistantEvent::Done { message }).unwrap();
        guard
            .finish(ModelRequestTerminal::Completed, Some(8))
            .unwrap();
        drop(guard);
        let lower = runtime.model_requests(&session).unwrap();
        assert_eq!(lower.token_limit, 1000);
        assert_eq!(lower.settled_tokens, 8);
        assert_eq!(lower.remaining_tokens, 992);
        assert_eq!(lower.requests[0].token_limit, 1000);
        drop(runtime);
        // A trusted host explicitly allocates a larger budget before reopening
        // the original session tree. Consumption and the original record facts
        // stay intact; only the effective cap is raised.
        let raised = Runtime::open(
            temp.path().join("session.db"),
            temp.path().join("workspace"),
            Arc::new(UnreachableProvider),
            request.model.clone(),
            RuntimeConfig {
                max_total_tokens: 2000,
                ..Default::default()
            },
        )
        .unwrap();
        let ledger = raised.model_requests(&session).unwrap();
        assert_eq!(ledger.token_limit, 2000);
        assert_eq!(ledger.settled_tokens, 8);
        assert_eq!(ledger.reserved_tokens, 0);
        assert_eq!(ledger.remaining_tokens, 1992);
        assert!(!ledger.budget_unknown);
        assert_eq!(ledger.requests, lower.requests);
        assert_eq!(ledger.requests[0].token_limit, 1000);
    }

    #[test]
    fn event_capacity_keeps_received_observation_order_and_a_durable_terminal_slot() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve_with_limit(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
            4,
        )
        .unwrap();
        guard.start().unwrap();
        let mut first = AssistantMessage::new(&request.model);
        first.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        first.usage_status = UsageStatus::Reported;
        assert_eq!(
            guard
                .observe(&AssistantEvent::UsageUpdate {
                    partial: first.clone()
                })
                .unwrap(),
            None
        );
        let before = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(
            guard
                .observe(&AssistantEvent::UsageUpdate {
                    partial: first.clone()
                })
                .unwrap(),
            None
        );
        assert_eq!(
            runtime.model_requests(&context.lease.session_id).unwrap(),
            before
        );
        let second = AssistantEvent::Done {
            message: AssistantMessage {
                usage: Usage {
                    output: 5,
                    total_tokens: 9,
                    ..first.usage.clone()
                },
                stop_reason: StopReason::Stop,
                ..first
            },
        };
        assert!(matches!(
            guard.observe(&second),
            Err(RuntimeError::Budget(_))
        ));
        assert_eq!(
            runtime.model_requests(&context.lease.session_id).unwrap(),
            before
        );
        guard
            .finish(ModelRequestTerminal::RecordingFailed, None)
            .unwrap();
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(ledger.reserved_tokens, 1000);
        assert_eq!(
            ledger.requests[0].terminal,
            Some(ModelRequestTerminal::RecordingFailed)
        );
        assert_eq!(
            ledger.requests[0]
                .observations
                .iter()
                .map(|entry| entry.observation.usage.total_tokens)
                .collect::<Vec<_>>(),
            [8, 9]
        );
        assert!(
            ledger.requests[0].observations[0].cursor < ledger.requests[0].observations[1].cursor
        );
        assert_eq!(
            ledger.requests[0].terminal_cursor,
            Some(ledger.requests[0].observations[1].cursor)
        );
        let (requests, cursor) = (
            ledger.requests.len(),
            runtime
                .store()
                .snapshot(&context.lease.session_id)
                .unwrap()
                .cursor,
        );
        assert!(matches!(
            RequestGuard::reserve_with_limit(
                context.clone(),
                &mut request,
                uuid::Uuid::new_v4().to_string(),
                2,
                4
            ),
            Err(RuntimeError::Budget(_))
        ));
        assert_eq!(
            runtime
                .model_requests(&context.lease.session_id)
                .unwrap()
                .requests
                .len(),
            requests
        );
        assert_eq!(
            runtime
                .store()
                .snapshot(&context.lease.session_id)
                .unwrap()
                .cursor,
            cursor
        );
    }

    #[test]
    fn an_existing_tool_unknown_survives_a_model_recording_error() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve_test(
            context.clone(),
            &mut request,
            uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
        guard.start().unwrap();
        context.unknown.store(true, Ordering::Release);
        let before = runtime.store().snapshot(&context.lease.session_id).unwrap();
        let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
        db.execute_batch("CREATE TRIGGER model_unknown_failure BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='model_request' AND json_extract(NEW.record,'$.event.data.record') IN ('observed','finished') BEGIN SELECT RAISE(ABORT,'existing unknown fixture'); END;").unwrap();
        let event = AssistantEvent::Done {
            message: AssistantMessage::new(&request.model),
        };
        assert!(guard.observe(&event).is_err());
        context.model_recording_failed();
        assert!(
            guard
                .finish(ModelRequestTerminal::RecordingFailed, None)
                .is_err()
        );
        drop(guard);
        assert!(context.unknown.load(Ordering::Acquire));
        assert!(crate::lock(&context.model_recording_error).is_some());
        assert_eq!(
            runtime.store().snapshot(&context.lease.session_id).unwrap(),
            before
        );
        assert_eq!(
            runtime
                .model_requests(&context.lease.session_id)
                .unwrap()
                .reserved_tokens,
            1000
        );
    }

    #[tokio::test]
    async fn already_cancelled_stream_does_not_dispatch_or_leave_a_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, request) = context(temp.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let provider = AccountedProvider::new(context.clone());
        let events = provider.stream(request, cancel).collect::<Vec<_>>().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], AssistantEvent::Error { .. }));
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert!(ledger.requests.is_empty());
        assert_eq!(ledger.reserved_tokens, 0);
        assert_eq!(ledger.remaining_tokens, 1000);
        assert!(!ledger.budget_unknown);
        let store = SessionStore::open(temp.path().join("session.db")).unwrap();
        assert_eq!(
            read(&store, &context.lease.session_id, 1000).unwrap(),
            ledger
        );
    }

    #[tokio::test]
    async fn retained_error_stream_has_a_recorded_terminal_before_yield() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, request) = context(temp.path());
        let connection = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER model_start_fixture BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='host' AND json_extract(NEW.record,'$.event.kind')='model_request' AND json_extract(NEW.record,'$.event.data.record')='started' BEGIN SELECT RAISE(ABORT,'start recording fixture'); END;").unwrap();
        let provider = AccountedProvider::new(context.clone());
        let mut stream = provider.stream(request, CancellationToken::new());
        assert!(matches!(
            stream.next().await,
            Some(AssistantEvent::Error { .. })
        ));
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(
            ledger.requests[0].terminal,
            Some(ModelRequestTerminal::RecordingFailed)
        );
        assert!(ledger.requests[0].terminal_cursor.is_some());
        assert_eq!(ledger.reserved_tokens, 1000);
        assert!(!context.unknown.load(Ordering::Acquire));
        drop(stream);
        assert_eq!(
            runtime.model_requests(&context.lease.session_id).unwrap(),
            ledger
        );
    }
}
