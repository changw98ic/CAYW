//! Optional relay for Devices that establish outbound WebSocket connections.
//!
//! The host supplies the execution handler. This crate creates no shell, model,
//! MCP connection or Device listener. See the crate README for protocol and TLS
//! boundaries, durable request states, and the limits of local verification.
mod client;
mod protocol;
mod server;
mod store;

pub use client::{DeviceHandler, OperatorClient, RemoteDevice};
pub use protocol::{
    DeviceCredential, DeviceStatus, Limits, PairingCode, PendingBind, RemoteError, RequestRecord,
    RequestState, Secret, ServerConfig, ServerUrl, VERSION,
};
pub use server::{ServerState, serve};
pub use tokio_util::sync::CancellationToken;
