//! Projection of Core facts into one trace [`Document`].
//!
//! The plugin never reads Core/Cells databases. It receives ordered
//! [`EventRecord`]s and turns three kinds of facts into a projection:
//!
//! * structured Host `trace_node` events, whose output is exactly one
//!   [`TraceEnvelope`] emitted by the actual Luau script;
//! * Core execution and capability records.
//!
//! Tool preflight events are kept as raw evidence only. They never create or
//! rebind executions because the requested operation may still be rejected.
//!
//! `TraceStore` owns session continuity. This module only guarantees an
//! idempotent projection and honest terminal states: unknown effects stay
//! unknown, absent observations stay gaps, and ordinary executions without a
//! manifest are recorded with their calls but can never be replayed.

use crate::Error;
use crate::types::*;
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityStage, CoreEvent, EventRecord,
    ExecutionOutcome, ExecutionRecord, ExecutionState,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Host event kind produced by the minimal Cells output callback.
pub const TRACE_NODE_KIND: &str = "trace_node";
const OBSERVED_PREFIX: &str = "observed:";
const MAX_METRICS_SAMPLES: usize = 1024;

/// Apply one ordered Core event to the projection.
///
/// The caller has already verified that this event is the next cursor in its
/// session. Applying the same event twice must not duplicate steps, calls or
/// revisions; conflicting repeats return [`Error::Conflict`].
pub fn apply(document: &mut Document, event: &EventRecord) -> Result<(), Error> {
    match &event.event {
        CoreEvent::ExecutionStarted { execution } => start_execution(document, event, execution),
        CoreEvent::ExecutionYielded { execution } => {
            check_event_lease(event, execution)?;
            if !matches!(
                execution.state,
                ExecutionState::Running | ExecutionState::Yielded
            ) {
                return Err(Error::Invalid("ExecutionYielded 携带了终态执行记录".into()));
            }
            let project = ensure_project(
                document,
                &execution.execution_id,
                &event.session_id,
                &execution.lease.attempt_id,
            )?;
            merge_record_identity(project, execution)?;
            if project.runtime_status == Status::Running {
                observed_eval_mut(project, event.timestamp).saved_at_ms = event.timestamp;
            }
            Ok(())
        }
        CoreEvent::ExecutionFinished { execution } => finish_execution(document, event, execution),
        CoreEvent::CapabilityPlanned { call } => {
            apply_capability(document, event, call, Some(CapabilityStage::Planned))
        }
        CoreEvent::CapabilityStarted { call } => {
            apply_capability(document, event, call, Some(CapabilityStage::Started))
        }
        CoreEvent::CapabilitySettled { call } => {
            apply_capability(document, event, call, Some(CapabilityStage::Settled))
        }
        // A reconciled capability is a durable terminal that replaced an
        // unknown one. Its full record carries the known outcome.
        CoreEvent::CapabilityReconciled { call, .. } => {
            apply_capability(document, event, call, None)
        }
        CoreEvent::Host { kind, data } if kind == TRACE_NODE_KIND => {
            // Trace payloads are script output, not authority over the Core
            // stream. Quarantine a malformed graph while retaining the event
            // and allowing subsequent authoritative events to be consumed.
            let original = data
                .get("execution_id")
                .and_then(Value::as_str)
                .and_then(|id| document.projects.get(id))
                .cloned();
            if let Err(error) = apply_trace_node(document, event, data)
                && let Some(mut project) =
                    original.filter(|project| project.session_id == event.session_id)
            {
                project.trace_error = Some(error.to_string());
                observed_eval_mut(&mut project, event.timestamp);
                add_gap(
                    &mut project,
                    None,
                    format!("节点追踪已停止：{error}；原始事件仍会保存"),
                );
                document.projects.insert(project.id.clone(), project);
            }
            Ok(())
        }
        CoreEvent::AttemptFinished { lease } => finish_attempt(document, event, &lease.attempt_id),
        CoreEvent::AttemptRecovered {
            executions,
            capabilities,
            ..
        } => {
            for execution in executions {
                if execution.terminal.is_some() {
                    finish_execution(document, event, execution)?;
                } else {
                    let project = ensure_project(
                        document,
                        &execution.execution_id,
                        &event.session_id,
                        &execution.lease.attempt_id,
                    )?;
                    merge_record_identity(project, execution)?;
                }
            }
            for call in capabilities {
                apply_capability(document, event, call, None)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Executions
// ---------------------------------------------------------------------------

fn start_execution(
    document: &mut Document,
    event: &EventRecord,
    execution: &ExecutionRecord,
) -> Result<(), Error> {
    check_event_lease(event, execution)?;
    let project = ensure_project(
        document,
        &execution.execution_id,
        &event.session_id,
        &execution.lease.attempt_id,
    )?;
    merge_record_identity(project, execution)?;
    if !matches!(
        execution.state,
        ExecutionState::Running | ExecutionState::Yielded
    ) {
        // A recovered terminal record may be the first fact observed.
        return finish_execution(document, event, execution);
    }
    if project.runtime_status != Status::Running {
        // Duplicate/late start after a terminal projection; keep the terminal.
        return Ok(());
    }
    project.runtime_status = Status::Running;
    let saved = event.timestamp;
    let evaluation = observed_eval_mut(project, saved);
    if !matches!(
        evaluation.status,
        Status::Completed | Status::Failed | Status::OutcomeUnknown
    ) {
        evaluation.status = Status::Running;
        evaluation.saved_at_ms = saved;
    }
    Ok(())
}

fn is_terminal_status(status: &Status) -> bool {
    matches!(
        status,
        Status::Completed | Status::Failed | Status::Blocked | Status::OutcomeUnknown
    )
}

/// Whether an already terminal project already records this exact terminal.
fn terminal_matches(project: &Project, status: &Status, error: Option<&str>) -> bool {
    if &project.runtime_status != status {
        return false;
    }
    let Some(evaluation) = observed_index(project).map(|index| &project.evaluations[index]) else {
        return false;
    };
    if let Some(error) = error {
        let in_step = evaluation
            .steps
            .iter()
            .any(|step| step.error.as_deref() == Some(error));
        let in_gap = evaluation
            .gaps
            .iter()
            .any(|gap| gap.message.contains(error));
        if !in_step && !in_gap {
            return false;
        }
    }
    match status {
        // A completed runtime may still carry a graph gap (for example a
        // missing completion fact); that does not make the terminal different.
        Status::Completed => matches!(evaluation.status, Status::Completed | Status::Blocked),
        other => &evaluation.status == other,
    }
}

fn finish_execution(
    document: &mut Document,
    event: &EventRecord,
    execution: &ExecutionRecord,
) -> Result<(), Error> {
    check_event_lease(event, execution)?;
    let (status, error) = terminal_facts(execution)?;
    let project = ensure_project(
        document,
        &execution.execution_id,
        &event.session_id,
        &execution.lease.attempt_id,
    )?;
    merge_record_identity(project, execution)?;
    if !matches!(
        execution.state,
        ExecutionState::Completed
            | ExecutionState::Failed
            | ExecutionState::Cancelled
            | ExecutionState::Interrupted
    ) {
        return Err(Error::Invalid(format!(
            "执行 {} 在非终态收到了 ExecutionFinished",
            execution.execution_id
        )));
    }

    // Terminal projection is final. A repeated delivery of the same terminal is
    // a no-op; a different terminal for the same run is a conflict.
    if is_terminal_status(&project.runtime_status) {
        if terminal_matches(project, &status, error.as_deref()) {
            return Ok(());
        }
        return Err(Error::Conflict(format!(
            "执行 {} 已经记录了不同的终态",
            execution.execution_id
        )));
    }

    let saved = event.timestamp;
    observed_eval_mut(project, saved);
    // A plain execution has no manifest and therefore no graph to rebuild.
    if project.revisions.is_empty() {
        add_gap(
            project,
            None,
            "本次执行没有 trace manifest；没有可构建或自动重算的节点图",
        );
        add_gap(
            project,
            None,
            "只记录了原始执行事实；节点输入、前置状态和输出未捕获",
        );
    }

    observed_eval_mut(project, saved);
    let index = observed_index(project).expect("observed evaluation was just ensured");
    let steps = &project.evaluations[index].steps;
    let running: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, step)| step.status == Status::Running)
        .map(|(step_index, _)| step_index)
        .collect();
    let failed = matches!(status, Status::Failed | Status::OutcomeUnknown);
    let unknown = status == Status::OutcomeUnknown;
    let mut missing_completions = project.trace_error.is_some();

    if failed {
        let message = error
            .clone()
            .unwrap_or_else(|| "执行没有记录错误文本".into());
        if let Some((&active, earlier)) = running.split_last() {
            let started_at = project.evaluations[index].steps[active].started_at_ms;
            let derived = event.timestamp.saturating_sub(started_at) as f64;
            {
                let step = &mut project.evaluations[index].steps[active];
                step.status = if unknown {
                    Status::OutcomeUnknown
                } else {
                    Status::Failed
                };
                step.error = Some(message.clone());
                step.elapsed_ms = derived;
                step.evidence_cursor = Some(event.cursor);
            }
            for &step_index in earlier {
                let node_id = project.evaluations[index].steps[step_index].node_id.clone();
                let step = &mut project.evaluations[index].steps[step_index];
                step.status = Status::Blocked;
                step.elapsed_ms = derived;
                add_gap(
                    project,
                    Some(&node_id),
                    format!("执行已失败；节点 {node_id} 没有自己的终态事实"),
                );
            }
        } else if project.evaluations[index].steps.is_empty() {
            add_gap(project, None, format!("执行在节点开始前失败：{message}"));
        } else {
            add_gap(
                project,
                None,
                format!("全部节点完成后执行仍然失败：{message}"),
            );
        }
    } else {
        for &step_index in &running {
            let node_id = project.evaluations[index].steps[step_index].node_id.clone();
            let started_at = project.evaluations[index].steps[step_index].started_at_ms;
            let derived = event.timestamp.saturating_sub(started_at) as f64;
            let step = &mut project.evaluations[index].steps[step_index];
            step.status = Status::Blocked;
            step.elapsed_ms = derived;
            missing_completions = true;
            add_gap(
                project,
                Some(&node_id),
                format!("执行已结束，但节点 {node_id} 缺少完成事实；不能当作成功"),
            );
        }
    }

    let has_graph = !project.revisions.is_empty();
    if !failed && has_graph && !missing_completions {
        let complete_path = project.evaluations[index].steps.last().is_some_and(|step| {
            step.status == Status::Completed
                && step.branch.is_none()
                && !project.revisions[0]
                    .plan
                    .edges
                    .iter()
                    .any(|edge| edge.from == step.node_id)
        });
        if !complete_path {
            missing_completions = true;
            add_gap(project, None, "执行已结束，但实际路径尚未到达计划的终点");
        }
    }
    let elapsed = total_elapsed(&project.evaluations[index]);
    // The observed graph output is the last node's actual output, matching what
    // a replay reports. Core's execution terminal only carries cell metadata, so
    // it is used as the result only for an ordinary run without a graph.
    let graph_output = if !missing_completions {
        project.evaluations[index]
            .steps
            .last()
            .and_then(|step| step.output.clone())
    } else {
        None
    };
    let terminal_output = match &execution.terminal {
        Some(ExecutionOutcome::Completed { result }) => Some(result.clone()),
        _ => None,
    };
    {
        let evaluation = &mut project.evaluations[index];
        evaluation.status = if failed {
            status.clone()
        } else if missing_completions {
            Status::Blocked
        } else {
            Status::Completed
        };
        evaluation.output = if status == Status::Completed {
            if has_graph {
                graph_output
            } else {
                terminal_output
            }
        } else {
            None
        };
        evaluation.elapsed_ms = elapsed;
        evaluation.saved_at_ms = saved;
    }
    project.active_node = None;
    // The runtime status records what actually happened. A graph gap stays in
    // the evaluation and gaps, not in the runtime terminal.
    project.runtime_status = status.clone();

    if failed {
        record_sample(
            &mut document.metrics.error_to_summary_ms,
            event_latency_ms(event),
        );
    }
    Ok(())
}

/// Map a terminal execution record to the projection status and error text.
/// The state/terminal pair is checked for consistency.
fn terminal_facts(execution: &ExecutionRecord) -> Result<(Status, Option<String>), Error> {
    let state = execution.state;
    match (&state, &execution.terminal) {
        (ExecutionState::Completed, Some(ExecutionOutcome::Completed { .. })) => {
            Ok((Status::Completed, None))
        }
        (ExecutionState::Completed, None) => Ok((Status::Completed, None)),
        (ExecutionState::Failed, Some(ExecutionOutcome::Failed { error })) => {
            Ok((Status::Failed, Some(error.clone())))
        }
        (ExecutionState::Failed, None) => {
            Ok((Status::Failed, Some("执行失败但没有记录错误文本".into())))
        }
        (ExecutionState::Cancelled, Some(ExecutionOutcome::Cancelled { reason })) => {
            Ok((Status::Failed, Some(reason.clone())))
        }
        (ExecutionState::Cancelled, _) => Ok((Status::Failed, Some("执行被取消".into()))),
        (ExecutionState::Interrupted, Some(ExecutionOutcome::Interrupted { reason })) => {
            Ok((Status::OutcomeUnknown, Some(reason.clone())))
        }
        (ExecutionState::Interrupted, _) => Ok((
            Status::OutcomeUnknown,
            Some("执行器已消失；不能确认原有侧效果".into()),
        )),
        (ExecutionState::Running | ExecutionState::Yielded, _) => Err(Error::Invalid(format!(
            "执行 {} 在非终态收到了 ExecutionFinished",
            execution.execution_id
        ))),
        (terminal_state, terminal) => Err(Error::Invalid(format!(
            "执行 {} 的状态 {terminal_state:?} 与终态 {terminal:?} 不一致",
            execution.execution_id
        ))),
    }
}

fn finish_attempt(
    document: &mut Document,
    event: &EventRecord,
    attempt_id: &str,
) -> Result<(), Error> {
    let saved = event.timestamp;
    for project in document.projects.values_mut() {
        if project.session_id != event.session_id
            || project.attempt_id != attempt_id
            || project.runtime_status != Status::Running
        {
            continue;
        }
        project.runtime_status = Status::OutcomeUnknown;
        project.active_node = None;
        observed_eval_mut(project, saved);
        let index = observed_index(project).expect("observed evaluation exists");
        let running = project.evaluations[index]
            .steps
            .iter()
            .rposition(|step| step.status == Status::Running);
        if let Some(step_index) = running {
            let started_at = project.evaluations[index].steps[step_index].started_at_ms;
            let derived = event.timestamp.saturating_sub(started_at) as f64;
            let step = &mut project.evaluations[index].steps[step_index];
            step.status = Status::OutcomeUnknown;
            step.error = Some("尝试在收到执行终态前结束；侧效果未知".into());
            step.elapsed_ms = derived;
            step.evidence_cursor = Some(event.cursor);
        }
        project.evaluations[index].status = Status::OutcomeUnknown;
        project.evaluations[index].output = None;
        project.evaluations[index].elapsed_ms = total_elapsed(&project.evaluations[index]);
        project.evaluations[index].saved_at_ms = saved;
        add_gap(
            project,
            None,
            "尝试在收到执行终态前结束；不能确认侧效果，也不会自动重放",
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

fn apply_capability(
    document: &mut Document,
    event: &EventRecord,
    call: &CapabilityCallRecord,
    expected: Option<CapabilityStage>,
) -> Result<(), Error> {
    if let Some(expected) = expected
        && call.stage != expected
    {
        return Err(Error::Invalid(format!(
            "能力调用 {}:{} 的事件阶段与记录阶段不一致",
            call.key.execution_id, call.key.sequence
        )));
    }
    match call.stage {
        CapabilityStage::Planned | CapabilityStage::Started => {
            if call.outcome.is_some() {
                return Err(Error::Invalid("未结算的能力调用不能带终态结果".into()));
            }
        }
        CapabilityStage::Settled => {
            if call.outcome.is_none() {
                return Err(Error::Invalid("已结算的能力调用必须带终态结果".into()));
            }
        }
    }

    let project = ensure_project(
        document,
        &call.key.execution_id,
        &event.session_id,
        &call.lease.attempt_id,
    )?;
    // Core records recovery and reconciliation after the original attempt.
    let reconciled = matches!(event.event, CoreEvent::CapabilityReconciled { .. });
    if reconciled
        && matches!(
            call.outcome,
            Some(CapabilityOutcome::OutcomeUnknown { .. }) | None
        )
    {
        return Err(Error::Invalid("核对后的能力调用必须有确定结果".into()));
    }
    let detached = reconciled || matches!(event.event, CoreEvent::AttemptRecovered { .. });
    if event.session_id != call.lease.session_id
        || (!detached && event.attempt.as_ref() != Some(&call.lease))
    {
        return Err(Error::Conflict("能力调用与事件的会话或尝试不一致".into()));
    }
    merge_lease(project, &call.lease)?;
    merge_field(&mut project.task_id, &call.task_id, "task")?;
    merge_field(&mut project.source_hash, &call.source_hash, "source_hash")?;
    merge_field(&mut project.attempt_id, &call.lease.attempt_id, "attempt")?;

    let started_at = (call.stage != CapabilityStage::Planned).then_some(event.timestamp);
    let settled_at = (call.stage == CapabilityStage::Settled).then_some(event.timestamp);
    let position = project.calls.iter().position(|record| {
        record.execution_id == call.key.execution_id && record.sequence == call.key.sequence
    });

    if let Some(position) = position {
        let current = &mut project.calls[position];
        if current.name != call.name || current.arguments != call.arguments {
            return Err(Error::Conflict(format!(
                "能力调用 {}:{} 的名称或参数与已记录事实不同",
                call.key.execution_id, call.key.sequence
            )));
        }
        if let (Some(recorded), Some(incoming)) = (&current.outcome, &call.outcome) {
            if recorded != incoming {
                let replaces_unknown =
                    reconciled && matches!(recorded, CapabilityOutcome::OutcomeUnknown { .. });
                if !replaces_unknown {
                    return Err(Error::Conflict(format!(
                        "能力调用 {}:{} 已经有了不同的终态",
                        call.key.execution_id, call.key.sequence
                    )));
                }
                current.outcome = Some(incoming.clone());
            }
        } else if current.outcome.is_none() {
            current.outcome = call.outcome.clone();
        }
        if current.started_at_ms.is_none() {
            current.started_at_ms = started_at;
        }
        if current.settled_at_ms.is_none() {
            current.settled_at_ms = settled_at;
        }
        current.evidence_cursor = event.cursor;
    } else {
        project.calls.push(RecordedCall {
            execution_id: call.key.execution_id.clone(),
            sequence: call.key.sequence,
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            outcome: call.outcome.clone(),
            started_at_ms: started_at,
            settled_at_ms: settled_at,
            evidence_cursor: event.cursor,
            node_id: project.active_node.clone(),
        });
        project.calls.sort_by_key(|record| record.sequence);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Host trace_node
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceNodeData {
    execution_id: String,
    parent_call_id: String,
    source_hash: String,
    task_id: String,
    entry: TraceEntry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceEntry {
    cursor: u64,
    output: Value,
}

fn apply_trace_node(
    document: &mut Document,
    event: &EventRecord,
    data: &Value,
) -> Result<(), Error> {
    let data: TraceNodeData = serde_json::from_value(data.clone())
        .map_err(|error| Error::Invalid(format!("trace_node 的数据不符合固定结构：{error}")))?;
    if data.execution_id.is_empty() || data.parent_call_id.is_empty() {
        return Err(Error::Invalid("trace_node 缺少执行标识或父调用标识".into()));
    }
    if data.task_id.is_empty() {
        return Err(Error::Invalid("trace_node 缺少任务标识".into()));
    }
    // Only a precise trace envelope with the shared schema is trace data.
    // Any other JSON output from the script is ordinary output and is ignored.
    let Some(value) = cell_json_value(&data.entry.output) else {
        return Ok(());
    };
    let Some(envelope) = parse_envelope(value)? else {
        return Ok(());
    };
    if data.entry.cursor == 0 {
        return Err(Error::Invalid("trace_node 的输出游标必须从 1 开始".into()));
    }
    let lease = event
        .attempt
        .as_ref()
        .ok_or_else(|| Error::Invalid("trace_node 缺少尝试标识".into()))?;
    if lease.session_id != event.session_id {
        return Err(Error::Conflict("trace_node 的会话标识不一致".into()));
    }
    {
        let project = document
            .projects
            .get_mut(&data.execution_id)
            .ok_or_else(|| Error::Invalid("节点输出没有对应的 Core 执行记录".into()))?;
        if project.trace_error.is_some() {
            return Ok(());
        }
        if project.session_id != event.session_id {
            return Err(Error::Conflict("节点输出不属于本会话".into()));
        }
        merge_lease(project, lease)?;
        merge_field(
            &mut project.parent_call_id,
            &data.parent_call_id,
            "parent_call_id",
        )?;
        merge_field(&mut project.task_id, &data.task_id, "task")?;
        merge_field(&mut project.source_hash, &data.source_hash, "source_hash")?;
        match envelope.trace {
            NodeTrace::Manifest { plan } => apply_manifest(project, &plan, event)?,
            NodeTrace::Started {
                node_id,
                input,
                state_before,
                context_before,
                seed,
            } => apply_started(
                project,
                event,
                &node_id,
                input,
                state_before,
                context_before,
                seed,
            )?,
            NodeTrace::Completed {
                node_id,
                result,
                elapsed_ms,
            } => apply_completed(project, event, &node_id, result, elapsed_ms)?,
        }
    }
    record_sample(
        &mut document.metrics.trace_to_store_ms,
        event_latency_ms(event),
    );
    Ok(())
}

/// Cells publishes an `OutputEntry` whose `output` is a tagged `CellOutput`.
/// Only the JSON payload can carry a trace envelope; text/image/audio/notify
/// outputs are ordinary output, never trace facts.
fn cell_json_value(output: &Value) -> Option<&Value> {
    match output {
        Value::Object(object) if object.contains_key("type") => {
            if object.get("type").and_then(Value::as_str) != Some("json") {
                return None;
            }
            object.get("value")
        }
        _ => Some(output),
    }
}

/// Parse only the exact trace schema. Other JSON is not trace.
fn parse_envelope(output: &Value) -> Result<Option<TraceEnvelope>, Error> {
    let Some(object) = output.as_object() else {
        return Ok(None);
    };
    match object.get("schema").and_then(Value::as_str) {
        None => Ok(None),
        Some(schema) if schema != TRACE_SCHEMA => Ok(None),
        Some(_) => serde_json::from_value(output.clone())
            .map(Some)
            .map_err(|error| Error::Invalid(format!("trace envelope 不符合固定结构：{error}"))),
    }
}

fn apply_manifest(
    project: &mut Project,
    plan: &GraphPlan,
    event: &EventRecord,
) -> Result<(), Error> {
    crate::engine::validate_plan(plan)?;
    let generated = crate::engine::script_for_plan(plan)?;
    if project.source_hash != script_digest(&generated) {
        return Err(Error::Conflict(
            "节点计划与 Core 记录的实际执行源码不一致".into(),
        ));
    }
    match project.revisions.first() {
        None => {
            project.revisions.push(Revision {
                version: 1,
                parent: None,
                changed_node: None,
                plan: plan.clone(),
                created_at_ms: event.timestamp,
                reason: "observed manifest".into(),
            });
        }
        Some(first) if first.version == 1 => {
            if &first.plan != plan {
                return Err(Error::Conflict(
                    "同一次执行收到了不同的 trace manifest".into(),
                ));
            }
        }
        Some(_) => {
            return Err(Error::Invalid(
                "trace manifest 必须对应第一个内容版本".into(),
            ));
        }
    }
    let saved = event.timestamp;
    // `observed_eval_mut` binds the observed run to the first (manifest)
    // revision as soon as that revision exists.
    let evaluation = observed_eval_mut(project, saved);
    evaluation.saved_at_ms = saved;
    Ok(())
}

fn apply_started(
    project: &mut Project,
    event: &EventRecord,
    node_id: &str,
    input: Value,
    state_before: Value,
    context_before: Value,
    seed: Option<Value>,
) -> Result<(), Error> {
    if project.revisions.is_empty() {
        add_gap(
            project,
            None,
            "收到节点 trace 但没有 manifest；缺少计划，无法校验该节点",
        );
        return Ok(());
    }
    if is_terminal_status(&project.runtime_status) {
        // A replay of the exact recorded start is harmless; a new or changed
        // start after the terminal must not add a Running step.
        let duplicate = observed_index(project).is_some_and(|index| {
            project.evaluations[index].steps.iter().any(|step| {
                step.node_id == node_id
                    && matches!(step.status, Status::Running | Status::Completed)
                    && step.input == input
                    && step.state_before == state_before
            })
        });
        if duplicate {
            return Ok(());
        }
        return Err(Error::Conflict(format!(
            "执行已结束，不能再记录节点 {node_id} 的启动事实"
        )));
    }
    ensure_node_in_plan(project, node_id)?;
    if let Some(evaluation) = observed_index(project).map(|index| &project.evaluations[index]) {
        let duplicate = evaluation.steps.iter().any(|step| {
            step.node_id == node_id
                && matches!(step.status, Status::Running | Status::Completed)
                && step.input == input
                && step.state_before == state_before
        });
        if !duplicate {
            let plan = &project.revisions[0].plan;
            let (expected, expected_input, expected_state) = match evaluation.steps.last() {
                None => (Some(plan.entry.as_str()), &plan.input, &plan.initial_state),
                Some(previous) if previous.status == Status::Completed => {
                    let target = plan
                        .edges
                        .iter()
                        .find(|edge| {
                            edge.from == previous.node_id && edge.branch == previous.branch
                        })
                        .map(|edge| edge.to.as_str());
                    (
                        target,
                        previous.output.as_ref().expect("completed output"),
                        previous.state_after.as_ref().expect("completed state"),
                    )
                }
                Some(_) => {
                    return Err(Error::Conflict("前一节点尚未结束，不能开始下一节点".into()));
                }
            };
            if expected != Some(node_id)
                || expected_input != &input
                || expected_state != &state_before
            {
                return Err(Error::Conflict(
                    "节点启动与计划分支、前一输出或进入前状态不一致".into(),
                ));
            }
        }
    }
    let saved = event.timestamp;
    let evaluation = observed_eval_mut(project, saved);
    if let Some(index) = evaluation
        .steps
        .iter()
        .rposition(|step| step.node_id == node_id)
    {
        let step = &evaluation.steps[index];
        // The same start fact may be projected again after its completion was
        // already recorded. Identical snapshots stay a no-op.
        if matches!(step.status, Status::Running | Status::Completed)
            && step.input == input
            && step.state_before == state_before
        {
            evaluation.saved_at_ms = saved;
            return Ok(());
        }
        return Err(Error::Conflict(format!(
            "节点 {node_id} 重复启动，或启动事实与已记录内容不一致"
        )));
    }
    evaluation.steps.push(Step {
        node_id: node_id.to_string(),
        status: Status::Running,
        input,
        state_before,
        context_before,
        seed,
        output: None,
        state_after: None,
        branch: None,
        error: None,
        // Intermediate execution/yield events must not change this boundary.
        started_at_ms: saved,
        elapsed_ms: 0.0,
        evidence_cursor: Some(event.cursor),
        reused: false,
    });
    evaluation.saved_at_ms = saved;
    project.active_node = Some(node_id.to_string());
    Ok(())
}

fn apply_completed(
    project: &mut Project,
    event: &EventRecord,
    node_id: &str,
    result: NodeResult,
    elapsed_ms: f64,
) -> Result<(), Error> {
    if !elapsed_ms.is_finite() || elapsed_ms < 0.0 {
        return Err(Error::Invalid(format!(
            "节点 {node_id} 的耗时不是有效的非负数"
        )));
    }
    if project.revisions.is_empty() {
        add_gap(
            project,
            None,
            "收到节点 trace 但没有 manifest；缺少计划，无法校验该节点",
        );
        return Ok(());
    }
    if is_terminal_status(&project.runtime_status) {
        // Only an exact replay of a recorded completion is accepted; a late
        // completion must not rewrite a Failed/Blocked/unknown terminal.
        let duplicate = observed_index(project).is_some_and(|index| {
            project.evaluations[index].steps.iter().any(|step| {
                step.node_id == node_id
                    && step.status == Status::Completed
                    && step.output.as_ref() == Some(&result.output)
                    && step.state_after.as_ref() == Some(&result.state)
                    && step.branch == result.branch
            })
        });
        if duplicate {
            return Ok(());
        }
        return Err(Error::Conflict(format!(
            "执行已结束，不能再记录节点 {node_id} 的完成事实"
        )));
    }
    ensure_node_in_plan(project, node_id)?;
    let saved = event.timestamp;
    let evaluation = observed_eval_mut(project, saved);
    let running = evaluation
        .steps
        .iter()
        .rposition(|step| step.node_id == node_id && step.status == Status::Running);
    match running {
        Some(index) => {
            let derived = event
                .timestamp
                .saturating_sub(evaluation.steps[index].started_at_ms)
                as f64;
            let step = &mut evaluation.steps[index];
            step.status = Status::Completed;
            step.output = Some(result.output.clone());
            step.state_after = Some(result.state.clone());
            step.branch = result.branch.clone();
            step.elapsed_ms = if elapsed_ms > 0.0 {
                elapsed_ms
            } else {
                derived
            };
            step.evidence_cursor = Some(event.cursor);
        }
        None => {
            if let Some(index) = evaluation
                .steps
                .iter()
                .rposition(|step| step.node_id == node_id)
            {
                let step = &evaluation.steps[index];
                if step.status == Status::Completed
                    && step.output.as_ref() == Some(&result.output)
                    && step.state_after.as_ref() == Some(&result.state)
                    && step.branch == result.branch
                {
                    return Ok(());
                }
                return Err(Error::Conflict(format!(
                    "节点 {node_id} 的完成事实与已记录内容不一致"
                )));
            }
            return Err(Error::Conflict(format!(
                "节点 {node_id} 的完成事实没有对应的启动"
            )));
        }
    }
    // Per-node output is available immediately; the final graph output is
    // assigned only after Core confirms that the execution completed.
    evaluation.output = None;
    evaluation.elapsed_ms = total_elapsed(evaluation);
    evaluation.saved_at_ms = saved;
    if project.active_node.as_deref() == Some(node_id) {
        project.active_node = None;
    }
    Ok(())
}

fn ensure_node_in_plan(project: &Project, node_id: &str) -> Result<(), Error> {
    let Some(revision) = project.revisions.first() else {
        return Ok(());
    };
    if revision.plan.nodes.iter().any(|node| node.id == node_id) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "trace 节点 {node_id} 不在已记录的 manifest 中"
        )))
    }
}

fn script_digest(script: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(script.as_bytes()))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn ensure_project<'a>(
    document: &'a mut Document,
    execution_id: &str,
    session_id: &str,
    attempt_id: &str,
) -> Result<&'a mut Project, Error> {
    if execution_id.is_empty() || execution_id.len() > cayw_core::MAX_IDENTIFIER_BYTES {
        return Err(Error::Invalid("执行标识为空或过长".into()));
    }
    let project = document
        .projects
        .entry(execution_id.to_string())
        .or_insert_with(|| Project {
            id: execution_id.to_string(),
            session_id: String::new(),
            attempt_id: String::new(),
            task_id: String::new(),
            source_hash: String::new(),
            parent_call_id: String::new(),
            lease: None,
            revisions: Vec::new(),
            evaluations: Vec::new(),
            calls: Vec::new(),
            gaps: Vec::new(),
            active_node: None,
            trace_error: None,
            runtime_status: Status::Running,
            symbol_hangs: Default::default(),
        });
    merge_field(&mut project.session_id, session_id, "session")?;
    merge_field(&mut project.attempt_id, attempt_id, "attempt")?;
    Ok(project)
}

fn merge_record_identity(project: &mut Project, execution: &ExecutionRecord) -> Result<(), Error> {
    merge_lease(project, &execution.lease)?;
    merge_field(
        &mut project.parent_call_id,
        &execution.parent_call_id,
        "parent_call_id",
    )?;
    merge_field(
        &mut project.attempt_id,
        &execution.lease.attempt_id,
        "attempt",
    )?;
    merge_field(&mut project.task_id, &execution.task_id, "task")?;
    merge_field(
        &mut project.source_hash,
        &execution.source_hash,
        "source_hash",
    )
}

/// Reject an execution fact delivered inside another session or attempt. The
/// project is keyed by execution id; identity fields must never silently merge
/// across runs. `AttemptRecovered` intentionally lists terminal records from
/// earlier attempts in the same session, so it is exempt from the attempt check.
fn check_event_lease(event: &EventRecord, execution: &ExecutionRecord) -> Result<(), Error> {
    if event.session_id != execution.lease.session_id {
        return Err(Error::Conflict(format!(
            "执行 {} 不属于事件所在的会话",
            execution.execution_id
        )));
    }
    let recovered = matches!(event.event, CoreEvent::AttemptRecovered { .. });
    if !recovered
        && let Some(attempt) = &event.attempt
        && attempt != &execution.lease
    {
        return Err(Error::Conflict(format!(
            "执行 {} 不属于事件所在的尝试",
            execution.execution_id
        )));
    }
    Ok(())
}

fn merge_lease(project: &mut Project, lease: &cayw_core::AttemptLease) -> Result<(), Error> {
    match &project.lease {
        Some(recorded) if recorded != lease => {
            Err(Error::Conflict("执行的完整尝试标识发生变化".into()))
        }
        Some(_) => Ok(()),
        None => {
            project.lease = Some(lease.clone());
            Ok(())
        }
    }
}

fn merge_field(field: &mut String, incoming: &str, name: &str) -> Result<(), Error> {
    if incoming.is_empty() {
        return Ok(());
    }
    if field.is_empty() {
        *field = incoming.to_string();
        return Ok(());
    }
    if field == incoming {
        return Ok(());
    }
    Err(Error::Conflict(format!(
        "同一次执行出现了不同的 {name} 事实"
    )))
}

fn observed_index(project: &Project) -> Option<usize> {
    let id = format!("{OBSERVED_PREFIX}{}", project.id);
    project
        .evaluations
        .iter()
        .position(|evaluation| evaluation.id == id)
}

/// Return the single observed evaluation, creating it on first fact. Its
/// version follows only the manifest revision; edits and replays never move
/// the observed run to another version.
fn observed_eval_mut(project: &mut Project, at_ms: u64) -> &mut Evaluation {
    let id = format!("{OBSERVED_PREFIX}{}", project.id);
    let version = project
        .revisions
        .first()
        .map_or(0, |revision| revision.version);
    let index = match project
        .evaluations
        .iter()
        .position(|evaluation| evaluation.id == id)
    {
        Some(index) => index,
        None => {
            project.evaluations.push(Evaluation {
                id,
                version,
                mode: "observed".into(),
                status: Status::Running,
                steps: Vec::new(),
                output: None,
                gaps: Vec::new(),
                elapsed_ms: 0.0,
                saved_at_ms: at_ms,
                executed_nodes: Vec::new(),
            });
            project.evaluations.len() - 1
        }
    };
    let evaluation = &mut project.evaluations[index];
    if evaluation.version == 0 && version != 0 {
        evaluation.version = version;
    }
    evaluation
}

fn add_gap(project: &mut Project, node_id: Option<&str>, message: impl Into<String>) {
    let message = message.into();
    let gap = Gap {
        node_id: node_id.map(str::to_string),
        message: message.clone(),
    };
    if !project
        .gaps
        .iter()
        .any(|existing| existing.node_id.as_deref() == node_id && existing.message == message)
    {
        project.gaps.push(gap.clone());
    }
    let id = format!("{OBSERVED_PREFIX}{}", project.id);
    if let Some(evaluation) = project
        .evaluations
        .iter_mut()
        .find(|evaluation| evaluation.id == id)
        && !evaluation
            .gaps
            .iter()
            .any(|existing| existing.node_id.as_deref() == node_id && existing.message == message)
    {
        evaluation.gaps.push(gap);
    }
}

fn total_elapsed(evaluation: &Evaluation) -> f64 {
    evaluation.steps.iter().map(|step| step.elapsed_ms).sum()
}

fn event_latency_ms(event: &EventRecord) -> f64 {
    now_ms().saturating_sub(event.timestamp) as f64
}

fn record_sample(samples: &mut Vec<f64>, sample: f64) {
    if samples.len() < MAX_METRICS_SAMPLES {
        samples.push(sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_agent::AgentEvent;
    use cayw_core::{AttemptLease, CapabilityCallKey};
    use serde_json::json;

    fn lease() -> AttemptLease {
        AttemptLease {
            session_id: "s1".into(),
            attempt_id: "a1".into(),
            worker_id: "w1".into(),
            fence: 1,
        }
    }

    fn record(cursor: u64, event: CoreEvent) -> EventRecord {
        EventRecord {
            protocol_version: cayw_core::PROTOCOL_VERSION,
            session_id: "s1".into(),
            cursor,
            timestamp: 1_000 + cursor,
            attempt: Some(lease()),
            event,
        }
    }

    fn execution(state: ExecutionState, terminal: Option<ExecutionOutcome>) -> ExecutionRecord {
        ExecutionRecord {
            execution_id: "e1".into(),
            parent_call_id: "call1".into(),
            task_id: "t1".into(),
            source_hash: script_digest(&crate::engine::script_for_plan(&plan()).unwrap()),
            lease: lease(),
            state,
            terminal,
        }
    }

    fn plan() -> GraphPlan {
        GraphPlan {
            id: "p1".into(),
            label: "plan".into(),
            entry: "a".into(),
            nodes: vec![
                Node {
                    id: "a".into(),
                    label: "A".into(),
                    module: "m".into(),
                    source: "return {output=1,state=state}".into(),
                    params: json!({}),
                    input_pointer: String::new(),
                    implementation: "luau".into(),
                },
                Node {
                    id: "b".into(),
                    label: "B".into(),
                    module: "m".into(),
                    source: "return {output=2,state=state}".into(),
                    params: json!({}),
                    input_pointer: String::new(),
                    implementation: "luau".into(),
                },
            ],
            edges: vec![Edge {
                from: "a".into(),
                to: "b".into(),
                branch: None,
            }],
            input: json!({"value": 1}),
            initial_state: json!({"visited": 0}),
            context: json!({}),
        }
    }

    fn envelope(trace: NodeTrace) -> Value {
        json!({"schema": TRACE_SCHEMA, "trace": trace})
    }

    fn host(cursor: u64, output: Value) -> EventRecord {
        record(
            cursor,
            CoreEvent::Host {
                kind: TRACE_NODE_KIND.into(),
                data: json!({
                    "execution_id": "e1",
                    "parent_call_id": "call1",
                    "source_hash": script_digest(&crate::engine::script_for_plan(&plan()).unwrap()),
                    "task_id": "t1",
                    // The real Host payload serializes the Cells OutputEntry.
                    "entry": {"cursor": cursor, "output": {"type": "json", "value": output}},
                }),
            },
        )
    }

    fn start_and_manifest(document: &mut Document) {
        apply(
            document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        apply(
            document,
            &host(2, envelope(NodeTrace::Manifest { plan: plan() })),
        )
        .unwrap();
    }

    #[test]
    fn observed_run_projects_steps_and_terminal_output() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &host(2, envelope(NodeTrace::Manifest { plan: plan() })),
        )
        .unwrap();
        apply(
            &mut document,
            &host(
                3,
                envelope(NodeTrace::Started {
                    node_id: "a".into(),
                    input: plan().input,
                    state_before: json!({"visited": 0}),

                    context_before: Value::Null,
                    seed: None,
                }),
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &host(
                4,
                envelope(NodeTrace::Completed {
                    node_id: "a".into(),
                    result: NodeResult {
                        output: json!({"value": 2}),
                        state: json!({"visited": 1}),
                        branch: None,
                    },
                    elapsed_ms: 0.0,
                }),
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &host(
                5,
                envelope(NodeTrace::Started {
                    node_id: "b".into(),
                    input: json!({"value":2}),
                    state_before: json!({"visited":1}),

                    context_before: Value::Null,
                    seed: None,
                }),
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &host(
                6,
                envelope(NodeTrace::Completed {
                    node_id: "b".into(),
                    result: NodeResult {
                        output: json!({"value":3}),
                        state: json!({"visited":2}),
                        branch: None,
                    },
                    elapsed_ms: 0.0,
                }),
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &record(
                7,
                CoreEvent::ExecutionFinished {
                    execution: execution(
                        ExecutionState::Completed,
                        Some(ExecutionOutcome::Completed {
                            result: json!({"final": true}),
                        }),
                    ),
                },
            ),
        )
        .unwrap();

        let project = document.projects.get("e1").unwrap();
        assert_eq!(project.version(), 1);
        assert_eq!(project.runtime_status, Status::Completed);
        let evaluation = project.latest().unwrap();
        assert_eq!(evaluation.id, "observed:e1");
        assert_eq!(evaluation.mode, "observed");
        assert_eq!(evaluation.version, 1);
        assert_eq!(evaluation.status, Status::Completed);
        assert_eq!(evaluation.steps.len(), 2);
        assert_eq!(evaluation.steps[0].output, Some(json!({"value": 2})));
        assert_eq!(evaluation.steps[0].state_after, Some(json!({"visited": 1})));
        // The script reports 0; capture derives the duration from the two
        // actual event timestamps (1003 -> 1004).
        assert_eq!(evaluation.steps[0].elapsed_ms, 1.0);
        // Core's terminal result is cell metadata; the observed graph output is
        // the last completed node's actual output.
        assert_eq!(evaluation.output, Some(json!({"value": 3})));
        assert!(evaluation.gaps.is_empty());
        assert!(document.metrics.trace_to_store_ms.len() >= 3);
    }

    #[test]
    fn replayed_events_are_idempotent() {
        let mut document = Document::default();
        let events = [
            record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
            host(2, envelope(NodeTrace::Manifest { plan: plan() })),
            host(
                3,
                envelope(NodeTrace::Started {
                    node_id: "a".into(),
                    input: plan().input,
                    state_before: json!({"visited": 0}),

                    context_before: Value::Null,
                    seed: None,
                }),
            ),
            host(
                4,
                envelope(NodeTrace::Completed {
                    node_id: "a".into(),
                    result: NodeResult {
                        output: json!({"value": 2}),
                        state: json!({"visited": 1}),
                        branch: None,
                    },
                    elapsed_ms: 0.0,
                }),
            ),
        ];
        for event in &events {
            apply(&mut document, event).unwrap();
        }
        let first = document.projects.get("e1").unwrap().clone();
        for event in &events {
            apply(&mut document, event).unwrap();
        }
        let project = document.projects.get("e1").unwrap();
        assert_eq!(project.revisions.len(), 1);
        assert_eq!(project.evaluations.len(), 1);
        assert_eq!(project.evaluations[0].steps.len(), 1);
        assert_eq!(project.version(), first.version());
        assert_eq!(project.gaps, first.gaps);
    }

    #[test]
    fn ordinary_execution_without_manifest_has_no_graph_claim() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &record(
                2,
                CoreEvent::CapabilitySettled {
                    call: CapabilityCallRecord {
                        key: CapabilityCallKey {
                            execution_id: "e1".into(),
                            sequence: 0,
                        },
                        name: "fs.read".into(),
                        arguments: json!({"path": "input.json"}),
                        parent_call_id: "call1".into(),
                        task_id: "t1".into(),
                        source_hash: script_digest(
                            &crate::engine::script_for_plan(&plan()).unwrap(),
                        ),
                        lease: lease(),
                        stage: CapabilityStage::Settled,
                        authorization: None,
                        outcome: Some(cayw_core::CapabilityOutcome::Completed {
                            result: json!({"content": "x"}),
                        }),
                        external_provenance: None,
                    },
                },
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &record(
                3,
                CoreEvent::ExecutionFinished {
                    execution: execution(ExecutionState::Completed, None),
                },
            ),
        )
        .unwrap();

        let project = document.projects.get("e1").unwrap();
        assert!(project.revisions.is_empty());
        assert_eq!(project.calls.len(), 1);
        assert_eq!(project.calls[0].name, "fs.read");
        assert_eq!(project.runtime_status, Status::Completed);
        let evaluation = project.latest().unwrap();
        assert_eq!(evaluation.status, Status::Completed);
        assert!(evaluation.steps.is_empty());
        assert!(
            project
                .gaps
                .iter()
                .any(|gap| gap.message.contains("manifest"))
        );
    }

    #[test]
    fn failed_execution_marks_active_step_with_error_evidence() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &host(2, envelope(NodeTrace::Manifest { plan: plan() })),
        )
        .unwrap();
        apply(
            &mut document,
            &host(
                3,
                envelope(NodeTrace::Started {
                    node_id: "a".into(),
                    input: plan().input,
                    state_before: plan().initial_state,

                    context_before: Value::Null,
                    seed: None,
                }),
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &record(
                4,
                CoreEvent::ExecutionFinished {
                    execution: execution(
                        ExecutionState::Failed,
                        Some(ExecutionOutcome::Failed {
                            error: "input.amount is nil".into(),
                        }),
                    ),
                },
            ),
        )
        .unwrap();

        let project = document.projects.get("e1").unwrap();
        assert_eq!(project.runtime_status, Status::Failed);
        let evaluation = project.latest().unwrap();
        assert_eq!(evaluation.status, Status::Failed);
        assert_eq!(evaluation.steps[0].status, Status::Failed);
        assert_eq!(
            evaluation.steps[0].error.as_deref(),
            Some("input.amount is nil")
        );
        assert_eq!(evaluation.steps[0].evidence_cursor, Some(4));
        assert_eq!(document.metrics.error_to_summary_ms.len(), 1);
    }

    #[test]
    fn non_trace_output_is_ignored() {
        let mut document = Document::default();
        apply(
            &mut document,
            &host(1, json!({"schema": "other.v1", "value": 1})),
        )
        .unwrap();
        apply(&mut document, &host(2, json!({"plain": "output"}))).unwrap();
        // A text output is never trace, even when its text mentions the schema.
        apply(
            &mut document,
            &record(
                3,
                CoreEvent::Host {
                    kind: TRACE_NODE_KIND.into(),
                    data: json!({
                        "execution_id": "e1",
                        "parent_call_id": "call1",
                        "source_hash": script_digest(&crate::engine::script_for_plan(&plan()).unwrap()),
                        "task_id": "t1",
                        "entry": {"cursor": 3,
                            "output": {"type": "text",
                                "text": "{\"schema\":\"cayw.trace.v1\"}"}},
                    }),
                },
            ),
        )
        .unwrap();
        assert!(document.projects.is_empty());
        assert!(document.metrics.trace_to_store_ms.is_empty());
    }

    #[test]
    fn preflight_wait_does_not_create_or_rebind_executions() {
        let mut document = Document::default();
        let wait = |cell_id: &str| {
            record(
                2,
                CoreEvent::Agent {
                    event: Box::new(AgentEvent::ToolExecutionStart {
                        call: cayw_ai::ToolCall::new(
                            "wait",
                            "execute_code",
                            json!({"action":"wait","cell_id":cell_id}),
                        ),
                    }),
                },
            )
        };
        apply(&mut document, &wait("missing")).unwrap();
        assert!(document.projects.is_empty());
        start_and_manifest(&mut document);
        let mut other_attempt = wait("e1");
        other_attempt.attempt.as_mut().unwrap().attempt_id = "a2".into();
        apply(&mut document, &other_attempt).unwrap();
        assert_eq!(document.projects["e1"].attempt_id, "a1");
        assert!(document.projects["e1"].trace_error.is_none());
    }

    #[test]
    fn node_duration_survives_yield_and_wait_events() {
        for failed in [false, true] {
            let mut document = Document::default();
            start_and_manifest(&mut document);
            apply(
                &mut document,
                &host(
                    3,
                    envelope(NodeTrace::Started {
                        node_id: "a".into(),
                        input: plan().input,
                        state_before: plan().initial_state,

                        context_before: Value::Null,
                        seed: None,
                    }),
                ),
            )
            .unwrap();
            apply(
                &mut document,
                &record(
                    4,
                    CoreEvent::ExecutionYielded {
                        execution: execution(ExecutionState::Yielded, None),
                    },
                ),
            )
            .unwrap();
            if failed {
                apply(
                    &mut document,
                    &record(
                        5,
                        CoreEvent::ExecutionFinished {
                            execution: execution(
                                ExecutionState::Failed,
                                Some(ExecutionOutcome::Failed {
                                    error: "failed after yielding".into(),
                                }),
                            ),
                        },
                    ),
                )
                .unwrap();
            } else {
                apply(
                    &mut document,
                    &host(
                        5,
                        envelope(NodeTrace::Completed {
                            node_id: "a".into(),
                            result: NodeResult {
                                output: json!(2),
                                state: json!({}),
                                branch: None,
                            },
                            elapsed_ms: 0.0,
                        }),
                    ),
                )
                .unwrap();
            }
            let step = &document.projects["e1"].latest().unwrap().steps[0];
            assert_eq!(step.started_at_ms, 1003);
            assert_eq!(step.elapsed_ms, 2.0);
        }
    }

    #[test]
    fn invalid_node_path_is_a_gap_and_does_not_block_core_terminal() {
        let mut document = Document::default();
        start_and_manifest(&mut document);
        apply(
            &mut document,
            &host(
                3,
                envelope(NodeTrace::Started {
                    node_id: "b".into(),
                    input: plan().input,
                    state_before: plan().initial_state,

                    context_before: Value::Null,
                    seed: None,
                }),
            ),
        )
        .unwrap();
        assert!(document.projects["e1"].trace_error.is_some());
        assert!(document.projects["e1"].latest().unwrap().steps.is_empty());
        apply(
            &mut document,
            &record(
                4,
                CoreEvent::ExecutionFinished {
                    execution: execution(
                        ExecutionState::Completed,
                        Some(ExecutionOutcome::Completed { result: json!(1) }),
                    ),
                },
            ),
        )
        .unwrap();
        assert_eq!(document.projects["e1"].runtime_status, Status::Completed);
        assert_eq!(
            document.projects["e1"].latest().unwrap().status,
            Status::Blocked
        );
        assert!(document.projects["e1"].latest().unwrap().output.is_none());
    }

    #[test]
    fn recovered_terminal_without_manifest_preserves_evaluation_gaps() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionFinished {
                    execution: execution(ExecutionState::Completed, None),
                },
            ),
        )
        .unwrap();
        assert_eq!(document.projects["e1"].latest().unwrap().gaps.len(), 2);
    }

    #[test]
    fn different_manifest_for_same_execution_conflicts() {
        let mut document = Document::default();
        start_and_manifest(&mut document);
        let mut changed = plan();
        changed.label = "changed".into();
        apply(
            &mut document,
            &host(3, envelope(NodeTrace::Manifest { plan: changed })),
        )
        .unwrap();
        let project = &document.projects["e1"];
        assert_eq!(project.revisions[0].plan, plan());
        assert!(
            project
                .trace_error
                .as_deref()
                .unwrap()
                .contains("实际执行源码")
        );
    }

    #[test]
    fn completion_without_start_quarantines_only_the_graph() {
        let mut document = Document::default();
        start_and_manifest(&mut document);
        apply(
            &mut document,
            &host(
                2,
                envelope(NodeTrace::Completed {
                    node_id: "a".into(),
                    result: NodeResult {
                        output: json!(1),
                        state: json!({}),
                        branch: None,
                    },
                    elapsed_ms: 1.0,
                }),
            ),
        )
        .unwrap();
        assert!(
            document.projects["e1"]
                .trace_error
                .as_deref()
                .unwrap()
                .contains("没有对应的启动")
        );
        assert!(document.projects["e1"].latest().unwrap().steps.is_empty());
    }

    #[test]
    fn terminal_interrupted_stays_unknown() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &record(
                2,
                CoreEvent::ExecutionFinished {
                    execution: execution(
                        ExecutionState::Interrupted,
                        Some(ExecutionOutcome::Interrupted {
                            reason: "VM gone".into(),
                        }),
                    ),
                },
            ),
        )
        .unwrap();
        let project = document.projects.get("e1").unwrap();
        assert_eq!(project.runtime_status, Status::OutcomeUnknown);
        assert_eq!(project.latest().unwrap().status, Status::OutcomeUnknown);
    }

    #[test]
    fn tool_start_wait_attaches_to_known_execution_and_never_resets_terminal() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        let wait = EventRecord {
            event: CoreEvent::Agent {
                event: Box::new(AgentEvent::ToolExecutionStart {
                    call: cayw_ai::ToolCall::new(
                        "call2",
                        "execute_code",
                        json!({"action": "wait", "cell_id": "e1", "cursor": 0,
                            "yield_time_ms": 0, "max_output_tokens": 1024}),
                    ),
                }),
            },
            ..record(
                2,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            )
        };
        apply(&mut document, &wait).unwrap();
        assert_eq!(
            document
                .projects
                .get("e1")
                .unwrap()
                .latest()
                .unwrap()
                .status,
            Status::Running
        );
    }

    fn capability_record(
        stage: CapabilityStage,
        outcome: Option<cayw_core::CapabilityOutcome>,
    ) -> CapabilityCallRecord {
        CapabilityCallRecord {
            key: CapabilityCallKey {
                execution_id: "e1".into(),
                sequence: 0,
            },
            name: "fs.read".into(),
            arguments: json!({"path": "input.json"}),
            parent_call_id: "call1".into(),
            task_id: "t1".into(),
            source_hash: script_digest(&crate::engine::script_for_plan(&plan()).unwrap()),
            lease: lease(),
            stage,
            authorization: None,
            outcome,
            external_provenance: None,
        }
    }

    #[test]
    fn capability_lifecycle_is_idempotent_and_terminal_is_final() {
        let planned = record(
            1,
            CoreEvent::CapabilityPlanned {
                call: capability_record(CapabilityStage::Planned, None),
            },
        );
        let started = record(
            2,
            CoreEvent::CapabilityStarted {
                call: capability_record(CapabilityStage::Started, None),
            },
        );
        let settled = record(
            3,
            CoreEvent::CapabilitySettled {
                call: capability_record(
                    CapabilityStage::Settled,
                    Some(cayw_core::CapabilityOutcome::Completed {
                        result: json!({"content": "x"}),
                    }),
                ),
            },
        );
        let mut document = Document::default();
        for event in [&planned, &started, &settled] {
            apply(&mut document, event).unwrap();
        }
        let project = document.projects.get("e1").unwrap();
        assert_eq!(project.calls.len(), 1);
        assert_eq!(project.calls[0].started_at_ms, Some(1_002));
        assert_eq!(project.calls[0].settled_at_ms, Some(1_003));
        assert!(project.calls[0].outcome.is_some());

        // Replaying the earlier stages confirms facts and cannot erase the
        // committed terminal.
        for event in [&planned, &started, &settled] {
            apply(&mut document, event).unwrap();
        }
        assert_eq!(document.projects.get("e1").unwrap().calls.len(), 1);
        assert!(
            document.projects.get("e1").unwrap().calls[0]
                .outcome
                .is_some()
        );

        let conflicting = record(
            4,
            CoreEvent::CapabilitySettled {
                call: capability_record(
                    CapabilityStage::Settled,
                    Some(cayw_core::CapabilityOutcome::Failed {
                        error: "different".into(),
                    }),
                ),
            },
        );
        let error = apply(&mut document, &conflicting).unwrap_err();
        assert!(matches!(error, Error::Conflict(_)));
        assert_eq!(document.projects.get("e1").unwrap().calls.len(), 1);
    }

    fn settled_unknown() -> EventRecord {
        record(
            1,
            CoreEvent::CapabilitySettled {
                call: capability_record(
                    CapabilityStage::Settled,
                    Some(cayw_core::CapabilityOutcome::OutcomeUnknown {
                        error: "writer disappeared".into(),
                    }),
                ),
            },
        )
    }

    fn reconciled(outcome: cayw_core::CapabilityOutcome) -> EventRecord {
        let mut event = record(
            2,
            CoreEvent::CapabilityReconciled {
                reconciliation: cayw_core::CapabilityReconciliation {
                    key: CapabilityCallKey {
                        execution_id: "e1".into(),
                        sequence: 0,
                    },
                    expected: Box::new(cayw_core::ReconciliationOrigin::from(&capability_record(
                        CapabilityStage::Settled,
                        None,
                    ))),
                    actor: "host".into(),
                    evidence: "checked the original effect".into(),
                    outcome: outcome.clone(),
                    external_provenance: None,
                },
                call: capability_record(CapabilityStage::Settled, Some(outcome)),
            },
        );
        // Core commits this event after the attempt has ended.
        event.attempt = None;
        event
    }

    #[test]
    fn core_reconciliation_updates_the_same_unknown_call() {
        let known = [
            cayw_core::CapabilityOutcome::Completed {
                result: json!({"content": "x"}),
            },
            cayw_core::CapabilityOutcome::Failed {
                error: "disk full".into(),
            },
            cayw_core::CapabilityOutcome::NotExecuted {
                reason: "rejected before dispatch".into(),
            },
        ];
        for outcome in known {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("trace.json");
            let store = crate::TraceStore::open(&path).unwrap();
            let original = settled_unknown();
            let update = reconciled(outcome.clone());
            let following = record(
                3,
                CoreEvent::Host {
                    kind: "after_reconciliation".into(),
                    data: json!({}),
                },
            );
            let events = vec![original, update, following];
            store.ingest(&events).unwrap();
            let document = store.snapshot();
            let project = &document.projects["e1"];
            assert_eq!(project.calls.len(), 1);
            assert_eq!(project.calls[0].outcome.as_ref(), Some(&outcome));
            assert_eq!(project.calls[0].name, "fs.read");
            assert_eq!(project.calls[0].arguments, json!({"path": "input.json"}));
            assert_eq!(project.attempt_id, "a1");
            assert_eq!(document.events, events);
            assert_eq!(document.sessions["s1"].after, 3);
            drop(store);
            let reopened = crate::TraceStore::open(path).unwrap();
            assert_eq!(reopened.snapshot().events, events);
            assert_eq!(reopened.snapshot().sessions["s1"].after, 3);
        }
    }

    #[test]
    fn reconciliation_rejects_a_different_call_or_terminal() {
        let mut document = Document::default();
        apply(&mut document, &settled_unknown()).unwrap();
        let original_call = document.projects.get("e1").unwrap().calls[0].clone();

        let mut other_attempt = reconciled(cayw_core::CapabilityOutcome::Completed {
            result: json!({"content": "x"}),
        });
        if let CoreEvent::CapabilityReconciled { call, .. } = &mut other_attempt.event {
            call.lease.attempt_id = "a2".into();
        }
        assert!(matches!(
            apply(&mut document, &other_attempt),
            Err(Error::Conflict(_))
        ));

        let mut other_name = reconciled(cayw_core::CapabilityOutcome::Failed {
            error: "disk full".into(),
        });
        if let CoreEvent::CapabilityReconciled { call, .. } = &mut other_name.event {
            call.name = "fs.write".into();
        }
        assert!(matches!(
            apply(&mut document, &other_name),
            Err(Error::Conflict(_))
        ));

        let completed = record(
            1,
            CoreEvent::CapabilitySettled {
                call: capability_record(
                    CapabilityStage::Settled,
                    Some(cayw_core::CapabilityOutcome::Completed {
                        result: json!({"content": "kept"}),
                    }),
                ),
            },
        );
        let mut settled = Document::default();
        apply(&mut settled, &completed).unwrap();
        assert!(matches!(
            apply(
                &mut settled,
                &reconciled(cayw_core::CapabilityOutcome::Failed {
                    error: "must not replace a known result".into(),
                })
            ),
            Err(Error::Conflict(_))
        ));
        assert_eq!(
            settled.projects.get("e1").unwrap().calls[0].outcome,
            Some(cayw_core::CapabilityOutcome::Completed {
                result: json!({"content": "kept"}),
            })
        );
        assert_eq!(
            document.projects.get("e1").unwrap().calls[0].outcome,
            original_call.outcome
        );
        assert_eq!(
            document.projects.get("e1").unwrap().calls[0].name,
            "fs.read"
        );
    }

    #[test]
    fn execution_from_another_attempt_is_rejected() {
        let mut document = Document::default();
        let mut foreign = execution(ExecutionState::Running, None);
        foreign.lease.attempt_id = "a2".into();
        let error = apply(
            &mut document,
            &record(1, CoreEvent::ExecutionStarted { execution: foreign }),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Conflict(_)));
        assert!(document.projects.is_empty());
    }

    fn host_identity(cursor: u64, task: &str, source: &str, output: Value) -> EventRecord {
        record(
            cursor,
            CoreEvent::Host {
                kind: TRACE_NODE_KIND.into(),
                data: json!({
                    "execution_id": "e1",
                    "parent_call_id": "call1",
                    "source_hash": source,
                    "task_id": task,
                    "entry": {"cursor": cursor,
                        "output": {"type": "json", "value": output}},
                }),
            },
        )
    }

    #[test]
    fn trace_identity_must_match_core_execution() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        let manifest = envelope(NodeTrace::Manifest { plan: plan() });
        let wrong_task = host_identity(2, "t2", "sha256:abc", manifest.clone());
        apply(&mut document, &wrong_task).unwrap();
        assert!(document.projects["e1"].trace_error.is_some());
        let wrong_source = host_identity(2, "t1", "sha256:other", manifest);
        document.projects.get_mut("e1").unwrap().trace_error = None;
        apply(&mut document, &wrong_source).unwrap();
        assert!(document.projects["e1"].trace_error.is_some());
        let project = document.projects.get("e1").unwrap();
        assert_eq!(project.task_id, "t1");
        assert_eq!(
            project.source_hash,
            execution(ExecutionState::Running, None).source_hash
        );
        assert!(project.revisions.is_empty());
    }

    #[test]
    fn duplicate_terminal_after_missing_completion_is_stable() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &host(2, envelope(NodeTrace::Manifest { plan: plan() })),
        )
        .unwrap();
        apply(
            &mut document,
            &host(
                3,
                envelope(NodeTrace::Started {
                    node_id: "a".into(),
                    input: plan().input,
                    state_before: plan().initial_state,

                    context_before: Value::Null,
                    seed: None,
                }),
            ),
        )
        .unwrap();
        let finish = record(
            4,
            CoreEvent::ExecutionFinished {
                execution: execution(
                    ExecutionState::Completed,
                    Some(ExecutionOutcome::Completed {
                        result: json!({"final": true}),
                    }),
                ),
            },
        );
        apply(&mut document, &finish).unwrap();
        let first = document.projects.get("e1").unwrap().clone();
        assert_eq!(first.latest().unwrap().status, Status::Blocked);
        assert_eq!(first.latest().unwrap().steps[0].status, Status::Blocked);

        apply(&mut document, &finish).unwrap();
        let project = document.projects.get("e1").unwrap();
        assert_eq!(project.latest().unwrap().status, Status::Blocked);
        assert_eq!(project.latest().unwrap().steps[0].status, Status::Blocked);
        assert_eq!(project.gaps, first.gaps);
        assert_eq!(project.runtime_status, Status::Completed);
    }

    #[test]
    fn late_completion_cannot_rewrite_failed_terminal() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &host(2, envelope(NodeTrace::Manifest { plan: plan() })),
        )
        .unwrap();
        apply(
            &mut document,
            &host(
                3,
                envelope(NodeTrace::Started {
                    node_id: "a".into(),
                    input: plan().input,
                    state_before: plan().initial_state,

                    context_before: Value::Null,
                    seed: None,
                }),
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &record(
                4,
                CoreEvent::ExecutionFinished {
                    execution: execution(
                        ExecutionState::Failed,
                        Some(ExecutionOutcome::Failed {
                            error: "boom".into(),
                        }),
                    ),
                },
            ),
        )
        .unwrap();
        let late = host(
            5,
            envelope(NodeTrace::Completed {
                node_id: "a".into(),
                result: NodeResult {
                    output: json!({"value": 2}),
                    state: json!({"visited":1}),
                    branch: None,
                },
                elapsed_ms: 1.0,
            }),
        );
        apply(&mut document, &late).unwrap();
        assert!(document.projects["e1"].trace_error.is_some());
        let step = &document.projects.get("e1").unwrap().latest().unwrap().steps[0];
        assert_eq!(step.status, Status::Failed);
        assert_eq!(step.error.as_deref(), Some("boom"));
        assert_eq!(step.evidence_cursor, Some(4));
    }

    #[test]
    fn duplicate_completion_after_terminal_is_noop() {
        let mut document = Document::default();
        apply(
            &mut document,
            &record(
                1,
                CoreEvent::ExecutionStarted {
                    execution: execution(ExecutionState::Running, None),
                },
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &host(2, envelope(NodeTrace::Manifest { plan: plan() })),
        )
        .unwrap();
        apply(
            &mut document,
            &host(
                3,
                envelope(NodeTrace::Started {
                    node_id: "a".into(),
                    input: plan().input,
                    state_before: plan().initial_state,

                    context_before: Value::Null,
                    seed: None,
                }),
            ),
        )
        .unwrap();
        let complete = host(
            4,
            envelope(NodeTrace::Completed {
                node_id: "a".into(),
                result: NodeResult {
                    output: json!({"value": 2}),
                    state: json!({"visited":1}),
                    branch: None,
                },
                elapsed_ms: 1.0,
            }),
        );
        apply(&mut document, &complete).unwrap();
        apply(
            &mut document,
            &host(
                5,
                envelope(NodeTrace::Started {
                    node_id: "b".into(),
                    input: json!({"value":2}),
                    state_before: json!({"visited":1}),

                    context_before: Value::Null,
                    seed: None,
                }),
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &host(
                6,
                envelope(NodeTrace::Completed {
                    node_id: "b".into(),
                    result: NodeResult {
                        output: json!({"value":3}),
                        state: json!({"visited":2}),
                        branch: None,
                    },
                    elapsed_ms: 0.0,
                }),
            ),
        )
        .unwrap();
        apply(
            &mut document,
            &record(
                7,
                CoreEvent::ExecutionFinished {
                    execution: execution(
                        ExecutionState::Completed,
                        Some(ExecutionOutcome::Completed {
                            result: json!({"final": true}),
                        }),
                    ),
                },
            ),
        )
        .unwrap();
        apply(&mut document, &complete).unwrap();
        let evaluation = document.projects.get("e1").unwrap().latest().unwrap();
        assert_eq!(evaluation.status, Status::Completed);
        assert_eq!(evaluation.steps.len(), 2);
        assert_eq!(evaluation.steps[0].status, Status::Completed);
    }
}
