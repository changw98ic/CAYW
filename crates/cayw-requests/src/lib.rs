//! Task-scoped requests over an existing, permission-gated host catalog.
//!
//! Model-facing tools can request or inspect; only trusted Rust host code can
//! start tasks, evaluate, grant, revoke, or finish them. Requests execute no work.
mod authority;
mod catalog;
mod integration;
mod store;
mod tools;
mod types;

pub use catalog::{HostIdentity, OperationCatalog, OperationScope};
pub use integration::{RequestGate, RequestHost, RequestHostConfig};
pub use store::RequestStore;
pub use tools::RequestTools;
pub use types::*;
