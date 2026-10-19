//! Downstream Issue #1 examples. Only public CAYW interfaces are used.
use async_trait::async_trait;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_codemode::CapabilityError;
use cayw_core::{CoreEvent, EventRecord};
use cayw_host::ExternalHost;
use cayw_runtime::{Runtime, RuntimeConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const WATCHDOG: Duration = Duration::from_secs(20);
pub const TOKEN: &str = "issue1-public-offline-fixture-token";
pub const FILE_SCRIPT: &str = "cayw.call('fs.write',{path='result.txt',content='Issue 1 local effect'}); return cayw.call('fs.read',{path='result.txt'})";
pub const UNKNOWN_SCRIPT: &str = "return cayw.call('search.query',{query='synthetic-effect'})";

pub fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(std::io::Error::other(message).into())
    }
}

/// This provider selects a fixed script. It never performs the file operation.
pub struct FixedProvider {
    script: &'static str,
    pub requests: AtomicUsize,
    pub first_started: Semaphore,
    pub release_first: Arc<Semaphore>,
    hold_first: bool,
}
impl FixedProvider {
    pub fn new(script: &'static str, hold_first: bool) -> Self {
        Self {
            script,
            requests: AtomicUsize::new(0),
            first_started: Semaphore::new(0),
            release_first: Arc::new(Semaphore::new(0)),
            hold_first,
        }
    }
}
impl Provider for FixedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let ordinal = self.requests.fetch_add(1, Ordering::SeqCst);
        // History survives a process restart; a process-local counter alone does
        // not. Include its current length so a replacement fixture never reuses
        // an identifier already retained in this logical Session.
        let call_id = format!("issue1-{}-{ordinal}", request.messages.len());
        let message = cayw_runtime::fixture::script_response(&request, self.script, &call_id);
        if self.hold_first && ordinal == 0 {
            self.first_started.add_permits(1);
            let release = self.release_first.clone();
            return Box::pin(futures::stream::once(async move {
                tokio::select! {
                    _ = cancel.cancelled() => AssistantEvent::Error {
                        message: message.failure(cayw_ai::StopReason::Aborted, "fixture cancelled before release"),
                    },
                    permit = release.acquire() => {
                        permit.expect("fixture release stays open").forget();
                        AssistantEvent::Done { message }
                    }
                }
            }));
        }
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

pub fn open(root: &Path, provider: Arc<dyn Provider>, config: RuntimeConfig) -> Result<Runtime> {
    std::fs::create_dir_all(root.join("workspace"))?;
    Ok(Runtime::open(
        root.join("session.sqlite"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "issue1-offline"),
        config,
    )?)
}

/// All evidence uses create_new; a rerun must name a fresh directory.
pub fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    File::open(path.parent().ok_or("evidence has no parent")?)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SavedCursor {
    pub session_id: String,
    pub after: u64,
}
impl SavedCursor {
    /// Save only a contiguous page already processed by this consumer.
    pub fn advance(&self, page: &[EventRecord]) -> Result<Self> {
        let mut after = self.after;
        for event in page {
            require(
                event.protocol_version == cayw_core::PROTOCOL_VERSION,
                "wrong event contract",
            )?;
            require(
                event.session_id == self.session_id,
                "event belongs to a different session",
            )?;
            after = after.checked_add(1).ok_or("cursor overflow")?;
            require(event.cursor == after, "event cursor gap or duplicate")?;
        }
        Ok(Self {
            session_id: self.session_id.clone(),
            after,
        })
    }
}

pub fn check_origin(events: &[EventRecord]) -> Result<Value> {
    let calls = events
        .iter()
        .filter_map(|event| {
            if let CoreEvent::CapabilitySettled { call } = &event.event {
                Some((event, call))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    require(!calls.is_empty(), "no real Host capability terminal")?;
    for (event, call) in &calls {
        require(
            event.attempt.as_ref() == Some(&call.lease),
            "event lost original attempt/worker",
        )?;
        let execution = events
            .iter()
            .find_map(|event| match &event.event {
                CoreEvent::ExecutionStarted { execution }
                    if execution.execution_id == call.key.execution_id =>
                {
                    Some(execution)
                }
                _ => None,
            })
            .ok_or("capability lacks execution origin")?;
        require(
            execution.lease == call.lease
                && execution.task_id == call.task_id
                && execution.parent_call_id == call.parent_call_id
                && execution.source_hash == call.source_hash,
            "capability lost parent, task, worker or source",
        )?;
        require(
            !call.lease.worker_id.is_empty() && !call.parent_call_id.is_empty(),
            "empty original identity",
        )?;
    }
    let first = calls[0].1;
    Ok(
        json!({"task_id":first.task_id,"lease":first.lease,"execution_id":first.key.execution_id,
        "parent_call_id":first.parent_call_id,"source_hash":first.source_hash,
        "sequences":calls.iter().map(|(_, c)| c.key.sequence).collect::<Vec<_>>() }),
    )
}

/// A deliberately incomplete synthetic service effect. Host dispatch, permissions,
/// Core start and Cells intent are real. No external network is used.
pub struct InterruptedService {
    pub root: std::path::PathBuf,
    pub calls: AtomicUsize,
}
#[async_trait]
impl ExternalHost for InterruptedService {
    async fn call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        let ordinal = self.calls.fetch_add(1, Ordering::SeqCst);
        save(
            &self.root.join(format!("synthetic-effect-{ordinal}.json")),
            &json!({
                "capability":name,"arguments":args,"ordinal":ordinal,
                "synthetic":true,"pid":std::process::id()
            }),
        )
        .map_err(|e| CapabilityError::unknown(e.to_string()))?;
        println!("{}", json!({"marker":"effect-synced","ordinal":ordinal}));
        std::io::stdout()
            .flush()
            .map_err(|e| CapabilityError::unknown(e.to_string()))?;
        cancel.cancelled().await;
        Err(CapabilityError::unknown(
            "synthetic service cancelled after synced effect",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_agent::{AgentEvent, ToolResult};
    use cayw_ai::{AssistantMessage, Content, Message, StopReason, ToolCall};
    use cayw_core::{CoreError, SessionStore};

    fn plan(store: &SessionStore, lease: &cayw_core::AttemptLease, id: &str) -> ToolCall {
        store
            .append_agent_event(lease, &AgentEvent::AgentStart)
            .unwrap();
        store
            .append_agent_event(lease, &AgentEvent::TurnStart)
            .unwrap();
        let call = ToolCall::new(id, "write_file", json!({"path":"original.txt"}));
        let mut message = AssistantMessage::new(&Model::new("fixture", "pairing"));
        message.stop_reason = StopReason::ToolUse;
        message.content = vec![Content::ToolCall(call.clone())];
        let message = Message::Assistant(message);
        store
            .append_agent_event(
                lease,
                &AgentEvent::MessageStart {
                    message: message.clone(),
                },
            )
            .unwrap();
        store
            .append_agent_event(lease, &AgentEvent::MessageEnd { message })
            .unwrap();
        store
            .append_agent_event(
                lease,
                &AgentEvent::ToolExecutionStart { call: call.clone() },
            )
            .unwrap();
        store.mark_tool_started(lease, &call.id).unwrap();
        call
    }
    fn terminal(call: ToolCall) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            call,
            result: ToolResult::text("synthetic known result"),
            duration_ms: None,
        }
    }
    #[test]
    fn public_core_rejects_mismatched_duplicate_and_cross_attempt_results_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path().join("pairing.sqlite")).unwrap();
        let session = store.create_session("pairing").unwrap();
        let old = store.begin_attempt(&session.id, "first-worker").unwrap();
        let call = plan(&store, &old, "old-call");
        let before = store.snapshot(&session.id).unwrap();
        for forged in [
            ToolCall::new("missing", "write_file", call.arguments.clone()),
            ToolCall::new(&call.id, "read_file", call.arguments.clone()),
            ToolCall::new(&call.id, &call.name, json!({"path":"wrong.txt"})),
        ] {
            assert!(store.append_agent_event(&old, &terminal(forged)).is_err());
            assert_eq!(store.snapshot(&session.id).unwrap(), before);
            assert!(
                store
                    .events(&session.id, before.cursor, 100)
                    .unwrap()
                    .is_empty()
            );
        }
        store
            .append_agent_event(&old, &terminal(call.clone()))
            .unwrap();
        let completed = store.snapshot(&session.id).unwrap();
        assert!(
            store
                .append_agent_event(&old, &terminal(call.clone()))
                .is_err()
        );
        assert_eq!(store.snapshot(&session.id).unwrap(), completed);
        store.recover(&session.id).unwrap();
        let current = store
            .begin_attempt(&session.id, "replacement-worker")
            .unwrap();
        let current_call = plan(&store, &current, "current-call");
        let before = store.snapshot(&session.id).unwrap();
        assert!(matches!(
            store.append_agent_event(&old, &terminal(current_call.clone())),
            Err(CoreError::StaleLease)
        ));
        assert!(store.append_agent_event(&current, &terminal(call)).is_err());
        assert_eq!(store.snapshot(&session.id).unwrap(), before);
        assert!(
            store
                .events(&session.id, before.cursor, 100)
                .unwrap()
                .is_empty()
        );
        store
            .append_agent_event(&current, &terminal(current_call))
            .unwrap();
        assert_ne!(old.attempt_id, current.attempt_id);
        assert_ne!(old.worker_id, current.worker_id);
        assert!(current.fence > old.fence);
    }
    #[test]
    fn saved_cursor_rejects_skip_duplicate_and_wrong_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path().join("cursor.sqlite")).unwrap();
        let session = store.create_session("cursor").unwrap();
        let events = store.events(&session.id, 0, 100).unwrap();
        let initial = SavedCursor {
            session_id: session.id,
            after: 0,
        };
        let saved = initial.advance(&events).unwrap();
        assert_eq!(saved.after, 1);
        assert!(saved.advance(&events).is_err());
        let mut skipped = events.clone();
        skipped[0].cursor += 1;
        assert!(initial.advance(&skipped).is_err());
        let mut foreign = events;
        foreign[0].session_id = "different-session".into();
        assert!(initial.advance(&foreign).is_err());
    }
}
