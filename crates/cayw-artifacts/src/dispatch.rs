//! Host / Runtime dispatch bridge for accepted artifact actions (F19).
//!
//! `cayw-artifacts` does not depend on `cayw-runtime` or `cayw-host`. Consumers
//! implement [`HostActionDispatch`] by forwarding to the real paths already used
//! elsewhere:
//! - `FollowUpTask` → Session / Agent follow-up (same route as Plannotator /
//!   public SDK prompt insert for the owning Session)
//! - `InvokeTool` → Host capability call (`Host::call` / Runtime request-tool
//!   invoke) for an already-authorized tool name — never a new capability grant
//!
//! [`RecordingHostDispatch`] is the in-crate stand-in that proves the bridge is
//! actually invoked (tests + offline example).

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;

/// Inputs for a follow-up task handoff to Runtime / Agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUpDispatch<'a> {
    pub action_id: &'a str,
    pub artifact_id: &'a str,
    pub session_id: &'a str,
    pub prompt: &'a str,
}

/// Inputs for an already-authorized tool invoke via Host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDispatch<'a> {
    pub action_id: &'a str,
    pub artifact_id: &'a str,
    pub session_id: &'a str,
    pub tool: &'a str,
    pub args_json: &'a str,
}

/// Structured result returned by a successful host/runtime handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostDispatchResult {
    /// JSON object text recorded on the action (`outcome_json`).
    pub outcome_json: String,
}

/// Real execution bridge. Runtime/Host consumers implement this; the protocol
/// calls it for every accepted `FollowUpTask` / `InvokeTool`.
pub trait HostActionDispatch: Send + Sync {
    fn follow_up_task(&self, req: FollowUpDispatch<'_>) -> Result<HostDispatchResult, String>;
    fn invoke_tool(&self, req: ToolDispatch<'_>) -> Result<HostDispatchResult, String>;
    /// Downcast support for product-path assertions (Recording vs Runtime).
    fn as_any(&self) -> &dyn std::any::Any;
}

/// One recorded bridge call (test / offline proof).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordedHostCall {
    FollowUpTask {
        action_id: String,
        artifact_id: String,
        session_id: String,
        prompt: String,
    },
    InvokeTool {
        action_id: String,
        artifact_id: String,
        session_id: String,
        tool: String,
        args_json: String,
    },
}

/// In-memory bridge that records every call and returns a success payload.
/// Used by tests and the offline example to prove dispatch hits a real handler
/// (not just pending JSON). Production wires Runtime/Host behind the same trait.
#[derive(Debug, Default)]
pub struct RecordingHostDispatch {
    calls: Mutex<Vec<RecordedHostCall>>,
}

impl RecordingHostDispatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calls(&self) -> Vec<RecordedHostCall> {
        self.calls.lock().expect("dispatch lock").clone()
    }

    pub fn clear(&self) {
        self.calls.lock().expect("dispatch lock").clear();
    }
}

impl HostActionDispatch for RecordingHostDispatch {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn follow_up_task(&self, req: FollowUpDispatch<'_>) -> Result<HostDispatchResult, String> {
        self.calls
            .lock()
            .expect("dispatch lock")
            .push(RecordedHostCall::FollowUpTask {
                action_id: req.action_id.to_owned(),
                artifact_id: req.artifact_id.to_owned(),
                session_id: req.session_id.to_owned(),
                prompt: req.prompt.to_owned(),
            });
        Ok(HostDispatchResult {
            outcome_json: json!({
                "kind": "follow_up_task_dispatched",
                "session_id": req.session_id,
                "artifact_id": req.artifact_id,
                "action_id": req.action_id,
                "prompt": req.prompt,
            })
            .to_string(),
        })
    }

    fn invoke_tool(&self, req: ToolDispatch<'_>) -> Result<HostDispatchResult, String> {
        self.calls
            .lock()
            .expect("dispatch lock")
            .push(RecordedHostCall::InvokeTool {
                action_id: req.action_id.to_owned(),
                artifact_id: req.artifact_id.to_owned(),
                session_id: req.session_id.to_owned(),
                tool: req.tool.to_owned(),
                args_json: req.args_json.to_owned(),
            });
        Ok(HostDispatchResult {
            outcome_json: json!({
                "kind": "tool_invoked",
                "session_id": req.session_id,
                "artifact_id": req.artifact_id,
                "action_id": req.action_id,
                "tool": req.tool,
                "args_json": req.args_json,
            })
            .to_string(),
        })
    }
}

/// Shared bridge handle used by [`crate::ActionProtocol`].
pub type SharedHostDispatch = Arc<dyn HostActionDispatch>;
