//! Lossy, bounded observation. A slow or disconnected reader never blocks the
//! caller. Hosts classify fields; credentials and sensitive key names are always
//! redacted. Observer events do not grant permission or feed an executor.

use crate::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    Public,
    Private,
    Thinking,
    Credential,
}

#[derive(Debug, Clone)]
pub struct ObservedField {
    pub value: Value,
    pub sensitivity: Sensitivity,
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub kind: String,
    pub fields: BTreeMap<String, ObservedField>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedactedObservation {
    pub kind: String,
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct ObserverConfig {
    pub capacity: usize,
    pub max_event_bytes: usize,
    pub allow_private: bool,
    pub allow_thinking: bool,
    /// Additional case-insensitive exact field names, at any depth.
    pub redact_keys: BTreeSet<String>,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self {
            capacity: 128,
            max_event_bytes: 16 * 1024,
            allow_private: false,
            allow_thinking: false,
            redact_keys: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    Queued,
    DroppedFull,
    DroppedClosed,
    DroppedOversize,
    DroppedInvalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserverStats {
    pub queued: u64,
    pub dropped_full: u64,
    pub dropped_closed: u64,
    pub dropped_oversize: u64,
    pub dropped_invalid: u64,
}

#[derive(Default)]
struct Counters {
    queued: AtomicU64,
    full: AtomicU64,
    closed: AtomicU64,
    oversize: AtomicU64,
    invalid: AtomicU64,
}

#[derive(Clone)]
pub struct Observer {
    manifest: PluginManifest,
    sender: mpsc::Sender<RedactedObservation>,
    config: Arc<ObserverConfig>,
    counters: Arc<Counters>,
    stopped: CancellationToken,
}

pub struct ObservationReader {
    receiver: mpsc::Receiver<RedactedObservation>,
    stopped: CancellationToken,
}

impl ObservationReader {
    pub async fn recv(&mut self) -> Option<RedactedObservation> {
        tokio::select! {
            biased;
            _ = self.stopped.cancelled() => {
                self.receiver.close();
                self.receiver.recv().await
            },
            event = self.receiver.recv() => event,
        }
    }
    pub fn try_recv(&mut self) -> Result<RedactedObservation, mpsc::error::TryRecvError> {
        if self.stopped.is_cancelled() {
            self.receiver.close();
        }
        self.receiver.try_recv()
    }
    /// Closes new writes; already queued events can still be drained.
    pub fn close(&mut self) {
        self.receiver.close();
    }
}

impl Observer {
    pub fn channel(config: ObserverConfig) -> Result<(Self, ObservationReader), PluginError> {
        if config.capacity == 0 || config.capacity > 1_000_000 || config.max_event_bytes == 0 {
            return Err(PluginError::InvalidArguments(
                "invalid observer capacity or event limit".into(),
            ));
        }
        let (sender, receiver) = mpsc::channel(config.capacity);
        let manifest = PluginManifest {
            name: "observer".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kind: PluginKind::Native,
            protocol_version: PLUGIN_PROTOCOL_VERSION,
            capabilities: vec![ToolDefinition {
                name: "observer.stats".into(),
                description: "Read observation delivery counters".into(),
                parameters: serde_json::json!({"type":"object","additionalProperties":false}),
            }],
            permissions: BTreeMap::new(),
        };
        let stopped = CancellationToken::new();
        Ok((
            Self {
                manifest,
                sender,
                config: Arc::new(config),
                counters: Arc::new(Counters::default()),
                stopped: stopped.clone(),
            },
            ObservationReader { receiver, stopped },
        ))
    }

    pub fn publish(&self, observation: Observation) -> PublishOutcome {
        if self.stopped.is_cancelled() || self.sender.is_closed() {
            self.counters.closed.fetch_add(1, Ordering::Relaxed);
            return PublishOutcome::DroppedClosed;
        }
        if !valid_name(&observation.kind) || observation.fields.keys().any(|key| !valid_name(key)) {
            self.counters.invalid.fetch_add(1, Ordering::Relaxed);
            return PublishOutcome::DroppedInvalid;
        }
        let fields = observation
            .fields
            .into_iter()
            .map(|(key, field)| {
                let allowed = match field.sensitivity {
                    Sensitivity::Public => true,
                    Sensitivity::Private => self.config.allow_private,
                    Sensitivity::Thinking => self.config.allow_thinking,
                    Sensitivity::Credential => false,
                };
                let value = if allowed && !sensitive_key(&key, &self.config) {
                    redact(field.value, &self.config, 0)
                } else {
                    Value::String("[redacted]".into())
                };
                (key, value)
            })
            .collect();
        let event = RedactedObservation {
            kind: observation.kind,
            fields,
        };
        if serde_json::to_vec(&event)
            .map_or(true, |bytes| bytes.len() > self.config.max_event_bytes)
        {
            self.counters.oversize.fetch_add(1, Ordering::Relaxed);
            return PublishOutcome::DroppedOversize;
        }
        match self.sender.try_send(event) {
            Ok(()) => {
                self.counters.queued.fetch_add(1, Ordering::Relaxed);
                PublishOutcome::Queued
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.counters.full.fetch_add(1, Ordering::Relaxed);
                PublishOutcome::DroppedFull
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.counters.closed.fetch_add(1, Ordering::Relaxed);
                PublishOutcome::DroppedClosed
            }
        }
    }

    pub fn stats(&self) -> ObserverStats {
        ObserverStats {
            queued: self.counters.queued.load(Ordering::Relaxed),
            dropped_full: self.counters.full.load(Ordering::Relaxed),
            dropped_closed: self.counters.closed.load(Ordering::Relaxed),
            dropped_oversize: self.counters.oversize.load(Ordering::Relaxed),
            dropped_invalid: self.counters.invalid.load(Ordering::Relaxed),
        }
    }
}

#[async_trait]
impl Plugin for Observer {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    async fn call(
        &self,
        method: &str,
        _arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, PluginError> {
        if cancel.is_cancelled() {
            return Err(PluginError::Cancelled);
        }
        if method != "observer.stats" {
            return Err(PluginError::Denied(method.into()));
        }
        serde_json::to_value(self.stats()).map_err(|error| PluginError::Failed(error.to_string()))
    }
    fn shutdown(&self) {
        self.stopped.cancel();
    }
}

fn sensitive_key(key: &str, config: &ObserverConfig) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    [
        "password",
        "passwd",
        "credential",
        "secret",
        "apikey",
        "authorization",
        "accesstoken",
        "refreshtoken",
        "privatekey",
        "cookie",
    ]
    .iter()
    .any(|sensitive| normalized.contains(sensitive))
        || (!config.allow_thinking
            && ["thinking", "reasoning", "chainofthought"]
                .iter()
                .any(|sensitive| normalized.contains(sensitive)))
        || config
            .redact_keys
            .iter()
            .any(|redacted| redacted.eq_ignore_ascii_case(key))
}

fn redact(value: Value, config: &ObserverConfig, depth: usize) -> Value {
    if depth >= 32 {
        return Value::String("[redacted: depth]".into());
    }
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| {
                    let value = if sensitive_key(&key, config) {
                        Value::String("[redacted]".into())
                    } else {
                        redact(value, config, depth + 1)
                    };
                    (key, value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact(value, config, depth + 1))
                .collect(),
        ),
        other => other,
    }
}
