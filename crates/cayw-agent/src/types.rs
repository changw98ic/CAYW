use crate::AgentTool;
use async_trait::async_trait;
use cayw_ai::{
    AssistantMessage, CancellationToken, Message, Model, StreamOptions, ToolCall, ToolResultMessage,
};
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};
use thiserror::Error;

pub use cayw_agent_types::AgentEvent;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

#[derive(Clone)]
pub struct AgentContext {
    pub model: Model,
    pub options: StreamOptions,
    pub messages: Vec<Message>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

#[derive(Debug, Clone)]
pub struct AgentSnapshot {
    pub model: Model,
    pub messages: Vec<Message>,
    pub is_running: bool,
    pub streaming_message: Option<AssistantMessage>,
    pub pending_tool_calls: BTreeSet<String>,
    pub error_message: Option<String>,
    /// A cancelled/dropped tool may have produced effects that the host must inspect.
    pub needs_reconciliation: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub messages: Vec<Message>,
    pub last_message: AssistantMessage,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum AgentError {
    #[error("The agent is already running")]
    Busy,
    #[error("Cannot continue an empty or system-only conversation")]
    EmptyHistory,
    #[error("Continuing after an assistant message requires steering or follow-up input")]
    NeedsInput,
    #[error("A prompt must contain at least one message")]
    EmptyPrompt,
    #[error("Tool effects are unresolved; supply reconciled history before continuing")]
    UnresolvedToolEffects,
    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),
    #[error("Event listener failed: {0}")]
    Listener(String),
    #[error("Agent callback failed: {0}")]
    Callback(String),
}

#[derive(Clone)]
pub struct ToolHookContext {
    pub assistant: AssistantMessage,
    pub call: ToolCall,
    pub arguments: Value,
    pub context: AgentContext,
}

#[derive(Debug, Clone, Default)]
pub struct BeforeToolDecision {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ToolResultPatch {
    pub content: Option<Vec<cayw_ai::Content>>,
    pub details: Option<Value>,
    /// `Some(None)` clears the field; omitted with a content override also clears it.
    pub structured_content: Option<Option<Value>>,
    pub is_error: Option<bool>,
    pub usage: Option<cayw_ai::Usage>,
    pub terminate: Option<bool>,
}

impl ToolResultPatch {
    pub fn apply(self, result: &mut crate::ToolResult) {
        if let Some(content) = self.content {
            result.content = content;
            result.structured_content = None;
        }
        if let Some(content) = self.structured_content {
            result.structured_content = content;
        }
        if let Some(details) = self.details {
            result.details = Some(details);
        }
        if let Some(value) = self.is_error {
            result.is_error = value;
        }
        if let Some(value) = self.usage {
            result.usage = Some(value);
        }
        if let Some(value) = self.terminate {
            result.terminate = value;
        }
    }
}

#[derive(Clone)]
pub struct TurnContext {
    pub assistant: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: AgentContext,
    pub new_messages: Vec<Message>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TurnDecision {
    #[default]
    Auto,
    Continue,
    End,
}

#[derive(Clone, Default)]
pub struct NextTurnUpdate {
    pub context: Option<AgentContext>,
    pub messages: Vec<Message>,
}

/// Hooks are awaited at explicit turn boundaries. They must honor cancellation
/// and must not detach side effects that outlive their returned future.
#[async_trait]
pub trait AgentHooks: Send + Sync {
    async fn prepare_request(
        &self,
        _context: AgentContext,
        _cancel: CancellationToken,
    ) -> Result<Option<AgentContext>, String> {
        Ok(None)
    }
    async fn before_tool_call(
        &self,
        _context: ToolHookContext,
        _cancel: CancellationToken,
    ) -> Result<BeforeToolDecision, String> {
        Ok(BeforeToolDecision::default())
    }
    async fn after_tool_call(
        &self,
        _context: ToolHookContext,
        _result: crate::ToolResult,
        _cancel: CancellationToken,
    ) -> Result<ToolResultPatch, String> {
        Ok(ToolResultPatch::default())
    }
    async fn finish_turn(
        &self,
        _turn: TurnContext,
        _cancel: CancellationToken,
    ) -> Result<TurnDecision, String> {
        Ok(TurnDecision::Auto)
    }
    async fn prepare_next_turn(
        &self,
        _turn: TurnContext,
        _cancel: CancellationToken,
    ) -> Result<NextTurnUpdate, String> {
        Ok(NextTurnUpdate::default())
    }
}

pub(crate) struct NoHooks;
impl AgentHooks for NoHooks {}
