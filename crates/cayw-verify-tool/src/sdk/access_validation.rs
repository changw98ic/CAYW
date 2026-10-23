//! Rechecks access evidence from original bytes, durable records and file effects.
use super::{
    Result, ScenarioReport, contiguous_events,
    provider::{CONTENT, PIPELINE, PROMPT, model, write_arguments},
    validation::{required_attachment, validate_calls},
};
use crate::{evidence::attachment, measurement::sha256};
use cayw_core::{
    ApprovalDecision, ApprovalRequest, ApprovalTarget, AttemptLease, CapabilityAuthorization,
    CapabilityCallRecord, CapabilityOutcome, CapabilityStage, CoreEvent, EventRecord,
};
use cayw_runtime::{AuditRecord, GrantRecord, SessionView, TaskRecord, TaskState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::SocketAddr,
    path::{Component, Path},
};

fn ensure(condition: bool, reason: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(reason.into())
    }
}
fn observed<'a>(report: &'a ScenarioReport, kind: &str) -> Vec<&'a Value> {
    report
        .observations
        .iter()
        .filter(|row| row.kind == kind)
        .map(|row| &row.value)
        .collect()
}
fn one<'a>(report: &'a ScenarioReport, kind: &str) -> Result<&'a Value> {
    let values = observed(report, kind);
    ensure(
        values.len() == 1,
        &format!("missing or duplicate access observation: {kind}"),
    )?;
    Ok(values[0])
}
fn json_lines(bytes: &[u8]) -> Result<Vec<Value>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    ensure(
        bytes.ends_with(b"\n"),
        "access journal has no final newline",
    )?;
    Ok(std::str::from_utf8(bytes)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?)
}
fn fields(value: &Value, expected: &[&str]) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
    })
}
fn byte_record(value: &Value) -> Result<Vec<u8>> {
    ensure(
        fields(value, &["bytes", "length", "sha256", "utf8"]),
        "raw byte record is incomplete",
    )?;
    let bytes: Vec<u8> = serde_json::from_value(value["bytes"].clone())?;
    ensure(
        value["length"] == bytes.len()
            && value["sha256"] == sha256(&bytes)
            && value["utf8"] == serde_json::to_value(String::from_utf8(bytes.clone()).ok())?,
        "raw access bytes contradict their length, digest or UTF-8",
    )?;
    Ok(bytes)
}
fn file_bytes(value: &Value) -> Result<Vec<u8>> {
    ensure(
        fields(value, &["bytes", "sha256", "utf8"]),
        "file byte evidence is incomplete",
    )?;
    let bytes: Vec<u8> = serde_json::from_value(value["bytes"].clone())?;
    ensure(
        value["sha256"] == sha256(&bytes)
            && value["utf8"] == serde_json::to_value(String::from_utf8(bytes.clone()).ok())?,
        "file bytes contradict their original digest or UTF-8",
    )?;
    Ok(bytes)
}
fn wire_json(bytes: &[u8], newline: bool) -> Result<Value> {
    ensure(
        bytes.ends_with(b"\n") == newline
            && bytes.iter().filter(|byte| **byte == b'\n').count() == usize::from(newline),
        "raw access frame has a changed newline boundary",
    )?;
    let value: Value = serde_json::from_slice(bytes)?;
    ensure(value.is_object(), "access frame is not an object")?;
    Ok(value)
}

pub(super) struct Wire {
    pub(super) rows: Vec<Value>,
}
impl Wire {
    fn read(report: &ScenarioReport, root: &Path) -> Result<Self> {
        Self::read_attachment(report, root, "access_ipc_frames", "access-ipc-frames.jsonl")
    }
    pub(super) fn read_attachment(
        report: &ScenarioReport,
        root: &Path,
        name: &str,
        path: &str,
    ) -> Result<Self> {
        ensure(
            report.artifacts[name]["path"] == path,
            "access frame attachment changed path",
        )?;
        let rows = json_lines(&required_attachment(report, root, name)?)?;
        ensure(!rows.is_empty(), "access IPC frame journal is empty")?;
        let mut requests = BTreeMap::new();
        let mut replies = BTreeSet::new();
        let mut authenticated = BTreeSet::new();
        let mut closed = BTreeSet::new();
        let mut elapsed = 0;
        for (index, row) in rows.iter().enumerate() {
            let connection = row["connection"]
                .as_u64()
                .filter(|id| *id > 0)
                .ok_or("access connection identity absent")?;
            let now = row["elapsed_ns"]
                .as_u64()
                .ok_or("wire elapsed time absent")?;
            let raw = row["utf8"]
                .as_str()
                .ok_or("original access frame bytes absent")?
                .as_bytes();
            ensure(
                row["sequence"] == index + 1
                    && now >= elapsed
                    && row["bytes"] == raw.len()
                    && row["sha256"] == sha256(raw),
                "access wire byte digest, length or order changed",
            )?;
            elapsed = now;
            ensure(
                !closed.contains(&connection),
                "IPC frames appear after connection closure",
            )?;
            let direction = row["direction"]
                .as_str()
                .ok_or("access frame direction absent")?;
            let decoded = wire_json(raw, direction != "relay_terminal")?;
            ensure(
                row["frame"] == decoded,
                "decoded IPC frame differs from its original bytes",
            )?;
            if direction == "relay_terminal" {
                ensure(
                    decoded == json!({"result":{"Ok":null}}),
                    "access relay failed or lacks its normal terminal",
                )?;
                closed.insert(connection);
                continue;
            }
            ensure(
                decoded["version"] == cayw_ipc::PROTOCOL_VERSION,
                "access frame changed IPC version",
            )?;
            let id = decoded["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or("wire request identity absent")?;
            let key = (connection, id.to_owned());
            match direction {
                "client_to_server" => {
                    ensure(
                        fields(&decoded, &["version", "id", "method", "params"])
                            && decoded["method"].is_string()
                            && requests.insert(key, index).is_none(),
                        "IPC request has changed fields or a repeated identity",
                    )?;
                    if decoded["method"] == "hello" {
                        ensure(
                            authenticated.insert(connection)
                                && decoded["params"]
                                    == json!({"token":super::scenarios::ipc_config().token}),
                            "access IPC authentication is missing or changed",
                        )?;
                    } else {
                        ensure(
                            authenticated.contains(&connection),
                            "access request precedes authentication",
                        )?;
                    }
                }
                "server_to_client" => {
                    let original = *requests.get(&key).ok_or("unpaired access IPC response")?;
                    ensure(
                        original < index && replies.insert(key),
                        "access IPC response is duplicated or out of order",
                    )?;
                    ensure(
                        fields(&decoded, &["version", "id", "result"])
                            || fields(&decoded, &["version", "id", "error"]),
                        "IPC response has an ambiguous result or error",
                    )?;
                    if rows[original]["frame"]["method"] == "hello" {
                        ensure(
                            decoded["result"] == hello_result(report),
                            "access hello result contradicts the server configuration",
                        )?;
                    }
                }
                _ => return Err("unknown access wire direction".into()),
            }
        }
        ensure(
            requests.len() == replies.len() && authenticated == closed,
            "access IPC journal lost a response or connection terminal",
        )?;
        Ok(Self { rows })
    }
    pub(super) fn requests(&self, method: &str) -> Vec<&Value> {
        self.rows
            .iter()
            .filter(|row| {
                row["direction"] == "client_to_server" && row["frame"]["method"] == method
            })
            .collect()
    }
    pub(super) fn reply<'a>(&'a self, request: &Value) -> Result<&'a Value> {
        let replies = self
            .rows
            .iter()
            .filter(|row| {
                row["direction"] == "server_to_client"
                    && row["connection"] == request["connection"]
                    && row["frame"]["id"] == request["frame"]["id"]
            })
            .collect::<Vec<_>>();
        ensure(
            replies.len() == 1,
            "access request has no unique original reply",
        )?;
        Ok(replies[0])
    }
    fn only(&self, method: &str) -> Result<&Value> {
        let requests = self.requests(method);
        ensure(requests.len() == 1, "access method is missing or repeated")?;
        Ok(requests[0])
    }
}
fn hello_result(report: &ScenarioReport) -> Value {
    let configuration = &report.specification["configuration"]["ipc"]["server"];
    json!({"version":cayw_ipc::PROTOCOL_VERSION,"max_frame_bytes":configuration["max_frame_bytes"],
        "max_in_flight":configuration["max_in_flight"],"max_requests_per_connection":configuration["max_requests_per_connection"],
        "max_id_bytes":configuration["max_id_bytes"]})
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    session: SessionView,
    sessions: Vec<SessionView>,
    tasks: Vec<TaskRecord>,
    events: Vec<EventRecord>,
    audit: Vec<AuditRecord>,
    grants: Vec<GrantRecord>,
    approvals: Vec<ApprovalRequest>,
    health: Value,
    provider_calls: u64,
    provider_requests: Vec<Value>,
    file: Value,
    workspace: Vec<Value>,
}
impl Snapshot {
    fn read(value: &Value, report: &ScenarioReport, entries: &[Value]) -> Result<Self> {
        let snapshot: Self = serde_json::from_value(value.clone())?;
        ensure(
            serde_json::to_value(&snapshot)? == *value
                && snapshot.sessions == vec![snapshot.session.clone()]
                && contiguous_events(
                    &snapshot.session.id,
                    0,
                    &snapshot.events,
                    snapshot.session.cursor,
                )
                && !snapshot.events.is_empty(),
            "access snapshot is incomplete or contradicts its Session/event identities",
        )?;
        ensure(
            usize::try_from(snapshot.provider_calls)? == snapshot.provider_requests.len()
                && entries.starts_with(&snapshot.provider_requests),
            "snapshot lost an original Provider request",
        )?;
        let health = &snapshot.health;
        ensure(
            fields(
                health,
                &[
                    "status",
                    "protocol_version",
                    "running",
                    "queued",
                    "max_concurrent_runs",
                    "provider",
                    "model",
                ],
            ) && health["status"] == "ok"
                && health["protocol_version"] == cayw_core::PROTOCOL_VERSION
                && health["provider"] == report.specification["input"]["provider"]["provider"]
                && health["model"] == report.specification["input"]["provider"]["id"]
                && health["max_concurrent_runs"]
                    == report.specification["configuration"]["runtime"]["max_concurrent_runs"]
                && health["queued"] == 0,
            "access health contradicts the original Runtime configuration",
        )?;
        for (index, audit) in snapshot.audit.iter().enumerate() {
            ensure(
                audit.cursor == index as u64 + 1 && audit.session_id == snapshot.session.id,
                "access audit has gaps or changed Session identity",
            )?;
        }
        if !snapshot.file.is_null() {
            file_bytes(&snapshot.file)?;
        }
        let mut paths = BTreeSet::new();
        for entry in &snapshot.workspace {
            let path = entry["path"]
                .as_str()
                .ok_or("workspace entry path absent")?;
            ensure(
                !path.is_empty()
                    && Path::new(path)
                        .components()
                        .all(|component| matches!(component, Component::Normal(_)))
                    && paths.insert(path),
                "workspace entry path is unsafe or repeated",
            )?;
            match entry["kind"].as_str() {
                Some("file") => {
                    ensure(
                        fields(entry, &["path", "kind", "content"]),
                        "workspace file fields changed",
                    )?;
                    file_bytes(&entry["content"])?;
                }
                Some("directory") => ensure(
                    fields(entry, &["path", "kind"]),
                    "workspace directory fields changed",
                )?,
                _ => return Err("unexpected workspace entry type".into()),
            }
        }
        let result = snapshot
            .workspace
            .iter()
            .find(|entry| entry["path"] == "result.txt");
        ensure(
            match result {
                Some(entry) => entry["kind"] == "file" && entry["content"] == snapshot.file,
                None => snapshot.file.is_null(),
            },
            "observed file contradicts the complete workspace listing",
        )?;
        let mut approvals = BTreeMap::new();
        let mut executions = BTreeMap::new();
        let mut capabilities = BTreeMap::new();
        for record in &snapshot.events {
            match &record.event {
                CoreEvent::ApprovalRequested { approval }
                | CoreEvent::ApprovalDecided { approval } => {
                    approvals.insert(approval.id.clone(), approval.clone());
                }
                CoreEvent::ExecutionStarted { execution }
                | CoreEvent::ExecutionFinished { execution } => {
                    executions.insert(execution.execution_id.clone(), execution.clone());
                }
                CoreEvent::CapabilityPlanned { call }
                | CoreEvent::CapabilityStarted { call }
                | CoreEvent::CapabilitySettled { call } => {
                    capabilities.insert(call.key.clone(), call.clone());
                }
                _ => (),
            }
        }
        let pending = approvals
            .values()
            .filter(|approval| approval.decision.is_none())
            .cloned()
            .collect::<Vec<_>>();
        let unresolved = capabilities
            .values()
            .filter(|call| call.outcome.is_none())
            .cloned()
            .collect::<Vec<_>>();
        ensure(
            snapshot
                .approvals
                .iter()
                .map(|approval| (approval.id.clone(), approval.clone()))
                .collect::<BTreeMap<_, _>>()
                == approvals
                && snapshot.approvals.len() == approvals.len()
                && snapshot
                    .session
                    .pending_approvals
                    .iter()
                    .map(|approval| (approval.id.clone(), approval.clone()))
                    .collect::<BTreeMap<_, _>>()
                    == pending
                        .into_iter()
                        .map(|approval| (approval.id.clone(), approval))
                        .collect()
                && snapshot.session.executions == executions.into_values().collect::<Vec<_>>()
                && snapshot.session.unresolved_capabilities == unresolved,
            "access Session projection or stored approvals contradict the original durable events",
        )?;
        Ok(snapshot)
    }
    fn verify_files(&self, root: &Path) -> Result<()> {
        let mut paths = Vec::new();
        // This enumerates only generated evidence, not repository source files.
        fn collect(root: &Path, directory: &Path, paths: &mut Vec<Value>) -> Result<()> {
            let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let name = path
                    .strip_prefix(root)?
                    .to_str()
                    .ok_or("workspace path is not UTF-8")?
                    .to_owned();
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    paths.push(json!({"path":name,"kind":"directory"}));
                    collect(root, &path, paths)?;
                } else if kind.is_file() {
                    let bytes = attachment(root, &name)?;
                    paths.push(json!({"path":name,"kind":"file","content":{"sha256":sha256(&bytes),"utf8":String::from_utf8(bytes.clone()).ok(),"bytes":bytes}}));
                } else {
                    return Err("workspace evidence contains a symlink or special file".into());
                }
            }
            Ok(())
        }
        collect(&root.join("workspace"), &root.join("workspace"), &mut paths)?;
        ensure(
            paths == self.workspace,
            "actual workspace contents differ from the final recorded file effects",
        )
    }
}

pub(super) fn validate(report: &ScenarioReport, root: &Path) -> Result<()> {
    if report.status != "passed" {
        return Ok(());
    }
    ensure(
        report.source == "offline synthetic"
            && report.specification["input"]["prompt"] == PROMPT
            && report.specification["input"]["script"] == PIPELINE
            && report.specification["input"]["script_sha256"] == sha256(PIPELINE.as_bytes())
            && report.specification["input"]["expected_content"] == CONTENT
            && report.specification["input"]["expected_content_sha256"]
                == sha256(CONTENT.as_bytes())
            && report.specification["input"]["provider"] == serde_json::to_value(model())?
            && report.specification["permissions"]["grants"] == json!([]),
        "access fixture input or permission policy changed",
    )?;
    ensure(
        report.artifacts["provider"]["path"] == "provider.jsonl"
            && report.artifacts["access_provider_requests"]["path"] == "provider.jsonl",
        "required access Provider attachment declaration absent",
    )?;
    let provider = required_attachment(report, root, "provider")?;
    ensure(
        provider == required_attachment(report, root, "access_provider_requests")?,
        "access Provider attachments disagree",
    )?;
    let entries = json_lines(&provider)?;
    for (index, entry) in entries.iter().enumerate() {
        let request: cayw_ai::ModelRequest = serde_json::from_value(entry["request"].clone())?;
        ensure(
            entry["source"] == report.source
                && entry["call"] == index + 1
                && entry["request_sha256"] == sha256(&serde_json::to_vec(&request)?)
                && request.model == model(),
            "access Provider request lost its original model, order or digest",
        )?;
        let users = entry["request"]["messages"]
            .as_array()
            .ok_or("Provider history absent")?
            .iter()
            .filter(|message| message["role"] == "user")
            .collect::<Vec<_>>();
        ensure(
            users.len() == 1 && users[0]["content"] == json!([{"type":"text","text":PROMPT}]),
            "access Provider input changed or replayed its original prompt",
        )?;
    }
    let wire = Wire::read(report, root)?;
    match report.scenario.as_str() {
        "ipc-approval" => approval(report, root, &wire, &entries),
        "ipc-rejections" => rejections(report, root, &wire, &entries),
        _ => Err("unknown access evidence scenario".into()),
    }
}

fn original_lease(events: &[EventRecord]) -> Result<&AttemptLease> {
    let starts = events
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::AttemptStarted { lease } => Some((record, lease)),
            _ => None,
        })
        .collect::<Vec<_>>();
    ensure(
        starts.len() == 1,
        "approval evidence has no unique original attempt",
    )?;
    let (record, lease) = starts[0];
    ensure(
        lease.session_id == record.session_id
            && !lease.attempt_id.is_empty()
            && !lease.worker_id.is_empty()
            && lease.fence > 0
            && record.attempt.as_ref() == Some(lease)
            && events
                .iter()
                .skip(1)
                .all(|event| event.attempt.as_ref() == Some(lease)),
        "approval event lost its exact original executor lease",
    )?;
    Ok(lease)
}
fn capability_events(events: &[EventRecord], kind: u8) -> Vec<(u64, &CapabilityCallRecord)> {
    events
        .iter()
        .filter_map(|record| match (&record.event, kind) {
            (CoreEvent::CapabilityPlanned { call }, 0)
            | (CoreEvent::CapabilityStarted { call }, 1)
            | (CoreEvent::CapabilitySettled { call }, 2) => Some((record.cursor, call)),
            _ => None,
        })
        .collect()
}
fn match_task(task: &TaskRecord, lease: &AttemptLease) -> bool {
    task.session_id == lease.session_id
        && task.attempt_id.as_ref() == Some(&lease.attempt_id)
        && task.worker_id.as_ref() == Some(&lease.worker_id)
        && task.fence == lease.fence
        && task.input.text == PROMPT
        && task.input.images.is_empty()
        && !task.id.is_empty()
        && !task.cancel_requested
        && task.error.is_none()
}
fn approval(report: &ScenarioReport, root: &Path, wire: &Wire, entries: &[Value]) -> Result<()> {
    ensure(
        entries.len() == 2,
        "approval Provider history is missing or replayed",
    )?;
    let final_value = &one(report, "access_approval_final")?["snapshot"];
    let final_state = Snapshot::read(final_value, report, entries)?;
    final_state.verify_files(root)?;
    let events = &final_state.events;
    let lease = original_lease(events)?;
    ensure(
        super::scenarios::clean(&final_state.session)
            && final_state.tasks.len() == 1
            && final_state.tasks[0].state == TaskState::Completed
            && match_task(&final_state.tasks[0], lease)
            && final_state.provider_requests == entries
            && final_state.health["running"] == 0
            && file_bytes(&final_state.file)? == CONTENT.as_bytes(),
        "approval has no complete original task and actual file effect",
    )?;
    let task = &final_state.tasks[0];
    let requested = events
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::ApprovalRequested { approval } => Some((record.cursor, approval)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let decided = events
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::ApprovalDecided { approval } => Some((record.cursor, approval)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let planned = capability_events(events, 0);
    let started = capability_events(events, 1);
    let settled = capability_events(events, 2);
    let waiting = observed(report, "access_approval_waiting");
    let decisions = observed(report, "access_approval_decided");
    let approval_requests = observed(report, "access_approval_request");
    ensure(
        [
            requested.len(),
            decided.len(),
            planned.len(),
            started.len(),
            settled.len(),
            waiting.len(),
            decisions.len(),
            approval_requests.len(),
            final_state.grants.len(),
        ]
        .into_iter()
        .all(|count| count == 2),
        "approval evidence lost a distinct write/read request, decision or capability terminal",
    )?;
    ensure(
        final_state.audit.len() == 6,
        "approval audit lost a decision pair or its original task terminal",
    )?;
    let mut expected_audit = Vec::new();
    let mut previous_settle = 0;
    for index in 0..2 {
        let (planned_at, plan) = planned[index];
        let (started_at, start) = started[index];
        let (settled_at, terminal) = settled[index];
        let (requested_at, original) = requested[index];
        let (decided_at, resolution) = decided[index];
        let expected = if index == 0 {
            write_arguments(false)
        } else {
            json!({"path":"result.txt"})
        };
        let name = if index == 0 { "fs.write" } else { "fs.read" };
        let expected_scope = serde_json::to_string(&expected)?;
        ensure(
            original.decision.is_none()
                && matches!(&original.input.target,ApprovalTarget::CapabilityCall {key} if *key == plan.key)
                && original.session_id == lease.session_id
                && original.attempt_id == lease.attempt_id
                && original.input.task_id == task.id
                && original.input.capability == name
                && original.input.scope == expected_scope
                && plan.key.sequence == index as u64
                && plan.name == name
                && plan.arguments == expected
                && plan.lease == *lease
                && plan.task_id == task.id
                && plan.source_hash == format!("sha256:{}", sha256(PIPELINE.as_bytes()))
                && !plan.parent_call_id.is_empty()
                && plan.stage == CapabilityStage::Planned
                && plan.authorization.is_none()
                && plan.outcome.is_none()
                && previous_settle < planned_at
                && planned_at < requested_at
                && requested_at < decided_at
                && decided_at < started_at
                && started_at < settled_at,
            "approval target or decision/start ordering differs from the exact original planned child",
        )?;
        let mut expected_resolution = original.clone();
        expected_resolution.decision = resolution.decision.clone();
        ensure(
            *resolution == expected_resolution
                && resolution.decision.as_ref().is_some_and(|decision| {
                    decision.actor == "rpc-user" && decision.decision == ApprovalDecision::Approve
                })
                && waiting[index]["approval"] == serde_json::to_value(original)?
                && decisions[index] == &json!({"original":original,"response":resolution})
                && approval_requests[index]
                    == &json!({"session_id":lease.session_id,"approval_id":original.id,"decision":"allow_once"}),
            "approval response changed its original input, decision or host actor",
        )?;
        let authority: CapabilityAuthorization = serde_json::from_value(
            start
                .authorization
                .clone()
                .ok_or("started child has no persisted authorization")?,
        )?;
        let mut expected_start = plan.clone();
        expected_start.stage = CapabilityStage::Started;
        expected_start.authorization = start.authorization.clone();
        let mut expected_terminal = start.clone();
        expected_terminal.stage = CapabilityStage::Settled;
        expected_terminal.outcome = terminal.outcome.clone();
        ensure(
            *start == expected_start
                && *terminal == expected_terminal
                && authority.actor == "rpc-user"
                && authority.target == plan.key
                && authority.task_id == task.id
                && authority.capability == name
                && authority.scope == original.input.scope
                && authority.approval_id.as_ref() == Some(&original.id)
                && authority.decision == ApprovalDecision::Approve
                && authority.external_provenance.is_none(),
            "started or settled capability changed the original key, lease, task, source, parent or exact permission",
        )?;
        match &terminal.outcome {
            Some(CapabilityOutcome::Completed { result }) => {
                let expected = if index == 0 {
                    json!({"bytes":CONTENT.len(),"sha256":sha256(CONTENT.as_bytes())})
                } else {
                    json!({"path":"result.txt","bytes":CONTENT.len(),"sha256":sha256(CONTENT.as_bytes()),"content":CONTENT})
                };
                ensure(
                    *result == expected,
                    "approved Host write/read lost its actual content result",
                )?;
            }
            _ => return Err("approved capability has no successful terminal".into()),
        }
        let paused = Snapshot::read(&waiting[index]["snapshot"], report, entries)?;
        ensure(
            events.starts_with(&paused.events)
                && paused.session.cursor == requested_at
                && paused.session.attempt.as_ref() == Some(lease)
                && paused.session.state == cayw_core::SessionState::AwaitingApproval
                && paused.session.pending_approvals == vec![original.clone()]
                && paused.health["running"] == 1
                && paused.tasks.len() == 1
                && paused.tasks[0].state == TaskState::Running
                && match_task(&paused.tasks[0], lease)
                && paused.tasks[0].id == task.id
                && paused.provider_calls == 1
                && paused.provider_requests == entries[..1]
                && capability_events(&paused.events, 1).len() == index
                && capability_events(&paused.events, 2).len() == index
                && paused.grants.len() == index
                && serde_json::to_value(&paused.audit)? == json!(expected_audit)
                && if index == 0 {
                    paused.file.is_null() && paused.workspace.is_empty()
                } else {
                    file_bytes(&paused.file)? == CONTENT.as_bytes()
                },
            "approval pause lost its complete unchanged original pending task or has an effect before permission",
        )?;
        let grant = final_state
            .grants
            .iter()
            .find(|grant| grant.approval_id == original.id)
            .ok_or("original approval grant absent")?;
        ensure(
            grant.session_id == lease.session_id
                && grant.attempt_id == lease.attempt_id
                && grant.capability == name
                && grant.scope == original.input.scope
                && grant.actor == "rpc-user"
                && grant.mode == "allow_once"
                && grant.consumed
                && grant.state == "revoked",
            "final grant contradicts the original exact one-use permission",
        )?;
        // Build each record from the original approval and grant. Only its
        // observed timestamp is carried into the unchanged pause/final prefix.
        let cursor = (index * 2 + 1) as u64;
        expected_audit.push(json!({
            "cursor":cursor,"session_id":lease.session_id,"task_id":task.id,
            "kind":"approval_decision_started","timestamp":final_state.audit[index * 2].timestamp,
            "data":{"approval_id":original.id,"attempt_id":lease.attempt_id,
                "choice":"allow_once","actor":"rpc-user"}
        }));
        expected_audit.push(json!({
            "cursor":cursor + 1,"session_id":lease.session_id,"task_id":task.id,
            "kind":"approval_decision_committed","timestamp":final_state.audit[index * 2 + 1].timestamp,
            "data":{"decision_cursor":cursor,"grant_id":grant.id}
        }));
        let admitted = events
            .iter()
            .filter_map(|record| match &record.event {
                CoreEvent::Host { kind, data }
                    if kind == "permission_admitted" && data["approval_id"] == original.id =>
                {
                    Some((record.cursor, data))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let scoped = events
            .iter()
            .filter_map(|record| match &record.event {
                CoreEvent::Host { kind, data }
                    if kind == "approval_scope_decided" && data["approval_id"] == original.id =>
                {
                    Some((record.cursor, data))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut active = grant.clone();
        active.state = "active".into();
        ensure(
            admitted.len() == 1
                && scoped.len() == 1
                && decided_at < scoped[0].0
                && scoped[0].0 < admitted[0].0
                && admitted[0].0 < started_at
                && admitted[0].1
                    == &json!({"approval_id":original.id,"grant":active,"grant_id":grant.id,"key":plan.key,"task_id":task.id})
                && scoped[0].1
                    == &json!({"actor":"rpc-user","approval_id":original.id,"choice":"allow_once","grant_id":grant.id,"scope":original.input.scope,"task_id":task.id}),
            "original permission admission or scoped decision contradicts the persisted grant",
        )?;
        if index == 1 {
            let mut active = final_state
                .grants
                .iter()
                .find(|grant| grant.approval_id == requested[0].1.id)
                .ok_or("write grant missing")?
                .clone();
            active.state = "active".into();
            ensure(
                paused.grants == vec![active],
                "read pause replaced the original consumed write grant",
            )?;
        }
        previous_settle = settled_at;
    }
    ensure(
        matches!(&events.last().ok_or("approval history empty")?.event,CoreEvent::AttemptFinished {lease:ended} if ended == lease),
        "approval lacks its original attempt terminal",
    )?;
    expected_audit.push(json!({
        "cursor":5,"session_id":lease.session_id,"task_id":null,
        "kind":"grants_expired","timestamp":final_state.audit[4].timestamp,
        "data":{"reason":"task finished"}
    }));
    expected_audit.push(json!({
        "cursor":6,"session_id":lease.session_id,"task_id":task.id,
        "kind":"run_finished","timestamp":final_state.audit[5].timestamp,
        "data":{"error":null,"worker_id":lease.worker_id}
    }));
    ensure(
        serde_json::to_value(&final_state.audit)? == json!(expected_audit),
        "approval audit does not record the exact original decisions, completed task and revoked grants",
    )?;
    // Reuse the existing full Agent/Cells/Provider pairing validator. Its task
    // input is the original final snapshot rather than a new claimed check.
    let mut pairing = ScenarioReport::new(
        "access-pairing",
        report.iteration,
        report.specification.clone(),
        Value::Null,
    );
    pairing.observe("access_tasks", &final_state.tasks)?;
    validate_calls(events, entries, &pairing, 1)?;
    approval_wire(report, wire, &final_state, &waiting, &decisions)?;
    Ok(())
}

fn approval_wire(
    report: &ScenarioReport,
    wire: &Wire,
    final_state: &Snapshot,
    waiting: &[&Value],
    decisions: &[&Value],
) -> Result<()> {
    let created = one(report, "access_session_created")?;
    let session = &final_state.session.id;
    let create = wire.only("session.create")?;
    let prompt = wire.only("session.prompt")?;
    ensure(
        create["frame"]["params"] == json!({"label":"Offline IPC approvals"})
            && wire.reply(create)?["frame"]["result"] == *created
            && created["id"] == *session
            && created["cursor"] == 1
            && prompt["frame"]["params"] == json!({"session_id":session,"text":PROMPT})
            && one(report, "access_prompt_request")? == &prompt["frame"]["params"]
            && wire.reply(prompt)?["frame"]["result"]
                == serde_json::to_value(&final_state.session)?
            && one(report, "access_prompt_response")? == &wire.reply(prompt)?["frame"]["result"]
            && prompt["connection"] != create["connection"]
            && wire.requests("hello").len() == 2,
        "approval wire lost the original created Session, prompt or independent control connection",
    )?;
    let event_requests = wire.requests("session.events");
    let pages = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "access_live")
        .collect::<Vec<_>>();
    let event_observations = observed(report, "access_events_request");
    ensure(
        !pages.is_empty()
            && event_requests.len() == pages.len()
            && event_observations.len() == pages.len(),
        "approval lost a live original IPC page",
    )?;
    let mut cursor = 0;
    for ((request, page), recorded) in event_requests.iter().zip(&pages).zip(&event_observations) {
        let next = page.records.last().map_or(cursor, |record| record.cursor);
        ensure(
            page.session_id == *session
                && page.after == cursor
                && contiguous_events(session, cursor, &page.records, next)
                && final_state.events.get(cursor as usize..next as usize)
                    == Some(page.records.as_slice())
                && request["connection"] == create["connection"]
                && request["frame"]["params"]
                    == json!({"session_id":session,"after":cursor,"limit":100,"wait_ms":1000})
                && request["frame"]["params"] == **recorded
                && wire.reply(request)?["frame"]["result"] == serde_json::to_value(&page.records)?,
            "approval live event bytes differ from their original page or complete durable history",
        )?;
        cursor = next;
    }
    let finals = report
        .event_pages
        .iter()
        .filter(|page| page.stage == "access_final")
        .collect::<Vec<_>>();
    ensure(
        finals.len() == 1
            && finals[0].after == 0
            && finals[0].session_id == *session
            && finals[0].records == final_state.events
            && report.event_pages.len() == pages.len() + 1,
        "approval final event page is missing or changed",
    )?;
    let approval_requests = wire.requests("approval.decide");
    let query_observations = observed(report, "access_pending_query");
    ensure(
        approval_requests.len() == 2 && query_observations.len() == 6,
        "approval wire lost a distinct original decision or pending query",
    )?;
    for index in 0..2 {
        let request = approval_requests[index];
        ensure(
            request["connection"] == create["connection"]
                && request["frame"]["params"]
                    == json!({"session_id":session,"approval_id":waiting[index]["approval"]["id"],"decision":"allow_once"})
                && wire.reply(request)?["frame"]["result"] == decisions[index]["response"],
            "approval decision lacks its identical original wire response",
        )?;
        for (offset, (method, field)) in [
            ("session.get", "session"),
            ("session.tasks", "tasks"),
            ("session.grants", "grants"),
        ]
        .into_iter()
        .enumerate()
        {
            let requests = wire.requests(method);
            ensure(
                requests.len() == 2,
                "pending approval query was lost or repeated",
            )?;
            let query = requests[index];
            ensure(
                query["connection"] == create["connection"]
                    && query["frame"]["params"] == json!({"session_id":session})
                    && query["sequence"].as_u64() < request["sequence"].as_u64()
                    && wire.reply(query)?["frame"]["result"] == waiting[index]["snapshot"][field]
                    && query_observations[index * 3 + offset]
                        == &json!({"method":method,"response":waiting[index]["snapshot"][field]}),
                "pending query differs from original complete Session, Tasks or permissions",
            )?;
        }
    }
    let methods = [
        "hello",
        "session.create",
        "session.prompt",
        "session.events",
        "session.get",
        "session.tasks",
        "session.grants",
        "approval.decide",
    ];
    ensure(
        wire.rows
            .iter()
            .filter(|row| row["direction"] == "client_to_server")
            .all(|row| methods.contains(&row["frame"]["method"].as_str().unwrap_or(""))),
        "approval wire contains extra dispatched work",
    )
}

fn raw_rejection(
    report: &ScenarioReport,
    record: &Value,
    session: &str,
    index: usize,
) -> Result<()> {
    let wire = &record["wire"];
    ensure(
        fields(
            wire,
            &[
                "transport",
                "address",
                "sent",
                "hello_response_bytes",
                "received",
                "response",
                "closed",
                "closure",
            ],
        ) && wire["transport"] == "direct_tcp"
            && wire["closed"] == true
            && matches!(
                wire["closure"].as_str(),
                Some("eof" | "connection reset" | "connection aborted")
            ),
        "rejected raw input has no actual direct-socket closure",
    )?;
    let address: SocketAddr = wire["address"]
        .as_str()
        .ok_or("raw socket address absent")?
        .parse()?;
    ensure(
        address.ip().is_loopback() && address.port() != 0,
        "rejection raw socket was not loopback",
    )?;
    let sent = wire["sent"]
        .as_array()
        .ok_or("rejected original bytes absent")?;
    ensure(
        sent.len() == 2,
        "rejection lost its original hello or business frame",
    )?;
    let hello = wire_json(&byte_record(&sent[0])?, true)?;
    let business_bytes = byte_record(&sent[1])?;
    let received = byte_record(&wire["received"])?;
    let hello_response = byte_record(&wire["hello_response_bytes"])?;
    let token = super::scenarios::ipc_config().token;
    let expected_hello = json!({"version":if index == 1 {2} else {cayw_ipc::PROTOCOL_VERSION},"id":"access-hello","method":"hello",
        "params":{"token":if index == 0 {"deliberately-wrong-offline-token"} else {&token}}});
    ensure(
        hello == expected_hello,
        "rejected hello bytes no longer contain the original wrong token/version",
    )?;
    let prompt = wire_json(&business_bytes, index != 2)?;
    let text = if index == 2 {
        "x".repeat(1025)
    } else {
        PROMPT.into()
    };
    ensure(
        prompt
            == json!({"version":cayw_ipc::PROTOCOL_VERSION,"id":"access-unauthorized-prompt","method":"session.prompt","params":{"session_id":session,"text":text}}),
        "rejection business bytes changed their original task request",
    )?;
    if index == 2 {
        let limit = report.specification["configuration"]["ipc"]["server"]["max_frame_bytes"]
            .as_u64()
            .ok_or("server frame limit absent")?;
        ensure(
            limit == 1024
                && business_bytes.len() as u64 > limit
                && !business_bytes.contains(&b'\n')
                && received.is_empty()
                && wire["response"].is_null(),
            "oversize rejection lacks original over-limit no-newline bytes and server closure",
        )?;
        ensure(
            wire_json(&hello_response, true)?
                == json!({"version":cayw_ipc::PROTOCOL_VERSION,"id":"access-hello","result":hello_result(report)}),
            "oversize input did not follow an actual successful hello",
        )?;
    } else {
        let response = wire_json(&received, true)?;
        let code = if index == 0 {
            "unauthorized"
        } else {
            "unsupported_version"
        };
        ensure(
            hello_response.is_empty()
                && fields(&response, &["version", "id", "error"])
                && response["version"] == cayw_ipc::PROTOCOL_VERSION
                && response["id"] == hello["id"]
                && response["error"]["code"] == code
                && response["error"]["message"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty())
                && response == wire["response"],
            "raw rejection reply differs from original bytes or request identity",
        )?;
    }
    Ok(())
}
fn rejections(report: &ScenarioReport, root: &Path, wire: &Wire, entries: &[Value]) -> Result<()> {
    ensure(
        entries.is_empty(),
        "rejected access requests called the Provider",
    )?;
    let created = one(report, "access_rejection_created")?;
    let final_value = &one(report, "access_rejection_final")?["snapshot"];
    let final_state = Snapshot::read(final_value, report, entries)?;
    let session = &final_state.session;
    ensure(
        serde_json::to_value(session)? == *created
            && super::scenarios::clean(session)
            && session.messages.is_empty()
            && session.cursor == 1
            && session.executions.is_empty()
            && !session.frozen
            && session.binding.is_none()
            && final_state.events.len() == 1
            && matches!(&final_state.events[0].event,CoreEvent::SessionCreated {label} if label == &session.label)
            && final_state.events[0].attempt.is_none()
            && final_state.tasks.is_empty()
            && final_state.approvals.is_empty()
            && final_state.audit.is_empty()
            && final_state.grants.is_empty()
            && final_state.provider_calls == 0
            && final_state.provider_requests.is_empty()
            && final_state.file.is_null()
            && final_state.workspace.is_empty()
            && final_state.health["running"] == 0
            && report.event_pages.is_empty(),
        "rejection baseline has changed Session/Tasks/events/permissions/Provider/file facts",
    )?;
    final_state.verify_files(root)?;
    let cases = observed(report, "access_rejection");
    let names = [
        "wrong-token",
        "wrong-version",
        "oversized-no-newline",
        "prompt-extra-field",
        "approval-forged-actor",
        "events-zero-limit",
    ];
    ensure(
        cases.len() == names.len(),
        "rejection evidence omitted or repeated a required case",
    )?;
    let mut handler_calls = 0;
    let mut client_requests = Vec::new();
    for (index, (record, name)) in cases.iter().zip(names).enumerate() {
        ensure(
            record["case"] == name
                && record["before"] == *final_value
                && record["after"] == *final_value,
            "rejection case lost its complete original unchanged before/after snapshot",
        )?;
        Snapshot::read(&record["before"], report, entries)?;
        Snapshot::read(&record["after"], report, entries)?;
        ensure(
            record["handler_calls_before"] == handler_calls,
            "rejection dispatch counter chain changed",
        )?;
        if index < 3 {
            raw_rejection(report, record, &session.id, index)?;
        } else {
            let (method, params, code) = match index {
                3 => (
                    "session.prompt",
                    json!({"session_id":session.id,"text":PROMPT,"extra":true}),
                    "invalid_params",
                ),
                4 => (
                    "approval.decide",
                    json!({"session_id":session.id,"approval_id":"unissued-approval","decision":"allow_once","actor":"forged-model"}),
                    "invalid_params",
                ),
                _ => (
                    "session.events",
                    json!({"session_id":session.id,"after":0,"limit":0,"wait_ms":0}),
                    "session_error",
                ),
            };
            let error: cayw_ipc::ClientError = serde_json::from_value(record["error"].clone())?;
            let request = wire.only(method)?;
            let response = wire.reply(request)?;
            ensure(
                record["wire"].is_null()
                    && record["frames"] == "access-ipc-frames.jsonl"
                    && record["unexpected_response"].is_null()
                    && record["request"] == json!({"method":method,"params":params})
                    && request["frame"]["params"] == params
                    && error.code == code
                    && !error.outcome_unknown
                    && !error.message.is_empty()
                    && response["frame"]["error"]
                        == json!({"code":error.code,"message":error.message}),
                "Runtime invalid-parameter evidence changed its actual Client request, paired reply or known error",
            )?;
            client_requests.push(request);
            handler_calls += 1;
        }
        ensure(
            record["handler_calls_after"] == handler_calls,
            "rejected bytes dispatched work or Runtime parameter validation was skipped",
        )?;
    }
    let hello = wire.only("hello")?;
    ensure(
        client_requests.len() == 3
            && client_requests
                .iter()
                .all(|request| request["connection"] == hello["connection"])
            && wire
                .rows
                .iter()
                .filter(|row| row["direction"] == "client_to_server")
                .count()
                == 4,
        "rejection wire omitted an authenticated original Runtime request or added other work",
    )?;
    Ok(())
}
