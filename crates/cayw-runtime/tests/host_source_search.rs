//! Host-owned Workspace search over the real Runtime, CodeMode, Host, Bridge and IPC.
//!
//! The model is a fixed offline script. The search source uses the real bounded
//! `cayw-workspace::fs.grep` executor and a fixed root handle. The first Provider
//! request must not contain file content; only an allowed `search.query` may scan.

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus,
};
use cayw_bridge::{
    AccessRequest, Limits, Operation, Provenance, Registry, RegistryPolicy, SourceIdentity,
    Visibility,
};
use cayw_codemode::CapabilityError;
use cayw_core::{CapabilityCallRecord, CapabilityOutcome, CoreEvent, EventRecord, SessionState};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{
    ApprovalChoice, BridgeDispatchEvent, Runtime, RuntimeConfig, ScopedPermission, SessionView,
};
use cayw_sources::search::{WorkspaceSearch, WorkspaceSearchStats};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;

const MARKER: &str = "I9-UNIQUE-SEARCH-MARKER-7c4f";

struct AllowAll;

#[async_trait]
impl RegistryPolicy for AllowAll {
    fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
        true
    }

    async fn authorize(
        &self,
        _: &AccessRequest,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

enum Turn {
    Script(String),
    Text(String),
}

struct ScriptProvider {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<ModelRequest>>,
    calls: AtomicUsize,
}

impl ScriptProvider {
    fn new(scripts: Vec<String>) -> Self {
        Self::with_turns(scripts.into_iter().map(Turn::Script).collect())
    }

    fn with_turns(turns: Vec<Turn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Provider for ScriptProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        // Runtime appends current tool declarations as a System message. Select
        // the test turn from the last conversation message, not that declaration.
        let latest = request
            .messages
            .iter()
            .rev()
            .find(|message| !matches!(message, Message::System(_)));
        let turn = if matches!(latest, Some(Message::User { .. })) {
            self.turns
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Turn::Script("return 'no queued turn'".into()))
        } else {
            Turn::Script(String::new())
        };
        self.requests.lock().unwrap().push(request.clone());
        let call_id = format!(
            "host-source-search-{}",
            self.calls.fetch_add(1, Ordering::Relaxed)
        );
        let message = match turn {
            Turn::Script(script) => {
                cayw_runtime::fixture::script_response(&request, &script, &call_id)
            }
            Turn::Text(text) => text_response(&request, &text),
        };
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}

/// Direct assistant text. This creates no `execute_code` call and no Cell.
fn text_response(request: &ModelRequest, text: &str) -> AssistantMessage {
    let mut message = AssistantMessage::new(&request.model);
    // Match script_response's explicit synthetic counters for successful text.
    message.usage = Usage {
        input: 4,
        output: 4,
        total_tokens: 8,
        ..Default::default()
    };
    message.usage_status = UsageStatus::Reported;
    message.content.push(Content::text(text));
    message.stop_reason = StopReason::Stop;
    message
}

/// Owns the unique evidence directory. With `CAYW_ISSUE9_ARTIFACTS_DIR` set,
/// the workspace files and SQLite database remain after the test. Without it,
/// a `TempDir` keeps the same layout and is removed at the end of the test.
struct Case {
    root: PathBuf,
    #[allow(dead_code)]
    _temp: Option<TempDir>,
}

impl Case {
    fn new(name: &str) -> Self {
        if let Ok(base) = std::env::var("CAYW_ISSUE9_ARTIFACTS_DIR") {
            let base = PathBuf::from(base);
            std::fs::create_dir_all(&base).unwrap();
            let root = base.join(format!("{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            Self { root, _temp: None }
        } else {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().to_path_buf();
            Self {
                root,
                _temp: Some(temp),
            }
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

fn setup(name: &str) -> (Case, PathBuf) {
    let case = Case::new(name);
    let workspace = case.root().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    std::fs::write(
        workspace.join("notes/marker.txt"),
        format!("alpha line\n{MARKER}\nomega line\n"),
    )
    .unwrap();
    std::fs::write(workspace.join("notes/other.txt"), "unrelated text\n").unwrap();
    // macOS tempdirs live under the `/var` root alias. `Workspace::open` uses
    // strict per-segment no-follow, so the host passes the canonical test root.
    let workspace = workspace.canonicalize().unwrap();
    (case, workspace)
}

fn search_arguments() -> Value {
    json!({"query": MARKER, "path": "notes", "limit": 5})
}

fn search_script() -> String {
    format!(
        "return cayw.call('search.query', {{query={}, path='notes', limit=5}})",
        serde_json::to_string(MARKER).unwrap()
    )
}

fn config(registry: &Registry, allowed: Option<Value>) -> RuntimeConfig {
    let mut config = RuntimeConfig {
        bridge: Some(registry.consumer()),
        max_run_time: Duration::from_secs(15),
        ..Default::default()
    };
    if let Some(arguments) = allowed {
        config
            .permissions
            .push(ScopedPermission::exact("search.query", arguments));
    }
    config
}

fn open(
    case: &Case,
    workspace: &Path,
    database: &str,
    config: RuntimeConfig,
    provider: Arc<dyn Provider>,
) -> Runtime {
    write_json(
        case.root(),
        &format!("runtime-config-{}.json", uuid::Uuid::new_v4()),
        &json!({
            "database": case.root().join(database), "workspace": workspace,
            "model": {"provider":"fixture","id":"host-source-search"}, "real_model": false,
            "permissions": config.permissions,
            "bindings": config.bridge.as_ref().map(|bridge| bridge.binding_snapshot()),
            "runtime": {"max_run_time_ms":config.max_run_time.as_millis(), "max_turns":config.max_turns,
                "max_total_tokens":config.max_total_tokens, "max_capability_calls":config.max_capability_calls,
                "max_capability_bytes":config.max_capability_bytes, "max_history_bytes":config.max_history_bytes,
                "max_event_bytes":config.max_event_bytes},
            "host": {"max_calls":config.host.max_calls, "max_argument_bytes":config.host.max_argument_bytes,
                "max_result_bytes":config.host.max_result_bytes, "max_total_bytes":config.host.max_total_bytes,
                "call_timeout_ms":config.host.call_timeout.as_millis()},
            "codemode": {"host_calls":config.codemode.host_calls, "output_bytes":config.codemode.output_bytes,
                "timeout_ms":config.codemode.timeout.as_millis()}, "requests":config.request_limits
        }),
    );
    Runtime::open(
        case.root().join(database),
        workspace,
        provider,
        Model::new("fixture", "host-source-search"),
        config,
    )
    .unwrap()
}

fn dispatches(rt: &Runtime, id: &str) -> Vec<BridgeDispatchEvent> {
    let mut cursor = 0;
    let mut events = Vec::new();
    loop {
        let page = rt.bridge_dispatches(id, cursor, 3).unwrap();
        if cursor == page.next_cursor {
            break;
        }
        cursor = page.next_cursor;
        events.extend(page.events);
    }
    events
}

fn all_events(rt: &Runtime, id: &str) -> Vec<EventRecord> {
    let mut cursor = 0;
    let mut events = Vec::new();
    loop {
        let page = rt.events(id, cursor, 1000).unwrap();
        if page.is_empty() {
            break;
        }
        cursor = page.last().unwrap().cursor;
        events.extend(page);
        assert!(events.len() < 100_000, "event history did not terminate");
    }
    events
}

fn settled(rt: &Runtime, id: &str) -> Vec<CapabilityCallRecord> {
    all_events(rt, id)
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call }
            | CoreEvent::CapabilityReconciled { call, .. } => Some(call),
            _ => None,
        })
        .collect()
}

/// Every content operation writes exactly one prepared event (`outcome == None`)
/// followed by one settled event for the same record and origin.
fn dispatch_pair(events: &[BridgeDispatchEvent]) -> (&BridgeDispatchEvent, &BridgeDispatchEvent) {
    assert_eq!(
        events.len(),
        2,
        "expected one prepared and one settled dispatch event"
    );
    let prepared = &events[0];
    let settled = &events[1];
    assert!(
        prepared.outcome.is_none(),
        "first dispatch event must be prepared: {prepared:?}"
    );
    assert!(
        settled.outcome.is_some(),
        "second dispatch event must be settled: {settled:?}"
    );
    assert!(prepared.cursor < settled.cursor);
    assert_eq!(prepared.origin, settled.origin);
    assert_eq!(
        serde_json::to_value(&prepared.record).unwrap(),
        serde_json::to_value(&settled.record).unwrap()
    );
    (prepared, settled)
}

fn tool_result_count(view: &SessionView) -> usize {
    view.messages
        .iter()
        .filter(|message| matches!(message, Message::ToolResult(_)))
        .count()
}

fn write_json<T: Serialize + ?Sized>(root: &Path, name: &str, value: &T) {
    std::fs::create_dir_all(root).unwrap();
    let path = root.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    serde_json::to_writer_pretty(&mut file, value).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
    std::fs::File::open(root).unwrap().sync_all().unwrap();
}

/// Persists the full session, Core events, ToolResult output pages, tasks,
/// complete Provider requests, all dispatch pairs, and the run configuration.
fn capture(root: &Path, rt: &Runtime, id: &str, provider: &ScriptProvider, config: &Value) {
    std::fs::create_dir_all(root).unwrap();
    let session = rt.get(id).unwrap();
    let core = all_events(rt, id);
    let tasks = rt.tasks(id).unwrap();
    let dispatch_events = dispatches(rt, id);
    let requests = provider.requests.lock().unwrap().clone();
    let tool_results = session
        .messages
        .iter()
        .filter_map(|message| {
            if let Message::ToolResult(result) = message {
                Some(result.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let pages = tool_results
        .iter()
        .map(|result| result.structured_content.clone())
        .collect::<Vec<_>>();
    write_json(root, "session.json", &session);
    write_json(root, "core-events.json", &core);
    write_json(root, "tool-results.json", &tool_results);
    write_json(root, "tool-result-pages.json", &pages);
    write_json(root, "tasks.json", &tasks);
    write_json(root, "provider-requests.json", &requests);
    write_json(root, "dispatches.json", &dispatch_events);
    write_json(root, "config.json", config);
    eprintln!("host source search artifacts: {}", root.display());
}

/// Closes the source, then records the post-close registry and source stats.
fn close_source(root: &Path, registry: &Registry, id: &str, stats: &WorkspaceSearchStats) {
    registry.disconnect(id).unwrap();
    write_json(
        root,
        "source-close.json",
        &json!({
            "registry": registry.snapshot(id),
            "stats": stats.snapshot(),
        }),
    );
}

async fn pending(rt: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(approval) = rt.get(id).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_search_returns_marker_context_and_full_provenance() {
    let (case, workspace) = setup("authenticated-search");
    let source = WorkspaceSearch::open(&workspace).unwrap();
    let stats = source.stats();
    let manifest = source.manifest().clone();
    assert!(manifest.search);
    assert!(manifest.tools.is_empty());
    assert!(manifest.skills.is_empty());
    assert!(manifest.resources.is_empty());

    let identity = SourceIdentity {
        id: "workspace-search".into(),
        version: "1".into(),
        origin: "host:workspace-search".into(),
    };
    let registry = Registry::new(Limits::default(), Arc::new(AllowAll)).unwrap();
    let pin = registry
        .register(identity.clone(), manifest.clone(), source.into_handle())
        .unwrap();
    assert_eq!(pin.source, identity);
    assert_eq!(pin.generation, 1);
    assert_eq!(pin.manifest_sha256.len(), 64);
    let pins = registry.consumer().binding_snapshot();
    assert_eq!(pins.sources.get("workspace-search"), Some(&pin));

    let arguments = search_arguments();
    let provider = Arc::new(ScriptProvider::new(vec![search_script()]));
    let rt = open(
        &case,
        &workspace,
        "sessions.db",
        config(&registry, Some(arguments.clone())),
        provider.clone(),
    );
    assert!(
        rt.capabilities()
            .unwrap()
            .iter()
            .any(|tool| tool.name == "search.query")
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let token = "host-source-search-token";
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt.clone()),
        ServerConfig::new(token),
        shutdown.clone(),
    ));
    assert!(Client::connect(address, "wrong-token").await.is_err());
    let client = Client::connect(address, token).await.unwrap();
    let health = client.request("runtime.health", json!({})).await.unwrap();
    assert_eq!(health["status"], "ok");
    let session = client
        .request("session.create", json!({"label":"host search"}))
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap().to_string();
    let answer = client
        .request(
            "session.prompt",
            json!({"session_id":id,"text":"find the unique marker"}),
        )
        .await
        .unwrap();
    assert_eq!(answer["state"], "idle");

    let first_request = {
        let requests = provider.requests.lock().unwrap();
        assert!(!requests.is_empty());
        serde_json::to_string(&requests[0].messages).unwrap()
    };
    assert!(!first_request.contains(MARKER));

    let calls: Vec<CapabilityCallRecord> = settled(&rt, &id)
        .into_iter()
        .filter(|call| call.name == "search.query")
        .collect();
    assert_eq!(calls.len(), 1);
    let result = match calls[0].outcome.as_ref() {
        Some(CapabilityOutcome::Completed { result }) => result.clone(),
        other => panic!("search did not complete: {other:?}"),
    };
    assert_eq!(calls[0].arguments, arguments);
    assert_eq!(
        calls[0].external_provenance,
        Some(serde_json::to_value(&pins).unwrap())
    );
    assert_eq!(result["source"], serde_json::to_value(&pin).unwrap());
    let value = &result["value"];
    assert_eq!(value["query"], MARKER);
    assert_eq!(value["path"], "notes");
    assert_eq!(value["limit"], 5);
    assert_eq!(value["summary"]["match_count"], 1);
    assert_eq!(value["summary"]["file_count"], 1);
    assert_eq!(value["summary"]["truncated"], false);
    assert_eq!(value["summary"]["scan_complete"], true);
    let matches = value["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    let hit = &matches[0];
    assert_eq!(hit["path"], "notes/marker.txt");
    assert_eq!(hit["text"], MARKER);
    assert_eq!(hit["start_line"], 2);
    let context = hit["context"]["text"].as_str().unwrap();
    assert!(context.contains("alpha line"));
    assert!(context.contains(MARKER));
    assert!(context.contains("omega line"));

    let events = dispatches(&rt, &id);
    let (prepared, event) = dispatch_pair(&events);
    assert!(prepared.outcome.is_none());
    assert_eq!(event.record.capability, "search.query");
    assert_eq!(event.record.key.operation, Operation::Search);
    assert_eq!(event.record.key.target, "search.query");
    assert_eq!(event.record.key.source, pin);
    assert_eq!(event.record.arguments, arguments);
    assert_eq!(
        event.record.arguments_sha256,
        cayw_bridge::content_sha256(&serde_json::to_string(&arguments).unwrap())
    );
    assert_eq!(event.record.context.session_id, id);
    assert!(!event.record.context.task_id.is_empty());
    assert!(!event.record.context.attempt_id.is_empty());
    assert!(!event.record.context.execution_id.is_empty());
    assert!(event.record.context.sequence > 0);
    assert!(event.outcome.is_some());
    assert_eq!(
        result["request"],
        serde_json::to_value(&event.record.key).unwrap()
    );

    let snapshot = stats.snapshot();
    assert_eq!(snapshot.queries, 1);
    assert_eq!(snapshot.rejections, 0);
    assert!(!snapshot.closed);
    assert!(!snapshot.instance_id.is_empty());

    let evidence_config = json!({
        "test":"authenticated_search_returns_marker_context_and_full_provenance",
        "workspace": &workspace,
        "database": case.root().join("sessions.db"),
        "identity": &identity,
        "provenance": &pin,
        "manifest": &manifest,
        "binding_snapshot": &pins,
        "stats_before_close": &snapshot,
        "search_arguments": &arguments,
        "permissions": [ScopedPermission::exact("search.query", arguments.clone())],
        "first_provider_request_contains_marker": false,
        "ipc": {"wrong_token_rejected": true, "health": &health},
        "model": {"provider":"fixture","id":"host-source-search"},
        "provider": {"kind":"fixed offline script","real_model":false},
        "max_run_time_ms": 15_000,
    });
    capture(case.root(), &rt, &id, &provider, &evidence_config);
    write_json(
        case.root(),
        "search-result.json",
        &json!({"capability_call": &calls[0], "source_reply": &result}),
    );
    close_source(case.root(), &registry, "workspace-search", &stats);
    assert!(stats.snapshot().closed);

    client.close().await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_permission_denial_never_reaches_the_search_source() {
    let (case, workspace) = setup("permission-denial");
    let source = WorkspaceSearch::open(&workspace).unwrap();
    let stats = source.stats();
    let manifest = source.manifest().clone();
    let identity = SourceIdentity {
        id: "workspace-search".into(),
        version: "1".into(),
        origin: "host:workspace-search".into(),
    };
    let registry = Registry::new(Limits::default(), Arc::new(AllowAll)).unwrap();
    let pin = registry
        .register(identity, manifest.clone(), source.into_handle())
        .unwrap();
    let denied_arguments = json!({"query":"different-marker", "path":"notes"});
    let provider = Arc::new(ScriptProvider::new(vec![
        "return cayw.call('search.query', {query='different-marker', path='notes'})".into(),
    ]));
    let rt = open(
        &case,
        &workspace,
        "sessions.db",
        config(&registry, None),
        provider.clone(),
    );
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(&worker_id, "denied search", CancellationToken::new())
            .await
    });
    let approval = pending(&rt, &id).await;
    assert_eq!(approval.input.capability, "search.query");
    assert_eq!(
        approval.input.scope,
        ScopedPermission::exact("search.query", denied_arguments.clone()).scope
    );
    rt.approve(&id, &approval.id, ApprovalChoice::Deny, "fixture-host")
        .unwrap();
    let view = worker.await.unwrap().unwrap();
    assert_eq!(view.state, SessionState::Idle);
    assert!(dispatches(&rt, &id).is_empty());
    let calls: Vec<CapabilityCallRecord> = settled(&rt, &id)
        .into_iter()
        .filter(|call| call.name == "search.query")
        .collect();
    assert_eq!(calls.len(), 1);
    assert!(matches!(
        calls[0].outcome.as_ref(),
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
    let snapshot = stats.snapshot();
    assert_eq!(snapshot.queries, 0);
    assert_eq!(snapshot.rejections, 0);
    assert!(!snapshot.closed);

    let evidence_config = json!({
        "test":"exact_permission_denial_never_reaches_the_search_source",
        "workspace": &workspace,
        "database": case.root().join("sessions.db"),
        "provenance": &pin,
        "manifest": &manifest,
        "denied_arguments": &denied_arguments,
        "permissions": [],
        "approval": &approval,
        "approval_choice": "deny",
        "session_state": view.state,
        "dispatch_count": 0,
        "stats_before_close": &snapshot,
        "model": {"provider":"fixture","id":"host-source-search"},
        "provider": {"kind":"fixed offline script","real_model":false},
        "max_run_time_ms": 15_000,
    });
    capture(case.root(), &rt, &id, &provider, &evidence_config);
    write_json(
        case.root(),
        "denial.json",
        &json!({
            "approval": &approval,
            "capability_calls": &calls,
            "session": &view,
        }),
    );
    close_source(case.root(), &registry, "workspace-search", &stats);
    assert!(stats.snapshot().closed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_keeps_plain_chat_and_missing_search_is_explicit() {
    let (case, workspace) = setup("disconnect-plain-missing");
    let source = WorkspaceSearch::open(&workspace).unwrap();
    let stats = source.stats();
    let manifest = source.manifest().clone();
    let identity = SourceIdentity {
        id: "workspace-search".into(),
        version: "1".into(),
        origin: "host:workspace-search".into(),
    };
    let registry = Registry::new(Limits::default(), Arc::new(AllowAll)).unwrap();
    let pin = registry
        .register(identity, manifest.clone(), source.into_handle())
        .unwrap();
    let arguments = search_arguments();
    let provider = Arc::new(ScriptProvider::with_turns(vec![
        Turn::Script(search_script()),
        Turn::Text("plain chat ok".into()),
        Turn::Script(search_script()),
    ]));
    let rt = open(
        &case,
        &workspace,
        "sessions.db",
        config(&registry, Some(arguments.clone())),
        provider.clone(),
    );
    let id = rt.create(None).unwrap().id;
    let allowed = rt
        .prompt(&id, "allowed search", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(allowed.state, SessionState::Idle);
    assert_eq!(stats.snapshot().queries, 1);

    registry.disconnect("workspace-search").unwrap();
    assert!(stats.snapshot().closed);

    let tool_results_before_plain = tool_result_count(&allowed);
    let plain = rt
        .prompt(&id, "plain chat", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(plain.state, SessionState::Idle);
    let tool_results_after_plain = tool_result_count(&plain);
    assert_eq!(
        tool_results_after_plain, tool_results_before_plain,
        "plain chat must not add an execute_code ToolResult"
    );
    assert!(
        matches!(
            plain.messages.last(),
            Some(Message::Assistant(message))
                if message.tool_calls().is_empty()
                    && cayw_ai::content_text(&message.content).contains("plain chat ok")
        ),
        "plain chat must be direct assistant text: {:?}",
        plain.messages.last()
    );

    let disconnected = rt
        .prompt(&id, "search after disconnect", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(disconnected.state, SessionState::Idle);
    assert!(
        serde_json::to_string(&disconnected.messages)
            .unwrap()
            .contains("source disconnected")
    );
    assert_eq!(stats.snapshot().queries, 1);
    let dispatch_events = dispatches(&rt, &id);
    let (_prepared, dispatched) = dispatch_pair(&dispatch_events);
    assert_eq!(dispatched.record.capability, "search.query");
    assert_eq!(dispatched.record.key.operation, Operation::Search);
    assert_eq!(dispatched.record.key.source, pin);
    assert_eq!(dispatched.record.arguments, arguments);
    assert!(dispatched.outcome.is_some());
    let calls: Vec<CapabilityCallRecord> = settled(&rt, &id)
        .into_iter()
        .filter(|call| call.name == "search.query")
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(matches!(
        calls[0].outcome.as_ref(),
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(matches!(
        calls[1].outcome.as_ref(),
        Some(CapabilityOutcome::Failed { error })
            if error.contains("source disconnected")
    ));

    let main_root = case.root().join("main");
    let evidence_config = json!({
        "test":"disconnect_keeps_plain_chat_and_missing_search_is_explicit",
        "workspace": &workspace,
        "database": case.root().join("sessions.db"),
        "provenance": &pin,
        "manifest": &manifest,
        "search_arguments": &arguments,
        "permissions": [ScopedPermission::exact("search.query", arguments.clone())],
        "sequence": [
            "allowed search",
            "disconnect source",
            "plain chat returns direct assistant text",
            "search after disconnect fails before dispatch",
        ],
        "provider_turns": [
            "execute_code search",
            "direct assistant text",
            "execute_code search",
        ],
        "source_disconnected_before_plain_chat": true,
        "stats_before_source_close": stats.snapshot(),
        "model": {"provider":"fixture","id":"host-source-search"},
        "provider": {
            "kind":"fixed offline script plus direct assistant text",
            "real_model":false,
        },
        "max_run_time_ms": 15_000,
    });
    capture(&main_root, &rt, &id, &provider, &evidence_config);
    write_json(
        &main_root,
        "plain-chat.json",
        &json!({
            "state": plain.state,
            "tool_results_before": tool_results_before_plain,
            "tool_results_after": tool_results_after_plain,
            "execute_code_tool_result_added": false,
            "last_message": plain.messages.last(),
            "messages": &plain.messages,
        }),
    );
    write_json(
        &main_root,
        "disconnected-search.json",
        &json!({
            "state": disconnected.state,
            "dispatch_event_count": dispatch_events.len(),
            "dispatched_operation_count": dispatch_events.len() / 2,
            "dispatch_events": &dispatch_events,
            "capability_calls": &calls,
            "stats": stats.snapshot(),
            "messages": &disconnected.messages,
        }),
    );
    close_source(&main_root, &registry, "workspace-search", &stats);
    assert!(stats.snapshot().closed);

    // The missing-source runtime uses its own persistent case root. No bridge
    // source is registered; the workspace path is canonical for macOS.
    let missing_case = Case::new("missing-source");
    let missing_workspace = missing_case.root().join("workspace");
    std::fs::create_dir_all(&missing_workspace).unwrap();
    let missing_workspace = missing_workspace.canonicalize().unwrap();
    let missing_provider = Arc::new(ScriptProvider::new(vec![search_script()]));
    let missing_settings = RuntimeConfig::default();
    let missing_max_run_time_ms = missing_settings.max_run_time.as_millis();
    let missing_rt = open(
        &missing_case,
        &missing_workspace,
        "sessions.db",
        missing_settings,
        missing_provider.clone(),
    );
    let missing_capabilities = missing_rt
        .capabilities()
        .unwrap()
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    assert!(
        !missing_capabilities
            .iter()
            .any(|name| name == "search.query")
    );
    let missing_id = missing_rt.create(None).unwrap().id;
    let missing_view = missing_rt
        .prompt(&missing_id, "missing search", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(missing_view.state, SessionState::Idle);
    assert!(
        serde_json::to_string(&missing_view.messages)
            .unwrap()
            .contains("Unknown capability: search.query")
    );

    let missing_root = missing_case.root().join("missing");
    let missing_config = json!({
        "test":"missing_search_source_is_explicit",
        "max_run_time_ms": missing_max_run_time_ms,
        "workspace": &missing_workspace,
        "database": missing_case.root().join("sessions.db"),
        "capabilities": &missing_capabilities,
        "bridge": null,
        "permissions": [],
        "search_arguments": search_arguments(),
        "model": {"provider":"fixture","id":"host-source-search"},
        "provider": {"kind":"fixed offline script","real_model":false},
    });
    capture(
        &missing_root,
        &missing_rt,
        &missing_id,
        &missing_provider,
        &missing_config,
    );
    write_json(
        &missing_root,
        "missing-source.json",
        &json!({
            "capabilities": &missing_capabilities,
            "source_registered": false,
            "source_snapshot": null,
            "state": missing_view.state,
            "messages": &missing_view.messages,
        }),
    );
    write_json(
        &missing_root,
        "source-close.json",
        &json!({
            "source": null,
            "reason": "no search source registered",
        }),
    );
}
