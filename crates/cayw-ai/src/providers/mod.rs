//! HTTP model protocol adapters.

mod openai;
mod sse;

pub use openai::{OpenAiProvider, ProviderConfigError};
