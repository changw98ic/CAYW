//! Turn applied feedback into Agent next-attempt message/context material.
//!
//! [`ApplyOutcome::Applied`] alone is only a return value. Callers must fold
//! payloads into the Agent prompt via [`FEEDBACK_SECTION_KEY`] (a SystemMessage
//! section). Payload JSON is the wire form — no prose reformatting.

use crate::apply::{ApplyOutcome, applied_payloads};
use crate::types::{AgentFeedbackPayload, PlannotatorError};

/// SystemMessage.sections key carrying structured plannotator feedback.
pub const FEEDBACK_SECTION_KEY: &str = "cayw-plannotator-feedback";

/// Serialize applied payloads for a SystemMessage section body.
///
/// Returns `None` when there is nothing to inject.
pub fn feedback_section_json(
    payloads: &[&AgentFeedbackPayload],
) -> Result<Option<String>, PlannotatorError> {
    if payloads.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(payloads).map(Some).map_err(|error| {
        PlannotatorError::Invalid(format!("feedback payload serialization failed: {error}"))
    })
}

/// Collect Applied outcomes into a SystemMessage section body (rejects ignored).
pub fn collect_applied_section(
    outcomes: &[ApplyOutcome],
) -> Result<Option<String>, PlannotatorError> {
    feedback_section_json(&applied_payloads(outcomes))
}

/// Build `(section_key, body)` for injection into Agent context, if any Applied.
pub fn applied_feedback_section(
    outcomes: &[ApplyOutcome],
) -> Result<Option<(String, String)>, PlannotatorError> {
    Ok(collect_applied_section(outcomes)?.map(|body| (FEEDBACK_SECTION_KEY.to_string(), body)))
}
