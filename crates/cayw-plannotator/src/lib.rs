//! Annotation / plan review feedback for CAYW workbench surfaces (F08 first cut).
//!
//! Humans annotate plans, Markdown, diffs, or local pages in place. Submissions
//! become **structured feedback** bound to `Session` + target + file version,
//! then inject into a **subsequent** Agent attempt. Version mismatch refuses
//! silent apply — stale annotations never rewrite onto new content.
//!
//! First cut (F08 / SLICE I) + audit remediation:
//! - `FeedbackStore` records `StructuredFeedback` (process-local; Runtime may
//!   mirror into the Core session event log)
//! - `apply_feedback_for_attempt` checks live file versions before injection
//! - `inject` folds [`ApplyOutcome::Applied`] into Agent SystemMessage sections
//! - decisions: `Approve` / `DenyWithFeedback` / `ApproveWithNotes`
//!
//! Browser Plannotator UI and Pi extension install remain out of scope.
//!
//! Design reference (not a code transplant):
//! [backnotprop/plannotator](https://github.com/backnotprop/plannotator) /
//! [@plannotator/pi-extension](https://pi.dev/packages/@plannotator/pi-extension).
//! See `docs/sources/f08-plannotator-feedback.md`.
#![forbid(unsafe_code)]

mod apply;
mod inject;
mod store;
mod types;

pub use apply::{
    ApplyContext, ApplyOutcome, LiveFileVersion, applied_payloads, apply_feedback_for_attempt,
};
pub use inject::{
    FEEDBACK_SECTION_KEY, applied_feedback_section, collect_applied_section, feedback_section_json,
};
pub use store::{FeedbackDraft, FeedbackStore};
pub use types::*;

/// Wire / document protocol for this first-cut feedback layer.
pub const PLANNOTATOR_PROTOCOL_VERSION: u32 = 1;
