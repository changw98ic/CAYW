//! Public DTOs for F02 output filtering.

use serde::{Deserialize, Serialize};

/// How filtering should behave for one command capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterOptions {
    /// When false, filtered view equals original (still records metrics).
    pub enabled: bool,
    /// Named deterministic profile; ignored when `enabled` is false.
    pub profile: crate::profiles::FilterProfile,
    /// Keep first N non-empty lines as head context (0 = none).
    #[serde(default = "default_head")]
    pub head_lines: usize,
    /// Keep last N non-empty lines as tail context (0 = none).
    #[serde(default = "default_tail")]
    pub tail_lines: usize,
    /// Absolute cap on filtered lines after selection (0 = unlimited).
    #[serde(default)]
    pub max_lines: usize,
}

fn default_head() -> usize {
    8
}

fn default_tail() -> usize {
    12
}

impl Default for FilterOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            profile: crate::profiles::FilterProfile::Generic,
            head_lines: default_head(),
            tail_lines: default_tail(),
            max_lines: 200,
        }
    }
}

/// Byte/line reduction for one filter application.
///
/// This is **not** a claim that the task was cheaper or faster overall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputReduction {
    pub original_bytes: usize,
    pub filtered_bytes: usize,
    pub original_lines: usize,
    pub filtered_lines: usize,
    /// True when filtered text differs from original.
    pub changed: bool,
}

impl OutputReduction {
    /// Fraction of bytes removed, in basis points (0..=10_000). None if original empty.
    pub fn reduction_bps(&self) -> Option<u32> {
        if self.original_bytes == 0 {
            return None;
        }
        let removed = self.original_bytes.saturating_sub(self.filtered_bytes);
        Some(((removed as u128) * 10_000 / self.original_bytes as u128) as u32)
    }
}

/// Result of applying a filter once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterResult {
    /// Full original command output (UTF-8 lossy if needed upstream).
    pub original: String,
    /// Filtered / summary view for context injection.
    pub filtered: String,
    pub options: FilterOptions,
    pub reduction: OutputReduction,
    /// SHA-256 hex of original UTF-8 bytes (stable identity for dual entry).
    pub original_digest: String,
    /// Profile / rule name that produced `filtered`.
    pub applied_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutputFilterError {
    #[error("invalid output filter input: {0}")]
    Invalid(String),
}
