//! Optional Pi → CAYW external host mapping (F15 first cut / SLICE S).
//!
//! Provides:
//! - a version / compatibility matrix (`CompatMatrix`) keyed by plugin@version × surface
//! - family-level mapping from Pi public extension surfaces to CAYW seams
//! - an optional [`PiCompatHost`] that installs / calls / unloads through
//!   `cayw-plugins::PluginRegistry`
//!
//! First cut does **not** load unmodified npm Pi ExtensionAPI packages. The
//! offline [`PiFixturePlugin`] (`cayw-pi-fixture@0.1.0`) proves the host
//! lifecycle path; real package rows are recorded as `unavailable` until a
//! later Node/ABI bridge lands. Refusals are always explicit
//! (`incompatible` / `unavailable`), never silent success.
//!
//! Design reference (not a code transplant):
//! [weijiafu14/pi2dsh](https://github.com/weijiafu14/pi2dsh) compat-layer shape.
//! See `docs/sources/f15-pi-compat-host.md`.
#![forbid(unsafe_code)]

mod error;
mod fixture;
mod host;
mod mapping;
mod matrix;
mod types;

pub use error::PiCompatError;
pub use fixture::PiFixturePlugin;
pub use host::PiCompatHost;
pub use mapping::{FAMILY_MAPPINGS, FamilyMapping, mapping_for};
pub use matrix::{CompatMatrix, CompatRow};
pub use types::*;
