use super::{Result, ScenarioReport};
use crate::{evidence::attachment, measurement::sha256};
use cayw_ai::{Message, ModelRequest, ToolCall};
use cayw_core::{AttemptLease, CoreEvent, EventRecord, ExecutionRecord, ExecutionState};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub(crate) fn validate_recorded_sample(report: &ScenarioReport, root: &Path) -> Result<()> {
    if matches!(report.scenario.as_str(), "ipc-approval" | "ipc-rejections") {
        return super::access_validation::validate(report, root);
    }
    if crate::cases::CASES
        .iter()
        .any(|case| case.name() == report.scenario)
    {
        let bytes = attachment(
            root,
            report.artifacts["luau"]["path"]
                .as_str()
                .ok_or("Luau record absent")?,
        )?;
        let mut sample: crate::runner::Sample = serde_json::from_slice(&bytes)?;
        if sample.schema_version != 2
            || sample.sample_id != report.sample_id
            || sample.case != report.scenario
            || sample.process_id != report.process_id
            || sample.iteration != report.iteration
            || sample.input != report.specification["input"]
            || sample.permission != report.specification["permissions"]
            || sample.config != report.specification["configuration"]
        {
            return Err("Luau raw input contradicts the scenario".into());
        }
        let audit: Vec<Value> = std::str::from_utf8(&attachment(
            root,
            report.artifacts["audit"]["path"]
                .as_str()
                .ok_or("audit absent")?,
        )?)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
        if serde_json::to_value(&sample.audit_events)? != Value::Array(audit) {
            return Err("Luau audit differs from its original attachment".into());
        }
        if sample.case == crate::cases::Case::Cold.name() {
            validate_luau_child(report, root, &sample)?;
        }
        let recorded_repairs = report
            .observations
            .iter()
            .filter(|row| row.kind == "repair_attempt")
            .map(|row| row.value.clone())
            .collect::<Vec<_>>();
        let expected_repairs = sample.repair_record().into_iter().collect::<Vec<_>>();
        let expected_timings = serde_json::to_value(&sample.timing)?
            .as_object()
            .unwrap()
            .iter()
            .filter_map(|(phase, value)| value.as_u64().map(|value| (phase.clone(), value)))
            .collect::<BTreeMap<_, _>>();
        let mut recorded_timings = report.timing_ns.clone();
        if recorded_timings.remove("scenario_total").is_none()
            || recorded_repairs != expected_repairs
            || recorded_timings != expected_timings
        {
            return Err(
                "Luau repair attempts or timings differ from the original execution record".into(),
            );
        }
        crate::runner::revalidate(&mut sample)?;
        if report.status == "passed" && !sample.semantic_passed {
            return Err("Luau task does not satisfy its recorded result".into());
        }
        return Ok(());
    }
    if super::BuiltinScenario::parse(&report.scenario).is_err() {
        return Ok(());
    }
    let journal = &report.artifacts["provider"];
    let bytes = attachment(
        root,
        journal["path"]
            .as_str()
            .ok_or("required Provider journal absent")?,
    )?;
    if journal["sha256"] != sha256(&bytes) {
        return Err("Provider journal digest mismatch".into());
    }
    let entries: Vec<Value> = std::str::from_utf8(&bytes)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    if entries.is_empty() {
        return Err("Provider journal is empty".into());
    }
    for (i, entry) in entries.iter().enumerate() {
        let request: cayw_ai::ModelRequest = serde_json::from_value(entry["request"].clone())?;
        if entry["call"] != i + 1
            || entry["source"] != report.source
            || entry["request"]["model"] != report.specification["input"]["provider"]
            || entry["request_sha256"] != sha256(&serde_json::to_vec(&request)?)
        {
            return Err("Provider journal identity or request digest contradicts the input".into());
        }
        let messages = entry["request"]["messages"]
            .as_array()
            .ok_or("Provider messages absent")?;
        let users: Vec<_> = messages.iter().filter(|m| m["role"] == "user").collect();
        if users.is_empty() || users.iter().any(|m|m["content"] != serde_json::json!([{"type":"text","text":report.specification["input"]["prompt"]}])) {
            return Err("Provider prompt contradicts its scenario declaration".into());
        }
    }
    if matches!(
        report.scenario.as_str(),
        "ipc-lifecycle" | "ipc-event-reconnect"
    ) {
        super::fault_validation::wire(report, root)?;
    }
    if report.status != "passed" {
        return Ok(());
    }
    // Exact durable events are repeated in IPC/SDK snapshots. Deduplicate only
    // identical records; conflicting data at the same cursor is invalid.
    let mut sessions = BTreeMap::<String, BTreeMap<u64, cayw_core::EventRecord>>::new();
    for page in &report.event_pages {
        let last = page.records.last().map_or(page.after, |r| r.cursor);
        if !super::contiguous_events(&page.session_id, page.after, &page.records, last) {
            return Err("event page has gaps or an unrelated session".into());
        }
        for event in &page.records {
            let previous = sessions
                .entry(event.session_id.clone())
                .or_default()
                .insert(event.cursor, event.clone());
            if previous.as_ref().is_some_and(|old| old != event) {
                return Err("conflicting original event contents".into());
            }
        }
    }
    if sessions.len() != 1 {
        return Err("successful pipeline needs its original complete session".into());
    }
    let events: Vec<_> = sessions
        .into_values()
        .next()
        .unwrap()
        .into_values()
        .collect();
    if !super::contiguous_events(
        &events[0].session_id,
        0,
        &events,
        events.last().unwrap().cursor,
    ) {
        return Err("pipeline event history is incomplete".into());
    }
    if matches!(
        report.scenario.as_str(),
        "ipc-lifecycle" | "ipc-event-reconnect" | "ipc-reconnect" | "ipc-cancel"
    ) {
        validate_session_origin(report, &events)?;
    }
    let runs = match report.scenario.as_str() {
        "sdk-lifecycle" | "ipc-lifecycle" => 2,
        "cold-sdk" | "ipc-event-reconnect" => 1,
        _ => return super::fault_validation::validate(report, root, &events, &entries),
    };
    let mut checks = ScenarioReport::new("revalidate", 1, Value::Null, Value::Null);
    super::scenarios::validate_pipeline(&events, runs, &mut checks);
    if checks.checks.iter().any(|check| !check.passed) {
        return Err("pipeline task evidence fails semantic validation".into());
    }
    if report.scenario == "ipc-event-reconnect" {
        validate_event_reconnect(report, root, &events)?;
    }
    if report.scenario == "ipc-lifecycle" {
        validate_ipc_lifecycle(report, root, &events)?;
    }
    let cold = if report.scenario == "cold-sdk" {
        Some(validate_cold_report(report, root)?)
    } else {
        None
    };
    validate_calls(&events, &entries, cold.as_ref().unwrap_or(report), runs)?;
    let actual = attachment(root, "workspace/result.txt")?;
    if actual != super::provider::CONTENT.as_bytes() {
        return Err("actual task file effect is absent or incorrect".into());
    }
    Ok(())
}

pub(super) fn required_attachment(
    report: &ScenarioReport,
    root: &Path,
    name: &str,
) -> Result<Vec<u8>> {
    let descriptor = &report.artifacts[name];
    let bytes = attachment(
        root,
        descriptor["path"]
            .as_str()
            .ok_or("required attachment declaration absent")?,
    )?;
    if descriptor["sha256"] != sha256(&bytes) {
        return Err("required attachment digest mismatch".into());
    }
    Ok(bytes)
}

fn validate_luau_child(
    report: &ScenarioReport,
    root: &Path,
    sample: &crate::runner::Sample,
) -> Result<()> {
    let output: crate::child::ChildOutput =
        serde_json::from_slice(&required_attachment(report, root, "child")?)?;
    if output.process_id != sample.process_id
        || !output.success
        || output.exit_code != Some(0)
        || output.timed_out
        || output.stopped
        || !output.cleanup_confirmed
        || output.error.is_some()
        || output.stdout_bytes != output.stdout.len() as u64
        || output.stderr_bytes != output.stderr.len() as u64
        || output.stdout_sha256 != sha256(&output.stdout)
        || output.stderr_sha256 != sha256(&output.stderr)
        || !output.stderr.is_empty()
    {
        return Err(
            "Luau cold task has no matching original process exit and output receipt".into(),
        );
    }
    let mut child: crate::runner::Sample = serde_json::from_slice(&output.stdout)?;
    let mut parent = serde_json::to_value(sample)?;
    // The parent adds only ownership checks and its surrounding spawn duration.
    child.timing.parent_spawn_to_exit_ns = sample.timing.parent_spawn_to_exit_ns;
    parent["semantic_checks"] = serde_json::to_value(&child.semantic_checks)?;
    parent["semantic_passed"] = child.semantic_passed.into();
    if serde_json::to_value(child)? != parent {
        return Err("Luau cold report differs from its original child output".into());
    }
    Ok(())
}

fn validate_cold_report(report: &ScenarioReport, root: &Path) -> Result<ScenarioReport> {
    let bytes = required_attachment(report, root, "cold_report")?;
    let child: ScenarioReport = serde_json::from_slice(&bytes)?;
    let observed = report
        .observations
        .iter()
        .filter(|row| row.kind == "cold_child_report")
        .collect::<Vec<_>>();
    let receipt: crate::child::ChildOutput =
        serde_json::from_slice(&required_attachment(report, root, "child_exit")?)?;
    let stdout = required_attachment(report, root, "child_stdout")?;
    let stderr = required_attachment(report, root, "child_stderr")?;
    if child.schema_version != 2
        || child.scenario != "cold-sdk-child"
        || child.iteration != report.iteration
        || child.sample_id != format!("{:04}/cold-sdk-child", report.iteration)
        || child.specification != report.specification
        || child.binding != report.binding
        || child.source != report.source
        || child.process_id == report.process_id
        || receipt.process_id != child.process_id
        || receipt.exit_code != Some(0)
        || !receipt.success
        || receipt.timed_out
        || receipt.stopped
        || !receipt.cleanup_confirmed
        || receipt.error.is_some()
        || receipt.stdout_bytes != stdout.len() as u64
        || receipt.stderr_bytes != stderr.len() as u64
        || receipt.stdout != stdout
        || receipt.stderr != stderr
        || receipt.stdout_sha256 != sha256(&stdout)
        || receipt.stderr_sha256 != sha256(&stderr)
        || !stderr.is_empty()
        || serde_json::from_slice::<Value>(&stdout)? != serde_json::to_value(&child)?
        || observed.len() != 1
        || observed[0].value != serde_json::to_value(&child)?
        || serde_json::to_value(&report.event_pages)? != serde_json::to_value(&child.event_pages)?
        || child.status != "passed"
        || !child.has_task_checks()
        || child.checks.iter().any(|check| !check.passed)
    {
        return Err("cold report does not identify the original successful child".into());
    }
    let raw = attachment(
        root,
        child.artifacts["records"]["path"]
            .as_str()
            .ok_or("child records absent")?,
    )?;
    let observations: Vec<Value> = std::str::from_utf8(&raw)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let mut artifacts = child.artifacts.clone();
    artifacts
        .as_object_mut()
        .ok_or("child artifacts absent")?
        .remove("records");
    let terminal = observations.last().ok_or("child terminal absent")?;
    let pages: Vec<_> = observations
        .iter()
        .filter(|row| row["kind"] == "event_page")
        .map(|row| row["value"].clone())
        .collect();
    if child.artifacts["records"]["sha256"] != sha256(&raw)
        || serde_json::to_value(&child.observations)? != serde_json::json!(observations)
        || observations
            .iter()
            .enumerate()
            .any(|(i, row)| row["sequence"] != i + 1)
        || observations[0]["kind"] != "scenario_start"
        || observations[0]["value"]["sample_id"] != child.sample_id
        || observations[0]["value"]["specification"] != child.specification
        || observations[0]["value"]["binding"] != child.binding
        || serde_json::to_value(&child.event_pages)? != serde_json::json!(pages)
        || terminal["kind"] != "scenario_terminal"
        || terminal["value"]["status"] != child.status
        || terminal["value"]["checks"] != serde_json::to_value(&child.checks)?
        || terminal["value"]["task_checks"] != serde_json::to_value(&child.task_checks)?
        || terminal["value"]["timing_ns"] != serde_json::to_value(&child.timing_ns)?
        || terminal["value"]["process_peak_rss"] != serde_json::to_value(&child.process_peak_rss)?
        || terminal["value"]["artifacts"] != artifacts
    {
        return Err("cold child raw observations contradict its original report".into());
    }
    Ok(child)
}

fn validate_session_origin(report: &ScenarioReport, events: &[EventRecord]) -> Result<()> {
    let first = events.first().ok_or("original session events absent")?;
    let CoreEvent::SessionCreated { label } = &first.event else {
        return Err("original session history has no creation event".into());
    };
    if first.attempt.is_some()
        || events
            .iter()
            .filter(|record| matches!(&record.event, CoreEvent::SessionCreated { .. }))
            .count()
            != 1
    {
        return Err("original session creation is duplicated or belongs to an attempt".into());
    }
    let mut snapshots = 0;
    for observation in &report.observations {
        if !observation.kind.ends_with("_session") {
            continue;
        }
        let session: cayw_runtime::SessionView = serde_json::from_value(observation.value.clone())?;
        if session.id != first.session_id
            || session.label != *label
            || !events.iter().any(|record| record.cursor == session.cursor)
        {
            return Err("original session creation differs from its retained SDK snapshots".into());
        }
        snapshots += 1;
    }
    if snapshots == 0 {
        return Err("original session has no retained SDK snapshot".into());
    }
    Ok(())
}

fn validate_ipc_lifecycle(
    report: &ScenarioReport,
    root: &Path,
    events: &[EventRecord],
) -> Result<()> {
    let wire = super::access_validation::Wire::read_attachment(
        report,
        root,
        "ipc_frames",
        "ipc-frames.jsonl",
    )?;
    let requests = wire
        .rows
        .iter()
        .filter(|row| row["direction"] == "client_to_server" && row["frame"]["method"] != "hello")
        .collect::<Vec<_>>();
    let recorded_requests = report
        .observations
        .iter()
        .filter(|row| row.kind == "rpc_request")
        .collect::<Vec<_>>();
    let terminals = report
        .observations
        .iter()
        .filter(|row| row.kind == "rpc_terminal")
        .collect::<Vec<_>>();
    let hellos = wire.requests("hello");
    let final_states = report
        .observations
        .iter()
        .filter(|row| row.kind == "completed_sdk_session")
        .collect::<Vec<_>>();
    let pages = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "successful_ipc_pipeline")
        .collect::<Vec<_>>();
    let sdk_pages = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "completed_sdk")
        .collect::<Vec<_>>();
    if hellos.len() != 1
        || final_states.len() != 1
        || requests.len() != 3 + pages.len()
        || recorded_requests.len() != requests.len()
        || terminals.len() != requests.len()
        || pages.is_empty()
        || !pages.last().unwrap().records.is_empty()
        || sdk_pages.len() != 1
        || sdk_pages[0].after != 0
        || sdk_pages[0].records != events
    {
        return Err(
            "IPC lifecycle lost its original requests, replies or complete event pages".into(),
        );
    }
    let final_value = &final_states[0].value;
    let final_state: cayw_runtime::SessionView = serde_json::from_value(final_value.clone())?;
    if final_state.id != events[0].session_id
        || final_state.cursor != events.last().unwrap().cursor
        || !super::scenarios::clean(&final_state)
        || final_state.frozen
        || final_state.binding.is_some()
        || sdk_pages[0].session_id != final_state.id
    {
        return Err("IPC lifecycle final SDK state differs from its original event history".into());
    }
    let finished = events
        .iter()
        .filter(|record| matches!(&record.event, CoreEvent::AttemptFinished { .. }))
        .collect::<Vec<_>>();
    if finished.len() != 2 {
        return Err("IPC lifecycle requires its two original completed attempts".into());
    }
    let mut after = 0;
    let mut received = Vec::new();
    let mut previous_reply = wire.reply(hellos[0])?["sequence"]
        .as_u64()
        .ok_or("hello reply sequence absent")?;
    for (index, ((request, recorded), terminal)) in requests
        .iter()
        .zip(recorded_requests)
        .zip(terminals)
        .enumerate()
    {
        let frame = &request["frame"];
        let reply = wire.reply(request)?;
        let result = reply["frame"]
            .get("result")
            .ok_or("successful IPC lifecycle has no original result")?;
        let request_sequence = request["sequence"]
            .as_u64()
            .ok_or("request sequence absent")?;
        if request["connection"] != hellos[0]["connection"]
            || request_sequence <= previous_reply
            || recorded.value
                != serde_json::json!({"method":frame["method"],"params":frame["params"]})
            || terminal.value
                != serde_json::json!({"method":frame["method"],"result":{"Ok":result}})
            || recorded.sequence >= terminal.sequence
        {
            return Err(
                "IPC lifecycle request or terminal differs from its original wire record".into(),
            );
        }
        previous_reply = reply["sequence"].as_u64().ok_or("reply sequence absent")?;
        if index == 0 {
            let created: cayw_runtime::SessionView = serde_json::from_value(result.clone())?;
            if frame["method"] != "session.create"
                || frame["params"] != serde_json::json!({"label":"IPC successful pipeline"})
                || created.id != final_state.id
                || created.label != "IPC successful pipeline"
                || created.cursor != events[0].cursor
                || !matches!(&events[0].event,CoreEvent::SessionCreated {label} if label == &created.label)
                || !super::scenarios::clean(&created)
                || !created.messages.is_empty()
                || !created.executions.is_empty()
                || created.frozen
                || created.binding.is_some()
            {
                return Err(
                    "IPC lifecycle creation result differs from its original session".into(),
                );
            }
        } else if index <= 2 {
            let state: cayw_runtime::SessionView = serde_json::from_value(result.clone())?;
            let end = finished[index - 1];
            let executions = events
                .iter()
                .filter(|record| record.cursor <= end.cursor)
                .filter_map(|record| match &record.event {
                    CoreEvent::ExecutionFinished { execution } => Some(execution.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if frame["method"] != "session.prompt"
                || frame["params"]
                    != serde_json::json!({"session_id":final_state.id,"text":report.specification["input"]["prompt"]})
                || state.id != final_state.id
                || state.label != final_state.label
                || state.cursor != end.cursor
                || !super::scenarios::clean(&state)
                || state.executions != executions
                || state.frozen
                || state.binding.is_some()
                || (index == 2 && result != final_value)
                || !final_state.messages.starts_with(&state.messages)
            {
                return Err(
                    "IPC lifecycle prompt reply differs from its original completed attempt".into(),
                );
            }
        } else {
            let page = pages[index - 3];
            if frame["method"] != "session.events"
                || frame["params"]
                    != serde_json::json!({"session_id":final_state.id,"after":after,"limit":2,"wait_ms":0})
                || page.session_id != final_state.id
                || page.after != after
                || result != &serde_json::to_value(&page.records)?
            {
                return Err(
                    "IPC lifecycle page differs from its original request or response".into(),
                );
            }
            after = page.records.last().map_or(after, |record| record.cursor);
            received.extend(page.records.clone());
        }
    }
    if received != events {
        return Err("IPC lifecycle pages do not retain the complete original SDK history".into());
    }
    Ok(())
}

fn validate_event_reconnect(
    report: &ScenarioReport,
    root: &Path,
    events: &[EventRecord],
) -> Result<()> {
    let saved: Value = serde_json::from_slice(&required_attachment(report, root, "cursor")?)?;
    let cursor = saved["cursor"]
        .as_u64()
        .ok_or("saved event cursor absent")?;
    let session = saved["session_id"]
        .as_str()
        .ok_or("saved event session absent")?;
    let first: Vec<_> = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "before_event_disconnect")
        .collect();
    let resumed: Vec<_> = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "event_connection_resumed")
        .collect();
    let final_pages: Vec<_> = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "final_sdk")
        .collect();
    if first.len() != 1
        || first[0].after != 0
        || first[0].records.len() != 1
        || first[0].session_id != session
        || first[0].records[0].cursor != cursor
        || first[0].records[0] != events[0]
        || final_pages.len() != 1
        || final_pages[0].after != 0
        || final_pages[0].session_id != session
        || final_pages[0].records != events
        || resumed.is_empty()
        || !resumed.last().unwrap().records.is_empty()
    {
        return Err("event reconnect lost its original saved page or SDK history".into());
    }
    let mut after = cursor;
    let mut suffix = Vec::new();
    for page in &resumed {
        if page.session_id != session || page.after != after {
            return Err("event reconnect cursor chain differs from disk".into());
        }
        after = page.records.last().map_or(after, |record| record.cursor);
        suffix.extend(page.records.clone());
    }
    let expected: Vec<_> = events
        .iter()
        .filter(|record| record.cursor > cursor)
        .cloned()
        .collect();
    if expected.is_empty() || suffix != expected {
        return Err("event reconnect suffix differs from original SDK records".into());
    }
    let frames = super::fault_validation::wire(report, root)?;
    let prompts: Vec<_> = frames
        .iter()
        .filter(|frame| {
            frame["direction"] == "client_to_server" && frame["frame"]["method"] == "session.prompt"
        })
        .collect();
    let requests: Vec<_> = frames
        .iter()
        .filter(|frame| {
            frame["direction"] == "client_to_server" && frame["frame"]["method"] == "session.events"
        })
        .collect();
    if prompts.len() != 1
        || requests.len() != resumed.len() + 1
        || requests[0]["frame"]["params"]
            != serde_json::json!({"session_id":session,"after":0,"limit":1,"wait_ms":0})
    {
        return Err("event reconnect has missing or replacement requests".into());
    }
    let execution = &prompts[0]["connection"];
    let initial = &requests[0]["connection"];
    let fresh = &requests[1]["connection"];
    if !execution.is_u64()
        || !initial.is_u64()
        || !fresh.is_u64()
        || execution == initial
        || execution == fresh
        || initial == fresh
        || prompts[0]["frame"]["params"]
            != serde_json::json!({"session_id":session,"text":report.specification["input"]["prompt"]})
    {
        return Err("event requests did not use independent authenticated connections".into());
    }
    for (request, page) in requests
        .iter()
        .zip(std::iter::once(first[0]).chain(resumed.iter().copied()))
    {
        let connection = if page.stage == "before_event_disconnect" {
            initial
        } else {
            fresh
        };
        if &request["connection"] != connection
            || request["frame"]["params"]["session_id"] != session
            || request["frame"]["params"]["after"] != page.after
        {
            return Err("event request contradicts its saved page".into());
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
            return Err("event page has no identical original wire response".into());
        }
    }
    let replies: Vec<_> = frames
        .iter()
        .filter(|frame| {
            frame["direction"] == "server_to_client"
                && &frame["connection"] == execution
                && frame["frame"]["id"] == prompts[0]["frame"]["id"]
        })
        .collect();
    let before: Vec<_> = report
        .observations
        .iter()
        .filter(|row| row.kind == "before_event_disconnect_session")
        .collect();
    let during: Vec<_> = report
        .observations
        .iter()
        .filter(|row| row.kind == "after_event_disconnect_session")
        .collect();
    let final_state: Vec<_> = report
        .observations
        .iter()
        .filter(|row| row.kind == "final_sdk_session")
        .collect();
    if replies.len() != 1
        || before.len() != 1
        || during.len() != 1
        || final_state.len() != 1
        || before[0].value != during[0].value
        || before[0].value["attempt"].is_null()
        || replies[0]["frame"]["result"] != final_state[0].value
        || !frames.iter().any(|frame| {
            frame["direction"] == "relay_terminal"
                && &frame["connection"] == initial
                && frame["sequence"].as_u64() < replies[0]["sequence"].as_u64()
        })
        || frames
            .iter()
            .any(|frame| frame["frame"]["method"] == "rpc.cancel")
    {
        return Err("event socket loss did not preserve the original completed request".into());
    }
    Ok(())
}

fn original_lease(record: &EventRecord) -> Result<&AttemptLease> {
    let lease = record
        .attempt
        .as_ref()
        .ok_or("event has no original attempt")?;
    if lease.session_id != record.session_id
        || lease.attempt_id.is_empty()
        || lease.worker_id.is_empty()
        || lease.fence == 0
    {
        return Err("event attempt does not identify its original executor".into());
    }
    Ok(lease)
}

pub(super) fn validate_calls(
    events: &[EventRecord],
    entries: &[Value],
    report: &ScenarioReport,
    runs: usize,
) -> Result<()> {
    let mut declared = BTreeMap::<String, (ToolCall, AttemptLease)>::new();
    let mut active = BTreeMap::<String, (ToolCall, AttemptLease)>::new();
    let mut results = BTreeMap::<String, Value>::new();
    let mut executions = BTreeMap::<String, ExecutionRecord>::new();
    let mut completed = BTreeSet::new();
    let mut pages = BTreeMap::<String, Value>::new();
    let mut waits = BTreeMap::<String, cayw_core::ExecutionWaitRecord>::new();
    let mut attempt_ends = BTreeSet::new();
    for record in events {
        match &record.event {
            CoreEvent::Agent { event } => {
                let lease = original_lease(record)?;
                let event = serde_json::to_value(event)?;
                match event["type"].as_str() {
                    Some("message_end") => {
                        let message: Message = serde_json::from_value(event["message"].clone())?;
                        match message {
                            Message::Assistant(message) => {
                                for call in message.tool_calls() {
                                    if call.name != "execute_code"
                                        || call.arguments_error.is_some()
                                        || declared
                                            .insert(call.id.clone(), (call, lease.clone()))
                                            .is_some()
                                    {
                                        return Err(
                                            "assistant tool declaration is invalid or duplicated"
                                                .into(),
                                        );
                                    }
                                }
                            }
                            Message::ToolResult(message) => {
                                let (call, origin) = declared
                                    .get(&message.tool_call_id)
                                    .ok_or("undeclared tool result")?;
                                if origin != lease
                                    || !result_matches(
                                        call,
                                        results
                                            .get(&call.id)
                                            .ok_or("result precedes tool completion")?,
                                        &message,
                                    )
                                {
                                    return Err("durable tool message contradicts the original call or result".into());
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("tool_execution_start") => {
                        let call: ToolCall = serde_json::from_value(event["call"].clone())?;
                        if declared.get(&call.id) != Some(&(call.clone(), lease.clone()))
                            || results.contains_key(&call.id)
                            || active
                                .insert(call.id.clone(), (call, lease.clone()))
                                .is_some()
                        {
                            return Err(
                                "started tool differs from its original assistant declaration"
                                    .into(),
                            );
                        }
                    }
                    Some("tool_execution_end") => {
                        let call: ToolCall = serde_json::from_value(event["call"].clone())?;
                        if active.remove(&call.id) != Some((call.clone(), lease.clone()))
                            || event["result"]["is_error"] != false
                            || event["result"]["outcome_unknown"] != false
                            || results
                                .insert(call.id.clone(), event["result"].clone())
                                .is_some()
                        {
                            return Err(
                                "tool result has no identical original call and executor".into()
                            );
                        }
                        let page = &event["result"]["structured_content"];
                        validate_page(page)?;
                        let cell = page["cell_id"].as_str().unwrap();
                        let execution = executions
                            .get(cell)
                            .ok_or("tool page has no original execution")?;
                        if &execution.lease != lease {
                            return Err("tool page belongs to another executor".into());
                        }
                        match call.arguments["action"].as_str() {
                            Some("exec")
                                if execution.parent_call_id == call.id
                                    && !pages.contains_key(cell)
                                    && page["cursor"] == 0 => {}
                            Some("wait") => {
                                let previous = pages
                                    .get(cell)
                                    .ok_or("wait has no previously delivered page")?;
                                let binding = waits
                                    .remove(&call.id)
                                    .ok_or("wait has no durable execution binding")?;
                                if binding.execution_id != cell
                                    || binding.task_id != execution.task_id
                                    || binding.lease != *lease
                                    || binding.arguments != call.arguments
                                    || call.arguments["cell_id"] != cell
                                    || call.arguments["cursor"] != previous["next_cursor"]
                                    || page["cursor"] != previous["next_cursor"]
                                    || page_complete(previous)
                                {
                                    return Err(
                                        "wait cursor or origin contradicts its previous page"
                                            .into(),
                                    );
                                }
                            }
                            _ => {
                                return Err(
                                    "tool page belongs to another original execution".into()
                                );
                            }
                        }
                        pages.insert(cell.to_owned(), page.clone());
                    }
                    Some("agent_end") => {
                        if !active.is_empty() || !attempt_ends.insert(lease.attempt_id.clone()) {
                            return Err("agent ended with unfinished or duplicate work".into());
                        }
                        let messages: Vec<Message> =
                            serde_json::from_value(event["messages"].clone())?;
                        if !matches!(messages.last(), Some(Message::Assistant(message))
                            if message.stop_reason == cayw_ai::StopReason::Stop && message.error_message.is_none())
                        {
                            return Err(
                                "successful task lacks a normal final assistant response".into()
                            );
                        }
                    }
                    _ => {}
                }
            }
            CoreEvent::ExecutionStarted { execution } => {
                let lease = original_lease(record)?;
                let (call, origin) = active
                    .get(&execution.parent_call_id)
                    .ok_or("execution has no active parent tool")?;
                let script = call.arguments["script"]
                    .as_str()
                    .ok_or("execution script absent")?;
                if origin != lease
                    || &execution.lease != lease
                    || execution.task_id.is_empty()
                    || call.arguments["action"] != "exec"
                    || execution.state != ExecutionState::Running
                    || execution.terminal.is_some()
                    || execution.source_hash != format!("sha256:{}", sha256(script.as_bytes()))
                    || executions
                        .insert(execution.execution_id.clone(), execution.clone())
                        .is_some()
                {
                    return Err(
                        "execution origin contradicts the active tool, task or source".into(),
                    );
                }
            }
            CoreEvent::ExecutionWaitRegistered { binding } => {
                let execution = executions
                    .get(&binding.execution_id)
                    .ok_or("wait refers to an unknown execution")?;
                let (call, lease) = active
                    .get(&binding.call_id)
                    .ok_or("wait has no active original tool")?;
                if &binding.lease != original_lease(record)?
                    || &binding.lease != lease
                    || execution.lease != binding.lease
                    || execution.task_id != binding.task_id
                    || binding.arguments != call.arguments
                    || call.arguments["action"] != "wait"
                    || call.arguments["cell_id"] != execution.execution_id
                    || waits
                        .insert(binding.call_id.clone(), binding.clone())
                        .is_some()
                {
                    return Err(
                        "wait execution binding contradicts its original task and arguments".into(),
                    );
                }
            }
            CoreEvent::ExecutionFinished { execution } => {
                let original = executions
                    .get(&execution.execution_id)
                    .ok_or("execution terminal has no start")?;
                if original.parent_call_id != execution.parent_call_id
                    || original.task_id != execution.task_id
                    || original.source_hash != execution.source_hash
                    || original.lease != execution.lease
                    || &execution.lease != original_lease(record)?
                    || execution.state != ExecutionState::Completed
                    || !matches!(
                        &execution.terminal,
                        Some(cayw_core::ExecutionOutcome::Completed { .. })
                    )
                    || !completed.insert(execution.execution_id.clone())
                {
                    return Err("execution terminal contradicts its original identity or successful outcome".into());
                }
            }
            CoreEvent::CapabilityPlanned { call }
            | CoreEvent::CapabilityStarted { call }
            | CoreEvent::CapabilitySettled { call } => {
                let execution = executions
                    .get(&call.key.execution_id)
                    .ok_or("capability has no original execution")?;
                if call.lease != execution.lease
                    || call.task_id != execution.task_id
                    || call.parent_call_id != execution.parent_call_id
                    || call.source_hash != execution.source_hash
                    || &call.lease != original_lease(record)?
                {
                    return Err("capability belongs to another task, executor or source".into());
                }
            }
            _ => {}
        }
    }
    if !active.is_empty()
        || !waits.is_empty()
        || declared.len() != results.len()
        || executions.len() != runs
        || completed.len() != runs
        || pages.len() != runs
        || pages.values().any(|page| !page_complete(page))
        || attempt_ends.len() != runs
    {
        return Err("pipeline tool pairing or completed cells are incomplete".into());
    }
    let tasks: Vec<&Value> = report
        .observations
        .iter()
        .filter(|o| o.kind.ends_with("_tasks"))
        .filter_map(|o| o.value.as_array())
        .flatten()
        .collect();
    for execution in executions.values() {
        if !tasks.iter().any(|task| {
            task["id"] == execution.task_id
                && task["session_id"] == execution.lease.session_id
                && task["attempt_id"] == execution.lease.attempt_id
                && task["worker_id"] == execution.lease.worker_id
                && task["state"] == "completed"
                && task["text"] == report.specification["input"]["prompt"]
        }) {
            return Err("execution has no matching durable completed task".into());
        }
    }
    let mut seen_results = BTreeSet::new();
    for entry in entries {
        let request: ModelRequest = serde_json::from_value(entry["request"].clone())?;
        for message in &request.messages {
            match message {
                Message::Assistant(message) => {
                    for call in message.tool_calls() {
                        if declared
                            .get(&call.id)
                            .is_none_or(|(original, _)| original != &call)
                        {
                            return Err("Provider history changed an executed tool call".into());
                        }
                    }
                }
                Message::ToolResult(message) => {
                    let (call, _) = declared
                        .get(&message.tool_call_id)
                        .ok_or("Provider history contains an unrelated result")?;
                    if !result_matches(
                        call,
                        results
                            .get(&call.id)
                            .ok_or("Provider result lacks execution")?,
                        message,
                    ) {
                        return Err("Provider history changed an actual tool result".into());
                    }
                    seen_results.insert(call.id.clone());
                }
                _ => {}
            }
        }
    }
    if seen_results.len() != results.len() {
        return Err("Provider did not consume every original tool result".into());
    }
    Ok(())
}

fn result_matches(call: &ToolCall, result: &Value, message: &cayw_ai::ToolResultMessage) -> bool {
    message.tool_name == call.name
        && message.tool_call_id == call.id
        && serde_json::to_value(&message.content).is_ok_and(|v| v == result["content"])
        && serde_json::to_value(&message.details).is_ok_and(|v| v == result["details"])
        && serde_json::to_value(&message.structured_content)
            .is_ok_and(|v| v == result["structured_content"])
        && serde_json::to_value(&message.usage).is_ok_and(|v| v == result["usage"])
        && message.is_error == (result["is_error"] == true)
        && message.outcome_unknown == (result["outcome_unknown"] == true)
}

fn page_complete(page: &Value) -> bool {
    page["status"] == "completed"
        && page["vm_alive"] == false
        && page["has_more"] == false
        && page["outcome_unknown"] == false
        && page["error"].is_null()
        && page["required_budget"].is_null()
}

fn validate_page(page: &Value) -> Result<()> {
    let live = match page["status"].as_str() {
        Some("completed") => false,
        Some("not_started" | "running" | "yielded") => true,
        _ => return Err("task page has an unsuccessful execution status".into()),
    };
    if page["cell_id"].as_str().is_none_or(str::is_empty)
        || page["vm_alive"] != live
        || !page["has_more"].is_boolean()
        || page["outcome_unknown"] != false
        || !page["error"].is_null()
        || (!page["required_budget"].is_null()
            && (page["has_more"] != true || !page["required_budget"].is_u64()))
    {
        return Err("task page has inconsistent VM, error or output state".into());
    }
    let mut cursor = page["cursor"].as_u64().ok_or("page cursor absent")?;
    for output in page["outputs"].as_array().ok_or("page outputs absent")? {
        cursor = cursor.checked_add(1).ok_or("page cursor overflow")?;
        if output["cursor"] != cursor {
            return Err("page output cursors are incomplete".into());
        }
    }
    if page["next_cursor"] != cursor {
        return Err("page next cursor contradicts its outputs".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Provider};
    use serde_json::json;
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    struct PagesProvider {
        script: String,
        journal: PathBuf,
        calls: Mutex<usize>,
        running_first_page: bool,
    }
    impl Provider for PagesProvider {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let entry = json!({"call":*calls,"request":request});
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.journal)
                .unwrap();
            writeln!(file, "{entry}").unwrap();
            let mut message = cayw_runtime::fixture::script_response(
                &request,
                &self.script,
                &format!("page-{}", *calls),
            );
            if self.running_first_page
                && *calls == 1
                && let Some(cayw_ai::Content::ToolCall(call)) = message.content.first_mut()
            {
                call.arguments["yield_time_ms"] = 0.into();
            }
            Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
        }
    }
    async fn recorded(
        script: String,
        running: bool,
    ) -> (PathBuf, Vec<EventRecord>, Vec<Value>, ScenarioReport) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cw-pages-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("workspace")).unwrap();
        let provider = Arc::new(PagesProvider {
            script,
            journal: root.join("provider.jsonl"),
            calls: Mutex::new(0),
            running_first_page: running,
        });
        let mut config = super::super::scenarios::config(false, &root);
        config.cells.max_output_tokens = 16 * 1024;
        config.cells.item_bytes = 12 * 1024;
        config.cells.max_yield_time = Duration::from_millis(50);
        let runtime = cayw_runtime::Runtime::open(
            root.join("session.sqlite"),
            root.join("workspace"),
            provider,
            super::super::provider::model(),
            config,
        )
        .unwrap();
        let id = runtime.create(None).unwrap().id;
        let view = runtime
            .prompt(
                &id,
                super::super::provider::PROMPT,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(super::super::scenarios::clean(&view), "{view:?}");
        let mut report = ScenarioReport::new(
            "page-test",
            1,
            json!({"input":{"prompt":super::super::provider::PROMPT}}),
            Value::Null,
        );
        let events =
            super::super::scenarios::capture(&runtime, &id, "actual", &mut report).unwrap();
        let entries = fs::read_to_string(root.join("provider.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        (root, events, entries, report)
    }
    fn pages(events: &[EventRecord]) -> Vec<Value> {
        events
            .iter()
            .filter_map(|record| {
                let event = serde_json::to_value(&record.event).unwrap();
                (event["type"] == "agent" && event["event"]["type"] == "tool_execution_end").then(
                    || {
                        assert_eq!(
                            event["event"]["result"]["is_error"], false,
                            "{}",
                            event["event"]
                        );
                        event["event"]["result"]["structured_content"].clone()
                    },
                )
            })
            .collect()
    }
    #[tokio::test]
    async fn completed_intermediate_pages_are_valid_when_the_last_page_is_consumed() {
        let script = format!(
            "for i=1,12 do text(string.rep('x',3500)) end; {}",
            super::super::provider::PIPELINE
        );
        let (root, events, entries, report) = recorded(script, false).await;
        let pages = pages(&events);
        assert!(
            pages
                .iter()
                .any(|page| page["status"] == "completed" && page["has_more"] == true),
            "{pages:?}"
        );
        assert_eq!(pages.last().unwrap()["has_more"], false);
        validate_calls(&events, &entries, &report, 1).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn live_initial_page_is_valid_when_original_wait_finishes() {
        let (root, events, entries, report) =
            recorded(super::super::provider::PIPELINE.into(), true).await;
        let pages = pages(&events);
        assert!(
            pages.iter().any(|page| matches!(
                page["status"].as_str(),
                Some("not_started" | "running")
            ) && page["vm_alive"] == true),
            "{pages:?}"
        );
        validate_calls(&events, &entries, &report, 1).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    fn transform(value: &mut Value, change: &mut impl FnMut(&mut Value)) {
        change(value);
        match value {
            Value::Object(fields) => {
                for child in fields.values_mut() {
                    transform(child, change);
                }
            }
            Value::Array(values) => {
                for child in values {
                    transform(child, change);
                }
            }
            _ => {}
        }
    }
    #[tokio::test]
    async fn missing_final_wait_result_cannot_pass() {
        let (root, mut events, entries, report) =
            recorded(super::super::provider::PIPELINE.into(), true).await;
        let last = events
            .iter()
            .rposition(|record| {
                let event = serde_json::to_value(&record.event).unwrap();
                event["type"] == "agent" && event["event"]["type"] == "tool_execution_end"
            })
            .unwrap();
        events.remove(last);
        assert!(validate_calls(&events, &entries, &report, 1).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn wait_must_consume_the_previous_page_cursor_on_the_same_cell() {
        let (root, events, entries, report) =
            recorded(super::super::provider::PIPELINE.into(), true).await;
        validate_calls(&events, &entries, &report, 1).unwrap();
        for field in ["cursor", "cell_id"] {
            let mut encoded = serde_json::to_value(&events).unwrap();
            transform(&mut encoded, &mut |value| {
                if value["action"] == "wait" {
                    value[field] = if field == "cursor" {
                        json!(999)
                    } else {
                        json!("another-cell")
                    };
                }
            });
            let damaged: Vec<EventRecord> = serde_json::from_value(encoded).unwrap();
            assert!(
                validate_calls(&damaged, &entries, &report, 1).is_err(),
                "{field}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn final_pages_cannot_remain_live_yielded_unread_failed_or_unknown() {
        let (root, events, entries, report) =
            recorded(super::super::provider::PIPELINE.into(), true).await;
        validate_calls(&events, &entries, &report, 1).unwrap();
        for (status, live, more, unknown) in [
            ("running", true, false, false),
            ("yielded", true, false, false),
            ("completed", false, true, false),
            ("failed", false, false, false),
            ("outcome_unknown", false, false, true),
        ] {
            let mut encoded = serde_json::to_value(&events).unwrap();
            let mut journal = serde_json::to_value(&entries).unwrap();
            let mut change = |value: &mut Value| {
                if value["cell_id"].is_string()
                    && value["status"] == "completed"
                    && value["outputs"].is_array()
                {
                    value["status"] = status.into();
                    value["vm_alive"] = live.into();
                    value["has_more"] = more.into();
                    value["outcome_unknown"] = unknown.into();
                }
            };
            transform(&mut encoded, &mut change);
            transform(&mut journal, &mut change);
            let damaged: Vec<EventRecord> = serde_json::from_value(encoded).unwrap();
            let journal: Vec<Value> = serde_json::from_value(journal).unwrap();
            assert!(
                validate_calls(&damaged, &journal, &report, 1).is_err(),
                "{status}/{more}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn yielded_intermediate_page_is_valid_when_explicit_wait_completes() {
        let script = format!("yield_control(); {}", super::super::provider::PIPELINE);
        let (root, events, entries, report) = recorded(script, false).await;
        let pages = pages(&events);
        assert!(
            pages
                .iter()
                .any(|page| page["status"] == "yielded" && page["vm_alive"] == true),
            "{pages:?}"
        );
        validate_calls(&events, &entries, &report, 1).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
