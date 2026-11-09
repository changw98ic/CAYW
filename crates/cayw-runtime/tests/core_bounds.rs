//! Runtime Core metadata bounds: oversized Host results stay known and
//! oversized tool arguments never reach planning or execution.

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, Usage, UsageStatus,
};
use cayw_codemode::CapabilityError;
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_host::ExternalHost;
use cayw_runtime::{ContentConfig, Runtime, RuntimeConfig, ScopedPermission};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

const PAYLOAD_BYTES: usize = 70 * 1024;

struct LargeResultHost {
    calls: AtomicUsize,
}

#[async_trait]
impl ExternalHost for LargeResultHost {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        assert_eq!(name, "search.query");
        assert_eq!(arguments, json!({"query":"large"}));
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"payload":"x".repeat(PAYLOAD_BYTES)}))
    }
}

struct ScriptProvider {
    calls: AtomicUsize,
}

impl Provider for ScriptProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let follow_up = last_user(&request).as_deref() == Some("follow up");
        let message = if follow_up {
            final_message(&request)
        } else {
            cayw_runtime::fixture::script_response(
                &request,
                "local res=cayw.call('search.query',{query='large'}); return res",
                &format!("large-{}", self.calls.fetch_add(1, Ordering::SeqCst)),
            )
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

struct HugeArgumentsProvider;

impl Provider for HugeArgumentsProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = if last_user(&request).as_deref() == Some("follow up") {
            final_message(&request)
        } else {
            let mut message = AssistantMessage::new(&request.model);
            message.usage = Usage {
                input: 4,
                output: 4,
                total_tokens: 8,
                ..Default::default()
            };
            message.usage_status = UsageStatus::Reported;
            message.content.push(Content::ToolCall(ToolCall::new(
                "huge",
                "execute_code",
                json!({
                    "action":"exec",
                    "script":"x".repeat(PAYLOAD_BYTES),
                    "yield_time_ms":30_000,
                    "max_output_tokens":128*1024,
                }),
            )));
            message.stop_reason = StopReason::ToolUse;
            message
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

struct StreamingHugeProvider {
    calls: AtomicUsize,
}

impl Provider for StreamingHugeProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        if last_user(&request).as_deref() == Some("follow up") {
            return Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: final_message(&request),
            }]));
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut partial = AssistantMessage::new(&request.model);
        partial.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        partial.usage_status = UsageStatus::Reported;
        partial.content.push(Content::ToolCall(ToolCall::new(
            "huge-stream",
            "execute_code",
            json!({
                "action":"exec",
                "script":"x".repeat(PAYLOAD_BYTES),
                "yield_time_ms":30_000,
                "max_output_tokens":128*1024,
            }),
        )));
        partial.stop_reason = StopReason::ToolUse;
        // Providers accumulate tool-call arguments and republish the partial on
        // every delta. Each MessageUpdate is durable, so the raw oversized
        // partial must never be stored.
        Box::pin(futures::stream::iter([
            AssistantEvent::ToolCallStart {
                index: 0,
                partial: AssistantMessage::new(&request.model),
            },
            AssistantEvent::ToolCallDelta {
                index: 0,
                delta: "x".repeat(256),
                partial: partial.clone(),
            },
            AssistantEvent::ToolCallEnd {
                index: 0,
                partial: partial.clone(),
            },
            AssistantEvent::Done { message: partial },
        ]))
    }
}

struct FrozenReadProvider;

impl Provider for FrozenReadProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = if last_user(&request).as_deref() == Some("follow up") {
            final_message(&request)
        } else {
            cayw_runtime::fixture::script_response(
                &request,
                "local res=cayw.call('fs.read',{path='big.txt'}); return res",
                "frozen-large",
            )
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn last_user(request: &ModelRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User { content, .. } => Some(cayw_ai::content_text(content)),
            _ => None,
        })
}

fn final_message(request: &ModelRequest) -> AssistantMessage {
    let mut message = AssistantMessage::new(&request.model);
    message.usage = Usage {
        input: 4,
        output: 4,
        total_tokens: 8,
        ..Default::default()
    };
    message.usage_status = UsageStatus::Reported;
    message.content.push(Content::text("follow up complete"));
    message.stop_reason = StopReason::Stop;
    message
}

fn open(
    root: &Path,
    content: bool,
    host: Arc<LargeResultHost>,
    provider: Arc<dyn Provider>,
) -> Runtime {
    std::fs::create_dir(root.join("workspace")).unwrap();
    let mut config = RuntimeConfig {
        content: content.then(|| ContentConfig {
            directory: root.join("content"),
            device_id: "core-bounds-device".into(),
            limits: cayw_content::Limits {
                max_content_bytes: 1024 * 1024,
                ..Default::default()
            },
        }),
        // Isolate the new durable bound from the older externalization path.
        auto_externalize_threshold_bytes: 0,
        permissions: vec![ScopedPermission::exact(
            "search.query",
            json!({"query":"large"}),
        )],
        ..Default::default()
    };
    config.host.external = Some(host);
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "core-bounds"),
        config,
    )
    .unwrap()
}

fn open_frozen(root: &Path, content: bool) -> Runtime {
    std::fs::create_dir(root.join("workspace")).unwrap();
    std::fs::write(root.join("workspace/big.txt"), "y".repeat(PAYLOAD_BYTES)).unwrap();
    let mut config = RuntimeConfig {
        content: content.then(|| ContentConfig {
            directory: root.join("content"),
            device_id: "core-bounds-device".into(),
            limits: cayw_content::Limits {
                max_content_bytes: 1024 * 1024,
                ..Default::default()
            },
        }),
        auto_externalize_threshold_bytes: 0,
        permissions: vec![ScopedPermission::exact(
            "fs.read",
            json!({"path":"big.txt"}),
        )],
        ..Default::default()
    };
    config.host.frozen_reads = true;
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(FrozenReadProvider),
        Model::new("fixture", "core-bounds"),
        config,
    )
    .unwrap()
}

fn host_result(rt: &Runtime, session: &str, name: &str) -> Value {
    let calls: Vec<_> = rt
        .events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call } if call.name == name => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1, "exactly one original capability terminal");
    match calls[0].outcome.clone().unwrap() {
        CapabilityOutcome::Completed { result } => result,
        other => panic!("Host success was not recorded as Completed: {other:?}"),
    }
}

async fn assert_second_prompt(rt: &Runtime, session: &str, calls: &AtomicUsize, expected: usize) {
    let view = rt
        .prompt(session, "follow up", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert_eq!(calls.load(Ordering::SeqCst), expected);
    assert!(rt.get(session).unwrap().pending_approvals.is_empty());
}

async fn oversized_result(content: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let host = Arc::new(LargeResultHost {
        calls: AtomicUsize::new(0),
    });
    let provider = Arc::new(ScriptProvider {
        calls: AtomicUsize::new(0),
    });
    let rt = open(&root, content, host.clone(), provider);
    let session = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&session, "first", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:#?}");
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);

    let result = host_result(&rt, &session, "search.query");
    assert!(
        serde_json::to_vec(&result).unwrap().len() < cayw_core::MAX_METADATA_BYTES,
        "bounded Core result must fit the durable metadata limit: {result}"
    );
    assert_eq!(result["core_result_bounded"], json!(true));
    assert!(
        result["original_sha256"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("sha256:"))
    );
    if content {
        assert_eq!(result["externalized"], json!(true));
        assert!(result["handle"].as_str().is_some_and(|h| !h.is_empty()));
    } else {
        assert_eq!(result["result_omitted"], json!(true));
        assert!(
            result["sha256"]
                .as_str()
                .is_some_and(|hash| hash.starts_with("sha256:"))
        );
    }
    assert!(
        result["bytes"].as_u64().unwrap() > cayw_core::MAX_METADATA_BYTES as u64,
        "receipt/handle must describe the original oversized result: {result}"
    );

    // Cells and Core must agree on the same bounded value. A second
    // externalization would hand the model a different handle than the one
    // durably recorded in the Core terminal.
    let presented: String = rt
        .get(&session)
        .unwrap()
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(cayw_ai::content_text(&result.content)),
            _ => None,
        })
        .collect();
    if content {
        assert!(
            presented.contains(result["handle"].as_str().unwrap()),
            "the model must receive the same durable handle: {presented}"
        );
    } else {
        assert!(
            presented.contains("result_omitted"),
            "the model must receive the omission receipt: {presented}"
        );
    }

    assert_second_prompt(&rt, &session, &host.calls, 1).await;
}

#[tokio::test]
async fn oversized_host_result_without_content_store_is_known_and_continues() {
    oversized_result(false).await;
}

#[tokio::test]
async fn oversized_host_result_with_content_store_is_externalized_and_continues() {
    oversized_result(true).await;
}

#[tokio::test]
async fn streamed_oversized_arguments_are_bounded_before_events_persist() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let host = Arc::new(LargeResultHost {
        calls: AtomicUsize::new(0),
    });
    let rt = open(
        &root,
        false,
        host.clone(),
        Arc::new(StreamingHugeProvider {
            calls: AtomicUsize::new(0),
        }),
    );
    let session = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&session, "first", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:#?}");
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);

    let messages = rt.get(&session).unwrap().messages;
    let call = messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) => assistant
                .tool_calls()
                .into_iter()
                .find(|call| call.id == "huge-stream"),
            _ => None,
        })
        .expect("the streamed call keeps its identity");
    assert_eq!(call.arguments["tool_arguments_omitted"], json!(true));
    assert!(serde_json::to_vec(&call.arguments).unwrap().len() < cayw_core::MAX_METADATA_BYTES);
    let result = messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "huge-stream" => Some(result),
            _ => None,
        })
        .expect("the known failure is paired with the streamed call");
    assert!(result.is_error);
    assert!(!result.outcome_unknown);

    assert_second_prompt(&rt, &session, &host.calls, 0).await;
}

#[tokio::test]
async fn oversized_tool_arguments_never_plan_or_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let host = Arc::new(LargeResultHost {
        calls: AtomicUsize::new(0),
    });
    let rt = open(&root, false, host.clone(), Arc::new(HugeArgumentsProvider));
    let session = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&session, "first", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:#?}");
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    assert!(
        !rt.events(&session, 0, 1000).unwrap().iter().any(|event| {
            matches!(
                &event.event,
                CoreEvent::CapabilityPlanned { .. } | CoreEvent::CapabilityStarted { .. }
            )
        }),
        "oversized arguments must not reach capability planning"
    );

    let messages = rt.get(&session).unwrap().messages;
    let call = messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) => assistant
                .tool_calls()
                .into_iter()
                .find(|call| call.id == "huge"),
            _ => None,
        })
        .expect("the original call identity remains paired in history");
    assert_eq!(call.arguments["tool_arguments_omitted"], json!(true));
    assert!(
        call.arguments["sha256"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("sha256:"))
    );
    assert!(call.arguments["bytes"].as_u64().unwrap() > cayw_core::MAX_METADATA_BYTES as u64);
    assert!(serde_json::to_vec(&call.arguments).unwrap().len() < cayw_core::MAX_METADATA_BYTES);

    let result = messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "huge" => Some(result),
            _ => None,
        })
        .expect("the known failure is paired with the original call");
    assert!(result.is_error);
    assert!(!result.outcome_unknown);
    assert!(cayw_ai::content_text(&result.content).contains("durable metadata limit"));

    assert_second_prompt(&rt, &session, &host.calls, 0).await;
}

async fn frozen_oversized_read(content: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let rt = open_frozen(&root, content);
    let session = rt.create(None).unwrap().id;
    let view = rt
        .prompt(
            &session,
            "read a large frozen file",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:#?}");

    let result = host_result(&rt, &session, "fs.read");
    assert!(
        serde_json::to_vec(&result).unwrap().len() < cayw_core::MAX_METADATA_BYTES,
        "the Core terminal for an oversized frozen read must be bounded: {result}"
    );
    assert_eq!(result["core_result_bounded"], json!(true));
    assert!(
        result["original_sha256"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("sha256:"))
    );
    if content {
        assert_eq!(result["externalized"], json!(true));
        assert!(result["handle"].as_str().is_some_and(|h| !h.is_empty()));
    } else {
        assert_eq!(result["result_omitted"], json!(true));
    }

    let presented: String = rt
        .get(&session)
        .unwrap()
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(cayw_ai::content_text(&result.content)),
            _ => None,
        })
        .collect();
    if content {
        assert!(
            presented.contains(result["handle"].as_str().unwrap()),
            "the model must receive the same durable handle: {presented}"
        );
    } else {
        assert!(
            presented.contains("result_omitted"),
            "the model must receive the omission receipt: {presented}"
        );
    }

    // Recovery re-reads the shared-read Evidence. It must accept the bounded
    // terminal through the recorded original hash, not the handle's own hash.
    let recovered = rt.recover(&session).unwrap();
    assert_eq!(recovered.state, SessionState::Idle);
    assert!(recovered.pending_approvals.is_empty());

    let view = rt
        .prompt(&session, "follow up", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
}

#[tokio::test]
async fn frozen_oversized_read_with_content_store_is_bounded_and_recovers() {
    frozen_oversized_read(true).await;
}

#[tokio::test]
async fn frozen_oversized_read_without_content_store_is_bounded_and_recovers() {
    frozen_oversized_read(false).await;
}
