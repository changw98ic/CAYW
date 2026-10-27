//! Optional Pi → CAYW external host mapping (F15 first cut / SLICE S).
//!
//! Provides:
//! - a version / compatibility matrix (`CompatMatrix`) keyed by plugin@version × surface
//! - family-level mapping from Pi public extension surfaces to CAYW seams
//! - an optional [`PiCompatHost`] that installs / calls / unloads through
//!   `cayw-plugins::PluginRegistry`
//!
//! Install paths:
//! - [`PiFixturePlugin`] (`cayw-pi-fixture@0.1.0`) via `load_native` — CI / offline smoke
//! - [`PiSampleExtension`] (`cayw-pi-sample-ext@0.1.0`) via `load_external` — homemade
//!   CAYW-protocol Rust subprocess (legacy / protocol smoke; **not** Pi ExtensionAPI)
//! - [`PiNpmHelloExtension`] (`@earendil-works/pi-coding-agent@0.84.1` `hello.ts`) via
//!   Node adapter + `load_external` — **primary acceptance**: real unmodified npm Pi
//!   extension speaking Pi ExtensionAPI; adapter translates on the CAYW side only
//!
//! **Proven this cut:** `tools.registerTool` + `tools.invoke` for the pinned
//! `hello.ts` only. Commands / events / `sendMessage` / session / UI are
//! unavailable or incompatible; the npm bridge throws `unsupported:` (not silent
//! noop). This is **not** full Pi ExtensionAPI compatibility and **not** F15
//! complete. Refusals are always explicit.
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
mod npm_ext;
mod sample;
mod types;

pub use error::PiCompatError;
pub use fixture::PiFixturePlugin;
pub use host::PiCompatHost;
pub use mapping::{FAMILY_MAPPINGS, FamilyMapping, mapping_for};
pub use matrix::{CompatMatrix, CompatRow};
pub use npm_ext::PiNpmHelloExtension;
pub use sample::PiSampleExtension;
pub use types::*;
