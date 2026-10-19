//! Shared offline input with a real Runtime, Core database, Host and IPC adapter.
use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall,
};
use cayw_modules::{Module, ModuleContext, ModuleError, ModuleKind, ModuleManifest};
use cayw_runtime::{Runtime, RuntimeConfig};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
pub struct OfflineProvider {
    next_call: AtomicUsize,
    stream_gate: Option<CancellationToken>,
}
impl Provider for OfflineProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        if let Some(Message::ToolResult(result)) = request.messages.last() {
            let page = result.structured_content.as_ref();
            let status = page.and_then(|page| page["status"].as_str());
            if matches!(status, Some("running" | "yielded"))
                || page.is_some_and(|page| page["has_more"] == true)
            {
                let page = page.expect("status came from a structured cell page");
                let id = self.next_call.fetch_add(1, Ordering::SeqCst);
                message.content = vec![Content::ToolCall(ToolCall::new(
                    format!("fixture-wait-{id}"),
                    "execute_code",
                    json!({"action":"wait", "cell_id":page["cell_id"], "cursor":page["next_cursor"],
                        "yield_time_ms":1000, "max_output_tokens":65536}),
                ))];
                message.stop_reason = StopReason::ToolUse;
            } else {
                message.content = vec![Content::text("fixture completed")];
                message.stop_reason = StopReason::Stop;
            }
            return Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
                message,
            }]));
        }
        let id = self.next_call.fetch_add(1, Ordering::SeqCst);
        let stream_gate = self.stream_gate.clone();
        Box::pin(async_stream::stream! {
            yield AssistantEvent::Start { message: message.clone() };
            message.content = vec![Content::text("streamed fixture 你好")];
            yield AssistantEvent::TextDelta { index: 0, delta: "streamed fixture 你好".into(), partial: message.clone() };
            tokio::select! {
                _ = cancel.cancelled() => {
                    yield AssistantEvent::Error { message: message.failure(StopReason::Aborted, "fixture cancelled") };
                    return;
                }
                _ = async {
                    match stream_gate {
                        Some(gate) => gate.cancelled().await,
                        None => tokio::time::sleep(Duration::from_millis(700)).await,
                    }
                } => {}
            }
            message.content.push(Content::ToolCall(ToolCall::new(
                format!("fixture-call-{id}"), "execute_code",
                json!({"action":"exec", "script":"return cayw.call('fs.write', {path='fixture-output.txt', content='offline terminal verification'})", "yield_time_ms":1000, "max_output_tokens":65536}),
            )));
            message.stop_reason = StopReason::ToolUse;
            yield AssistantEvent::Done { message };
        })
    }
}

struct BindingModule {
    manifest: ModuleManifest,
}
#[async_trait]
impl Module for BindingModule {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }
    async fn call(&self, _: &str, _: Value, _: CancellationToken) -> Result<Value, ModuleError> {
        Err(ModuleError::Failed(
            "binding-only offline module has no capabilities".into(),
        ))
    }
}

pub struct OfflineRuntime {
    pub runtime: Runtime,
    pub directory: tempfile::TempDir,
    pub frozen_id: String,
}
impl OfflineRuntime {
    pub fn new() -> Self {
        Self::with_stream_gate(None)
    }

    pub fn with_stream_gate(stream_gate: Option<CancellationToken>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::open(
            directory.path().join("runtime.db"),
            directory.path(),
            Arc::new(OfflineProvider {
                stream_gate,
                ..OfflineProvider::default()
            }),
            Model::new("offline-tui", "fixture"),
            RuntimeConfig::default(),
        )
        .unwrap();
        let frozen = runtime.create(Some("Frozen fixture".into())).unwrap();
        let module = BindingModule {
            manifest: ModuleManifest {
                name: "fixture-binding".into(),
                version: "1.0.0".into(),
                protocol_version: 1,
                kind: ModuleKind::Native,
                capabilities: vec![],
                permissions: BTreeMap::new(),
            },
        };
        runtime
            .load_native(Arc::new(module), ModuleContext::default())
            .unwrap();
        runtime.bind_module(&frozen.id, "fixture-binding").unwrap();
        runtime.unload_module("fixture-binding").unwrap();
        Self {
            runtime,
            directory,
            frozen_id: frozen.id,
        }
    }
}
