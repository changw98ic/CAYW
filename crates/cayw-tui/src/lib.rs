//! Optional terminal client for the host-owned CAYW runtime.
//! This crate never executes model requests or tools itself.

pub mod config;
pub mod driver;
pub mod input;
pub mod render;
pub mod state;
pub mod terminal;
pub mod transport;

pub use input::{Editor, sanitize};
pub use render::draw;
pub use state::{App, Command, Decision, Dialog, Focus, SessionView};
