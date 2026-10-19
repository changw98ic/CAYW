use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fmt, time::Duration};
use url::{Host, Url};

pub const VERSION: u32 = 1;

/// Errors deliberately contain no transport text, request payload, URL or credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum RemoteError {
    #[error("authentication rejected")]
    Unauthorized,
    #[error("device revoked")]
    Revoked,
    #[error("pairing or binding expired")]
    Expired,
    #[error("identifier already used with different content")]
    Conflict,
    #[error("record not found")]
    NotFound,
    #[error("device disconnected")]
    Offline,
    #[error("capacity reached")]
    Busy,
    #[error("invalid request")]
    InvalidRequest,
    #[error("incompatible protocol version")]
    IncompatibleVersion,
    #[error("message exceeds configured bound")]
    TooLarge,
    #[error("TLS or an explicit existing TLS terminator declaration is required")]
    TlsRequired,
    #[error("transport disconnected; query status before taking further action")]
    Disconnected,
    #[error("operation timed out; query status before taking further action")]
    Timeout,
    #[error("request cancelled with a known outcome")]
    Cancelled,
    #[error("execution failed with a known outcome")]
    HandlerFailed,
    #[error("execution outcome unknown")]
    OutcomeUnknown,
    #[error("durable storage unavailable")]
    Storage,
    #[error("configuration invalid")]
    Configuration,
}

#[derive(Clone)]
pub struct Secret(String);
impl Secret {
    /// Supply at least 256 bits of random entropy, encoded as a header-safe string.
    pub fn new(value: impl Into<String>) -> Result<Self, RemoteError> {
        let value = value.into();
        if !(32..=256).contains(&value.len())
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(RemoteError::Configuration);
        }
        Ok(Self(value))
    }
    pub fn generate() -> Result<Self, RemoteError> {
        Self::new(random_id()?)
    }
    /// Explicit escape hatch for host credential storage; never log this value.
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

pub(crate) fn random_id() -> Result<String, RemoteError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| RemoteError::Configuration)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub(crate) fn hash(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
pub(crate) fn now_ms() -> Result<i64, RemoteError> {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| RemoteError::Storage)?;
    i64::try_from(time.as_millis()).map_err(|_| RemoteError::Storage)
}
pub(crate) fn deadline(duration: Duration) -> Result<i64, RemoteError> {
    now_ms()?
        .checked_add(i64::try_from(duration.as_millis()).map_err(|_| RemoteError::Configuration)?)
        .ok_or(RemoteError::Configuration)
}

#[derive(Clone, Debug)]
pub struct DeviceCredential {
    pub device_id: String,
    pub secret: Secret,
}
#[derive(Clone, Debug)]
pub struct PendingBind {
    pub bind_id: String,
    pub code: Secret,
    pub secret: Secret,
}
impl PendingBind {
    /// Persist this entire pending handshake on the Device before sending it.
    pub fn new(code: Secret) -> Result<Self, RemoteError> {
        Ok(Self {
            bind_id: random_id()?,
            code,
            secret: Secret::generate()?,
        })
    }
}
#[derive(Clone, Debug)]
pub struct PairingCode {
    pub code: Secret,
    pub expires_at_ms: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceStatus {
    pub device_id: String,
    pub generation: u64,
    pub connected: bool,
    pub revoked: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    Running,
    CancelRequested,
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
}
impl RequestState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::CancelRequested)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RequestRecord {
    pub request_id: String,
    pub device_id: String,
    pub generation: u64,
    pub state: RequestState,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    pub value: Option<Value>,
    pub error: Option<RemoteError>,
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_frame_bytes: usize,
    pub queue_capacity: usize,
    pub max_inflight_per_device: usize,
    pub max_connections: usize,
    /// Also bounds request tombstones. Exhaustion rejects new IDs; never evicts deduplication records.
    pub max_records: usize,
    pub request_timeout: Duration,
    pub handshake_timeout: Duration,
    /// Whole, nonzero milliseconds; the server supplies the connection timing.
    pub heartbeat_interval: Duration,
    /// Whole milliseconds, strictly greater than `heartbeat_interval`.
    pub heartbeat_timeout: Duration,
    pub bind_ttl: Duration,
    pub max_pair_ttl: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 64 * 1024,
            queue_capacity: 32,
            max_inflight_per_device: 16,
            max_connections: 128,
            max_records: 100_000,
            request_timeout: Duration::from_secs(60),
            handshake_timeout: Duration::from_secs(10),
            heartbeat_interval: Duration::from_secs(10),
            heartbeat_timeout: Duration::from_secs(30),
            bind_ttl: Duration::from_secs(600),
            max_pair_ttl: Duration::from_secs(600),
        }
    }
}
impl Limits {
    pub(crate) fn validate(&self) -> Result<(), RemoteError> {
        if !(1024..=16 * 1024 * 1024).contains(&self.max_frame_bytes)
            || !(1..=4096).contains(&self.queue_capacity)
            || self.max_inflight_per_device == 0
            || !(1..=65_536).contains(&self.max_connections)
            || !(1..=10_000_000).contains(&self.max_records)
            || self.max_inflight_per_device > 4096
            || [
                self.request_timeout,
                self.handshake_timeout,
                self.heartbeat_interval,
                self.heartbeat_timeout,
                self.bind_ttl,
                self.max_pair_ttl,
            ]
            .iter()
            .any(|duration| duration.is_zero() || *duration > Duration::from_secs(86_400))
            // Heartbeat values cross the wire as whole milliseconds. Reject
            // truncation here, before either endpoint opens a connection/store.
            || [self.heartbeat_interval, self.heartbeat_timeout]
                .iter()
                .any(|duration| duration.subsec_nanos() % 1_000_000 != 0)
            || self.heartbeat_timeout <= self.heartbeat_interval
        {
            return Err(RemoteError::Configuration);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default)]
pub struct ServerConfig {
    pub limits: Limits,
    /// Explicit deployment declaration; this crate does not create a TLS proxy.
    pub tls_terminated: bool,
    /// If set, requests carrying an Origin must exactly match this value.
    /// Browser Origin headers are rejected by default; native clients send none.
    pub allowed_origin: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ServerUrl {
    url: Url,
}
impl ServerUrl {
    /// An explicit origin URL. Remote cleartext needs an explicit existing TLS
    /// termination declaration. No credentials, query, fragment or base path.
    pub fn new(value: &str, tls_terminated: bool) -> Result<Self, RemoteError> {
        let url = Url::parse(value).map_err(|_| RemoteError::InvalidRequest)?;
        if !matches!(url.scheme(), "ws" | "wss")
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(RemoteError::InvalidRequest);
        }
        let local = match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if url.scheme() == "ws" && !local && !tls_terminated {
            return Err(RemoteError::TlsRequired);
        }
        Ok(Self { url })
    }
    pub fn endpoint(&self, role: &str) -> String {
        let mut url = self.url.clone();
        url.set_path(&format!("/v1/{role}"));
        url.into()
    }
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    pub version: u32,
    pub request_id: String,
    pub device_id: String,
    pub generation: u64,
    pub body: Body,
}
// Never derive Debug: this enum carries pairing secrets and user payloads.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Body {
    Pair {
        ttl_ms: u64,
    },
    Pairing {
        code: String,
        expires_at_ms: i64,
    },
    Bind {
        bind_id: String,
        code: String,
        secret: String,
    },
    Bound,
    Hello {
        max_frame_bytes: usize,
        max_inflight: usize,
        max_records: usize,
        queue_capacity: usize,
    },
    Welcome {
        max_frame_bytes: usize,
        max_inflight: usize,
        max_records: usize,
        queue_capacity: usize,
        heartbeat_interval_ms: u64,
        heartbeat_timeout_ms: u64,
    },
    Invoke {
        payload: Value,
    },
    Execute {
        payload: Value,
    },
    Outcome {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_value"
        )]
        value: Option<Value>,
        error: Option<RemoteError>,
    },
    Status,
    Cancel,
    Revoke,
    DeviceStatus,
    DeviceInfo {
        status: DeviceStatus,
    },
    Record {
        record: RequestRecord,
    },
    Ack,
    Error {
        error: RemoteError,
    },
}
impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Body([redacted])")
    }
}
impl Envelope {
    pub(crate) fn new(request_id: String, device_id: String, generation: u64, body: Body) -> Self {
        Self {
            version: VERSION,
            request_id,
            device_id,
            generation,
            body,
        }
    }
    pub(crate) fn validate(&self) -> Result<(), RemoteError> {
        if self.version != VERSION {
            return Err(RemoteError::IncompatibleVersion);
        }
        if !valid_id(&self.request_id) || (!self.device_id.is_empty() && !valid_id(&self.device_id))
        {
            return Err(RemoteError::InvalidRequest);
        }
        Ok(())
    }
    pub(crate) fn encode(&self, limit: usize) -> Result<String, RemoteError> {
        let result = serde_json::to_string(self).map_err(|_| RemoteError::InvalidRequest)?;
        if result.len() > limit {
            return Err(RemoteError::TooLarge);
        }
        Ok(result)
    }
}
