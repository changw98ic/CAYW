//! Explicit offline fixture decisions and durable audit records for the real Host.
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use cayw_host::{AuditSink, ExternalHost, PermissionGate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::{Mutex, MutexGuard},
};
use tokio::sync::Notify;

pub const SOURCE: &str = "offline synthetic";
pub const BLOCK_QUERY: &str = "cayw-harness.blocking-fixture";

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRecord {
    pub source: String,
    pub capability: String,
    pub arguments: Value,
    pub decision: String,
}

pub struct FixtureGate {
    pub grants: Vec<(String, Value)>,
    pub decisions: Mutex<Vec<PermissionRecord>>,
}

#[async_trait]
impl PermissionGate for FixtureGate {
    async fn authorize(
        &self,
        capability: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        if cancel.is_cancelled() {
            return Err(CapabilityError::new("fixture permission check cancelled"));
        }
        let granted = self
            .grants
            .iter()
            .any(|(name, args)| name == capability && args == arguments);
        lock(&self.decisions).push(PermissionRecord {
            source: SOURCE.into(),
            capability: capability.into(),
            arguments: arguments.clone(),
            decision: if granted {
                "fixture_grant"
            } else {
                "fixture_denial"
            }
            .into(),
        });
        if granted {
            Ok(())
        } else {
            Err(CapabilityError::new(
                "explicit offline fixture gate denied this exact call",
            ))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    pub source: String,
    pub event: String,
    pub call_id: String,
    pub capability: Option<String>,
    pub arguments: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub outcome_unknown: bool,
}

struct AuditState {
    file: File,
    next_id: u64,
    events: Vec<AuditEvent>,
}

pub struct FixtureAudit(Mutex<AuditState>);
impl FixtureAudit {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.sync_all()?;
        #[cfg(unix)]
        File::open(path.parent().expect("audit path has a parent"))?.sync_all()?;
        Ok(Self(Mutex::new(AuditState {
            file,
            next_id: 0,
            events: vec![],
        })))
    }
    fn append(state: &mut AuditState, event: AuditEvent) -> Result<(), CapabilityError> {
        // No await between writing a terminal record and confirming its durability.
        let mut bytes = serde_json::to_vec(&event)
            .map_err(|error| CapabilityError::unknown(error.to_string()))?;
        bytes.push(b'\n');
        state
            .file
            .write_all(&bytes)
            .and_then(|()| state.file.sync_all())
            .map_err(|error| CapabilityError::unknown(format!("fixture audit: {error}")))?;
        state.events.push(event);
        Ok(())
    }
    pub fn events(&self) -> Vec<AuditEvent> {
        lock(&self.0).events.clone()
    }
}

#[async_trait]
impl AuditSink for FixtureAudit {
    async fn started(
        &self,
        capability: &str,
        arguments: &Value,
    ) -> Result<String, CapabilityError> {
        let mut state = lock(&self.0);
        state.next_id += 1;
        let id = format!("call-{:04}", state.next_id);
        Self::append(
            &mut state,
            AuditEvent {
                source: SOURCE.into(),
                event: "started".into(),
                call_id: id.clone(),
                capability: Some(capability.into()),
                arguments: Some(arguments.clone()),
                result: None,
                error: None,
                outcome_unknown: false,
            },
        )?;
        Ok(id)
    }
    async fn settled(
        &self,
        call_id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        let (value, error, unknown) = match result {
            Ok(value) => (Some(value.clone()), None, false),
            Err(error) => (None, Some(error.message.clone()), error.outcome_unknown),
        };
        Self::append(
            &mut lock(&self.0),
            AuditEvent {
                source: SOURCE.into(),
                event: "settled".into(),
                call_id: call_id.into(),
                capability: None,
                arguments: None,
                result: value,
                error,
                outcome_unknown: unknown,
            },
        )
    }
    fn dropped(&self, call_id: &str) -> Result<(), CapabilityError> {
        Self::append(
            &mut lock(&self.0),
            AuditEvent {
                source: SOURCE.into(),
                event: "dropped".into(),
                call_id: call_id.into(),
                capability: None,
                arguments: None,
                result: None,
                error: Some("Host future dropped before a settled record".into()),
                outcome_unknown: true,
            },
        )
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BlockingObservation {
    pub source: String,
    pub entered: bool,
    pub active: bool,
    pub stopped: bool,
    pub cancellation_observed_on_stop: bool,
    pub cancellation_requested_after_entry: bool,
    pub settled_before_execute_return: bool,
    pub cancel_to_execute_return_ns: Option<u64>,
}

#[derive(Default)]
pub struct BlockingFixture {
    pub entered: Notify,
    pub state: Mutex<BlockingObservation>,
}
struct Pending<'a> {
    fixture: &'a BlockingFixture,
    cancel: CancellationToken,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.fixture.state);
        state.active = false;
        state.stopped = true;
        state.cancellation_observed_on_stop = self.cancel.is_cancelled();
    }
}

#[async_trait]
impl ExternalHost for BlockingFixture {
    async fn call(
        &self,
        capability: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if capability != "search.query" || arguments != json!({"query": BLOCK_QUERY}) {
            return Err(CapabilityError::new(
                "offline fixture has no such external operation",
            ));
        }
        {
            let mut state = lock(&self.state);
            state.source = SOURCE.into();
            state.entered = true;
            state.active = true;
        }
        let _pending = Pending {
            fixture: self,
            cancel: cancel.clone(),
        };
        self.entered.notify_one();
        // This is a genuinely pending async capability, with no network or detached task.
        cancel.cancelled().await;
        Err(CapabilityError::new(
            "offline fixture stopped after cancellation; no effects",
        ))
    }
}
