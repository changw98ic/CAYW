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

use crate::paths::{RetryPath, build_retry_paths};
use crate::timeline::{RunTimeline, build_run_timeline};
use crate::tracks::build_duration_track_with_epoch;
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
    all_steps: Vec<RunStep>,
    page_steps: Vec<RunStep>,
}

impl SessionBuilder {
    fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            turn: 0,
            ordinal: 0,
            open: HashMap::new(),
            all_steps: Vec::new(),
            page_steps: Vec::new(),
        }
    }

    fn on_agent(&mut self, record: &EventRecord, event: &AgentEvent, emit_step: bool) {
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
                let started_from_open = self.open.remove(&call.id).map(|open| open.started_at_ms);
                let started = started_from_open.unwrap_or_else(|| {
                    if let Some(duration) = duration_ms {
                        record.timestamp.saturating_sub(*duration)
                    } else {
                        record.timestamp
                    }
                });
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
                self.all_steps.push(step.clone());
                if emit_step {
                    self.page_steps.push(step);
                }
                self.ordinal = self.ordinal.saturating_add(1);
            }
            _ => {}
        }
    }
}

/// Fold Core event records for one session within a window `[start..end]`
/// into a [`SessionRunSnapshot`]. Events in `0..start` are consumed as prefix
/// context (maintaining turn, ordinal, and open call start timestamps) without
/// emitting steps prior to `start`.
pub fn snapshot_from_runtime_events_window(
    session_id: &str,
    label: Option<String>,
    task_fingerprint: Option<String>,
    records: &[EventRecord],
    start: usize,
    end: usize,
) -> SessionRunSnapshot {
    let mut builder = SessionBuilder::new(session_id);
    let clamped_start = start.min(records.len());
    let clamped_end = end.min(records.len()).max(clamped_start);
    for (i, record) in records[..clamped_end].iter().enumerate() {
        if record.session_id != session_id {
            continue;
        }
        if let CoreEvent::Agent { event } = &record.event {
            let emit_step = i >= clamped_start;
            builder.on_agent(record, event, emit_step);
        }
    }
    SessionRunSnapshot {
        session_id: session_id.into(),
        label,
        task_fingerprint,
        steps: builder.page_steps,
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
    snapshot_from_runtime_events_window(
        session_id,
        label,
        task_fingerprint,
        records,
        0,
        records.len(),
    )
}

/// Group records by session and build snapshots within an event window `[start..end]`.
pub fn snapshots_from_runtime_events_window(
    records: &[EventRecord],
    start: usize,
    end: usize,
) -> Vec<SessionRunSnapshot> {
    let mut order: Vec<String> = Vec::new();
    let mut seen = BTreeMap::<String, ()>::new();
    let clamped_end = end.min(records.len());
    for record in &records[..clamped_end] {
        if seen.insert(record.session_id.clone(), ()).is_none() {
            order.push(record.session_id.clone());
        }
    }
    order
        .into_iter()
        .map(|session_id| {
            snapshot_from_runtime_events_window(&session_id, None, None, records, start, end)
        })
        .filter(|snapshot| !snapshot.steps.is_empty())
        .collect()
}

/// Group records by session and build one snapshot per session that produced steps.
pub fn snapshots_from_runtime_events(records: &[EventRecord]) -> Vec<SessionRunSnapshot> {
    snapshots_from_runtime_events_window(records, 0, records.len())
}

/// Translate event records within a window then fold each session into a [`RunTimeline`] and [`SessionRunSnapshot`].
pub fn timelines_and_snapshots_from_runtime_events_window(
    records: &[EventRecord],
    start: usize,
    end: usize,
) -> (Vec<RunTimeline>, Vec<SessionRunSnapshot>) {
    let mut order: Vec<String> = Vec::new();
    let mut seen = BTreeMap::<String, ()>::new();
    let clamped_end = end.min(records.len());
    for record in &records[..clamped_end] {
        if seen.insert(record.session_id.clone(), ()).is_none() {
            order.push(record.session_id.clone());
        }
    }

    let mut timelines = Vec::new();
    let mut snapshots = Vec::new();

    let clamped_start = start.min(records.len());
    for session_id in order {
        let mut builder = SessionBuilder::new(&session_id);
        for (i, record) in records[..clamped_end].iter().enumerate() {
            if record.session_id != session_id {
                continue;
            }
            if let CoreEvent::Agent { event } = &record.event {
                let emit_step = i >= clamped_start;
                builder.on_agent(record, event, emit_step);
            }
        }
        if builder.page_steps.is_empty() {
            continue;
        }

        let full_snapshot = SessionRunSnapshot {
            session_id: session_id.clone(),
            label: None,
            task_fingerprint: None,
            steps: builder.all_steps,
        };
        let page_snapshot = SessionRunSnapshot {
            session_id: session_id.clone(),
            label: None,
            task_fingerprint: None,
            steps: builder.page_steps,
        };

        // Align track offset with the session's overall start time.
        let session_t0 = full_snapshot.steps.first().map(|s| s.started_at_ms);
        let track = build_duration_track_with_epoch(&page_snapshot, session_t0);

        // Keep retry paths relevant to the steps in this window.
        let all_retry_paths = build_retry_paths(&full_snapshot);
        let page_step_ids: std::collections::HashSet<&str> = page_snapshot
            .steps
            .iter()
            .map(|s| s.step_id.as_str())
            .collect();
        let retry_paths: Vec<RetryPath> = all_retry_paths
            .into_iter()
            .filter(|path| {
                page_step_ids.contains(path.failure_step_id.as_str())
                    || path
                        .follow_up_step_ids
                        .iter()
                        .any(|id| page_step_ids.contains(id.as_str()))
            })
            .collect();

        let failure_count = page_snapshot
            .steps
            .iter()
            .filter(|step| step.outcome == StepOutcome::Failed)
            .count() as u64;

        let timeline = RunTimeline {
            session_id: session_id.clone(),
            label: None,
            task_fingerprint: None,
            step_count: page_snapshot.steps.len() as u64,
            failure_count,
            retry_paths,
            track,
        };

        snapshots.push(page_snapshot);
        timelines.push(timeline);
    }

    (timelines, snapshots)
}

/// Translate event records within a window then fold each session into a [`RunTimeline`].
pub fn timelines_from_runtime_events_window(
    records: &[EventRecord],
    start: usize,
    end: usize,
) -> Vec<RunTimeline> {
    timelines_and_snapshots_from_runtime_events_window(records, start, end).0
}

/// Translate event records then fold each session into a [`RunTimeline`].
pub fn timelines_from_runtime_events(records: &[EventRecord]) -> Vec<RunTimeline> {
    timelines_from_runtime_events_window(records, 0, records.len())
}

/// Merge two or more session run snapshots for the same session into one combined snapshot.
pub fn merge_session_snapshots(
    session_id: &str,
    label: Option<String>,
    task_fingerprint: Option<String>,
    snapshots: &[SessionRunSnapshot],
) -> SessionRunSnapshot {
    let mut steps = Vec::new();
    for snapshot in snapshots {
        if snapshot.session_id == session_id {
            steps.extend(snapshot.steps.clone());
        }
    }
    steps.sort_by_key(|step| (step.turn, step.ordinal, step.started_at_ms));
    SessionRunSnapshot {
        session_id: session_id.into(),
        label,
        task_fingerprint,
        steps,
    }
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
