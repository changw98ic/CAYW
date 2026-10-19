//! Authenticated, versioned JSON RPC over loopback TCP.
//!
//! See the crate README for the wire format, resource limits and cancellation contract.
//! This transport never reconnects or replays calls. An unknown outcome requires the host
//! to reconcile durable runtime state before deciding what to do next.

mod client;
mod server;
mod wire;

pub use client::Client;
pub use server::serve;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, io, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// The only wire version accepted by this crate.
pub const PROTOCOL_VERSION: u32 = 1;

/// A terminal RPC error. Codes ending in `_outcome_unknown` (or `outcome_unknown`)
/// explicitly mean that side effects may have happened.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

impl RpcError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Host-provided dispatch. `hello` and `rpc.cancel` are reserved by the transport.
///
/// The returned future must own its work: do not detach side-effect tasks. On cancellation
/// the transport cancels the supplied token, polls for `cancellation_grace`, then drops
/// the future. Dropping a future cannot undo external side effects.
#[async_trait]
pub trait RpcHandler: Send + Sync + 'static {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError>;
}

/// Per-server limits. Each active connection has its own request and response limits.
/// Construct with [`ServerConfig::new`] so authentication has no default secret.
#[derive(Clone)]
pub struct ServerConfig {
    pub token: String,
    /// Maximum JSON bytes per frame, excluding the terminating newline (256..=64 MiB).
    pub max_frame_bytes: usize,
    /// Simultaneously executing handler futures per connection; extra calls receive `busy`.
    pub max_in_flight: usize,
    pub request_timeout: Duration,
    pub max_connections: usize,
    /// All frame IDs, including hello, rejected calls and cancel frames, consume this budget.
    pub max_requests_per_connection: usize,
    /// UTF-8 byte length, not character count.
    pub max_id_bytes: usize,
    pub response_queue_capacity: usize,
    pub handshake_timeout: Duration,
    /// A complete frame must arrive within this interval, including idle time between frames.
    pub frame_timeout: Duration,
    pub write_timeout: Duration,
    pub cancellation_grace: Duration,
}

impl ServerConfig {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            max_frame_bytes: 1024 * 1024,
            max_in_flight: 64,
            request_timeout: Duration::from_secs(30),
            max_connections: 128,
            max_requests_per_connection: 65_536,
            max_id_bytes: 128,
            response_queue_capacity: 64,
            handshake_timeout: Duration::from_secs(5),
            frame_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(5),
            cancellation_grace: Duration::from_millis(250),
        }
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        let counts = [
            self.max_in_flight,
            self.max_connections,
            self.response_queue_capacity,
        ];
        let durations = [
            self.request_timeout,
            self.handshake_timeout,
            self.frame_timeout,
            self.write_timeout,
            self.cancellation_grace,
        ];
        if self.token.is_empty()
            || !valid_frame_limit(self.max_frame_bytes)
            || self.max_requests_per_connection < 2
            || self.max_id_bytes == 0
            || self.max_id_bytes > self.max_frame_bytes / 2
            || counts
                .iter()
                .any(|&value| value == 0 || value > Semaphore::MAX_PERMITS)
            || durations.iter().any(|&value| !valid_duration(value))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid IPC server configuration",
            ));
        }
        let hello = wire::Request::hello(self.token.clone());
        wire::encode(&hello, self.max_frame_bytes)
            .map(|_| ())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "authentication frame exceeds configured limit",
                )
            })
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("token", &"[redacted]")
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("max_in_flight", &self.max_in_flight)
            .field("request_timeout", &self.request_timeout)
            .field("max_connections", &self.max_connections)
            .field(
                "max_requests_per_connection",
                &self.max_requests_per_connection,
            )
            .field("max_id_bytes", &self.max_id_bytes)
            .field("response_queue_capacity", &self.response_queue_capacity)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("frame_timeout", &self.frame_timeout)
            .field("write_timeout", &self.write_timeout)
            .field("cancellation_grace", &self.cancellation_grace)
            .finish()
    }
}

/// Local limits are intersected with the limits advertised by the authenticated server.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub max_frame_bytes: usize,
    pub max_in_flight: usize,
    pub max_requests_per_connection: usize,
    pub max_id_bytes: usize,
    pub write_queue_capacity: usize,
    /// Bounds TCP connection and the entire hello exchange together.
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub write_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: 1024 * 1024,
            max_in_flight: 64,
            max_requests_per_connection: 65_536,
            max_id_bytes: 128,
            write_queue_capacity: 64,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(5),
        }
    }
}

impl ClientConfig {
    pub(crate) fn validate(&self) -> Result<(), ClientError> {
        let counts = [self.max_in_flight, self.write_queue_capacity];
        let durations = [
            self.connect_timeout,
            self.request_timeout,
            self.write_timeout,
        ];
        if !valid_frame_limit(self.max_frame_bytes)
            || self.max_requests_per_connection < 2
            || self.max_id_bytes == 0
            || self.max_id_bytes > self.max_frame_bytes / 2
            || counts
                .iter()
                .any(|&value| value == 0 || value > Semaphore::MAX_PERMITS)
            || durations.iter().any(|&value| !valid_duration(value))
        {
            return Err(ClientError::known(
                "invalid_config",
                "invalid IPC client configuration",
            ));
        }
        Ok(())
    }
}

fn valid_frame_limit(value: usize) -> bool {
    (256..=64 * 1024 * 1024).contains(&value)
}
fn valid_duration(value: Duration) -> bool {
    !value.is_zero() && std::time::Instant::now().checked_add(value).is_some()
}

/// `outcome_unknown` must be checked before retrying a side-effecting operation.
/// Errors before admission are known not to have dispatched this call. Once admitted,
/// timeouts, protocol violations and connection loss conservatively report unknown.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClientError {
    pub code: String,
    pub message: String,
    pub outcome_unknown: bool,
}

impl ClientError {
    pub(crate) fn known(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            outcome_unknown: false,
        }
    }
    pub(crate) fn unknown(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            outcome_unknown: true,
        }
    }
}

impl From<RpcError> for ClientError {
    fn from(error: RpcError) -> Self {
        Self {
            outcome_unknown: error.code == "outcome_unknown"
                || error.code.ends_with("_outcome_unknown"),
            code: error.code,
            message: error.message,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} (outcome_unknown={})",
            self.code, self.message, self.outcome_unknown
        )
    }
}
impl std::error::Error for ClientError {}
