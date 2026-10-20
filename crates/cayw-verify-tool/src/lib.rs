//! Offline acceptance tools. The product Runtime does not depend on this crate.
//! Custom scenarios implement [`sdk::Scenario`] and run through the same evidence writer.
pub mod cases;
pub mod child;
pub mod evidence;
mod fixtures;
pub use fixtures::SOURCE as OFFLINE_SOURCE;
pub mod measurement;
pub mod provenance;
pub mod runner;
pub mod sdk;
