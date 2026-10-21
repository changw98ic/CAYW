//! Lossy debug side channel. A slow or closed reader never blocks the caller.
//! Records carry the time, the conversation, and a failure reason when there is
//! one. Sensitive values are removed by key name here; callers do not classify
//! fields. A debug record never grants permission or feeds an executor.

use crate::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DebugRecord {
    pub kind: String,
    /// Milliseconds since the Unix epoch, taken when the record is built.
    pub at_ms: u64,
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct DebugConfig {
    pub capacity: usize,
    pub max_record_bytes: usize,
    /// Additional case-insensitive exact field names, at any depth.
    pub redact_keys: BTreeSet<String>,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            capacity: 128,
            max_record_bytes: 16 * 1024,
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
pub struct DebugStats {
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
pub struct DebugLog {
    manifest: PluginManifest,
    sender: mpsc::Sender<DebugRecord>,
    config: Arc<DebugConfig>,
    counters: Arc<Counters>,
    stopped: CancellationToken,
}

pub struct DebugReader {
    receiver: mpsc::Receiver<DebugRecord>,
    stopped: CancellationToken,
}

impl DebugReader {
    pub async fn recv(&mut self) -> Option<DebugRecord> {
        tokio::select! {
            biased;
            _ = self.stopped.cancelled() => {
                self.receiver.close();
                self.receiver.recv().await
            },
            event = self.receiver.recv() => event,
        }
    }
    pub fn try_recv(&mut self) -> Result<DebugRecord, mpsc::error::TryRecvError> {
        if self.stopped.is_cancelled() {
            self.receiver.close();
        }
        self.receiver.try_recv()
    }
    /// Closes new writes; already queued records can still be drained.
    pub fn close(&mut self) {
        self.receiver.close();
    }
}

impl DebugLog {
    pub fn channel(config: DebugConfig) -> Result<(Self, DebugReader), PluginError> {
        if config.capacity == 0 || config.capacity > 1_000_000 || config.max_record_bytes == 0 {
            return Err(PluginError::InvalidArguments(
                "invalid debug capacity or record limit".into(),
            ));
        }
        let (sender, receiver) = mpsc::channel(config.capacity);
        let manifest = PluginManifest {
            name: "debug".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kind: PluginKind::Native,
            protocol_version: PLUGIN_PROTOCOL_VERSION,
            capabilities: vec![ToolDefinition {
                name: "debug.stats".into(),
                description: "Read debug delivery counters".into(),
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
            DebugReader { receiver, stopped },
        ))
    }

    pub fn publish(
        &self,
        kind: impl Into<String>,
        fields: BTreeMap<String, Value>,
    ) -> Result<DebugRecord, PublishOutcome> {
        if self.stopped.is_cancelled() || self.sender.is_closed() {
            self.counters.closed.fetch_add(1, Ordering::Relaxed);
            return Err(PublishOutcome::DroppedClosed);
        }
        let kind = kind.into();
        if !valid_name(&kind) || fields.keys().any(|key| !valid_name(key)) {
            self.counters.invalid.fetch_add(1, Ordering::Relaxed);
            return Err(PublishOutcome::DroppedInvalid);
        }
        let record = DebugRecord {
            kind,
            at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|time| time.as_millis() as u64)
                .unwrap_or(0),
            fields: fields
                .into_iter()
                .map(|(key, value)| {
                    let value = if sensitive_key(&key, &self.config) {
                        Value::String("[redacted]".into())
                    } else {
                        redact(value, &self.config, 0)
                    };
                    (key, value)
                })
                .collect(),
        };
        if serde_json::to_vec(&record).map_or(true, |bytes| bytes.len() > self.config.max_record_bytes)
        {
            self.counters.oversize.fetch_add(1, Ordering::Relaxed);
            return Err(PublishOutcome::DroppedOversize);
        }
        match self.sender.try_send(record.clone()) {
            Ok(()) => {
                self.counters.queued.fetch_add(1, Ordering::Relaxed);
                Ok(record)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.counters.full.fetch_add(1, Ordering::Relaxed);
                Err(PublishOutcome::DroppedFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.counters.closed.fetch_add(1, Ordering::Relaxed);
                Err(PublishOutcome::DroppedClosed)
            }
        }
    }

    pub fn stats(&self) -> DebugStats {
        DebugStats {
            queued: self.counters.queued.load(Ordering::Relaxed),
            dropped_full: self.counters.full.load(Ordering::Relaxed),
            dropped_closed: self.counters.closed.load(Ordering::Relaxed),
            dropped_oversize: self.counters.oversize.load(Ordering::Relaxed),
            dropped_invalid: self.counters.invalid.load(Ordering::Relaxed),
        }
    }
}

#[async_trait]
impl Plugin for DebugLog {
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
        if method != "debug.stats" {
            return Err(PluginError::Denied(method.into()));
        }
        serde_json::to_value(self.stats()).map_err(|error| PluginError::Failed(error.to_string()))
    }
    fn shutdown(&self) {
        self.stopped.cancel();
    }
}

/// Follows a cursor the host advances by reading a runtime. Enabling it copies
/// events into the debug log. Disabling it stops copying and does not touch the
/// source. This type never reads or writes a runtime itself.
#[derive(Clone)]
pub struct DebugFollow {
    debug: DebugLog,
    enabled: Arc<std::sync::atomic::AtomicBool>,
}

pub struct Followed {
    /// None when debug is off or the record was dropped. The source is unchanged.
    pub record: Option<DebugRecord>,
    /// Cursor of the source event. The host reads the runtime after the last one.
    pub cursor: u64,
}

impl DebugFollow {
    pub fn new(debug: DebugLog) -> Self {
        Self {
            debug,
            enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
    }

    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Copy events the host already read. The result has one entry per source
    /// event, in source order, whether or not it was stored. A missing record
    /// means debug is off or the log dropped it. Neither changes the source.
    pub fn sync(&self, events: &[SourceEvent]) -> Vec<Followed> {
        events
            .iter()
            .map(|event| {
                let record = if self.enabled() {
                    let mut fields = event.fields.clone();
                    fields.insert("session".into(), Value::String(event.session_id.clone()));
                    fields.insert("event_at_ms".into(), Value::from(event.at_ms));
                    fields.insert("cursor".into(), Value::from(event.cursor));
                    self.debug.publish(&event.kind, fields).ok()
                } else {
                    None
                };
                Followed {
                    record,
                    cursor: event.cursor,
                }
            })
            .collect()
    }
}

/// One event a host read from a runtime, unchanged except for later redaction.
#[derive(Debug, Clone)]
pub struct SourceEvent {
    pub cursor: u64,
    pub at_ms: u64,
    pub session_id: String,
    pub kind: String,
    pub fields: BTreeMap<String, Value>,
}

fn sensitive_key(key: &str, config: &DebugConfig) -> bool {
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
        "thinking",
        "reasoning",
        "chainofthought",
    ]
    .iter()
    .any(|sensitive| normalized.contains(sensitive))
        || config
            .redact_keys
            .iter()
            .any(|redacted| redacted.eq_ignore_ascii_case(key))
}

fn redact(value: Value, config: &DebugConfig, depth: usize) -> Value {
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
