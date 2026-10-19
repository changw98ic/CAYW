//! Durable headless sessions and transport-independent versioned contracts.
//!
//! The host supplies tools and authority. [`SessionStore`] records decisions and
//! fences workers; it does not grant capabilities or undo external side effects.

mod error;
mod protocol;
mod store;
mod types;

pub use error::CoreError;
pub use protocol::*;
pub use store::SessionStore;
pub use types::*;
