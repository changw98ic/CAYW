//! Durable, bounded cells over the CodeMode VM.
//!
//! Runtime registers [`CellTool`] as its only `execute_code` tool and retains
//! the manager between replies. [`CellBackend`] receives the immutable trusted
//! origin of every cell and the explicit sequence of every real capability.
//! SQLite commits intents before dispatch and results before Lua resumes.
//! Opening a journal only opens/validates storage. Crash recovery requires an
//! explicit [`ExecutorStopped`] assertion and the task execution lock; a lost
//! Lua stack is never replayed. Runtime recovery uses [`SqliteJournal::stopped_scope`]
//! before saving any host repairs, retains [`StoppedScope`] across the host and
//! Cells commits, and supplies the persisted facts through [`HostSettlement`].
//! Ordinary call evidence cannot clear a backend lifecycle failure: that requires
//! [`HostExecutionSettlement`] bound to the original failure. After all records
//! settle, [`StoppedScope::finish`] requires no active session VM; Runtime then
//! explicitly calls [`ReadCoordinator::thaw`] while retaining the guard. Dropping
//! the guard before finish keeps the freeze. No Core dependency is required.
//!
//! Each cell has its own writable globals and `_G`; trusted helper closures use
//! the guarded CodeMode dispatcher. Live output is durably validated before Lua
//! continues. Ordinary known errors can be handled in Luau. Explicit stops remain
//! typed in durable records and pages; normal exit ends the cell, while unknown
//! effects keep the session frozen until reconciliation.
//!
//! The original [`CellBackend::call`] independently admits every invocation,
//! including shared hits/followers. Audio remains typed
//! JSON in the Agent result because cayw-ai has no native audio variant.

mod backend;
mod bridge;
mod cache;
mod journal;
mod manager;
mod settlement;
mod tool;
mod types;
pub mod values;

pub use backend::{BackendCompletion, BackendResponse, CellBackend};
pub use cache::{
    ReadCandidate, ReadCoordinator, ReadOrigin, ReadPolicy, ReadRole, ReadSource, ReadStamp,
};
pub use journal::SqliteJournal;
pub use manager::CellManager;
pub use settlement::StoppedScope;
pub use tool::CellTool;
pub use types::*;
pub use values::{
    AuthorizedCellBackend, CELL_LOAD, CELL_STORE, CellValueAuthorizer, CellValueExecutor,
    CellValueOperation, authorized_cell_backend,
};
