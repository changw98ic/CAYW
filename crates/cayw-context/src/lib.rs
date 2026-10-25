//! Context / token / cost / cache overview for CAYW run insight (F05 first cut).
//!
//! Builds read-only observation views from native model-request accounting
//! snapshots. This crate does **not** invent prices, tokenize messages, or
//! rewrite the durable ledger. Unknown costs stay empty; cache rates are only
//! reported when the denominator is positive.
//!
//! First cut (F05 / SLICE F):
//! - per-session token / cost / cache overview
//! - composition by provider-reported token parts (input / output / cache /
//!   reasoning)
//! - request-level evolution steps
//! - cross-session KPI aggregation
//! - parent / child fleet overview sharing one budget lineage
//!
//! Message-category composition (user / inject / skill / …), Insights heatmaps,
//! TUI/WebUI rendering, and Runtime wiring are out of scope for this slice.
//!
//! Design reference (not a code transplant):
//! [bowenliang123/dsh-context](https://github.com/bowenliang123/dsh-context).
//! See `docs/sources/f05-context-usage.md` for the source record.
#![forbid(unsafe_code)]

mod evolution;
mod fleet;
mod overview;
mod tokens;
mod types;

pub use evolution::{EvolutionStep, build_evolution};
pub use fleet::{FleetMember, FleetOverview, build_fleet_overview};
pub use overview::{
    CrossSessionOverview, SessionOverview, build_cross_session_overview, build_session_overview,
};
pub use tokens::{TokenTotals, aggregate_tokens, cache_hit_rate, merge_cost};
pub use types::*;

/// Wire / document protocol for this first-cut overview.
pub const CONTEXT_PROTOCOL_VERSION: u32 = 1;
