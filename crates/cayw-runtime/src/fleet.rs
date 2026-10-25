//! F12 FleetView: live sub-agent inventory, stop and steer.
//!
//! Design reference: nicobailon/pi-subagents FleetView / inspector shape.
//! Scheduling, budgets, results and recovery remain owned by CAYW Session/Worker;
//! this module does not transplant upstream TypeScript.

use crate::{Runtime, RuntimeError, TaskState, lock};
use cayw_agent::AgentControl;
use cayw_ai::{CancellationToken, Message};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// How `agent.run` binds the child to the parent capability call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunMode {
    /// Parent capability waits for the child's durable terminal (existing Issue 5).
    Foreground,
    /// Parent capability returns a handle immediately; the child keeps running.
    Background,
}

impl AgentRunMode {
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self, RuntimeError> {
        match raw {
            None | Some("foreground") => Ok(Self::Foreground),
            Some("background") => Ok(Self::Background),
            Some(other) => Err(RuntimeError::InvalidInput(format!(
                "agent.run mode must be foreground or background, not {other}"
            ))),
        }
    }

    pub(crate) fn detached(self) -> bool {
        matches!(self, Self::Background)
    }
}

/// Coarse FleetView membership state (not a second Task queue).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMemberState {
    Running,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

impl FleetMemberState {
    pub(crate) fn from_task(state: TaskState) -> Self {
        match state {
            TaskState::Completed => Self::Completed,
            TaskState::Failed => Self::Failed,
            TaskState::Cancelled => Self::Cancelled,
            TaskState::OutcomeUnknown | TaskState::Draining | TaskState::Interrupted => {
                Self::Unknown
            }
            _ => Self::Running,
        }
    }
}

/// One row in the FleetView snapshot.
#[derive(Clone, Debug, Serialize)]
pub struct FleetMember {
    pub child_task_id: String,
    pub child_session_id: String,
    pub parent_session_id: String,
    pub parent_task_id: String,
    pub label: String,
    pub depth: usize,
    pub mode: AgentRunMode,
    /// Background children are detached: parent cancel / drain must not stop them.
    pub detached: bool,
    pub state: FleetMemberState,
    pub error: Option<String>,
}

/// Bounded FleetView page for SDK / IPC / WebUI.
#[derive(Clone, Debug, Serialize)]
pub struct FleetView {
    pub members: Vec<FleetMember>,
    pub next_after: Option<String>,
}

pub(crate) struct FleetEntry {
    pub parent_session_id: String,
    pub parent_task_id: String,
    pub child_session_id: String,
    pub label: String,
    pub depth: usize,
    pub mode: AgentRunMode,
    pub detached: bool,
    pub cancel: CancellationToken,
    pub state: FleetMemberState,
    pub error: Option<String>,
    /// Live-slot release target shared with the root RunScope tree.
    pub live: Arc<AtomicUsize>,
    pub slot_held: bool,
    pub control: Mutex<Option<AgentControl>>,
}

/// Runtime-wide inventory of local sub-agents for FleetView and intervention.
#[derive(Default)]
pub(crate) struct FleetRegistry {
    members: Mutex<BTreeMap<String, FleetEntry>>,
}

impl FleetRegistry {
    pub(crate) fn register(
        &self,
        child_task_id: String,
        entry: FleetEntry,
    ) -> Result<(), RuntimeError> {
        let mut members = lock(&self.members);
        if members.contains_key(&child_task_id) {
            return Err(RuntimeError::InvalidInput(
                "fleet already tracks this child task".into(),
            ));
        }
        members.insert(child_task_id, entry);
        Ok(())
    }

    pub(crate) fn attach_control(&self, child_session_id: &str, control: AgentControl) {
        let members = lock(&self.members);
        for entry in members.values() {
            if entry.child_session_id == child_session_id {
                *lock(&entry.control) = Some(control);
                return;
            }
        }
    }

    pub(crate) fn clear_control(&self, child_session_id: &str) {
        let members = lock(&self.members);
        for entry in members.values() {
            if entry.child_session_id == child_session_id {
                *lock(&entry.control) = None;
                return;
            }
        }
    }

    pub(crate) fn cancel_token(&self, child_task_id: &str) -> Option<CancellationToken> {
        lock(&self.members)
            .get(child_task_id)
            .map(|entry| entry.cancel.clone())
    }

    pub(crate) fn mark_terminal(
        &self,
        child_task_id: &str,
        state: FleetMemberState,
        error: Option<String>,
        release_slot: bool,
    ) {
        let mut members = lock(&self.members);
        let Some(entry) = members.get_mut(child_task_id) else {
            return;
        };
        entry.state = state;
        entry.error = error;
        *lock(&entry.control) = None;
        if release_slot && entry.slot_held {
            entry.live.fetch_sub(1, Ordering::AcqRel);
            entry.slot_held = false;
        }
    }

    pub(crate) fn snapshot(
        &self,
        parent_session_id: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<FleetView, RuntimeError> {
        if limit == 0 || limit > 1000 {
            return Err(RuntimeError::InvalidInput(
                "fleet page limit must be 1..1000".into(),
            ));
        }
        let members = lock(&self.members);
        let mut rows: Vec<FleetMember> = members
            .iter()
            .filter(|(_, entry)| {
                parent_session_id.is_none_or(|parent| entry.parent_session_id == parent)
            })
            .filter(|(id, _)| after.is_none_or(|cursor| id.as_str() > cursor))
            .map(|(id, entry)| FleetMember {
                child_task_id: id.clone(),
                child_session_id: entry.child_session_id.clone(),
                parent_session_id: entry.parent_session_id.clone(),
                parent_task_id: entry.parent_task_id.clone(),
                label: entry.label.clone(),
                depth: entry.depth,
                mode: entry.mode,
                detached: entry.detached,
                state: entry.state,
                error: entry.error.clone(),
            })
            .collect();
        rows.sort_by(|left, right| left.child_task_id.cmp(&right.child_task_id));
        let next_after = (rows.len() > limit).then(|| rows[limit - 1].child_task_id.clone());
        rows.truncate(limit);
        Ok(FleetView {
            members: rows,
            next_after,
        })
    }

    pub(crate) fn stop(&self, child_task_id: &str) -> Result<Value, RuntimeError> {
        let members = lock(&self.members);
        let entry = members.get(child_task_id).ok_or_else(|| {
            RuntimeError::InvalidInput(format!("fleet has no child task {child_task_id}"))
        })?;
        if !matches!(entry.state, FleetMemberState::Running) {
            return Ok(json!({
                "child_task_id": child_task_id,
                "state": entry.state,
                "stopped": false,
                "reason": "not_running",
            }));
        }
        entry.cancel.cancel();
        if let Some(control) = lock(&entry.control).as_ref() {
            control.abort();
        }
        Ok(json!({
            "child_task_id": child_task_id,
            "state": FleetMemberState::Running,
            "stopped": true,
        }))
    }

    pub(crate) fn steer(&self, child_task_id: &str, text: &str) -> Result<Value, RuntimeError> {
        if text.trim().is_empty() || text.len() > 256 * 1024 {
            return Err(RuntimeError::InvalidInput(
                "fleet steer text must contain 1..262144 bytes".into(),
            ));
        }
        let members = lock(&self.members);
        let entry = members.get(child_task_id).ok_or_else(|| {
            RuntimeError::InvalidInput(format!("fleet has no child task {child_task_id}"))
        })?;
        if !matches!(entry.state, FleetMemberState::Running) {
            return Err(RuntimeError::InvalidInput(
                "fleet steer requires a running child".into(),
            ));
        }
        let control = lock(&entry.control);
        let Some(control) = control.as_ref() else {
            return Err(RuntimeError::InvalidInput(
                "fleet steer control is not attached yet".into(),
            ));
        };
        control.steer(Message::user(text));
        Ok(json!({
            "child_task_id": child_task_id,
            "steered": true,
            "bytes": text.len(),
        }))
    }
}

impl Runtime {
    /// FleetView page. Optional `parent_session_id` scopes to one parent Session.
    pub fn fleet(
        &self,
        parent_session_id: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<FleetView, RuntimeError> {
        self.inner.fleet.snapshot(parent_session_id, after, limit)
    }

    /// Request stop for one fleet member. Idempotent when already terminal.
    pub fn fleet_stop(&self, child_task_id: &str) -> Result<Value, RuntimeError> {
        if child_task_id.trim().is_empty() || child_task_id.len() > 128 {
            return Err(RuntimeError::InvalidInput(
                "child_task_id must contain 1..128 bytes".into(),
            ));
        }
        self.inner.fleet.stop(child_task_id)
    }

    /// Append a steering user message to a running fleet member.
    pub fn fleet_steer(&self, child_task_id: &str, text: &str) -> Result<Value, RuntimeError> {
        if child_task_id.trim().is_empty() || child_task_id.len() > 128 {
            return Err(RuntimeError::InvalidInput(
                "child_task_id must contain 1..128 bytes".into(),
            ));
        }
        self.inner.fleet.steer(child_task_id, text)
    }
}
