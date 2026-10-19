//! Offline acceptance tools. The product Runtime does not depend on this crate.
//! Custom scenarios implement [`sdk::Scenario`] and run through the same evidence writer.
pub mod measurement;
pub mod provenance;
pub mod sdk;
