//! Translate durable Runtime / Trace `EventRecord`s into [`RunStep`] snapshots.
//!
//! Authority is the append-only Core event log: tool steps come from
//! `AgentEvent::ToolExecutionStart` / `ToolExecutionEnd` pairs. This module does
//! not invent outcomes, call an LLM, or rewrite evidence cursors.

use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::content_text;
use cayw_core::{CoreEvent, EventRecord};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

use crate::timeline::{RunTimeline, build_run_timeline};
use crate::types::{RunStep, SessionRunSnapshot, StepOutcome};

/// Stable host digest of tool arguments (equal digests ⇒ same params).
pub fn params_digest(arguments: &Value) -> String {
    let bytes = serde_json::to_vec(arguments).unwrap_or_else(|_| b"null".to_vec());
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn outcome_from_result(result: &ToolResult) -> StepOutcome {
    if result.is_error {
        return StepOutcome::Failed;
    }
    let text = content_text(&result.content);
    if text.trim().is_empty() && result.structured_content.is_none() {
        StepOutcome::Empty
    } else {
        StepOutcome::Success
    }
}

fn error_summary(result: &ToolResult) -> Option<String> {
    if !result.is_error {
        return None;
    }
    let text = content_text(&result.content);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(512).collect())
    }
}

struct OpenCall {
    started_at_ms: u64,
}

struct SessionBuilder {
    session_id: String,
    turn: u64,
    ordinal: u64,
    open: HashMap<String, OpenCall>,
    steps: Vec<RunStep>,
}

impl SessionBuilder {
    fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            turn: 0,
            ordinal: 0,
            open: HashMap::new(),
            steps: Vec::new(),
        }
    }

    fn on_agent(&mut self, record: &EventRecord, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStart => {
                self.turn = self.turn.saturating_add(1);
                self.ordinal = 0;
            }
            AgentEvent::ToolExecutionStart { call } => {
                self.open.insert(
                    call.id.clone(),
                    OpenCall {
                        started_at_ms: record.timestamp,
                    },
                );
            }
            AgentEvent::ToolExecutionEnd {
                call,
                result,
                duration_ms,
            } => {
                let started = self
                    .open
                    .remove(&call.id)
                    .map(|open| open.started_at_ms)
                    .unwrap_or(record.timestamp);
                let elapsed =
                    duration_ms.unwrap_or_else(|| record.timestamp.saturating_sub(started));
                if self.turn == 0 {
                    // Tool ends without TurnStart still land in turn 1.
                    self.turn = 1;
                }
                let step = RunStep {
                    step_id: call.id.clone(),
                    session_id: self.session_id.clone(),
                    turn: self.turn,
                    ordinal: self.ordinal,
                    tool: call.name.clone(),
                    params_digest: params_digest(&call.arguments),
                    outcome: outcome_from_result(result),
                    started_at_ms: started,
                    elapsed_ms: elapsed,
                    evidence_cursor: Some(record.cursor),
                    error_summary: error_summary(result),
                };
                self.ordinal = self.ordinal.saturating_add(1);
                self.steps.push(step);
            }
            _ => {}
        }
    }
}

/// Fold Core event records for one session into a [`SessionRunSnapshot`].
///
/// Records for other `session_id`s are ignored. `label` / `task_fingerprint`
/// are caller-supplied metadata (kept empty when unknown).
pub fn snapshot_from_runtime_events(
    session_id: &str,
    label: Option<String>,
    task_fingerprint: Option<String>,
    records: &[EventRecord],
) -> SessionRunSnapshot {
    let mut builder = SessionBuilder::new(session_id);
    for record in records {
        if record.session_id != session_id {
            continue;
        }
        if let CoreEvent::Agent { event } = &record.event {
            builder.on_agent(record, event);
        }
    }
    SessionRunSnapshot {
        session_id: session_id.into(),
        label,
        task_fingerprint,
        steps: builder.steps,
    }
}

/// Group records by session and build one snapshot per session that produced steps.
pub fn snapshots_from_runtime_events(records: &[EventRecord]) -> Vec<SessionRunSnapshot> {
    let mut order: Vec<String> = Vec::new();
    let mut seen = BTreeMap::<String, ()>::new();
    for record in records {
        if seen.insert(record.session_id.clone(), ()).is_none() {
            order.push(record.session_id.clone());
        }
    }
    order
        .into_iter()
        .map(|session_id| snapshot_from_runtime_events(&session_id, None, None, records))
        .filter(|snapshot| !snapshot.steps.is_empty())
        .collect()
}

/// Translate event records then fold each session into a [`RunTimeline`].
pub fn timelines_from_runtime_events(records: &[EventRecord]) -> Vec<RunTimeline> {
    snapshots_from_runtime_events(records)
        .iter()
        .map(build_run_timeline)
        .collect()
}

/// Pagination / truncation signal for a run-inspector response.
///
/// Callers must not assume a single Core event page covers a long session.
/// When `has_more` is true, request again with `after = next_after`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunInspectorPage {
    /// Durable session cursor (exclusive end / total event count).
    pub events_total: u64,
    /// Exclusive start cursor used for this response.
    pub after: u64,
    /// How many Core events were scanned into this view.
    pub events_scanned: u64,
    /// More durable events exist beyond [`Self::next_after`].
    pub has_more: bool,
    /// Pass as `after` on the next request when `has_more`.
    pub next_after: u64,
}

/// One RPC / product payload: raw steps plus folded timeline and page meta.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunInspectorView {
    pub protocol_version: u32,
    pub snapshot: SessionRunSnapshot,
    pub timeline: RunTimeline,
    pub events_total: u64,
    pub after: u64,
    pub events_scanned: u64,
    pub has_more: bool,
    pub next_after: u64,
}

impl RunInspectorView {
    fn with_page(mut self, page: RunInspectorPage) -> Self {
        self.events_total = page.events_total;
        self.after = page.after;
        self.events_scanned = page.events_scanned;
        self.has_more = page.has_more;
        self.next_after = page.next_after;
        self
    }
}

/// Build the product view for one session from a supplied event slice.
///
/// Page metadata assumes the slice is complete for the session (has_more=false).
/// Prefer [`inspect_runtime_events_page`] when the caller knows session totals.
pub fn inspect_runtime_events(
    session_id: &str,
    label: Option<String>,
    task_fingerprint: Option<String>,
    records: &[EventRecord],
) -> RunInspectorView {
    let scanned = records
        .iter()
        .filter(|record| record.session_id == session_id)
        .count() as u64;
    let next_after = records
        .iter()
        .filter(|record| record.session_id == session_id)
        .map(|record| record.cursor)
        .max()
        .unwrap_or(0);
    inspect_runtime_events_page(
        session_id,
        label,
        task_fingerprint,
        records,
        RunInspectorPage {
            events_total: next_after,
            after: 0,
            events_scanned: scanned,
            has_more: false,
            next_after,
        },
    )
}

/// Build the product view and attach explicit pagination metadata.
pub fn inspect_runtime_events_page(
    session_id: &str,
    label: Option<String>,
    task_fingerprint: Option<String>,
    records: &[EventRecord],
    page: RunInspectorPage,
) -> RunInspectorView {
    let snapshot = snapshot_from_runtime_events(session_id, label, task_fingerprint, records);
    let timeline = build_run_timeline(&snapshot);
    RunInspectorView {
        protocol_version: crate::RUN_INSPECTOR_PROTOCOL_VERSION,
        snapshot,
        timeline,
        events_total: 0,
        after: 0,
        events_scanned: 0,
        has_more: false,
        next_after: 0,
    }
    .with_page(page)
}
