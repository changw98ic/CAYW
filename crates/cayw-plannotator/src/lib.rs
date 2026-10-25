//! Annotation / plan review feedback for CAYW workbench surfaces (F08 first cut).
//!
//! Humans annotate plans, Markdown, diffs, or local pages in place. Submissions
//! become **structured feedback** bound to `Session` + target + file version,
//! then inject into a **subsequent** Agent attempt. Version mismatch refuses
//! silent apply — stale annotations never rewrite onto new content.
//!
//! First cut (F08 / SLICE I):
//! - `FeedbackStore` (process-local) records `StructuredFeedback`
//! - `apply_feedback_for_attempt` checks live file versions before injection
//! - decisions: `Approve` / `DenyWithFeedback` / `ApproveWithNotes`
//!
//! Browser Plannotator UI, Pi extension install, TUI/WebUI render wiring, and
//! durable Core ledger persistence are out of scope for this slice.
//!
//! Design reference (not a code transplant):
//! [backnotprop/plannotator](https://github.com/backnotprop/plannotator) /
//! [@plannotator/pi-extension](https://pi.dev/packages/@plannotator/pi-extension).
//! See `docs/sources/f08-plannotator-feedback.md`.
#![forbid(unsafe_code)]

mod apply;
mod store;
mod types;

pub use apply::{
    ApplyContext, ApplyOutcome, LiveFileVersion, applied_payloads, apply_feedback_for_attempt,
};
pub use store::{FeedbackDraft, FeedbackStore};
pub use types::*;

/// Wire / document protocol for this first-cut feedback layer.
pub const PLANNOTATOR_PROTOCOL_VERSION: u32 = 1;
