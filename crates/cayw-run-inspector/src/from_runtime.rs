//! Translate durable Runtime / Trace `EventRecord`s into [`RunStep`] snapshots.
//!
//! Authority is the append-only Core event log: tool steps come from
//! `AgentEvent::ToolExecutionStart` / `ToolExecutionEnd` pairs. Background
//! execution and child capability records update those rows without rewriting
//! the underlying log. A page is not an execution terminal.

use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::content_text;
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CoreEvent, EventRecord, ExecutionOutcome,
    ExecutionRecord, ExecutionState, ReconciledOutcome,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::paths::{RetryPath, build_retry_paths};
use crate::timeline::{RunTimeline, build_run_timeline};
use crate::tracks::build_duration_track_with_epoch;
use crate::types::{RunStep, SessionRunSnapshot, StepOutcome};

/// Stable host digest of tool arguments (equal digests ⇒ same params).
pub fn params_digest(arguments: &Value) -> String {
    let bytes = serde_json::to_vec(arguments).unwrap_or_else(|_| b"null".to_vec());
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn outcome_from_result(tool: &str, result: &ToolResult) -> StepOutcome {
    if result.outcome_unknown {
        return StepOutcome::OutcomeUnknown;
    }
    if tool == "execute_code"
        && let Some(page) = &result.structured_content
    {
        match page.get("status").and_then(Value::as_str) {
            Some("cancelled") => return StepOutcome::Cancelled,
            Some("interrupted") => return StepOutcome::Interrupted,
            _ => {}
        }
        if page.get("vm_alive") == Some(&Value::Bool(true)) {
            return StepOutcome::Running;
        }
    }
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
    if !result.is_error && !result.outcome_unknown {
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

#[derive(Clone)]
struct OpenCall {
    started_at_ms: u64,
}

#[derive(Clone, Default)]
struct ExecutionProjection {
    state: Option<ExecutionState>,
    terminal_at_ms: Option<u64>,
    terminal_error: Option<String>,
    // Keep only diagnostic facts, never raw capability arguments or outputs.
    diagnostics: BTreeMap<u64, (StepOutcome, String)>,
    reconciled: BTreeMap<u64, String>,
    cursor: u64,
}

impl ExecutionProjection {
    fn project(&self, step: &mut RunStep) {
        if let Some(ended) = self.terminal_at_ms {
            step.elapsed_ms = step
                .elapsed_ms
                .max(ended.saturating_sub(step.started_at_ms));
        }
        let unknown = self
            .diagnostics
            .values()
            .any(|(outcome, _)| *outcome == StepOutcome::OutcomeUnknown);
        if unknown {
            step.outcome = StepOutcome::OutcomeUnknown;
        } else if step.outcome != StepOutcome::OutcomeUnknown {
            if let Some(state) = self.state
                && step.outcome != StepOutcome::Failed
            {
                match state {
                    ExecutionState::Running | ExecutionState::Yielded => {
                        step.outcome = StepOutcome::Running;
                    }
                    ExecutionState::Cancelled => step.outcome = StepOutcome::Cancelled,
                    ExecutionState::Interrupted => step.outcome = StepOutcome::Interrupted,
                    ExecutionState::Failed => step.outcome = StepOutcome::Failed,
                    ExecutionState::Completed => {
                        if step.outcome == StepOutcome::Running {
                            step.outcome = StepOutcome::Success;
                        }
                    }
                }
            }
            // A completed Luau program can have handled a failed operation.
            // Keep that child fact visible; do not label the VM itself failed.
            if !self.diagnostics.is_empty()
                && !matches!(
                    step.outcome,
                    StepOutcome::Running | StepOutcome::Cancelled | StepOutcome::Interrupted
                )
            {
                step.outcome = StepOutcome::Failed;
            }
        }
        let mut errors: Vec<&str> = self.terminal_error.iter().map(String::as_str).collect();
        errors.extend(self.diagnostics.values().map(|(_, error)| error.as_str()));
        errors.extend(self.reconciled.values().map(String::as_str));
        if !errors.is_empty() {
            let errors = errors.join("; ");
            step.error_summary = Some(match self.state {
                Some(state) => format!("Execution {state:?}; {errors}"),
                None => errors,
            });
        }
        step.evidence_cursor = Some(self.cursor.max(step.evidence_cursor.unwrap_or(0)));
    }
}

fn capability_diagnostic(call: &CapabilityCallRecord) -> Option<(StepOutcome, String)> {
    let (outcome, error) = match &call.outcome {
        Some(CapabilityOutcome::OutcomeUnknown { error }) => {
            (StepOutcome::OutcomeUnknown, error.clone())
        }
        Some(CapabilityOutcome::Failed { error }) => (StepOutcome::Failed, error.clone()),
        Some(CapabilityOutcome::NotExecuted { reason }) => {
            (StepOutcome::Failed, format!("not executed: {reason}"))
        }
        Some(CapabilityOutcome::Completed { result })
            if matches!(
                call.name.as_str(),
                "build.run" | "test.run" | "check.run" | "lint.run" | "format.run"
            ) && result.get("success") == Some(&Value::Bool(false)) =>
        {
            let error = match result.get("exit_code") {
                Some(code) => format!("process returned exit_code={code}"),
                None => "process reported success=false".into(),
            };
            (StepOutcome::Failed, error)
        }
        _ => return None,
    };
    Some((
        outcome,
        format!("Child {} #{}: {error}", call.name, call.key.sequence),
    ))
}

#[derive(Clone)]
struct SessionBuilder {
    session_id: String,
    turn: u64,
    ordinal: u64,
    open: HashMap<String, OpenCall>,
    all_steps: Vec<RunStep>,
    executions: HashMap<String, ExecutionProjection>,
    bindings: HashMap<String, String>,
    dirty_calls: HashSet<String>,
    dirty_executions: HashSet<String>,
    reconciliations: HashMap<String, (StepOutcome, Option<String>, u64)>,
}

impl SessionBuilder {
    fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            turn: 0,
            ordinal: 0,
            open: HashMap::new(),
            all_steps: Vec::new(),
            executions: HashMap::new(),
            bindings: HashMap::new(),
            dirty_calls: HashSet::new(),
            dirty_executions: HashSet::new(),
            reconciliations: HashMap::new(),
        }
    }

    fn on_record(&mut self, record: &EventRecord, emit_step: bool) {
        match &record.event {
            CoreEvent::Agent { event } => self.on_agent(record, event, emit_step),
            CoreEvent::ExecutionStarted { execution }
            | CoreEvent::ExecutionYielded { execution }
            | CoreEvent::ExecutionFinished { execution } => {
                self.on_execution(record, execution, emit_step)
            }
            CoreEvent::ExecutionWaitRegistered { binding } => {
                self.bindings
                    .insert(binding.call_id.clone(), binding.execution_id.clone());
            }
            CoreEvent::CapabilityPlanned { call }
            | CoreEvent::CapabilityStarted { call }
            | CoreEvent::CapabilitySettled { call }
            | CoreEvent::CapabilityReconciled { call, .. } => {
                self.on_capability(record, call, emit_step)
            }
            CoreEvent::AttemptRecovered {
                executions,
                capabilities,
                ..
            } => {
                for execution in executions {
                    self.on_execution(record, execution, emit_step);
                }
                for call in capabilities {
                    self.on_capability(record, call, emit_step);
                }
            }
            CoreEvent::Reconciled { reconciliation } => {
                let (outcome, summary) = match &reconciliation.outcome {
                    ReconciledOutcome::Completed { result } => (
                        outcome_from_result(&reconciliation.expected.call.name, result),
                        error_summary(result),
                    ),
                    ReconciledOutcome::NotExecuted { reason } => {
                        (StepOutcome::Failed, Some(format!("not executed: {reason}")))
                    }
                };
                self.reconciliations.insert(
                    reconciliation.call_id.clone(),
                    (outcome, summary, record.cursor),
                );
                if emit_step {
                    self.dirty_calls.insert(reconciliation.call_id.clone());
                }
            }
            _ => {}
        }
    }

    fn on_execution(&mut self, record: &EventRecord, execution: &ExecutionRecord, emit: bool) {
        self.bindings.insert(
            execution.parent_call_id.clone(),
            execution.execution_id.clone(),
        );
        let projection = self
            .executions
            .entry(execution.execution_id.clone())
            .or_default();
        projection.state = Some(execution.state);
        if execution.terminal.is_some() && projection.terminal_at_ms.is_none() {
            projection.terminal_at_ms = Some(record.timestamp);
        }
        projection.terminal_error = match &execution.terminal {
            Some(ExecutionOutcome::Failed { error }) => Some(error.clone()),
            Some(
                ExecutionOutcome::Cancelled { reason } | ExecutionOutcome::Interrupted { reason },
            ) => Some(reason.clone()),
            _ => None,
        };
        projection.cursor = record.cursor;
        if emit {
            self.dirty_executions.insert(execution.execution_id.clone());
        }
    }

    fn on_capability(&mut self, record: &EventRecord, call: &CapabilityCallRecord, emit: bool) {
        self.bindings
            .insert(call.parent_call_id.clone(), call.key.execution_id.clone());
        let projection = self
            .executions
            .entry(call.key.execution_id.clone())
            .or_default();
        if let Some(diagnostic) = capability_diagnostic(call) {
            projection.diagnostics.insert(call.key.sequence, diagnostic);
        } else {
            projection.diagnostics.remove(&call.key.sequence);
        }
        if matches!(&record.event, CoreEvent::CapabilityReconciled { .. }) {
            projection.reconciled.insert(
                call.key.sequence,
                format!(
                    "Child {} #{} reconciled at cursor {}",
                    call.name, call.key.sequence, record.cursor
                ),
            );
        }
        projection.cursor = record.cursor;
        if emit {
            self.dirty_executions.insert(call.key.execution_id.clone());
        }
    }

    fn finish(mut self) -> (Vec<RunStep>, Vec<RunStep>) {
        let mut page_steps = Vec::new();
        for step in &mut self.all_steps {
            let reconciled = self.reconciliations.get(&step.step_id);
            if let Some((outcome, summary, cursor)) = reconciled {
                step.outcome = *outcome;
                step.error_summary = summary.clone();
                step.evidence_cursor = Some(*cursor);
            }
            let execution_id = self.bindings.get(&step.step_id);
            if let Some(execution) = execution_id.and_then(|id| self.executions.get(id)) {
                execution.project(step);
            }
            if let Some((_, _, cursor)) = reconciled {
                let notice = format!("Tool reconciled at cursor {cursor}");
                step.error_summary = Some(match &step.error_summary {
                    Some(summary) => format!("{notice}; {summary}"),
                    None => notice,
                });
            }
            if self.dirty_calls.contains(&step.step_id)
                || execution_id.is_some_and(|id| self.dirty_executions.contains(id))
            {
                page_steps.push(step.clone());
            }
        }
        (self.all_steps, page_steps)
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
                    outcome: outcome_from_result(&call.name, result),
                    started_at_ms: started,
                    elapsed_ms: elapsed,
                    evidence_cursor: Some(record.cursor),
                    error_summary: error_summary(result),
                };
                self.all_steps.push(step);
                if emit_step {
                    self.dirty_calls.insert(call.id.clone());
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
        builder.on_record(record, i >= clamped_start);
    }
    SessionRunSnapshot {
        session_id: session_id.into(),
        label,
        task_fingerprint,
        steps: builder.finish().1,
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
            builder.on_record(record, i >= clamped_start);
        }
        let Some((timeline, page_snapshot)) = archive_tracks(builder) else {
            continue;
        };
        snapshots.push(page_snapshot);
        timelines.push(timeline);
    }

    (timelines, snapshots)
}

fn archive_tracks(builder: SessionBuilder) -> Option<(RunTimeline, SessionRunSnapshot)> {
    let session_id = builder.session_id.clone();
    let (all_steps, page_steps) = builder.finish();
    if page_steps.is_empty() {
        return None;
    }

    let full_snapshot = SessionRunSnapshot {
        session_id: session_id.clone(),
        label: None,
        task_fingerprint: None,
        steps: all_steps,
    };
    let page_snapshot = SessionRunSnapshot {
        session_id: session_id.clone(),
        label: None,
        task_fingerprint: None,
        steps: page_steps,
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

    Some((timeline, page_snapshot))
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
    let mut by_id = HashMap::<String, RunStep>::new();
    for snapshot in snapshots {
        if snapshot.session_id == session_id {
            for step in &snapshot.steps {
                let existing = by_id
                    .entry(step.step_id.clone())
                    .or_insert_with(|| step.clone());
                if step.evidence_cursor >= existing.evidence_cursor {
                    *existing = step.clone();
                }
            }
        }
    }
    let mut steps: Vec<_> = by_id.into_values().collect();
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
    /// Events in the requested window.
    pub events_scanned: u64,
    /// Events read this time, including any uncached prefix.
    pub events_read: u64,
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
    pub events_read: u64,
    pub has_more: bool,
    pub next_after: u64,
}

impl RunInspectorView {
    fn with_page(mut self, page: RunInspectorPage) -> Self {
        self.events_total = page.events_total;
        self.after = page.after;
        self.events_scanned = page.events_scanned;
        self.events_read = page.events_read;
        self.has_more = page.has_more;
        self.next_after = page.next_after;
        self
    }
}

/// Incremental fold for stores that read Core events in bounded pages.
/// Feed the prefix through `after` as context, then the requested window.
/// Only step projections and execution identities are retained, not raw events.
#[derive(Clone)]
pub struct RuntimeEventInspector {
    builder: SessionBuilder,
    after: u64,
}

impl RuntimeEventInspector {
    pub fn new(session_id: &str, after: u64) -> Self {
        Self {
            builder: SessionBuilder::new(session_id),
            after,
        }
    }

    /// Retain call and execution identities from a previously read prefix.
    pub fn continue_after(mut self, after: u64) -> Self {
        self.after = after;
        self.builder.dirty_calls.clear();
        self.builder.dirty_executions.clear();
        self
    }

    pub fn push(&mut self, record: &EventRecord) {
        if record.session_id == self.builder.session_id {
            self.builder.on_record(record, record.cursor > self.after);
        }
    }

    pub fn push_archive(&mut self, record: &EventRecord, emit: bool) {
        self.builder.on_record(record, emit);
    }

    pub fn finish_archive(self) -> Option<(RunTimeline, SessionRunSnapshot)> {
        archive_tracks(self.builder)
    }

    pub fn finish(
        self,
        label: Option<String>,
        task_fingerprint: Option<String>,
        page: RunInspectorPage,
    ) -> RunInspectorView {
        let session_id = self.builder.session_id.clone();
        let snapshot = SessionRunSnapshot {
            session_id,
            label,
            task_fingerprint,
            steps: self.builder.finish().1,
        };
        let timeline = build_run_timeline(&snapshot);
        RunInspectorView {
            protocol_version: crate::RUN_INSPECTOR_PROTOCOL_VERSION,
            snapshot,
            timeline,
            events_total: 0,
            after: 0,
            events_scanned: 0,
            events_read: 0,
            has_more: false,
            next_after: 0,
        }
        .with_page(page)
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
            events_read: records.len() as u64,
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
        events_read: 0,
        has_more: false,
        next_after: 0,
    }
    .with_page(page)
}
