//! Model-independent messages and streaming providers for the CAYW agent core.

#[cfg(feature = "fault-injection")]
pub mod faults;
pub mod json;
pub mod providers;
mod transcript;
mod types;
mod validation;

pub use tokio_util::sync::CancellationToken;
pub use transcript::*;
pub use types::*;
pub use validation::*;

pub type AssistantStream = futures::stream::BoxStream<'static, AssistantEvent>;

/// Appended to a serialized tool result whose final outcome is unconfirmed.
pub const INTERRUPTED_TOOL_OUTCOME_NOTE: &str = "[Note: Previous tool execution was interrupted before its final outcome was confirmed. Inspect current state before repeating side-effecting operations.]";

/// Add the interruption note to text serialized for the model. An empty tool
/// result is represented by the note alone.
pub fn interrupted_tool_text(text: &str, outcome_unknown: bool) -> String {
    match (text.is_empty(), outcome_unknown) {
        (_, false) => text.to_owned(),
        (true, true) => INTERRUPTED_TOOL_OUTCOME_NOTE.to_owned(),
        (false, true) => format!("{text}\n{INTERRUPTED_TOOL_OUTCOME_NOTE}"),
    }
}

/// Request failures are emitted as `AssistantEvent::Error`, not hidden or retried.
/// Dropping the stream must drop its in-flight request; providers must honor cancellation.
pub trait Provider: Send + Sync {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream;
}
