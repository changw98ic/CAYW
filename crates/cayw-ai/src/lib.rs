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

/// Request failures are emitted as `AssistantEvent::Error`, not hidden or retried.
/// Dropping the stream must drop its in-flight request; providers must honor cancellation.
pub trait Provider: Send + Sync {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream;
}
