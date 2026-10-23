//! Durable model accounting. No tokenizer or price is assumed. A request holds
//! the budget lineage's entire remainder until a successful, fully reported
//! terminal is committed. This deliberately permits only one unsettled request
//! in a session tree; neither a new task, child, fork nor reopen resets it.
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
                        if terminal != ModelRequestTerminal::Completed
                            || last.observation.event != "done"
                            || last.observation.usage_status != UsageStatus::Reported
                            || !matches!(
                                last.observation.stop_reason,
                                StopReason::Stop | StopReason::Length | StopReason::ToolUse
                            )
                            || checked_tokens(&last.observation.usage) != Some(tokens)
                            || tokens > record.reservation_tokens
                        {
                            return Err(corrupt());
                        }
                        record.outstanding_reservation_tokens = 0;
                        record.settled_tokens = Some(tokens);
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

/// Discover child/fork links before selecting records. Unrelated roots do not
/// share request/event capacity, and their request payloads are not projected.
fn project(
    store: &SessionStore,
    session: &str,
    configured_limit: u64,
) -> Result<Projection, RuntimeError> {
    store.snapshot(session)?;
    let sessions = store.list_sessions()?;
    let mut links = BTreeMap::new();
    for snapshot in &sessions {
        visit(store, snapshot, |event| {
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
    let root = lineage(session, &links)?;
    let mut records = BTreeMap::new();
    let mut ledger_events = 0;
    for snapshot in &sessions {
        if lineage(&snapshot.id, &links)? != root {
            continue;
        }
        visit(store, snapshot, |event| {
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
    let pending_terminals = records
        .values()
        .filter(|record| record.reservation_cursor.is_some() && record.terminal.is_none())
        .count();
    let mut requests = Vec::new();
    let mut token_limit = configured_limit;
    let mut settled_tokens = 0_u64;
    let mut reserved_tokens = 0_u64;
    let mut overflowed = false;
    for record in records.into_values() {
        let canonical = lineage(&record.identity.lease.session_id, &links)?;
        if canonical != record.identity.budget_session_id {
            return Err(corrupt());
        }
        if canonical != root {
            continue;
        }
        if record.reservation_cursor.is_some() {
            token_limit = token_limit.min(record.token_limit);
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

fn visit(
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

impl Runtime {
    /// Read-only ledger for the complete persistent session budget lineage.
    /// Missing prices stay unknown even when reported tokens have settled.
    pub fn model_requests(&self, session: &str) -> Result<ModelRequestLedger, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let limit = self.inner.config.local_agents.as_ref().map_or(
            self.inner.config.max_total_tokens,
            |agents| {
                agents
                    .shared_total_tokens
                    .min(self.inner.config.max_total_tokens)
            },
        );
        read(&self.inner.store, session, limit)
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
    let message = event.message();
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
    let event = match event {
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
    }
    .into();
    ModelUsageObservation {
        event,
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
    finished: bool,
    recording_failed: bool,
    last_received: Option<ModelUsageObservation>,
    last_persisted: Option<ModelUsageObservation>,
}

impl RequestGuard {
    fn reserve(
        context: Arc<RunContext>,
        request: &mut ModelRequest,
        id: String,
        turn: u64,
    ) -> Result<Self, RuntimeError> {
        Self::reserve_with_limit(context, request, id, turn, MAX_LEDGER_EVENTS)
    }
    fn reserve_with_limit(
        context: Arc<RunContext>,
        request: &mut ModelRequest,
        id: String,
        turn: u64,
        event_limit: usize,
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
            finished: false,
            recording_failed: false,
            last_received: None,
            last_persisted: None,
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
                    if cancelled || self.context.cancel.is_cancelled() {
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
        })
        .map(|_| ())
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
        Ok(())
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.finished {
            let terminal = if self.recording_failed {
                ModelRequestTerminal::RecordingFailed
            } else if self.context.cancel.is_cancelled() {
                ModelRequestTerminal::Cancelled
            } else {
                ModelRequestTerminal::Dropped
            };
            if self.finish(terminal, None).is_err() {
                self.context.model_recording_failed();
            }
        }
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
            let mut guard=match RequestGuard::reserve(context.clone(),&mut request,id,turn) {
                Ok(guard)=>guard,
                Err(error)=>{
                    if !matches!(error,RuntimeError::Budget(_)|RuntimeError::Cancelled|RuntimeError::InvalidInput(_)) { context.model_recording_failed(); }
                    yield error_message(&partial,"model request was refused before provider invocation; inspect its persistent ledger and budget");
                    return;
                }
            };
            if let Err(error) = guard.start() {
                if matches!(error,RuntimeError::Cancelled) {
                    if guard.finish(ModelRequestTerminal::Cancelled,None).is_err() {context.model_recording_failed();}
                    yield AssistantEvent::Error {message:partial.failure(StopReason::Aborted,"model request cancelled before dispatch admission; reservation retained")};
                } else {
                    context.model_recording_failed();
                    let _ = guard.finish(ModelRequestTerminal::RecordingFailed,None);
                    yield error_message(&partial,"model request recording failed before provider invocation");
                }
                return;
            }
            if cancel.is_cancelled() || context.cancel.is_cancelled() {
                if guard.finish(ModelRequestTerminal::Cancelled,None).is_err() {context.model_recording_failed();}
                yield AssistantEvent::Error {message:partial.failure(StopReason::Aborted,"model request cancelled before provider invocation; reservation retained")};
                return;
            }
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
                    let settlement=if successful && message.usage_status==UsageStatus::Reported {checked_tokens(&message.usage)} else {None};
                    if guard.finish(terminal,settlement).is_err() {
                        context.model_recording_failed();
                        let _ = guard.finish(ModelRequestTerminal::RecordingFailed,None);
                        provider_cancel.cancel();
                        drop(upstream);
                        yield error_message(&partial,"model terminal recording failed; its reservation remains unsettled");
                        return;
                    }
                    drop(upstream);
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
    fn consecutive_equal_observations_keep_every_metadata_change_and_return() {
        let temp = tempfile::tempdir().unwrap();
        let (runtime, context, mut request) = context(temp.path());
        let mut guard = RequestGuard::reserve(
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
        let mut guard = RequestGuard::reserve(
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
        let mut guard = RequestGuard::reserve(
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
        let mut guard = RequestGuard::reserve(
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
        let mut guard = RequestGuard::reserve(
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
        let mut guard = RequestGuard::reserve(
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
            let mut guard = RequestGuard::reserve(
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
            RequestGuard::reserve(context.clone(), &mut request, id.clone(), 1).unwrap();
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
        let store = SessionStore::open(temp.path().join("session.db")).unwrap();
        assert_eq!(
            read(&store, &context.lease.session_id, 2000).unwrap(),
            ledger
        );
    }
    #[test]
    fn committed_peer_cancel_before_started_rejects_the_original_dispatch() {
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
        let mut guard = RequestGuard::reserve(
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
        drop(guard);
        let ledger = runtime.model_requests(&context.lease.session_id).unwrap();
        assert_eq!(ledger.requests[0].started_cursor, None);
        assert_eq!(ledger.reserved_tokens, 1000);
        assert_eq!(
            ledger.requests[0].terminal,
            Some(ModelRequestTerminal::Cancelled)
        );
        assert!(!context.unknown.load(Ordering::Acquire));
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
        let mut guard = RequestGuard::reserve(
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
