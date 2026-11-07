//! Dual entry: filtered summary + full original under one digest identity.

use serde::{Deserialize, Serialize};

use crate::types::FilterResult;

/// Two retrieval paths for the same command capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DualEntry {
    /// Stable id = SHA-256 of original UTF-8 bytes.
    pub original_digest: String,
    pub summary: String,
    pub original: String,
    pub filter_enabled: bool,
    pub applied_profile: String,
}

/// Build dual entry from a [`FilterResult`].
pub fn dual_entry(result: &FilterResult) -> DualEntry {
    DualEntry {
        original_digest: result.original_digest.clone(),
        summary: result.filtered.clone(),
        original: result.original.clone(),
        filter_enabled: result.options.enabled,
        applied_profile: result.applied_profile.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::apply_filter;
    use crate::types::FilterOptions;

    #[test]
    fn dual_preserves_both() {
        let r = apply_filter("ok\nerror: x\n", FilterOptions::default()).unwrap();
        let d = dual_entry(&r);
        assert_eq!(d.original, r.original);
        assert_eq!(d.summary, r.filtered);
        assert!(!d.original_digest.is_empty());
    }
}
