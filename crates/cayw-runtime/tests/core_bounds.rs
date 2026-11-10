//! Runtime Core metadata behavior: results and arguments persist verbatim
//! because the former artificial Core size ceilings are gone; only genuine
//! tool failures are recorded as known errors.

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
const PAGE_BYTES: usize = 80 * 1024;

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

/// A provider that streams a tool call whose argument exceeds the removed
/// Core metadata bound for a single value (the 2 MiB ceiling is gone).
struct OverMetadataArgumentsProvider;

impl Provider for OverMetadataArgumentsProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        if last_user(&request).as_deref() == Some("follow up")
            || request
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResult(_)))
        {
            return Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: final_message(&request),
            }]));
        }
        let mut message = AssistantMessage::new(&request.model);
        message.usage = Usage {
            input: 4,
            output: 4,
            total_tokens: 8,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        message.content.push(Content::ToolCall(ToolCall::new(
            "over-metadata",
            "execute_code",
            json!({
                "action":"exec",
                "script":"x".repeat(2 * 1024 * 1024 + 200 * 1024),
                "yield_time_ms":30_000,
                "max_output_tokens":128*1024,
            }),
        )));
        message.stop_reason = StopReason::ToolUse;
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
        // every delta. Every MessageUpdate is durable, so a 70 KiB argument now
        // persists verbatim instead of being rewritten into a summary.
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

struct PageProvider;

impl Provider for PageProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = if last_user(&request).as_deref() == Some("follow up") {
            final_message(&request)
        } else {
            cayw_runtime::fixture::script_response(
                &request,
                "return string.rep('z', 80 * 1024)",
                "page-80k",
            )
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
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
        // Isolate Core persistence from the separate model-presentation
        // externalization path so the durable terminal is what gets asserted.
        auto_externalize_threshold_bytes: 0,
        // The CellLimits item budget must admit the same Host result that
        // Core now persists so this test observes Core, not a Cells refusal.
        cells: cayw_cells::CellLimits {
            item_bytes: Some(120 * 1024),
            ..Default::default()
        },
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
        cells: cayw_cells::CellLimits {
            item_bytes: Some(120 * 1024),
            ..Default::default()
        },
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

fn has_receipt(result: &Value) -> bool {
    result.get("core_result_bounded").is_some()
        || result.get("result_omitted").is_some()
        || result.get("externalized").is_some()
}

fn presented(rt: &Runtime, session: &str) -> String {
    rt.get(session)
        .unwrap()
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(cayw_ai::content_text(&result.content)),
            _ => None,
        })
        .collect()
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
    assert_eq!(result, json!({"payload":"x".repeat(PAYLOAD_BYTES)}));
    assert!(
        !has_receipt(&result),
        "the original result must persist: {result}"
    );

    let shown = presented(&rt, &session);
    assert!(
        shown.contains(&"x".repeat(1024)),
        "the model must receive the original result, got {} bytes",
        shown.len()
    );

    assert_second_prompt(&rt, &session, &host.calls, 1).await;
}

#[tokio::test]
async fn oversized_host_result_without_content_store_is_known_and_continues() {
    oversized_result(false).await;
}

#[tokio::test]
async fn oversized_host_result_with_content_store_persists_verbatim_and_continues() {
    oversized_result(true).await;
}

#[tokio::test]
async fn streamed_oversized_arguments_persist_verbatim_without_an_artificial_ceiling() {
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
    assert!(
        !rt.events(&session, 0, 1000).unwrap().iter().any(|event| {
            matches!(
                &event.event,
                CoreEvent::CapabilityPlanned { .. } | CoreEvent::CapabilityStarted { .. }
            )
        }),
        "oversized script arguments must not reach capability planning"
    );

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
    assert!(call.arguments.get("tool_arguments_omitted").is_none());
    assert_eq!(
        call.arguments["script"].as_str().unwrap().len(),
        PAYLOAD_BYTES
    );
    let result = messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "huge-stream" => Some(result),
            _ => None,
        })
        .expect("the call keeps a paired result");
    assert!(!result.outcome_unknown);

    assert_second_prompt(&rt, &session, &host.calls, 0).await;
}

#[tokio::test]
async fn large_tool_arguments_persist_verbatim_without_capability_dispatch() {
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
    assert!(call.arguments.get("tool_arguments_omitted").is_none());
    assert!(call.arguments.get("sha256").is_none());
    assert_eq!(
        call.arguments["script"].as_str().unwrap().len(),
        PAYLOAD_BYTES
    );

    let result = messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "huge" => Some(result),
            _ => None,
        })
        .expect("the call keeps a paired result");
    assert!(!result.outcome_unknown);

    assert_second_prompt(&rt, &session, &host.calls, 0).await;
}

#[tokio::test]
async fn execute_code_page_between_64k_and_128k_is_known_and_continues() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let host = Arc::new(LargeResultHost {
        calls: AtomicUsize::new(0),
    });
    let rt = open(&root, false, host.clone(), Arc::new(PageProvider));
    let session = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&session, "first", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:#?}");

    let result = rt
        .get(&session)
        .unwrap()
        .messages
        .into_iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "page-80k" => Some(result),
            _ => None,
        })
        .expect("the execute_code terminal is paired");
    assert!(
        !result.is_error,
        "the 80 KiB page must complete without error"
    );
    assert!(!result.outcome_unknown);
    let page = result
        .structured_content
        .as_ref()
        .expect("the CellPage is retained verbatim");
    let outputs = page["outputs"].as_array().expect("CellPage outputs");
    let output_bytes: usize = outputs
        .iter()
        .filter_map(|item| {
            item["output"]
                .get("text")
                .or_else(|| item["output"].get("value"))
                .and_then(Value::as_str)
        })
        .map(str::len)
        .sum();
    assert!(
        output_bytes >= PAGE_BYTES,
        "the 80 KiB page must be retained (got {output_bytes} bytes)"
    );
    assert_eq!(page["status"], json!("completed"));
    assert_eq!(page["has_more"], json!(false));

    assert_second_prompt(&rt, &session, &host.calls, 0).await;
}

#[tokio::test]
async fn arguments_above_removed_core_metadata_ceiling_persist_without_a_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let host = Arc::new(LargeResultHost {
        calls: AtomicUsize::new(0),
    });
    let rt = open(
        &root,
        false,
        host.clone(),
        Arc::new(OverMetadataArgumentsProvider),
    );
    let session = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&session, "first", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        view.state,
        SessionState::Idle,
        "a huge argument is persisted and fails as a known tool error"
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    let messages = rt.get(&session).unwrap().messages;
    let call = messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) => assistant
                .tool_calls()
                .into_iter()
                .find(|call| call.id == "over-metadata"),
            _ => None,
        })
        .expect("the huge call persists verbatim");
    assert!(call.arguments.get("tool_arguments_omitted").is_none());
    assert_eq!(
        call.arguments["script"].as_str().unwrap().len(),
        2 * 1024 * 1024 + 200 * 1024,
        "the removed Core metadata ceiling must not truncate the argument"
    );
    let result = messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "over-metadata" => Some(result),
            _ => None,
        })
        .expect("the huge call keeps a paired result");
    assert!(!result.outcome_unknown);
    // The admitted call really executed: the returned CellPage completed even
    // though the script was larger than the removed Core metadata ceiling.
    let page = result
        .structured_content
        .as_ref()
        .expect("execute_code returns a CellPage");
    assert_eq!(page["status"], json!("completed"), "{page:#?}");
    // The huge script never calls cayw.call, so it never plans a Core capability.
    let records = rt.events(&session, 0, 1000).unwrap();
    assert!(
        !records.iter().any(|record| matches!(
            &record.event,
            CoreEvent::CapabilityPlanned { .. } | CoreEvent::CapabilityStarted { .. }
        )),
        "the huge script never calls cayw.call, so no Core capability is planned"
    );
    // The agent loop finished and left the session reusable.
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::Assistant(assistant)
            if cayw_ai::content_text(&assistant.content) == "follow up complete")),
        "the agent must receive the admitted tool result and finish the attempt"
    );
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
    assert_eq!(result["bytes"], json!(PAYLOAD_BYTES));
    assert_eq!(result["content"].as_str().unwrap().len(), PAYLOAD_BYTES);
    assert!(
        !has_receipt(&result),
        "the frozen read must persist verbatim"
    );

    let shown = presented(&rt, &session);
    assert!(shown.contains(&"y".repeat(1024)), "the model sees the read");

    // Recovery re-reads the shared-read Evidence and must re-hash the exact
    // original value now retained by Core.
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
async fn frozen_oversized_read_with_content_store_persists_and_recovers() {
    frozen_oversized_read(true).await;
}

#[tokio::test]
async fn frozen_oversized_read_without_content_store_persists_and_recovers() {
    frozen_oversized_read(false).await;
}
