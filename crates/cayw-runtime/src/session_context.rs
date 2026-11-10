//! Derived context for one process.
//!
//! Maintenance reservations are not Core events. They do not take an execution
//! lease, do not enter the model-request ledger, and do not reserve the rest of
//! a session budget tree. An in-flight or unknown maintenance reservation does
//! not stop a foreground request.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cayw_ai::{Content, Message};
use cayw_context::{
    Assembly, CompressionRecord, ContextInput, Piece, SourceUnit, SummaryRecord, UnitKind,
    VersionDecision, accept_compression, accept_summary, advance_frontier, input_budget, plan,
};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::RuntimeError;

const MAINTENANCE_FILE: &str = "context-maintenance.json";

pub(crate) struct SessionContexts {
    sessions: BTreeMap<String, Derived>,
    dir: Option<PathBuf>,
    /// The maintenance file existed but could not be read completely. The
    /// foreground still runs. It is never overwritten, so unknown usage cannot
    /// silently turn into zero.
    damaged: bool,
}

#[derive(Default)]
struct Derived {
    summaries: Vec<SummaryRecord>,
    compressions: Vec<CompressionRecord>,
    ledger: MaintenanceLedger,
    epoch: u64,
    frontier: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobKind {
    Summary,
    Compression,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReservationState {
    Reserved,
    /// The maintenance call's effect is not confirmed. The reservation stays occupied.
    #[allow(dead_code)]
    Unknown,
    Settled,
}

struct ReservationSpec {
    kind: JobKind,
    start_cursor: u64,
    end_cursor: u64,
    input_bytes: u64,
    margin: u64,
    output_max: u64,
}

struct Reservation {
    id: u64,
    attempt: u64,
    kind: JobKind,
    start_cursor: u64,
    end_cursor: u64,
    /// Input bytes plus protocol margin plus this call's output bound.
    /// This is never the remaining balance of the session tree.
    tokens: u64,
    state: ReservationState,
    confirmed: Option<u64>,
    overage: bool,
}

#[derive(Default)]
struct MaintenanceLedger {
    next_id: u64,
    items: Vec<Reservation>,
}

#[derive(Deserialize)]
struct StoredSnapshot {
    #[serde(default)]
    sessions: BTreeMap<String, StoredSession>,
}

#[derive(Default, Deserialize)]
struct StoredSession {
    #[serde(default)]
    epoch: u64,
    #[serde(default)]
    frontier: u64,
    #[serde(default)]
    summaries: Vec<SummaryRecord>,
    #[serde(default)]
    compressions: Vec<CompressionRecord>,
    #[serde(default)]
    reservations: Vec<StoredReservation>,
}

#[derive(Deserialize)]
struct StoredReservation {
    id: u64,
    attempt: u64,
    kind: String,
    start_cursor: u64,
    end_cursor: u64,
    tokens: u64,
    state: String,
    #[serde(default)]
    confirmed: Option<u64>,
    #[serde(default)]
    overage: bool,
}

/// Host-confirmed usage for one maintenance reservation. The confirmed value is
/// explicit: an unknown maintenance result has no local zero default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceReconciliation {
    pub session_id: String,
    pub reservation_id: u64,
    pub attempt: u64,
    /// The provider's reported usage. A value above the reservation is overage.
    pub confirmed_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MaintenanceReconciliationRecord {
    pub session_id: String,
    pub reservation_id: u64,
    pub attempt: u64,
    pub confirmed_tokens: u64,
    pub overage: bool,
    /// This session's remaining open maintenance hold after settling.
    pub occupied_tokens: u64,
}

impl SessionContexts {
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        let (sessions, damaged) = match dir.as_deref() {
            Some(dir) => load(dir),
            None => (BTreeMap::new(), false),
        };
        Self {
            sessions,
            dir,
            damaged,
        }
    }

    fn session_mut(&mut self, session_id: &str) -> &mut Derived {
        self.sessions.entry(session_id.to_owned()).or_default()
    }

    fn reserve(&mut self, session_id: &str, spec: ReservationSpec) -> Result<u64, RuntimeError> {
        let ReservationSpec {
            kind,
            start_cursor,
            end_cursor,
            input_bytes,
            margin,
            output_max,
        } = spec;
        if output_max == 0 || start_cursor == 0 || end_cursor < start_cursor {
            return Err(RuntimeError::InvalidInput(
                "maintenance reservation needs a positive output bound and cursor range".into(),
            ));
        }
        let derived = self.session_mut(session_id);
        if kind == JobKind::Compression
            && derived.ledger.items.iter().any(|item| {
                item.kind == JobKind::Compression && item.state != ReservationState::Settled
            })
        {
            return Err(RuntimeError::InvalidInput(
                "this session already has an active compression".into(),
            ));
        }
        if derived.ledger.items.iter().any(|item| {
            item.kind == kind
                && item.start_cursor == start_cursor
                && item.end_cursor == end_cursor
                && item.state != ReservationState::Settled
        }) {
            return Err(RuntimeError::InvalidInput(
                "this maintenance range is already active".into(),
            ));
        }
        derived.ledger.next_id = derived.ledger.next_id.saturating_add(1);
        let id = derived.ledger.next_id;
        let tokens = input_bytes
            .saturating_add(margin)
            .saturating_add(output_max);
        derived.ledger.items.push(Reservation {
            id,
            attempt: 1,
            kind,
            start_cursor,
            end_cursor,
            tokens,
            state: ReservationState::Reserved,
            confirmed: None,
            overage: false,
        });
        Ok(id)
    }

    #[allow(dead_code)]
    fn mark_unknown(
        &mut self,
        session_id: &str,
        id: u64,
        attempt: u64,
    ) -> Result<(), RuntimeError> {
        let item = reservation_mut(self, session_id, id, attempt)?;
        if item.state == ReservationState::Settled {
            return Err(RuntimeError::InvalidInput(
                "settled maintenance usage cannot become unknown".into(),
            ));
        }
        item.state = ReservationState::Unknown;
        Ok(())
    }

    /// Records the reported usage and releases the reservation.
    /// A report above the reservation is overage, not a corrupted ledger.
    /// Repeating the same confirmed value is idempotent; a different value for
    /// an already settled reservation is refused.
    pub(crate) fn reconcile(
        &mut self,
        session_id: &str,
        id: u64,
        attempt: u64,
        confirmed: u64,
    ) -> Result<bool, RuntimeError> {
        let item = reservation_mut(self, session_id, id, attempt)?;
        if item.state == ReservationState::Settled {
            return if item.confirmed == Some(confirmed) {
                Ok(item.overage)
            } else {
                Err(RuntimeError::InvalidInput(
                    "maintenance usage was already reconciled with a different value".into(),
                ))
            };
        }
        let overage = confirmed > item.tokens;
        item.overage = overage;
        item.confirmed = Some(confirmed);
        item.state = ReservationState::Settled;
        Ok(overage)
    }

    #[allow(dead_code)]
    fn release_local(&mut self, session_id: &str, kind: JobKind, end_cursor: u64) {
        let Some(derived) = self.sessions.get_mut(session_id) else {
            return;
        };
        if let Some(item) = derived.ledger.items.iter_mut().find(|item| {
            item.kind == kind
                && item.end_cursor == end_cursor
                && item.state != ReservationState::Settled
        }) {
            item.confirmed = Some(0);
            item.overage = false;
            item.state = ReservationState::Settled;
        }
    }

    pub(crate) fn occupied(&self, session_id: &str) -> u64 {
        self.sessions
            .get(session_id)
            .map(|derived| {
                derived
                    .ledger
                    .items
                    .iter()
                    .filter(|item| item.state != ReservationState::Settled)
                    .map(|item| item.tokens)
                    .fold(0_u64, u64::saturating_add)
            })
            .unwrap_or(0)
    }

    #[allow(dead_code)]
    fn foreground_may_enter(&self) -> bool {
        true
    }

    fn persist(&self) {
        let Some(dir) = &self.dir else {
            return;
        };
        // A damaged file may still describe unknown usage. Do not replace it
        // with a snapshot that would report that usage as zero.
        if self.damaged {
            return;
        }
        let Ok(bytes) = serde_json::to_vec_pretty(&snapshot(self)) else {
            return;
        };
        let _ = write_private(&dir.join(MAINTENANCE_FILE), &bytes);
    }
}

impl crate::Runtime {
    /// Reconciles one maintenance reservation with the provider's reported
    /// usage. This path is independent from `reconcile_model_request`: it
    /// appends no Core model-request event, takes no model budget lock, and
    /// does not gate foreground requests.
    ///
    /// An unknown reservation stays occupied until a confirmed value is
    /// supplied. A report above the reservation records that reported value and
    /// marks overage instead of treating the ledger as corrupted. The same
    /// confirmed value may be submitted repeatedly.
    pub fn reconcile_maintenance(
        &self,
        input: MaintenanceReconciliation,
    ) -> Result<MaintenanceReconciliationRecord, RuntimeError> {
        let mut contexts = crate::lock(&self.inner.context);
        let overage = contexts.reconcile(
            &input.session_id,
            input.reservation_id,
            input.attempt,
            input.confirmed_tokens,
        )?;
        let occupied_tokens = contexts.occupied(&input.session_id);
        contexts.persist();
        Ok(MaintenanceReconciliationRecord {
            session_id: input.session_id,
            reservation_id: input.reservation_id,
            attempt: input.attempt,
            confirmed_tokens: input.confirmed_tokens,
            overage,
            occupied_tokens,
        })
    }
}

/// Reads back published summaries, compressions, and open maintenance
/// reservations. A missing file is an empty context. An unreadable or
/// malformed file is reported as damaged instead of blocking the foreground.
fn load(dir: &Path) -> (BTreeMap<String, Derived>, bool) {
    let bytes = match fs::read(dir.join(MAINTENANCE_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (BTreeMap::new(), false);
        }
        Err(_) => return (BTreeMap::new(), true),
    };
    let Ok(stored) = serde_json::from_slice::<StoredSnapshot>(&bytes) else {
        return (BTreeMap::new(), true);
    };
    let mut damaged = false;
    let sessions = stored
        .sessions
        .into_iter()
        .filter_map(|(session_id, item)| {
            if session_id.is_empty() {
                damaged = true;
                return None;
            }
            let mut ledger = MaintenanceLedger::default();
            for reservation in item.reservations {
                // An unrecognized kind or state keeps the usage occupied. It
                // is never turned into a settled zero.
                let kind = match reservation.kind.as_str() {
                    "compression" => JobKind::Compression,
                    "summary" => JobKind::Summary,
                    _ => {
                        damaged = true;
                        JobKind::Summary
                    }
                };
                let state = match reservation.state.as_str() {
                    "settled" => ReservationState::Settled,
                    "reserved" => ReservationState::Reserved,
                    "unknown" => ReservationState::Unknown,
                    _ => {
                        damaged = true;
                        ReservationState::Unknown
                    }
                };
                ledger.next_id = ledger.next_id.max(reservation.id);
                ledger.items.push(Reservation {
                    id: reservation.id,
                    attempt: reservation.attempt,
                    kind,
                    start_cursor: reservation.start_cursor,
                    end_cursor: reservation.end_cursor,
                    tokens: reservation.tokens,
                    state,
                    confirmed: reservation.confirmed,
                    overage: reservation.overage,
                });
            }
            Some((
                session_id,
                Derived {
                    summaries: item.summaries,
                    compressions: item.compressions,
                    ledger,
                    epoch: item.epoch,
                    frontier: item.frontier,
                },
            ))
        })
        .collect();
    (sessions, damaged)
}

#[allow(dead_code)]
fn reservation_mut<'a>(
    contexts: &'a mut SessionContexts,
    session_id: &str,
    id: u64,
    attempt: u64,
) -> Result<&'a mut Reservation, RuntimeError> {
    let derived = contexts
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| RuntimeError::InvalidInput("unknown maintenance session".into()))?;
    derived
        .ledger
        .items
        .iter_mut()
        .find(|item| item.id == id && item.attempt == attempt)
        .ok_or_else(|| {
            RuntimeError::InvalidInput("maintenance attempt does not match this reservation".into())
        })
}

fn snapshot(contexts: &SessionContexts) -> serde_json::Value {
    let sessions = contexts
        .sessions
        .iter()
        .map(|(session_id, derived)| {
            let reservations = derived
                .ledger
                .items
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "id": item.id,
                        "attempt": item.attempt,
                        "kind": match item.kind {
                            JobKind::Summary => "summary",
                            JobKind::Compression => "compression",
                        },
                        "start_cursor": item.start_cursor,
                        "end_cursor": item.end_cursor,
                        "tokens": item.tokens,
                        "state": match item.state {
                            ReservationState::Reserved => "reserved",
                            ReservationState::Unknown => "unknown",
                            ReservationState::Settled => "settled",
                        },
                        "confirmed": item.confirmed,
                        "overage": item.overage,
                    })
                })
                .collect::<Vec<_>>();
            (
                session_id.clone(),
                serde_json::json!({
                    "epoch": derived.epoch,
                    "frontier": derived.frontier,
                    "summaries": derived.summaries,
                    "compressions": derived.compressions,
                    "reservations": reservations,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({ "sessions": sessions })
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default();
    let temporary = parent.join(format!(".{}.tmp", name.to_string_lossy()));
    let mut file = fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    #[cfg(unix)]
    file.mode(0o600);
    let mut file = file.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)
}

#[allow(dead_code)]
pub(crate) fn publish_compression(
    context: &Mutex<SessionContexts>,
    session_id: &str,
    record: CompressionRecord,
) -> Result<VersionDecision, RuntimeError> {
    let mut contexts = crate::lock(context);
    let decision = {
        let derived = contexts.session_mut(session_id);
        accept_compression(&derived.compressions, &record)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?
    };
    if decision == VersionDecision::Accepted {
        contexts.release_local(session_id, JobKind::Compression, record.covered_through);
        contexts.session_mut(session_id).compressions.push(record);
        contexts.persist();
    }
    Ok(decision)
}

pub(crate) fn prepare_provider_input(
    context: &Mutex<SessionContexts>,
    session_id: &str,
    messages: &[Message],
    window: Option<u64>,
    output_reserve: u64,
) -> Result<Vec<Message>, RuntimeError> {
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    let budget = match input_budget(
        window,
        output_reserve,
        cayw_context::DEFAULT_PROTOCOL_MARGIN,
    ) {
        Ok(budget) => budget,
        // A missing window is the conservative estimate, not the provider's
        // real limit. If the requested output reserve already fills it, skip
        // local assembly instead of refusing a request the provider may accept.
        Err(_) if window.is_none() => return Ok(messages.to_vec()),
        Err(error) => return Err(RuntimeError::InvalidInput(error.to_string())),
    };
    let units = units_from(messages);
    let mut contexts = crate::lock(context);
    let planned = {
        let derived = contexts.session_mut(session_id);
        plan(&ContextInput {
            session_id,
            units: &units,
            summaries: &derived.summaries,
            compressions: &derived.compressions,
            budget,
            epoch: derived.epoch,
        })
    };
    let planned = match planned {
        Ok(planned) => planned,
        Err(_) if budget.window_was_default => return Ok(messages.to_vec()),
        Err(_) => {
            let raw = message_tokens(messages);
            return if raw > budget.input_limit {
                Err(RuntimeError::Budget("context input"))
            } else {
                Ok(messages.to_vec())
            };
        }
    };
    let mut changed = false;
    for summary in &planned.new_summaries {
        let accept = contexts
            .sessions
            .get(session_id)
            .map(|derived| accept_summary(&derived.summaries, summary))
            .transpose()
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        if accept == Some(VersionDecision::Accepted) {
            contexts
                .session_mut(session_id)
                .summaries
                .push(summary.clone());
            changed = true;
        }
    }
    {
        let derived = contexts.session_mut(session_id);
        let ready = derived
            .summaries
            .iter()
            .map(|summary| (summary.start_cursor, summary.end_cursor))
            .collect::<Vec<_>>();
        if let Ok(frontier) = advance_frontier(0, &ready) {
            derived.frontier = frontier;
        }
    }
    if let Some(job) = &planned.compression_job
        && contexts
            .reserve(
                session_id,
                ReservationSpec {
                    kind: JobKind::Compression,
                    start_cursor: job.start_cursor,
                    end_cursor: job.covered_through,
                    input_bytes: job.input_bytes,
                    margin: budget.margin,
                    output_max: budget.output_reserve,
                },
            )
            .is_ok()
    {
        changed = true;
    }
    for pending in &planned.pending_summaries {
        if contexts
            .reserve(
                session_id,
                ReservationSpec {
                    kind: JobKind::Summary,
                    start_cursor: pending.start_cursor,
                    end_cursor: pending.end_cursor,
                    input_bytes: pending.input_bytes,
                    margin: budget.margin,
                    output_max: budget.output_reserve,
                },
            )
            .is_ok()
        {
            changed = true;
        }
    }
    {
        let derived = contexts.session_mut(session_id);
        derived.epoch = derived.epoch.saturating_add(1);
    }
    if changed {
        contexts.persist();
    }
    let replacement = planned.assembly.as_ref().and_then(|assembly| {
        if apply_assembly(assembly, budget.window_was_default) {
            render(messages, assembly).ok()
        } else {
            None
        }
    });
    if let Some(rendered) = replacement
        && message_tokens(&rendered) <= budget.input_limit
    {
        return Ok(rendered);
    }
    if !budget.window_was_default && planned.input_tokens > budget.input_limit {
        return Err(RuntimeError::Budget("context input"));
    }
    Ok(messages.to_vec())
}

pub(crate) fn units_from(messages: &[Message]) -> Vec<SourceUnit> {
    let mut units = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let start = index as u64 + 1;
        match &messages[index] {
            Message::System(_) | Message::Custom { .. } => {
                units.push(unit(
                    start,
                    start,
                    UnitKind::Fixed,
                    messages[index].text(),
                    false,
                    false,
                ));
                index += 1;
            }
            Message::User { .. } => {
                if let Some(Message::Assistant(assistant)) = messages.get(index + 1)
                    && assistant.tool_calls().is_empty()
                {
                    units.push(unit(
                        start,
                        start + 1,
                        UnitKind::CompletedTurn,
                        joined(&messages[index..=index + 1]),
                        false,
                        false,
                    ));
                    index += 2;
                } else if index + 1 == messages.len() {
                    units.push(unit(
                        start,
                        start,
                        UnitKind::Active,
                        messages[index].text(),
                        false,
                        true,
                    ));
                    index += 1;
                } else {
                    units.push(unit(
                        start,
                        start,
                        UnitKind::CompletedTurn,
                        messages[index].text(),
                        false,
                        false,
                    ));
                    index += 1;
                }
            }
            Message::Assistant(assistant) => {
                let calls = assistant.tool_calls();
                if calls.is_empty() {
                    let kind = if index + 1 == messages.len() {
                        UnitKind::Active
                    } else {
                        UnitKind::CompletedTurn
                    };
                    units.push(unit(
                        start,
                        start,
                        kind,
                        messages[index].text(),
                        false,
                        false,
                    ));
                    index += 1;
                    continue;
                }
                let mut next = index + 1;
                let mut matched = 0;
                let mut unknown = false;
                while matched < calls.len() {
                    let Some(Message::ToolResult(result)) = messages.get(next) else {
                        break;
                    };
                    if result.tool_call_id != calls[matched].id
                        || result.tool_name != calls[matched].name
                    {
                        break;
                    }
                    unknown |= result.outcome_unknown;
                    matched += 1;
                    next += 1;
                }
                let complete = matched == calls.len();
                units.push(unit(
                    start,
                    next as u64,
                    if complete {
                        UnitKind::ToolExchange
                    } else {
                        UnitKind::Active
                    },
                    joined(&messages[index..next]),
                    unknown,
                    unknown || !complete,
                ));
                index = next;
            }
            Message::ToolResult(result) => {
                units.push(unit(
                    start,
                    start,
                    UnitKind::Active,
                    messages[index].text(),
                    result.outcome_unknown,
                    true,
                ));
                index += 1;
            }
        }
    }
    if let Some(last) = units.last_mut() {
        last.pin_original = true;
    }
    units
}

fn unit(
    start: u64,
    end: u64,
    kind: UnitKind,
    text: String,
    outcome_unknown: bool,
    pin_original: bool,
) -> SourceUnit {
    SourceUnit {
        start_cursor: start,
        end_cursor: end,
        kind,
        text,
        outcome_unknown,
        pin_original,
    }
}

fn joined(messages: &[Message]) -> String {
    messages
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply_assembly(assembly: &Assembly, window_was_default: bool) -> bool {
    if !assembly.replaces_history || !assembly.sendable || assembly.model_calls != 0 {
        return false;
    }
    if !window_was_default {
        return true;
    }
    // A missing window schedules work. It does not excerpt or reject a prompt
    // that can already be sent. A published compression or a shorter summary may.
    assembly
        .pieces
        .iter()
        .any(|piece| matches!(piece, Piece::Compression { .. } | Piece::Summary { .. }))
}

fn render(messages: &[Message], assembly: &Assembly) -> Result<Vec<Message>, ()> {
    let mut rendered = Vec::new();
    let mut expect = 1_u64;
    for piece in &assembly.pieces {
        let (start, end, text, _) = piece.range();
        if start != expect || end < start || end as usize > messages.len() {
            return Err(());
        }
        let from = (start - 1) as usize;
        let to = end as usize;
        match piece {
            Piece::Fixed { .. } | Piece::Original { .. } => {
                let slice = &messages[from..to];
                let source_len: usize = slice.iter().map(|message| message.text().len()).sum();
                if text.len() < source_len {
                    rendered.extend(slice.iter().map(|message| excerpt_message(message, text)));
                } else {
                    rendered.extend(slice.iter().cloned());
                }
            }
            Piece::Compression { .. } | Piece::Summary { .. } => {
                rendered.push(Message::system(text.to_owned()));
            }
        }
        expect = end.saturating_add(1);
    }
    if expect != messages.len() as u64 + 1 {
        Err(())
    } else {
        Ok(rendered)
    }
}

fn excerpt_message(message: &Message, excerpt: &str) -> Message {
    match message {
        Message::ToolResult(result) => {
            let mut copy = result.clone();
            copy.content = vec![Content::text(excerpt)];
            Message::ToolResult(copy)
        }
        other => other.clone(),
    }
}

fn message_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|message| u64::try_from(message.text().len()).unwrap_or(u64::MAX))
        .fold(0_u64, u64::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_ai::{AssistantMessage, Model, ToolCall, ToolDefinition, ToolResultMessage};
    use std::os::unix::fs::PermissionsExt;

    fn contexts() -> SessionContexts {
        SessionContexts::new(None)
    }

    fn spec(
        kind: JobKind,
        start_cursor: u64,
        end_cursor: u64,
        input_bytes: u64,
        margin: u64,
        output_max: u64,
    ) -> ReservationSpec {
        ReservationSpec {
            kind,
            start_cursor,
            end_cursor,
            input_bytes,
            margin,
            output_max,
        }
    }

    fn test_runtime(dir: &Path) -> crate::Runtime {
        struct NoopProvider;
        impl cayw_ai::Provider for NoopProvider {
            fn stream(
                &self,
                _: cayw_ai::ModelRequest,
                _: cayw_ai::CancellationToken,
            ) -> cayw_ai::AssistantStream {
                Box::pin(futures::stream::empty())
            }
        }
        let workspace = dir.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        crate::Runtime::open(
            dir.join("session.db"),
            workspace,
            std::sync::Arc::new(NoopProvider),
            Model::new("fixture", "test"),
            crate::RuntimeConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn maintenance_reconcile_keeps_unknown_occupied_and_records_overage_idempotently() {
        let mut contexts = contexts();
        let id = contexts
            .reserve("s", spec(JobKind::Summary, 1, 4, 10, 3, 2))
            .unwrap();
        contexts.mark_unknown("s", id, 1).unwrap();
        // Unknown usage is still occupied; it is never treated as zero.
        assert_eq!(contexts.occupied("s"), 15);
        assert!(contexts.foreground_may_enter());
        // A wrong attempt cannot settle somebody else's reservation.
        assert!(contexts.reconcile("s", id, 2, 20).is_err());
        assert_eq!(contexts.occupied("s"), 15);
        // A report above the reservation records the reported value and
        // releases the hold.
        assert!(contexts.reconcile("s", id, 1, 20).unwrap());
        assert_eq!(contexts.occupied("s"), 0);
        let settled = contexts.sessions["s"]
            .ledger
            .items
            .iter()
            .find(|item| item.id == id)
            .unwrap();
        assert_eq!(settled.confirmed, Some(20));
        assert!(settled.overage);
        // Repeating the same confirmed value is idempotent.
        assert!(contexts.reconcile("s", id, 1, 20).unwrap());
        assert_eq!(contexts.occupied("s"), 0);
        // A different value is refused.
        assert!(contexts.reconcile("s", id, 1, 21).is_err());
    }

    #[test]
    fn runtime_reconcile_maintenance_does_not_take_the_budget_lock() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let session = runtime.create(None).unwrap().id;
        let id = {
            let mut contexts = crate::lock(&runtime.inner.context);
            let id = contexts
                .reserve(&session, spec(JobKind::Summary, 1, 2, 10, 2, 5))
                .unwrap();
            contexts.mark_unknown(&session, id, 1).unwrap();
            contexts.persist();
            id
        };
        // Hold this session's budget lock. Reconciliation must not wait for it.
        let budget = runtime.inner.budget_lock(&session);
        let _held = budget.try_lock_owned().unwrap();
        // Unknown maintenance usage does not block a foreground request.
        let messages = vec![Message::user("still available")];
        let prepared =
            prepare_provider_input(&runtime.inner.context, &session, &messages, None, 1_024)
                .unwrap();
        assert_eq!(prepared, messages);
        let input = MaintenanceReconciliation {
            session_id: session.clone(),
            reservation_id: id,
            attempt: 1,
            confirmed_tokens: 30,
        };
        let record = runtime.reconcile_maintenance(input.clone()).unwrap();
        assert!(record.overage);
        assert_eq!(record.confirmed_tokens, 30);
        assert_eq!(record.occupied_tokens, 0);
        assert_eq!(runtime.reconcile_maintenance(input).unwrap(), record);
        assert!(
            runtime
                .reconcile_maintenance(MaintenanceReconciliation {
                    session_id: session,
                    reservation_id: id,
                    attempt: 2,
                    confirmed_tokens: 30,
                })
                .is_err(),
            "a wrong attempt must fail"
        );
    }

    #[tokio::test]
    async fn rpc_reconcile_maintenance_writes_no_core_event_and_is_idempotent() {
        use cayw_ipc::RpcHandler;

        let temp = tempfile::tempdir().unwrap();
        let runtime = test_runtime(temp.path());
        let session = runtime.create(None).unwrap().id;
        let cursor = runtime.store().snapshot(&session).unwrap().cursor;
        let ledger = runtime.model_requests(&session).unwrap();
        let id = {
            let mut contexts = crate::lock(&runtime.inner.context);
            let id = contexts
                .reserve(&session, spec(JobKind::Compression, 1, 3, 10, 2, 4))
                .unwrap();
            contexts.mark_unknown(&session, id, 1).unwrap();
            id
        };
        let params = serde_json::json!({
            "session_id": session,
            "reservation_id": id,
            "attempt": 1,
            "confirmed_tokens": 40,
        });
        let value = runtime
            .call(
                "session.maintenance.reconcile",
                params.clone(),
                cayw_ai::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(value["overage"], true);
        assert_eq!(value["confirmed_tokens"], 40);
        assert_eq!(value["occupied_tokens"], 0);
        // No Core event was written at all, and the model-request ledger is
        // unchanged.
        assert_eq!(runtime.store().snapshot(&session).unwrap().cursor, cursor);
        assert_eq!(runtime.model_requests(&session).unwrap(), ledger);
        let repeated = runtime
            .call(
                "session.maintenance.reconcile",
                params,
                cayw_ai::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(repeated, value);
        let wrong_attempt = runtime
            .call(
                "session.maintenance.reconcile",
                serde_json::json!({
                    "session_id": session,
                    "reservation_id": id,
                    "attempt": 2,
                    "confirmed_tokens": 40,
                }),
                cayw_ai::CancellationToken::new(),
            )
            .await;
        assert!(wrong_attempt.is_err());
    }

    #[test]
    fn reservation_is_the_call_bound_and_unknown_does_not_block_or_become_zero() {
        let mut contexts = contexts();
        let id = contexts
            .reserve("s", spec(JobKind::Compression, 1, 4, 10, 3, 2))
            .unwrap();
        assert_eq!(contexts.occupied("s"), 15);
        assert!(contexts.foreground_may_enter());
        assert!(
            contexts
                .reserve("s", spec(JobKind::Compression, 5, 8, 10, 3, 2))
                .is_err(),
            "one session keeps a single active compression"
        );
        contexts.mark_unknown("s", id, 1).unwrap();
        assert_eq!(contexts.occupied("s"), 15);
        assert!(contexts.foreground_may_enter());
        contexts.reconcile("s", id, 1, 20).unwrap();
        assert_eq!(contexts.occupied("s"), 0);
        let settled = contexts.sessions["s"]
            .ledger
            .items
            .iter()
            .find(|item| item.id == id)
            .unwrap();
        assert!(settled.overage);
        assert_eq!(settled.confirmed, Some(20));
        assert!(contexts.reconcile("s", id, 2, 20).is_err());
        contexts
            .reserve("s", spec(JobKind::Compression, 5, 8, 4, 1, 1))
            .unwrap();
    }

    #[test]
    fn below_replacement_the_provider_input_stays_original() {
        let context = Mutex::new(contexts());
        let messages = vec![Message::user("hello")];
        let before = messages.clone();
        let prepared = prepare_provider_input(&context, "s", &messages, None, 1_024).unwrap();
        assert_eq!(prepared, before);
        assert_eq!(messages, before);
        assert_eq!(context.lock().unwrap().occupied("s"), 0);
    }

    #[test]
    fn missing_window_does_not_reject_an_already_sendable_prompt() {
        let context = Mutex::new(contexts());
        let messages = vec![Message::user("z".repeat(20_000))];
        let before = messages.clone();
        let prepared = prepare_provider_input(&context, "s", &messages, None, 100).unwrap();
        assert_eq!(prepared, before);
        assert_eq!(messages, before);
    }

    #[test]
    fn missing_window_does_not_excerpt_a_saved_tool_body() {
        let messages = saved_tool_body();
        let before = messages.clone();
        let context = Mutex::new(contexts());
        let prepared = prepare_provider_input(&context, "s", &messages, None, 100).unwrap();
        assert_eq!(prepared, before);
        assert_eq!(messages, before);
    }

    #[test]
    fn set_window_excerpts_a_saved_tool_body_without_deleting_history() {
        let messages = saved_tool_body();
        let before = messages.clone();
        let context = Mutex::new(contexts());
        let prepared = prepare_provider_input(&context, "s", &messages, Some(8_192), 100).unwrap();
        assert_eq!(messages, before);
        let excerpt = prepared
            .iter()
            .find(|message| {
                matches!(message, Message::ToolResult(result)
                    if result.tool_call_id == "call-1" && result.tool_name == "read")
            })
            .expect("tool result stays paired");
        assert!(excerpt.text().contains("original history was not deleted"));
        assert!(!excerpt.text().contains(&"B".repeat(9_000)));
        assert!(
            messages
                .iter()
                .any(|message| message.text().contains(&"B".repeat(9_000)))
        );
    }

    fn saved_tool_body() -> Vec<Message> {
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        assistant.content.push(Content::ToolCall(ToolCall::new(
            "call-1",
            "read",
            serde_json::json!({}),
        )));
        let result = ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            content: vec![Content::text("B".repeat(9_000))],
            details: None,
            structured_content: None,
            usage: None,
            is_error: false,
            outcome_unknown: false,
            timestamp: 1,
            duration_ms: Some(1),
        };
        vec![
            Message::Assistant(assistant),
            Message::ToolResult(result),
            Message::user("now"),
        ]
    }

    #[test]
    fn set_window_refuses_an_oversized_prompt_without_deleting_history() {
        let context = Mutex::new(contexts());
        let messages = vec![Message::user("z".repeat(20_000))];
        let before = messages.clone();
        let error = prepare_provider_input(&context, "s", &messages, Some(2_000), 100).unwrap_err();
        assert!(matches!(error, RuntimeError::Budget("context input")));
        assert_eq!(messages, before);
    }

    #[test]
    fn published_compression_replaces_only_the_covered_prefix() {
        let messages = early_tool_history();
        let before = messages.clone();
        let context = Mutex::new(contexts());
        publish_compression(
            &context,
            "s",
            CompressionRecord {
                compression_id: "c1".into(),
                version: 1,
                session_id: "s".into(),
                covered_through: 3,
                text: "compressed early".into(),
                evidence: vec!["s:2".into()],
            },
        )
        .unwrap();
        let prepared = prepare_provider_input(&context, "s", &messages, Some(1_325), 1).unwrap();
        assert_eq!(messages, before);
        assert_eq!(prepared[0], before[0]);
        assert!(
            prepared
                .iter()
                .any(|message| message.text() == "compressed early")
        );
        assert!(
            !prepared
                .iter()
                .any(|message| message.text().contains(&"E".repeat(200)))
        );
        assert_eq!(prepared.last().unwrap().text(), "CURRENT");
        assert!(prepared.iter().any(|message| {
            matches!(message, Message::ToolResult(result)
                if result.tool_call_id == "call-1"
                    && result.tool_name == "read"
                    && !result.outcome_unknown
                    && result.content == vec![Content::text("file body")])
        }));
    }

    #[test]
    fn restored_compression_replaces_only_the_covered_prefix() {
        let dir = tempfile::tempdir().unwrap();
        publish_compression(
            &Mutex::new(SessionContexts::new(Some(dir.path().to_path_buf()))),
            "s",
            CompressionRecord {
                compression_id: "c1".into(),
                version: 1,
                session_id: "s".into(),
                covered_through: 3,
                text: "compressed early".into(),
                evidence: vec!["s:2".into()],
            },
        )
        .unwrap();
        let messages = early_tool_history();
        let before = messages.clone();
        let restored = Mutex::new(SessionContexts::new(Some(dir.path().to_path_buf())));
        let prepared = prepare_provider_input(&restored, "s", &messages, Some(1_325), 1).unwrap();
        assert_eq!(messages, before);
        assert!(
            prepared
                .iter()
                .any(|message| message.text() == "compressed early")
        );
        assert!(
            !prepared
                .iter()
                .any(|message| message.text().contains(&"E".repeat(200)))
        );
        assert_eq!(prepared.last().unwrap().text(), "CURRENT");
        assert!(prepared.iter().any(|message| {
            matches!(message, Message::ToolResult(result)
                if result.tool_call_id == "call-1"
                    && result.tool_name == "read"
                    && !result.outcome_unknown
                    && result.content == vec![Content::text("file body")])
        }));
    }

    fn early_tool_history() -> Vec<Message> {
        let early = "E".repeat(200);
        let mut system = Message::system("rules");
        if let Message::System(system) = &mut system {
            system.tools_added.push(ToolDefinition {
                name: "read".into(),
                description: "read a file".into(),
                parameters: serde_json::json!({}),
            });
        }
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        assistant
            .content
            .push(Content::text("finished the early turn"));
        let mut tool_assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        tool_assistant.content.push(Content::ToolCall(ToolCall::new(
            "call-1",
            "read",
            serde_json::json!({}),
        )));
        let tool_result = Message::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            content: vec![Content::text("file body")],
            details: None,
            structured_content: None,
            usage: None,
            is_error: false,
            outcome_unknown: false,
            timestamp: 1,
            duration_ms: Some(1),
        });
        vec![
            system,
            Message::user(early),
            Message::Assistant(assistant),
            Message::Assistant(tool_assistant),
            tool_result,
            Message::user("CURRENT"),
        ]
    }

    #[test]
    fn unknown_tool_result_stays_paired() {
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        assistant.content.push(Content::ToolCall(ToolCall::new(
            "call-9",
            "read",
            serde_json::json!({}),
        )));
        let result = ToolResultMessage {
            tool_call_id: "call-9".into(),
            tool_name: "read".into(),
            content: vec![Content::text("maybe written")],
            details: None,
            structured_content: None,
            usage: None,
            is_error: false,
            outcome_unknown: true,
            timestamp: 1,
            duration_ms: None,
        };
        let messages = vec![
            Message::user("g".repeat(500)),
            Message::Assistant(assistant),
            Message::ToolResult(result),
        ];
        let before = messages.clone();
        let context = Mutex::new(contexts());
        publish_compression(
            &context,
            "s",
            CompressionRecord {
                compression_id: "c".into(),
                version: 1,
                session_id: "s".into(),
                covered_through: 3,
                text: "should not swallow unknown".into(),
                evidence: vec![],
            },
        )
        .unwrap();
        let prepared = prepare_provider_input(&context, "s", &messages, Some(1_600), 1).unwrap();
        assert_eq!(messages, before);
        match prepared.last().unwrap() {
            Message::ToolResult(result) => {
                assert!(result.outcome_unknown);
                assert_eq!(result.tool_call_id, "call-9");
                assert_eq!(result.tool_name, "read");
            }
            other => panic!("unknown tool result was rewritten: {other:?}"),
        }
    }

    #[test]
    fn maintenance_record_is_private_and_outside_the_model_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let context = Mutex::new(SessionContexts::new(Some(dir.path().to_path_buf())));
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        assistant.content.push(Content::text("done"));
        let messages = vec![
            Message::user("remember this turn"),
            Message::Assistant(assistant),
        ];
        prepare_provider_input(&context, "session", &messages, None, 1_024).unwrap();
        let path = dir.path().join(MAINTENANCE_FILE);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let stored = fs::read_to_string(&path).unwrap();
        assert!(stored.contains("remember this turn"));
        assert!(!stored.contains("model_request"));
    }

    #[test]
    fn new_context_reads_back_summaries_compressions_and_open_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let context = Mutex::new(SessionContexts::new(Some(path.clone())));
        publish_compression(
            &context,
            "s",
            CompressionRecord {
                compression_id: "c1".into(),
                version: 1,
                session_id: "s".into(),
                covered_through: 2,
                text: "condensed early".into(),
                evidence: vec!["s:1".into()],
            },
        )
        .unwrap();
        {
            let mut contexts = context.lock().unwrap();
            let id = contexts
                .reserve("s", spec(JobKind::Summary, 3, 4, 10, 2, 5))
                .unwrap();
            contexts.mark_unknown("s", id, 1).unwrap();
        }
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        assistant.content.push(Content::text("done"));
        let messages = vec![
            Message::user("short turn"),
            Message::Assistant(assistant),
            Message::user("current"),
        ];
        prepare_provider_input(&context, "s", &messages, None, 1_024).unwrap();
        drop(context);

        let restored = SessionContexts::new(Some(path));
        let derived = &restored.sessions["s"];
        assert_eq!(derived.summaries.len(), 1);
        assert!(derived.summaries[0].text.starts_with("short turn"));
        assert_eq!(derived.frontier, 2);
        assert_eq!(derived.compressions.len(), 1);
        assert_eq!(derived.compressions[0].text, "condensed early");
        // 10 input bytes + 2 margin + 5 output bound. Unknown stays occupied.
        assert_eq!(restored.occupied("s"), 17);
        assert_eq!(derived.ledger.items.len(), 1);
        assert_eq!(derived.ledger.items[0].state, ReservationState::Unknown);
        assert_eq!(derived.ledger.items[0].confirmed, None);
        assert!(restored.foreground_may_enter());
    }

    #[test]
    fn damaged_maintenance_file_does_not_block_or_get_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MAINTENANCE_FILE);
        let raw = b"{ this is not a maintenance record".to_vec();
        fs::write(&path, &raw).unwrap();
        let context = Mutex::new(SessionContexts::new(Some(dir.path().to_path_buf())));

        let mut assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        assistant.content.push(Content::text("done"));
        let messages = vec![Message::user("hello"), Message::Assistant(assistant)];
        let before = messages.clone();
        let prepared = prepare_provider_input(&context, "s", &messages, None, 1_024).unwrap();

        assert_eq!(prepared, before);
        assert_eq!(fs::read(&path).unwrap(), raw);
    }
}
