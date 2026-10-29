//! Cross-component agent event and tool-result contracts.
//!
//! This crate only owns the existing shared event/result types. It is not an
//! independent disk or wire format: both types still use `cayw-ai` message,
//! stream and usage types, and Core keeps reading and writing Core v3 bytes.

use cayw_ai::{
    AssistantEvent, AssistantMessage, Content, Message, ToolCall, ToolResultMessage, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<Message>,
    },
    TurnStart,
    TurnEnd {
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: Message,
    },
    MessageUpdate {
        message: AssistantMessage,
        event: Box<AssistantEvent>,
    },
    MessageEnd {
        message: Message,
    },
    ToolExecutionStart {
        call: ToolCall,
    },
    ToolExecutionUpdate {
        call: ToolCall,
        result: crate::ToolResult,
    },
    ToolExecutionEnd {
        call: ToolCall,
        result: crate::ToolResult,
        duration_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<Content>,
    pub details: Option<Value>,
    pub structured_content: Option<Value>,
    pub usage: Option<Usage>,
    pub is_error: bool,
    pub terminate: bool,
    pub outcome_unknown: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(text)],
            ..Default::default()
        }
    }
    pub fn error(error: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(error)
        }
    }
}
