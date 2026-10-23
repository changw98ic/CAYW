//! Rechecks recorded cancellation and process-loss facts against their originals.
use super::{Result, ScenarioReport, validation::required_attachment};
use crate::measurement::sha256;
use cayw_core::{
    AttemptLease, CapabilityOutcome, CoreEvent, EventRecord, ExecutionOutcome, ExecutionState,
};
use cayw_runtime::{SessionView, TaskRecord, TaskState};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::Path;

fn observed<'a>(report: &'a ScenarioReport, kind: &str) -> Result<&'a Value> {
    let mut values = report.observations.iter().filter(|row| row.kind == kind);
    let value = &values
        .next()
        .ok_or_else(|| format!("missing original observation: {kind}"))?
        .value;
    if values.next().is_some() {
        return Err(format!("duplicate observation: {kind}").into());
    }
    Ok(value)
}
fn decode<T: DeserializeOwned>(report: &ScenarioReport, kind: &str) -> Result<T> {
    Ok(serde_json::from_value(observed(report, kind)?.clone())?)
}
fn tasks(
    report: &ScenarioReport,
    kind: &str,
    lease: &AttemptLease,
    state: TaskState,
) -> Result<TaskRecord> {
    let values: Vec<TaskRecord> = decode(report, kind)?;
    let task = values.first().ok_or("original task absent")?;
    if values.len() != 1
        || task.session_id != lease.session_id
        || task.attempt_id.as_ref() != Some(&lease.attempt_id)
        || task.worker_id.as_ref() != Some(&lease.worker_id)
        || task.state != state
        || task.id.is_empty()
        || !task.input.images.is_empty()
        || task.input.text
            != report.specification["input"]["prompt"]
                .as_str()
                .ok_or("prompt absent")?
    {
        return Err("fault task lost its original identity or terminal state".into());
    }
    Ok(task.clone())
}

pub(super) fn validate(
    report: &ScenarioReport,
    root: &Path,
    events: &[EventRecord],
    entries: &[Value],
) -> Result<()> {
    let starts: Vec<_> = events
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::AttemptStarted { lease } => Some((record, lease)),
            _ => None,
        })
        .collect();
    let (start, lease) = starts
        .first()
        .copied()
        .ok_or("fault history has no original attempt")?;
    if starts.len() != 1
        || start.attempt.as_ref() != Some(lease)
        || lease.session_id != start.session_id
        || lease.attempt_id.is_empty()
        || lease.worker_id.is_empty()
        || lease.fence == 0
        || entries.len() != 1
        || events.iter().any(|record| {
            record.protocol_version != cayw_core::PROTOCOL_VERSION
                || record.session_id != lease.session_id
                || (record.cursor >= start.cursor && record.attempt.as_ref() != Some(lease))
        })
    {
        return Err("fault history changed the original executor or replayed the Provider".into());
    }
    match report.scenario.as_str() {
        "ipc-reconnect" | "ipc-cancel" => cancellation(report, root, events, lease),
        "host-restart-before-dispatch" | "host-restart-unknown" => {
            restart(report, root, events, lease)
        }
        _ => Err("unknown recorded fault scenario".into()),
    }
}

pub(super) fn wire(report: &ScenarioReport, root: &Path) -> Result<Vec<Value>> {
    let bytes = required_attachment(report, root, "ipc_frames")?;
    let frames: Vec<Value> = std::str::from_utf8(&bytes)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    if observed(report, "actual_ipc_frames")? != &json!(frames) {
        return Err("wire attachment contradicts observed frames".into());
    }
    for (index, frame) in frames.iter().enumerate() {
        let raw = frame["utf8"]
            .as_str()
            .ok_or("wire bytes absent")?
            .as_bytes();
        if frame["sequence"] != index + 1
            || frame["bytes"] != raw.len()
            || frame["sha256"] != sha256(raw)
            || frame["frame"] != serde_json::from_slice::<Value>(raw)?
            || !frame["connection"].is_u64()
        {
            return Err("wire frame bytes or sequence contradict its decoded record".into());
        }
    }
    Ok(frames)
}
fn cancellation(
    report: &ScenarioReport,
    root: &Path,
    events: &[EventRecord],
    lease: &AttemptLease,
) -> Result<()> {
    let final_view: SessionView = decode(report, "final_sdk_session")?;
    let converged: SessionView = decode(report, "converged_session")?;
    let final_task = tasks(report, "final_sdk_tasks", lease, TaskState::Cancelled)?;
    let task = tasks(report, "converged_tasks", lease, TaskState::Cancelled)?;
    let entered = observed(report, "fault_point_provider_entered")?;
    if final_view != converged
        || !super::scenarios::clean(&final_view)
        || final_view.id != lease.session_id
        || final_view.cursor != events.last().ok_or("fault events absent")?.cursor
        || task != final_task
        || task.fence != lease.fence
        || entered["provider_calls"] != 1
        || entered["session"]["attempt"] != serde_json::to_value(lease)?
        || !final_view.executions.is_empty()
        || root.join("workspace/result.txt").exists()
        || events.iter().any(|record| {
            matches!(
                record.event,
                CoreEvent::ExecutionStarted { .. }
                    | CoreEvent::CapabilityStarted { .. }
                    | CoreEvent::ToolStarted { .. }
            )
        })
        || !matches!(&events.last().unwrap().event,CoreEvent::AttemptFinished{lease:closed} if closed==lease)
    {
        return Err(
            "cancelled request has no original durable idle task or has unexpected effects".into(),
        );
    }
    let aborted = events
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::Agent { event } => serde_json::to_value(event).ok(),
            _ => None,
        })
        .any(|event| {
            event["type"] == "message_end"
                && event["message"]["role"] == "assistant"
                && event["message"]["stop_reason"] == "aborted"
        });
    if !aborted {
        return Err("cancelled task lacks its original aborted assistant terminal".into());
    }
    let saved: Value = serde_json::from_slice(&required_attachment(report, root, "cursor")?)?;
    let cursor = saved["cursor"].as_u64().ok_or("saved cursor absent")?;
    if saved["session_id"] != lease.session_id {
        return Err("saved cursor changed sessions".into());
    }
    let first: Vec<_> = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "before_fault")
        .collect();
    let resumed: Vec<_> = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "after_reconnect")
        .collect();
    if first.len() != 1
        || first[0].after != 0
        || first[0].records.len() != 1
        || first[0].records[0] != events[0]
        || first[0].records[0].cursor != cursor
        || resumed.is_empty()
        || !resumed.last().unwrap().records.is_empty()
    {
        return Err("fault reconnect lost its saved original event page".into());
    }
    let mut after = cursor;
    let mut suffix = Vec::new();
    for page in &resumed {
        if page.after != after || page.session_id != lease.session_id {
            return Err("fault reconnect cursor chain changed".into());
        }
        after = page.records.last().map_or(after, |event| event.cursor);
        suffix.extend(page.records.clone());
    }
    if suffix
        != events
            .iter()
            .filter(|event| event.cursor > cursor)
            .cloned()
            .collect::<Vec<_>>()
    {
        return Err("fault reconnect event content differs from SDK history".into());
    }
    let frames = wire(report, root)?;
    let prompts: Vec<_> = frames
        .iter()
        .filter(|frame| {
            frame["direction"] == "client_to_server" && frame["frame"]["method"] == "session.prompt"
        })
        .collect();
    if prompts.len() != 1
        || prompts[0]["frame"]["params"]
            != json!({"session_id":lease.session_id,"text":report.specification["input"]["prompt"]})
    {
        return Err("fault reconnect replaced the original prompt".into());
    }
    let prompt = prompts[0];
    let reconnect: Vec<_> = frames
        .iter()
        .filter(|frame| {
            frame["direction"] == "client_to_server" && frame["frame"]["method"] == "session.events"
        })
        .collect();
    if reconnect.len() != resumed.len() + 1
        || reconnect[0]["connection"] != prompt["connection"]
        || reconnect[1]["connection"] == prompt["connection"]
    {
        return Err("fault reconnect is not an explicit new connection".into());
    }
    for (request, page) in reconnect
        .iter()
        .zip(std::iter::once(first[0]).chain(resumed.iter().copied()))
    {
        if request["frame"]["params"]["after"] != page.after
            || request["frame"]["params"]["session_id"] != page.session_id
        {
            return Err("fault page differs from its original request".into());
        }
        let replies: Vec<_> = frames
            .iter()
            .filter(|frame| {
                frame["direction"] == "server_to_client"
                    && frame["connection"] == request["connection"]
                    && frame["frame"]["id"] == request["frame"]["id"]
            })
            .collect();
        if replies.len() != 1
            || replies[0]["frame"]["result"] != serde_json::to_value(&page.records)?
        {
            return Err("fault page lost its original wire result".into());
        }
    }
    let terminal: std::result::Result<Value, cayw_ipc::ClientError> =
        decode(report, "prompt_transport_terminal")?;
    if report.scenario == "ipc-reconnect" {
        if !terminal.as_ref().is_err_and(|error| error.outcome_unknown)
            || !frames.iter().any(|frame| {
                frame["direction"] == "relay_terminal"
                    && frame["connection"] == prompt["connection"]
            })
            || frames
                .iter()
                .any(|frame| frame["frame"]["method"] == "rpc.cancel")
        {
            return Err(
                "execution socket loss did not retain its unknown transport outcome".into(),
            );
        }
    } else {
        let mut checks = ScenarioReport::new("cancel-recheck", 1, Value::Null, Value::Null);
        super::scenarios::validate_cancel_terminal(&frames, &terminal, &mut checks);
        let cancels = frames
            .iter()
            .filter(|frame| {
                frame["direction"] == "client_to_server"
                    && frame["frame"]["method"] == "rpc.cancel"
                    && frame["connection"] == prompt["connection"]
                    && frame["frame"]["params"]["request_id"] == prompt["frame"]["id"]
            })
            .count();
        if cancels != 1 || checks.checks.iter().any(|check| !check.passed) {
            return Err("cancel request or consumed original reply was changed".into());
        }
    }
    Ok(())
}

fn restart(
    report: &ScenarioReport,
    root: &Path,
    events: &[EventRecord],
    lease: &AttemptLease,
) -> Result<()> {
    let effect = report.scenario == "host-restart-unknown";
    let before: SessionView = decode(report, "before_kill_session")?;
    let opened: SessionView = decode(report, "reopened_before_recovery")?;
    let after: SessionView = decode(report, "after_recover_session")?;
    let original = tasks(report, "before_kill_tasks", lease, TaskState::Running)?;
    let recovered = tasks(report, "after_recover_tasks", lease, TaskState::Interrupted)?;
    let prefix: Vec<_> = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "before_kill")
        .collect();
    if prefix.len() != 1
        || prefix[0].after != 0
        || !events.starts_with(&prefix[0].records)
        || before != opened
        || before.attempt.as_ref() != Some(lease)
        || before.id != lease.session_id
        || before.cursor
            != prefix[0]
                .records
                .last()
                .ok_or("original events absent")?
                .cursor
        || after.id != lease.session_id
        || after.attempt.is_some()
        || after.cursor != events.last().ok_or("recovery events absent")?.cursor
        || original.id != recovered.id
        || original.fence != lease.fence
        || recovered.fence <= original.fence
        || original.queued_at != recovered.queued_at
        || !recovered.cancel_requested
        || observed(report, "resume_original_interrupted_task")?["result"]
            .get("Err")
            .is_none()
        || root.join("reopened-provider.jsonl").exists()
    {
        return Err("recovery changed the original task, history or executor identity".into());
    }
    let (executions, capabilities) = match &events.last().unwrap().event {
        CoreEvent::AttemptRecovered {
            lease: Some(origin),
            unknown_call_ids,
            not_executed_call_ids,
            executions,
            capabilities,
        } if origin == lease && unknown_call_ids.is_empty() && not_executed_call_ids.is_empty() => {
            (executions, capabilities)
        }
        _ => return Err("recovery lacks its original attempt terminal".into()),
    };
    if events.len() != prefix[0].records.len() + 1 {
        return Err("recovery replayed or altered the original work".into());
    }
    let identity: Value =
        serde_json::from_slice(&required_attachment(report, root, "host_identity")?)?;
    let receipt: crate::child::ChildOutput =
        serde_json::from_slice(&required_attachment(report, root, "child_exit")?)?;
    let stdout = required_attachment(report, root, "child_stdout")?;
    let stderr = required_attachment(report, root, "child_stderr")?;
    let marker = required_attachment(report, root, "fault_marker")?;
    let readiness = required_attachment(report, root, "fault_ready")?;
    if observed(report, "host_identity")? != &identity
        || identity["session_id"] != lease.session_id
        || identity["binding"] != report.binding
        || identity["process_id"] != receipt.process_id
        || receipt.process_id == report.process_id
        || !receipt.stopped
        || receipt.success
        || receipt.timed_out
        || !receipt.cleanup_confirmed
        || receipt.error.is_some()
        || !stdout.is_empty()
        || !stderr.is_empty()
        || receipt.stdout != stdout
        || receipt.stderr != stderr
        || receipt.stdout_bytes != 0
        || receipt.stderr_bytes != 0
        || receipt.stdout_sha256 != sha256(&stdout)
        || receipt.stderr_sha256 != sha256(&stderr)
        || observed(report, "host_exit")? != &serde_json::to_value(&receipt)?
        || observed(report, "fault_point_durable_marker")?["value"]
            != serde_json::from_slice::<Value>(&marker)?
        || observed(report, "fault_point_durable_marker")?["sha256"] != sha256(&marker)
        || observed(report, "fault_point_readiness")?
            != &serde_json::from_slice::<Value>(&readiness)?
    {
        return Err("recovery lacks matching original process exit and fault records".into());
    }
    if !effect {
        let marker: Value = serde_json::from_slice(&marker)?;
        if !super::scenarios::clean(&after)
            || !executions.is_empty()
            || !capabilities.is_empty()
            || !before.executions.is_empty()
            || root.join("workspace/result.txt").exists()
            || marker["session"] != serde_json::to_value(&before)?
            || marker["tasks"] != json!([original])
            || marker["process_id"] != receipt.process_id
            || marker["provider_calls"] != 1
        {
            return Err(
                "before-dispatch recovery has unexpected execution or missing original task".into(),
            );
        }
        return Ok(());
    }
    let starts: Vec<_> = events
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::CapabilityStarted { call } => Some(call),
            _ => None,
        })
        .collect();
    if starts.len() != 1
        || capabilities.len() != 1
        || executions.len() != 1
        || before.executions.len() != 1
        || after.state != cayw_core::SessionState::NeedsReconciliation
        || after.unresolved_capabilities != *capabilities
        || after.executions != *executions
        || serde_json::from_slice::<Value>(&marker)? != super::provider::effect_result()
        || serde_json::from_slice::<Value>(&readiness)?
            != json!({"effect_sha256":sha256(&marker),"process_id":receipt.process_id})
        || decode::<SessionView>(report, "second_reopen_still_unknown")? != after
        || observed(report, "prompt_frozen_unknown")?["result"]
            .get("Err")
            .is_none()
    {
        return Err("unknown effect did not remain frozen across recovery and reopen".into());
    }
    let start = starts[0];
    let settled = &capabilities[0];
    let mut expected = start.clone();
    expected.stage = cayw_core::CapabilityStage::Settled;
    expected.outcome = settled.outcome.clone();
    let mut execution = before.executions[0].clone();
    execution.state = ExecutionState::Interrupted;
    execution.terminal = executions[0].terminal.clone();
    let authority: cayw_core::CapabilityAuthorization = serde_json::from_value(
        start
            .authorization
            .clone()
            .ok_or("original effect authority absent")?,
    )?;
    if settled != &expected
        || !matches!(
            settled.outcome,
            Some(CapabilityOutcome::OutcomeUnknown { .. })
        )
        || start.name != "search.query"
        || start.arguments != super::provider::external_arguments()
        || start.outcome.is_some()
        || start.lease != *lease
        || start.task_id != original.id
        || start.key.execution_id != execution.execution_id
        || start.parent_call_id != execution.parent_call_id
        || start.source_hash
            != format!(
                "sha256:{}",
                sha256(super::provider::EFFECT_SCRIPT.as_bytes())
            )
        || execution != executions[0]
        || !matches!(
            execution.terminal,
            Some(ExecutionOutcome::Interrupted { .. })
        )
        || authority.target != start.key
        || authority.task_id != start.task_id
        || authority.capability != start.name
        || authority.decision != cayw_core::ApprovalDecision::Approve
        || authority.actor != "runtime-config"
        || authority.scope != serde_json::to_string(&start.arguments)?
    {
        return Err(
            "unknown effect changed its exact original capability, permission or interrupted VM"
                .into(),
        );
    }
    Ok(())
}
