//! Fixed injected Provider and host effects. No HTTP adapter or credentials.
// The caller receives exact requests; all generated tool code uses public Runtime paths.
use crate::measurement::sha256;
use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_codemode::CapabilityError;
use cayw_host::ExternalHost;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub const PROMPT: &str = "Run the fixed offline SDK/IPC acceptance input.";
pub const CONTENT: &str = "offline SDK/IPC fixture\n第二行\n";
pub const PIPELINE: &str = "cayw.call('fs.write', {path='result.txt', content='offline SDK/IPC fixture\\n第二行\\n'}); local read = cayw.call('fs.read', {path='result.txt'}); cayw.emit(read); return read";
pub const EFFECT_SCRIPT: &str =
    "return cayw.call('search.query', {query='sdk-ipc-one-local-effect'})";
pub const FINAL: &str = "offline fixed provider finished";

pub fn warm_pipeline() -> String {
    PIPELINE.replacen(
        "{path='result.txt',",
        &format!(
            "{{path='result.txt', overwrite=true, expected_sha256='{}',",
            sha256(CONTENT.as_bytes())
        ),
        1,
    )
}
pub fn write_arguments(warm: bool) -> Value {
    let mut args = json!({"path":"result.txt","content":CONTENT});
    if warm {
        args["expected_sha256"] = json!(sha256(CONTENT.as_bytes()));
        args["overwrite"] = json!(true);
    }
    args
}

pub fn model() -> Model {
    Model::new("cayw-verify-tool-offline-synthetic", "fixed-script-v1")
}
pub fn external_arguments() -> Value {
    json!({"query":"sdk-ipc-one-local-effect"})
}
pub fn effect_result() -> Value {
    json!({"synthetic":true,"effect":"one durable local marker","sha256":sha256(CONTENT.as_bytes())})
}

pub fn write_new(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(path.parent().expect("fixture file has parent"))?.sync_all()?;
    Ok(())
}

pub struct FixedProvider {
    pub script: &'static str,
    pub gate: bool,
    pub release: CancellationToken,
    pub entered: CancellationToken,
    calls: Arc<AtomicU64>,
    journal: Option<PathBuf>,
    journal_lock: Arc<Mutex<()>>,
}
impl FixedProvider {
    pub fn new(script: &'static str, gate: bool, journal: Option<PathBuf>) -> Self {
        Self {
            script,
            gate,
            release: CancellationToken::new(),
            entered: CancellationToken::new(),
            calls: Arc::new(AtomicU64::new(0)),
            journal,
            journal_lock: Arc::new(Mutex::new(())),
        }
    }
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Acquire)
    }
}
impl Provider for FixedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let index = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        let gated = self.gate && index == 1;
        // The second user turn overwrites the known fixture bytes. Select solely
        // from the actual request history, without bypassing Host preconditions.
        let script = if self.script == PIPELINE
            && request
                .messages
                .iter()
                .filter(|m| matches!(m, Message::User { .. }))
                .count()
                > 1
        {
            warm_pipeline()
        } else {
            self.script.to_owned()
        };
        let release = self.release.clone();
        let entered = self.entered.clone();
        let journal = self.journal.clone();
        let journal_lock = self.journal_lock.clone();
        Box::pin(futures::stream::once(async move {
            if let Some(path) = journal {
                let record = json!({"source":"offline synthetic", "call":index,
                    "request_sha256":sha256(&serde_json::to_vec(&request).expect("request serializes")),
                    "request":request});
                let _lock = journal_lock
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let stored = (|| -> io::Result<()> {
                    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
                    serde_json::to_writer(&mut file, &record)?;
                    file.write_all(b"\n")?;
                    file.sync_all()
                })();
                if let Err(error) = stored {
                    return AssistantEvent::Error {
                        message: AssistantMessage::new(&request.model).failure(
                            StopReason::Error,
                            format!("fixture journal failed: {error}"),
                        ),
                    };
                }
            }
            entered.cancel();
            if gated {
                tokio::select! { biased;
                    () = cancel.cancelled() => return AssistantEvent::Error { message:
                        AssistantMessage::new(&request.model).failure(StopReason::Aborted, "fixture gate cancelled") },
                    () = release.cancelled() => {},
                }
            }
            if cancel.is_cancelled() {
                return AssistantEvent::Error {
                    message: AssistantMessage::new(&request.model)
                        .failure(StopReason::Aborted, "fixture provider cancelled"),
                };
            }
            let mut message = cayw_runtime::fixture::script_response(
                &request,
                &script,
                &format!("fixed-{index}"),
            );
            if message.stop_reason == StopReason::Stop {
                message.content.push(Content::text(FINAL));
            }
            AssistantEvent::Done { message }
        }))
    }
}

/// A durable, single local marker followed by an unresolved wait. The parent
/// kills the entire fixture host after observing both this file and Core's start.
pub struct EffectFixture(pub PathBuf);
#[async_trait]
impl ExternalHost for EffectFixture {
    async fn call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if name != "search.query" || args != external_arguments() {
            return Err(CapabilityError::new(
                "fixture received an unexpected operation",
            ));
        }
        write_new(&self.0, &effect_result())
            .map_err(|error| CapabilityError::unknown(error.to_string()))?;
        // Visible readiness follows the effect file and its parent-directory sync.
        let bytes =
            std::fs::read(&self.0).map_err(|error| CapabilityError::unknown(error.to_string()))?;
        write_new(
            &self.0.with_file_name("effect-ready.json"),
            &json!({"effect_sha256":sha256(&bytes),"process_id":std::process::id()}),
        )
        .map_err(|error| CapabilityError::unknown(error.to_string()))?;
        cancel.cancelled().await;
        Err(CapabilityError::unknown(
            "fixture cancelled after durable local effect",
        ))
    }
}
