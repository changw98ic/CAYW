//! Device-owned local authority, independent of the network and agent runtime.
//! See the crate README for storage, threat, quota and durability boundaries.
#![forbid(unsafe_code)]

mod local;
mod types;
pub use local::ContentStore;
#[cfg(feature = "fault-injection")]
pub use local::fail_current_state_directory_sync;
pub use types::*;

#[cfg(feature = "encryption")]
mod encrypted;
#[cfg(feature = "encryption")]
pub use encrypted::{EncryptedSnapshot, KeyHandle, Manifest, RemoteObjectStore, RestoreMode};
