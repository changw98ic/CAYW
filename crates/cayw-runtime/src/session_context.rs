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
const SNAPSHOT_VERSION: u32 = 2;

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
    failures: u64,
    last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobKind {
    Summary,
    Compression,
    MemoryWriter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReservationState {
    Reserved,
    /// The maintenance call's effect is not confirmed. The reservation stays occupied.
    Unknown,
    Settled,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReservationSpec {
    pub kind: JobKind,
    pub start_cursor: u64,
    pub end_cursor: u64,
    pub input_bytes: u64,
    pub margin: u64,
    pub output_max: u64,
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
    version: u32,
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
    failures: u64,
    #[serde(default)]
    last_error: Option<String>,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextMaintenanceStatus {
    pub session_id: String,
    pub damaged: bool,
    pub occupied_tokens: u64,
    pub summaries: usize,
    pub compressions: usize,
    pub failures: u64,
    pub last_error: Option<String>,
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

    pub(crate) fn summaries(&self, session_id: &str) -> Vec<SummaryRecord> {
        self.sessions
            .get(session_id)
            .map(|derived| derived.summaries.clone())
            .unwrap_or_default()
    }

    pub(crate) fn compressions(&self, session_id: &str) -> Vec<CompressionRecord> {
        self.sessions
            .get(session_id)
            .map(|derived| derived.compressions.clone())
            .unwrap_or_default()
    }

    pub(crate) fn reserve(
        &mut self,
        session_id: &str,
        spec: ReservationSpec,
    ) -> Result<u64, RuntimeError> {
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
                item.kind == JobKind::Compression && item.state == ReservationState::Reserved
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
                && item.state == ReservationState::Reserved
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

    pub(crate) fn mark_unknown(
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

    pub(crate) fn rollback_reservation(&mut self, session_id: &str, id: u64) {
        if let Some(derived) = self.sessions.get_mut(session_id) {
            derived.ledger.items.retain(|item| item.id != id);
        }
    }

    pub(crate) fn record_error(&mut self, session_id: &str, reason: impl Into<String>) {
        let derived = self.session_mut(session_id);
        derived.failures = derived.failures.saturating_add(1);
        derived.last_error = Some(reason.into());
        let _ = self.persist();
    }

    pub(crate) fn status(&self, session_id: &str) -> ContextMaintenanceStatus {
        let occupied_tokens = self.occupied(session_id);
        let damaged_error = self.damaged.then(|| {
            format!(
                "context maintenance file ({MAINTENANCE_FILE}) is damaged and cannot be overwritten"
            )
        });
        match self.sessions.get(session_id) {
            Some(derived) => ContextMaintenanceStatus {
                session_id: session_id.to_owned(),
                damaged: self.damaged,
                occupied_tokens,
                summaries: derived.summaries.len(),
                compressions: derived.compressions.len(),
                failures: derived.failures,
                last_error: derived.last_error.clone().or(damaged_error),
            },
            None => ContextMaintenanceStatus {
                session_id: session_id.to_owned(),
                damaged: self.damaged,
                occupied_tokens,
                summaries: 0,
                compressions: 0,
                failures: 0,
                last_error: damaged_error,
            },
        }
    }

    pub(crate) fn persist(&self) -> Result<(), RuntimeError> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        // A damaged file may still describe unknown usage. Do not replace it
        // with a snapshot that would report that usage as zero.
        if self.damaged {
            return Err(RuntimeError::Workspace(format!(
                "context maintenance file ({MAINTENANCE_FILE}) is damaged and cannot be overwritten"
            )));
        }
        let bytes = serde_json::to_vec_pretty(&snapshot(self)).map_err(|error| {
            RuntimeError::Workspace(format!("cannot serialize context maintenance: {error}"))
        })?;
        write_private(&dir.join(MAINTENANCE_FILE), &bytes).map_err(|error| {
            RuntimeError::Workspace(format!("cannot persist context maintenance: {error}"))
        })
    }
}

impl crate::Runtime {
    /// Read derived context maintenance counts and recorded background failure state
    /// for one session without mutating history.
    pub fn context_maintenance_status(
        &self,
        session_id: &str,
    ) -> Result<ContextMaintenanceStatus, RuntimeError> {
        let _ = self.get(session_id)?;
        Ok(crate::lock(&self.inner.context).status(session_id))
    }

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
        let prev = {
            let item = reservation_mut(
                &mut contexts,
                &input.session_id,
                input.reservation_id,
                input.attempt,
            )?;
            (item.state, item.confirmed, item.overage)
        };
        let overage = contexts.reconcile(
            &input.session_id,
            input.reservation_id,
            input.attempt,
            input.confirmed_tokens,
        )?;
        if let Err(error) = contexts.persist() {
            if let Ok(item) = reservation_mut(
                &mut contexts,
                &input.session_id,
                input.reservation_id,
                input.attempt,
            ) {
                item.state = prev.0;
                item.confirmed = prev.1;
                item.overage = prev.2;
            }
            let derived = contexts.session_mut(&input.session_id);
            derived.failures = derived.failures.saturating_add(1);
            derived.last_error = Some(error.to_string());
            return Err(error);
        }
        let occupied_tokens = contexts.occupied(&input.session_id);
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
    let keep_derived = stored.version >= SNAPSHOT_VERSION;
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
                // is never turned into a settled zero. Any crash-abandoned
                // "reserved" entry from an earlier process becomes Unknown so
                // its usage stays occupied without freezing future maintenance.
                let kind = match reservation.kind.as_str() {
                    "compression" => JobKind::Compression,
                    "summary" => JobKind::Summary,
                    "memory_writer" => JobKind::MemoryWriter,
                    _ => {
                        damaged = true;
                        JobKind::Summary
                    }
                };
                let state = match reservation.state.as_str() {
                    "settled" => ReservationState::Settled,
                    "reserved" | "unknown" => ReservationState::Unknown,
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
                    summaries: if keep_derived {
                        item.summaries
                    } else {
                        Vec::new()
                    },
                    compressions: if keep_derived {
                        item.compressions
                    } else {
                        Vec::new()
                    },
                    ledger,
                    epoch: item.epoch,
                    frontier: if keep_derived { item.frontier } else { 0 },
                    failures: item.failures,
                    last_error: item.last_error,
                },
            ))
        })
        .collect();
    (sessions, damaged)
}

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
                            JobKind::MemoryWriter => "memory_writer",
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
                    "failures": derived.failures,
                    "last_error": derived.last_error,
                    "summaries": derived.summaries,
                    "compressions": derived.compressions,
                    "reservations": reservations,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "version": SNAPSHOT_VERSION,
        "sessions": sessions,
    })
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

pub(crate) fn effective_input_budget(
    window: Option<u64>,
    output_reserve: u64,
) -> Result<cayw_context::InputBudget, RuntimeError> {
    let margin = cayw_context::DEFAULT_PROTOCOL_MARGIN;
    match input_budget(window, output_reserve, margin) {
        Ok(budget) => Ok(budget),
        Err(_) if window.is_none() && output_reserve > 0 => {
            let fallback_window = output_reserve
                .saturating_add(margin)
                .saturating_add(cayw_context::CONSERVATIVE_CONTEXT_WINDOW);
            let mut budget = input_budget(Some(fallback_window), output_reserve, margin)
                .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
            budget.window_was_default = true;
            Ok(budget)
        }
        Err(error) => Err(RuntimeError::InvalidInput(error.to_string())),
    }
}

pub(crate) fn publish_summary(
    context: &Mutex<SessionContexts>,
    session_id: &str,
    record: SummaryRecord,
    units: Option<&[SourceUnit]>,
) -> Result<VersionDecision, RuntimeError> {
    let mut contexts = crate::lock(context);
    let decision = {
        let derived = contexts.session_mut(session_id);
        accept_summary(&derived.summaries, &record)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?
    };
    if decision == VersionDecision::Accepted {
        let prev_frontier = {
            let derived = contexts.session_mut(session_id);
            let prev_frontier = derived.frontier;
            derived.summaries.push(record);
            if let Some(units) = units {
                if let Some(frontier) = compute_frontier(units, &derived.summaries) {
                    derived.frontier = frontier;
                }
            } else if let Ok(frontier) = advance_frontier(
                derived.frontier,
                &derived
                    .summaries
                    .iter()
                    .map(|summary| (summary.start_cursor, summary.end_cursor))
                    .collect::<Vec<_>>(),
            ) {
                derived.frontier = frontier;
            }
            prev_frontier
        };
        if let Err(error) = contexts.persist() {
            let derived = contexts.session_mut(session_id);
            derived.summaries.pop();
            derived.frontier = prev_frontier;
            derived.failures = derived.failures.saturating_add(1);
            derived.last_error = Some(error.to_string());
            return Err(error);
        }
    }
    Ok(decision)
}

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
        contexts.session_mut(session_id).compressions.push(record);
        if let Err(error) = contexts.persist() {
            let derived = contexts.session_mut(session_id);
            derived.compressions.pop();
            derived.failures = derived.failures.saturating_add(1);
            derived.last_error = Some(error.to_string());
            return Err(error);
        }
    }
    Ok(decision)
}

pub(crate) struct MaintenanceInspection {
    pub budget: cayw_context::InputBudget,
    pub input_tokens: u64,
    pub units: Vec<SourceUnit>,
    pub pending_summaries: Vec<cayw_context::PendingSummary>,
    pub compression_job: Option<cayw_context::CompressionJob>,
    pub summaries: Vec<SummaryRecord>,
    pub compressions: Vec<CompressionRecord>,
}

pub(crate) fn inspect_maintenance(
    context: &Mutex<SessionContexts>,
    session_id: &str,
    messages: &[Message],
    ranges: &[cayw_context::CursorRange],
    window: Option<u64>,
    output_reserve: u64,
) -> Result<Option<MaintenanceInspection>, RuntimeError> {
    if messages.is_empty() {
        return Ok(None);
    }
    let budget = effective_input_budget(window, output_reserve)?;
    let spans = units_from_ranges(messages, ranges);
    let units = spans
        .iter()
        .map(|span| span.unit.clone())
        .collect::<Vec<_>>();
    let mut contexts = crate::lock(context);
    let (planned, compressions, prev_len, prev_frontier) = {
        let derived = contexts.session_mut(session_id);
        let planned = plan(&ContextInput {
            session_id,
            units: &units,
            summaries: &derived.summaries,
            compressions: &derived.compressions,
            budget,
            epoch: derived.epoch,
        })
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        (
            planned,
            derived.compressions.clone(),
            derived.summaries.len(),
            derived.frontier,
        )
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
    if changed {
        {
            let derived = contexts.session_mut(session_id);
            if let Some(frontier) = compute_frontier(&units, &derived.summaries) {
                derived.frontier = frontier;
            }
        }
        if let Err(error) = contexts.persist() {
            let derived = contexts.session_mut(session_id);
            derived.summaries.truncate(prev_len);
            derived.frontier = prev_frontier;
            derived.failures = derived.failures.saturating_add(1);
            derived.last_error = Some(error.to_string());
        }
    }
    let summaries = contexts.summaries(session_id);
    Ok(Some(MaintenanceInspection {
        budget,
        input_tokens: planned
            .assembly
            .as_ref()
            .map(|assembly| assembly.input_tokens)
            .unwrap_or(planned.input_tokens),
        units,
        pending_summaries: planned.pending_summaries,
        compression_job: planned.compression_job,
        summaries,
        compressions,
    }))
}

#[cfg(test)]
pub(crate) fn prepare_provider_input(
    context: &Mutex<SessionContexts>,
    session_id: &str,
    messages: &[Message],
    window: Option<u64>,
    output_reserve: u64,
) -> Result<Vec<Message>, RuntimeError> {
    prepare_provider_input_with_ranges(context, session_id, messages, &[], window, output_reserve)
}

pub(crate) fn prepare_provider_input_with_ranges(
    context: &Mutex<SessionContexts>,
    session_id: &str,
    messages: &[Message],
    ranges: &[cayw_context::CursorRange],
    window: Option<u64>,
    output_reserve: u64,
) -> Result<Vec<Message>, RuntimeError> {
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    let budget = match effective_input_budget(window, output_reserve) {
        Ok(budget) => budget,
        // A missing window is the conservative estimate, not the provider's
        // real limit. If an extreme output reserve exceeds the planner ceiling,
        // skip local assembly instead of refusing a request the provider may accept.
        Err(_) if window.is_none() && output_reserve > 0 => return Ok(messages.to_vec()),
        Err(error) => return Err(error),
    };
    let spans = units_from_ranges(messages, ranges);
    let units = spans
        .iter()
        .map(|span| span.unit.clone())
        .collect::<Vec<_>>();
    let mut contexts = crate::lock(context);
    let (planned, prev_len, prev_frontier) = {
        let derived = contexts.session_mut(session_id);
        (
            plan(&ContextInput {
                session_id,
                units: &units,
                summaries: &derived.summaries,
                compressions: &derived.compressions,
                budget,
                epoch: derived.epoch,
            }),
            derived.summaries.len(),
            derived.frontier,
        )
    };
    let planned = match planned {
        Ok(planned) => planned,
        Err(error) if budget.window_was_default => {
            contexts.record_error(session_id, error.to_string());
            return Ok(messages.to_vec());
        }
        Err(error) => {
            contexts.record_error(session_id, error.to_string());
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
        if let Some(frontier) = compute_frontier(&units, &derived.summaries) {
            derived.frontier = frontier;
        }
        derived.epoch = derived.epoch.saturating_add(1);
    }
    if changed && let Err(error) = contexts.persist() {
        let derived = contexts.session_mut(session_id);
        derived.summaries.truncate(prev_len);
        derived.frontier = prev_frontier;
        derived.failures = derived.failures.saturating_add(1);
        derived.last_error = Some(error.to_string());
    }
    let replacement = planned.assembly.as_ref().and_then(|assembly| {
        if apply_assembly(assembly, budget.window_was_default) {
            render(messages, &spans, assembly).ok()
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

#[derive(Debug, Clone)]
pub(crate) struct UnitSpan {
    pub unit: SourceUnit,
    pub msg_from: usize,
    pub msg_to: usize,
}

fn merge_unit_kind(left: UnitKind, right: UnitKind) -> UnitKind {
    if left == UnitKind::Active || right == UnitKind::Active {
        UnitKind::Active
    } else if left == UnitKind::ToolExchange || right == UnitKind::ToolExchange {
        UnitKind::ToolExchange
    } else if left == UnitKind::CompletedTurn || right == UnitKind::CompletedTurn {
        UnitKind::CompletedTurn
    } else {
        UnitKind::Fixed
    }
}

pub(crate) fn units_from_ranges(
    messages: &[Message],
    ranges: &[cayw_context::CursorRange],
) -> Vec<UnitSpan> {
    let use_ranges = ranges.len() == messages.len();
    let span_cursors = |from: usize, to: usize| -> (u64, u64) {
        if use_ranges {
            let start = ranges[from].start_cursor;
            let end = ranges[to - 1].end_cursor.max(start);
            (start, end)
        } else {
            (from as u64 + 1, to as u64)
        }
    };
    let mut spans: Vec<UnitSpan> = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let (to, kind, text, outcome_unknown, pin_original) = match &messages[index] {
            Message::System(_) | Message::Custom { .. } => (
                index + 1,
                UnitKind::Fixed,
                messages[index].text(),
                false,
                false,
            ),
            Message::User { .. } => {
                if let Some(Message::Assistant(assistant)) = messages.get(index + 1)
                    && assistant.tool_calls().is_empty()
                {
                    (
                        index + 2,
                        UnitKind::CompletedTurn,
                        joined(&messages[index..=index + 1]),
                        false,
                        false,
                    )
                } else if index + 1 == messages.len() {
                    (
                        index + 1,
                        UnitKind::Active,
                        messages[index].text(),
                        false,
                        true,
                    )
                } else {
                    (
                        index + 1,
                        UnitKind::CompletedTurn,
                        messages[index].text(),
                        false,
                        false,
                    )
                }
            }
            Message::Assistant(assistant) => {
                let calls = assistant.tool_calls();
                if calls.is_empty() {
                    let kind = if assistant.stop_reason == cayw_ai::StopReason::Pending
                        || assistant.stop_reason.is_failure()
                    {
                        UnitKind::Active
                    } else {
                        UnitKind::CompletedTurn
                    };
                    (index + 1, kind, messages[index].text(), false, false)
                } else {
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
                    (
                        next,
                        if complete {
                            UnitKind::ToolExchange
                        } else {
                            UnitKind::Active
                        },
                        joined(&messages[index..next]),
                        unknown,
                        unknown || !complete,
                    )
                }
            }
            Message::ToolResult(result) => (
                index + 1,
                UnitKind::Active,
                messages[index].text(),
                result.outcome_unknown,
                true,
            ),
        };
        let (mut start_cursor, mut end_cursor) = span_cursors(index, to);
        if start_cursor == 0 {
            start_cursor = 1;
        }
        if use_ranges
            && let Some(prev) = spans.last_mut()
            && prev.unit.start_cursor == start_cursor
            && prev.unit.end_cursor == end_cursor
        {
            let prev_was_fixed = prev.unit.kind == UnitKind::Fixed;
            prev.msg_to = to;
            prev.unit.kind = merge_unit_kind(prev.unit.kind, kind);
            prev.unit.outcome_unknown |= outcome_unknown;
            prev.unit.pin_original |= pin_original;
            if prev.unit.kind == UnitKind::Fixed {
                if !text.is_empty() {
                    if !prev.unit.text.is_empty() {
                        prev.unit.text.push('\n');
                    }
                    prev.unit.text.push_str(&text);
                }
            } else if prev_was_fixed {
                prev.unit.text = if kind == UnitKind::Fixed {
                    String::new()
                } else {
                    text
                };
            } else if kind != UnitKind::Fixed && !text.is_empty() {
                if !prev.unit.text.is_empty() {
                    prev.unit.text.push('\n');
                }
                prev.unit.text.push_str(&text);
            }
            index = to;
            continue;
        }
        if let Some(prev) = spans.last()
            && start_cursor <= prev.unit.end_cursor
        {
            let shift = prev.unit.end_cursor.saturating_add(1) - start_cursor;
            start_cursor = start_cursor.saturating_add(shift);
            end_cursor = end_cursor.saturating_add(shift);
        }
        if end_cursor < start_cursor {
            end_cursor = start_cursor;
        }
        spans.push(UnitSpan {
            unit: unit(
                start_cursor,
                end_cursor,
                kind,
                text,
                outcome_unknown,
                pin_original,
            ),
            msg_from: index,
            msg_to: to,
        });
        index = to;
    }
    if matches!(messages.last(), Some(Message::ToolResult(_)))
        && let Some(last) = spans.last_mut()
    {
        last.unit.pin_original = true;
    }
    spans
}

fn compute_frontier(units: &[SourceUnit], summaries: &[SummaryRecord]) -> Option<u64> {
    let mut ready = Vec::new();
    let mut prev_end = 0_u64;
    for unit in units {
        let is_covered = match unit.kind {
            UnitKind::Fixed => true,
            UnitKind::CompletedTurn | UnitKind::ToolExchange
                if !unit.pin_original && !unit.outcome_unknown =>
            {
                summaries.iter().any(|summary| {
                    summary.start_cursor <= unit.start_cursor
                        && summary.end_cursor >= unit.end_cursor
                })
            }
            _ => false,
        };
        if is_covered {
            if unit.start_cursor > prev_end.saturating_add(1) {
                ready.push((prev_end.saturating_add(1), unit.start_cursor - 1));
            }
            ready.push((unit.start_cursor, unit.end_cursor));
        }
        prev_end = unit.end_cursor;
    }
    for summary in summaries {
        ready.push((summary.start_cursor, summary.end_cursor));
    }
    advance_frontier(0, &ready).ok()
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

fn render(
    messages: &[Message],
    spans: &[UnitSpan],
    assembly: &Assembly,
) -> Result<Vec<Message>, ()> {
    if spans.is_empty() {
        return Err(());
    }
    let mut rendered = Vec::new();
    let mut span_index = 0usize;
    for piece in &assembly.pieces {
        let (start, end, text, _) = piece.range();
        let Some(first_span) = spans.get(span_index) else {
            return Err(());
        };
        if start != first_span.unit.start_cursor || end < start {
            return Err(());
        }
        let Some(offset) = spans[span_index..]
            .iter()
            .position(|span| span.unit.end_cursor == end)
        else {
            return Err(());
        };
        let end_span_index = span_index + offset;
        let from = first_span.msg_from;
        let to = spans[end_span_index].msg_to;
        if from > to || to > messages.len() {
            return Err(());
        }
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
                for message in &messages[from..to] {
                    if matches!(message, Message::System(_) | Message::Custom { .. }) {
                        rendered.push(message.clone());
                    }
                }
                rendered.push(Message::system(text.to_owned()));
            }
        }
        span_index = end_span_index + 1;
    }
    if span_index != spans.len() {
        Err(())
    } else {
        Ok(rendered)
    }
}

fn excerpt_message(message: &Message, excerpt: &str) -> Message {
    match message {
        Message::ToolResult(result) if cayw_ai::content_text(&result.content).len() > 8 * 1024 => {
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
            contexts.persist().unwrap();
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

    #[test]
    fn units_from_ranges_uses_durable_event_cursors_not_message_indexes() {
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        assistant.content.push(Content::text("done"));
        let ranges = vec![
            cayw_context::CursorRange::new(10, 10).unwrap(),
            cayw_context::CursorRange::new(25, 25).unwrap(),
            cayw_context::CursorRange::new(40, 40).unwrap(),
        ];
        let messages = vec![
            Message::user("first question"),
            Message::Assistant(assistant),
            Message::user("current question"),
        ];
        let context = Mutex::new(contexts());
        prepare_provider_input_with_ranges(&context, "s", &messages, &ranges, None, 1_024).unwrap();
        let summaries = context.lock().unwrap().summaries("s");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].start_cursor, 10);
        assert_eq!(summaries[0].end_cursor, 25);
        assert_eq!(
            summaries[0].evidence,
            vec!["s:10".to_string(), "s:25".to_string()]
        );
    }

    #[test]
    fn prepare_provider_input_does_not_fabricate_synchronous_prefix_summaries_or_compressions() {
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        assistant.content.push(Content::text("A".repeat(200)));
        let messages = vec![
            Message::user("U".repeat(200)),
            Message::Assistant(assistant),
            Message::user("CURRENT"),
        ];
        let ranges = vec![
            cayw_context::CursorRange::new(10, 10).unwrap(),
            cayw_context::CursorRange::new(20, 20).unwrap(),
            cayw_context::CursorRange::new(30, 30).unwrap(),
        ];
        let context = Mutex::new(contexts());
        let first =
            prepare_provider_input_with_ranges(&context, "s", &messages, &ranges, Some(1_450), 1)
                .unwrap();
        assert_eq!(first, messages);
        assert!(
            context.lock().unwrap().summaries("s").is_empty(),
            "long turns wait for background model maintenance instead of synchronous prefix truncation"
        );
        assert!(
            context.lock().unwrap().compressions("s").is_empty(),
            "compression runs as background model maintenance instead of synchronous prefix truncation"
        );
    }

    #[test]
    fn forked_session_equal_cursors_merge_without_fake_cursor_shift() {
        let mut system = Message::system("rules");
        if let Message::System(inner) = &mut system {
            inner.tools_added.push(ToolDefinition {
                name: "execute_code".into(),
                description: "run code".into(),
                parameters: serde_json::json!({}),
            });
        }
        let mut a1 = AssistantMessage::new(&Model::new("fixture", "model"));
        a1.content.push(Content::text("inherited reply 1"));
        let mut a2 = AssistantMessage::new(&Model::new("fixture", "model"));
        a2.content.push(Content::text("inherited reply 2"));
        let messages = vec![
            system.clone(),
            Message::user("inherited prompt 1"),
            Message::Assistant(a1),
            Message::user("inherited prompt 2"),
            Message::Assistant(a2),
            Message::user("post-fork prompt"),
        ];
        let ranges = vec![
            cayw_context::CursorRange::new(2, 2).unwrap(),
            cayw_context::CursorRange::new(2, 2).unwrap(),
            cayw_context::CursorRange::new(2, 2).unwrap(),
            cayw_context::CursorRange::new(2, 2).unwrap(),
            cayw_context::CursorRange::new(2, 2).unwrap(),
            cayw_context::CursorRange::new(5, 5).unwrap(),
        ];
        let spans = units_from_ranges(&messages, &ranges);
        assert_eq!(spans.len(), 2);
        assert_eq!(
            (spans[0].unit.start_cursor, spans[0].unit.end_cursor),
            (2, 2)
        );
        assert_eq!(
            (spans[1].unit.start_cursor, spans[1].unit.end_cursor),
            (5, 5)
        );

        let context = Mutex::new(contexts());
        prepare_provider_input_with_ranges(&context, "forked", &messages, &ranges, None, 1_024)
            .unwrap();
        let summaries = context.lock().unwrap().summaries("forked");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].evidence, vec!["forked:2".to_string()]);
    }

    #[test]
    fn mid_transcript_tool_declarations_stay_intact_when_later_history_is_compressed() {
        let mut tool_decl = Message::system("");
        if let Message::System(inner) = &mut tool_decl {
            inner.tools_added.push(ToolDefinition {
                name: "execute_code".into(),
                description: "run code".into(),
                parameters: serde_json::json!({}),
            });
        }
        let mut call1 = AssistantMessage::new(&Model::new("fixture", "model"));
        call1.content.push(Content::ToolCall(ToolCall::new(
            "c1",
            "execute_code",
            serde_json::json!({}),
        )));
        let res1 = Message::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "execute_code".into(),
            content: vec![Content::text("O".repeat(400))],
            details: None,
            structured_content: None,
            usage: None,
            is_error: false,
            outcome_unknown: false,
            timestamp: 1,
            duration_ms: Some(1),
        });
        let mut call2 = AssistantMessage::new(&Model::new("fixture", "model"));
        call2.content.push(Content::ToolCall(ToolCall::new(
            "c2",
            "execute_code",
            serde_json::json!({}),
        )));
        let res2 = Message::ToolResult(ToolResultMessage {
            tool_call_id: "c2".into(),
            tool_name: "execute_code".into(),
            content: vec![Content::text("still active result")],
            details: None,
            structured_content: None,
            usage: None,
            is_error: false,
            outcome_unknown: false,
            timestamp: 2,
            duration_ms: Some(1),
        });
        let messages = vec![
            Message::system("base rules"),
            Message::user("initial goal"),
            tool_decl,
            Message::Assistant(call1),
            res1,
            Message::Assistant(call2),
            res2,
            Message::user("CURRENT"),
        ];
        let context = Mutex::new(contexts());
        publish_compression(
            &context,
            "s",
            CompressionRecord {
                compression_id: "comp-1".into(),
                version: 1,
                session_id: "s".into(),
                covered_through: 5,
                text: "compressed including post-declaration tool exchange".into(),
                evidence: vec!["s:2".into(), "s:5".into()],
            },
        )
        .unwrap();
        let prepared = prepare_provider_input(&context, "s", &messages, Some(1_300), 1).unwrap();
        assert!(
            prepared
                .iter()
                .any(|m| m.text() == "compressed including post-declaration tool exchange")
        );
        assert!(
            !prepared.iter().any(|m| m.text().contains(&"O".repeat(400))),
            "history after mid-transcript tool declaration must be replaced"
        );
        assert_eq!(
            cayw_ai::current_system(&prepared).tools.len(),
            1,
            "tool declaration inside compressed range must stay active for remaining tool calls"
        );
    }

    #[test]
    fn legacy_v1_snapshot_drops_truncated_records_while_keeping_reservations_occupied() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MAINTENANCE_FILE),
            serde_json::to_vec_pretty(&serde_json::json!({
                "sessions": {
                    "s": {
                        "epoch": 3,
                        "frontier": 4,
                        "summaries": [{
                            "summary_id": "old",
                            "version": 1,
                            "session_id": "s",
                            "kind": "user_turn",
                            "start_cursor": 1,
                            "end_cursor": 2,
                            "text": "[summary s:1-2] truncated...",
                            "evidence": ["s:1"],
                            "local_reuse": false
                        }],
                        "compressions": [{
                            "compression_id": "old-c",
                            "version": 1,
                            "session_id": "s",
                            "covered_through": 2,
                            "text": "[compressed s:1-2] truncated...",
                            "evidence": ["s:1"]
                        }],
                        "reservations": [{
                            "id": 1,
                            "attempt": 1,
                            "kind": "compression",
                            "start_cursor": 1,
                            "end_cursor": 2,
                            "tokens": 42,
                            "state": "reserved",
                            "confirmed": null,
                            "overage": false
                        }]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut loaded = SessionContexts::new(Some(dir.path().to_path_buf()));
        assert!(loaded.summaries("s").is_empty());
        assert!(loaded.compressions("s").is_empty());
        assert_eq!(loaded.occupied("s"), 42);
        loaded
            .reserve("s", spec(JobKind::Compression, 1, 4, 10, 2, 5))
            .expect("abandoned reservation stays occupied as Unknown without freezing new jobs");
    }
}
