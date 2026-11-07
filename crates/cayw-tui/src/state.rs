//! UI projection of the shared runtime contract. The runtime owns all effects.
use crate::input::{Editor, sanitize};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    Sessions,
    History,
    #[default]
    Composer,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewTab {
    #[default]
    Chat,
    Files,
    Diff,
    Logs,
}

impl ViewTab {
    pub fn next(self) -> Self {
        match self {
            Self::Chat => Self::Files,
            Self::Files => Self::Diff,
            Self::Diff => Self::Logs,
            Self::Logs => Self::Chat,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Chat => Self::Logs,
            Self::Files => Self::Chat,
            Self::Diff => Self::Files,
            Self::Logs => Self::Diff,
        }
    }
}

/// Read-only file inspection entry projected from runtime snapshots and events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub action: String,
    pub status: String,
    pub preview: String,
}

/// Read-only diff inspection entry projected from runtime snapshots and events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: String,
    pub status: String,
    pub diff: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    AllowTask,
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowTask => "allow_task",
            Self::Deny => "deny",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    List,
    Create {
        label: String,
    },
    Get {
        session_id: String,
    },
    Prompt {
        session_id: String,
        text: String,
    },
    Cancel {
        session_id: String,
        task_id: String,
    },
    Decide {
        session_id: String,
        approval_id: String,
        decision: Decision,
    },
    Fork {
        session_id: String,
        label: String,
    },
    Reconnect,
    Quit,
}

impl Command {
    pub fn rpc(&self) -> Option<(&'static str, Value)> {
        Some(match self {
            Self::List => ("session.list", json!({})),
            Self::Create { label } => ("session.create", json!({"label":label})),
            Self::Get { session_id } => ("session.get", json!({"session_id":session_id})),
            Self::Prompt { session_id, text } => (
                "session.prompt",
                json!({"session_id":session_id,"text":text}),
            ),
            // Transport resolves only this retained Task into its full queue identity.
            Self::Cancel {
                session_id,
                task_id,
            } => (
                "session.cancel",
                json!({"session_id":session_id,"task_id":task_id}),
            ),
            Self::Decide {
                session_id,
                approval_id,
                decision,
            } => (
                "approval.decide",
                json!({"session_id":session_id,"approval_id":approval_id,"decision":decision.as_str()}),
            ),
            Self::Fork { session_id, label } => (
                "session.fork",
                json!({"session_id":session_id,"label":label,"confirm":true,"mode":"chat"}),
            ),
            Self::Reconnect | Self::Quit => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub enum Dialog {
    Create(Editor),
    Fork { session_id: String, label: String },
    Help,
}

#[derive(Clone, Debug)]
struct PendingPrompt {
    text: String,
    editor_revision: u64,
    uncertain: bool,
}

/// The complete worker lease owns a stream; timestamps alone do not identify a run.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AttemptIdentity {
    session_id: String,
    attempt_id: String,
    worker_id: String,
    fence: u64,
}

impl AttemptIdentity {
    fn parse(value: &Value) -> Result<Option<Self>, String> {
        if value.is_null() {
            return Ok(None);
        }
        Ok(Some(Self {
            session_id: required_string(value, "session_id")?.into(),
            attempt_id: required_string(value, "attempt_id")?.into(),
            worker_id: required_string(value, "worker_id")?.into(),
            fence: value["fence"]
                .as_u64()
                .filter(|fence| *fence > 0)
                .ok_or("Attempt is missing a positive fence")?,
        }))
    }
}

#[derive(Clone, Debug)]
enum AttemptTransition {
    Started(AttemptIdentity),
    Ended(AttemptIdentity),
}

/// Replay at the consumed cursor, separate from the newer snapshot projection.
/// A page can stop between an old update and its terminal, so this candidate is
/// never displayed until replay reaches the authoritative snapshot position.
#[derive(Clone, Debug, Default)]
struct StreamReplay {
    attempt: Option<AttemptIdentity>,
    highest_fence: u64,
    message: Option<Value>,
    agent_ended: bool,
}

impl StreamReplay {
    fn event(
        &mut self,
        session_id: &str,
        attempt: Option<&AttemptIdentity>,
        event: &Value,
    ) -> Result<Option<AttemptTransition>, String> {
        let kind = required_string(event, "type")?;
        let Some(attempt) = attempt.filter(|attempt| attempt.session_id == session_id) else {
            return Ok(None);
        };
        if matches!(
            kind,
            "attempt_started" | "attempt_finished" | "attempt_recovered"
        ) {
            if AttemptIdentity::parse(&event["lease"])?.as_ref() != Some(attempt) {
                return Ok(None);
            }
            if kind == "attempt_started" && attempt.fence > self.highest_fence {
                self.highest_fence = attempt.fence;
                self.attempt = Some(attempt.clone());
                self.message = None;
                self.agent_ended = false;
                return Ok(Some(AttemptTransition::Started(attempt.clone())));
            }
            if kind != "attempt_started" && self.attempt.as_ref() == Some(attempt) {
                self.attempt = None;
                self.message = None;
                return Ok(Some(AttemptTransition::Ended(attempt.clone())));
            }
            return Ok(None);
        }
        if kind != "agent" || self.attempt.as_ref() != Some(attempt) || self.agent_ended {
            return Ok(None);
        }
        let event = &event["event"];
        match required_string(event, "type")? {
            "message_update" => {
                let message = event["message"]
                    .as_object()
                    .ok_or("Missing streaming message")?;
                self.message = Some(Value::Object(message.clone()));
            }
            "message_start" if event["message"]["role"] == "assistant" => {
                self.message = None;
            }
            "message_end" | "turn_end" => {
                let message = &event["message"];
                // Agent events are ordered within the matched worker lease. A
                // completed assistant or turn ends the current stream even if
                // the provider changes its observation timestamp at completion.
                if message["role"] == "assistant" || event["type"] == "turn_end" {
                    self.message = None;
                }
            }
            "agent_end" => {
                self.message = None;
                self.agent_ended = true;
            }
            _ => {}
        }
        Ok(None)
    }
}

#[derive(Clone, Debug)]
pub struct SessionView {
    pub id: String,
    pub label: String,
    pub state: String,
    /// Last event envelope consumed by the UI. Snapshot refreshes never advance it.
    pub cursor: u64,
    /// Position represented by the authoritative history and effect projection.
    pub snapshot_cursor: u64,
    pub messages: Vec<Value>,
    pub streaming_text: Option<String>,
    pub approvals: Vec<Value>,
    pub unresolved_calls: Vec<Value>,
    pub frozen: bool,
    pub binding: Option<Value>,
    pub tool_status: Option<String>,
    pub files: Vec<FileEntry>,
    pub diffs: Vec<DiffEntry>,
    pub logs: Vec<String>,
    pub scroll: u16,
    pub synced: bool,
    pub request_pending: bool,
    pub cancel_pending: bool,
    /// Identity returned by this client's enqueue; never inferred from a Session lease.
    original_task_id: Option<String>,
    decisions_pending: BTreeSet<String>,
    pending_prompt: Option<PendingPrompt>,
    active_attempt: Option<AttemptIdentity>,
    stream_replay: StreamReplay,
    refresh_after: Option<u64>,
    snapshot_executions: Vec<Value>,
    settled_capabilities: Vec<Value>,
}

impl SessionView {
    pub fn from_snapshot(value: Value) -> Result<Self, String> {
        let id = required_string(&value, "id")?.to_owned();
        let label = required_string(&value, "label")?.to_owned();
        let state = required_string(&value, "state")?;
        if !matches!(
            state,
            "idle" | "running" | "awaiting_approval" | "needs_reconciliation"
        ) {
            return Err("Snapshot contains an unsupported session state".into());
        }
        let cursor = value["cursor"]
            .as_u64()
            .ok_or("Snapshot is missing a valid cursor")?;
        let active_attempt = AttemptIdentity::parse(&value["attempt"])?;
        if active_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.session_id != id)
        {
            return Err("Snapshot attempt belongs to another session".into());
        }
        let messages = array(&value, "messages")?;
        let approvals = array(&value, "pending_approvals")?;
        for approval in &approvals {
            required_string(approval, "id")?;
        }
        let unresolved_calls = array(&value, "unresolved_calls")?;
        let snapshot_executions = value["executions"].as_array().cloned().unwrap_or_default();
        let frozen = value["frozen"]
            .as_bool()
            .ok_or("Snapshot frozen flag must be a boolean")?;
        let binding = match value.get("binding") {
            Some(Value::Null) => None,
            Some(value @ Value::Object(_)) => Some(value.clone()),
            _ => return Err("Snapshot binding must be an object or null".into()),
        };
        let mut view = Self {
            id,
            label,
            state: state.into(),
            // A snapshot contains committed messages, but not the unfinished
            // assistant message. New attachments must read the event log from
            // its recoverable beginning; snapshot_cursor prevents reprojecting
            // committed history, approvals, bindings and effects along the way.
            cursor: 0,
            snapshot_cursor: cursor,
            messages,
            streaming_text: None,
            approvals,
            unresolved_calls,
            frozen,
            binding,
            tool_status: None,
            files: Vec::new(),
            diffs: Vec::new(),
            logs: Vec::new(),
            scroll: 0,
            synced: true,
            request_pending: false,
            cancel_pending: false,
            original_task_id: None,
            decisions_pending: BTreeSet::new(),
            pending_prompt: None,
            active_attempt,
            stream_replay: StreamReplay::default(),
            refresh_after: None,
            snapshot_executions,
            settled_capabilities: Vec::new(),
        };
        view.protect_unknown();
        view.rebuild_inspectors();
        Ok(view)
    }

    pub fn can_prompt(&self) -> bool {
        self.synced
            && !self
                .pending_prompt
                .as_ref()
                .is_some_and(|prompt| prompt.uncertain)
            && !self.request_pending
            && !self.frozen
            && self.state == "idle"
            && self.approvals.is_empty()
            && self.unresolved_calls.is_empty()
    }

    pub fn can_fork(&self) -> bool {
        self.synced
            && self.frozen
            && !self
                .pending_prompt
                .as_ref()
                .is_some_and(|prompt| prompt.uncertain)
            && !self.request_pending
            && self.state == "idle"
            && self.approvals.is_empty()
            && self.unresolved_calls.is_empty()
    }

    pub fn has_unknown(&self) -> bool {
        self.state == "needs_reconciliation"
            || self
                .unresolved_calls
                .iter()
                .any(|call| call["state"] == "outcome_unknown")
    }

    fn protect_unknown(&mut self) {
        if self.has_unknown() {
            self.state = "needs_reconciliation".into();
        }
    }

    fn rebuild_inspectors(&mut self) {
        let (files, diffs) = project_files_and_diffs(
            &self.messages,
            &self.approvals,
            &self.unresolved_calls,
            &self.settled_capabilities,
        );
        self.files = files;
        self.diffs = diffs;
        if self.logs.is_empty() {
            for execution in &self.snapshot_executions {
                let cell = execution["cell_id"]
                    .as_str()
                    .or_else(|| execution["execution_id"].as_str())
                    .unwrap_or("");
                let state = string(execution, "state");
                if !cell.is_empty() {
                    push_log(&mut self.logs, format!("cell {cell} · {state}"));
                }
            }
        }
    }

    fn project_stream(&mut self) {
        self.streaming_text = (self.cursor >= self.snapshot_cursor
            && self.active_attempt.is_some()
            && self.active_attempt == self.stream_replay.attempt)
            .then(|| self.stream_replay.message.as_ref().map(message_text))
            .flatten();
    }

    fn event(&mut self, event: &Value, project: bool) -> Result<(), String> {
        let kind = required_string(event, "type")?;
        if kind == "agent" {
            return self.agent_event(
                event.get("event").ok_or("Agent wrapper is missing event")?,
                project,
            );
        }
        if kind == "capability_settled"
            && let Some(call) = event.get("call").filter(|value| value.is_object())
        {
            let key = &call["key"];
            self.settled_capabilities
                .retain(|existing| existing["key"] != *key || key.is_null());
            self.settled_capabilities.push(call.clone());
            if self.settled_capabilities.len() > MAX_INSPECTOR_ITEMS {
                let drain = self.settled_capabilities.len() - MAX_INSPECTOR_ITEMS;
                self.settled_capabilities.drain(0..drain);
            }
            self.rebuild_inspectors();
        }
        // Already represented by the snapshot: consume its envelope without
        // duplicating history or rewinding approvals, bindings or effect state.
        if !project {
            return Ok(());
        }
        match kind {
            "attempt_started" => {
                if !self.has_unknown() {
                    self.state = "running".into();
                }
                self.cancel_pending = false;
            }
            "attempt_finished" => {
                // Completion claims cannot settle a call attached from a snapshot.
                // A get at this event position owns the final call pairing/state.
                self.synced = false;
                self.cancel_pending = false;
                self.tool_status = None;
                self.state = if self.has_unknown() {
                    "needs_reconciliation"
                } else if !self.approvals.is_empty() {
                    "awaiting_approval"
                } else {
                    "idle"
                }
                .into();
            }
            "approval_requested" => {
                let approval = event.get("approval").ok_or("Missing approval")?;
                let id = required_string(approval, "id")?;
                self.approvals.retain(|value| value["id"] != id);
                self.approvals.push(approval.clone());
                if !self.has_unknown() {
                    self.state = "awaiting_approval".into();
                }
            }
            "approval_decided" => {
                let approval = event.get("approval").ok_or("Missing approval")?;
                let id = required_string(approval, "id")?;
                self.approvals.retain(|value| value["id"] != id);
                self.decisions_pending.remove(id);
                if self.state == "awaiting_approval" && self.approvals.is_empty() {
                    self.state = "running".into();
                }
            }
            "attempt_recovered" => {
                self.synced = false;
                if let Some(ids) = event["unknown_call_ids"].as_array() {
                    for id in ids {
                        self.remember_unknown(json!({"id":id}));
                    }
                }
            }
            "reconciled" => {
                // These events alone cannot prove the complete current effects.
                // A fresh authoritative snapshot must unblock the next prompt.
                self.synced = false;
            }
            "session_forked" => {
                self.messages = event["messages"]
                    .as_array()
                    .cloned()
                    .ok_or("Fork event is missing messages")?;
            }
            "host" => {
                // Host extensions can change the plugin binding or frozen flag.
                self.synced = false;
            }
            "tool_started" => {
                self.tool_status = Some(format!("Running tool {}", string(event, "call_id")));
            }
            _ => {} // Forward-compatible core events still consume their envelope.
        }
        self.protect_unknown();
        self.rebuild_inspectors();
        Ok(())
    }

    fn agent_event(&mut self, event: &Value, project: bool) -> Result<(), String> {
        let kind = required_string(event, "type")?;
        // StreamReplay consumes unfinished text independently. Only events after
        // the snapshot may mutate committed history or the effect projection.
        if !project {
            return Ok(());
        }
        match kind {
            "agent_start" | "turn_start" if !self.has_unknown() && self.approvals.is_empty() => {
                self.state = "running".into();
            }
            "message_update" => {
                event
                    .get("message")
                    .filter(|v| v.is_object())
                    .ok_or("Missing streaming message")?;
            }
            "message_end" => {
                let message = event
                    .get("message")
                    .filter(|v| v.is_object())
                    .ok_or("Missing completed message")?;
                self.observe_unknown_message(message);
                self.messages.push(message.clone());
            }
            "agent_end" => {
                if let Some(messages) = event["messages"].as_array() {
                    for message in messages {
                        self.observe_unknown_message(message);
                    }
                }
                // AttemptFinished and snapshots own the final session state.
                self.tool_status = None;
            }
            "tool_execution_start" => {
                self.tool_status = Some(format!("Running {}", string(&event["call"], "name")));
            }
            "tool_execution_update" | "tool_execution_end" => {
                if kind == "tool_execution_end" {
                    self.synced = false;
                }
                let call = &event["call"];
                if event["result"]["outcome_unknown"] == true {
                    self.remember_unknown(call.clone());
                }
                self.tool_status = Some(format!(
                    "{}: {}",
                    string(call, "name"),
                    message_text(&event["result"])
                ));
            }
            _ => {}
        }
        self.rebuild_inspectors();
        Ok(())
    }

    fn observe_unknown_message(&mut self, message: &Value) {
        if message["outcome_unknown"] == true {
            self.remember_unknown(
                json!({"id":message["tool_call_id"], "name":message["tool_name"]}),
            );
        }
    }

    fn remember_unknown(&mut self, call: Value) {
        if !self
            .unresolved_calls
            .iter()
            .any(|value| value["call"]["id"] == call["id"])
        {
            self.unresolved_calls
                .push(json!({"call":call,"state":"outcome_unknown"}));
        }
        self.state = "needs_reconciliation".into();
    }
}

#[derive(Debug, Default)]
pub struct App {
    pub sessions: Vec<SessionView>,
    pub selected_id: Option<String>,
    pub input: Editor,
    pub focus: Focus,
    pub view: ViewTab,
    pub dialog: Option<Dialog>,
    pub connected: bool,
    pub status: String,
    drafts: BTreeMap<String, Editor>,
    pub quitting: bool,
}

impl App {
    pub fn selected(&self) -> Option<&SessionView> {
        let id = self.selected_id.as_ref()?;
        self.sessions.iter().find(|session| &session.id == id)
    }

    fn selected_mut(&mut self) -> Option<&mut SessionView> {
        let id = self.selected_id.as_ref()?;
        self.sessions.iter_mut().find(|session| &session.id == id)
    }

    pub fn set_connected(&mut self) {
        self.connected = true;
        self.status = "Connected. Refreshing sessions…".into();
    }

    pub fn set_disconnected(&mut self, reason: &str) {
        self.connected = false;
        self.status = format!(
            "{} | Ctrl+R reconnect. Prompts are never replayed.",
            sanitize(reason)
        );
        for session in &mut self.sessions {
            session.synced = false;
            session.request_pending = false;
            if let Some(prompt) = &mut session.pending_prompt {
                prompt.uncertain = true;
            }
            session.cancel_pending = false;
            session.decisions_pending.clear();
        }
    }

    pub fn upsert_snapshot(&mut self, value: Value, select: bool) -> Result<(), String> {
        let incoming = SessionView::from_snapshot(value)?;
        self.upsert_session_view(incoming, select);
        Ok(())
    }

    fn upsert_session_view(&mut self, mut incoming: SessionView, select: bool) {
        let id = incoming.id.clone();
        if let Some(index) = self.sessions.iter().position(|session| session.id == id) {
            let old = &self.sessions[index];
            if incoming.snapshot_cursor < old.snapshot_cursor.max(old.cursor) {
                // A slow list/get response must not overwrite already consumed events.
                if select {
                    self.select_id(&id);
                }
                return;
            }
            incoming.scroll = old.scroll;
            incoming.cursor = old.cursor;
            incoming.logs = old.logs.clone();
            incoming.settled_capabilities = old.settled_capabilities.clone();
            incoming.stream_replay = old.stream_replay.clone();
            if incoming.active_attempt.is_none() && incoming.state != "idle" {
                incoming.active_attempt = old.active_attempt.clone();
            }
            incoming.project_stream();
            if matches!(incoming.state.as_str(), "running" | "awaiting_approval") {
                incoming.tool_status = old.tool_status.clone();
            }
            incoming.refresh_after = old
                .refresh_after
                .filter(|cursor| *cursor > incoming.snapshot_cursor);
            incoming.synced = incoming.refresh_after.is_none();
            incoming.pending_prompt = old.pending_prompt.clone();
            incoming.original_task_id = old.original_task_id.clone();
            incoming.request_pending = old.request_pending;
            incoming.cancel_pending = old.cancel_pending && incoming.state != "idle";
            incoming.decisions_pending = old
                .decisions_pending
                .iter()
                .filter(|id| {
                    incoming
                        .approvals
                        .iter()
                        .any(|approval| approval["id"] == id.as_str())
                })
                .cloned()
                .collect();
            incoming.rebuild_inspectors();
            self.sessions[index] = incoming;
        } else {
            self.sessions.push(incoming);
        }
        if select || self.selected_id.is_none() {
            self.select_id(&id);
        }
    }

    pub fn apply_list(&mut self, value: Value) -> Result<(), String> {
        let list = match value {
            Value::Array(list) => list,
            _ => return Err("session.list did not return an array".into()),
        };
        // Validate the complete page before mutating existing projections.
        let mut ids = BTreeSet::new();
        let mut views = Vec::with_capacity(list.len());
        for value in list {
            let session = SessionView::from_snapshot(value)?;
            if !ids.insert(session.id.clone()) {
                return Err("session.list contains duplicate ids".into());
            }
            views.push(session);
        }
        for session in views {
            self.upsert_session_view(session, false);
        }
        self.status = if self.sessions.is_empty() {
            "No sessions. Ctrl+N creates one."
        } else {
            "Sessions refreshed."
        }
        .into();
        Ok(())
    }

    pub fn apply_events(&mut self, id: &str, value: Value) -> Result<(), String> {
        let records = value
            .as_array()
            .ok_or("session.events did not return an array")?;
        let session = self
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .ok_or("Events reference an unknown session")?;
        let mut next = session.clone();
        let mut previous = None;
        for record in records {
            let cursor = record["cursor"]
                .as_u64()
                .filter(|cursor| *cursor > 0)
                .ok_or("Event is missing a positive cursor")?;
            if record
                .get("session_id")
                .is_some_and(|value| value.as_str() != Some(id))
            {
                return Err("Event belongs to another session; cursor retained".into());
            }
            if previous.is_some_and(|previous| cursor <= previous) {
                return Err("Events are not ordered by cursor".into());
            }
            previous = Some(cursor);
            let event = record
                .get("event")
                .filter(|value| value.is_object())
                .ok_or("Event record is missing event")?;
            required_string(event, "type")?;
            if cursor <= next.cursor {
                continue;
            }
            let attempt = AttemptIdentity::parse(&record["attempt"])?;
            let transition = next.stream_replay.event(id, attempt.as_ref(), event)?;
            if cursor > next.snapshot_cursor {
                match transition {
                    Some(AttemptTransition::Started(attempt)) => {
                        next.active_attempt = Some(attempt);
                    }
                    Some(AttemptTransition::Ended(attempt))
                        if next.active_attempt.as_ref() == Some(&attempt) =>
                    {
                        next.active_attempt = None;
                    }
                    _ => {}
                }
            }
            next.event(event, cursor > next.snapshot_cursor)?;
            let is_delta = event["type"] == "agent"
                && matches!(
                    event["event"]["type"].as_str(),
                    Some("message_update" | "tool_execution_update")
                );
            if !is_delta {
                push_log(
                    &mut next.logs,
                    format_event_log(cursor, record.get("attempt"), event),
                );
            }
            if !next.synced {
                next.refresh_after = Some(next.refresh_after.unwrap_or(0).max(cursor));
            }
            next.cursor = cursor;
            next.project_stream();
        }
        *session = next;
        Ok(())
    }

    /// Invalidate mutation controls without moving the consumed event cursor.
    pub fn invalidate_events(&mut self, id: &str) {
        if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.synced = false;
            session.refresh_after = Some(session.cursor);
        }
    }

    fn clear_admitted_draft(&mut self, id: &str) {
        let pending = self
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .and_then(|session| session.pending_prompt.take());
        if let Some(prompt) = pending {
            let editor = if self.selected_id.as_deref() == Some(id) {
                Some(&mut self.input)
            } else {
                self.drafts.get_mut(id)
            };
            if let Some(editor) = editor
                && editor.revision() == prompt.editor_revision
                && editor.text() == prompt.text
            {
                editor.clear();
            }
        }
    }

    pub fn task_admitted(&mut self, id: &str, task_id: &str) -> Vec<Command> {
        let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) else {
            return vec![];
        };
        session.original_task_id = Some(task_id.into());
        if session.cancel_pending {
            vec![Command::Cancel {
                session_id: id.into(),
                task_id: task_id.into(),
            }]
        } else {
            vec![]
        }
    }

    pub fn prompt_completed(&mut self, id: &str) {
        self.clear_admitted_draft(id);
        if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.request_pending = false;
        }
    }

    pub fn cancel_acknowledged(&mut self, id: &str, cancelled: bool) {
        if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.cancel_pending = cancelled;
        }
        self.status = if cancelled {
            "Cancellation requested. Waiting for the runtime's final result."
        } else {
            "No active run was cancelled. Refreshing session state."
        }
        .into();
    }

    pub fn approval_acknowledged(&mut self, id: &str, approval_id: &str) {
        if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.decisions_pending.remove(approval_id);
            session.approvals.retain(|value| value["id"] != approval_id);
            if session.approvals.is_empty() && session.state == "awaiting_approval" {
                session.state = "running".into();
            }
        }
        self.status = "Approval decision recorded by the runtime.".into();
    }

    pub fn request_failed(&mut self, id: &str, method: &str, error: &str) {
        self.status = format!(
            "{method}: {}. Refresh the session before retrying.",
            sanitize(error)
        );
        if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            if method == "approval.decide" {
                session.decisions_pending.clear();
            }
            if method == "session.cancel" {
                session.cancel_pending = false;
            }
            if method == "session.prompt" {
                session.request_pending = false;
                session.synced = false;
                let code = error.split(':').next().unwrap_or("").trim();
                if code == "session_frozen" {
                    session.frozen = true;
                }
                // Runtime's `cancelled` means execution never started. It is a
                // known terminal only when the IPC client explicitly confirms
                // the outcome; a missing/unknown result must keep the lock.
                let known_cancelled =
                    code == "cancelled" && error.ends_with(" (outcome_unknown=false)");
                let known_terminal = !error.contains("outcome_unknown=true")
                    && (known_cancelled
                        || error.contains("request was not sent")
                        || error == "IPC request queue unavailable"
                        || matches!(
                            code,
                            "busy"
                                | "queue_full"
                                | "client_busy"
                                | "session_frozen"
                                | "invalid_params"
                                | "request_too_large"
                                | "request_cancelled"
                        ));
                if known_terminal {
                    // The editor still contains the original text unless the user
                    // edited it. Never restore over a newer draft or resend it.
                    session.pending_prompt = None;
                } else if let Some(prompt) = &mut session.pending_prompt {
                    prompt.uncertain = true;
                }
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.kind == KeyEventKind::Release {
            return vec![];
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && matches!(key.code, KeyCode::Char('q' | 'c')) {
            self.quitting = true;
            return vec![Command::Quit];
        }
        if self.dialog.is_some() {
            return self.dialog_key(key);
        }
        if key.code == KeyCode::F(1)
            || (alt && matches!(key.code, KeyCode::Char('h' | '/' | '?')))
            || (self.focus != Focus::Composer
                && key.code == KeyCode::Char('?')
                && key.modifiers.is_empty())
        {
            self.dialog = Some(Dialog::Help);
            return vec![];
        }
        if ctrl {
            match key.code {
                KeyCode::Char('r') => return vec![Command::Reconnect],
                KeyCode::Char('l') if self.connected => return vec![Command::List],
                KeyCode::Char('n') if self.connected => {
                    self.dialog = Some(Dialog::Create(Editor::default()));
                    return vec![];
                }
                KeyCode::Char('x') if self.connected => {
                    if let Some(session) = self.selected_mut()
                        && !session.cancel_pending
                        && (session.request_pending
                            || matches!(session.state.as_str(), "running" | "awaiting_approval"))
                    {
                        session.cancel_pending = true;
                        if let Some(task_id) = &session.original_task_id {
                            return vec![Command::Cancel {
                                session_id: session.id.clone(),
                                task_id: task_id.clone(),
                            }];
                        }
                        self.status =
                            "Waiting for the original task admission before cancellation.".into();
                    }
                    return vec![];
                }
                KeyCode::Char('f') if self.connected => {
                    if let Some(session) = self.selected().filter(|session| session.can_fork()) {
                        self.dialog = Some(Dialog::Fork {
                            session_id: session.id.clone(),
                            label: format!("{} (chat fork)", session.label),
                        });
                    } else {
                        self.status =
                            "Fork requires a frozen session with all effects settled.".into();
                    }
                    return vec![];
                }
                _ => {}
            }
        }
        if alt {
            let tab = match key.code {
                KeyCode::Char('1') => Some(ViewTab::Chat),
                KeyCode::Char('2') => Some(ViewTab::Files),
                KeyCode::Char('3') => Some(ViewTab::Diff),
                KeyCode::Char('4') => Some(ViewTab::Logs),
                _ => None,
            };
            if let Some(tab) = tab {
                self.view = tab;
                if let Some(session) = self.selected_mut() {
                    session.scroll = 0;
                }
                return vec![];
            }
        }
        if alt && self.connected {
            let decision = match key.code {
                KeyCode::Char('o') => Some(Decision::AllowOnce),
                KeyCode::Char('t') => Some(Decision::AllowTask),
                KeyCode::Char('d') => Some(Decision::Deny),
                _ => None,
            };
            if let Some(decision) = decision {
                return self.decide(decision);
            }
        }
        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Sessions => Focus::History,
                    Focus::History => Focus::Composer,
                    Focus::Composer => Focus::Sessions,
                }
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Sessions => Focus::Composer,
                    Focus::History => Focus::Sessions,
                    Focus::Composer => Focus::History,
                }
            }
            KeyCode::PageUp => self.scroll_history(10),
            KeyCode::PageDown => self.scroll_history(-10),
            KeyCode::Up if self.focus == Focus::Sessions => return self.select_relative(-1),
            KeyCode::Down if self.focus == Focus::Sessions => return self.select_relative(1),
            KeyCode::Enter if self.focus == Focus::Sessions => {
                self.focus = Focus::Composer;
                if self.connected
                    && let Some(session) = self.selected()
                {
                    return vec![Command::Get {
                        session_id: session.id.clone(),
                    }];
                }
            }
            KeyCode::Up if self.focus == Focus::History => self.scroll_history(1),
            KeyCode::Down if self.focus == Focus::History => self.scroll_history(-1),
            KeyCode::Left | KeyCode::Char('[') if self.focus == Focus::History => {
                self.view = self.view.previous();
                if let Some(session) = self.selected_mut() {
                    session.scroll = 0;
                }
            }
            KeyCode::Right | KeyCode::Char(']') if self.focus == Focus::History => {
                self.view = self.view.next();
                if let Some(session) = self.selected_mut() {
                    session.scroll = 0;
                }
            }
            KeyCode::Home if self.focus == Focus::History => {
                if let Some(session) = self.selected_mut() {
                    session.scroll = u16::MAX;
                }
            }
            KeyCode::End if self.focus == Focus::History => {
                if let Some(session) = self.selected_mut() {
                    session.scroll = 0;
                }
            }
            _ if self.focus == Focus::Composer => {
                if key.code == KeyCode::Enter
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
                {
                    return self.send_prompt();
                }
                edit_key(&mut self.input, key);
            }
            _ => {}
        }
        vec![]
    }

    pub fn paste(&mut self, text: &str) {
        match &mut self.dialog {
            Some(Dialog::Create(editor)) => editor.insert(&text.replace(['\r', '\n'], " ")),
            Some(Dialog::Fork { .. } | Dialog::Help) => {}
            None if self.focus == Focus::Composer => self.input.insert(text),
            None => {}
        }
    }

    pub fn scroll_history(&mut self, lines: i16) {
        if let Some(session) = self.selected_mut() {
            session.scroll = if lines >= 0 {
                session.scroll.saturating_add(lines as u16)
            } else {
                session.scroll.saturating_sub(lines.unsigned_abs())
            };
        }
    }

    fn send_prompt(&mut self) -> Vec<Command> {
        if !self.connected || self.input.text().trim().is_empty() {
            return vec![];
        }
        let Some(session) = self.selected().filter(|session| session.can_prompt()) else {
            self.status = "Send blocked: wait for completion, approval, reconnection, or host reconciliation.".into();
            return vec![];
        };
        let id = session.id.clone();
        let text = self.input.text().to_owned();
        let editor_revision = self.input.revision();
        if let Some(session) = self.selected_mut() {
            session.original_task_id = None;
            session.cancel_pending = false;
            session.pending_prompt = Some(PendingPrompt {
                text: text.clone(),
                editor_revision,
                uncertain: false,
            });
            session.request_pending = true;
            session.scroll = 0;
        }
        self.status = "Prompt sent. Ctrl+X requests cancellation.".into();
        vec![Command::Prompt {
            session_id: id,
            text,
        }]
    }

    fn decide(&mut self, decision: Decision) -> Vec<Command> {
        let Some(session) = self
            .selected_mut()
            .filter(|session| session.synced && !session.frozen && !session.has_unknown())
        else {
            return vec![];
        };
        let Some(approval) = session.approvals.first() else {
            return vec![];
        };
        let Some(id) = approval["id"].as_str().map(str::to_owned) else {
            return vec![];
        };
        if !session.decisions_pending.insert(id.clone()) {
            return vec![];
        }
        vec![Command::Decide {
            session_id: session.id.clone(),
            approval_id: id,
            decision,
        }]
    }

    fn dialog_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(dialog) = self.dialog.take() else {
            return vec![];
        };
        if key.code == KeyCode::Esc {
            return vec![];
        }
        match dialog {
            Dialog::Help => {
                if !matches!(
                    key.code,
                    KeyCode::Enter | KeyCode::Char('q' | '?') | KeyCode::F(1)
                ) {
                    self.dialog = Some(Dialog::Help);
                }
            }
            Dialog::Fork { session_id, label } => {
                if key.code == KeyCode::Char('y') && key.modifiers.is_empty() {
                    if self.connected
                        && self
                            .selected()
                            .is_some_and(|session| session.id == session_id && session.can_fork())
                    {
                        return vec![Command::Fork { session_id, label }];
                    }
                } else if key.code != KeyCode::Char('n') {
                    self.dialog = Some(Dialog::Fork { session_id, label });
                }
            }
            Dialog::Create(mut editor) => {
                if key.code == KeyCode::Enter && self.connected {
                    let trimmed = editor.text().trim();
                    let label = if trimmed.is_empty() {
                        format!("Session {}", self.sessions.len() + 1)
                    } else {
                        trimmed.to_owned()
                    };
                    if label.len() <= 1024 {
                        return vec![Command::Create { label }];
                    }
                    self.status = "Session label requires 1–1024 bytes.".into();
                } else {
                    edit_key(&mut editor, key);
                }
                self.dialog = Some(Dialog::Create(editor));
            }
        }
        vec![]
    }

    fn select_relative(&mut self, direction: isize) -> Vec<Command> {
        if self.sessions.is_empty() {
            return vec![];
        }
        let index = self
            .sessions
            .iter()
            .position(|session| Some(&session.id) == self.selected_id.as_ref())
            .unwrap_or(0);
        let next = index
            .saturating_add_signed(direction)
            .min(self.sessions.len() - 1);
        let id = self.sessions[next].id.clone();
        self.select_id(&id);
        if self.connected {
            vec![Command::Get { session_id: id }]
        } else {
            vec![]
        }
    }

    fn select_id(&mut self, id: &str) {
        if self.selected_id.as_deref() == Some(id) {
            return;
        }
        if let Some(previous) = &self.selected_id {
            self.drafts
                .insert(previous.clone(), std::mem::take(&mut self.input));
        }
        self.input = self
            .drafts
            .remove(id)
            .unwrap_or_else(|| std::mem::take(&mut self.input));
        self.selected_id = Some(id.to_owned());
    }
}

fn edit_key(editor: &mut Editor, key: KeyEvent) {
    let control = key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
    match key.code {
        KeyCode::Left => editor.left(),
        KeyCode::Right => editor.right(),
        KeyCode::Up => editor.up(),
        KeyCode::Down => editor.down(),
        KeyCode::Home => editor.home(),
        KeyCode::End => editor.end(),
        KeyCode::Backspace => editor.backspace(),
        KeyCode::Delete => editor.delete(),
        KeyCode::Enter if !control => editor.insert("\n"),
        KeyCode::Char(ch) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
            editor.insert(&ch.to_string())
        }
        _ => {}
    }
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Protocol field {key} must be a nonempty string"))
}

fn array(value: &Value, key: &str) -> Result<Vec<Value>, String> {
    value[key]
        .as_array()
        .cloned()
        .ok_or_else(|| format!("Protocol field {key} must be an array"))
}

pub fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

pub fn message_text(message: &Value) -> String {
    if let Some(content) = message["content"].as_str() {
        return sanitize(content);
    }
    message["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| match part["type"].as_str() {
                    Some("text") => Some(string(part, "text").to_owned()),
                    Some("toolCall") => Some(format!("[tool {}]", string(part, "name"))),
                    Some("image") => Some("[image]".into()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .map_or_else(String::new, |text| sanitize(&text))
}

const MAX_LOGS: usize = 200;
const MAX_INSPECTOR_ITEMS: usize = 64;
const MAX_PREVIEW_BYTES: usize = 4096;

fn clip_text(text: &str, max_bytes: usize) -> String {
    let clean = sanitize(text);
    if clean.len() <= max_bytes {
        return clean;
    }
    let mut end = max_bytes;
    while end > 0 && !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated]", &clean[..end])
}

fn push_log(logs: &mut Vec<String>, entry: String) {
    logs.push(clip_text(&entry, 512));
    if logs.len() > MAX_LOGS {
        logs.drain(0..logs.len() - MAX_LOGS);
    }
}

fn format_event_log(cursor: u64, attempt: Option<&Value>, event: &Value) -> String {
    let kind = string(event, "type");
    let attempt_id = attempt
        .and_then(|value| value["attempt_id"].as_str())
        .map(short_id)
        .unwrap_or("-");
    let detail = match kind {
        "agent" => {
            let inner = &event["event"];
            let inner_kind = string(inner, "type");
            match inner_kind {
                "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
                    let name = string(&inner["call"], "name");
                    let unknown = if inner["result"]["outcome_unknown"] == true {
                        " [outcome_unknown]"
                    } else {
                        ""
                    };
                    format!("agent/{inner_kind} · {name}{unknown}")
                }
                "message_start" | "message_update" | "message_end" => {
                    let role = string(&inner["message"], "role");
                    format!(
                        "agent/{inner_kind} · {}",
                        if role.is_empty() { "assistant" } else { role }
                    )
                }
                _ => format!("agent/{inner_kind}"),
            }
        }
        "approval_requested" | "approval_decided" => {
            let id = short_id(string(&event["approval"], "id"));
            format!("{kind} · {id}")
        }
        "capability_planned" | "capability_started" | "capability_settled" => {
            let call = &event["call"];
            let name = string(call, "name");
            let path = string(&call["arguments"], "path");
            let outcome = call["outcome"]["type"].as_str().unwrap_or("");
            let mut parts = vec![kind.to_owned()];
            if !name.is_empty() {
                parts.push(name.to_owned());
            }
            if !path.is_empty() {
                parts.push(path.to_owned());
            }
            if !outcome.is_empty() {
                parts.push(format!("({outcome})"));
            }
            parts.join(" · ")
        }
        "execution_started" | "execution_finished" => {
            let exec_id = short_id(string(&event["execution"], "execution_id"));
            let state = string(&event["execution"], "state");
            format!("{kind} · {exec_id} ({state})")
        }
        "tool_started" => format!("tool_started · {}", string(event, "call_id")),
        "host" => format!("host · {}", string(event, "kind")),
        "reconciled" => format!(
            "reconciled · {}",
            string(&event["reconciliation"], "call_id")
        ),
        _ => kind.to_owned(),
    };
    format!("#{cursor} [{attempt_id}] {detail}")
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn project_files_and_diffs(
    messages: &[Value],
    approvals: &[Value],
    unresolved_calls: &[Value],
    settled_capabilities: &[Value],
) -> (Vec<FileEntry>, Vec<DiffEntry>) {
    let mut files = Vec::new();
    let mut diffs = Vec::new();
    for call in settled_capabilities {
        collect_from_settled_capability(call, &mut files, &mut diffs);
    }
    for message in messages {
        if message["role"] == "toolResult" {
            collect_from_tool_result(message, &mut files, &mut diffs);
        }
    }
    for call in unresolved_calls {
        let name = string(&call["call"], "name");
        let state = string(call, "state");
        let args = &call["call"]["arguments"];
        if let Some(path) = args["path"].as_str().filter(|path| !path.is_empty()) {
            files.push(FileEntry {
                path: clip_text(path, 240),
                action: clip_text(if name.is_empty() { "call" } else { name }, 64),
                status: clip_text(
                    if state.is_empty() {
                        "unresolved"
                    } else {
                        state
                    },
                    64,
                ),
                preview: "Unresolved call — awaiting runtime settlement or host reconciliation."
                    .into(),
            });
        }
    }
    for approval in approvals {
        let details = if approval["input"].is_object() {
            &approval["input"]
        } else {
            approval
        };
        let capability = string(details, "capability");
        let scope_value = details["scope"]
            .as_str()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .unwrap_or_else(|| details["scope"].clone());
        if let Some(path) = scope_value["path"].as_str().filter(|path| !path.is_empty()) {
            if let (Some(old), Some(new)) =
                (scope_value["old"].as_str(), scope_value["new"].as_str())
            {
                diffs.push(DiffEntry {
                    path: clip_text(path, 240),
                    status: "awaiting_approval".into(),
                    diff: format_replacement_diff(old, new),
                });
            } else if let Some(content) = scope_value["content"].as_str() {
                files.push(FileEntry {
                    path: clip_text(path, 240),
                    action: clip_text(
                        if capability.is_empty() {
                            "approval"
                        } else {
                            capability
                        },
                        64,
                    ),
                    status: "awaiting_approval".into(),
                    preview: clip_text(content, MAX_PREVIEW_BYTES),
                });
            }
        }
    }
    if files.len() > MAX_INSPECTOR_ITEMS {
        files.drain(0..files.len() - MAX_INSPECTOR_ITEMS);
    }
    if diffs.len() > MAX_INSPECTOR_ITEMS {
        diffs.drain(0..diffs.len() - MAX_INSPECTOR_ITEMS);
    }
    (files, diffs)
}

fn collect_from_settled_capability(
    call: &Value,
    files: &mut Vec<FileEntry>,
    diffs: &mut Vec<DiffEntry>,
) {
    let name = string(call, "name");
    let args = &call["arguments"];
    let path = args["path"]
        .as_str()
        .or_else(|| args["file"].as_str())
        .unwrap_or("");
    let outcome_type = call["outcome"]["type"].as_str().unwrap_or("settled");
    let result = &call["outcome"]["result"];
    if outcome_type == "outcome_unknown" {
        files.push(FileEntry {
            path: clip_text(if path.is_empty() { name } else { path }, 240),
            action: clip_text(if name.is_empty() { "capability" } else { name }, 64),
            status: "outcome_unknown".into(),
            preview: "Outcome unknown — host reconciliation required; unverified output hidden."
                .into(),
        });
        return;
    }
    if outcome_type != "completed" {
        if !path.is_empty() {
            let reason = call["outcome"]["reason"]
                .as_str()
                .or_else(|| call["outcome"]["error"].as_str())
                .unwrap_or("not executed");
            files.push(FileEntry {
                path: clip_text(path, 240),
                action: clip_text(if name.is_empty() { "capability" } else { name }, 64),
                status: clip_text(outcome_type, 64),
                preview: clip_text(reason, MAX_PREVIEW_BYTES),
            });
        }
        return;
    }
    if !path.is_empty() {
        if let (Some(old), Some(new)) = (args["old"].as_str(), args["new"].as_str()) {
            diffs.push(DiffEntry {
                path: clip_text(path, 240),
                status: format!("{name} · completed"),
                diff: format_unified_diff(path, Some(old), new),
            });
            files.push(FileEntry {
                path: clip_text(path, 240),
                action: clip_text(name, 64),
                status: "completed".into(),
                preview: format_numbered_lines(new),
            });
        } else if let Some(content) = args["content"].as_str() {
            let bytes = result["bytes"].as_u64();
            let status = match bytes {
                Some(b) => format!("completed · {b} B"),
                None => "completed".into(),
            };
            files.push(FileEntry {
                path: clip_text(path, 240),
                action: clip_text(if name.is_empty() { "write" } else { name }, 64),
                status: status.clone(),
                preview: format_numbered_lines(content),
            });
            diffs.push(DiffEntry {
                path: clip_text(path, 240),
                status: format!(
                    "{} · {status}",
                    if name.is_empty() { "write" } else { name }
                ),
                diff: format_unified_diff(path, None, content),
            });
        } else if let Some(content) = result["content"].as_str() {
            files.push(FileEntry {
                path: clip_text(path, 240),
                action: clip_text(if name.is_empty() { "read" } else { name }, 64),
                status: "completed".into(),
                preview: format_numbered_lines(content),
            });
        }
    }
    collect_value_entry(result, "completed", files, diffs);
}

fn collect_from_tool_result(
    message: &Value,
    files: &mut Vec<FileEntry>,
    diffs: &mut Vec<DiffEntry>,
) {
    let page = &message["structured_content"];
    let page_unknown = page["outcome_unknown"] == true || page["status"] == "outcome_unknown";
    let unknown = message["outcome_unknown"] == true || page_unknown;
    let status = if unknown {
        "outcome_unknown"
    } else if message["is_error"] == true && page["status"] != "cancelled" {
        "failed"
    } else {
        page["status"].as_str().unwrap_or("completed")
    };
    if unknown {
        let tool = string(message, "tool_name");
        files.push(FileEntry {
            path: format!(
                "{} ({})",
                if tool.is_empty() { "tool" } else { tool },
                string(message, "tool_call_id")
            ),
            action: "unknown".into(),
            status: "outcome_unknown".into(),
            preview: "Outcome unknown — host reconciliation required; unverified output hidden."
                .into(),
        });
        return;
    }
    if let Some(outputs) = page["outputs"].as_array() {
        for item in outputs {
            let output = if item["output"].is_object() {
                &item["output"]
            } else {
                item
            };
            collect_value_entry(&output["value"], status, files, diffs);
            if output["type"] == "text"
                && let Some(text) = output["text"].as_str()
                && looks_like_diff(text)
            {
                diffs.push(DiffEntry {
                    path: "cell output".into(),
                    status: status.into(),
                    diff: clip_text(text, MAX_PREVIEW_BYTES),
                });
            }
        }
    }
    collect_value_entry(page, status, files, diffs);
    let text = message_text(message);
    if looks_like_diff(&text) {
        diffs.push(DiffEntry {
            path: clip_text(string(message, "tool_name"), 120),
            status: status.into(),
            diff: clip_text(&text, MAX_PREVIEW_BYTES),
        });
    }
}

fn collect_value_entry(
    value: &Value,
    status: &str,
    files: &mut Vec<FileEntry>,
    diffs: &mut Vec<DiffEntry>,
) {
    if !value.is_object() {
        return;
    }
    let path = value["path"]
        .as_str()
        .or_else(|| value["file"].as_str())
        .unwrap_or("");
    if let Some(diff) = value["diff"]
        .as_str()
        .or_else(|| value["patch"].as_str())
        .filter(|text| !text.is_empty())
    {
        diffs.push(DiffEntry {
            path: clip_text(if path.is_empty() { "diff" } else { path }, 240),
            status: status.into(),
            diff: clip_text(diff, MAX_PREVIEW_BYTES),
        });
    } else if let Some(hunks) = value["hunks"].as_array() {
        let rendered = hunks
            .iter()
            .map(|hunk| {
                if let Some(text) = hunk.as_str() {
                    text.to_owned()
                } else if let Some(header) = hunk["header"].as_str() {
                    let body = hunk["lines"]
                        .as_array()
                        .map(|lines| {
                            lines
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .or_else(|| hunk["patch"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    format!("{header}\n{body}")
                } else {
                    hunk.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !rendered.is_empty() {
            diffs.push(DiffEntry {
                path: clip_text(if path.is_empty() { "hunks" } else { path }, 240),
                status: status.into(),
                diff: clip_text(&rendered, MAX_PREVIEW_BYTES),
            });
        }
    } else if let (Some(old), Some(new)) = (value["old"].as_str(), value["new"].as_str()) {
        diffs.push(DiffEntry {
            path: clip_text(if path.is_empty() { "patch" } else { path }, 240),
            status: status.into(),
            diff: format_replacement_diff(old, new),
        });
    }
    if !path.is_empty()
        && let Some(content) = value["content"].as_str()
    {
        let clean_path = clip_text(path, 240);
        if !files.iter().any(|existing| existing.path == clean_path) {
            files.push(FileEntry {
                path: clean_path,
                action: value["action"].as_str().unwrap_or("file").into(),
                status: status.into(),
                preview: format_numbered_lines(content),
            });
        }
    }
}

fn format_replacement_diff(old: &str, new: &str) -> String {
    let mut out = Vec::new();
    for line in sanitize(old).lines() {
        out.push(format!("-{line}"));
    }
    for line in sanitize(new).lines() {
        out.push(format!("+{line}"));
    }
    clip_text(&out.join("\n"), MAX_PREVIEW_BYTES)
}

fn format_unified_diff(path: &str, old: Option<&str>, new: &str) -> String {
    let clean_path = sanitize(path);
    let new_clean = sanitize(new);
    let new_lines: Vec<&str> = new_clean.lines().collect();
    let mut out = Vec::new();
    match old {
        Some(old_text) => {
            let old_clean = sanitize(old_text);
            let old_lines: Vec<&str> = old_clean.lines().collect();
            out.push(format!("--- a/{clean_path}"));
            out.push(format!("+++ b/{clean_path}"));
            out.push(format!(
                "@@ -1,{} +1,{} @@",
                old_lines.len().max(1),
                new_lines.len().max(1)
            ));
            for line in old_lines {
                out.push(format!("-{line}"));
            }
        }
        None => {
            out.push("--- /dev/null".into());
            out.push(format!("+++ b/{clean_path}"));
            out.push(format!("@@ -0,0 +1,{} @@", new_lines.len().max(1)));
        }
    }
    for line in new_lines {
        out.push(format!("+{line}"));
    }
    clip_text(&out.join("\n"), MAX_PREVIEW_BYTES)
}

fn format_numbered_lines(content: &str) -> String {
    let clean = sanitize(content);
    let lines: Vec<String> = clean
        .lines()
        .enumerate()
        .map(|(idx, line)| format!("{:>3} │ {line}", idx + 1))
        .collect();
    if lines.is_empty() {
        "  1 │ ".into()
    } else {
        clip_text(&lines.join("\n"), MAX_PREVIEW_BYTES)
    }
}

fn looks_like_diff(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("@@ ") || line.starts_with("--- ") || line.starts_with("+++ "))
}
