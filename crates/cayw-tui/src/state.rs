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
            Self::Cancel { session_id } => ("session.cancel", json!({"session_id":session_id})),
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
    pub scroll: u16,
    pub synced: bool,
    pub request_pending: bool,
    pub cancel_pending: bool,
    decisions_pending: BTreeSet<String>,
    pending_prompt: Option<PendingPrompt>,
    active_attempt: Option<AttemptIdentity>,
    stream_replay: StreamReplay,
    refresh_after: Option<u64>,
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
            scroll: 0,
            synced: true,
            request_pending: false,
            cancel_pending: false,
            decisions_pending: BTreeSet::new(),
            pending_prompt: None,
            active_attempt,
            stream_replay: StreamReplay::default(),
            refresh_after: None,
        };
        view.protect_unknown();
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
        let mut incoming = SessionView::from_snapshot(value)?;
        let id = incoming.id.clone();
        if let Some(index) = self.sessions.iter().position(|session| session.id == id) {
            let old = &self.sessions[index];
            if incoming.snapshot_cursor < old.snapshot_cursor.max(old.cursor) {
                // A slow list/get response must not overwrite already consumed events.
                if select {
                    self.select_id(&id);
                }
                return Ok(());
            }
            incoming.scroll = old.scroll;
            incoming.cursor = old.cursor;
            incoming.stream_replay = old.stream_replay.clone();
            incoming.project_stream();
            if matches!(incoming.state.as_str(), "running" | "awaiting_approval") {
                incoming.tool_status = old.tool_status.clone();
            }
            incoming.refresh_after = old
                .refresh_after
                .filter(|cursor| *cursor > incoming.snapshot_cursor);
            incoming.synced = incoming.refresh_after.is_none();
            incoming.pending_prompt = old.pending_prompt.clone();
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
            self.sessions[index] = incoming;
        } else {
            self.sessions.push(incoming);
        }
        if select || self.selected_id.is_none() {
            self.select_id(&id);
        }
        Ok(())
    }

    pub fn apply_list(&mut self, value: Value) -> Result<(), String> {
        let list = value
            .as_array()
            .ok_or("session.list did not return an array")?;
        // Validate the complete page before mutating existing projections.
        let mut ids = BTreeSet::new();
        for value in list {
            let session = SessionView::from_snapshot(value.clone())?;
            if !ids.insert(session.id) {
                return Err("session.list contains duplicate ids".into());
            }
        }
        for value in list {
            self.upsert_snapshot(value.clone(), false)?;
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
                        return vec![Command::Cancel {
                            session_id: session.id.clone(),
                        }];
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
            Some(Dialog::Fork { .. }) => {}
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
                    let label = editor.text().trim();
                    if !label.is_empty() && label.len() <= 1024 {
                        return vec![Command::Create {
                            label: label.into(),
                        }];
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
