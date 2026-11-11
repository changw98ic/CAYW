//! Asynchronous model maintenance for summaries and execution-session compression.
//!
//! Maintenance calls run outside the foreground turn lease and outside the
//! Core model-request ledger. They take the budget tree lock only while
//! recording or settling their bounded reservation, never while streaming.

use std::sync::{Arc, Weak};

use cayw_ai::{
    AssistantEvent, CancellationToken, Message, ModelRequest, Provider, StopReason, UsageStatus,
    content_text,
};
use cayw_context::{
    CompressionJob, CompressionRecord, CursorRange, InputBudget, PendingSummary, SourceUnit,
    SummaryKind, SummaryRecord, UnitKind, VersionDecision, estimate_text, log_id,
};
use futures::StreamExt;

use crate::{
    Inner, Runtime,
    session_context::{
        JobKind, MaintenanceInspection, ReservationSpec, inspect_maintenance, publish_compression,
        publish_summary,
    },
};

/// Unsummarized segment threshold from `docs/memory-context-work.md` (~4K tokens).
const SEGMENT_SUMMARY_TRIGGER_BYTES: u64 = 4_096;

const SUMMARY_SYSTEM_PROMPT: &str = "Summarize this completed conversation segment or tool exchange in the same language as the source. Synthesize the whole segment rather than copying only its opening lines. Preserve goals, concrete outcomes, file or state changes, later decisions, user corrections, constraint updates, and unfinished items. Target roughly 200 UTF-8 bytes while keeping every final decision or correction. Return plain summary text only, with no markdown fences or tool calls.";

const COMPRESSION_SYSTEM_PROMPT: &str = "Compress this historical session prefix in the same language as the source so future turns can continue without re-reading raw history. Synthesize the full history rather than copying prefixes: preserve goals, completed actions, verified state, active constraints, and especially later decisions or user corrections that supersede earlier assumptions. Target roughly 384 UTF-8 bytes and keep the output strictly shorter than the source. Return plain compressed context text only, with no markdown fences or tool calls.";

pub(crate) enum MaintenanceStreamOutcome {
    Completed {
        text: String,
        confirmed_tokens: Option<u64>,
    },
    Failed {
        confirmed_tokens: Option<u64>,
        reason: String,
    },
}

pub(crate) fn model_is_configured(config: &crate::RuntimeConfig, model: &cayw_ai::Model) -> bool {
    model.context_window.is_some()
        || (config.background_model_maintenance
            && !matches!(model.provider.as_str(), "unconfigured" | "fixture"))
}

pub(crate) fn schedule_mid_turn(inner: Weak<Inner>, session_id: &str) {
    schedule(inner, session_id, false);
}

pub(crate) fn schedule_post_turn(inner: Weak<Inner>, session_id: &str) {
    schedule(inner, session_id, true);
}

fn schedule(inner: Weak<Inner>, session_id: &str, post_turn: bool) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let Some(strong) = inner.upgrade() else {
        return;
    };
    if strong.maintenance_cancel.is_cancelled() {
        return;
    }
    let runtime = Runtime {
        inner: strong.clone(),
    };
    let (inspection, forgotten) = match collect_inspection(&runtime, session_id) {
        Ok(Some(pair)) => pair,
        Ok(None) => return,
        Err(error) => {
            crate::lock(&strong.context).record_error(session_id, error.to_string());
            return;
        }
    };
    {
        let selected = crate::lock(&strong.selection);
        if !model_is_configured(&strong.config, &selected.model) {
            return;
        }
    }
    let pending: Vec<PendingSummary> = inspection
        .pending_summaries
        .iter()
        .filter(|item| !overlaps_forgotten(item.start_cursor, item.end_cursor, &forgotten))
        .cloned()
        .collect();
    let unsummarized_bytes = pending
        .iter()
        .map(|item| item.input_bytes)
        .fold(0_u64, u64::saturating_add);
    let run_summaries = if post_turn {
        !pending.is_empty()
    } else {
        unsummarized_bytes >= SEGMENT_SUMMARY_TRIGGER_BYTES
            || inspection.input_tokens >= inspection.budget.compress_at
    };
    if run_summaries {
        for item in pending {
            let Some(unit) = inspection
                .units
                .iter()
                .find(|u| u.start_cursor == item.start_cursor && u.end_cursor == item.end_cursor)
                .cloned()
            else {
                continue;
            };
            let weak = Arc::downgrade(&strong);
            let cancel = strong.maintenance_cancel.child_token();
            let sid = session_id.to_owned();
            let budget = inspection.budget;
            let units_snapshot = inspection.units.clone();
            handle.spawn(async move {
                run_summary_job(
                    weak,
                    sid,
                    unit,
                    item.input_bytes,
                    budget,
                    units_snapshot,
                    cancel,
                )
                .await;
            });
        }
    }
    if !strong.config.main_session
        && let Some(job) = inspection.compression_job
    {
        let weak = Arc::downgrade(&strong);
        let cancel = strong.maintenance_cancel.child_token();
        let sid = session_id.to_owned();
        let budget = inspection.budget;
        let units = inspection.units;
        let summaries = inspection.summaries;
        let compressions = inspection.compressions;
        handle.spawn(async move {
            run_compression_job(
                weak,
                sid,
                job,
                budget,
                units,
                summaries,
                compressions,
                cancel,
            )
            .await;
        });
    }
}

fn collect_inspection(
    runtime: &Runtime,
    session_id: &str,
) -> Result<Option<(MaintenanceInspection, Vec<CursorRange>)>, crate::RuntimeError> {
    let messages = runtime.continuation_messages(session_id)?;
    let (projected, ranges) = runtime.projected_model_input_with_ranges(session_id, &messages)?;
    let (window, output) = {
        let selected = crate::lock(&runtime.inner.selection);
        (
            selected.model.context_window,
            selected
                .model
                .max_output_tokens
                .unwrap_or(cayw_context::DEFAULT_OUTPUT_RESERVE),
        )
    };
    let Some(inspection) = inspect_maintenance(
        &runtime.inner.context,
        session_id,
        &projected,
        &ranges,
        window,
        output,
    )?
    else {
        return Ok(None);
    };
    let forgotten = if runtime.inner.config.main_session {
        runtime.inner.memory.forgotten_cursor_ranges(session_id)?
    } else {
        Vec::new()
    };
    Ok(Some((inspection, forgotten)))
}

async fn run_summary_job(
    inner: Weak<Inner>,
    session_id: String,
    unit: SourceUnit,
    input_bytes: u64,
    budget: InputBudget,
    units_snapshot: Vec<SourceUnit>,
    cancel: CancellationToken,
) {
    let Some((reservation_id, budget_root, provider, mut model)) = reserve_maintenance(
        &inner,
        &session_id,
        ReservationSpec {
            kind: JobKind::Summary,
            start_cursor: unit.start_cursor,
            end_cursor: unit.end_cursor,
            input_bytes,
            margin: budget.margin,
            output_max: budget.output_reserve,
        },
        &cancel,
        |strong| {
            if strong.config.main_session {
                let forgotten = match strong.memory.forgotten_cursor_ranges(&session_id) {
                    Ok(forgotten) => forgotten,
                    Err(error) => {
                        crate::lock(&strong.context).record_error(&session_id, error.to_string());
                        return false;
                    }
                };
                if overlaps_forgotten(unit.start_cursor, unit.end_cursor, &forgotten) {
                    return false;
                }
            }
            let contexts = crate::lock(&strong.context);
            !contexts.summaries(&session_id).iter().any(|existing| {
                existing.start_cursor == unit.start_cursor && existing.end_cursor == unit.end_cursor
            })
        },
    )
    .await
    else {
        return;
    };

    model.max_output_tokens = Some(budget.output_reserve);
    let user_prompt = format!(
        "Source range: {session_id}:{}..{}\nKind: {:?}\n\n{}",
        unit.start_cursor, unit.end_cursor, unit.kind, unit.text
    );
    let outcome = consume_maintenance_stream(
        provider,
        model,
        SUMMARY_SYSTEM_PROMPT,
        &user_prompt,
        cancel.clone(),
    )
    .await;

    let Some(strong) = inner.upgrade() else {
        return;
    };
    settle_reservation(&strong, &budget_root, &session_id, reservation_id, &outcome).await;

    let text = match outcome {
        MaintenanceStreamOutcome::Completed { text, .. } => text,
        MaintenanceStreamOutcome::Failed { reason, .. } => {
            if !cancel.is_cancelled() {
                crate::lock(&strong.context)
                    .record_error(&session_id, format!("summary maintenance failed: {reason}"));
            }
            return;
        }
    };
    if strong.config.main_session {
        let forgotten = match strong.memory.forgotten_cursor_ranges(&session_id) {
            Ok(forgotten) => forgotten,
            Err(error) => {
                crate::lock(&strong.context).record_error(&session_id, error.to_string());
                return;
            }
        };
        if overlaps_forgotten(unit.start_cursor, unit.end_cursor, &forgotten) {
            return;
        }
    }
    let mut evidence = match log_id(&session_id, unit.start_cursor) {
        Ok(id) => vec![id],
        Err(error) => {
            crate::lock(&strong.context).record_error(&session_id, error.to_string());
            return;
        }
    };
    if unit.end_cursor != unit.start_cursor
        && let Ok(end_id) = log_id(&session_id, unit.end_cursor)
    {
        evidence.push(end_id);
    }
    let version = crate::lock(&strong.context)
        .summaries(&session_id)
        .iter()
        .filter(|s| s.start_cursor == unit.start_cursor && s.end_cursor == unit.end_cursor)
        .map(|s| s.version)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let record = SummaryRecord {
        summary_id: format!(
            "sum-{session_id}-{}-{}-v{version}",
            unit.start_cursor, unit.end_cursor
        ),
        version,
        session_id: session_id.clone(),
        kind: if unit.kind == UnitKind::ToolExchange {
            SummaryKind::Segment
        } else {
            SummaryKind::UserTurn
        },
        start_cursor: unit.start_cursor,
        end_cursor: unit.end_cursor,
        text,
        evidence,
        local_reuse: false,
    };
    if matches!(
        publish_summary(&strong.context, &session_id, record, Some(&units_snapshot)),
        Ok(VersionDecision::Accepted)
    ) {
        strong.changed.notify_waiters();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_compression_job(
    inner: Weak<Inner>,
    session_id: String,
    mut job: CompressionJob,
    mut budget: InputBudget,
    mut units: Vec<SourceUnit>,
    mut summaries: Vec<SummaryRecord>,
    mut compressions: Vec<CompressionRecord>,
    cancel: CancellationToken,
) {
    loop {
        let previous = compressions
            .iter()
            .filter(|record| {
                !record.text.is_empty()
                    && record.covered_through > 0
                    && record.covered_through < job.covered_through
                    && record.version > 0
            })
            .max_by_key(|record| (record.version, record.covered_through))
            .cloned();
        let covered_units: Vec<&SourceUnit> = units
            .iter()
            .filter(|unit| {
                matches!(unit.kind, UnitKind::CompletedTurn | UnitKind::ToolExchange)
                    && !unit.pin_original
                    && !unit.outcome_unknown
                    && unit.end_cursor <= job.covered_through
            })
            .collect();
        if covered_units.is_empty() {
            return;
        }
        let mut evidence = Vec::new();
        for unit in &covered_units {
            if let Ok(start_id) = log_id(&session_id, unit.start_cursor)
                && !evidence.contains(&start_id)
            {
                evidence.push(start_id);
            }
            if unit.end_cursor != unit.start_cursor
                && let Ok(end_id) = log_id(&session_id, unit.end_cursor)
                && !evidence.contains(&end_id)
            {
                evidence.push(end_id);
            }
        }
        if evidence.is_empty() {
            return;
        }

        let mut sections = Vec::new();
        if let Some(prev) = &previous {
            sections.push(format!(
                "Previous compressed history (through cursor {}):\n{}",
                prev.covered_through, prev.text
            ));
        }
        for unit in &covered_units {
            if previous
                .as_ref()
                .is_some_and(|prev| unit.end_cursor <= prev.covered_through)
            {
                continue;
            }
            if let Some(summary) = summaries
                .iter()
                .filter(|s| {
                    s.start_cursor == unit.start_cursor
                        && s.end_cursor == unit.end_cursor
                        && !s.local_reuse
                })
                .max_by_key(|s| s.version)
            {
                sections.push(format!(
                    "[Cursors {}..{} summary]\n{}",
                    unit.start_cursor, unit.end_cursor, summary.text
                ));
            } else {
                sections.push(format!(
                    "[Cursors {}..{} source]\n{}",
                    unit.start_cursor, unit.end_cursor, unit.text
                ));
            }
        }
        let user_prompt = sections.join("\n\n");
        let prompt_bytes = estimate_text(&user_prompt).tokens.max(job.input_bytes);
        let target_covered = job.covered_through;

        let Some((reservation_id, budget_root, provider, mut model)) = reserve_maintenance(
            &inner,
            &session_id,
            ReservationSpec {
                kind: JobKind::Compression,
                start_cursor: job.start_cursor,
                end_cursor: target_covered,
                input_bytes: prompt_bytes,
                margin: budget.margin,
                output_max: budget.output_reserve,
            },
            &cancel,
            |strong| {
                !crate::lock(&strong.context)
                    .compressions(&session_id)
                    .iter()
                    .any(|c| c.covered_through >= target_covered)
            },
        )
        .await
        else {
            return;
        };

        model.max_output_tokens = Some(budget.output_reserve);
        let outcome = consume_maintenance_stream(
            provider,
            model,
            COMPRESSION_SYSTEM_PROMPT,
            &user_prompt,
            cancel.clone(),
        )
        .await;

        let Some(strong) = inner.upgrade() else {
            return;
        };
        settle_reservation(&strong, &budget_root, &session_id, reservation_id, &outcome).await;

        match outcome {
            MaintenanceStreamOutcome::Completed { text, .. } => {
                let compressed_tokens = estimate_text(&text).tokens;
                if compressed_tokens > 0
                    && compressed_tokens < job.input_bytes
                    && compressed_tokens <= budget.replace_at
                {
                    let version = crate::lock(&strong.context)
                        .compressions(&session_id)
                        .iter()
                        .map(|record| record.version)
                        .max()
                        .unwrap_or(0)
                        .saturating_add(1);
                    let record = CompressionRecord {
                        compression_id: format!("comp-{session_id}-v{version}-{target_covered}"),
                        version,
                        session_id: session_id.clone(),
                        covered_through: target_covered,
                        text,
                        evidence,
                    };
                    if matches!(
                        publish_compression(&strong.context, &session_id, record),
                        Ok(VersionDecision::Accepted)
                    ) {
                        strong.changed.notify_waiters();
                    }
                } else {
                    crate::lock(&strong.context).record_error(
                        &session_id,
                        "compression output exceeded target budget or source length",
                    );
                }
            }
            MaintenanceStreamOutcome::Failed { reason, .. } => {
                if !cancel.is_cancelled() {
                    crate::lock(&strong.context).record_error(
                        &session_id,
                        format!("compression maintenance failed: {reason}"),
                    );
                }
            }
        }

        // If newer compressible turns finished while this single-session compression
        // reservation was active, advance once to the strictly newer cursor target.
        if strong.config.main_session || cancel.is_cancelled() {
            return;
        }
        let runtime = Runtime {
            inner: strong.clone(),
        };
        let inspection = match collect_inspection(&runtime, &session_id) {
            Ok(Some((inspection, _))) => inspection,
            Ok(None) => return,
            Err(error) => {
                crate::lock(&strong.context).record_error(&session_id, error.to_string());
                return;
            }
        };
        let Some(next_job) = inspection.compression_job else {
            return;
        };
        if next_job.covered_through <= target_covered {
            return;
        }
        job = next_job;
        budget = inspection.budget;
        units = inspection.units;
        summaries = inspection.summaries;
        compressions = inspection.compressions;
    }
}

pub(crate) async fn reserve_maintenance(
    inner: &Weak<Inner>,
    session_id: &str,
    spec: ReservationSpec,
    cancel: &CancellationToken,
    still_needed: impl FnOnce(&Inner) -> bool,
) -> Option<(u64, String, Arc<dyn Provider>, cayw_ai::Model)> {
    let strong = inner.upgrade()?;
    if cancel.is_cancelled() {
        return None;
    }
    let budget_root = match crate::model_requests::lineage_root(&strong.store, session_id) {
        Ok(root) => root,
        Err(error) => {
            crate::lock(&strong.context).record_error(session_id, error.to_string());
            return None;
        }
    };
    let lock = strong.budget_lock(&budget_root);
    let _guard = tokio::select! {
        biased;
        _ = cancel.cancelled() => return None,
        guard = lock.lock() => guard,
    };
    if cancel.is_cancelled() || !still_needed(&strong) {
        return None;
    }
    let reservation_id = {
        let mut contexts = crate::lock(&strong.context);
        let id = contexts.reserve(session_id, spec).ok()?;
        if let Err(error) = contexts.persist() {
            contexts.rollback_reservation(session_id, id);
            contexts.record_error(session_id, error.to_string());
            return None;
        }
        id
    };
    let (provider, model) = {
        let selected = crate::lock(&strong.selection);
        (selected.provider.clone(), selected.model.clone())
    };
    Some((reservation_id, budget_root, provider, model))
}

pub(crate) async fn settle_reservation(
    strong: &Inner,
    budget_root: &str,
    session_id: &str,
    reservation_id: u64,
    outcome: &MaintenanceStreamOutcome,
) {
    let lock = strong.budget_lock(budget_root);
    let _guard = lock.lock().await;
    let confirmed = match outcome {
        MaintenanceStreamOutcome::Completed {
            confirmed_tokens, ..
        }
        | MaintenanceStreamOutcome::Failed {
            confirmed_tokens, ..
        } => *confirmed_tokens,
    };
    let mut contexts = crate::lock(&strong.context);
    let step = match confirmed {
        Some(tokens) => contexts
            .reconcile(session_id, reservation_id, 1, tokens)
            .map(|_| ()),
        None => contexts.mark_unknown(session_id, reservation_id, 1),
    };
    if let Err(error) = step {
        contexts.record_error(session_id, error.to_string());
        return;
    }
    if let Err(error) = contexts.persist() {
        contexts.record_error(session_id, error.to_string());
    }
}

pub(crate) async fn consume_maintenance_stream(
    provider: Arc<dyn Provider>,
    model: cayw_ai::Model,
    system_prompt: &str,
    user_prompt: &str,
    cancel: CancellationToken,
) -> MaintenanceStreamOutcome {
    let options = cayw_ai::StreamOptions {
        max_tokens: model.max_output_tokens,
        ..Default::default()
    };
    let request = ModelRequest {
        model,
        messages: vec![
            Message::system(system_prompt.to_owned()),
            Message::user(user_prompt.to_owned()),
        ],
        options,
    };
    let mut stream = provider.stream(request, cancel.clone());
    let mut terminal_message = None;
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            item = stream.next() => item,
        };
        let Some(event) = next else {
            break;
        };
        match event {
            AssistantEvent::Done { message } | AssistantEvent::Error { message } => {
                terminal_message = Some(message);
                break;
            }
            _ => {}
        }
    }
    let Some(message) = terminal_message else {
        return MaintenanceStreamOutcome::Failed {
            confirmed_tokens: None,
            reason: if cancel.is_cancelled() {
                "maintenance stream cancelled".into()
            } else {
                "maintenance stream ended without terminal event".into()
            },
        };
    };
    let confirmed_tokens = if message.usage_status == UsageStatus::Reported {
        let total = if message.usage.total_tokens > 0 {
            message.usage.total_tokens
        } else {
            message.usage.input.saturating_add(message.usage.output)
        };
        Some(total)
    } else {
        None
    };
    if message.stop_reason == StopReason::Pending || message.stop_reason.is_failure() {
        let reason = message
            .error_message
            .clone()
            .unwrap_or_else(|| format!("stop_reason={:?}", message.stop_reason));
        return MaintenanceStreamOutcome::Failed {
            confirmed_tokens,
            reason,
        };
    }
    if !message.tool_calls().is_empty() {
        return MaintenanceStreamOutcome::Failed {
            confirmed_tokens,
            reason: "maintenance response unexpectedly contained tool calls".into(),
        };
    }
    let text = content_text(&message.content).trim().to_owned();
    if text.is_empty() {
        MaintenanceStreamOutcome::Failed {
            confirmed_tokens,
            reason: "maintenance response was empty".into(),
        }
    } else {
        MaintenanceStreamOutcome::Completed {
            text,
            confirmed_tokens,
        }
    }
}

pub(crate) fn overlaps_forgotten(start: u64, end: u64, forgotten: &[CursorRange]) -> bool {
    if start == 0 || end < start {
        return false;
    }
    forgotten
        .iter()
        .any(|item| item.start_cursor <= end && start <= item.end_cursor)
}
