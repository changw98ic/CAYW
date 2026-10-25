//! Actual host SIGKILL at local content commits. Only the Provider is synthetic.
#![cfg(all(feature = "fault-injection", target_os = "macos"))]
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_content::{ContentHash, Limits};
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityReconciliation, CoreEvent,
    ReconciliationRequest, ReconciliationVerifier, SessionState,
};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{
    ContentConfig, ContentView, ResourcePolicy, Runtime, RuntimeConfig, ScopedPermission,
    SessionView, TaskInput, TaskIntent, TaskPriority, TaskResources, TaskState,
};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const SCRIPT: &str =
    "return cayw.call('content.put',{name='note',text='committed text',expected_version=0})";
const TOKEN: &str = "local-content-recovery-token";
fn arguments() -> Value {
    json!({"name":"note","text":"committed text","expected_version":0})
}
const NEXT_PROMPT: &str = "read recovered content without replaying the original write";
const NEXT_SCRIPT: &str = "local content=cayw.call('content.read',{name='note'}) assert(content.record.version==1) assert(content.text=='committed text') return content";
struct Fixed(Arc<AtomicUsize>);
impl Provider for Fixed {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        let continuing = request.messages.iter().any(|message| {
            matches!(message, cayw_ai::Message::User { .. }) && message.text() == NEXT_PROMPT
        });
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: cayw_runtime::fixture::script_response(
                &request,
                if continuing { NEXT_SCRIPT } else { SCRIPT },
                &format!("content-{}", request.messages.len()),
            ),
        }]))
    }
}
fn open(
    root: &Path,
    calls: Arc<AtomicUsize>,
    verifier: Option<Arc<dyn ReconciliationVerifier>>,
) -> Runtime {
    let config = RuntimeConfig {
        resources: ResourcePolicy {
            max_declared_memory_bytes: 100,
        },
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "original-device".into(),
            limits: Limits::default(),
        }),
        permissions: vec![
            ScopedPermission::exact("content.put", arguments()),
            ScopedPermission::exact("content.read", json!({"name":"note"})),
        ],
        max_concurrent_runs: 1,
        max_queued_runs: 2,
        max_run_time: Duration::from_secs(120),
        reconciliation_verifier: verifier,
        ..Default::default()
    };
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(Fixed(calls)),
        Model::new("fixture", "content-crash"),
        config,
    )
    .unwrap()
}
fn durable(path: &Path, value: &impl serde::Serialize) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    serde_json::to_writer(&mut file, value).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
    File::open(path.parent().unwrap())
        .unwrap()
        .sync_all()
        .unwrap();
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn evidence_root(point: &str, ipc: bool) -> PathBuf {
    let base = std::env::var_os("CAYW_ISSUE18_CONTENT_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = base.join(format!(
        "content-{}-{}-{}",
        point.replace('.', "-"),
        if ipc { "ipc" } else { "sdk" },
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&path).unwrap();
    std::fs::create_dir(path.join("workspace")).unwrap();
    path.canonicalize().unwrap()
}
struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn stopped_host(root: &Path, point: &str) -> Value {
    let mut child = Owned(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "content_recovery_child", "--nocapture"])
            .env("CAYW_CONTENT_RECOVERY_ROOT", root)
            .env("CAYW_CONTENT_RECOVERY_POINT", point)
            .stdin(Stdio::null())
            .stdout(File::create(root.join("child.stdout.log")).unwrap())
            .stderr(File::create(root.join("child.stderr.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    while !root.join("ready.json").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before {point}; evidence={}",
            root.display()
        );
        assert!(
            Instant::now() < deadline,
            "checkpoint timeout; evidence={}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let ready = read(&root.join("ready.json"));
    assert_eq!(ready["point"], point);
    assert_eq!(ready["pid"], child.0.id());
    child.0.kill().unwrap();
    let status = child.0.wait().unwrap();
    assert_eq!(status.signal(), Some(9));
    durable(
        &root.join("child-exit.json"),
        &json!({"signal":status.signal(),"code":status.code(),"pid":child.0.id()}),
    );
    ready
}
#[test]
fn content_recovery_child() {
    let Some(root) = std::env::var_os("CAYW_CONTENT_RECOVERY_ROOT").map(PathBuf::from) else {
        return;
    };
    let point = std::env::var("CAYW_CONTENT_RECOVERY_POINT").unwrap();
    let hook_root = root.clone();
    cayw_ai::faults::install(Arc::new(move |observed, data| {
        if observed == point {
            let pending = hook_root.join("ready.pending");
            durable(
                &pending,
                &json!({"point":observed,"data":data,"pid":std::process::id()}),
            );
            std::fs::rename(&pending, hook_root.join("ready.json")).unwrap();
            File::open(&hook_root).unwrap().sync_all().unwrap();
            loop {
                std::thread::park();
            }
        }
    }))
    .unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let rt = open(&root, Arc::new(AtomicUsize::new(0)), None);
            let session = rt.create(None).unwrap().id;
            let task = rt
                .enqueue_with_resources(
                    &session,
                    TaskInput {
                        text: "write local content".into(),
                        images: vec![],
                    },
                    TaskIntent::General,
                    TaskPriority::Normal,
                    TaskResources {
                        declared_memory_bytes: 80,
                        ..Default::default()
                    },
                )
                .unwrap();
            durable(&root.join("original-task.json"), &task);
            rt.resume(&task.id, CancellationToken::new()).await.unwrap();
            panic!("missed content checkpoint");
        });
}

/// Rechecks the actual local files for this original call. An evidence string
/// alone cannot grant permission or replace a different original identity.
struct VerifiedContent {
    root: PathBuf,
    call: CapabilityCallRecord,
    view: ContentView,
    actor: String,
}
impl ReconciliationVerifier for VerifiedContent {
    fn verify(&self, request: &ReconciliationRequest) -> Result<(), String> {
        match request {
            ReconciliationRequest::Capability { reconciliation } => {
                if reconciliation.actor != self.actor
                    || reconciliation.key != self.call.key
                    || *reconciliation.expected != cayw_core::ReconciliationOrigin::from(&self.call)
                    || reconciliation.external_provenance != self.call.external_provenance
                    || reconciliation.outcome
                        != (CapabilityOutcome::Completed {
                            result: serde_json::to_value(&self.view).unwrap(),
                        })
                    || reconciliation.evidence
                        != self.root.join("independent-record.json").to_string_lossy()
                {
                    return Err(
                        "decision differs from independently observed original content".into(),
                    );
                }
            }
            ReconciliationRequest::Tool { .. } => {
                return Err(
                    "the original parent tool already retains its known interrupted result".into(),
                );
            }
        }
        if read(&self.root.join("child-exit.json"))["signal"] != 9 {
            return Err("the original host was not confirmed stopped".into());
        }
        let state = read(&self.root.join("content/state.json"));
        if state["records"][self.view.record.name.as_str()]
            != serde_json::to_value(&self.view.record).unwrap()
        {
            return Err("authoritative record changed after inspection".into());
        }
        let bytes = std::fs::read(
            self.root
                .join("content/objects")
                .join(&self.view.content_hash_hex),
        )
        .map_err(|error| error.to_string())?;
        if bytes != self.view.text.as_bytes()
            || ContentHash::of(&bytes) != self.view.record.content_hash
        {
            return Err("actual bytes differ from the observed content".into());
        }
        Ok(())
    }
}
async fn inspect(rt: &Runtime, client: Option<&Client>, session: &str) -> Option<ContentView> {
    match client {
        Some(client) => serde_json::from_value(
            client
                .request(
                    "content.inspect",
                    json!({"session_id":session,"name":"note"}),
                )
                .await
                .unwrap(),
        )
        .unwrap(),
        None => rt.content_inspect(session, None, "note").unwrap(),
    }
}
async fn run(point: &str, committed: bool, terminal_known: bool, ipc: bool) {
    let root = evidence_root(point, ipc);
    let ready = tokio::task::spawn_blocking({
        let root = root.clone();
        let point = point.to_owned();
        move || stopped_host(&root, &point)
    })
    .await
    .unwrap();
    let task = read(&root.join("original-task.json"));
    let session = task["session_id"].as_str().unwrap();
    let task_id = task["id"].as_str().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let rt = open(&root, calls.clone(), None);
    let before = rt.task(task_id).unwrap();
    assert_eq!(before.state, TaskState::Running);
    assert!(before.holds_slot);
    assert_eq!(
        rt.resources().unwrap().usage.reserved_declared_memory_bytes,
        80
    );
    let original = rt
        .events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|event| match event.event {
            CoreEvent::CapabilityStarted { call } => Some(call),
            _ => None,
        })
        .unwrap();
    assert_eq!(original.name, "content.put");
    assert_eq!(original.arguments, arguments());
    assert_eq!(original.task_id, task_id);
    assert_eq!(before.worker_id.as_ref(), Some(&original.lease.worker_id));
    assert_eq!(before.attempt_id.as_ref(), Some(&original.lease.attempt_id));
    assert_eq!(before.fence, original.lease.fence);
    let expected_name = ContentHash::of(
        &serde_json::to_vec(&(
            "cayw-runtime-content-v1",
            "original-device",
            session,
            Option::<&str>::None,
            "note",
        ))
        .unwrap(),
    )
    .to_string();
    if point.starts_with("content_") {
        assert_eq!(ready["data"]["name"], expected_name);
    }
    let mut server = None;
    let client = if ipc {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let work = tokio::spawn(serve(
            listener,
            Arc::new(rt.clone()),
            ServerConfig::new(TOKEN),
            stop.clone(),
        ));
        server = Some((stop, work));
        Some(Client::connect(addr, TOKEN).await.unwrap())
    } else {
        None
    };
    let view = inspect(&rt, client.as_ref(), session).await;
    assert_eq!(view.is_some(), committed);
    if let Some(view) = &view {
        assert_eq!(view.text, "committed text");
        assert_eq!(view.record.version, 1);
        assert_eq!(view.record.name.as_str(), expected_name);
        assert_eq!(
            view.content_hash_hex,
            ContentHash::of(b"committed text").to_string()
        );
        if point.starts_with("content_") {
            assert_eq!(ready["data"]["revision"], view.revision_hex);
        }
        durable(&root.join("independent-record.json"), view);
    }
    let recovered: SessionView = match &client {
        Some(client) => serde_json::from_value(
            client
                .request("session.recover", json!({"session_id":session}))
                .await
                .unwrap(),
        )
        .unwrap(),
        None => rt.recover(session).unwrap(),
    };
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let stopped = rt.task(task_id).unwrap();
    assert_eq!(stopped.worker_id, before.worker_id);
    assert_eq!(stopped.attempt_id, before.attempt_id);
    assert!(stopped.fence > before.fence);
    assert_eq!(stopped.resources, before.resources);
    assert!(!stopped.holds_slot);
    assert_eq!(
        rt.resources().unwrap().usage.reserved_declared_memory_bytes,
        0
    );
    if terminal_known {
        assert!(recovered.unresolved_capabilities.is_empty());
        let completed = rt
            .events(session, 0, 1000)
            .unwrap()
            .into_iter()
            .find_map(|event| match event.event {
                CoreEvent::CapabilitySettled { call } if call.key == original.key => Some(call),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            completed.outcome,
            Some(CapabilityOutcome::Completed {
                result: serde_json::to_value(view.as_ref().unwrap()).unwrap()
            })
        );
    } else {
        assert_eq!(recovered.state, SessionState::NeedsReconciliation);
        assert_eq!(recovered.unresolved_capabilities.len(), 1);
        let unknown = &recovered.unresolved_capabilities[0];
        assert_eq!(unknown.key, original.key);
        assert_eq!(unknown.lease, original.lease);
        assert!(matches!(
            unknown.outcome,
            Some(CapabilityOutcome::OutcomeUnknown { .. })
        ));
        assert!(rt.enqueue(session, "must not replay").is_err());
        rt.recover(session).unwrap();
        assert_eq!(inspect(&rt, client.as_ref(), session).await, view);
        if let Some(view) = view.clone() {
            reconcile_observed(&root, &rt, &calls, session, &original, &view, ipc).await;
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(inspect(&rt, client.as_ref(), session).await, view);
    durable(
        &root.join("verified.json"),
        &json!({"point":point,"ipc":ipc,"original_call":original,"original_task":before,"stopped_task":stopped,"committed":committed,"terminal_known":terminal_known,"provider_requests_during_recovery":calls.load(Ordering::SeqCst),"ready":ready,"actual_record":view}),
    );
    eprintln!("CONTENT_RECOVERY_EVIDENCE {}", root.display());
    if let Some(client) = client {
        client.close().await;
    }
    if let Some((stop, work)) = server {
        stop.cancel();
        work.await.unwrap().unwrap();
    }
}
async fn reconcile_observed(
    root: &Path,
    rt: &Runtime,
    calls: &Arc<AtomicUsize>,
    session: &str,
    original: &CapabilityCallRecord,
    view: &ContentView,
    ipc: bool,
) {
    let original_messages = rt.get(session).unwrap().messages;
    let actor = if ipc {
        "rpc-user"
    } else {
        "local-content-recovery-host"
    };
    let decision = CapabilityReconciliation {
        key: original.key.clone(),
        expected: original.into(),
        actor: actor.into(),
        evidence: root
            .join("independent-record.json")
            .to_string_lossy()
            .into_owned(),
        outcome: CapabilityOutcome::Completed {
            result: serde_json::to_value(view).unwrap(),
        },
        external_provenance: original.external_provenance.clone(),
    };
    assert!(
        rt.reconcile_capability(session, decision.clone()).is_err(),
        "an evidence string without a verifier cannot settle unknown"
    );
    let trusted = open(
        root,
        calls.clone(),
        Some(Arc::new(VerifiedContent {
            root: root.into(),
            call: original.clone(),
            view: view.clone(),
            actor: actor.into(),
        })),
    );
    let mut forged = decision.clone();
    forged.expected.task_id = "another-task".into();
    assert!(trusted.reconcile_capability(session, forged).is_err());
    forged = decision.clone();
    forged.outcome = CapabilityOutcome::Completed {
        result: json!({"version":2}),
    };
    assert!(trusted.reconcile_capability(session, forged).is_err());
    if ipc {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let work = tokio::spawn(serve(
            listener,
            Arc::new(trusted.clone()),
            ServerConfig::new(TOKEN),
            stop.clone(),
        ));
        let client = Client::connect(address, TOKEN).await.unwrap();
        client
            .request(
                "capability.reconcile",
                json!({"session_id":session,"reconciliation":decision}),
            )
            .await
            .unwrap();
        client
            .request(
                "capability.reconcile",
                json!({"session_id":session,"reconciliation":decision}),
            )
            .await
            .unwrap();
        client.close().await;
        stop.cancel();
        work.await.unwrap().unwrap();
    } else {
        trusted
            .reconcile_capability(session, decision.clone())
            .unwrap();
        trusted.reconcile_capability(session, decision).unwrap();
    }
    let after = trusted.get(session).unwrap();
    assert!(after.unresolved_capabilities.is_empty());
    assert_eq!(
        trusted.content_inspect(session, None, "note").unwrap(),
        Some(view.clone())
    );
    assert_eq!(after.state, SessionState::Idle);
    assert!(after.unresolved_calls.is_empty());
    assert_eq!(
        after.messages, original_messages,
        "capability reconciliation must preserve the original model observations"
    );
    continue_read(root, &trusted, session, original, view, ipc).await;
}

async fn continue_read(
    root: &Path,
    rt: &Runtime,
    session: &str,
    original: &CapabilityCallRecord,
    view: &ContentView,
    ipc: bool,
) {
    let settled = rt.get(session).unwrap();
    assert_eq!(settled.state, SessionState::Idle);
    assert!(settled.unresolved_calls.is_empty());
    let parent = settled
        .messages
        .iter()
        .find_map(|message| match message {
            cayw_ai::Message::ToolResult(result)
                if result.tool_call_id == original.parent_call_id =>
            {
                Some(result)
            }
            _ => None,
        })
        .unwrap();
    assert!(parent.is_error);
    assert!(!parent.outcome_unknown);
    assert_eq!(parent.tool_name, "execute_code");
    let execution = settled
        .executions
        .iter()
        .find(|execution| execution.execution_id == original.key.execution_id)
        .unwrap();
    assert!(matches!(
        execution.terminal,
        Some(cayw_core::ExecutionOutcome::Interrupted { .. })
    ));
    let db = rusqlite::Connection::open_with_flags(
        root.join("sessions.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let encoded: String = db
        .query_row(
            "SELECT body FROM cells_v1 WHERE id=?1 AND session=?2 AND task=?3",
            rusqlite::params![original.key.execution_id, session, original.task_id],
            |row| row.get(0),
        )
        .unwrap();
    let cell: cayw_cells::CellRecord = serde_json::from_str(&encoded).unwrap();
    assert_eq!(cell.source, SCRIPT);
    assert_eq!(cell.source_hash, original.source_hash);
    assert_eq!(cell.parent_call_id, original.parent_call_id);
    assert_eq!(cell.calls.len(), 1);
    assert_eq!(
        cell.calls[0].sequence.checked_sub(1),
        Some(original.key.sequence)
    );
    assert_eq!(cell.calls[0].status, cayw_cells::IntentStatus::Succeeded);
    assert_eq!(
        cell.calls[0].result.as_ref(),
        Some(&serde_json::to_value(view).unwrap())
    );
    durable(&root.join("independent-cell.json"), &cell);
    durable(&root.join("known-parent-result.json"), parent);
    let new_calls = Arc::new(AtomicUsize::new(0));
    let continued = open(root, new_calls.clone(), None);
    if ipc {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let work = tokio::spawn(serve(
            listener,
            Arc::new(continued.clone()),
            ServerConfig::new(TOKEN),
            stop.clone(),
        ));
        let client = Client::connect(address, TOKEN).await.unwrap();
        client
            .request(
                "session.prompt",
                json!({"session_id":session,"text":NEXT_PROMPT}),
            )
            .await
            .unwrap();
        client.close().await;
        stop.cancel();
        work.await.unwrap().unwrap();
    } else {
        continued
            .prompt(session, NEXT_PROMPT, CancellationToken::new())
            .await
            .unwrap();
    }
    assert!(new_calls.load(Ordering::SeqCst) > 0);
    assert_eq!(continued.get(session).unwrap().state, SessionState::Idle);
    assert_eq!(
        continued.content_inspect(session, None, "note").unwrap(),
        Some(view.clone())
    );
    let started: Vec<_> = continued
        .events(session, settled.cursor, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilityStarted { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].name, "content.read");
    assert_ne!(started[0].task_id, original.task_id);
    durable(
        &root.join("continuation-verified.json"),
        &json!({"original_parent_result":parent,"unchanged_original_messages":true,"state_after_settlement":settled.state,"subsequent_calls":started,"content":view,"provider_requests_for_new_task":new_calls.load(Ordering::SeqCst)}),
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn killed_staged_content_remains_uncommitted_and_unknown_call_is_not_replayed() {
    run("content_state_staged", false, false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn killed_after_replace_recovers_exact_record_through_authenticated_ipc() {
    run("content_state_renamed", true, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn killed_after_sync_preserves_committed_version_and_requires_verified_settlement() {
    run("content_state_synced", true, false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn committed_core_terminal_is_repaired_without_replaying_content() {
    run("core.capability_terminal.committed", true, true, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn authenticated_ipc_preserves_staged_unknown_without_replay() {
    run("content_state_staged", false, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn sdk_recovers_replaced_content_through_exact_verified_settlement() {
    run("content_state_renamed", true, false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn authenticated_ipc_preserves_synced_content_through_exact_verified_settlement() {
    run("content_state_synced", true, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn sdk_repairs_committed_core_terminal_without_replaying_content() {
    run("core.capability_terminal.committed", true, true, false).await;
}
