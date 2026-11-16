use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Content {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime_type: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    ToolCall(ToolCall),
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
            arguments_error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemMessage {
    pub content: String,
    #[serde(default)]
    pub sections: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub tools_added: Vec<ToolDefinition>,
    #[serde(default)]
    pub tools_removed: Vec<String>,
}

/// Who produced a user-role message. The role alone is not the author: the
/// runtime also uses that role to show a notice to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageAuthor {
    /// Typed or submitted by the user.
    #[default]
    User,
    /// Written by the runtime: a status notice, a due reminder, or a route prompt.
    Runtime,
}

impl MessageAuthor {
    pub fn is_user(self) -> bool {
        matches!(self, Self::User)
    }
}

fn author_is_user(author: &MessageAuthor) -> bool {
    author.is_user()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User {
        content: Vec<Content>,
        timestamp: u64,
        /// Absent on older records. Those remain user-authored.
        #[serde(default, skip_serializing_if = "author_is_user")]
        author: MessageAuthor,
    },
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    /// Host messages are retained in history, but not sent to a provider by default.
    Custom {
        kind: String,
        data: Value,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![Content::text(text)],
            timestamp: timestamp_ms(),
            author: MessageAuthor::User,
        }
    }

    /// A user-role message the runtime wrote. It can be shown to the model, but
    /// it is not something the user said.
    pub fn runtime(text: impl Into<String>) -> Self {
        Self::user(text).with_author(MessageAuthor::Runtime)
    }

    pub fn with_author(mut self, author: MessageAuthor) -> Self {
        if let Self::User { author: slot, .. } = &mut self {
            *slot = author;
        }
        self
    }

    /// True only when this message is a user-role turn the user actually authored.
    pub fn is_user_statement(&self) -> bool {
        matches!(
            self,
            Self::User {
                author: MessageAuthor::User,
                ..
            }
        )
    }
    pub fn system(text: impl Into<String>) -> Self {
        Self::System(SystemMessage {
            content: text.into(),
            ..Default::default()
        })
    }
    pub fn text(&self) -> String {
        match self {
            Self::System(system) => system.content.clone(),
            Self::User { content, .. } => content_text(content),
            Self::Assistant(message) => content_text(&message.content),
            Self::ToolResult(message) => content_text(&message.content),
            Self::Custom { .. } => String::new(),
        }
    }
}

pub fn content_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    #[default]
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

impl StopReason {
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Error | Self::Aborted)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Input excludes cache reads/writes; reasoning is a subset of output.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub total_tokens: u64,
}

impl Usage {
    pub fn update_total(&mut self) {
        self.total_tokens = self
            .input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write);
    }
}

/// Availability of the provider's usage report; numeric defaults are not a cost report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    #[default]
    Unknown,
    Partial,
    /// The provider supplied both input and output token counts, including explicit zeroes.
    Reported,
    /// The provider supplied a malformed, contradictory or decreasing report.
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<Content>,
    pub provider: String,
    pub model: String,
    pub response_id: Option<String>,
    pub response_model: Option<String>,
    pub usage: Usage,
    /// Missing historical availability remains unknown; counts alone cannot recover it.
    #[serde(default)]
    pub usage_status: UsageStatus,
    /// Present protocol usage fields, including a rejected report's values.
    #[serde(default)]
    pub usage_fields: BTreeMap<String, Value>,
    /// Adapter-owned replay data, including original item identities and opaque signatures.
    /// Adapters validate it against normalized content before sending it to a service.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_metadata: BTreeMap<String, Value>,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    pub timestamp: u64,
    pub duration_ms: Option<u64>,
}

impl AssistantMessage {
    pub fn new(model: &Model) -> Self {
        Self {
            content: vec![],
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_id: None,
            response_model: None,
            usage: Usage::default(),
            usage_status: UsageStatus::Unknown,
            usage_fields: BTreeMap::new(),
            provider_metadata: BTreeMap::new(),
            stop_reason: StopReason::Pending,
            error_message: None,
            timestamp: timestamp_ms(),
            duration_ms: None,
        }
    }
    pub fn failure(mut self, reason: StopReason, error: impl Into<String>) -> Self {
        self.stop_reason = reason;
        self.error_message = Some(error.into());
        self
    }
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|part| match part {
                Content::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<Content>,
    pub details: Option<Value>,
    pub structured_content: Option<Value>,
    pub usage: Option<Usage>,
    pub is_error: bool,
    /// Cancellation stopped waiting; a host must reconcile effects before retrying.
    pub outcome_unknown: bool,
    pub timestamp: u64,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub supports_images: bool,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

impl Model {
    pub fn new(provider: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            provider: provider.into(),
            supports_images: false,
            context_window: None,
            max_output_tokens: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub thinking_level: ThinkingLevel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: Model,
    pub messages: Vec<Message>,
    pub options: StreamOptions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantEvent {
    /// A canonical historical checkpoint. Live providers emit deltas between boundaries.
    Snapshot {
        message: AssistantMessage,
    },
    Start {
        message: AssistantMessage,
    },
    /// A usage report was received, even when the wire frame has no content.
    UsageUpdate {
        usage: Usage,
        usage_status: UsageStatus,
        usage_fields: BTreeMap<String, Value>,
        response_id: Option<String>,
        response_model: Option<String>,
    },
    TextStart {
        index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        index: usize,
        delta: String,
    },
    TextEnd {
        index: usize,
        partial: AssistantMessage,
    },
    ThinkingStart {
        index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        index: usize,
        delta: String,
    },
    ThinkingEnd {
        index: usize,
        partial: AssistantMessage,
    },
    ToolCallStart {
        index: usize,
        partial: AssistantMessage,
    },
    ToolCallDelta {
        index: usize,
        delta: String,
    },
    ToolCallEnd {
        index: usize,
        partial: AssistantMessage,
    },
    Done {
        message: AssistantMessage,
    },
    Error {
        message: AssistantMessage,
    },
}

impl AssistantEvent {
    /// Full snapshots occur at message and block boundaries, not per delta.
    pub fn snapshot(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Snapshot { message }
            | Self::Start { message }
            | Self::Done { message }
            | Self::Error { message } => Some(message),
            Self::TextStart { partial, .. }
            | Self::TextEnd { partial, .. }
            | Self::ThinkingStart { partial, .. }
            | Self::ThinkingEnd { partial, .. }
            | Self::ToolCallStart { partial, .. }
            | Self::ToolCallEnd { partial, .. } => Some(partial),
            Self::UsageUpdate { .. }
            | Self::TextDelta { .. }
            | Self::ThinkingDelta { .. }
            | Self::ToolCallDelta { .. } => None,
        }
    }

    pub fn usage_update(message: &AssistantMessage) -> Self {
        Self::UsageUpdate {
            usage: message.usage.clone(),
            usage_status: message.usage_status,
            usage_fields: message.usage_fields.clone(),
            response_id: message.response_id.clone(),
            response_model: message.response_model.clone(),
        }
    }

    /// Fold an ordered Provider event into the current view without cloning the
    /// growing message for each text fragment. Tool argument fragments accumulate
    /// as an incomplete raw string until a block-end or terminal snapshot confirms the call.
    pub fn apply_to(&self, message: &mut AssistantMessage) -> Result<(), String> {
        if let Some(snapshot) = self.snapshot() {
            *message = snapshot.clone();
            return Ok(());
        }
        if let Self::UsageUpdate {
            usage,
            usage_status,
            usage_fields,
            response_id,
            response_model,
        } = self
        {
            message.usage = usage.clone();
            message.usage_status = *usage_status;
            message.usage_fields = usage_fields.clone();
            message.response_id = response_id.clone();
            message.response_model = response_model.clone();
            return Ok(());
        }
        let (index, delta, kind) = match self {
            Self::TextDelta { index, delta } => (*index, delta, "text"),
            Self::ThinkingDelta { index, delta } => (*index, delta, "thinking"),
            Self::ToolCallDelta { index, delta } => (*index, delta, "tool call"),
            _ => unreachable!("snapshot handled above"),
        };
        match (kind, message.content.get_mut(index)) {
            ("text", Some(Content::Text { text })) => text.push_str(delta),
            ("thinking", Some(Content::Thinking { thinking, .. })) => thinking.push_str(delta),
            ("tool call", Some(Content::ToolCall(call))) => {
                if !call.arguments.is_string() {
                    call.arguments = Value::String(String::new());
                }
                let Value::String(raw) = &mut call.arguments else {
                    unreachable!()
                };
                raw.push_str(delta);
                call.arguments_error = Some("tool argument stream is incomplete".into());
            }
            _ => return Err(format!("{kind} delta does not match content block {index}")),
        }
        Ok(())
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }
}
