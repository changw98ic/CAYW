//! Durable audit for every Host call made by the visual editor.
//!
//! The audit file is append-only. A `started` record without a matching
//! `settled` or `dropped` record, an `outcome_unknown` settlement, or a
//! corrupt/truncated line is never cleared automatically. Opening a state
//! directory in that condition is refused until a human reconciles it.

use async_trait::async_trait;
use cayw_codemode::CapabilityError;
use cayw_host::AuditSink;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Append-only audit records. One JSON object per line.
pub const AUDIT_FILE: &str = "calls.jsonl";
/// Durable marker written when any outcome is unknown.
pub const UNKNOWN_MARKER: &str = "unknown.json";

const MAX_AUDIT_BYTES: u64 = 64 * 1024 * 1024;
const REDACT_ABOVE: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditRecord {
    event: String,
    call_id: String,
    #[serde(default)]
    capability: String,
    #[serde(default)]
    arguments: Value,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    outcome_unknown: bool,
    #[serde(default)]
    at_ms: u64,
}

/// Error opening or scanning a state directory.
#[derive(Debug, Clone)]
pub struct AuditOpenError {
    pub code: &'static str,
    pub message: String,
    pub outcome_unknown: bool,
}

impl AuditOpenError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            outcome_unknown: false,
        }
    }

    fn reconciliation(message: impl Into<String>) -> Self {
        Self {
            code: "reconciliation_required",
            message: message.into(),
            outcome_unknown: true,
        }
    }
}

impl AuditOpenError {
    /// Stable error code used by the source engine and server mapping.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// True only when durable reconciliation is required.
    pub fn outcome_unknown(&self) -> bool {
        self.outcome_unknown
    }
}

impl std::fmt::Display for AuditOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for AuditOpenError {}

/// Name expected by the source engine and crate root re-export.
pub type DurableAudit = AuditStore;

/// Name expected by the source engine's error mapping.
pub type AuditError = AuditOpenError;

/// Durable, single-writer audit owned by one editor process.
pub struct AuditStore {
    path: PathBuf,
    marker: PathBuf,
    file: Mutex<File>,
    unknown: AtomicBool,
    /// Random per-process prefix. Call ids must stay unique across restarts
    /// even when the wall clock jumps backwards; a millisecond timestamp alone
    /// cannot provide that guarantee.
    session: u64,
    counter: AtomicU64,
}

impl AuditStore {
    /// Opens the state directory, takes its writer lock, and refuses to start
    /// when any previous call was not durably settled or is marked unknown.
    pub fn open(state_dir: &Path) -> Result<Self, AuditOpenError> {
        fs::create_dir_all(state_dir).map_err(|error| {
            AuditOpenError::new(
                "invalid_state_dir",
                format!("cannot create state directory: {error}"),
            )
        })?;
        let state_dir = state_dir.canonicalize().map_err(|error| {
            AuditOpenError::new(
                "invalid_state_dir",
                format!("cannot resolve state directory: {error}"),
            )
        })?;
        let path = state_dir.join(AUDIT_FILE);
        let marker = state_dir.join(UNKNOWN_MARKER);

        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|error| {
            AuditOpenError::new(
                "invalid_state_dir",
                format!("cannot open audit file: {error}"),
            )
        })?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(AuditOpenError::new(
                    "state_dir_busy",
                    "another visual editor process already owns this state directory",
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(AuditOpenError::new(
                    "invalid_state_dir",
                    format!("cannot lock audit file: {error}"),
                ));
            }
        }
        sync_directory(&state_dir).map_err(|error| {
            AuditOpenError::new(
                "invalid_state_dir",
                format!("cannot synchronize state directory: {error}"),
            )
        })?;

        if marker.exists() {
            return Err(AuditOpenError::reconciliation(
                "state directory contains an unresolved unknown operation; reconcile it before editing",
            ));
        }
        match scan(&path, &state_dir) {
            Ok(None) => {}
            Ok(Some(reason)) => {
                let _ = write_marker(&marker, &reason);
                return Err(AuditOpenError::reconciliation(reason));
            }
            Err(error) => {
                let reason = format!("cannot read audit records: {error}");
                let _ = write_marker(&marker, &reason);
                return Err(AuditOpenError::reconciliation(reason));
            }
        }
        Ok(Self {
            path,
            marker,
            file: Mutex::new(file),
            unknown: AtomicBool::new(false),
            session: random_session_id(),
            counter: AtomicU64::new(1),
        })
    }

    /// Path of the append-only audit file.
    pub fn audit_path(&self) -> &Path {
        &self.path
    }

    /// Path of the durable unknown marker.
    pub fn marker_path(&self) -> &Path {
        &self.marker
    }

    /// True once this process has observed or recorded an unknown outcome.
    pub fn unknown(&self) -> bool {
        self.unknown.load(Ordering::Acquire) || self.marker.exists()
    }

    /// Records an outcome that the application could not confirm.
    pub fn record_unknown(&self, reason: &str) -> std::io::Result<()> {
        self.mark_unknown(reason)
    }

    fn next_id(&self) -> String {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{:016x}-{:016x}", self.session, counter)
    }

    fn append(&self, record: &AuditRecord) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.write_all(&line)?;
        file.sync_data()?;
        Ok(())
    }

    fn mark_unknown(&self, reason: &str) -> std::io::Result<()> {
        self.unknown.store(true, Ordering::Release);
        write_marker(&self.marker, reason)
    }
}

#[async_trait]
impl AuditSink for AuditStore {
    async fn started(
        &self,
        capability: &str,
        arguments: &Value,
    ) -> Result<String, CapabilityError> {
        let call_id = self.next_id();
        let record = AuditRecord {
            event: "started".into(),
            call_id: call_id.clone(),
            capability: capability.to_string(),
            arguments: redact(arguments),
            result: None,
            outcome_unknown: false,
            at_ms: now_ms(),
        };
        self.append(&record).map_err(|error| {
            CapabilityError::new(format!("could not durably audit call start: {error}"))
        })?;
        Ok(call_id)
    }

    async fn settled(
        &self,
        call_id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        let (summary, unknown) = match result {
            Ok(value) => (redact(value), false),
            Err(error) => (
                json!({
                    "error": error.message,
                    "outcome_unknown": error.outcome_unknown,
                }),
                error.outcome_unknown,
            ),
        };
        let record = AuditRecord {
            event: "settled".into(),
            call_id: call_id.to_string(),
            capability: String::new(),
            arguments: Value::Null,
            result: Some(summary),
            outcome_unknown: unknown,
            at_ms: now_ms(),
        };
        self.append(&record).map_err(|error| {
            CapabilityError::unknown(format!(
                "could not durably settle the audit record: {error}"
            ))
        })?;
        if unknown {
            self.mark_unknown("a Host capability returned an unknown outcome")
                .map_err(|error| {
                    CapabilityError::unknown(format!(
                        "could not durably record the unknown outcome: {error}"
                    ))
                })?;
        }
        Ok(())
    }

    fn dropped(&self, call_id: &str) -> Result<(), CapabilityError> {
        let record = AuditRecord {
            event: "dropped".into(),
            call_id: call_id.to_string(),
            capability: String::new(),
            arguments: Value::Null,
            result: None,
            outcome_unknown: true,
            at_ms: now_ms(),
        };
        self.append(&record).map_err(|error| {
            CapabilityError::unknown(format!("could not durably record a dropped call: {error}"))
        })?;
        self.mark_unknown("a Host call was dropped before its outcome was confirmed")
            .map_err(|error| {
                CapabilityError::unknown(format!(
                    "could not durably record the unknown outcome: {error}"
                ))
            })
    }
}

/// Scans the audit file. `Ok(Some(reason))` means the state cannot be used.
fn scan(path: &Path, state_dir: &Path) -> std::io::Result<Option<String>> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_AUDIT_BYTES {
        return Ok(Some(
            "audit file exceeds the bounded size; reconcile it before editing".into(),
        ));
    }
    let reader = BufReader::new(File::open(path)?);
    let mut terminal: BTreeMap<String, bool> = BTreeMap::new();
    for (line_number, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: AuditRecord = match serde_json::from_str(&line) {
            Ok(record) => record,
            Err(_) => {
                return Ok(Some(format!(
                    "audit line {} is corrupt; reconcile the state directory",
                    line_number + 1
                )));
            }
        };
        if record.call_id.trim().is_empty() {
            return Ok(Some(format!(
                "audit line {} has no call id; reconcile the state directory",
                line_number + 1
            )));
        }
        match record.event.as_str() {
            "started" => {
                if terminal.insert(record.call_id.clone(), false).is_some() {
                    return Ok(Some(format!(
                        "audit line {} repeats a call id; reconcile the state directory",
                        line_number + 1
                    )));
                }
            }
            "settled" | "dropped" => {
                let Some(settled) = terminal.get_mut(&record.call_id) else {
                    return Ok(Some(format!(
                        "audit line {} settles a call that never started; reconcile the state directory",
                        line_number + 1
                    )));
                };
                if *settled {
                    return Ok(Some(format!(
                        "audit line {} settles a call twice; reconcile the state directory",
                        line_number + 1
                    )));
                }
                *settled = true;
                if record.event == "dropped" || record.outcome_unknown {
                    return Ok(Some(format!(
                        "audit call {} has an unknown outcome; reconcile the state directory",
                        record.call_id
                    )));
                }
            }
            other => {
                return Ok(Some(format!(
                    "audit line {} has an unknown event `{other}`; reconcile the state directory",
                    line_number + 1
                )));
            }
        }
    }
    if let Some(call_id) = terminal
        .iter()
        .find_map(|(call_id, settled)| (!*settled).then_some(call_id))
    {
        return Ok(Some(format!(
            "audit call {call_id} was started but never settled; reconcile the state directory"
        )));
    }
    // Rewrite the marker only when it is absent; the marker is the durable
    // cross-process fence. The caller handles a missing directory separately.
    let _ = state_dir;
    Ok(None)
}

fn write_marker(path: &Path, reason: &str) -> std::io::Result<()> {
    let body = serde_json::to_vec(&json!({
        "code": "reconciliation_required",
        "message": reason,
        "at_ms": now_ms(),
    }))
    .map_err(std::io::Error::other)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&body)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A process-local random prefix sourced from the standard library's randomly
/// seeded hasher. It does not need a new dependency and does not read the clock.
fn random_session_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Audit arguments and results without persisting page source or user text.
/// Exact paths, hashes and sizes remain available for reconciliation.
fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                if matches!(key.as_str(), "content" | "html" | "text" | "draft")
                    && let Value::String(text) = item
                {
                    out.insert(key.clone(), redacted_string(text));
                } else {
                    out.insert(key.clone(), redact(item));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::String(text) if text.len() > REDACT_ABOVE => redacted_string(text),
        other => other.clone(),
    }
}

fn redacted_string(text: &str) -> Value {
    json!({
        "redacted": true,
        "bytes": text.len(),
        "sha256": format!("{:x}", Sha256::digest(text.as_bytes())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_hides_content_but_keeps_path_and_hash() {
        let value = json!({
            "path": "index.html",
            "content": "<p>secret</p>",
            "overwrite": true,
        });
        let redacted = redact(&value);
        assert_eq!(redacted["path"], "index.html");
        assert_eq!(redacted["content"]["redacted"], true);
        assert!(redacted["content"].get("sha256").is_some());
        assert!(redacted.to_string().find("secret").is_none());
    }

    #[test]
    fn call_ids_stay_unique_without_the_wall_clock() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::open(dir.path()).unwrap();
        let mut ids = std::collections::BTreeSet::new();
        for _ in 0..1024 {
            let id = store.next_id();
            assert_eq!(id.len(), 33, "{id}");
            assert_eq!(id.matches('-').count(), 1, "{id}");
            assert!(ids.insert(id), "call ids must not repeat");
        }
    }

    #[test]
    fn scan_refuses_started_without_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUDIT_FILE);
        fs::write(
            &path,
            "{\"event\":\"started\",\"call_id\":\"a\",\"capability\":\"fs.write\",\"at_ms\":1}\n",
        )
        .unwrap();
        let reason = scan(&path, dir.path()).unwrap().unwrap();
        assert!(reason.contains("never settled"));
    }

    #[test]
    fn scan_refuses_unknown_settlement_even_when_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUDIT_FILE);
        fs::write(
            &path,
            concat!(
                "{\"event\":\"started\",\"call_id\":\"a\",\"at_ms\":1}\n",
                "{\"event\":\"settled\",\"call_id\":\"a\",\"outcome_unknown\":true,\"at_ms\":2}\n"
            ),
        )
        .unwrap();
        let reason = scan(&path, dir.path()).unwrap().unwrap();
        assert!(reason.contains("unknown outcome"));
    }

    #[test]
    fn scan_accepts_settled_known_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUDIT_FILE);
        fs::write(
            &path,
            concat!(
                "{\"event\":\"started\",\"call_id\":\"a\",\"at_ms\":1}\n",
                "{\"event\":\"settled\",\"call_id\":\"a\",\"outcome_unknown\":false,\"at_ms\":2}\n"
            ),
        )
        .unwrap();
        assert!(scan(&path, dir.path()).unwrap().is_none());
    }
}
