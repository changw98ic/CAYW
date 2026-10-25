//! Optional deterministic command-output filtering for CAYW (F02 / SLICE B).
//!
//! First cut:
//! - toggleable [`FilterOptions`] with built-in deterministic profiles
//! - dual entry: filtered summary **and** full original text (never dropped)
//! - output-reduction metrics kept separate from task wall/cost/re-read compare
//!
//! Host/Runtime/CodeMode wiring and F01 result-handle persistence are out of
//! scope. Design reference (not a code transplant):
//! [rtk-ai/rtk](https://github.com/rtk-ai/rtk) (rtk-oss command filters).
//! See `docs/sources/f02-output-filter.md`.
#![forbid(unsafe_code)]

mod ansi;
mod compare;
mod dual;
mod pipeline;
mod profiles;
mod types;

pub use compare::{CompareReport, TaskRunCost, compare_filter_toggle};
pub use dual::{DualEntry, dual_entry};
pub use pipeline::{FilterError, apply_filter};
pub use profiles::{FilterProfile, builtin_profile};
pub use types::*;

/// Wire / document protocol for this first-cut output-filter surface.
pub const OUTPUT_FILTER_PROTOCOL_VERSION: u32 = 1;
